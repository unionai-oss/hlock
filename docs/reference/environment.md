# Environment variables

`hlock` reads these variables.

## Project and harness

| Variable | Default | Description |
|---|---|---|
| `HLOCK_MANIFEST` | the nearest `harness.toml` | Same as `-m/--manifest` |
| `HLOCK_LOCKFILE` | `harness.lock` next to the manifest | Same as `--lock` |
| `HLOCK_HARNESS` | unset | Target harness, between `--harness` and `[target] harness` in precedence. See [Harnesses](../guides/harnesses.md#target-a-harness). |
| `HLOCK_PROFILE_PATH` | unset | Extra profile directories (`:`-separated). See [Harness profiles](../guides/harness-profiles.md). |
| `HLOCK_SECRET_*` | unset | Values for secret placeholders in a pinned lock. See [Pinning your setup](../guides/pin.md#secrets-stay-out). |

The variables below are used by the worker (the `devbox` and `cluster`
[backends](../guides/backends.md)); the `local` backend ignores them.

## Worker

| Variable | Default | Description |
|---|---|---|
| `HL_PYTHON` | the interpreter running `hlock` | Interpreter the Rust core uses to launch the worker, and the default for [code harnesses](../guides/harnesses.md#how-hlock-runs-a-code-harness). Point it at one with `hlock` installed. |

## LLM calls

Set these to call a real model through any OpenAI-compatible endpoint. Without
`HL_LLM_BASE_URL`, the worker uses a deterministic offline reference LLM.

| Variable | Default | Description |
|---|---|---|
| `HL_LLM_BASE_URL` | unset | Base URL of an OpenAI-compatible API, e.g. `http://localhost:8000/v1` |
| `HL_LLM_MODEL` | unset | Model name to request |
| `HL_LLM_API_KEY` | unset | API key, if the endpoint needs one |

## Fine-tuning

Used by `hlock tune --depth ft` and `--depth full`.

| Variable | Default | Description |
|---|---|---|
| `HL_FT_BASE_MODEL` | `Qwen/Qwen2-0.5B` | Base model for LoRA fine-tuning |
| `HL_FT_STEPS` | `3` | Training steps |
| `HL_GPU_COST_PER_H` | `2.0` | GPU price per hour in USD, used for cost evidence |

## Output

| Variable | Description |
|---|---|
| `NO_COLOR` | Any value turns off colored CLI output |

`hlock backend status` shows which of these are set. API keys are reported as set or
unset, never printed.
