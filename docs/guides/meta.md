# The meta harness

Some of the work around a lock needs judgment rather than deterministic code: installing
what a lock needs on a machine you've never seen, filling in an eval pack from a repository,
updating skills and MCP servers to their latest releases. `hlock meta` hands that work to
an agent of its own, the **meta harness**, and checks the result itself.

By default the meta harness is hlock's **built-in agent**: a small tool-calling agent
that ships inside hlock, so there's nothing to install. It talks to an API provider with
your key, or to a model served on your machine. You can use an open-source harness
instead (Pi, Codex, Goose, OpenCode or Hermes) with a config of its own. Either way it's
pinned in `harness.meta.lock` and never touches your daily driver's config.

## Set it up

```console
$ hlock meta init
✓ wrote harness.meta.toml
harness  built-in agent  ships with hlock; nothing to install
model    anthropic:claude-sonnet-5-5  ANTHROPIC_API_KEY is set
api key  ANTHROPIC_API_KEY is set
✓ pinned builtin 0.2.0 → harness.meta.lock
next → hlock meta run install  ·  hlock meta jobs
```

`init` picks the model from what's on the machine: the first provider whose API key is
set (Anthropic `claude-sonnet-5-5`, OpenAI `gpt-5.5`, Google `gemini-3.5-flash`, or
OpenRouter with `--model`). With no key but a local runtime installed, it uses a model
for that runtime. Otherwise it picks Anthropic and reminds you to set the key.

```bash
hlock meta init --provider openai                 # an API provider, with its default model
hlock meta init --provider anthropic --model claude-opus-5-5
hlock meta init --base-url https://gateway.example.com/v1 --provider openai   # any OpenAI-compatible endpoint
hlock meta init --harness pi                      # an installed harness instead (pi, codex, goose, opencode, hermes)
hlock meta init --force                           # start over
```

A harness other than the built-in agent must be open source and able to run from its own
config root. Claude Code, Cursor Agent, Copilot CLI and Amp are refused, and so is Gemini
CLI, because its config root can't be moved. `init` configures it under
`.hlock/env/harness.meta/` and offers to install it if it's missing (`-y` installs
without asking).

### API keys

The key stays in your environment. `harness.meta.toml` records only the variable's name
(`key-env = "ANTHROPIC_API_KEY"`), and the agent reads it at run time. Nothing hlock
writes contains the key. `hlock meta run` checks that the variable is set before it starts.

### Local models

```console
$ hlock meta init --local
✓ wrote harness.meta.toml
harness  built-in agent  ships with hlock; nothing to install
model    ollama:qwen3-coder:30b  (local)  chosen with --local
✓ pinned builtin 0.2.0 → harness.meta.lock
Before the first job
  ollama pull qwen3-coder:30b
next → hlock meta run install  ·  hlock meta jobs
```

`--local` uses open weights served by a local runtime: the most preferred one installed
(Ollama, LM Studio, llama.cpp, MLX, vLLM, …; `--runtime` picks one). The model is a
catalog model already on disk if there is one, else the smallest of the best models that
fit the machine's memory (`--memory-gb` overrides the measurement, `--model` names one).
`init` offers to download it; `-y` downloads without asking. Small models can call tools,
but they do worse at multi-step jobs. hlock's check after each job tells you when one
didn't get there.

## The built-in agent

The built-in agent is part of hlock's Rust core. It speaks Anthropic's Messages API and
the OpenAI chat-completions API (which covers OpenAI, OpenRouter, Gemini and every local
runtime), and works through six tools:

| Tool | What it does |
|---|---|
| `list_dir`, `read_file`, `search` | Look around the project (or anywhere you point it) |
| `write_file`, `edit_file` | Create and change files, only inside the project |
| `run` | Run a shell command in the project |

What it may run is up to you:

- **Interactive runs** (the default) show you each command that isn't on the allowlist:
  `y` runs it, `n` refuses it, `a` allows that program for the rest of the run.
- **Headless runs** (`--headless`) run only allowlisted commands and refuse the rest. The
  agent is told why, and says in its summary what it couldn't do.

The allowlist matches on whole words at the start of a plain command, so `git log`
allows `git log -5` but not `git push`, and nothing with pipes, redirects, `;`, `&&` or
`$(…)` matches. The default allows `hlock` and read-only `git`. Change it in `[agent]`:

```toml title="harness.meta.toml"
[agent]
allow = ["hlock", "git log", "git show", "brew install", "npm install"]
max-steps = 60                # model turns per job
timeout-seconds = 300         # per command
```

