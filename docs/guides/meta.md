# The meta harness

Some of the work around a lock needs judgment rather than deterministic code: installing
what a lock needs on a machine you've never seen, filling in an eval pack from a repository,
updating skills and MCP servers to their latest releases. `hlock meta` hands that work to
an agent of its own, the **meta harness**, and checks the result itself.

The meta harness is an open-source agent harness (Pi by default) with a config of its
own, pinned in `harness.meta.lock`. It doesn't use your daily driver or its config, so
it behaves the same on every machine.

## Set it up

```console
$ hlock meta init
✓ wrote harness.meta.toml
harness  Pi  the most preferred open-source harness installed
model    anthropic:claude-sonnet-5-5  ANTHROPIC_API_KEY is set
api key  ANTHROPIC_API_KEY is set
✓ pinned pi 1.0.0 → harness.meta.lock
next → hlock meta run install  ·  hlock meta jobs
```

`init` looks at what's on the machine and picks:

| | Default |
|---|---|
| **Harness** | The first of Pi, Codex, Goose, OpenCode and Hermes that's installed, else Pi |
| **Model** | The first provider whose API key is set: Anthropic `claude-sonnet-5-5`, OpenAI `gpt-5.5`, Google `gemini-3.5-flash`. With no key but a local runtime installed: a model for that runtime. Otherwise Anthropic, with a reminder to set the key. |

Then it writes `harness.meta.toml`, configures the harness in its own config root under
`.hlock/env/harness.meta/`, offers to install the harness if it's missing, and pins the
result. Pass `-y` to install without asking.

```bash
hlock meta init --provider openai                 # an API provider, with its default model
hlock meta init --provider anthropic --model claude-opus-5-5
hlock meta init --harness goose                   # Goose or Codex if you'd rather run Rust
hlock meta init --force                           # start over
```

Only open-source harnesses can be the meta harness. Claude Code, Cursor Agent, Copilot
CLI and Amp are refused, and so is Gemini CLI, because its config root can't be moved.

### API keys

The key stays in your environment. `harness.meta.toml` records only the variable's name
(`key-env = "ANTHROPIC_API_KEY"`), and the harness reads it the way it always does.
Nothing hlock writes contains the key. `hlock meta run` checks that the variable is set
before it starts.

### Local models

```console
$ hlock meta init --local
✓ wrote harness.meta.toml
harness  Pi  the most preferred open-source harness installed
model    ollama:qwen3-coder:30b  (local)  chosen with --local
✓ pinned pi 1.0.0 → harness.meta.lock
Before the first job
  ollama pull qwen3-coder:30b
next → hlock meta run install  ·  hlock meta jobs
```

`--local` uses open weights served by a local runtime: the most preferred one installed
(Ollama, LM Studio, llama.cpp, MLX, vLLM, …; `--runtime` picks one). The model is a
catalog model already on disk if there is one, else the smallest of the best models that
fit the machine's memory (`--memory-gb` overrides the measurement, `--model` names one).
`init` offers to download it; `-y` downloads without asking.

## `harness.meta.toml`

```toml title="harness.meta.toml"
# hlock meta manifest: the agent hlock uses for its own work (hlock meta --help).
[target]
harness = "pi"                # an open-source harness: pi, codex, goose, opencode, hermes

[model]
provider = "anthropic"
name = "claude-sonnet-5-5"
key-env = "ANTHROPIC_API_KEY" # the variable holding the API key; the key is never written

[run]
mode = "interactive"          # or "headless"

# Extra instructions for a built-in job, or a custom job:
# [jobs.install]
# instructions = "Prefer Homebrew. Never use sudo."
```

