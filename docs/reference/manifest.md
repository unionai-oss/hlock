# `harness.toml`

The manifest declares what the resolver may try and what "best" means. `hlock init` writes
a starter one. Every section is optional, but a useful manifest has at least
`[evals]`, `[candidates]`, and `[objective]`.

```toml title="harness.toml"
[project]
name = "support-agent"

[target]
harness = "claude-code"

[evals]
packs = ["./evals"]
seeds = 3

[evals.splits]
search = 0.7

[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]

[candidates]
decision = ["none", "rules:default"]
harness = ["reference"]

[candidates.policy]
retries = [1]

[objective]
primary = "success-rate"
select = "min-cost"

[objective.constraints]
success-rate = ">=0.8"
cost-usd-per-task = "<=0.05"

[budget]
strategy = "grid"
max-trials = 200
max-spend-usd = 25.0
random-seed = 0

[backend]
target = "local"
```

`resolve` and `tune` refuse a manifest with a key they don't know in `[budget]`,
`[objective]`, `[evals]`, `[evals.splits]`, `[candidates]`, `[backend]`, `[target]`,
`[environment]`, `[[environment.binaries]]`, `[[secrets]]`, `[workspace]`,
`[[workspace.repos]]`, `[[workspace.files]]`, or at the top level, and name the closest
known key: a mistyped key is otherwise ignored.

## `[project]`

| Key | Description |
|---|---|
| `name` | Project name, recorded as the lock's stack name |
| `display-name` | The agent's name in registry listings and launch UIs (`"RBAC reviewer"`): one line of text |
| `avatar` | An image in the package, relative to `harness.toml` (`"./avatar.png"`) |

