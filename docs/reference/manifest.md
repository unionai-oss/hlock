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
`[objective]`, `[evals]`, `[evals.splits]`, `[candidates]`, `[backend]`, `[target]`, or at
the top level, and name the closest known key: a mistyped key is otherwise ignored.

## `[project]`

| Key | Description |
|---|---|
| `name` | Project name, recorded as the lock's stack name |

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

`[context]` and `[enforce]` are parsed and included in the manifest hash. They're
placeholders for the peripherals described in the
[design spec](https://github.com/unionai-oss/hlock/blob/main/specs/v0.md). The resolver
doesn't search over dependencies yet: `hlock resolve` treats `[skills]` and `[tools]` names
as fixed parts of every stack.

## The manifest hash

A resolved lock records a SHA-256 hash of the manifest file exactly as written, comments
and whitespace included: the text the run read, even if the file changes during it.
[`hlock check`](../guides/ci.md) compares it to the live file, so any edit to
`harness.toml` needs a fresh `hlock resolve`.

A pinned lock with declared dependencies records a hash of the dependency sections only
(`[skills]`, `[mcp]`, `[instructions]`, `[plugins]`, `[packages]` and `[tools]`), so editing
`[candidates]` or `[evals]` in the same file doesn't invalidate the pin.

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
