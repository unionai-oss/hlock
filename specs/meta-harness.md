# hlock meta: a locked agent for hlock's own work (draft)

Status: draft, implemented · Owner: Niels Bantilan · Last updated: 2026-10-17 · Builds on [v0](v0.md)

> hlock pins your agent. Some of hlock's own work needs an agent too. `hlock meta`
> makes that agent explicit: an agent built into hlock (or an open-source harness you
> choose), pinned by hlock, whose work hlock checks.

---

## 1. Summary

Some of the work around a lock can't be done by deterministic code:

- **installing dependencies**: a sync plan step that says "install `pi` 1.0.0" has a
  dozen possible package managers, platform quirks, and failure modes;
- **filling in an eval pack**: turning a repository and its history into tasks that
  look like real work;
- **updating skills, MCP servers and plugins**: finding newer upstream versions, reading
  changelogs, and applying the update through each harness's own mechanism.

Before `hlock meta`, hlock handed that work to an *assumed* agent.
[`hlock sync --skill`](../docs/guides/sync.md) writes an Agent Skill and says "open your
agent here and ask it to rehydrate the harness". Which agent, which version, which model,
with which tools and permissions, was left to whatever the user runs day to day: their
*daily driver*. That was the one unpinned, unchecked component in an otherwise locked
workflow.

`hlock meta` closes the gap. It is a namespace that applies hlock's own verbs to a second
harness, the **meta harness**: the agent hlock calls on for judgment work.

```bash
hlock meta init                        # pick a model for the built-in agent, pin it
hlock meta run install                 # have it install what harness.lock still needs
hlock meta run bootstrap-evals         # have it fill in an eval pack from this repo
hlock meta run update                  # have it update locked skills, MCP servers, plugins
hlock meta check                       # has the meta harness drifted from its lock?
```

Each job is a contract between the two sides:

| hlock (deterministic) | the meta harness (judgment) |
|---|---|
| decides what's needed (a sync plan, a lock, a pack format) | figures out how to do it on this machine |
| writes the prompt, with the exact context and rules | runs commands, edits files, asks the user |
| runs the meta harness from its lock, in its own config root | works inside the permissions that config grants |
| **verifies the result** and reports it | |

The agent never decides whether the job succeeded. hlock re-plans, re-loads, or
re-captures and says so.

## 2. Goals and non-goals

### Goals

1. **No hidden daily driver.** The agent hlock delegates to is named, pinned and
   checkable in `harness.meta.lock`, just like the project's agent in `harness.lock`.
2. **Works out of the box, and stays open.** The default meta harness is an agent built
   into hlock's Rust core (§4.4): nothing to install, no Node or Python runtime beyond
   hlock itself. Users who want a full harness can pick an open-source one (Pi, or Codex
   and Goose, written in Rust), which runs from a dedicated config root hlock writes,
   never the user's own config.
3. **Any model, keys stay in the environment.** It talks to an API provider with the
   user's API key, or to open weights served by a local runtime (Ollama, LM Studio,
   llama.cpp, …). Keys are read from environment variables; hlock writes only the
   variable's name.
4. **Onboarding in one command.** `hlock meta init` picks a harness, a provider and a
   model from what's on the machine, writes the config, offers to install the harness
   and download weights, pins the result, and lists anything left to do.
5. **Same verbs, same mental model.** `hlock meta <verb>` behaves like `hlock <verb>` on
   the meta pair of files. New verbs: `run`, `jobs`, `verify`.
6. **Deterministic verification.** Every built-in job ends with a check hlock runs
   itself. Custom jobs declare one.
7. **Safe by default.** Interactive by default, so the user sees and approves what the
   agent does. Headless only when asked for. hlock never passes a harness's
   permission-bypass flags.

### Non-goals (this draft)

- Choosing the meta harness by search (`hlock meta resolve`) or tuning it
  (`hlock meta tune`). Both need native trial execution and a meta eval pack (§11).
- hlock commands calling the meta harness on their own. Delegation is always an
  explicit `hlock meta run` (§11 discusses opt-in hooks like `hlock sync --meta`).
- A user-level meta harness shared across projects (§11).

## 3. Concepts

