# Declaring dependencies

[`hlock pin`](pin.md) records the setup you already have. `hlock add` asks for a
dependency you want: it declares it in `harness.toml`, resolves it to an exact commit (or
package version) and digest in `harness.lock`, and installs it into your harness. A
teammate's [`hlock sync`](sync.md) then fetches the same commit and checks its digest, so
nothing needs vendoring.

```console
$ hlock add skill github:acme/skills/skills/sql@^1.2
✓ resolved skill sql github:acme/skills/skills/sql@^1.2 @ v1.3.0  sha256:236af73c…
pinned the project's claude-code setup into harness.lock first
✓ copied skill sql → .claude/skills/sql
✓ declared skill sql in a new harness.toml, pinned in harness.lock
next → hlock check
```

`^1.2` matched the highest tag in that range (`v1.3.0`). In a project with no lock yet,
`add` pins the project's setup first, so the lock still says what else is installed. Only the
new entry is added to the lock; nothing else is captured again. The manifest now says
what you asked for:

```toml title="harness.toml"
[skills]
sql = "github:acme/skills/skills/sql@^1.2"
```

and the lock says what that resolved to:

```toml title="harness.lock"
[[skill]]
name = "sql"
origin = "third-party"
source = "github:acme/skills/skills/sql@^1.2"
digest = "sha256:236af73c06f7675ed3bce1156d2a59d921e47e0fa390c72a7a9fb5ebeccff23d"
scope = "project"
path = ".claude/skills/sql"
declared = true
constraint = "^1.2"
resolved-tag = "v1.3.0"
resolved-ref = "v1.3.0"
commit = "ec6c48f077090bdb2182e1f870c75a6b2f3e7026"
```

`add` never overwrites a file or skill it didn't put there: if one is in the way, it stops
and says so (use `--as` to pick another name). What it replaces of its own, it sets aside in
`.hlock/bak/` first, where the harness doesn't read it. A remote MCP server's URL with a
credential in it is refused, since the source is written into files you commit.

Dependencies install into the project by default (`.claude/skills/`, and the equivalent for
other harnesses), so they're committed with it and a teammate's `sync` reproduces them.
Pass `--scope user` to install one for yourself only.

## What you can add

| Kind | Source | Example |
|---|---|---|
| `skill` | GitHub, with a subpath and an optional ref or range | `github:anthropics/skills/skills/pdf`, `github:acme/skills/skills/sql@^1.2` |
| `skill`, `instructions` | Any git URL | `git+https://gitlab.com/acme/skills#path=sql&tag=v1.2.0` |
| `instructions` | A single file in a repository | `github:acme/rules/style.md` |
| `skill`, `instructions` | A path | `./skills/sql` |
| `mcp` | An npm or PyPI package (run with `npx` or `uvx`) | `npm:@modelcontextprotocol/server-github@^1`, `pypi:mcp-server-fetch` |
| `mcp` | A remote server | `https://mcp.linear.app/mcp` |
| `plugin` | A Claude Code plugin from a marketplace it knows | `plugin:acme-marketplace/acme-tools` |
| (none) | An APM package | `apm:github/awesome-copilot/plugins/context-engineering` |

After `@` (or `#`), a branch, tag or full commit SHA is a ref; anything that reads as a
version range is a range: `^1.2`, `~1.4.0`, `>=1.0 <2`, `1.x`, `1.2 - 1.4`, `^1 || ^3`, or a
bare `1.2.0` (which means exactly that version). A range matches tags named `v{version}`,
`{name}--v{version}` or `{name}-v{version}`, where `{name}` is the subpath's last segment (or
the repository's name). Without a ref, the dependency follows the default branch, and the
lock pins the commit it was at. Private repositories work through git's own credentials, as
in [`hlock pull`](pull.md).

The name is the source's last segment (`sql`, `style`, `github` for
`server-github`); `--as NAME` picks another.

