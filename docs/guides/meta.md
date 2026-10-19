# Delegating to the meta harness

Some of the work around a lock needs judgment rather than deterministic code: installing
what a lock needs on a machine you've never seen, filling in an eval pack from a
repository, updating skills and MCP servers to their latest releases. `hlock meta` hands
that work to an agent, the **meta harness**, and then checks the result itself.

By default the meta harness is hlock's **built-in agent**. It ships inside hlock, so
there's nothing to install: give it an API key, or a model running on your machine. If
you'd rather use a full harness, Pi, Codex, Goose, OpenCode and Hermes work too. Either
way, the agent is pinned in `harness.meta.lock` and never touches your own agent's config.

| hlock does | the agent does |
|---|---|
| decides what's needed (a sync plan, a lock, a pack format) | works out how to do it on this machine |
| writes the prompt, with the context and the rules | runs commands and edits files, within what you allow |
| **checks the result**, and exits non-zero if it isn't done | |

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

`init` picks the model from what's on the machine:

1. the first provider whose API key is set: Anthropic (`claude-sonnet-5-5`), OpenAI
   (`gpt-5.5`), Google (`gemini-3.5-flash`), or OpenRouter (pass `--model`);
2. with no key set, a local runtime if one is installed (see [Local models](#local-models));
3. otherwise Anthropic, with a reminder to set `ANTHROPIC_API_KEY`.

The key stays in your environment. `harness.meta.toml` records only the variable's name,
and nothing hlock writes contains the key. `hlock meta run` checks the variable is set
before it starts.

```bash
hlock meta init --provider openai                 # an API provider, with its default model
hlock meta init --provider anthropic --model claude-opus-5-5
hlock meta init --provider openai --base-url https://gateway.example.com/v1   # any OpenAI-compatible endpoint
hlock meta init --force                           # start over
```

Commit `harness.meta.toml` and `harness.meta.lock`. Every key is described in the
[`harness.meta.toml` reference](../reference/meta-manifest.md).

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

`--local` uses open weights served by the most preferred runtime you have installed
(Ollama, LM Studio, llama.cpp, MLX, vLLM, …; `--runtime` picks one). The model is one
already on disk if it's capable enough, else the smallest of the best models that fit
this machine's memory. `--model` names one, and `--memory-gb` overrides the memory
measurement. `init` offers to download it, and `-y` downloads without asking.

Small models can call tools, but they're unreliable at multi-step jobs. When one doesn't
get there, hlock's check after the job says so.

## Run a job

```console
$ hlock meta jobs
job                        description
install          built in  Install what the project's lock still needs on this machine
bootstrap-evals  built in  Fill in an eval pack with real tasks from this project
update           built in  Update the lock's skills, MCP servers and plugins
next → hlock meta run <job>
```

`hlock meta run JOB` renders the job's prompt from the project's state, runs the agent on
it, and checks the result. Here the built-in agent, on a small local model, runs a
[custom job](#custom-jobs) that asks for a `NOTES.md`:

```console
$ hlock meta run notes --headless
▶ built-in agent · notes · headless   python -m harness_lock.cli meta agent --meta-lock ~/demo/harness.meta.lock --headless <.hlock/meta/runs/notes-20261018T024933Z/prompt.md>
→ search "(project|summary|main purpose)" in README.md
  0 matches
→ read file README.md
  2 lines
→ write file NOTES.md
  wrote NOTES.md
→ $ hlock meta verify notes
  exit code 0
No issues detected. The project notes are in place. Let's finish. There are no more
tasks.
built-in agent finished (2 steps, 1,552 tokens)
✓ verified  `test -s NOTES.md` passed
run record: .hlock/meta/runs/notes-20261018T024933Z
```

- **Interactive** (the default): you watch the agent work and approve what it asks to do.
- **Headless** (`--headless`, or `[run] mode = "headless"`): it works unattended, and does
  only what's [allowed](#the-built-in-agent).

Words after the job name are added to the prompt, as in `hlock meta run install "use brew,
never sudo"`. `--dry-run` prints the command and the prompt without running anything.

| Exit code | Meaning |
|---|---|
| `0` | Verified |
| `1` | The check failed, or the agent couldn't run |
| `3` | The agent did its part; what's left is yours (for example, secrets to set) |

Each run is recorded in `.hlock/meta/runs/<job>-<time>/`: the prompt, the output of a
headless run, and `result.json` with the command, exit code and verdict. hlock adds a
`.gitignore` there, because agent output can quote anything the agent read.

### `install`

Gives the agent the project lock's [sync plan](sync.md): every step that isn't done yet,
why it's needed, a suggested command, and how to check it. The agent installs what's
missing and finishes with `hlock sync --yes`, which restores the config into the isolated
root in `.hlock/env/`, not your own config. hlock then plans again. The job is verified
when nothing is left but secrets, which are yours to set: it exits `3` and lists the
variables. This automates the hand-off in
[`hlock sync --skill`](sync.md#let-your-agent-do-the-manual-steps).

Installing usually means running package managers, so run it interactively, or add the
commands it needs to [`[agent] allow`](#the-built-in-agent) for headless runs.

### `bootstrap-evals`

Gets the agent to fill in an [eval pack](eval-packs.md) with real tasks from this
repository, in `--out` (default: the manifest's first eval pack, else `./evals`):

- with no pack yet, hlock first writes a template, as
  [`hlock evals init`](eval-packs.md#fill-in-a-template-with-your-agent) does (`-t coding`
  by default, with `--count` slots, default 10), and the agent fills each `TODO(hlock)`
  slot, following the template's guide;
- with a complete pack, the agent adds tasks until there are `--count`, without changing
  the existing ones.

hlock then checks the pack, as `hlock evals check` does. Errors fail the job, and
warnings are reported:

```console
$ hlock meta verify bootstrap-evals
✗ failed  evals isn't ready: 1 error
  count_words: prompt file missing.md not found
  warning: count_words: no expected artifacts or graders, so it always passes
  warning: 3 tasks leaves 1 for the holdout split; add tasks until there are at least 10
```

### `update`

For a [pinned lock](pin.md), gives the agent every skill, MCP server and plugin in the
lock, and asks it to move each to its newest release through your harness's own tools.
hlock then re-captures your harness and prints what changed. `--pin` re-pins
`harness.lock` so the update is recorded; without it, review the diff and run
`hlock pin` yourself.

### Custom jobs

```toml title="harness.meta.toml"
[jobs.notes]
description = "Write notes"
prompt = "Read README.md, then write a file NOTES.md with a one-line summary of the project."
verify = "test -s NOTES.md"
```

`{lock}`, `{manifest}`, `{project}` and `{meta_lock}` in a prompt are replaced with paths.
`verify` runs in the project after the agent exits, and exit 0 means verified. A job
without `verify` is reported as unverified. A built-in job takes `instructions` instead,
which are added to its prompt.

### Checking without running

`hlock meta verify JOB` runs only the check. The agent runs it to know when it's done,
and CI can run it too:

```console
$ hlock meta verify bootstrap-evals
✓ verified  2 tasks in evals
  warning: 2 tasks leaves 1 for the holdout split; add tasks until there are at least 10
```

## The built-in agent

The built-in agent is part of hlock's Rust core. It speaks Anthropic's API and the OpenAI
chat-completions API, which covers OpenAI, OpenRouter, Gemini, and every local runtime.
It works through six tools:

| Tool | What it does |
|---|---|
| `list_dir`, `read_file`, `search` | Look around the project, or anywhere it's pointed |
| `write_file`, `edit_file` | Create and change files, only inside the project |
| `run` | Run a shell command in the project |

What it may run is up to you:

- **Interactive runs** ask you about each command that isn't on the allowlist: `y` runs
  it, `n` refuses it, and `a` allows that program for the rest of the run.
- **Headless runs** run only allowlisted commands and refuse the rest. The agent is told
  why, and says in its summary what it couldn't do.

The allowlist matches whole words at the start of a plain command: `git log` allows
`git log -5` but not `git push`, and a command with pipes, redirects, `;`, `&&` or `$(…)`
never matches. The default allows `hlock` and read-only `git`. Change it in `[agent]`:

```toml title="harness.meta.toml"
[agent]
allow = ["hlock", "git log", "git show", "brew install", "npm install"]
max-steps = 60                # model turns per job
timeout-seconds = 300         # per command
```

The agent runs with your environment, so commands you approve act on your real setup.
`hlock meta pin` records its version (hlock's), its model and these settings in the lock,
so a wider allowlist shows up in review.

## Use an installed harness instead

```bash
hlock meta init --harness pi        # or codex, goose, opencode, hermes
```

The meta harness can be any open-source harness hlock can run from a config root of its
own. Claude Code, Cursor Agent, Copilot CLI and Amp are refused because they aren't open
source, and so is Gemini CLI, because its config root can't be moved. `init` offers to
install the harness if it's missing (`-y` installs without asking), then configures its
model under `.hlock/env/harness.meta/` and pins that config, with vendored files in
`.hlock/vendor/meta/`. Commit the vendored files; the config root is rebuilt from the lock.

An installed harness uses its own permission settings, not `[agent]`. hlock never passes
flags that bypass them, so a headless run can do only what its config allows. To allow
more, change its config in a session (`hlock meta launch`), then `hlock meta pin`, so the
change goes through review.

It runs with its config variable (`PI_CODING_AGENT_DIR` for Pi) pointing at that root,
and the commands it runs inherit the variable. So that it can still work on your setup,
any `hlock` command it runs puts your real config back first, and its prompt gives a
prefix (`env -u PI_CODING_AGENT_DIR`) for other commands that should see your config.

| | Built-in agent | Installed harness |
|---|---|---|
| Install | nothing | the harness (npm, brew, …) |
| What it may do | `[agent]` allowlist | the harness's own permission settings |
| `hlock meta sync` | not needed | rebuilds its config; `--install`, `--pull` |
| `hlock meta launch` | not available | opens a session, e.g. to sign in |

## Keep it honest

```console
$ hlock meta check
✓ the meta harness matches harness.meta.lock
```

`check` fails when the lock isn't for the harness, model or `[agent]` settings in
`harness.meta.toml`, when hlock's version changed since the built-in agent was pinned, or
when an installed harness's config or version changed. Re-pin with `hlock meta pin`.
`hlock meta show` prints what's pinned:

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