| Term | Meaning |
|---|---|
| **Meta harness** | The agent hlock uses for its own judgment work: the built-in agent, or an open-source harness with a config root of its own. |
| **Built-in agent** | A tool-calling agent loop in hlock's Rust core (`harness = "builtin"`), the default meta harness. |
| **Meta pair** | `harness.meta.toml` (the meta manifest) and `harness.meta.lock` (the pinned meta harness), next to the project's `harness.lock`. |
| **Meta root** | An installed meta harness's own config root, `.hlock/env/harness.meta/<harness>/…`. `hlock meta pin` writes it from `[model]` and captures it; every run uses it. The built-in agent has none. |
| **Job** | A unit of delegated work: a prompt hlock renders from project state, plus a verification hlock runs afterwards. Built in, or declared in `harness.meta.toml`. |
| **Run** | One execution of a job, recorded under `.hlock/meta/runs/`. |

## 4. Choosing the meta harness and model

### 4.1 Which harnesses qualify

The built-in agent always qualifies, and is the default. Otherwise, a harness can be the
meta harness when its profile says it is open source
(`open-source = true`), its config root can be relocated (`config-root.env`), and it can
take a prompt (`[launch] prompt` or `interactive`). A project's own
[code harness](../docs/guides/harnesses.md#your-own-agent-frameworks-and-custom-scripts)
qualifies too: its code is the user's.

| Harness | Language | API providers hlock can configure | Local models | Headless | First message |
|---|---|---|---|---|---|
| **built-in** (default) | Rust, in hlock | Anthropic, OpenAI, Google, OpenRouter, any OpenAI-compatible URL | yes | yes | yes |
| Pi | TypeScript | Anthropic, OpenAI, Google, OpenRouter | yes | `-p` | yes |
| Codex | Rust | OpenAI | yes | `exec` | yes |
| Goose | Rust | Anthropic, OpenAI, Google | yes | `run -t` | no |
| OpenCode | TypeScript | Anthropic, OpenAI, Google | yes | `run` | `--prompt` |
| Hermes | Python | (configure it yourself) | yes | `-z` | no |

Proprietary harnesses (Claude Code, Cursor Agent, Copilot CLI, Amp) and harnesses whose
root can't be relocated (Gemini CLI) are refused, with the reason.

Why a built-in agent rather than an installed one by default: an external harness means
an install step (Pi needs Node and npm), and the Rust harnesses can't be embedded. Codex
publishes only its protocol types to crates.io (`codex-core` is git-only, which would
block publishing hlock's crates), and Block's Goose isn't on crates.io at all. A small
loop of hlock's own works everywhere hlock does, and the jobs it runs are narrow and
verified, so it doesn't need to match a full harness's capability. Pi, Codex and Goose
stay available for users who want one (`hlock meta init --harness pi`).

### 4.2 `hlock meta init`

```text
hlock meta init [--harness NAME] [--provider NAME | --local] [--model ID]
                [--runtime NAME] [--memory-gb N] [-y] [--force]
```

With no options, `init` chooses:

1. **Harness:** the built-in agent, unless `--harness` names an installed one.
2. **Model:**
   - the first provider the harness supports whose API key is set, with its default
     model (Anthropic `claude-sonnet-5-5`, OpenAI `gpt-5.5`, Google `gemini-3.5-flash`;
     OpenRouter needs `--model`);
   - else, when no key is set and a local runtime is installed, that runtime and a
     model for it: a catalog model already on disk, else the smallest model of the best
     tier that fits the machine's memory (for example Ollama's `qwen3-coder:30b`);
   - else the harness's first provider, with a note to set its key.

`--provider`, `--local`, `--runtime`, `--model` and `--base-url` (any OpenAI-compatible
endpoint, such as a gateway) override each choice. Then `init`:

1. writes `harness.meta.toml`;
2. for an installed harness that isn't there yet, offers to install it with the
   profile's install command (`npm install -g @earendil-works/pi-coding-agent`);
3. offers to download the local model, if it isn't on disk (`ollama pull qwen3-coder:30b`);
4. runs `hlock meta pin`;
5. lists what's left: a key to export, a runtime to install.

`-y` accepts the offers. Without a terminal, `init` doesn't install or download anything.

### 4.3 Keys

The key itself is never written: not to `harness.meta.toml`, the meta root, the lock, or
run records. `[model] key-env` names the variable, and the harness reads it from the
environment the way it always does. `hlock meta run` checks the variable is set before
starting and says which one to export. Local runtimes need no key. The placeholders
local endpoints accept in a key field (`"local"`, `"ollama"`, `"EMPTY"`, …) aren't
treated as secrets when pinning.

### 4.4 The built-in agent

A tool-calling loop in `harness-lock-core` (`agent.rs`), run as `hlock meta agent`, a
hidden subcommand, so jobs run, stream and get recorded exactly as an installed
harness's do.

- **Models.** Two wire formats cover every provider: Anthropic's Messages API, and
  OpenAI chat completions (OpenAI, OpenRouter, Gemini's OpenAI-compatible endpoint, and
  every local runtime: Ollama, LM Studio, llama.cpp, vLLM, …). HTTP is a small blocking
  client (`ureq` with rustls), so the core stays synchronous with no async runtime.
  Small local models often write a tool call as JSON text instead of a structured call;
  the loop accepts that for known tools.
- **Tools.** `list_dir`, `read_file`, `search` (anywhere); `write_file`, `edit_file`
  (inside the project only); `run` (a shell command in the project, with a timeout and
  output truncated for the model).
- **Permissions.** `[agent] allow` lists command prefixes that run without asking
  (default: `@hlock`, `git log`, `git show`, `git diff`, `git status`). A command matches
  only when it's plain words (no pipes, redirects, `;`, `&&`, `$(…)` or backticks) that
  start with an allowed prefix's words. A `!` entry refuses commands that start with its
  words and carry all of its flags. `@hlock` is the hlock commands the jobs need that
  don't run code from the project or install software (not `resolve`, `run`,
  `evals check --run`, `evals grade`, or `sync --install`): the
  agent can write files in the project, so those would let it run anything. Interactive runs put every other command to the
  user (`y`, `n`, or `a` to allow that program for the rest of the run); headless runs
  refuse it and tell the model why.
