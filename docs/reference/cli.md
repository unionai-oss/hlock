# CLI reference

```console
$ hlock --help
 Usage: hlock [OPTIONS] COMMAND [ARGS]...

 hlock · make your agent harness pinnable, portable, and tunable.
```

Every command works on a project's `harness.toml` and `harness.lock`; see
[Global options](#global-options) for how hlock finds them.
Output is colored on a terminal and plain text when piped or when `NO_COLOR` is set.
Failures print a one-line error with a hint and exit `1`.

| Command | Purpose |
|---|---|
| [`pin`](#pin) | Pin your current harness setup into `harness.lock` |
| [`check`](#check) | Fail if a pinned setup changed, or `harness.toml` has drifted from `harness.lock` |
| [`add`](#add) | Declare a dependency, pin it, and install it into your harness |
| [`remove`](#remove) | Undeclare a dependency, unpin it, and take it out of your harness |
| [`outdated`](#outdated) | List declared dependencies with a newer commit, tag or version upstream |
| [`export`](#export) | Write the lock as `apm.lock.yaml` for APM, or as a CycloneDX or SPDX SBOM |
| [`audit`](#audit) | Check a lock's integrity, hidden characters, executables, policy and secrets |
| [`trust`](#trust) | List what a lock would run on this machine, and approve or deny it |
| [`sync`](#sync) | Rehydrate the setup in a lock on this machine, in a prefix, or in a Docker image |
| [`launch`](#launch) | Start a harness session that matches the lock |
| [`pull`](#pull) | Pull someone else's lock from a git repository |
| [`list`](#list) | List the locks you've pulled |
| [`rm`](#rm) | Remove pulled locks |
| [`run`](#run) | Run the locked agent on a prompt |
| [`evals`](#evals) | Write a starter eval pack from a template, check it, and grade with it |
| [`decide`](#decide) | Ask a decision model typed questions, and answer harness hooks with one |
| [`init`](#init) | Write a starter `harness.toml` |
| [`resolve`](#resolve) | Search the space and pin the winner in `harness.lock` |
| [`tune`](#tune) | Tune prompts, hyperparameters, and small models into `harness.tune.lock` |
| [`tune list`](#tune-list) | List the project's background tune jobs |
| [`tune show`](#tune-show) | Show a tune job's status, settings, and result |
| [`tune fetch`](#tune-fetch) | Write a finished tune job's result to `harness.tune.lock` |
| [`tune cancel`](#tune-cancel) | Stop a running tune job |
| [`upgrade`](#upgrade) | Plan, apply, or roll back component upgrades on a resolved lock |
| [`migrate`](#migrate) | Write a copy of a lock moved onto another harness |
| [`localize`](#localize) | Write an all-local equivalent of a lock |
| [`meta`](#meta) | Set up and run the meta harness, the agent hlock uses for its own work |
| [`harness list`](#harness-list) | List known harnesses and what's installed |
| [`harness show`](#harness-show) | Show a harness profile and the paths it reads here |
| [`runtime list`](#runtime-list) | List local inference runtimes and what's installed |
| [`show`](#show) | Show the locked stack and its evidence |
| [`tree`](#tree) | Print the locked component graph |
| [`backend status`](#backend-status) | Show the backend, how it was found, and what it runs |
| [`backend list`](#backend-list) | List the backends you can select |

## Global options

| Option | Environment | Description |
|---|---|---|
| `-m`, `--manifest PATH` | `HLOCK_MANIFEST` | The `harness.toml` to use |
| `--lock PATH` | `HLOCK_LOCKFILE` | The lockfile to use, or the name of a [pulled](#pull) lock |
| `-V`, `--version` | | Print the version |
| `-h`, `--help` | | Show help |

`-m` and `--lock` work before or after the command: `hlock -m agent/harness.toml resolve`
and `hlock resolve -m agent/harness.toml` are the same. The one exception is `launch`, where
everything after the command goes to the harness, so put them first.

**How hlock finds the project.** Without `-m`, hlock uses the nearest `harness.toml` in the
current directory or any parent, the way Cargo finds `Cargo.toml`, so commands work from
anywhere inside a project. The directory holding the manifest is the **project root**:

- relative paths in the manifest (`[evals] packs`, instruction files) resolve against it;
- the lock defaults to `harness.lock` next to the manifest;
- `.hlock/` lives there: trial artifacts (`store/`, `runs/`, `tune/`), vendored files (`vendor/`), and synced environments (`env/`).

Without a manifest (a project that only has a pinned lock), hlock uses the nearest
`harness.lock` instead. Paths you pass on the command line, like `-o` or `--evals`, are
relative to where you run the command.

## `init`

Write a starter `harness.toml` that uses the reference provider.

```bash
hlock init [--name NAME] [--pack PATH] [--harness NAME] [--force]
```

| Option | Default | Description |
|---|---|---|
| `--name` | `demo` | Project name |
| `--pack` | `./evals` | Eval pack directory. Warns if it has no `pack.toml`, and suggests `hlock evals init`. |
| `--harness` | the harness you're running inside | Write `[target] harness`. Without it, and outside a harness, `[target]` is left commented out. |
| `--force` | | Overwrite an existing `harness.toml` |

## `evals`

Write, check, and grade [eval packs](../guides/eval-packs.md).

`evals check`, `evals grade`, `evals run` and `evals calibrate` find the pack the same way
when you don't name one: the current directory if it has a `pack.toml`, else the first of
`[evals] packs` in `harness.toml`, else `./evals`.

### `evals init`

Write a starter eval pack. The default `hello-world` pack is complete and runs as is.
The other templates write task slots marked `TODO(hlock)`, plus an Agent Skill that guides
your agent through filling them in from your project. `--from-traces` drafts the tasks
from your agent's past sessions instead; see
[Draft a pack from your traces](../guides/eval-packs.md#draft-a-pack-from-your-traces).

```bash
hlock evals init [DIRECTORY] [-t TEMPLATE | --from-traces [PATH]] [--name NAME] [--tasks N]
                 [--no-skill] [--skill-dir DIR] [--force]
```

| Option | Default | Description |
|---|---|---|
| `DIRECTORY` | `./evals` | Where to write the pack |
| `-t`, `--template` | `hello-world` | `hello-world`, `coding`, `data`, `research`, `support`, or `blank` |
| `--name` | `<project>/<directory>` | Pack name |
| `--from-traces` | | Draft tasks from the sessions Claude Code, Codex and Pi logged for this project, or from `PATH`: a session log, or JSONL/CSV records with a prompt column |
| `--tasks` | `10` | How many task slots a template writes, or the most tasks `--from-traces` drafts. Ignored by `hello-world`. |
| `--no-skill` | | Don't write the Agent Skill |
| `--skill-dir` | `.agents/skills/hlock-eval-pack` | Where the skill's `SKILL.md` goes |
| `--force` | | Overwrite an existing `pack.toml` |

### `evals templates`

List the templates, with the kinds of task each one has slots for. Takes `--json`.

### `evals check`

Check that a pack is ready: required fields, no unknown keys, prompts and fixtures
present, graders that parse and whose files exist, no `TODO(hlock)` left, and tasks that
can fail. An unknown key in `[pack]`, `[pack.judge]` or a `[[task]]` is an error that
names the closest known key, since a mistyped key is otherwise ignored. Exits `1` on
errors; warnings don't fail. Takes `--json`.

```bash
hlock evals check [DIRECTORY] [--run [--judges]] [--json]
```

| Option | Default | Description |
|---|---|---|
| `DIRECTORY` | [the pack](#evals) | The pack |
| `--run` | | Also run each task's graders on its fixtures alone (they should fail) and on its fixtures plus its `solution` (they should pass): test commands, and judges that are decision models. Warns when a judge is uncertain on a solution. See [Check your graders](../guides/eval-packs.md#check-your-graders). |
| `--judges` | | With `--run`, also run LLM judges |

`--run` runs nothing while the pack has errors, and says so. The project's judge settings
(`[evals.judge]`, `[deciders.judge]`, `[deciders.default]`) apply.

### `evals grade`

Grade a directory as if a run of a task left it, with all of the task's graders, judges
included, and print each grade with its score and detail. With no `WORKSPACE`, grade the
task's solution. It grades a copy, so the directory isn't changed. Exits `1` if a grader
fails. Takes `--json`.

```bash
hlock evals grade TASK [WORKSPACE] [--pack DIR] [--trust] [--no-sandbox] [--json]
```

| Option | Default | Description |
|---|---|---|
| `TASK` | | The task id |
| `WORKSPACE` | the task's fixtures and solution | The directory to grade |
| `--pack` | [the pack](#evals) | The pack |
| `--trust` | | Approve the lock's sandbox without asking |
| `--no-sandbox` | | Run without the lock's [sandbox](../guides/sandbox.md) (also `HLOCK_NO_SANDBOX=1`) |

`tests` graders run in the lock's [sandbox](../guides/sandbox.md) when it covers `graders`.

Judges use `[evals.judge]` from the project's `harness.toml`, else the pack's
`[pack.judge]`, else `[deciders.judge]` or `[deciders.default]`, else one hlock finds (see
[Which judge](../guides/eval-packs.md#which-judge)). A judge's grade lists each criterion's
probability, and is marked `uncertain` near the line.

### `evals run`

Run the agent `harness.lock` pins on the pack's tasks, each in a fresh workspace, and grade
each run with the task's graders, judges included. It prints one line per run, with its
score, time and workspace, and the graders that failed. Exits `1` if a run fails. Takes
`--json`. Nothing is recorded in the lock. See
[Try tasks on your agent](../guides/eval-packs.md#try-tasks-on-your-agent).

```bash
hlock evals run [TASK]... [--pack DIR] [--seeds N] [--workdir DIR] [--trust] [--no-sandbox] [--json]
```

| Option | Default | Description |
|---|---|---|
| `TASK` | every task | The tasks to run |
| `--pack` | [the pack](#evals) | The pack |
| `--seeds` | `1` | Runs per task |
| `--workdir` | a new temporary directory | Where the workspaces (`<task>-<seed>/`) and agent logs (`<task>-<seed>.log`) go |
| `--trust` | | Approve every executable the lock would run |
| `--no-sandbox` | | Run without the lock's [sandbox](../guides/sandbox.md) (also `HLOCK_NO_SANDBOX=1`) |

An agent CLI runs through its non-interactive mode, as [`run`](#run) runs it, with the
workspace as its working directory and the lock's project-scoped skills and instruction
files set up in it. A code harness runs through its contract. A lock for the simulated
`reference` harness is refused. A run that takes longer than the task's `timeout-seconds`
is stopped and fails.

### `evals calibrate`

Judge each task's solution, and its run with no work done, with the pack's judge and with
a second one, and print the criteria they disagree on and the share they agree on. Takes
`--json`.

```bash
hlock evals calibrate [DIRECTORY] --against MODEL [--against-base-url URL] [--model MODEL] [--json]
```

| Option | Default | Description |
|---|---|---|
| `DIRECTORY` | [the pack](#evals) | The pack |
| `--against` | | The second judge, as `provider:model` (an LLM, usually) |
| `--against-base-url` | | An OpenAI-compatible endpoint for `--against`, for a local LLM |
| `--model` | the pack's judge | The judge to calibrate |

## `decide`

[Decision models](../guides/deciders.md): typed questions answered with probabilities.

### `decide ask`

Ask a decision model about `STATE`: text, `@file`, or `-` for stdin. Prints each answer
with its probability, the model that answered, the input tokens, and the cost when the
provider publishes a price. Takes `--json`.

```bash
hlock decide ask STATE [--noul TEXT]... [--choice "Q=A,B"]... [--score "Q=L0|L1|…"]... [--model MODEL] [--use USE] [--json]
```

| Option | Description |
|---|---|
| `--noul` | A yes/no question |
| `--choice` | A question and its options after `=`, comma-separated |
| `--score` | A question and its levels after `=`, `|`-separated, worst first |
| `--model` | `provider:model`. Default: `[deciders.<use>]`, else `[deciders.default]` in `harness.toml`, else Kev on a local Ollama or Ollaya, else Jev when `TYPESAFE_API_KEY` is set. |
| `--use` | Use `[deciders.<use>]`'s model and settings |

### `decide hook`

Answer one harness hook event with the lock's decider for `USE` (`approve`, `stop`,
`route` or `tool`), in the harness's format. [`sync`](#sync) writes this command into the
harness's config (or a plugin that runs it), with the lock as the global `--lock`. It
prints the harness's own answer when the decider is sure enough and `may` allows it, and
otherwise prints nothing, so the harness does what it would have done. It always exits `0`.

```bash
hlock decide hook USE --format FORMAT [--event EVENT] [--input JSON] [--lock PATH] [-v]
```

| Option | Description |
|---|---|
| `--format` | `claude-code`, `codex`, `gemini`, `cursor`, `copilot`, or `plugin`: `{"answer": …, "reason": …}`, which the Amp, OpenCode and Pi plugins and code harnesses read |
| `--event` | The harness event the hook is registered for. Default: the input's `hook_event_name`. |
| `--input` | The event as JSON, instead of on stdin |
| `-v` | Say what happened on stderr (also `HLOCK_DECIDE_VERBOSE=1`) |

## `resolve`

Run the resolver over the manifest's search space, check the winner on the holdout split,
write `harness.lock`, and print every stack's scores and the chosen stack with its
evidence. See [Finding the best stack](../guides/resolve.md).

```bash
hlock resolve [--backend NAME] [--harness NAME] [--workdir DIR] [--dry-run] [--no-cache] [--no-sandbox] [--json]
```

| Option | Default | Description |
|---|---|---|
| `--backend` | manifest `[backend] target` | Where trials run: `offline`, `local`, `flyte`, or a [custom backend](../guides/backends.md#write-your-own-backend) |
| `--harness` | manifest `[target] harness` | Target harness for this run. See [Harnesses](../guides/harnesses.md#resolve-for-a-target). |
| `--workdir` | `.hlock` | Where trial artifacts, the trial cache (`trials/`) and `report.md` go |
| `--dry-run` | | List the stacks, the tasks in each split, and the trial counts, and run nothing |
| `--no-cache` | | Run every trial again, even ones an earlier resolve ran |
| `--no-sandbox` | | Run code-harness trials and `tests` graders without `[sandbox]` (also `HLOCK_NO_SANDBOX=1`); the lock's evidence records it |
| `--json` | | Print `{"lock", "report", "notes"}`: the lock, every stack's scores and the holdout check, and the notes |

Before any trial, `resolve` fails on an unknown manifest key, an unsupported `strategy`,
`primary` or `select`, more than 200 stacks, or an agent CLI target paired with `reference:`
models.

## `pin`

Capture the live setup of one harness (version, models, local runtimes, and the project's
MCP servers, skills, plugins, instruction files and config) into a pinned lock. `--user`
adds the user-level ones from the harness's config root in your home directory, and `-i GLOB`
adds only those matching a pattern. Secrets
become `${HLOCK_SECRET_…}` placeholders. See [Pinning your setup](../guides/pin.md) and
[Project and user scope](../guides/pin.md#project-and-user-scope).

```bash
hlock pin [--harness NAME] [-o FILE] [--user | -i GLOB ...] [--no-vendor] [--force] [--json]
```

| Option | Default | Description |
|---|---|---|
| `--harness` | `[target]`, else the harness you're running inside | Harness to pin |
| `-o`, `--out` | `harness.lock` | Where to write the lock |
| `--user` | | Also pin your user-level skills, MCP servers, plugins, instructions and config (`~/.claude`, `~/.pi/agent`, …) |
| `-i`, `--include-user-scope` | | Also pin the user-level items whose path matches this glob, relative to the harness's config root (or `~/…`, absolute). Repeatable; not with `--user`. See [Pick parts of your user setup](../guides/pin.md#pick-parts-of-your-user-setup) |
| `--no-vendor` | | Record digests only; don't copy files into `.hlock/vendor/` |
| `--force` | | Replace a resolved lock (one with evidence) |
| `--json` | | Print the lock, the diff from the previous pin, vendored files, warnings, and what a project-only pin left out (`skipped`) as JSON |

When the lock runs programs that `harness.toml`'s `[environment]` doesn't declare, `pin`
names them and suggests [`hlock env suggest`](#env-suggest).

## `check`

Compare the manifest hash recorded in `harness.lock` with the hash of the current
`harness.toml`. Exits `0` when they match and `1` on drift, or when either file is
missing. See [Using hlock in CI](../guides/ci.md).

For a pinned lock (`kind = "pin"`), `check` needs no `harness.toml`: it re-captures the
harness and fails with a list of what changed. It checks your own config, or with
`--prefix DIR`, the isolated root that `hlock sync --prefix DIR` writes (`hlock sync` uses
`.hlock/env` by default).

For a resolved lock, `check` also fails when the lock is for a different harness than `[target] harness`, or
when the installed harness's version differs from the one the lock pins (skipped when the
harness isn't installed). It warns, without failing, when the eval packs have changed
since the lock's evidence was measured (their digest no longer matches `eval-pack-digest`).

When the project declares dependencies (in `harness.toml` or `apm.yml`), `check` also fails
when one is missing, changed, unlocked or no longer declared, or when a skill in the
harness's project skill directory is neither declared nor pinned. It warns, without
failing, about stale `--eval` evidence and an out-of-date exported `apm.lock.yaml`.

`check` also scans what the lock places for [hidden characters](../guides/audit.md#hidden-characters)
(failing on a critical one the lock didn't allow), and reports the project's
[policy](../guides/audit.md#policy) violations (failing under `enforcement = "block"`). See
[Check what's declared](../guides/dependencies.md#check-whats-declared).

```bash
hlock check [--prefix DIR]
```

## `add`

Declare a dependency in `harness.toml`, resolve it to a commit (or package version) and
digest in `harness.lock`, and install it into the harness. Only that entry of the lock
changes; a project with no lock is pinned first. See
[Declaring dependencies](../guides/dependencies.md).

```bash
hlock add KIND SOURCE [--as NAME] [--scope project|user] [--eval [-y]] [--dry-run] [--no-install] [--harness NAME] [--json]
hlock add apm:OWNER/REPO[/PATH][#REF] [OPTIONS]
hlock add --adopt NAME
```

| Option | Description |
|---|---|
| `KIND` | `skill`, `mcp`, `instructions` or `plugin`. An `apm:` source needs none |
| `SOURCE` | Where it comes from. See [What you can add](../guides/dependencies.md#what-you-can-add) |
| `--as NAME` | The name to declare it under (default: from the source) |
| `--scope` | `project` (default: installed in the project, committed with it) or `user` |
| `--eval` | Run the eval pack with and without it, then ask before keeping it. See [Measure before you keep it](../guides/dependencies.md#measure-before-you-keep-it) |
| `-y`, `--yes` | With `--eval`, keep it without asking (required when stdin isn't a terminal) |
| `--dry-run` | Resolve it and print what would change; change nothing |
| `--no-install` | Declare and pin it, but don't install it (`hlock sync` does) |
| `--adopt NAME` | Declare something `hlock pin` already recorded, from the source the lock knows for it |
| `--harness` | The harness to install into when there's no lock yet |
| `--allow-hidden FILE` | Let through hidden characters in FILE, as the error names it. Repeatable |
| `--json` | Print the result as JSON |

## `remove`

Take a dependency out of `harness.toml` and `harness.lock`, and out of the harness. A
package takes everything it brought with it. A skill or instruction file changed by hand
since it was added is left in place, with a warning.

```bash
hlock remove KIND NAME [--keep-installed] [--json]
```

## `outdated`

Look upstream for declared dependencies with a newer commit, tag or version. `wanted` is
the newest the declaration allows; `latest` is the newest there is.

```bash
hlock outdated [KIND] [--json]
```

## `export`

Write the lock for another tool. `--format apm-lock` writes `apm.lock.yaml` as APM writes
it, so `apm install --frozen` gets the same dependencies as `hlock sync`. What APM has no
field for (the harness, models, runtimes, config, evidence, plugins) is left out, and the
summary says so. See [Teammates who use APM](../guides/dependencies.md#teammates-who-use-apm).
`--format cyclonedx` and `--format spdx` write an [SBOM](../guides/audit.md#sbom) of the
lock (CycloneDX 1.6 or SPDX 2.3 JSON), with each model as a machine-learning-model component.

```bash
hlock export --format apm-lock|cyclonedx|spdx [-o FILE] [--force] [--json]
```

| Option | Description |
|---|---|
| `--format` | `apm-lock`, `cyclonedx`, or `spdx` |
| `-o`, `--out` | Where to write it (default `apm.lock.yaml`, `sbom.cdx.json` or `sbom.spdx.json` next to the lock; `-` for stdout) |
| `--force` | Overwrite an `apm.lock.yaml` that pins a commit `harness.lock` doesn't know |
| `--json` | Print a summary as JSON |

## `env suggest`

List the programs the lock runs, the packages that would install each, and whether
`harness.toml`'s [`[environment]`](manifest.md#environment) declares them. `high` ones the
lock runs as written: an MCP server's command, a hook, a setting's command. `low` ones are
in a skill's scripts, a script a hook runs, or a shell code block in a skill or
instructions file, and may be only an example. Package names come from this machine's
package manager where it has the program (Homebrew's Cellar, an npm or uv tool, `dpkg -S`),
else from a table of common tools, so check them before you commit.

```bash
hlock env suggest [--write [--all]] [--json]
```

```console
$ hlock env suggest
program  confidence  status   packages                              run by
jq       high        missing  apt jq, brew jq                       hook PreToolUse Bash
npx      high        missing  apt npm, brew node                    mcp github
rg       high        missing  apt ripgrep, brew ripgrep             hook PreToolUse Bash
uvx      high        missing  brew uv, pip uv                       mcp fetch
gh       low         missing  apt gh, brew gh                       skill release: scripts/pr.sh
protoc   low         missing  apt protobuf-compiler, brew protobuf  skill release: scripts/pr.sh
next → hlock env suggest --write  ·  hlock meta run environment
```

| Option | Description |
|---|---|
| `--write` | Add each missing `high` program's packages to `[environment]`: to `apt` and `brew` when both have one, else to a language installer that has it everywhere (`pip uv`), keeping the file's comments and layout |
| `--all` | With `--write`: the `low` ones too |
| `--json` | Print the programs as JSON (`program`, `confidence`, `sources`, `packages`, `found`, `status`) |

A program the stack doesn't need goes under `[environment] ignore`.
[`hlock meta run environment`](../guides/meta.md#environment) has an agent decide each
one. `hlock pin` ends with a line naming the `high` programs `[environment]` neither
declares nor ignores.

## `audit`

Check a lock: its vendored and fetched files match their digests, nothing it places hides
text from a reviewer, what it would run and whether it's trusted here, the project's policy,
and no vendored file holds a secret value. See [Audit and trust](../guides/audit.md#hlock-audit).

```bash
hlock audit [LOCK] [--ci] [--format table|json|sarif] [-o FILE] [--strip] [-v]
```

| Option | Description |
|---|---|
| `LOCK` | Lock to audit (default `harness.lock`): a path, a pulled lock's name, or a source to pull first |
| `--ci` | Also fail on warnings about the lock's own content |
| `--format` | `table` (default), `json`, or `sarif` (SARIF 2.1.0, for code scanning) |
| `-o`, `--out` | Write the JSON or SARIF report to FILE |
| `--strip` | Remove hidden characters from vendored files first, and update their digests. Refuses when a vendored file doesn't match its digest |
| `-v`, `--verbose` | Also list info-level characters |

Exits `0` when it passes and `1` when it doesn't.

## `trust`

List what a lock would make this machine run (MCP servers, plugins, hooks and other settings
that run a command, skill scripts, the harness install), content it lets through with hidden
characters, and the project policy's `[sources] trusted` list, with each one's digest and
status, and approve or deny them.
Approvals are kept in `~/.hlock/trust.toml` by digest. See
[Executables](../guides/audit.md#executables).

```bash
hlock trust [LOCK] [-y] [--only NAME|DIGEST ...] [--deny NAME|DIGEST ...] [--json]
```

| Option | Description |
|---|---|
| `-y`, `--yes` | Approve every executable not yet trusted, without asking |
| `--only` | Approve only these: a name, a full digest, or at least 8 characters of one. Repeatable |
| `--deny` | Deny these: `sync` won't install them, and won't ask again. Repeatable |
| `--json` | Print the executables and their status as JSON; change nothing |

## `sync`

Build a plan to reproduce a lock's setup, run its automatic steps, and list what needs you.
The setup goes into an isolated config root under `.hlock/env/` unless you pass
`--in-place`. See [Rehydrating with sync](../guides/sync.md).

```bash
hlock sync [LOCK] [OPTIONS]
```

| Option | Description |
|---|---|
| `LOCK` | Lock to sync (default `harness.lock`): a path, a [pulled](#pull) lock's name, or a source to pull first. A pulled lock's isolated root is `~/.hlock/env/<name>/`, the one `launch` uses |
| `--plan` | Print the plan and change nothing |
| `--json` | Print the plan as JSON and change nothing |
| `-y`, `--yes` | Don't ask before running automatic steps (required when stdin isn't a terminal) |
| `--install` | Let hlock run package managers and install scripts |
| `--pull` | Let hlock download model weights through installed runtimes |
| `--all-models` | Include models no locked role uses |
| `--prefix DIR` | Where isolated config roots live (default `.hlock/env`) |
| `--in-place` | Write into your own config (`~/.claude`, …) instead of an isolated root |
| `--apps` | Write [desktop apps'](../guides/sync.md#desktop-apps) own config (Claude Desktop, VS Code), which they read instead of an isolated root. `--in-place` implies it |
| `--docker` | Write a Docker build context to `.hlock/docker/<harness>/` |
| `--build` | With `--docker`, also run `docker build` |
| `--base IMAGE` | With `--docker`, build on IMAGE (Debian-based) instead of `node:22-bookworm` |
| `--skill` | Write an Agent Skill that guides an agent through the remaining steps |
| `--skill-dir DIR` | Where `--skill` writes `SKILL.md` (default `.agents/skills/hlock-rehydrate`) |
| `--exact` | Also move aside skills that aren't in the lock |
| `--frozen` | Fail if the declared dependencies would change the lock, instead of updating it (for CI) |
| `--trust` | Approve every executable the lock would run (it was reviewed another way). Without it, an executable that isn't trusted here isn't installed; on a terminal, `sync` asks once |
| `--allow-hidden FILE` | Let through hidden characters in FILE. Repeatable |
| `-v`, `--verbose` | List every step, including optional ones |

For the project's own lock, `sync` first locks what `harness.toml` or `apm.yml` newly
declares, or declares differently (no network when nothing changed), then fetches each
declared dependency at its commit, and installs the portable ones into the harnesses in
`[target] also` too. See [Sync on another machine](../guides/dependencies.md#sync-on-another-machine).

Exits `0` when everything matches, `3` while manual steps remain, and `1` if an automatic
step fails.

## `launch`

Verify the installed harness, restore the lock into an isolated config root under
`.hlock/env/`, and start the harness there. Naming a different harness translates the lock's
portable parts into it. See [Launching a locked harness](../guides/launch.md).

```bash
hlock launch [HARNESS] [OPTIONS] [-- HARNESS_ARGS...]
```

| Option | Description |
|---|---|
| `HARNESS` | Harness to start (default: the lock's) |
| `--lock FILE` | Lock to launch (default `harness.lock`): a path, a [pulled](#pull) lock's name, or a source to pull first. A pulled lock's isolated roots go in `~/.hlock/env/<name>/` |
| `--prefix DIR` | Where isolated roots live (default `.hlock/env`) |
| `--in-place` | Use your own config instead of an isolated root (same harness only) |
| `--allow-version-mismatch` | Start even if the installed version differs from the lock |
| `--dry-run` | Prepare everything and print the command without starting it |
| `--trust` | Approve every executable the lock would run. Without it, `launch` asks on a terminal, and fails without one |
| `--allow-hidden FILE` | Let through hidden characters in FILE |
| `--json` | With `--dry-run`, print the launch plan as JSON |
| `--no-sandbox` | Run without the lock's [sandbox](../guides/sandbox.md) (also `HLOCK_NO_SANDBOX=1`) |

With a sandbox, `launch` restores the lock into `.hlock/sandbox-env/` and runs the harness
through its runner; `--dry-run` prints the runner's command too. Other arguments are passed to the harness. hlock's own options count anywhere before `--`.

Where the harness's config can reference environment variables (Claude Code's MCP servers,
OpenCode's config), secrets are written as references, and their values are passed in the
harness's environment. `--json` never includes them.

## `secret`

Store and inspect the values for a lock's `HLOCK_SECRET_*` placeholders. A value comes
from the environment variable, else a reference in `~/.config/hlock/secrets.toml`, else
the OS keychain. Values are never printed. See [Secrets](../guides/secrets.md).

```bash
hlock secret list [--json]     # the lock's secrets and where each value would come from
hlock secret set NAME          # store a value in the keychain (prompted, or read from stdin)
hlock secret rm NAME           # remove it from the keychain
```

`NAME` works with or without the `HLOCK_SECRET_` prefix. The keychain is the macOS
Keychain (through `security`) or the Secret Service on Linux (through `secret-tool`).

## `pull`

Fetch a lock and the files it vendors from a git repository into the store,
`~/.hlock/pulls/<name>/`, recording the source and commit in a `[pulled]` table. Use it from
any project by name. Nothing from the lock runs. With no source, refresh every pulled lock.
See [Pulling someone else's lock](../guides/pull.md).

```bash
hlock pull SOURCE [--as NAME | -o FILE | --into DIR] [--force] [--allow-hidden FILE ...] [--registry URL] [--json]
hlock pull [NAME | LOCK ...] [--json]
```

| Option | Default | Description |
|---|---|---|
| `SOURCE` | | `github:OWNER/REPO[/PATH][@REF]`, `gitlab:GROUP/REPO[//PATH][@REF]`, `git:URL[//PATH][@REF]`, a GitHub or GitLab URL, or a registry package `NAMESPACE/NAME[@VERSION]` (see [Sharing through a registry](../guides/registry.md)) |
| `NAME`, `LOCK` | every pulled lock | Pulled locks to refresh from their recorded source: names in the store, or lock files pulled with `-o` |
| `--as NAME` | the repository owner (the package name, from a registry) | Store the lock as `NAME` |
| `-o`, `--out FILE` | | Write the lock into the project as `FILE` (its vendored files go in `.hlock/vendor/<name>/`) instead of the store |
| `--into DIR` | | Write the whole setup into `DIR`: `harness.toml`, `harness.lock` and the files it vendors, `apm.yml`, `apm.lock.yaml` |
| `--force` | | Replace a lock of that name that wasn't pulled from the same repository (with `--into`, files that already exist) |
| `--registry URL` | `HLOCK_REGISTRY`, else the one you logged in to | The registry a `NAMESPACE/NAME` source is in |
| `--allow-hidden FILE` | | Let through [hidden characters](../guides/audit.md#hidden-characters) in FILE (a file or directory the lock vendors); the lock records the exception. Repeatable |
| `--json` | | Print the lock, its path, the source, the commit, the diff from the lock it replaced, vendored files, other locks in the repository, and warnings as JSON |

`HLOCK_HOME` moves the store (default `~/.hlock`). `HLOCK_GITHUB_URL` and
`HLOCK_GITLAB_URL` set the base URL for GitHub Enterprise and self-hosted GitLab. Fetching
needs `git` on `PATH`.

## `list`

List the pulled locks in the store: name, harness, source, commit, and when it was pulled.

```bash
hlock list [--json]
```

## `rm`

Remove pulled locks from the store, along with the isolated roots `launch` made for them.

```bash
hlock rm NAME...
```

## `push`

Publish the project's setup to a registry: what exists of `harness.toml`, `harness.lock` (with
the files it vendors), `apm.yml` (or `apm.yaml`), `apm.lock.yaml` and `README.md`, packed into
a deterministic `tar.gz`. Refuses values that look like credentials and hidden characters.
See [Sharing through a registry](../guides/registry.md#publish).

```bash
hlock push [NAMESPACE/NAME] [--version V] [-C DIR] [--dry-run] [--registry URL] [--json]
```

| Option | Default | Description |
|---|---|---|
| `NAMESPACE/NAME` | your handle / the project's name | The package to publish |
| `--version V` | `apm.yml`'s `version`, else `sha-<digest>` | The version; a published version can't change |
| `-C`, `--dir DIR` | `.` | The project to push |
| `--dry-run` | | List the files, size and digest; push nothing |
| `--registry URL` | `HLOCK_REGISTRY`, else the one you logged in to | The registry to push to |
| `--json` | | Print the package, version, digest, size, files and warnings as JSON |

## `search`

Search a registry for setups and APM packages.

```bash
hlock search [QUERY...] [--type setup|apm-package] [--kind harness|apm] [--source hosted|github|gitlab] [--limit N] [--registry URL] [--json]
```

| Option | Default | Description |
|---|---|---|
| `--type` | | Only setups, or only APM packages |
| `--kind` | | Only setups with hlock's files (`harness`) or APM's (`apm`) |
| `--source` | | Only packages pushed to the registry (`hosted`), or found in GitHub or GitLab repositories |
| `--limit N` | 20 | How many results |
| `--json` | | Print the registry's answer |

## `login`

Log in to a registry with its device login: open its page, confirm the code, and keep the
token in the OS keychain (else `~/.hlock/credentials.toml`, mode `0600`). The URL becomes your
default registry (`~/.hlock/config.toml`).

```bash
hlock login [URL] [--token TOKEN | --no-browser]
```

| Option | Default | Description |
|---|---|---|
| `URL` | `HLOCK_REGISTRY`, else the saved one | The registry |
| `--token TOKEN` | | Use a token made in the registry's settings instead; `-` reads it from stdin |
| `--no-browser` | | Print the link instead of opening it |

## `logout`

Forget a registry's token (keychain and credentials file).

```bash
hlock logout [--registry URL]
```

## `whoami`

Show the user, namespaces and token scopes the registry knows you by, and where the token
came from.

```bash
hlock whoami [--registry URL] [--json]
```

## `apm push`

Publish the APM package in a directory (`apm.yml` with `name` and `version`, and `.apm/`) to
a registry, as the zip `apm publish` builds: `apm.yml`, `.apm/`, and `README.md`,
`CHANGELOG.md` and `LICENSE` when present. Refuses values that look like credentials and
hidden characters. See [APM packages](../guides/registry.md#apm-packages).

```bash
hlock apm push [NAMESPACE/NAME] [-C DIR] [--dry-run] [--registry URL] [--json]
```

| Option | Default | Description |
|---|---|---|
| `NAMESPACE/NAME` | your handle / `apm.yml`'s `name` | The package to publish; the version is `apm.yml`'s `version` |
| `-C`, `--dir DIR` | `.` | The package to push |
| `--dry-run` | | List the files, size and digest; push nothing |
| `--registry URL` | `HLOCK_REGISTRY`, else the one you logged in to | The registry to push to |
| `--json` | | Print the package, version, digest, size, files, warnings and `install` spec as JSON |

## `apm pull`

Download an APM package from a registry, check it against its digest and for unsafe paths,
links and hidden characters, and write its files into a directory. Nothing in it runs.

```bash
hlock apm pull NAMESPACE/NAME[@VERSION] [--into DIR] [--force] [--allow-hidden FILE] [--registry URL] [--json]
```

| Option | Default | Description |
|---|---|---|
| `@VERSION` | the newest (highest semver, else the last published) | The version to pull |
| `--into DIR` | `./NAME` | Where to write the package |
| `--force` | | Replace files that already exist in DIR |
| `--allow-hidden FILE` | | Let through hidden characters in FILE (a path in the package); repeatable |
| `--registry URL` | `HLOCK_REGISTRY`, else the one you logged in to | The registry the package is in |
| `--json` | | Print the package, version, digest, directory and files as JSON |

## `show`

Print the locked stack (harness, models by role, decision model, backend) and the
evidence (success rate, cost per task, latency p50/p95, trials).

```bash
hlock show [--json]
```

| Option | Description |
|---|---|
| `--json` | Print the full lockfile as JSON |

## `diff`

Show what changed between two locks, including the text behind changed digests: each changed
skill, instruction file and config file as a unified diff, checked against its digest first.
See [Review lock changes](../guides/diff.md).

```bash
hlock diff [OLD] [NEW] [--lock FILE] [--stat] [--format text|markdown] [--json] [--fetch] [--ci]
```

OLD and NEW are lock files or git revisions. With neither, it compares `harness.lock.prev`
(or `HEAD`'s lock, without one) with `harness.lock`; with only OLD, OLD with `harness.lock`.

| Option | Description |
|---|---|
| `--lock FILE` | The lock to compare (default: the project's) |
| `--stat` | Only the summary and per-file line counts |
| `--format markdown` | For a pull request comment: a summary table, each diff in a `<details>` block |
| `--json` | Components, where their content came from, and hunks |
| `--fetch` | Fetch declared dependencies that aren't in the store |
| `--ci` | Exit 1 when a changed component's content can't be found |

## `tree`

Print the locked components as a tree.

```bash
hlock tree
```

## `harness list`

List every known harness (agent CLIs, autonomous agents, frameworks, and project harnesses
from `[harnesses.<id>]`) with the installed version and install source. `--json` marks
autonomous agents with `"autonomous": true`. `●` marks the harness
hlock is running inside. See [Harnesses](../guides/harnesses.md).

```bash
hlock harness list [--json]
```

## `harness show`

Show one harness profile and the paths it resolves to on this machine: config root, config
files, skill directories, and for code harnesses the entrypoint and package versions.

```bash
hlock harness show [NAME] [--json]
```

`NAME` is an id or alias and defaults to the target harness (`--harness` order, see
[Harnesses](../guides/harnesses.md#target-a-harness)).

## `runtime list`

List local inference runtimes (Ollama, LM Studio, llama.cpp, vLLM, SGLang, MLX LM, LocalAI,
Docker Model Runner, Jan), whether each is installed, its default port, and its model store.

```bash
hlock runtime list [--json]
```

## `backend status`

Show the configured backend, how hlock found it, what it says it runs (its answer to
`backend.describe`), and which `HL_*` variables and Flyte config it will use. API keys
are reported as set or unset, never printed. A backend that can't describe itself shows
why in `runs`.

```bash
hlock backend status [--backend NAME]
```

| Option | Description |
|---|---|
| `--backend NAME` | Show this backend instead of the manifest's `[backend] target` |

## `backend list`

List the backends you can select: the built-ins, the manifest's `[backends]`,
`hlock-backend-*` executables on `PATH`, and installed `hlock.backends` entry points. See
[Write your own backend](../guides/backends.md#write-your-own-backend).

```bash
hlock backend list
```

## `optimizer status`

Show tune's optimizer (`NAME`, else `[tune] optimizer`, else `hlock`), how hlock found it,
its `[optimizers.<name>] options`, and what a Python optimizer says it is. See
[Optimizers](../guides/optimizers.md).

```bash
hlock optimizer status [NAME]
```

## `optimizer list`

List the optimizers you can select: the built-ins, the manifest's `[optimizers]`,
`hlock-optimizer-*` executables on `PATH`, and installed `hlock.optimizers` entry points.

```bash
hlock optimizer list
```

## `run`

Load `harness.lock` and run the locked agent on `PROMPT`. Exits `0` when it succeeds and
`1` when it fails. What runs depends on the lock's harness:

| Harness | What `run` does |
|---|---|
| Agent CLI (Claude Code, Codex, Pi, …) | Prepares the same isolated environment as [`launch`](#launch), then runs the harness's non-interactive mode from its profile's `[launch] prompt` (`claude -p`, `codex exec`, `opencode run`, …). Success is the harness's exit code. `--json` prints `harness`, `success`, `exit_code`, `output`, `stderr`, `latency_s`, `notes` and `missing_secrets`. |
| [Code harness](../guides/harnesses.md#how-hlock-runs-a-code-harness) | Calls its entrypoint or command. `--json` prints `success`, `output`, `cost_usd`, `latency_s`, `exit_code` and the captured `log`. |
| `reference` | Runs the locked model on the offline reference provider. `--json` prints `success`, `model` and `cost_usd`. |

```bash
hlock run PROMPT [--trust] [--no-sandbox] [--json]
```

| Option | Description |
|---|---|
| `--trust` | Approve every executable the lock would run. Without it, an agent CLI starts without what isn't [trusted](../guides/audit.md#executables) here, and a code harness doesn't start |
| `--json` | Capture the outcome and print it as JSON |
| `--no-sandbox` | Run without the lock's [sandbox](../guides/sandbox.md) (also `HLOCK_NO_SANDBOX=1`) |

## `sandbox`

Show the lock's [sandbox](../guides/sandbox.md) (`docker`, `openshell`, or a runner you
define): its runner and whether it's installed and
trusted, the image, network, secrets, what runs inside, the profile, and whether
`harness.toml`'s `[sandbox]` or the evidence differ. Takes `--json`.

```bash
hlock sandbox [--json]
```

## `upgrade`

Find newer versions of locked components, re-measure the stack, and apply only on
request. See [Upgrading components](../guides/upgrade.md).

`upgrade` works on resolved locks. On a pinned lock it exits `1`: update your setup and
re-pin with `hlock pin` instead. `hlock upgrade dep:NAME` works on any lock: it moves a
declared dependency to the newest commit, tag or version its declaration allows, keeping
the previous lock as `harness.lock.prev` (see
[Keep them current](../guides/dependencies.md#keep-them-current)).

```bash
hlock upgrade [--all | --to TARGET] [--apply] [--force] [--rollback] [--workdir DIR]
hlock upgrade dep:NAME [--eval [-y]] [--apply]
```

| Option | Description |
|---|---|
| `--all` | Consider every upgradeable component |
| `--to TARGET` | Upgrade to a specific model/version |
| `--apply` | Perform the upgrade. Without it, only print the plan. |
| `--force` | Keep the upgrade even if the success rate drops |
| `--rollback` | Restore `harness.lock` from `harness.lock.prev` |
| `--workdir DIR` | Where trial artifacts go |
| `--eval` | With `dep:NAME`, measure the new version on the eval pack first, then ask before applying (`-y` applies without asking) |

## `tune`

Tune prompts, hyperparameters, and small models, and write the result to
`harness.tune.lock`. See [Tuning](../guides/tune.md).

```bash
hlock tune [--depth LEVEL] [--data SOURCE] [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--depth` | `auto` | `probe`, `prompt`, `hpo`, `ft`, `full`, or `auto` |
| `--data` | `user` | `user`, `user:<paths>`, `synthetic[:n]`, or `hub:<pack>` |
| `--harness NAME` | the lock's | Tune for this harness instead |
| `--out FILE` | `harness.tune.lock` | Output file |
| `--strict` | | Fail unless the result beats the lock's own configuration, measured on the same tasks |
| `--apply` | | Also adopt the result into `harness.lock`, as `tune apply` does |
| `--max-trials N` | | Cap the grid's trials in total, spread round-robin across the configurations |
| `--backend` | manifest `[backend] target` | Where the work runs: `offline`, `local`, `flyte`, or a [custom backend](../guides/backends.md#write-your-own-backend) |
| `--workdir DIR` | `.hlock` | Where trial artifacts go |
| `--optimizer NAME` | `[tune] optimizer` | What searches for edits: `hlock`, `harness2`, or an [optimizer of your own](../guides/optimizers.md) |
| `--edit KINDS` | `[tune] edit` | What the optimizer may change: `instructions`, `skill`, comma-separated |
| `--budget-rollouts N`, `--budget-usd X` | `[tune.budget]` | The optimizer's budget |
| `-d`, `--detach` | | Run in the background as a tune job; can't be combined with `--apply` |
| `--no-sandbox` | | Run code-harness trials and `tests` graders without `[sandbox]` (also `HLOCK_NO_SANDBOX=1`) |

The job commands below take a `JOB`: a job id or a unique prefix of one. Without it,
they use the newest job. See [Running in the background](../guides/tune.md#running-in-the-background).

### `tune eval`

Run a tune candidate's trials for an optimizer, graded, within its session's budget. See
[Evaluating a candidate](../guides/tune.md#evaluating-a-candidate).

```bash
hlock tune eval --session DIR CANDIDATE [--split train|dev] [--tasks IDS] [--trials N] [--json]
```

| Option | Description |
|---|---|
| `--session DIR` | The tune session, from `harness.optimize`'s request |
| `--split` | `train` (default) or `dev`; the test split is hlock's |
| `--tasks IDS` | Comma-separated task ids (default: the whole split) |
| `--trials N` | Trials per task (default: the session's) |
| `--json` | Each result with its grader feedback and workspace, and the budget |

### `tune apply`

Adopt a tune lock as `harness.lock`, keeping the old lock in `harness.lock.prev` (`hlock
upgrade --rollback` restores it). It refuses a tune lock for another harness than
`[target] harness`, and a file that isn't a tune lock.

```bash
hlock tune apply [FILE]
```

| Option | Default | Description |
|---|---|---|
| `FILE` | `harness.tune.lock` | The tune lock to adopt |

### `tune list`

List the project's tune jobs, newest first, with their status, depth, backend, start
time, elapsed time, and success rate.

```bash
hlock tune list [--json]
```

### `tune show`

Show a tune job's status, settings, trials run so far, Flyte run ids, error, result, and
the end of its log.

```bash
hlock tune show [JOB] [--log N] [--json]
```

| Option | Default | Description |
|---|---|---|
| `--log N` | `10` | How many log lines to show for a job that hasn't succeeded |
| `--json` | | Print the job record as JSON |

### `tune fetch`

Write a succeeded job's tune lock to `harness.tune.lock`. Fails if the job hasn't
succeeded.

```bash
hlock tune fetch [JOB] [--out FILE] [--apply] [--wait]
```

| Option | Default | Description |
|---|---|---|
| `--out FILE` | `harness.tune.lock` | Where to write the tune lock |
| `--apply` | | Also adopt the result into `harness.lock` |
| `--wait` | | Wait for the job to finish first |

### `tune cancel`

Stop a queued or running tune job and the worker processes it started.

```bash
hlock tune cancel JOB
```

## `migrate`

Move a lock onto another harness: skills, MCP servers, instructions and decision models go
where it reads them, and models move to a provider it supports. See
[Switching harnesses with migrate](../guides/migrate.md).

```bash
hlock migrate [LOCK] --to HARNESS [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `LOCK` | `harness.lock` | Lock to migrate |
| `--to` | required, except with `--local` | Harness to move to (`codex`, `opencode`, `claude-code`, …) |
| `-o`, `--out` | `harness.<to>.lock` beside the source; `harness.local.lock` with `--local` | Where to write the new lock |
| `--model` | kept when the harness supports its provider | `provider:model` for the default role |
| `--local` | | Also move the models to a local runtime, as [`localize`](#localize) does |
| `--runtime` | the best installed | With `--local`: the inference runtime |
| `--json` | | Print the migrate report as JSON |

## `localize`

Replace a lock's proprietary harness and API models with local equivalents, and write the
result to a new lock. See [Going local with localize](../guides/localize.md).

```bash
hlock localize [LOCK] [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `LOCK` | `harness.lock` | Lock to localize |
| `-o`, `--out` | `harness.local.lock` | Where to write the localized lock |
| `--harness` | the lock's, if local-capable, else the best installed | Local harness to use |
| `--runtime` | the best installed | `ollama`, `lmstudio`, `llama-cpp`, `mlx-lm`, `vllm`, … |
| `--models` | the runtime's catalog and weights on disk | Comma-separated model ids to consider |
| `--memory-gb` | measured | Memory budget for model weights |
| `--evals` | `harness.toml`'s eval packs | Eval pack to evaluate candidates on. `--tasks` is the old name. |
| `--strict` | | Drop remote MCP servers instead of flagging them |
| `--allow-network` | | Keep self-hosted runtimes on your network |
| `--json` | | Print the localize report as JSON |

## `meta`

Set up the [meta harness](../guides/meta.md), the agent hlock uses for its own work
(hlock's built-in agent by default, or an open-source harness you install), pinned in
`harness.meta.lock`, and run jobs in it. The meta files live next to the
project's lock.

| Command | Description |
|---|---|
| `hlock meta init` | Choose the meta harness and its model, write `harness.meta.toml`, install, and pin |
| `hlock meta pin` | Apply `[model]` to the meta harness's config and pin it into `harness.meta.lock` |
| `hlock meta check` | Fail if the meta harness drifted from `harness.meta.lock` or `harness.meta.toml` |
| `hlock meta show` | Show the meta lock and whether the API key variable is set |
| `hlock meta sync` | Rehydrate an installed meta harness's config (`--plan`, `--json`, `-y`, `--install`, `--pull`, `-v`) |
| `hlock meta launch [ARGS...]` | Start an interactive session in an installed meta harness |
| `hlock meta jobs` | List built-in and custom jobs (`--json`) |
| `hlock meta run JOB [INSTRUCTIONS...]` | Run a job in the meta harness, then verify it |
| `hlock meta verify JOB` | Run only a job's verification (`--out`, `--count`, `--apps`, `--json`) |

### `meta init`

```bash
hlock meta init [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--harness` | `builtin` | `builtin` (ships with hlock), or an installed open-source harness: `pi`, `codex`, `goose`, `opencode`, `hermes` |
| `--provider` | the first provider whose API key is set | An API provider: `anthropic`, `openai`, `google`, `openrouter` (an installed harness may support fewer) |
| `--model` | one per provider, or the best local model that fits | The model |
| `--local` | | Use a local runtime instead of an API |
| `--runtime` | the best installed | The local runtime; implies `--local` |
| `--memory-gb` | measured | Memory budget for local weights |
| `--base-url` | the provider's or runtime's | The model API's URL, such as an OpenAI-compatible gateway |
| `-y`, `--yes` | | Install the harness and download weights without asking |
| `--force` | | Start over: replace `harness.meta.toml` and rebuild the meta harness's config |

### `meta run`

```bash
hlock meta run JOB [INSTRUCTIONS...] [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--headless` / `--interactive` | `[run] mode`, else interactive | Run non-interactively, or as a session you watch |
| `--out DIR` | the manifest's first eval pack, else `./evals` | `bootstrap-evals`: the pack directory |
| `--count N` | `10` | `bootstrap-evals`: how many tasks to aim for |
| `-t`, `--template` | `coding` | `bootstrap-evals`: the template to scaffold when there's no pack yet (`hlock evals templates`) |
| `--pin` | | `update`: re-pin the project's lock when something changed |
| `--apps` | | `install`: let the agent write [desktop apps'](../guides/meta.md#desktop-apps) own config, with `hlock sync --apps` |
| `--allow-version-mismatch` | | Run even if the installed meta harness differs from its lock |
| `--dry-run` | | Print the command and the prompt, and run nothing |
| `--json` | | With `--dry-run`: print the run plan as JSON |

`meta run` and `meta verify` exit `0` when the job is verified, `1` when it isn't, and `3`
when only you can finish it.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Failure: drift (`check`), a failed task (`run`), a failed sync step, a missing or invalid file, or a tuning error |
| `2` | Invalid command-line usage |
| `3` | `sync` finished its automatic steps, but manual steps remain; or a `meta` job left steps only you can do |