`hlock meta pin` records the agent's version (hlock's), its model, and these settings in
the lock, so a changed allowlist shows up in review and `hlock meta check` catches drift.
A lock pinned by another hlock version needs a re-pin (or `--allow-version-mismatch`).

## `harness.meta.toml`

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
allow = ["hlock", "git log", "git show", "git diff", "git status"]

[run]
mode = "interactive"          # or "headless"

# Extra instructions for a built-in job, or a custom job:
# [jobs.install]
# instructions = "Prefer Homebrew. Never use sudo."
```

| Key | Description |
|---|---|
| `target.harness` | The meta harness: `builtin`, `pi`, `codex`, `goose`, `opencode`, `hermes`, or a [project harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts) |
| `model.provider`, `model.name`, `model.key-env` | An API provider, its model, and the variable holding its key |
| `model.runtime`, `model.name` | A local runtime and its model |
| `model.base-url` | Where the model's API is, overriding the provider's or runtime's default |
| `agent.allow`, `agent.max-steps`, `agent.timeout-seconds` | What the [built-in agent](#the-built-in-agent) may run, and its limits |
| `run.mode` | How `hlock meta run` runs jobs by default: `interactive` or `headless` |
| `jobs.<name>` | Extra `instructions` for a built-in job, or a [custom job](#custom-jobs) |

After you edit it, run `hlock meta pin` to pin it again. `hlock meta check` fails until
you do.

Commit `harness.meta.toml` and `harness.meta.lock`, and for an installed harness also
`.hlock/vendor/meta/`. An installed harness's config root (`.hlock/env/harness.meta/`) is
rebuilt from the lock, so leave it out of version control.

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

- **Interactive** (the default): you watch the agent work and approve what it asks to do.
  An installed harness opens a normal session with the prompt as its first message; exit
  it when it's done.
- **Headless** (`--headless`, or `[run] mode = "headless"`): the agent runs the prompt
  unattended and its output streams to the terminal. An installed harness gets the same
  arguments as [`hlock run`](../reference/cli.md#run) (`pi -p`, `codex exec`, …).

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

The [built-in agent](#the-built-in-agent) writes only inside the project and runs only
what the allowlist or you allow. It uses your environment as is, so the commands you
approve act on your real setup.

An installed harness runs with its own config root, set through its config variable
(`PI_CODING_AGENT_DIR` for Pi), and the commands it runs inherit that variable. So that
it can still work on *your* setup:

- any `hlock` command it runs undoes the isolation first, so `hlock sync` and
  `hlock pin` act on your real config;
- its prompt names the variable and gives a prefix (`env -u PI_CODING_AGENT_DIR`) for
  other commands that should see your real config.

hlock never passes flags that bypass a harness's permission prompts. A headless run can
do only what that harness's own config allows. To allow more, change its config in
`hlock meta launch`, then `hlock meta pin`, so the change shows up in a reviewed lock.

Each run is recorded in `.hlock/meta/runs/<job>-<time>/`: the prompt, the output of a
headless run, and `result.json` with the command, exit code and verdict. hlock adds a
`.gitignore` there, because agent output can quote anything the agent read.

## Keep it honest

```console
$ hlock meta check
✓ the meta harness matches harness.meta.lock
```

`check` fails when the lock isn't for `[target]`'s harness, `[model]`'s model or
`[agent]`'s settings, when the built-in agent's version changed (a new hlock), or when an
installed harness's config or version changed since it was pinned. For an installed
harness, `hlock meta sync` rebuilds its config from the lock (`--install` installs the
harness, `--pull` downloads local weights) and `hlock meta launch` opens a session in it;
the built-in agent needs neither. `hlock meta show` prints the lock:

```console
$ hlock meta show
╭─ 📌 harness.meta.lock · meta · pinned ───────╮
│                                              │
│  harness        builtin 0.2.0  hlock@0.2.0   │
│  llm · default  anthropic:claude-sonnet-5-5  │
│                                              │
╰──────────────────────────────────────────────╯
allowed hlock, git log, git show, git diff, git status  (without asking; at most 60
steps a job)
api key ANTHROPIC_API_KEY is set
```

The design, including what's planned (choosing the meta harness by evidence, opt-in
delegation from other commands), is in the
[meta-harness spec](https://github.com/unionai-oss/hlock/blob/main/specs/meta-harness.md).
