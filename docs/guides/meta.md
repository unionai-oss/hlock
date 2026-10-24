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
environment      built in  Declare in harness.toml the packages the lock's programs need
next → hlock meta run <job>
```

`hlock meta run JOB` renders the job's prompt from the project's state, runs the agent on
it, and checks the result. Here the built-in agent, on a small local model, runs a
[custom job](#custom-jobs) that asks for a `NOTES.md`:

```console
$ hlock meta run notes --headless
▶ built-in agent · notes · headless   python -m hlock.cli meta agent --meta-lock ~/demo/harness.meta.lock --headless <.hlock/meta/runs/notes-20261018T024933Z/prompt.md>
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

#### Desktop apps

For a [desktop app's](harnesses.md#desktop-apps) lock, the job is how a missing app gets
installed when Homebrew can't do it. The prompt says how to install the app on this
machine (the cask, else the download page), where hlock looks for it, and how to check:
`hlock harness show claude-desktop`. It asks the agent to install into `~/Applications`
rather than use `sudo`. There's no need to open the app first: hlock creates its config.
`sync` suggests the job when the app is what's missing:

```console
$ hlock sync
…
2 steps need you:
  1. Install Claude Desktop  a download, installed by hand; installs the current version;
the lock pins 3.1623.1
     Download Claude Desktop from https://claude.ai/download
  2. Write ~/Library/Application Support/Claude/claude_desktop_config.json /mcpServers
Claude Desktop reads only your own config; hlock sync --apps writes it
next → hlock sync --apps  ·  hlock meta run install --apps (let the meta harness do it)
```

An app reads only your own config, so writing it is your call: pass `--apps` to
`hlock meta run install` (and `hlock meta verify install`) to let the agent finish with
`hlock sync --apps --yes`. Without it, the job stops once the app is installed, and leaves
the config to you:

```console
$ hlock meta verify install
! needs you  installed; left: Claude Desktop's own config to write with hlock sync --apps
```

`hlock meta run install` works on the project's lock. For a lock you pulled, use
[`hlock sync --skill`](sync.md#let-your-agent-do-the-manual-steps), whose skill carries the
same app instructions.

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

With a [decision model](#decision-models), it also reviews each task, and warns when its
prompt may lean on context it doesn't give, its graders may not check what it asks, or its
solution may not do it. These are warnings, never errors.

### `update`

For a [pinned lock](pin.md), gives the agent every skill, MCP server and plugin in the
lock, and asks it to move each to its newest release through your harness's own tools.
hlock then re-captures your harness and prints what changed. `--pin` re-pins
`harness.lock` so the update is recorded; without it, review the diff and run
`hlock pin` yourself.

### `environment`

Declares in `harness.toml`'s [`[environment]`](../reference/manifest.md#environment) what the
lock needs installed. hlock gives the agent the list
[`hlock env suggest`](../reference/cli.md#env-suggest) makes: each program the lock runs,
where it runs it, and the packages that would install it. The agent decides for each
missing one whether the stack needs it, reading the scripts named. It adds the package to
`apt` and `brew` (or `pip`, `npm`), checking names with `apt-cache` or `brew info`, or lists the
program under `ignore`. It edits only `[environment]` and installs nothing.

hlock's check fails while a `high` program (an MCP server's command, a hook's) is neither
declared nor ignored. It warns about each `low` one left, and, when `harness.toml` is
committed, about any other table that changed since the last commit:

```console
$ hlock meta verify environment
✓ verified  4 programs declared, 0 ignored
  warning: maybe needed: gh (low, missing): skill release: scripts/pr.sh. Packages: apt gh, brew gh. Found at /opt/homebrew/bin/gh.
  warning: maybe needed: protoc (low, missing): skill release: scripts/pr.sh. Packages: apt protobuf-compiler, brew protobuf. Found at /opt/homebrew/bin/protoc.
```

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
| `list_dir`, `read_file`, `search` | Look around the project; anywhere else needs your approval |
| `write_file`, `edit_file` | Create and change files in the project; anywhere else needs your approval |
| `run` | Run a shell command in the project |

What it may run is up to you:

- **Interactive runs** ask you about each command that isn't on the allowlist: `y` runs
  it, `n` refuses it, and `a` allows that program for the rest of the run.
- **Headless runs** run only allowlisted commands and refuse the rest. The agent is told
  why, and says in its summary what it couldn't do.

The file tools work the same way. Reading or writing outside the project, through a
symlink that leads out of it, or writing hlock's own files (`harness.toml`,
`harness.meta.toml`, `harness.lock` and other `harness.*.lock` files) or anything under
`.git/` or `.hlock/`, is put to you in an interactive run and refused in a headless one.
`search` skips files over 2 MB, binary files, and symlinks.

The allowlist matches whole words at the start of a plain command: `git log` allows
`git log -5` but not `git push`, and a command with pipes, redirects, `;`, `&&` or `$(…)`
never matches. The program must be a bare name found on `PATH`: `./git log` or
`/usr/bin/git log` isn't allowlisted, since a file with that name could be anything. An
entry starting with `!` refuses a command that starts with its words and has all of its
flags: `!npm install --global` refuses `npm install --global x`.

The default allows `@hlock` and read-only `git`. A `git` command with an option that
writes a file, runs a program, or points git elsewhere (`--output`, `--ext-diff`,
`--textconv`, `--no-index`, `-c`, `--config-env`, `--exec-path`, `--git-dir`,
`--work-tree`) isn't allowlisted. `@hlock` stands for the hlock commands the jobs need
that don't run code from the project or install software:

- `hlock check`, `show`, `tree`, `harness list`, `harness show`, `runtime list`, and
  `backend status`
- `hlock evals init` (but not with `--skill-dir`, which writes outside the project, or
  `--force`), `evals templates`, and `evals check`, but not `evals check --run` or
  `evals grade`, which run the pack's test scripts
- `hlock meta check`, `meta show`, `meta jobs`, and `meta verify` for the built-in jobs
  (`install`, `bootstrap-evals`, `update`, `environment`). A custom job's `verify` runs a shell command
  from `harness.meta.toml`, so the agent asks you before it runs `hlock meta verify` for
  one; hlock runs the check itself after the agent finishes either way.
- `hlock sync`, but not with `--install`, `--pull`, `--build`, `--in-place`, `--prefix`,
  or `--skill-dir`
- `hlock env suggest`, including `--write`, which edits `harness.toml`'s `[environment]`

An `@hlock` command with a path argument that's absolute, starts with `~`, or climbs out
with `..` isn't allowlisted either.

The agent can edit files in the project, so a command that runs what's written there,
such as `hlock resolve` (code harnesses) or `hlock evals check --run` (test graders), would
let it run anything. Allow plain `"hlock"` to permit every hlock command. Change the list in
`[agent]`:

```toml title="harness.meta.toml"
[agent]
allow = ["@hlock", "git log", "git show", "brew install", "npm install"]
max-steps = 60                # model turns per job
timeout-seconds = 300         # per command
```

The agent runs with your environment, so commands you approve act on your real setup.
`hlock meta pin` records its version (hlock's), its model and these settings in the lock,
so a wider allowlist shows up in review.

## Decision models

Give the meta harness a [decision model](deciders.md) for its checks in
`harness.meta.toml`:

```toml title="harness.meta.toml"
[deciders.default]
model = "ollama:kev:4b"
```

Without one there, the project's `[deciders.default]` from `harness.toml` is used, if set.
It's asked, and only warns:

| Where | It asks | You see |
|---|---|---|
| `bootstrap-evals` verify | Does each task's prompt stand alone, do its graders check what it asks, does its solution do it? | a warning per doubtful task |
| `install` verify | For each step still missing: does it need an agent, the network, permissions, or you? | `— needs the network (P=0.70)` after the step |
| `update` verify | For each change since the pin: could it break something that works? | `warning: may break something: …` |
| the built-in agent | When it says it's done: is the job done? | at most two more turns, with the reason |
| the built-in agent | When you're asked about a command: could it delete data or reach outside the project? | a risk note with the question |

A decision model never decides what the built-in agent may run: only `[agent] allow` and
you do.

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
allowed @hlock, git log, git show, git diff, git status  (without asking; at most 60
steps a job)
api key ANTHROPIC_API_KEY is set
```

The design, including what's planned (choosing the meta harness by evidence, opt-in
delegation from other commands), is in the
[meta-harness spec](https://github.com/unionai-oss/hlock/blob/main/specs/meta-harness.md).