An instruction file goes in the harness's rules directory: `.claude/rules/` for Claude Code,
`.cursor/rules/` for Cursor, `.github/instructions/` for Copilot. A harness without one
reads instructions only from its own file (`AGENTS.md`), so the dependency becomes that
file, if the project doesn't have one already.

## Look before you add

`--dry-run` resolves the source and prints what `add` would write, without writing,
installing or measuring anything:

```console
$ hlock add skill github:acme/skills/skills/sql@^1.2 --dry-run
✓ resolved skill sql github:acme/skills/skills/sql@^1.2 @ v1.3.0  sha256:236af73c…
would write
  harness.toml  [skills] sql = "github:acme/skills/skills/sql@^1.2"
  harness.lock  [[skill]] sql v1.3.0
  claude-code  .claude/skills/sql
```

## Measure before you keep it

Every installed skill costs context at startup, whether it's used or not. `--eval` measures
whether a dependency helps on your tasks. It runs the project's
[eval pack](eval-packs.md) twice, on the locked stack and on the locked stack plus the
dependency, and asks before keeping it:

```console
$ hlock add skill github:acme/greeter --eval
✓ resolved skill greeter github:acme/greeter @ bf73e0c  sha256:e897751c…
measured on ./evals (search split, 3 tasks × 1 seed, code harness agent)
                 success  cost/task  p50 latency
without greeter  0.00     $0.0000    0.02s
with greeter     1.00     $0.0000    0.02s        +1.00 success
keep greeter? [Y/n]
```

The candidate is installed into an isolated root (`.hlock/env/eval/`), so your project and
your own config aren't touched until you answer. Answering no leaves the project as it was.
Trials are cached by the stack, the content each side sees, the task, the seed and the eval
pack, so the baseline usually costs nothing the second time. When you keep it, the lock
records the comparison:

```toml title="harness.lock"
[skill.evidence]
eval-pack = "sha256:35b7ec2648cddb57360d402ecf2c50da20843fc904a1ded50c96689fb379b8d8"
split = "search"
trials = 3
baseline-success-rate = 0.0
success-rate = 1.0
baseline-cost-usd-per-task = 0.0
cost-usd-per-task = 0.0
digest = "sha256:e897751cd4763e9ce34cf19bf6df206e8f9dd1cb8de726a8ad1c6ef88260b8b0"
```

`hlock check` reports the evidence as stale once the dependency's digest changes.

!!! note "What runs the trials"
    `--eval` runs trials the way [`hlock resolve`](resolve.md) does. A
    [project harness](harness-profiles.md) (`[harnesses.<id>]`) runs for real, with each
    side's skills in `HLOCK_SKILLS_DIR` and its MCP servers in `HLOCK_MCP_CONFIG`. On the
    default `offline` backend, CLI harnesses such as Claude Code run on the offline reference
    harness, which doesn't read skills, so both sides score the same, and `add` says so.
    To measure those, set `[backend] target` to `local` or `flyte`.

`--eval` isn't a dry run: it installs (in isolation) and runs trials, which can cost money
and take minutes. Without an eval pack it stops and points you at `hlock evals init`.

## Check what's declared

`hlock check` compares the declared dependencies with the lock and with what's installed:

| Problem | Meaning | Fix |
|---|---|---|
| `missing` | declared and locked, but not installed | `hlock sync` |
| `changed` | installed, but its digest differs from the lock | `hlock sync` restores the locked copy (your edit goes to `.hlock/bak/`) |
| `unlocked` | declared, but not in the lock (or the lock has another source) | `hlock sync` |
| `removed` | locked as declared, but no longer declared | `hlock remove KIND NAME` |
| `undeclared` | in the harness's project skill directory, but neither declared nor pinned | `hlock pin`, then `hlock add --adopt NAME` |

```console
$ hlock check
✗ dependencies: 1 declared dependency differs
  changed skill sql differs from the lock at .claude/skills/sql  → hlock sync (restores the locked copy; your edit is kept in .hlock/bak/)
```