- **Limits.** `[agent] max-steps` (default 60 model turns) and `timeout-seconds` (default
  300 per command). Ctrl-C stops the run between steps.
- **Pinning.** The meta lock records `harness = { name = "builtin", version = <hlock's>,
  source = "hlock@<version>" }`, the model (provider, name, endpoint, and the key's
  variable in `params`), and the allowlist and limits in `[policy]`. `hlock meta check`
  fails when any of them drifts from `harness.meta.toml`, or when hlock's version
  changed; `hlock meta run` refuses a lock from another hlock version unless
  `--allow-version-mismatch`.

## 5. Files

```text
project/
├── harness.toml          # the project's manifest (unchanged)
├── harness.lock          # the project's lock (unchanged)
├── harness.meta.toml     # which meta harness and model; custom jobs   (commit)
├── harness.meta.lock     # the pinned meta harness                      (commit)
└── .hlock/
    ├── vendor/meta/<harness>/        # the meta root's vendored config  (commit)
    ├── env/harness.meta/<harness>/   # the meta root                    (ignore; rebuilt from the lock)
    └── meta/runs/<job>-<timestamp>/  # run records                      (ignored by a .gitignore hlock writes)
```

The meta pair sits in the directory of the project's lock (`HLOCK_META_MANIFEST` and
`HLOCK_META_LOCK` override the paths). The meta configuration is a **separate file**, not
a `[meta]` table in `harness.toml`, for two reasons:

- `harness.lock`'s `manifest-hash` covers the whole of `harness.toml`. A `[meta]` table
  there would mark the project's lock as drifted whenever the meta harness changed.
- The meta pair mirrors the project pair, so every `hlock meta <verb>` is
  `hlock <verb>` on the meta files.

`harness.meta.lock` is an ordinary pinned lock (`kind = "pin"`) of the meta root.
`show`, `tree` and `localize` read it unchanged.

**Vendoring per lock.** A lock named `harness.<name>.lock` (or `<name>.lock`) vendors into
`.hlock/vendor/<name>/<harness>/`; `harness.lock` keeps `.hlock/vendor/<harness>/`. The
meta lock and the project's lock never overwrite each other's vendored files, even when
both pin Pi.

