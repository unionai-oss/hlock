# `harness.meta.toml`

`harness.meta.toml` configures the [meta harness](../guides/meta.md), the agent hlock uses
for its own work. [`hlock meta init`](cli.md#meta-init) writes it, and
[`hlock meta pin`](cli.md#meta) pins it into `harness.meta.lock`. Both files sit next to the
project's `harness.lock`; `HLOCK_META_MANIFEST` and `HLOCK_META_LOCK` move them.

It's a separate file from `harness.toml` so that changing the meta harness never marks the
project's lock as drifted: `harness.lock`'s manifest hash covers all of `harness.toml`.

```toml title="harness.meta.toml"
# hlock meta manifest: the agent hlock uses for its own work (hlock meta --help).
[target]
harness = "builtin"           # builtin (ships with hlock), or one you install: pi, codex, goose, opencode, hermes

[model]
provider = "anthropic"
name = "claude-sonnet-5-5"
key-env = "ANTHROPIC_API_KEY" # the variable holding the API key; the key is never written

[agent]
# commands that run without asking; others need your OK, and unattended runs refuse them
allow = ["@hlock", "git log", "git show", "git diff", "git status"]

[run]
mode = "interactive"          # or "headless"

# Extra instructions for a built-in job, or a custom job:
# [jobs.install]
# instructions = "Prefer Homebrew. Never use sudo."
```

After editing it, run `hlock meta pin`. [`hlock meta check`](cli.md#meta) fails until the
lock matches.

## `[target]`

| Key | Default | Description |
|---|---|---|
| `harness` | `builtin` | `builtin`, hlock's [built-in agent](../guides/meta.md#the-built-in-agent); or an installed open-source harness: `pi`, `codex`, `goose`, `opencode`, `hermes`; or a [project harness](../guides/harnesses.md#your-own-agent-frameworks-and-custom-scripts) |

## `[model]`

Either an API provider:

| Key | Description |
|---|---|
| `provider` | `anthropic`, `openai`, `google`, or `openrouter` (an installed harness may support fewer) |
| `name` | The model, e.g. `claude-sonnet-5-5` |
| `key-env` | The environment variable holding the API key. The key itself is never written anywhere. |
| `base-url` | Where the API is, for a gateway or another OpenAI-compatible endpoint. Default: the provider's. |

or a local runtime serving open weights:

| Key | Description |
|---|---|
| `runtime` | `ollama`, `lmstudio`, `llama-cpp`, `mlx-lm`, `vllm`, … (see `hlock runtime list`) |
| `name` | The model, e.g. `qwen3-coder:30b` |
| `base-url` | Where the runtime listens. Default: its local port. |

## `[agent]`

What the built-in agent may do. Other harnesses use their own permission settings.

| Key | Default | Description |
|---|---|---|
| `allow` | `["@hlock", "git log", "git show", "git diff", "git status"]` | Commands that run without asking. Each entry matches a plain command (no pipes, redirects, `;`, `&&`, `$(…)`) whose first words are the entry's words. The program must be a bare name found on `PATH`; one named by path (`./git`, `/usr/bin/git`) never matches. An entry starting with `!` refuses commands that start with its words and have all of its flags. `@hlock` is the hlock commands that don't run project code or install software; see [the built-in agent](../guides/meta.md#the-built-in-agent). |
| `max-steps` | `60` | Model turns per job |
| `timeout-seconds` | `300` | Time limit for each command |

## `[run]`

| Key | Default | Description |
|---|---|---|
| `mode` | `interactive` | How [`hlock meta run`](cli.md#meta-run) runs jobs: `interactive` or `headless`. `--headless` and `--interactive` override it. |

## `[jobs.<name>]`

For a built-in job (`install`, `bootstrap-evals`, `update`):

| Key | Description |
|---|---|
| `instructions` | Text added to the job's prompt |

For a custom job:

| Key | Description |
|---|---|
| `description` | One line for `hlock meta jobs` |
| `prompt` | The prompt. `{lock}`, `{manifest}`, `{project}` and `{meta_lock}` are replaced with paths. |
| `verify` | A shell command run in the project after the agent exits. Exit 0 means verified. Without it, the run is reported as unverified. `{lock}`, `{manifest}`, `{project}` and `{meta_lock}` are replaced with shell-quoted paths, so don't quote them yourself. It's stopped after 10 minutes. |
| `instructions` | Text added to the prompt |

## `[deciders.default]`

The [decision model](../guides/deciders.md) the meta harness's checks use, such as
`bootstrap-evals` reviewing tasks: `model`, and optionally `base-url`, `key-env` and
`protocol`. Other uses aren't taken here.

## `harness.meta.lock`

`hlock meta pin` writes a pinned lock (`kind = "pin"`):

- **built-in agent:** `[harness]` is `builtin` at hlock's version; `[[llm]]` has the
  provider, model, endpoint, and the key's variable (in `params`); `[policy]` has the
  allowlist (`tool-allowlist`), `max-steps` and `tool-timeout-seconds`.
- **installed harness:** a capture of its own config root, like
  [`hlock pin`](../guides/pin.md), with vendored files in `.hlock/vendor/meta/<harness>/`.

Commit `harness.meta.toml`, `harness.meta.lock`, and `.hlock/vendor/meta/` if there is one.