Declaring is opt-in per item: whatever `hlock pin` recorded without a declaration stays
pinned as before. `hlock add --adopt NAME` declares something `pin` already recorded,
using the source the lock knows for it (a skill's git checkout, an MCP server's npm package,
a plugin's marketplace), pinned where it is now.

## Sync on another machine

A declared dependency with a commit is never vendored. `sync` fetches it at that commit and
checks its digest before putting it in place:

```console
$ hlock sync --yes
Sync plan for Claude Code 2.1.3 → .hlock/env/claude-code/config
✓  harness       Claude Code 2.1.3
●  skill         Fetch skill sql → .claude/skills/sql  at v1.3.0
●  instructions  Fetch instructions style → .claude/rules/style.md  at b4472ab009
●  mcp           MCP server github in .mcp.json
✓ Fetch skill sql → .claude/skills/sql
✓ Fetch instructions style → .claude/rules/style.md
✓ MCP server github in .mcp.json
✓ in sync at .hlock/env/claude-code/config  your own config wasn't touched
use it: CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config claude
to restore into your own config instead: hlock sync --in-place
```

The lock is authoritative. When every declared dependency's lock entry has the same
source as the declaration, `sync` looks nothing up upstream, and a dependency already
installed at its digest is skipped. When you've edited `harness.toml` (a new dependency, a
changed range), `sync` resolves what changed first and updates the lock:

```console
$ hlock sync --yes
updated harness.lock for the declared dependencies
  + mcp github https://api.githubcopilot.com/mcp/
```

In CI, `hlock sync --frozen` fails instead of changing the lock, like `cargo build --locked`.

Fetching goes through one git cache shared by every project on the machine,
`~/.hlock/cache/git/`. It holds bare repositories, fetched shallowly and without file
contents until a path is read, so ten skills from one repository cost one fetch.
`git ls-remote` resolves refs and ranges, one round trip per repository, in parallel.
Fetched trees are kept by digest in the project's `.hlock/store/trees/`.

## Keep them current

`hlock outdated` looks upstream. `wanted` is the newest the declaration allows, and
`latest` the newest there is:

```console
$ hlock outdated
name  kind   current  wanted  latest  source
sql   skill  v1.3.0   —       v2.0.0  github:acme/skills/skills/sql@^1.2
```

`hlock upgrade dep:NAME` resolves a declared dependency again to the newest match, shows
the move, and applies it with `--apply`. It keeps the previous lock as
`harness.lock.prev` for `hlock upgrade --rollback`. With `--eval`, it measures the new
version against the locked one first.

`hlock remove KIND NAME` undeclares a dependency, takes it out of the lock, and out of the
harness. A skill or instruction file you've changed by hand since is left where it is.

## Mixed-harness teams

A lock targets one harness, but a team rarely agrees on one. The portable declared
dependencies (skills, MCP servers and instruction files) can also go into other harnesses:

```toml title="harness.toml"
[target]
harness = "claude-code"
also = ["codex", "cursor-agent"]
```