| Key | Description |
|---|---|
| `target.harness` | The meta harness: `pi`, `codex`, `goose`, `opencode`, `hermes`, or a [project harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts) |
| `model.provider`, `model.name`, `model.key-env` | An API provider, its model, and the variable holding its key |
| `model.runtime`, `model.name`, `model.base-url` | A local runtime, its model, and where it listens (default: the runtime's local port) |
| `run.mode` | How `hlock meta run` runs jobs by default: `interactive` or `headless` |
| `jobs.<name>` | Extra `instructions` for a built-in job, or a [custom job](#custom-jobs) |

After you edit it, run `hlock meta pin`. That writes `[model]` into the meta harness's
config and pins it again. `hlock meta check` fails until you do.

Commit `harness.meta.toml`, `harness.meta.lock` and `.hlock/vendor/meta/`. The meta
harness's config root (`.hlock/env/harness.meta/`) is rebuilt from the lock, so leave it
out of version control.

## Run a job

```console
$ hlock meta jobs
job                        description
install          built in  Install what the project's lock still needs on this machine
bootstrap-evals  built in  Fill in an eval pack with real tasks from this project
update           built in  Update the lock's skills, MCP servers and plugins
next → hlock meta run <job>
```

`hlock meta run JOB` renders the job's prompt from the project's state, runs the meta
harness on it, and then checks the result itself:

- **Interactive** (the default): a normal session with the prompt as its first message.
  You watch, approve what it asks to do, and exit when it's done.
- **Headless** (`--headless`, or `[run] mode = "headless"`): the harness runs the prompt
  non-interactively, with the same arguments as [`hlock run`](../reference/cli.md#run)
  (`pi -p`, `codex exec`, `goose run -t`, …), and its output streams to the terminal.

Words after the job name are added to the prompt: `hlock meta run install "use brew,
never sudo"`. `--dry-run` prints the command and the prompt without running anything.

| Exit code | Meaning |
|---|---|
| `0` | Verified |
| `1` | Verification failed, or the harness couldn't run |
| `3` | The agent did its part; what's left is yours (for example, secrets to set) |

### `install`

Gives the agent the project lock's [sync plan](sync.md): every step that isn't done yet,
why it's needed, a suggested command, and how to check it. The agent installs what's
missing and finishes with `hlock sync --yes`. hlock then plans again: the job is verified
when nothing is left but secrets, which are yours to set (exit `3`, with the variables
listed). This replaces the hand-off in [`hlock sync --skill`](sync.md#let-your-agent-do-the-manual-steps).

### `bootstrap-evals`

Gets the agent to fill in an [eval pack](eval-packs.md) with real tasks from this
repository, in `--out` (default: the manifest's first eval pack, else `./evals`):

- with no pack there yet, hlock first writes a template, as
  [`hlock evals init`](eval-packs.md#fill-in-a-template-with-your-agent) does
  (`-t coding` by default, with `--count` task slots, default 10), and the agent replaces
  each `TODO(hlock)` slot following the template's guide;
- with a complete pack, the agent adds tasks until there are `--count`, without changing
  the existing ones.

hlock then checks the pack, as `hlock evals check` does:

```console
$ hlock meta verify bootstrap-evals
✗ failed  evals isn't ready: 1 error
  count_words: prompt file missing.md not found
  warning: count_words: no expected artifacts or graders, so it always passes
  warning: 3 tasks leaves 1 for the holdout split; add tasks until there are at least 10
```

Errors fail the job; warnings are reported but don't.

### `update`

For a [pinned lock](pin.md), gives the agent every skill, MCP server and plugin in the
lock, and asks it to move each to its newest release through the harness's own tools.
hlock then re-captures your harness and prints what changed. `--pin` re-pins
`harness.lock` so the update is recorded; without it, run `hlock pin` once you've
reviewed the diff.

### Custom jobs

```toml title="harness.meta.toml"
[jobs.changelog]
description = "Draft a CHANGELOG entry for the latest lock change"
prompt = """
Read `git diff HEAD~1 -- {lock}` and add an entry to CHANGELOG.md describing what
changed in the agent setup and why it matters.
"""
verify = "grep -q '## Unreleased' CHANGELOG.md"
```

`{lock}`, `{manifest}`, `{project}` and `{meta_lock}` in the prompt are replaced with
paths. `verify` runs in the project after the agent exits; exit 0 means verified. A job
without `verify` is reported as unverified.

### Checking without running

`hlock meta verify JOB` runs only the check. The agent runs it to know when it's done,
and it's handy in CI:

```console
$ hlock meta verify bootstrap-evals
✓ verified  2 tasks in evals
  warning: 2 tasks leaves 1 for the holdout split; add tasks until there are at least 10
```

## What the agent can touch

The meta harness runs with its own config root, set through its config variable
(`PI_CODING_AGENT_DIR` for Pi), and the commands it runs inherit that variable. So that
it can still work on *your* setup:

- any `hlock` command it runs undoes the isolation first, so `hlock sync` and
  `hlock pin` act on your real config;
- its prompt names the variable and gives a prefix (`env -u PI_CODING_AGENT_DIR`) for
  other commands that should see your real config.

hlock never passes flags that bypass a harness's permission prompts. A headless run can
do only what the meta harness's own config allows. To allow more, change its config in
`hlock meta launch`, then `hlock meta pin`, so the change shows up in a reviewed lock.

Each run is recorded in `.hlock/meta/runs/<job>-<time>/`: the prompt, the output of a
headless run, and `result.json` with the command, exit code and verdict. hlock adds a
`.gitignore` there, because agent output can quote anything the agent read.

## Keep it honest

```console
$ hlock meta check
✓ the meta harness matches harness.meta.lock
```

`check` fails when the lock isn't for `[target]`'s harness or `[model]`'s model, or when
the meta harness's config or version changed since it was pinned. `hlock meta sync`
rebuilds its config from the lock (`--install` installs the harness, `--pull` downloads
local weights), and `hlock meta show` prints the lock:

```console
$ hlock meta show
╭─ 📌 harness.meta.lock · meta · pinned ───────────────────────────────╮
│                                                                      │
│  harness        pi 1.0.0  npm:@earendil-works/pi-coding-agent@1.0.0  │
│  llm · default  anthropic:claude-sonnet-5-5                          │
│  config         {root}/settings.json                                 │
│                                                                      │
╰──────────────────────────────────────────────────────────────────────╯
api key ANTHROPIC_API_KEY is set
```

The design, including what's planned (choosing the meta harness by evidence, opt-in
delegation from other commands), is in the
[meta-harness spec](https://github.com/unionai-oss/hlock/blob/main/specs/meta-harness.md).
