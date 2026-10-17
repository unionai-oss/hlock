# `harness.toml`

The manifest declares what the resolver may try and what "best" means. `hlock init` writes
a starter one. Every section is optional, but a useful manifest has at least
`[tasks]`, `[candidates]`, and `[objective]`.

```toml title="harness.toml"
[project]
name = "support-agent"

[target]
harness = "claude-code"

[tasks]
packs = ["./tasks"]
seeds = 3

[tasks.splits]
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
max_trials = 200
max_spend_usd = 25.0
random_seed = 0

[backend]
target = "local"
```

## `[project]`

| Key | Description |
|---|---|
| `name` | Project name, recorded as the lock's stack name |

## `[tasks]`

| Key | Default | Description |
|---|---|---|
| `packs` | `[]` | Paths to [task pack](../guides/task-packs.md) directories |
| `seeds` | `3` | Runs per task per candidate |
| `splits.search` | `0.7` | Fraction of tasks in the search split. The rest is holdout. |

## `[candidates]`

The search space. The resolver tries every combination.

| Key | Description |
|---|---|
| `llm.<role>.any_of` | Candidate models for a role, as `"provider:model"`. v0.1 tunes the `planner` role. |
| `decision` | Candidate decision models, e.g. `"none"`, `"rules:default"` |
| `harness` | Candidate harnesses: names, or tables with `name`, `pattern`, and `config` |
| `policy.<key>` | Policy values, e.g. `retries = [1]`, `temperature = [0.2]`. `resolve` uses the first value. [`tune`](../guides/tune.md) searches retries and temperature itself. |

v0.1 ships the `reference` provider (models `qwen-1.5b`, `qwen-7b`, `qwen-32b`) and the
`reference` harness. Real providers run through the worker on the
[`devbox` and `cluster` backends](../guides/backends.md).

## `[objective]`

| Key | Default | Description |
|---|---|---|
| `primary` | `"success-rate"` | The metric being optimized |
| `select` | `"min-cost"` | How to choose among stacks that pass: `"min-cost"` or `"max-quality"` |
| `constraints` | `{}` | Hard limits, as a table of metric to comparison |

Constraint metrics are `success-rate` and `cost-usd-per-task`. Comparisons are `>=`,
`<=`, `>`, and `<` followed by a number. When no stack satisfies every constraint, the
resolver selects from the whole Pareto front instead of failing; see
[Selection](../concepts/index.md#selection).

## `[budget]`

| Key | Default | Description |
|---|---|---|
| `strategy` | `"grid"` | Search strategy. v0.1 supports `grid`. |
| `max_trials` | | Upper bound on trials |
| `max_spend_usd` | | Upper bound on spend |
| `random_seed` | `0` | Seed for the search/holdout split |

## `[target]`

| Key | Description |
|---|---|
| `harness` | The harness this lock is for: an id or alias from `hlock harness list`, or a `[harnesses.<id>]` table. `resolve` narrows `[candidates] harness` to it and `check` enforces it. See [Harnesses](../guides/harnesses.md#target-a-harness). |

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
| `target` | `"local"` | `local`, `devbox`, or `cluster`. See [Backends](../guides/backends.md). |

## Reserved sections

`[tools]`, `[skills]`, `[instructions]`, `[context]`, and `[enforce]` are parsed and
included in the manifest hash. They're placeholders for the peripherals described in
the [design spec](https://github.com/unionai-oss/hlock/blob/main/SPEC.md), and
the v0.1 resolver doesn't search over them yet.

## The manifest hash

The lock records a SHA-256 hash of the manifest file exactly as written, comments and
whitespace included. [`hlock check`](../guides/ci.md) compares it to the live file, so
any edit to `harness.toml` needs a fresh `hlock resolve`.
