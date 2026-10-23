# Migrate: move a lock to another harness

Status: implemented · Owner: Niels Bantilan · Last updated: 2026-10-21 · Builds on [v0](v0.md) §7.8.5, §7.8.7

> `hlock migrate --to codex` writes `harness.codex.lock`: the same setup (skills, MCP
> servers, instructions, decision models) moved onto another harness, with each part
> rewritten to the place and shape that harness reads, and the models moved to providers
> it can use. `hlock sync harness.codex.lock` then puts it all in place. `hlock localize`
> becomes the special case that also moves the models onto a local runtime.

---

## 1. Problem

`hlock localize` already translates a lock onto another harness, but only as a side effect
of going local, and only as far as the local harnesses it picks need. People also want to
switch harnesses for their own sake: Claude Code to Codex, Cursor to OpenCode, Pi to
Claude Code, keeping their API models. Today they can't, and the translation `localize`
does is incomplete in ways that a plain harness switch exposes:

1. **Project skills stay where the old harness reads them.** A skill pinned at
   `.claude/skills/review` keeps that path. Codex reads `.agents/skills` and `.codex/skills`,
   so `hlock sync` refuses it ("outside Codex CLI's skill directories"). OpenCode happens to
   read `.claude/skills`, which is why `localize` gets away with it.
2. **Project instructions keep their file name.** `CLAUDE.md` stays `CLAUDE.md`; Codex reads
   `AGENTS.md`, so sync refuses it ("isn't a file Codex CLI reads instructions from").
3. **User skills have no copy to install from.** A skill at `{root}/skills/x` that wasn't
   vendored resolves under the *new* harness's root after the switch, where it doesn't
   exist. Sync reports "no vendored copy or source to fetch it from".
4. **Deciders keep the old harness's wiring.** A `[[decider]]` wired as a Claude Code
   `PermissionRequest` hook in `settings` names a file the new harness doesn't have.
5. **Models aren't checked against the new harness.** Codex talks to OpenAI (or a local
   endpoint); `anthropic:claude-opus-…` can't run there. `localize` replaces every model, so
   it never had to decide.
6. **The model isn't configured.** A pinned lock restores the model through config, so
   `launch` doesn't pass `--model`; the new harness starts on its own default.
7. **The evidence is the old harness's.** Success rate and cost were measured on the source
   harness and are copied over as if they were the new one's.

## 2. Design

### 2.1 The command

```bash
hlock migrate [LOCK] --to <harness> [-o OUT] [--model provider:model] [--local [--runtime R]] [--json]
```

- `LOCK` defaults to the current lock (`harness.lock`, or what `--lock`/`HLOCK_LOCK` names).
- `--to` names the target harness, by id or alias. Required, except with `--local`, where
  it's chosen the way `localize` chooses (§2.6).
- `-o` defaults to `harness.<to>.lock` next to the source lock, matching the per-harness
  team layout (`.hlock/locks/harness.<name>.lock`). The source lock isn't touched.
- `--model provider:model` sets the `default` role's model (§2.3).
- `--local` hands off to `localize` with `--to` as its `--harness` (§2.6), and `-o` then
  defaults to `harness.local.lock`, as `localize`'s does.

Refused: an unknown harness, the lock's own harness without `--local` ("already on …"), and
a target that isn't an agent CLI or a desktop app (`kind = "framework"` or `"custom"`: those
run code, and have no config hlock writes skills and MCP servers into). Desktop apps
(`kind = "app"`) are allowed; see [gui-apps](gui-apps.md) §3.10.

