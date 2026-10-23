# Switching harnesses with migrate

`hlock migrate` moves a lock onto another harness. Your skills, MCP servers, instruction
files and decision models come along, each rewritten to the place and config shape the new
harness reads. Models move to a provider it supports.

```console
$ hlock migrate --to codex
✓ wrote harness.codex.lock  (from harness.lock)
╭─ ⇄ harness.codex.lock · migrated ───────────────────────────────────────╮
│                                                                         │
│  harness        codex 0.50.0  from claude-code                          │
│  llm · default  openai:gpt-5.5                                          │
│  carried        2 MCP servers, 1 skill, 1 instruction file, 0 deciders  │
│                                                                         │
╰─────────────────────────────────────────────────────────────────────────╯
  ~ harness claude-code → codex
  ~ skill review: .claude/skills/review → .agents/skills/review
  ~ instructions CLAUDE.md → AGENTS.md
  ~ llm default: anthropic:opus → openai:gpt-5.5 (Codex CLI doesn't talk to anthropic; --model picks another)
next → hlock sync harness.codex.lock  ·  hlock launch --lock harness.codex.lock
```

The source lock isn't touched. The new lock goes next to it as `harness.<to>.lock`, or
wherever `-o` says. Any lock works as the source: pinned, resolved, tuned, or pulled.

## Then

The migrated lock is an ordinary lock. Try the new harness in isolation first:

```bash
hlock launch --lock harness.codex.lock    # Codex on the migrated setup, in .hlock/env/
```

Then make the switch for real. `sync --in-place` writes into your own config, and puts the
moved project files in your repository:

```console
$ hlock sync harness.codex.lock --in-place --yes
✓ Write ~/.codex/config.toml /mcp_servers,/model,/model_provider
✓ Copy skill review → .agents/skills/review
✓ Write AGENTS.md
✓ in sync with harness.codex.lock
```

`hlock sync` keeps a `.hlock-bak` of each file it replaces. Once you're happy, commit the new
files. If you want the new lock to be `harness.lock`, pass `-o harness.lock` (the old one is
in git), and update `[target] harness` in `harness.toml` if it names the old harness.

## What moves, and where

| Part | Where it goes |
|---|---|
| MCP servers | Into the new harness's MCP config, in its shape (Codex's `[mcp_servers]`, OpenCode's `mcp`, Goose's extensions, …). Disabled servers stay off. IronClaw and ZeroClaw have no MCP config hlock writes, so `migrate` says to add them yourself. |
| Project skills | Kept where they are if the new harness reads that directory (OpenCode reads `.claude/skills`). Otherwise moved to its `.agents/skills`, else its own project skill directory. |
| User skills | Into the new harness's skill directory (`~/.codex/skills`, …), unless it already reads where they are (`~/.agents/skills`) |
| Project instructions | Kept if the new harness reads the file. Otherwise renamed to the file it does read: `CLAUDE.md` → `AGENTS.md` for Codex, `GEMINI.md` for Gemini CLI. If the project already has that file, the lock pins the project's file instead. |
| User instructions | Into the new harness's user instruction file (`~/.codex/AGENTS.md`, …), or dropped with a note when it has none |
| Decision models | Rewired onto the new harness's hooks. A use it has no hook for is dropped, with a note. |
| Guardrails, memory, tool guidance | Into the new harness's rules directory, if it has one. Otherwise they become `## Guardrails`, `## Memory` and `## Tool guidance` sections of the instruction file it reads |
| Subagents | Into the new harness's subagent directory (`.opencode/agent/`, `.claude/agents/`), with the frontmatter it needs (`mode: subagent` for OpenCode). Dropped, with a note, on a harness without subagents |
| Scripts | Kept in `.agents/scripts/`, which is the same for every harness |
| Middleware | Kept only if the new harness loads the same format (an OpenCode plugin file needs OpenCode); otherwise dropped, with a note |

A file that moves needs a copy for `sync` to put in its new place. When the lock doesn't
already vendor it or fetch it from git, `migrate` vendors the copy it finds in the project or
the old harness's config into `.hlock/vendor/<harness>/migrate/<lock>/`. It first checks that the
copy still matches the locked digest; if you've edited it since you pinned, it says to re-pin.

**What doesn't carry over:** the old harness's plugins and its own settings (permissions,
hooks, themes), and skills that ship with it. `migrate` lists them. Files move as they are:
`CLAUDE.md` becomes `AGENTS.md` byte for byte, `@imports` and all.

**Evidence** is cleared, since it measured the old harness. The report keeps the source's
numbers as a baseline; [`hlock resolve`](resolve.md) measures the new one.

## Models

A role whose provider the new harness supports keeps its model. A harness supports the
providers its [profile](harness-profiles.md) lists in `api-keys`, plus its
`default-provider`. OpenCode talks to Anthropic, so a Claude Code lock moved there keeps
`anthropic:opus`.

Otherwise the role moves to the new harness's provider and the default model `hlock meta
init` uses for it (`openai:gpt-5.5` for Codex). `--model` picks another:

```console
$ hlock migrate --to codex --model openai:gpt-5.5-mini
…
  ~ llm default: anthropic:opus → openai:gpt-5.5-mini (--model)
```

Some profiles don't say which providers their harness talks to (Hermes, Amp, nanobot,
PicoClaw, ZeroClaw). Then the model is kept as it is, with a note to check that harness's model
settings.

The new lock sets the model in the harness's config (Codex's `model` and `model_provider`),
so `sync` and `launch` start it on that model. Decision models are called by hlock, not the
harness, so they stay as they are.

## Going local

[`hlock localize`](localize.md) is a migration that also moves the models to a local
runtime. `hlock migrate --to opencode --local` is `hlock localize --harness opencode`, and
writes `harness.local.lock`; `--runtime` picks the runtime. For the model search options
(`--models`, `--memory-gb`, `--evals`), use `hlock localize`.

## Refused

`migrate` refuses an unknown harness, the lock's own harness (use `localize` to keep the
harness and go local), a framework harness like `pydantic-ai` (it runs your code; there's no
config to move skills into), and a `--model` whose provider the new harness doesn't support.
`--local` refuses a [desktop app](harnesses.md#desktop-apps), which hlock can't run on a
local model.

## To a desktop app

`--to` can name a desktop app (`claude-desktop`, `vscode`, `cursor-ide`, …), and a lock pinned from one can
move to an agent CLI. MCP servers are rewritten into the app's shape (VS Code's `servers`,
with `type`), and skills and instruction files go where it reads them (`CLAUDE.md` becomes
`AGENTS.md` for VS Code). An app picks its model in the app or your account, so the lock's
models are kept but not configured, and the report says so. A server the app can't hold
(a remote one, for Claude Desktop, where remote servers are connectors) stays in the lock and
is left out of its config, with a note. `hlock sync --apps harness.vscode.lock` then puts the
rest in place.