**Config under the project.** The meta root lives inside the project, and a harness's
own config root is never mistaken for project files when pinning, so its config is
vendored (`restore = "whole"`), not just recorded.

### 5.1 `harness.meta.toml`

```toml
# hlock meta manifest: the agent hlock uses for its own work.
[target]
harness = "pi"               # an open-source harness: pi, codex, goose, opencode, hermes

[model]
provider = "anthropic"
name = "claude-sonnet-5-5"
key-env = "ANTHROPIC_API_KEY" # the variable holding the API key; the key is never written

[run]
mode = "interactive"         # or "headless": the default for hlock meta run

[jobs.install]               # extra instructions for a built-in job
instructions = "Prefer Homebrew on macOS. Never use sudo."

[jobs.changelog]             # a custom job
description = "Draft a CHANGELOG entry for the latest lock change"
prompt = """
Read `git diff HEAD~1 -- {lock}` and add an entry to CHANGELOG.md describing what
changed in the agent setup and why it matters.
"""
verify = "grep -q '## Unreleased' CHANGELOG.md"
```

A local model replaces `provider` and `key-env` with `runtime`:

```toml
[model]
runtime = "ollama"           # hlock runtime list
name = "qwen3-coder:30b"
# base-url = "http://gpu-box:11434/v1"   # default: the runtime's local port
```

| Key | Description |
|---|---|
| `target.harness` | The meta harness. It must qualify (§4.1). |
| `model.provider`, `model.name`, `model.key-env` | An API provider, its model, and the variable holding its key |
| `model.runtime`, `model.name` | A local runtime serving open weights, and the model |
| `model.base-url` | Where the model's API is, overriding the provider's or runtime's default |
| `agent.allow`, `agent.max-steps`, `agent.timeout-seconds` | What the built-in agent may run, and its limits (§4.4) |
| `run.mode` | `interactive` (default) or `headless`. `--headless` and `--interactive` override it. |
| `jobs.<built-in>.instructions` | Text appended to a built-in job's prompt |
| `jobs.<name>.description` | One line for `hlock meta jobs` |
| `jobs.<name>.prompt` | A custom job's prompt. `{lock}`, `{manifest}`, `{project}` and `{meta_lock}` are substituted with paths. |
| `jobs.<name>.verify` | A shell command run in the project after the agent exits. Exit 0 means verified. Without it, the run is reported as unverified. |

A built-in job takes only `instructions`. A custom job needs a `prompt`.

### 5.2 Profile keys

Harness profiles gain what the meta harness needs:

```toml
api-keys = [{ provider = "anthropic", env = "ANTHROPIC_API_KEY" }, { provider = "openai", env = "OPENAI_API_KEY" }]

[launch]
model-flag = "--model"
prompt = ["-p", "{prompt}"]       # run one prompt non-interactively and exit (hlock run, and headless jobs)
interactive = ["{prompt}"]        # a session whose first message is the prompt

# How hlock meta sets an API provider and model (like local-provider, which it uses for local models).
[[api-provider]]
file = "settings"
set = { "/defaultProvider" = "{provider}", "/defaultModel" = "{model}" }
```

