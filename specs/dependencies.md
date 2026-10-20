# Declared dependencies: `hlock add`, sources, and APM interop

Status: implemented (see [the guide](../docs/guides/dependencies.md)) · Owner: Niels Bantilan · Last updated: 2026-10-20 · Builds on [v0](v0.md) §6, §7.7 and [pull](pull.md)

> `hlock pin` records the setup you already have. `hlock add` declares a dependency you
> want, fetches it, pins it, installs it into your harness, and can measure whether it
> helps before you keep it.

---

## 1. Summary

hlock captures agent setups well, but it has no way to *ask for* a dependency. To add a
skill today you install it some other way (`npx skills add`, a plugin marketplace, a `git
clone` into `~/.claude/skills`), then run `hlock pin`, and `pin` records whatever landed on
disk. If the skill came from a git checkout `pin` knows its commit; if it came from a copy,
it's vendored as `origin = "local"`, and the lock no longer knows where it came from.

[Microsoft APM](https://github.com/microsoft/apm) (Agent Package Manager) is built around
the step hlock skips. Its devex is npm's: `apm install anthropics/skills/skills/pdf`
writes the dependency into `apm.yml`, resolves it to a commit, records per-file hashes in
`apm.lock.yaml`, and deploys it into every harness the project targets (`.claude/`,
`.github/`, `.cursor/`, `.agents/`, …). Any git repository, subdirectory or single file is
a package, so there's no registry to publish to.

This spec adopts that loop for hlock, and adds the part APM doesn't have:

```bash
hlock add skill github:anthropics/skills/skills/pdf      # declare, fetch, pin, install
hlock add mcp npm:@modelcontextprotocol/server-github
hlock add skill github:acme/skills/skills/sql --eval      # measure on the eval pack first
hlock add apm:github/awesome-copilot/plugins/context-engineering   # an APM package
hlock remove skill pdf
hlock outdated                                           # what has a newer upstream
```

The v0 spec already designs the manifest side (`[skills]`, `[tools]`, `[instructions]`,
§6 and §7.7.2). Those sections are parsed and hashed today but do nothing. This spec makes
them work for the common case, which is a *pinned* lock with a few declared dependencies,
not a full resolve.

## 2. Goals and non-goals

Goals:

- One command from "I want this skill / MCP server / instruction file" to a dependency
  that's declared, pinned by commit and digest, and installed into the harness.
- A lock entry that always says where a dependency came from, so another machine fetches
  it instead of relying on a vendored copy.
- Be compatible with APM's manifest and lockfile (OpenAPM v0.1). A project that already
  uses `apm.yml` gets the same dependency set from hlock as from `apm install`, without
  converting anything or installing APM.
- Resolve fast. A dependency resolver in the Rust core, with no Python or Node on the
  path, that does no network work when the lock is current.
- Turn "should I add this?" into a measurement: `--eval` runs the eval pack with and
  without the dependency and records the result.

Non-goals:

- A registry, a hub, or a package format of hlock's own. Git is the source, as it is for
  `hlock pull` and APM.
- Publishing or packing (`apm pack`, plugin bundles). Authors can use APM or the
  harness's own plugin format.
- Compiling instructions into per-harness rule trees (`apm compile`). hlock installs
  instruction files as they are; see §7 for what it does across harnesses.
- APM's governance features (policy, executable approval, scanning). Those are in
  [audit](audit.md).

## 3. Design iterations

### 3.1 Iteration 1: `add` is `pin` with a fetch in front

The smallest version: `hlock add skill <source>` clones the source into the harness's
skills directory, then re-pins. It works, and it shows three problems:

1. **The lock forgets the source.** `pin` sees a directory in `~/.claude/skills/` with no
   `.git` and records it as `local`. The source is lost, and `sync` restores the vendored
   copy instead of fetching.
2. **Re-pinning records everything else too.** A user adding one skill also picks up an
   unrelated settings change in the same diff.
3. **Nothing distinguishes "I asked for this" from "it was there".** `check` can't tell a
   declared skill that got deleted from an undeclared one that appeared.

### 3.2 Iteration 2: declare in the manifest, pin into the lock

The fix is the split every package manager makes: the manifest says what you asked for, and
the lock says what that resolved to.

- `hlock add` writes an entry into `harness.toml` (`[skills]`, `[mcp]`, `[instructions]`,
  `[plugins]`), creating a minimal `harness.toml` if the project has none.
- It resolves the source (a ref, tag or semver range) to a commit, fetches it into the
  content-addressed store (`.hlock/store`), and computes the tree digest from v0 §7.7.2.
- It updates only that entry in `harness.lock`, with `declared = true` and the `source`,
  `resolved-ref` and `commit`. Nothing else is re-captured.
- It installs the dependency into the target harness (the same placement `sync` uses).

A pinned lock can now have a manifest. Its `manifest-hash` covers only the dependency
sections, so editing `[candidates]` in the same file doesn't invalidate a pin. `check`
reports three kinds of difference:

| Change | Meaning | Fix |
|---|---|---|
| declared, missing | `harness.toml` asks for it, it isn't installed | `hlock sync` |
| declared, changed | installed digest differs from the lock | `hlock sync`, or `hlock add` to accept |
| undeclared | installed, not in the manifest | `hlock add --adopt NAME`, or `hlock pin` |

Undeclared items are still pinned by `hlock pin` as today. Declaring is opt-in per item, so
existing pinned locks keep working unchanged.

### 3.3 Iteration 3: measure before you keep it

A dependency costs something even when it isn't used: every skill's name and description
spends context at startup and competes for selection (v0 §7.7.7). Nobody knows whether a
new skill helps on their work. APM can't answer this; hlock already has the parts.

`hlock add … --eval` runs the project's eval pack twice, on the locked stack and on the
locked stack plus the new dependency, using the resolver and the trial cache (the baseline
is usually cached already). It prints the difference and asks before keeping it:

```console
$ hlock add skill github:acme/skills/skills/sql --eval
✓ resolved acme/skills@4e1c2a9 skills/sql  sha256:9b2f…
measuring on ./evals (search split, 12 tasks × 3 seeds) …
                 success   cost/task   p50 latency
  without sql     0.72      $0.041       38s
  with sql        0.86      $0.044       41s      +0.14 success
keep sql? [Y/n]
```

When kept, the lock entry carries the comparison as `[skill.evidence]` (eval-pack hash,
split, both success rates). `check` reports the evidence as stale when the skill's digest
changes, as v0 §7.7.4 describes for editable skills.

Without an eval pack, `--eval` stops and points at `hlock evals init`. Plain `add` never
measures.

The trials run in an isolated config root (`.hlock/env/`, as `launch` uses), so nothing is
written to the project or your own config until you answer yes. Answering no leaves the
project exactly as it was; the fetched tree stays in the store, so a later `add` of the same
commit is instant.

`--eval` isn't `--dry-run`. `hlock add … --dry-run` resolves the source and prints what
`add` would write to `harness.toml`, `harness.lock` and the harness, then stops: no
installing, no trials, no cost. `--eval` really installs (in isolation) and really runs
trials, which costs money and takes minutes. The name says what it does, and matches
`hlock evals`.

Prior art: `claude plugin eval` runs a plugin's eval cases with and without the plugin and
reports the difference and its cost, for Claude Code. `--eval` is the same idea for any
dependency in any supported harness, run on the project's own pack rather than cases the
author shipped, and the result is kept in the lock.

### 3.4 Rejected alternatives

- **Read and write `apm.yml` as hlock's manifest.** APM's manifest has no place for
  models, harness versions, policy or evidence, and its schema is a working draft that
  changes every few weeks. hlock reads it (§6) but keeps `harness.toml` as its own.
- **Shell out to the `apm` CLI for APM packages.** It would guarantee identical answers,
  but it puts a Python install on the path of every `sync`, runs a second tool with its own
  cache and config, and makes hlock's speed depend on APM's. The risk of two resolvers
  giving different answers is handled by implementing to the OpenAPM spec and testing
  against APM's own fixtures (§6.5).
- **`hlock install`.** Reads like `npm install`, which also means "install everything".
  hlock already has `sync` for that. `add` is a single dependency.

## 4. Sources

The source syntax is v0 §7.7.2, with the shorthand APM and `hlock pull` already taught
people:

| Source | Example | Resolves to |
|---|---|---|
| GitHub shorthand, optional subpath | `github:anthropics/skills/skills/pdf` | commit + tree digest of the subpath |
| Any git URL | `git+https://gitlab.com/acme/skills#path=sql&tag=v1.2.0` | commit + tree digest |
| Single file | `github:acme/rules/style.instructions.md` | commit + file digest |
| Semver over git tags | `github:acme/skills/sql@^1.2` | highest matching `v{version}` tag (APM's convention), recorded as `constraint` and `resolved-tag` |
| Local path | `./skills/sql` (`editable = true` allowed) | tree digest |
| npm / PyPI (MCP servers) | `npm:@modelcontextprotocol/server-github@^1` | exact version + integrity |
| Remote MCP | `https://mcp.linear.app/mcp` | URL, recorded |
| APM package | `apm:owner/repo[/subpath][#ref]` | see §6 |
| Claude Code plugin | `plugin:acme-marketplace/acme-tools` | version + commit, as `pin` records today |

Private repositories work through git's own credentials, as in `hlock pull`.

## 5. Lock entries

A declared dependency uses the existing `[[skill]]`, `[[tool]]`, `[[instructions]]` and
`[[native]]` tables, with these keys added:

```toml
[[skill]]
name = "sql"
origin = "third-party"
declared = true
source = "github:acme/skills/skills/sql@^1.2"
constraint = "^1.2"
resolved-tag = "v1.3.0"
commit = "4e1c2a9f0d6b8e2f1c3a5b7d9e0f2a4c6b8d0e1f"
digest = "sha256:9b2f…"
scope = "project"

[skill.evidence]
eval-pack = "sha256:51c0…"
split = "search"
trials = 36
baseline-success-rate = 0.72
success-rate = 0.86
```

A declared dependency with a `commit` is never vendored: `sync` fetches it and verifies the
digest. Only `path` dependencies outside the project, and undeclared hand-written ones, are
vendored as today.

## 6. The dependency resolver

### 6.1 Two resolvers, two jobs

hlock already has a resolver, `hlock resolve`, but it solves a different problem: it
expands `[candidates]` into a grid of stacks, measures each one, and selects a winner
(`resolver.rs`, `search.rs`). It never sees a version constraint or a dependency graph.
The **dependency resolver** is new. It turns declared sources into an exact, flat set of
commits and digests, and it runs before anything is measured:

```
harness.toml / apm.yml ──► dependency resolver ──► fixed set of skills, MCP servers, …
                              (this spec)                  │
[candidates] ─────────────────────────────────────► hlock resolve (search, measure, select)
```

When declared dependencies become search axes (`optional`, `variants`, v0 §6), each
variant is a separate dependency resolution, and trials are cached by the resulting
digests, so re-resolving a manifest whose dependencies didn't change re-runs nothing.

In code it lives in a `deps` module of `harness-lock-core`, next to `pull.rs`, whose git
fetching it reuses. User-facing text says "dependencies" and never "resolve" for it, so
`hlock resolve` keeps one meaning.

### 6.2 Semantics: OpenAPM v0.1

The resolver implements the resolution rules of APM's normative spec, so the same
`apm.yml` gives the same set in both tools:

- **Sources:** git URLs on any host, with an optional subdirectory or single file
  ("virtual packages", classified by extension); local paths; `ref` as a branch, tag, commit
  or semver range.
- **Semver over tags:** a range matches tags named `v{version}` or `{name}--v{version}`;
  the highest match wins. No ref means the default branch's head, pinned by the lock.
- **Transitive dependencies:** a package that contains an `apm.yml` brings its own
  `dependencies.apm` and `dependencies.mcp`. The graph is walked breadth-first in
  declaration order.
- **Package conflicts:** when several paths constrain one package, take the highest version
  that satisfies all of them (intersection-pick). An empty intersection fails, naming both
  dependency chains. Nested copies of one package (`conflict_resolution: nest`) are reserved
  in OpenAPM and refused here too.
- **Primitive conflicts:** when two packages provide a skill or instruction with the same
  name, the project's own wins, then the first declared dependency.
- **Transitive MCP servers** are resolved and recorded, but the [audit](audit.md) spec's
  approval step decides whether they're installed.

### 6.3 Speed

Most `sync` runs should touch no network at all. The design follows `uv` and `cargo`:

- **The lock is authoritative.** When every declared dependency has a lock entry whose
  source and constraint are unchanged, the resolver does nothing. It doesn't contact a
  remote to check for newer versions; that's `hlock outdated`'s job.
- **Resolve refs without cloning.** Tags, branches and semver ranges resolve with
  `git ls-remote`, one round trip per repository, run in parallel.
- **One shared git cache.** Bare repositories live in `~/.hlock/cache/git/`, keyed by URL,
  and are fetched shallowly and sparsely (only the subpaths in use). Ten skills from
  `anthropics/skills` cost one fetch.
- **Content-addressed placement.** Fetched trees go into the store by tree digest. Placing a
  dependency into a harness is a copy (or a reflink where the filesystem supports it), and
  an unchanged digest is skipped.

A target worth testing against: `hlock sync` with a current lock and warm cache for a
project with 20 dependencies finishes in under a second.

### 6.4 APM compatibility

- **`apm.yml` is a manifest hlock reads.** If a project has one, its `dependencies` are
  declared dependencies, exactly as if they were in `harness.toml`. There's nothing to
  convert, and APM users keep editing `apm.yml`. If both files declare the same package,
  they must agree, or `check` reports the conflict.
- **`apm.lock.yaml` is an input.** When it's present and consistent with `apm.yml`, the
  resolver takes its `resolved_commit`s as given, so the two tools never disagree about
  versions in a project that already locked with APM. `harness.lock` records the same
  commits plus everything APM doesn't track (harness, models, runtimes, config, evidence).
- **`apm:` sources** in `harness.toml` (`apm:owner/repo[/subpath][#ref]`) name a package
  that is resolved with these semantics, including its transitive dependencies. Each
  placed file gets `origin = "apm"` and `required-by` for transitive packages.
- **Pinning an APM project.** When `hlock pin` finds `apm.lock.yaml`, files in its
  `deployed_files` are recorded as dependencies of their package, not as `first-party` or
  `local` files, so `sync` fetches them instead of vendoring them.
- **hlock writes `harness.lock`, never `apm.yml`.** It writes `apm.lock.yaml` only when
  asked, with `hlock export` (§6.7).

### 6.5 Staying compatible

APM's repository ships `CONFORMANCE.md`, which maps each OpenAPM requirement to tests. hlock
vendors the resolution fixtures for the requirements in §6.2 into
`crates/harness-lock-core/tests/openapm/`, runs them in CI, and records the spec version it
conforms to (`openapm = "0.1"` in `[provenance]`). A new OpenAPM version is a tracked
upgrade, not a silent drift.

### 6.6 Measuring APM packages

Because any declared dependency can take `--eval` (§3.3), hlock answers the question APM
leaves to the user: does this package make my agent better on my tasks? The same comparison
works for two refs of one package (`hlock add apm:…#v2 --eval` against a locked `#v1`).

### 6.7 Exporting `apm.lock.yaml`

On a mixed team, some people run `apm install` and some run `hlock sync`. For both to get
the same dependency set, hlock can write the APM lockfile from `harness.lock`:

```console
$ hlock export --format apm-lock
✓ wrote apm.lock.yaml  (7 packages, 23 files, lockfile_version 2)
  not exported: harness, 2 models, 1 runtime, config, evidence (APM has no place for them)
next → apm install --frozen   (should change nothing)
```

`hlock export` is one command for every format hlock writes for other tools; the
[audit](audit.md) spec adds `cyclonedx` and `spdx`. `-o FILE` writes elsewhere, and `-o -`
writes to stdout.

What it writes, per declared dependency resolved from a git or `apm:` source:

| `apm.lock.yaml` | From `harness.lock` |
|---|---|
| `repo_url`, `virtual_path` | `source`, split into repository and subpath |
| `resolved_commit`, `resolved_ref` | `commit`, and the tag or branch it came from |
| `constraint`, `resolved_tag` | the same keys, for semver sources (which makes it `lockfile_version: "2"`) |
| `content_hash` | the package tree's digest, computed the way APM computes it (§6.5 fixtures) |
| `deployed_files`, `deployed_file_hashes` | where `sync` placed the files, with per-file SHA-256 after CRLF→LF normalization |
| `depth`, `resolved_by` | the dependency graph from §6.2 |
| `declared_license` | the package's `apm.yml` `license`, when it has one |
| `mcp_servers`, `mcp_configs` | declared MCP servers, in APM's shape |

What it leaves out: everything APM has no field for (the harness, models, runtimes, config,
evidence), path dependencies outside the project, and plugins. The summary line lists what
was left out, so nobody mistakes the APM lock for the whole setup.

Rules:

- **`deployed_files` follows APM's targets, not hlock's.** APM places files by its own
  target conventions (`.agents/skills/` for most harnesses). Export uses the paths APM would
  use for the project's `targets:` (or the lock's harness, mapped to its APM target), so
  `apm install --frozen` on the exported file is a no-op. If hlock placed a file somewhere
  else, export notes the difference rather than writing a path APM wouldn't produce.
- **It refuses to overwrite a newer APM lock.** If `apm.lock.yaml` exists and pins a
  package to a commit that `harness.lock` doesn't know, export stops and says which
  package; `--force` overwrites.
- **`hlock check` reports a stale export.** When the project has an `apm.lock.yaml` that
  hlock exported (it carries `generated_by: hlock …` in the header comment) and the
  dependency set changed since, `check` says so and suggests re-exporting.
- **The tests round-trip it.** Export, run the §6.5 fixtures' reader over the result, and
  compare. When the `apm` CLI is available, an integration test also runs
  `apm install --frozen` and `apm audit` on the exported file.

## 7. Mixed-harness teams

APM's other strong point is fan-out: one manifest deploys into Copilot, Claude Code,
Cursor, Codex and others at once, because a team rarely agrees on one harness. A hlock lock
targets one harness, and `hlock launch <other>` translates its portable parts for a session.

Declared dependencies are portable by construction (skills, MCP servers, instruction
files), so `sync` can install them into more than one harness:

```toml
[target]
harness = "claude-code"
also = ["codex", "cursor-agent"]      # install portable declared deps here too
```

`hlock sync` writes the declared portable dependencies into each harness in `also`, using
the same per-harness placement `launch` uses today (MCP config shape, skills directory,
instruction file name). Harness-native items (plugins, settings, hooks) stay with the
primary harness, and the plan says so. Evidence in the lock was measured on the primary
harness only; `check` doesn't claim it holds for the others.

## 8. CLI

```bash
hlock add KIND SOURCE [--as NAME] [--scope project|user] [--eval] [--dry-run] [--no-install] [--json]
hlock add --adopt NAME              # declare something pin already recorded
hlock remove KIND NAME
hlock outdated [KIND] [--json]       # newer commits/tags/versions for declared deps
hlock upgrade dep:NAME [--eval]       # move a declared dep to its newest match, optionally measured
hlock export --format apm-lock [-o FILE] [--force]   # write apm.lock.yaml (§6.7)
```

`KIND` is `skill`, `mcp`, `instructions` or `plugin`; `hlock add apm:…` needs no kind.
`upgrade dep:` reuses the plan/apply/rollback of `hlock upgrade` (`harness.lock.prev`).

## 9. Phasing

1. The `deps` module: git and path sources, ref and semver resolution, the shared git
   cache; `add`/`remove`; `declared` and `source` in the lock; `check` reports the three
   kinds of difference.
2. Transitive resolution with OpenAPM semantics; reading `apm.yml` and `apm.lock.yaml`;
   `apm:` sources; the conformance fixtures; `hlock export --format apm-lock`.
3. `--eval`, `[*.evidence]`, `outdated`, `upgrade dep:`.
4. `[target] also` for mixed-harness teams.

## 10. Decisions

- **hlock has its own dependency resolver, in Rust, compatible with OpenAPM** (§6). It
  doesn't call the `apm` CLI.
- **`hlock add` installs into project scope by default** (`.claude/skills/` and the
  equivalent for other harnesses, committed with the project), because that's what a
  teammate's `sync` reproduces. `--scope user` is for personal setups.
- **hlock writes `apm.lock.yaml` only on request**, with `hlock export --format apm-lock`
  (§6.7), never as a side effect of `add` or `sync`.
- **The resolver uses the [`semver`](https://crates.io/crates/semver) crate** for ranges.
  It's the only new dependency of `harness-lock-core`; git is already called as a process.
- **`--eval` is opt-in**, even when the project has an eval pack. Measuring costs money and
  minutes, and `add` should stay fast. When a pack exists, `add`'s next-step hint suggests
  `hlock add … --eval` (or `hlock upgrade dep:NAME --eval` for an existing dependency).
- **`--eval` was called `--try` in an earlier draft.** Renamed so it isn't read as a dry
  run; `add` has a real `--dry-run` (§3.3).

## 11. Implementation notes

Choices the implementation made where this spec left room:

- **Where packages are declared.** `hlock add apm:…` writes to a `[packages]` table.
- **Instruction files** install into the harness's rules directory, a new profile key
  (`rules = { dir, ext }`: `.claude/rules/*.md`, `.cursor/rules/*.mdc`,
  `.github/instructions/*.instructions.md`, the paths APM's targets use). A harness without
  one gets the file as its own project instruction file, and only when it doesn't exist yet.
- **Tag patterns** are APM's four (`v{version}`, `{name}--v{version}`, `{name}-v{version}`,
  and a bare `{version}` when no other matches), not only the first two.
- **All declared packages resolve as one graph**, so constraints from two roots on a shared
  package are intersected; a package reached from several roots belongs to the first.
- **A project with no lock is pinned first** by `add`, so `check` on the new lock doesn't
  report the rest of the setup as drift.
- **`sync` locks new or changed declarations** before planning (`--frozen` refuses to),
  the way `cargo build` updates `Cargo.lock`.
- **`apm.lock.yaml` export follows APM 0.34's writer**: `owner/repo` with `host`, no
  `resolved_ref` for an unpinned declaration, the skill directory and its files in
  `deployed_files`, and no `deployments` ledger (APM derives it). Resolution follows the
  OpenAPM spec where APM's CLI differs (§6.2).
- **`--eval` runs trials through the resolver's adapters.** A code harness gets each
  side's skills and MCP servers; CLI harnesses on the `local` backend run on the reference
  harness, which ignores them, and `add` says so.
- **Plugins** install through `claude plugin install` and are read back from Claude Code's
  `installed_plugins.json`; other harnesses' plugins aren't supported yet.
