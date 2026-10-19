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
the [design spec](https://github.com/unionai-oss/hlock/blob/main/specs/v0.md), and
the v0.1 resolver doesn't search over them yet.

## The manifest hash

The lock records a SHA-256 hash of the manifest file exactly as written, comments and
whitespace included. [`hlock check`](../guides/ci.md) compares it to the live file, so
any edit to `harness.toml` needs a fresh `hlock resolve`.

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