The lock records `display-name` and `avatar` in its own
[`[project]`](lockfile.md#workspace-commands-profile-and-persona), with the avatar's digest.

## `[evals]`

| Key | Default | Description |
|---|---|---|
| `packs` | `[]` | Paths to [eval pack](../guides/eval-packs.md) directories |
| `seeds` | `3` | Runs per task per candidate |
| `splits.search` | `0.7` | Fraction of tasks in the search split. The rest is holdout. |
| `judge.model` | the pack's `[pack.judge]` | The model that scores [`judge` graders](../guides/eval-packs.md#judges), as `"provider:model"` |
| `judge.base-url` | the provider's API | An OpenAI-compatible endpoint, for other providers and local models |
| `judge.key-env` | the provider's usual variable | The environment variable the judge's API key is read from |
| `judge.protocol` | inferred | `systemone` or `decisions` for a decision model, `chat` for an LLM |
| `judge.fallback` | | An LLM that re-judges a decision model's uncertain grades, and its `judge.fallback-base-url` |
| `judge.uncertain` | `0.15` | How close to `pass` a decision model's grade is uncertain |

This section used to be called `[tasks]`, and that name still works.

## `[candidates]`

The search space. The resolver tries every combination.

| Key | Description |
|---|---|
| `llm.<role>.any_of` | Candidate models for a role, as `"provider:model"`. v0.1 tunes the `planner` role. |
| `decision` | Candidate decision models, e.g. `"none"`, `"rules:default"` |
| `harness` | Candidate harnesses: a list of names, or tables with `name`, `pattern`, and `config` |
| `policy.<key>` | Policy values, e.g. `retries = [1]`, `temperature = [0.2]`. `resolve` uses the first value. [`tune`](../guides/tune.md) searches retries and temperature itself. |

v0.1 ships the `reference` provider (models `qwen-1.5b`, `qwen-7b`, `qwen-32b`) and the
`reference` harness. Real providers run through the worker on the
[`local` and `flyte` backends](../guides/backends.md).

## `[objective]`

| Key | Default | Description |
|---|---|---|
| `primary` | `"success-rate"` | The metric being optimized. `success-rate` is the only one; anything else is an error. |
| `select` | `"min-cost"` | How to choose among stacks that pass: `"min-cost"` or `"max-quality"`. Anything else is an error. |
| `constraints` | `{}` | Hard limits, as a table of metric to comparison |

Constraint metrics are `success-rate` and `cost-usd-per-task`. Comparisons are `>=`,
`<=`, `>`, and `<` followed by a number. Any other metric, or a value that isn't a
comparison with a number, is an error. When no stack satisfies every constraint,
`resolve` fails and names the closest stack and what it missed; see
[Selection](../concepts/index.md#selection).

## `[budget]`

| Key | Default | Description |
|---|---|---|
| `strategy` | `"grid"` | Search strategy. `grid` is the only one; anything else is an error. |
| `max-trials` | | Upper bound on search trials, spread round-robin across the stacks before any run ([The resolver](../concepts/index.md#the-resolver)) |
| `max-spend-usd` | | Upper bound on spend: no trial starts that would likely pass it. Counts the holdout check too. |
| `random-seed` | `0` | Seed for the search/holdout split |

The snake_case spellings (`max_trials`, `max_spend_usd`, `random_seed`) older manifests use
still load.

## `[target]`

| Key | Description |
|---|---|
| `harness` | The harness this lock is for: an id or alias from `hlock harness list`, or a `[harnesses.<id>]` table. `resolve` narrows `[candidates] harness` to it and `check` enforces it. See [Harnesses](../guides/harnesses.md#target-a-harness). |
| `also` | More harnesses `hlock sync` installs the declared skills, MCP servers and instruction files into, e.g. `["codex", "cursor-agent"]`. See [Mixed-harness teams](../guides/dependencies.md#mixed-harness-teams). |

## `[harnesses.<id>]`

A project harness: an agent built on a framework, or any executable. It accepts every
[harness profile](../guides/harness-profiles.md) field, plus these at the top level:

| Key | Description |
|---|---|
| `entrypoint` | Python `module:callable` |
| `command` | Executable and arguments; `{prompt}`, `{model}`, and `{workspace}` are placeholders |
| `framework` | A framework profile (`pydantic-ai`, `deepagents`, `langgraph`, `openai-agents`, `claude-agent-sdk`, `google-adk`, `smolagents`, `crewai`) whose packages are pinned |
| `packages` | Extra packages to pin, e.g. `["pypi:httpx"]` |
| `python` | Interpreter for `entrypoint` (default `HL_PYTHON`, else `python3`) |
| `env` | Extra environment variables |

One of `entrypoint` or `command` is required.

```toml
[harnesses.triage]
framework = "pydantic-ai"
entrypoint = "triage.agent:run"
```

## `[backend]`

| Key | Default | Description |
|---|---|---|
| `target` | `"offline"` | `offline`, `local`, `flyte`, or the name of a [custom backend](../guides/backends.md#write-your-own-backend). The old names `devbox` (now `local`) and `cluster` (now `flyte`) still work and print a note. See [Backends](../guides/backends.md). |

## `[backends.<name>]`

Defines a backend, or configures a built-in one. hlock finds a backend that has no table
here as an `hlock-backend-<name>` executable on `PATH`, or as an installed
`hlock.backends` entry point.

```toml title="harness.toml"
[backends.ray]
command = ["ray-backend", "--address", "auto"]

[backends.logged]
python = "my_backend:LoggedBackend"

[backends.flyte]
options = { project = "agents", domain = "development" }
```

| Key | Description |
|---|---|
| `command` | An executable that speaks the [backend contract](../concepts/architecture.md#the-backend-contract): a program, or a list of a program and its arguments. hlock appends the request and response paths |
| `python` | A Python backend as `"module:attribute"`, a `hlock.backends.Backend` class or instance, loaded by hlock's worker |
| `options` | A table sent to the backend with every work item, as `backend_options` |

A table has at most one of `command` and `python`. `local` and `flyte` take only
`options`, and `offline` can't be configured.

## `[optimizers.<name>]`

Defines an [optimizer](../guides/optimizers.md), or configures a built-in one (`hlock`,
`harness2`). An optimizer is what `tune` searches with; the backend is where it runs. hlock
finds an optimizer that has no table here as an `hlock-optimizer-<name>` executable on
`PATH`, or as an installed `hlock.optimizers` entry point.

```toml title="harness.toml"
[optimizers.harness2]
options = { k = 2, mode = "sequential" }

[optimizers.mine]
python = "mine_opt:Mine"

[optimizers.ace]
command = ["ace-optimizer", "--fast"]
```

| Key | Description |
|---|---|
| `command` | An executable that answers `harness.optimize` in the [backend contract](../concepts/architecture.md#the-backend-contract)'s envelope. hlock appends the request and response paths |
| `python` | A Python optimizer as `"module:attribute"`, a `hlock.optimizers.Optimizer` class or instance |
| `options` | A table sent with the request, as `optimizer.options` |

A table has at most one of `command` and `python`. The built-ins take only `options`.

## `[tune]`

How [`hlock tune`](../guides/tune.md#tune-the-harness-text) edits the harness's text.

```toml title="harness.toml"
[tune]
edit = ["instructions", "skill"]
optimizer = "hlock"
proposer = "anthropic:opus"
trials = 2
alpha = 0.05
max-cost-increase = 0.25
budget = { rollouts = 100, usd = 20 }
```

| Key | Default | Description |
|---|---|---|
| `edit` | `["instructions"]` from `--depth prompt` on, else none | What the optimizer may change: `instructions` (with their roles: guardrails, memory, tool guidance), `skill`, `subagent`, `script`, `middleware`. Empty means no optimize stage |
| `optimizer` | `"hlock"` | What searches for edits: the built-in optimizer, [`"harness2"`](../guides/tune.md#harness2), or [your own](../guides/optimizers.md#write-an-optimizer). Where it runs is `[backend] target` |
| `proposer` | | The model that drafts edits. The built-in optimizer tries fixed guidance without one |
| `trials` | `2` | Trials per task in each evaluation and in the acceptance runs |
| `alpha` | `0.05` | The sign test's significance level an accepted candidate must reach |
| `max-cost-increase` | `0.25` | How much more per trial an accepted candidate may cost |
| `budget.rollouts` | `100` | The most trials the optimizer may run |
| `budget.usd` | | The most it may spend on trials |

## `[sandbox]`

The [sandbox](../guides/sandbox.md) the lock runs in. `pin`, `resolve` and `tune` copy it
into the lock's `[policy.sandbox]`; `resolve` and `tune` also run code-harness trials and
`tests` graders in it.

```toml title="harness.toml"
[sandbox]
runner = "docker"
image = "ghcr.io/acme/agent@sha256:4f1c9e0b7d2a"
network = "open"
secrets = "proxy"
applies-to = ["agent", "trials", "graders"]
grader-image = "ghcr.io/acme/ci@sha256:9d03…"
```

| Key | Default | Description |
|---|---|---|
| `runner` | `"none"` | `docker`, [`openshell`](../guides/sandbox.md#run-the-lock-on-openshell), or a runner defined in `~/.config/hlock/sandbox.toml` or `[sandbox.runners.<name>]` |
| `image` | | The image `docker` or `openshell` runs (required for them). Pin it `@sha256:` |
| `network` | `"open"` (`"none"` for `openshell`) | `open`, `none`, or a list of hosts the sandbox may reach (`["api.github.com", "*.pypi.org"]`); `openshell` takes `none` only |
| `secrets` | `"env"` (`"proxy"` for `openshell`) | `env` (provider keys as environment variables) or `proxy` (a [key proxy](../guides/sandbox.md#keep-provider-keys-out-of-the-agents-reach) on `docker`, OpenShell providers on `openshell`) |
| `applies-to` | all three | What runs inside: `agent`, `trials`, `graders` |
| `profile` | | A runtime policy file in the project (a seatbelt profile, an OpenShell policy), pinned by digest and passed as `{profile}` (to `openshell`, as `--policy`) |
| `grader-image` | `image` | The image `tests` graders run in |

`[sandbox.runners.<name>]` defines a [command runner](../guides/sandbox.md#define-your-own-runner)
for the project: `command`, `mount-args`, `mount-ro-args`, `network-args` and `networks`.

A list of hosts is an allowlist: each is a lowercase hostname, with `*.` for its subdomains,
and no scheme, port or path. The lock records `network = "allowlist"` and the `hosts`. Only
a command runner with an `allowlist` entry in `network-args` (or `networks`)
[enforces it](../guides/sandbox.md#define-your-own-runner); `docker` and `openshell` refuse it.

## `[environment]`

What the agent needs installed: packages for each installer, and binaries pinned by URL
and SHA-256. The lock records it as declared, in its own
[`[environment]`](lockfile.md#declared-environment-and-secrets); hlock doesn't install any
of it yet.

```toml title="harness.toml"
[environment]
apt = ["protobuf-compiler", "ripgrep"]    # Debian and Ubuntu
brew = ["protobuf", "ripgrep"]            # macOS
pip = ["ruff==0.6.9"]
npm = ["pnpm@9"]
cargo = ["cargo-nextest"]
go = ["golang.org/x/tools/gopls@v0.16.2"]

[[environment.binaries]]
url = "https://example.com/tool-1.2.3-linux-x64"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
path = "/usr/local/bin/tool"
```

| Key | Description |
|---|---|
| `apt`, `brew`, `pip`, `npm`, `cargo`, `go` | Packages, as that installer names them (`ruff==0.6.9`, `pnpm@9`, `golang.org/x/tools/gopls@v0.16.2`). Each is one word, not starting with `-` |
| `[[environment.binaries]]` | A file to download: `url` (`http://` or `https://`), `sha256` (64 lowercase hex digits) and `path`, the absolute path it's installed at, with no `..`. All three are required, and each `path` appears once |
| `ignore` | Programs the stack names but doesn't need installed, so [`hlock env suggest`](cli.md#env-suggest) stops suggesting them. Each is a program name, not a path |

The system installers say the platform: `apt` is for Debian and Ubuntu, `brew` for macOS,
and a program often has a different package name in each (`protobuf-compiler`,
`protobuf`), so each lists its own. `pip`, `npm`, `cargo` and `go` apply everywhere.

[`hlock env suggest`](cli.md#env-suggest) lists the programs the lock runs that these lists
don't cover, and `--write` adds their packages.

## `[[secrets]]`

The secrets the agent needs, declared up front so a launcher can ask for them before it
starts. Values never go in the manifest or the lock: each secret is supplied as
`HLOCK_SECRET_<name>`, from the environment, the secrets file or the keychain
([`hlock secret`](cli.md)).

```toml title="harness.toml"
[[secrets]]
name = "GITHUB_TOKEN"
used-by = ["mcp:github", "skill:release"]
optional = false
description = "Opens PRs on the user's behalf."
```

| Key | Default | Description |
|---|---|---|
| `name` | | Letters, digits and `_`. `GITHUB_TOKEN` and `HLOCK_SECRET_GITHUB_TOKEN` are the same secret, and each is declared once |
| `used-by` | `[]` | What needs it, e.g. `mcp:github`, `skill:release` |
| `optional` | `false` | `true` when the agent runs without it |
| `description` | | What it's for |

The lock merges each into its [`[[secret]]`](lockfile.md#declared-environment-and-secrets)
entries.

## `[workspace]`

The state the agent's workspace starts from: repositories to check out, and files from the
package to copy in. The seed is portable; the live state that grows from it isn't, and is
never locked. The lock records the seed in its own
[`[workspace]`](lockfile.md#workspace-commands-profile-and-persona), with each file's
digest. hlock doesn't check out or copy any of it yet.

```toml title="harness.toml"
[[workspace.repos]]
url = "https://github.com/org/repo"
ref = "main"
path = "src/repo"

[[workspace.files]]
path = "./seed/NOTES.md"    # in the package
target = "NOTES.md"         # in the workspace
```

| Key | Description |
|---|---|
| `[[workspace.repos]]` | A repository: `url` (`https://`, `http://`, `ssh://` or `git@`), an optional `ref` (a branch, tag or commit; the default branch without one), and `path`, where it's checked out in the workspace |
| `[[workspace.files]]` | A file or directory: `path`, relative to `harness.toml`, and `target`, where it goes in the workspace |

Paths are relative, with no `..`, and each place in the workspace is seeded once.

## `[commands]`

Slash commands the agent offers, by name, each a file in the package. The lock records
each as a [`[[command]]`](lockfile.md#workspace-commands-profile-and-persona) with its
digest, so an edited command shows up like an edited skill. hlock doesn't install them into
the harness yet.

```toml title="harness.toml"
[commands]
review = "./commands/review.md"
```

A name is letters, digits, `-`, `_` and `.`, not starting with `-` or `.`; a path is
relative to `harness.toml`, with no `..`.

## `[context]`

```toml title="harness.toml"
[context]
profile = "forked"
```

| Key | Description |
|---|---|
| `profile` | Whose memory and personal config the agent starts from: `shared` (the user's own), `forked` (a copy of them that writes back nowhere), or `none` (neither). A demo or customer-facing agent shouldn't see its author's memory; a personal one should. The lock records it as `[context] profile`; `launch` doesn't apply it yet |

hlock checks `[environment]`, `[[secrets]]`, `[workspace]`, `[commands]`,
`[context] profile`, and `[project]` `display-name` and `avatar` when it reads
`harness.toml`: a bad digest, path, package, name or mode is an error rather than ignored.
`sync`, `resolve` and `tune` also read the files `[workspace]`, `[commands]` and `avatar`
name, and stop when one is missing.

## Dependencies

`[skills]`, `[mcp]`, `[instructions]`, `[plugins]` and `[packages]` declare dependencies:
what [`hlock add`](../guides/dependencies.md) writes, and what `hlock sync` and
`hlock check` hold the lock to. Each entry is a name and a source:

```toml title="harness.toml"
[skills]
sql = "github:acme/skills/skills/sql@^1.2"
mine = { source = "./skills/mine", editable = true }
review = { source = "github:acme/skills/skills/review", scope = "user" }

[mcp]
github = "npm:@modelcontextprotocol/server-github@^1"
linear = "https://mcp.linear.app/mcp"

[instructions]
style = "github:acme/rules/style.md"

[plugins]
acme-tools = "plugin:acme-marketplace/acme-tools"

[packages]
context-engineering = "apm:github/awesome-copilot/plugins/context-engineering"
```

| Form | Meaning |
|---|---|
| `"SOURCE"` | A source, installed for the project. See [What you can add](../guides/dependencies.md#what-you-can-add) |
| `{ source = …, scope = "user" }` | Installed for you, not the project |
| `{ source = …, editable = true }` | A path you're working on: `check` warns, rather than fails, when it changes |
| `{ git = URL, path = …, tag = … }` | The same as `"git+URL#path=…&tag=…"` (`rev`, `ref`, `branch`, `commit` and `version` work too) |
| `{ gh = "OWNER/REPO/PATH" }`, `{ npm = … }`, `{ pypi = … }`, `{ url = … }`, `{ apm = … }`, `{ plugin = … }` | Shorthands for the matching source |

A `[tools]` entry with an `mcp` source (`github = { mcp = "npm:…" }`, as the v0 spec
spells it) counts as an MCP server too. `[instructions] files` and the `[skills.builtin]` and
`[skills.trust]` tables aren't dependencies. An `apm.yml` next to the manifest declares
dependencies as well ([APM packages](../guides/dependencies.md#a-project-that-already-uses-apm)).

## Reserved sections

`[enforce]`, and `[context]`'s keys besides `profile`, are parsed and included in the
manifest hash. They're placeholders for the peripherals described in the
[design spec](https://github.com/unionai-oss/hlock/blob/main/specs/v0.md). The resolver
doesn't search over dependencies yet: `hlock resolve` treats `[skills]` and `[tools]` names
as fixed parts of every stack. It keeps the pins `hlock add` and `hlock sync` recorded for
declared dependencies (commits, versions, digests, and the secrets their MCP servers use),
so `hlock add` then `hlock resolve` leaves a lock that `hlock check` passes.

## The manifest hash

A resolved lock records a SHA-256 hash of the manifest file exactly as written, comments
and whitespace included: the text the run read, even if the file changes during it.
[`hlock check`](../guides/ci.md) compares it to the live file, so any edit to
`harness.toml` needs a fresh `hlock resolve`.

A pinned lock with declared dependencies records a hash of the dependency sections only
(`[skills]`, `[mcp]`, `[instructions]`, `[plugins]`, `[packages]` and `[tools]`, and
`[environment]`, `[[secrets]]`, `[workspace]`, `[commands]`, `[context] profile`, and
`[project]` `display-name` and `avatar` when there are any), so editing `[candidates]` or
`[evals]` in the same file doesn't invalidate the pin.

## `[deciders.<use>]`

[Decision models](../guides/deciders.md), by what they decide: `approve` and `stop` in the
agent (and `route` and `tool`), and `default`, `judge` and `draft` for hlock's own
judgments.

```toml
[deciders.approve]
model = "typesafe:jev-1.13.0"
policy = ".hlock/approve.md"
```

| Key | Default | Description |
|---|---|---|
| `model` | | `provider:model`, or `none` |
| `any-of` | | Candidates for `hlock resolve`, instead of `model` |
| `may` | `["deny", "ask"]` | `approve` only: which of `allow`, `ask`, `deny` it may answer |
| `policy` | | A file of rules added to the question |
| `min-probability` | allow 0.9, ask 0.5, deny 0.8; stop 0.9; route 0.6; tool 0.3 | How sure it must be to act: a number, a table per answer for `approve`, or a list for `resolve` to choose from |
| `hooks` | the harness profile's | Harness events to wire it to |
| `timeout-ms` | `2000` | How long a hook waits |
| `base-url`, `key-env`, `protocol`, `long-context` | the provider's | Where and how it's reached |

hlock checks `[deciders]` when it reads `harness.toml`: an unknown use or key, or a model
that isn't a decision model, is an error rather than ignored. A use with only `any-of`
falls back to `[deciders.default]`'s `model` outside `resolve`.