The result is an ordinary lock: `kind = "migrated"`, `base-lock` naming the source, and a
`[migrate]` table with the report (what moved, what was replaced, what didn't carry over).
`hlock sync OUT` installs the harness and puts every part in place; `hlock launch --lock
OUT` starts it.

### 2.2 What moves, and where

The translation is shared with `localize` (§2.6), so both get every fix below.

| Part | How it moves |
|---|---|
| Harness | `[harness]` becomes the target, with its probed version and source; the pattern carries over. |
| MCP servers | Kept in `[[tool]]`, and written into the target's MCP config (`mcp.write`) in its shape (`codex`, `opencode`, `goose`, …), as a pointer-restored config entry. |
| Skills, user | A path under the old harness (`{root}/skills/x`, `~/…`) that the target doesn't read becomes `<target skills.write>/<name>`. A path the target reads (`~/.agents/skills/x` for Codex) stays. |
| Skills, project | A path in a directory the target reads (`.claude/skills` for OpenCode) stays. Otherwise it moves to the target's `.agents/skills`, else its first project skill directory, else its user skill directory. |
| Instructions, user | Each user file goes to the target's user file of the same name, else its first one (as `launch` and `localize` do today); files with nowhere to go are dropped with a note. |
| Instructions, project | A file the target reads stays. Otherwise it's renamed to the target's first project instruction file (`CLAUDE.md` → `AGENTS.md`). If the project already has that file, the lock pins the project's file instead, since it's what the target will read. Two files that land on one name keep the first. A file in the old harness's rules directory moves to the target's rules directory, or is dropped with a note when the target has none. |
| Deciders | Rewired for the target (`decide::wiring`): its hook file and events. A use the target can't wire (no `[deciders.<use>]`) is dropped with a note. |
| Models | §2.3. |
| Local runtimes | Kept while a kept role still uses them. |
| Plugins, settings | Don't carry: the old harness's `[[native]]` plugins (packages stay) and its `[[config]]` files are dropped and listed. Built-in and plugin-bundled skills are dropped. |
| Evidence | Cleared, with the source's numbers kept in the report as the baseline: they measured the old harness. |

**Content for moved files.** When a skill or instruction file moves, `sync` needs something
to copy to the new place. A vendored copy, a git source, or a declared dependency at a
commit already works from anywhere. Otherwise `migrate` vendors the content it finds at the
old place (the project file, or the file under the old harness's root) into
`.hlock/vendor/<to>/migrate/`, after checking it still matches the locked digest. Content
that changed since it was pinned isn't vendored; the report says to re-pin, and sync shows
the step as manual.

**One vendor directory per output lock.** What a run vendors goes under
`.hlock/vendor/<to>/migrate/<out stem>/` (and `…/localize/<out stem>/` for `localize`), and a
run replaces only its own output's directory. Without the stem, two locks migrated to the same
harness (`harness.codex.lock`, then `-o other.lock`) would share one `config.pointers.json`,
and the second run would break the first lock's digest. `localize` had the same problem with
`.hlock/vendor/<harness>/localize/`; this fixes it there too.

### 2.3 Models

For each `[[llm]]` role:

1. `--model provider:model` replaces the `default` role (or the first role). The target must
   support that provider.
2. A role the target can run is kept. A target supports a provider when its profile lists
   it in `api-keys` or names it as `model.default-provider`. A role on a local runtime is
   kept when the target can use a local endpoint (`local-models = true` with a
   `[[local-provider]]`). `reference`/`unknown` providers and `harness-default` models are
   kept as they are.
3. Anything else moves to the target's provider: its `default-provider` if hlock has a
   default model for it, else the first provider in its `api-keys` that has one, with the
   same default models `hlock meta init` uses. A target with neither (Copilot, Cursor) gets
   its provider with `model = "harness-default"`: the harness's own model.
4. A target whose profile names no providers at all (no `api-keys`, no
   `default-provider`: Hermes, Amp, nanobot, PicoClaw, ZeroClaw today) keeps the model, with a
   note to check its model settings. The profile doesn't say, so refusing would be a guess.
   Only a local role that such a target also can't run is refused, with a hint to use `--local`.

Each replaced role is reported (`llm default: anthropic:opus → openai:gpt-5.5 (Codex CLI
doesn't talk to anthropic)`), with a note that `--model` picks another.

The decision model and deciders are called by hlock, not the harness, so their models stay.

**Configuring the model.** The new lock sets the model in the target's config, since a
pinned lock's `launch` restores config instead of passing `--model`:

- an API model: the target's `[[api-provider]]` settings (Codex: `/model_provider`,
  `/model`), else its first config-file `model.default` source (Claude Code:
  `settings#/model`), written as `provider/model` when the profile's model format says so;
- a local model: the target's `[[local-provider]]` settings, as `localize` writes them.

These are merged with the MCP pointers into one pointer-restored config entry per file.

### 2.4 Report

`[migrate]` in the lock, and `--json`, hold:

```json
{
  "source": "harness.lock",
  "harness": {"from": "claude-code", "to": "codex", "installed": true, "version": "0.50.0"},
  "carried": {"mcp": 2, "skills": 3, "instructions": 1, "deciders": 1},
  "replaced": ["harness claude-code → codex", "llm default: anthropic:opus → openai:gpt-5.5 (…)",
               "skill review: .claude/skills/review → .agents/skills/review",
               "instructions CLAUDE.md → AGENTS.md"],
  "notes": ["Claude Code's 2 plugins don't carry over: …"],
  "baseline": {"success-rate": 0.8, "trials": 12}
}
```

The CLI prints a panel (harness, models, what carried) followed by the `~` replaced lines and
the `!` notes, the same way `localize` does, then `next → hlock sync OUT · hlock launch --lock OUT`.

### 2.5 Python API

`hlock.migrate(path="harness.lock", to=None, out=None, model=None, manifest=None) -> str`
returns JSON `{"lock", "report", "path"}`, like `hlock.localize`.

### 2.6 Localize is migrate plus local models

`localize` becomes: pick a local-capable target (unchanged rules), migrate onto it with the
shared translation (§2.2), then move API models, the decision model and hosted deciders to a
local runtime, and evaluate or rank them (unchanged). Its lock keeps `kind = "localized"`
and its `[localize]` report; `hlock localize` and its options are unchanged.

`hlock migrate --to X --local [--runtime R]` runs `localize --harness X [--runtime R]` and
prints the localize result. The model-search options (`--models`, `--memory-gb`, `--evals`,
`--strict`, `--allow-network`) stay on `hlock localize`.

What localize gains from sharing the translation: project skills and instructions move to
where the target reads them (fixes localizing onto Codex, Pi, Hermes or Goose from Claude
Code), user skills get a vendored copy, and deciders are rewired for the new harness.

## 3. Non-goals

- **Translating settings and plugins.** Permissions, hooks, themes and plugins have no
  common model across harnesses. They're listed, not converted.
- **Rewriting file contents.** `CLAUDE.md` moves to `AGENTS.md` as it is, `@imports` and
  all; skills are copied byte for byte.
- **Equivalent-model tables.** The replacement for an unsupported provider is that
  provider's default model, not a tier-matched one. `--model` chooses; `resolve` measures.
- **Editing the source project in place.** `migrate` writes a lock; `sync` makes the change,
  with its usual plan, backups and trust checks.

## 4. Tests

- Every pair (Rust, `migrate::tests::every_pair`): for each agent-CLI profile, a lock with
  that harness's own project and user skills, instruction files, an MCP server, a decider and
  a model on its provider is migrated to every other CLI and planned by `sync`. No skill,
  instruction or config step may be refused or fail its digest check: each part lands where
  the target reads it, or is dropped with a note. A new profile is checked against all the
  others without writing a test for it.

- Core (Rust): skill and instruction path mapping for Codex, OpenCode and Hermes targets;
  provider support and the replacement model; decider rewiring and dropping.
- CLI (pytest): Claude Code → Codex on a pinned project (skills move to `.agents/skills`
  and are vendored, `CLAUDE.md` → `AGENTS.md`, MCP in `config.toml`, model set to OpenAI,
  Claude plugins listed); `sync --json` plans every moved part as an automatic step;
  `launch --dry-run` writes Codex's config; refusals (same harness, framework target,
  unsupported `--model` provider); `--local` matches `localize`; the localize suite still
  passes.
