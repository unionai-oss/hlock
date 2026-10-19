# Environment variables

`hlock` reads these variables.

## Project and harness

| Variable | Default | Description |
|---|---|---|
| `HLOCK_MANIFEST` | the nearest `harness.toml` | Same as `-m/--manifest` |
| `HLOCK_LOCKFILE` | `harness.lock` next to the manifest | Same as `--lock` |
| `HLOCK_HARNESS` | unset | Target harness, between `--harness` and `[target] harness` in precedence. See [Harnesses](../guides/harnesses.md#target-a-harness). |
| `HLOCK_PROFILE_PATH` | unset | Extra profile directories (`:`-separated). See [Harness profiles](../guides/harness-profiles.md). |
| `HLOCK_SECRET_*` | unset | Values for secret placeholders in a pinned lock. They take precedence over the secrets file and the keychain. See [Secrets](../guides/secrets.md). |
| `HLOCK_SECRETS_FILE` | `~/.config/hlock/secrets.toml` | Where secret references live. See [Secrets](../guides/secrets.md#reference-a-password-manager). |
| `HLOCK_KEYCHAIN` | on | `0` stops hlock from reading or writing the OS keychain |
| `HLOCK_HOME` | `~/.hlock` | hlock's per-user directory: [pulled](../guides/pull.md) locks in `pulls/`, their isolated roots in `env/` |
| `HLOCK_GITHUB_URL` | `https://github.com` | Base URL `github:` sources fetch from (for GitHub Enterprise). See [Pulling someone else's lock](../guides/pull.md#sources). |
| `HLOCK_GITLAB_URL` | `https://gitlab.com` | Base URL `gitlab:` sources fetch from (for a self-hosted GitLab) |
| `HLOCK_META_MANIFEST` | `harness.meta.toml` next to the lock | The [meta harness](../guides/meta.md)'s manifest |
| `HLOCK_META_LOCK` | `harness.meta.lock` next to the lock | The meta harness's lock |
| `HLOCK_JUDGE_AUTO` | on | `0` stops hlock from picking a judge (Kev on a local Ollama or Ollaya, or Jev with `TYPESAFE_API_KEY`) when none is set |
| `HLOCK_BIN` | the `hlock` on `PATH` | The command the hooks `sync` writes for [decision models](../guides/deciders.md) run |
| `HLOCK_DECIDE_VERBOSE` | unset | `1` makes `hlock decide hook` say what it decided on stderr |
| `HLOCK_DECIDE_LOCK` | set for code harnesses | The lock whose deciders `ctx.decide` and `hlock decide hook --format plugin` ask |
| `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL` | unset | TypeSafe's key and endpoint, for `typesafe:` deciders |
| `OLLAYA_HOST`, `OLLAYA_API_KEY` | `127.0.0.1:11435` | Where Ollaya serves `ollaya:` deciders, and its key if it has one |
| `HLOCK_META_RESTORE` | set by `hlock meta run` | Inside a meta harness session: the config variables isolation changed, with their original values. `hlock` commands put them back. Don't set it yourself. |

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