`hlock sync` then installs them into each harness in `also`, where that harness reads them
(`.agents/skills/` for Codex, Cursor's MCP config, and so on). Plugins, settings and hooks
stay with the primary harness, and the plan says so. The lock's evidence was measured on the
primary harness only.

`also` can name a [desktop app](harnesses.md#desktop-apps), such as `claude-desktop`, which
gets the MCP servers in its own config:

```toml title="harness.toml"
[target]
harness = "claude-code"
also = ["claude-desktop"]
```

An app reads only its own config, so `hlock sync` writes it only with `--apps`, while the
primary harness stays in its isolated root. Until then its steps are optional: `sync` lists
them ("1 step for Claude Desktop waits for hlock sync --apps") and still exits `0`. An app
has no project config, so a project-scoped dependency goes into its user config, and one it
can't hold (a remote MCP server, for Claude Desktop) is left for you to add there. An app is
left out of an explicit `--prefix` and of `--docker`.

## APM packages

!!! tip "Already using APM?"
    [Coming from APM](../getting-started/apm.md) explains what hlock adds on top of APM and
    how to use the two together.

[APM](https://github.com/microsoft/apm) (Agent Package Manager) packages are git
repositories, or paths in them, with an `apm.yml`. hlock resolves them itself, in Rust, by
the rules of the [OpenAPM v0.1](https://github.com/microsoft/apm) spec, so a project needs
neither APM nor Python on the path to install them:

```bash
hlock add apm:github/awesome-copilot/plugins/context-engineering
```

A package brings its skills (`SKILL.md` at its root, `skills/<name>/` and
`.apm/skills/<name>/`), its instruction files (`.apm/instructions/*.instructions.md`), its
MCP servers (`dependencies.mcp`), and the packages its own `apm.yml` depends on. Each lands
in the lock as its own entry with `origin = "apm"` and `required-by` naming the packages
above it, and each package gets a `[[package]]` table. hlock doesn't install a package's
prompts, agents or hooks; the `[[package]]` entry counts what it left out. An MCP server a
package brings is recorded in the lock but not configured, because it starts a process from a
file the project didn't write; `add` and `sync` say so, and you can configure it yourself if
you trust it. The MCP servers your own `apm.yml` declares are configured like any other.

All the declared packages resolve as one graph, breadth-first in declaration order:

- When several packages constrain one package, the highest version that satisfies all of
  them wins. If none does, `add` fails and names both chains:

    ```console
    $ hlock add apm:acme/other
    ✗ error: github.com/acme/base: no version satisfies every constraint on it:
      app -> base#~1.0.0
      other -> base#^2.0.0
      app -> lib -> base#^1.0.0
    ```

- When two packages provide a skill or instruction file of the same name, the project's own
  wins, then the first declared.
- A dependency chain deeper than 50 packages fails, and `conflict_resolution: nest` is refused
  (OpenAPM v0.1 reserves it).

These are the OpenAPM v0.1 spec's rules, and hlock runs the spec's own conformance fixtures
(`crates/harness-lock-core/tests/openapm/`). APM's CLI (0.34) implements some of them
differently while the spec settles: it keeps the first version of a package it meets, and
reads a narrower set of version ranges. A project whose packages don't conflict gets the
same versions from both.

### A project that already uses APM

If the project has an `apm.yml`, its `dependencies` are declared dependencies, exactly as if
they were in `harness.toml`. There's nothing to convert, and APM users keep editing
`apm.yml`. When both files declare the same package, they must agree, or `check` reports a
conflict. When `apm.lock.yaml` is present and agrees with every requirement on a package,
hlock takes its commit as given, so the two tools never disagree about versions. When
`hlock pin` finds files that `apm install` deployed, it records them as dependencies of their
package, so `sync` fetches them instead of vendoring them.

### Teammates who use APM

`hlock export --format apm-lock` writes `apm.lock.yaml` from `harness.lock`, so teammates who
run `apm install` get the same dependency set as those who run `hlock sync`:

```console
$ hlock export --format apm-lock
✓ wrote apm.lock.yaml  (2 packages, 2 files, lockfile_version 2, target claude)
  not exported: harness, 1 model, config (APM has no place for them)
next → apm install --frozen   (should change nothing)
```

The file is written the way APM 0.34 writes its own lock: `content_hash` and the per-file
hashes computed as APM computes them, and `deployed_files` at the paths APM uses for the
project's `targets:` (or for the lock's harness). Where hlock placed a file elsewhere,
export notes the difference rather than writing a path APM wouldn't produce. hlock tests
that `apm install --frozen` and `apm audit` change nothing on an exported file.

hlock writes `apm.lock.yaml` only when asked, never as a side effect of `add` or `sync`. It
won't overwrite one that pins a commit `harness.lock` doesn't know (someone ran
`apm install` since); `--force` overwrites it. `hlock check` reports an exported file
that's out of date. `-o FILE` writes elsewhere, and `-o -` writes to stdout.