`prompt` is the key [`hlock run`](../docs/reference/cli.md#run) already uses for agent
CLIs: its arguments go right after the binary, before the model flag, so a subcommand
(`codex exec`, `opencode run`) gets its flags after it. `interactive` arguments go at the
end, like any arguments passed to `hlock launch`.

| Harness | `prompt` (headless) | `interactive` |
|---|---|---|
| Pi | `-p {prompt}` | `{prompt}` |
| Codex | `exec {prompt}` | `{prompt}` |
| OpenCode | `run {prompt}` | `--prompt {prompt}` |
| Goose | `run -t {prompt}` | |
| Hermes | `-z {prompt}` | |
| Claude Code, Cursor Agent | `-p {prompt}` | `{prompt}` |
| Gemini CLI, Copilot CLI | `-p {prompt}` | `-i {prompt}` |
| Amp | `-x {prompt}` | |

The proprietary harnesses' `interactive` keys are there for completeness; `hlock meta`
doesn't run them.

## 6. Commands

`hlock meta` takes the verbs that make sense for a harness hlock *uses* rather than one
it *searches for*:

| Command | Base verb | What it does |
|---|---|---|
| `hlock meta init` | `init` | Choose the meta harness and model, write `harness.meta.toml`, install, pin (§4.2) |
| `hlock meta pin` | `pin` | Write `[model]` into the meta root, then capture the meta root into `harness.meta.lock` |
| `hlock meta check` | `check` | Fail if the lock isn't for `[target]`'s harness and `[model]`'s model, or the meta root drifted from the lock |
| `hlock meta show` | `show` | Show the meta lock, and whether the key variable is set |
| `hlock meta sync` | `sync` | For an installed harness: rehydrate the meta root from the lock; `--install` installs the harness, `--pull` downloads weights |
| `hlock meta launch` | `launch` | For an installed harness: start an interactive session (to sign in, or to look around) |
| `hlock meta jobs` | | List the built-in and custom jobs |
| `hlock meta run JOB` | `run` | Run a job in the meta harness, then verify it |
| `hlock meta verify JOB` | | Run only a job's verification (what the agent and CI call) |

Verbs that don't carry over yet:

- `resolve`, `tune`, `upgrade`: choosing or tuning the meta harness by evidence needs
  trials that run real harnesses and an eval pack of meta tasks (§11).
- `localize`: `hlock meta init --local` covers it.
- `tree`: `hlock tree --lock harness.meta.lock`.

The meta harness always runs from the meta root: there's no `--in-place`.

### 6.1 `hlock meta run`

```text
hlock meta run JOB [INSTRUCTIONS...] [--headless | --interactive]
                   [--allow-version-mismatch] [--out DIR] [--count N] [-t TEMPLATE] [--pin]
                   [--dry-run] [--json]
```

1. **Load** `harness.meta.lock`. It's required: delegating to an unpinned agent is
   what this feature removes. The error says to run `hlock meta init`.
2. **Preflight**: the key variable is set; a local runtime is installed (a model that
   isn't on disk yet is a note).
3. **Prepare** the meta harness the way `hlock meta launch` does: check the installed
   version against the lock, and restore the locked meta root.
4. **Render** the job's prompt from project state (§7), with the user's `INSTRUCTIONS`
   and the job's `instructions` appended. The prompt is written to the run directory.
5. **Run** the meta harness:
   - *interactive*: a normal session with the prompt as its first message, attached to
     the terminal. The user watches, approves, and exits when it's done. A harness with
     no first-message mode starts empty, and hlock prints one line to paste that points
     it at the prompt file;
   - *headless*: the harness runs the prompt non-interactively. Output streams to the
     terminal and to `output.log`.
6. **Verify** with hlock's own check (§7) and print the verdict.

`--dry-run` stops after step 4 and prints the command and the prompt (preflight problems
become notes); `--json` prints them as JSON. Code harnesses always run headless, through
the [code-harness contract](../docs/guides/harnesses.md#how-hlock-runs-a-code-harness)
with the prompt in `HLOCK_PROMPT`.

Exit codes:

| Code | Meaning |
|---|---|
| `0` | Verified |
| `1` | Verification failed, the harness failed, or the job couldn't start |
| `3` | The agent did its part; what's left needs the user (for example, secrets to set), as with `hlock sync` |

A custom job without `verify` is reported as *unverified* and exits with the harness's
exit code.

## 7. Built-in jobs

Every prompt has the same frame:

```markdown
You are hlock's meta harness: the agent hlock (harness-lock) calls for work that
needs judgment. When you finish, hlock checks the result itself, so do the job
rather than describe it.

Project: <path>

## Job: <name>
<goal, with the context hlock computed>

## Rules
- Stay inside the job. Don't change files or settings it doesn't need.
- Ask the user before installing system software or downloading large files, and say how big they are.
- Never print, log, or commit secret values. Ask the user to set secrets in their shell; don't set them yourself.
- <environment note: see §8>

## Check your work
Run `hlock meta verify <name> …` and fix what it reports before you finish.

## Instructions from the user
<INSTRUCTIONS and [jobs.<name>] instructions, if any>
```

The agent and hlock run the same check: the agent runs it to know when it's done, and
hlock runs it afterwards to decide. hlock puts its own directory first on the agent's
`PATH`, so the agent runs the same `hlock`, however it was installed.

### 7.1 `install`: install what the project's lock needs

- **Input:** the project's lock (`harness.lock`, or `--lock`).
- **Context:** the lock's [sync plan](../docs/guides/sync.md) for this machine: each
  step that isn't satisfied, with why it's needed, the suggested command, and the check.
  Package and model steps are included whether or not hlock could run them itself.
- **Goal:** leave no step that needs a person, then run `hlock sync --yes` for the
  automatic ones.
- **Verify:** plan again. Verified when no non-optional step is left except secrets.
  The agent must not set secrets: with only secrets left, the run exits `3` and lists
  the `HLOCK_SECRET_*` variables to set.

`hlock sync --skill` stays for users who'd rather drive their own agent.

### 7.2 `bootstrap-evals`: fill in an eval pack from the project

This builds on [`hlock evals`](../docs/guides/eval-packs.md), which already scaffolds
templates for the user's own agent to fill in. The meta harness is that agent, with hlock
running it and checking the result.

- **Input:** `--out DIR` (default: the manifest's first `[evals] packs` entry, else
  `./evals`), `--template` (default `coding`; any `hlock evals templates` entry except
  `hello-world`, which is already complete), `--count N` (default 10, the minimum for a
  useful holdout split).
- **Scaffold (hlock, deterministic):** with no `pack.toml` in `DIR`, write the template
  with `N` `TODO(hlock)` slots, as `hlock evals init -t <template> --tasks N` does. The
  template's guide (the `hlock-eval-pack` Agent Skill: where to look for real tasks, the
  task kinds, the rules) is written to the run record and named in the prompt.
- **Goal:** replace every slot with a task drawn from real work in this repository (its
  history, issues, tests, docs), with a checkable outcome. For a complete pack, add tasks
  until there are `N`, without changing existing ones, and bump the version.
- **Verify:** `EvalPack::check`, as `hlock evals check` does. Any error (a field missing,
  an open `TODO(hlock)`, a missing prompt or fixtures directory, a duplicate id) fails
  the job; warnings (a task that always passes, too few tasks for a holdout split) are
  reported and don't. Fewer tasks than `--count` is a note.

### 7.3 `update`: update locked skills, MCP servers and plugins

- **Input:** the project's lock, which must be a pinned lock (`hlock pin`). A resolved
  lock's components come from `harness.toml`; updating them is
  [`hlock upgrade`](../docs/guides/upgrade.md)'s job.
- **Context:** the project harness's config root, and each skill (name, origin, source),
  MCP server (transport, command and arguments, or URL) and plugin (kind, version,
  source) in the lock.
- **Goal:** move each one to its newest upstream release through the harness's own
  mechanism, skipping anything that looks like a breaking change unless the user says
  otherwise, and summarize what changed and what was skipped.
- **Verify:** re-capture the project's harness and diff it against the lock, as
  `hlock check` does. Verified when the capture succeeds. The diff is the report: hlock
  prints it, and `--pin` re-pins the project's lock so the update is recorded. Without
  `--pin`, the lock is left alone and `hlock check` shows the drift until `hlock pin`.

## 8. Isolation and permissions

**The built-in agent** writes only inside the project, runs only what `[agent] allow` or
the user allows (§4.4), and runs with the user's environment as is, so the commands the
user approves act on their real setup. The rest of this section is about installed
harnesses.

**Isolation.** An installed meta harness runs from its lock in the meta root, by setting the
harness's config-root variable (`PI_CODING_AGENT_DIR`, `CODEX_HOME`, `XDG_CONFIG_HOME`,
…). Commands the agent runs inherit it. Without care, an agent fixing the project's Pi
setup would edit its own copy instead. Two rules prevent that:

1. hlock sets `HLOCK_META_RESTORE` in the meta harness's environment: the variables it
   changed, with their original values. Any `hlock` command run inside the session puts
   them back (for its own process and everything it starts), so `hlock sync`,
   `hlock pin` and `hlock meta verify` act on the user's real setup.
2. The prompt's environment note names the variables and gives the exact prefix for
   other commands that must see the real setup (`env -u PI_CODING_AGENT_DIR pi install …`).

**Sign-in.** Harnesses that sign in with OAuth keep that state in their config root, so
the meta root starts signed out. API keys from the environment need no sign-in;
otherwise sign in once with `hlock meta launch`.

**Permissions.** hlock never passes permission-bypass flags (`--dangerously-skip-permissions`,
`--yolo`, …). What the meta harness may do is its own configuration, which the meta lock
pins and `hlock meta check` enforces. Interactive runs ask the user as usual. A headless
run can do only what that configuration already allows; to grant more, configure the
meta root (`hlock meta launch`), then `hlock meta pin`, so the grant is in a reviewed lock.

**Secrets.** Prompts tell the agent never to handle secret values. `install` treats
secrets as the user's job and exits `3` when they're the only thing left.

## 9. Run records

Each run writes `.hlock/meta/runs/<job>-<UTC timestamp>/`:

| File | Contents |
|---|---|
| `prompt.md` | The exact prompt the meta harness received |
| `output.log` | Headless runs: the harness's combined output |
| `result.json` | Job, meta harness and version, mode, command (with the prompt as the file's path), harness exit code, verdict, start and end times |

hlock writes `.hlock/meta/.gitignore` (ignoring everything) the first time, because
agent output can quote anything it read.

## 10. Python API

The CLI is a thin layer over bindings, as for every other command:

| Function | Returns |
|---|---|
| `meta_init(harness=None, provider=None, model=None, runtime=None, memory_gb=None)` | JSON: the choice (§4.2) and the `harness.meta.toml` text to write |
| `meta_pin(out, meta_manifest, vendor=True, force=False)` | JSON: the pinned meta lock, its diff from the previous one, vendored files, warnings |
| `meta_check(path, meta_manifest=None)` | JSON list of drift lines (empty when it matches) |
| `meta_config(meta_manifest=None)` | JSON: target, model, default mode, and every job |
| `meta_prepare(job, lock, meta_lock, meta_manifest, ..., guide=None)` | JSON run plan: rendered prompt, argv, env, mode, notes. `guide` names a template guide for `bootstrap-evals`. |
| `agent_run(prompt, meta_lock, interactive=False, say=None, ask=None, project=None)` | JSON outcome of a built-in agent run: `status` (`done`, `max-steps`, `stopped`, `error`), `steps`, `summary`, token counts. `say` and `ask` are the progress and approval callbacks. |
| `meta_verify(job, lock, meta_lock, meta_manifest, ...)` | JSON verdict: `status` (`verified`, `failed`, `needs-you`, `unverified`), `summary`, `details`, `secrets`, `diff` |

## 11. Later

- **Choosing the meta harness by evidence.** A meta eval pack (install this lock on a
  fresh container, draft a pack for this repo, apply these updates) graded by the same
  verifications as §7, so `hlock meta resolve` can pick a meta harness and model and
  `hlock meta tune` can tune its prompts. Needs native trial execution (v0 §7.4).
- **Opt-in delegation from base commands:** `hlock sync --meta` runs `install` for the
  manual steps; `hlock evals init --meta` runs `bootstrap-evals` instead of writing a skill.
- **User-level meta harness:** `~/.config/hlock/harness.meta.lock` as a fallback when a
  project has none.
- **API-provider config for more harnesses:** Hermes, and providers beyond each profile's
  list (OpenRouter for Goose and OpenCode, Azure, Bedrock).
- **More jobs:** writing a code harness's profile, triaging a failed trial (with
  `harness-lock/triage`, v0 §11), drafting a `harness.toml` from a pinned lock.

## 12. Open questions

1. Should `install` run `hlock sync --install --pull --yes` itself before handing the
   rest to the agent, so the agent only sees what hlock can't do?
2. Should headless runs default to a time limit, and a spend limit for API models?
3. Should `update` group changes by risk (patch, minor, major) and ask before majors in
   headless mode?
4. Should the built-in agent gain a sandbox (a container, or OS-level restrictions) so
   headless runs can allow more than an allowlist safely?
