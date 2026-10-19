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
| [`sync`](#sync) | Rehydrate the setup in a lock on this machine, in a prefix, or in a Docker image |
| [`launch`](#launch) | Start a harness session that matches the lock |
| [`pull`](#pull) | Pull someone else's lock from a git repository |
| [`list`](#list) | List the locks you've pulled |
| [`rm`](#rm) | Remove pulled locks |
| [`run`](#run) | Run the locked agent on a prompt |
| [`evals`](#evals) | Write a starter eval pack from a template, and check one |
| [`init`](#init) | Write a starter `harness.toml` |
| [`resolve`](#resolve) | Search the space and pin the winner in `harness.lock` |
| [`tune`](#tune) | Tune prompts, hyperparameters, and small models into `harness.tune.lock` |
| [`tune list`](#tune-list) | List the project's background tune jobs |
| [`tune show`](#tune-show) | Show a tune job's status, settings, and result |
| [`tune fetch`](#tune-fetch) | Write a finished tune job's result to `harness.tune.lock` |
| [`tune cancel`](#tune-cancel) | Stop a running tune job |
| [`upgrade`](#upgrade) | Plan, apply, or roll back component upgrades on a resolved lock |
| [`localize`](#localize) | Write an all-local equivalent of a lock |
| [`meta`](#meta) | Set up and run the meta harness, the agent hlock uses for its own work |
| [`harness list`](#harness-list) | List known harnesses and what's installed |
| [`harness show`](#harness-show) | Show a harness profile and the paths it reads here |
| [`runtime list`](#runtime-list) | List local inference runtimes and what's installed |
| [`show`](#show) | Show the locked stack and its evidence |
| [`tree`](#tree) | Print the locked component graph |
| [`backend status`](#backend-status) | Show the configured backend and environment |

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
- `.harnesslock/` (trial artifacts), `.hlock/vendor/` and `.hlock/env/` live there.

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

Write and check [eval packs](../guides/eval-packs.md).

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

Check that a pack is ready: required fields, prompts and fixtures present, no
`TODO(hlock)` left, and tasks that can fail. Exits `1` on errors; warnings don't fail.
Takes `--json`.

```bash
hlock evals check [DIRECTORY] [--json]
```

## `resolve`

Run the resolver over the manifest's search space, write `harness.lock`, and print the
chosen stack with its evidence.

```bash
hlock resolve [--backend local|devbox|cluster] [--harness NAME] [--workdir DIR]
```

| Option | Default | Description |
|---|---|---|
| `--backend` | manifest `[backend] target` | Where trials run |
| `--harness` | manifest `[target] harness` | Target harness for this run. See [Harnesses](../guides/harnesses.md#resolve-for-a-target). |
| `--workdir` | `.harnesslock` | Where trial artifacts go |

## `pin`

Capture the live setup of one harness (version, models, local runtimes, MCP servers, skills,
plugins, instruction files, config) into a pinned lock. Secrets become
`${HLOCK_SECRET_…}` placeholders. See [Pinning your setup](../guides/pin.md).

```bash
hlock pin [--harness NAME] [-o FILE] [--no-vendor] [--force] [--json]
```

| Option | Default | Description |
|---|---|---|
| `--harness` | `[target]`, else the harness you're running inside | Harness to pin |
| `-o`, `--out` | `harness.lock` | Where to write the lock |
| `--no-vendor` | | Record digests only; don't copy files into `.hlock/vendor/` |
| `--force` | | Replace a resolved lock (one with evidence) |
| `--json` | | Print the lock, the diff from the previous pin, vendored files, and warnings as JSON |

## `check`

Compare the manifest hash recorded in `harness.lock` with the hash of the current
`harness.toml`. Exits `0` when they match and `1` on drift, or when either file is
missing. See [Using hlock in CI](../guides/ci.md).

For a pinned lock (`kind = "pin"`), `check` needs no `harness.toml`: it re-captures the
harness and fails with a list of what changed.

For a resolved lock, `check` also fails when the lock is for a different harness than `[target] harness`, or
when the installed harness's version differs from the one the lock pins (skipped when the
harness isn't installed).

```bash
hlock check
```

## `sync`

Build a plan to reproduce a lock's setup, run its automatic steps, and list what needs you.
See [Rehydrating with sync](../guides/sync.md).

```bash
hlock sync [LOCK] [OPTIONS]
```

| Option | Description |
|---|---|
| `LOCK` | Lock to sync (default `harness.lock`): a path, a [pulled](#pull) lock's name, or a source to pull first |
| `--plan` | Print the plan and change nothing |
| `--json` | Print the plan as JSON and change nothing |
| `-y`, `--yes` | Don't ask before running automatic steps (required when stdin isn't a terminal) |
| `--install` | Let hlock run package managers and install scripts |
| `--pull` | Let hlock download model weights through installed runtimes |
| `--all-models` | Include models no locked role uses |
| `--prefix DIR` | Materialize into an isolated config root under `DIR` |
| `--docker` | Write a Docker build context to `.hlock/docker/<harness>/` |
| `--build` | With `--docker`, also run `docker build` |
| `--skill` | Write an Agent Skill that guides an agent through the remaining steps |
| `--skill-dir DIR` | Where `--skill` writes `SKILL.md` (default `.agents/skills/hlock-rehydrate`) |
| `--exact` | Also move aside skills that aren't in the lock |
| `-v`, `--verbose` | List every step, including optional ones |

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
| `--json` | With `--dry-run`, print the launch plan as JSON |

Other arguments are passed to the harness. hlock's own options count anywhere before `--`.

## `pull`

Fetch a lock and the files it vendors from a git repository into the store,
`~/.hlock/pulls/<name>/`, recording the source and commit in a `[pulled]` table. Use it from
any project by name. Nothing from the lock runs. With no source, refresh every pulled lock.
See [Pulling someone else's lock](../guides/pull.md).

```bash
hlock pull SOURCE [--as NAME | -o FILE] [--force] [--json]
hlock pull [NAME | LOCK ...] [--json]
```

| Option | Default | Description |
|---|---|---|
| `SOURCE` | | `github:OWNER/REPO[/PATH][@REF]`, `gitlab:GROUP/REPO[//PATH][@REF]`, `git:URL[//PATH][@REF]`, or a GitHub or GitLab URL |
| `NAME`, `LOCK` | every pulled lock | Pulled locks to refresh from their recorded source: names in the store, or lock files pulled with `-o` |
| `--as NAME` | the repository owner | Store the lock as `NAME` |
| `-o`, `--out FILE` | | Write the lock into the project as `FILE` (its vendored files go in `.hlock/vendor/<name>/`) instead of the store |
| `--force` | | Replace a lock of that name that wasn't pulled from the same repository |
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

## `show`

Print the locked stack (harness, models by role, decision model, backend) and the
evidence (success rate, cost per task, latency p50/p95, trials).

```bash
hlock show [--json]
```

| Option | Description |
|---|---|
| `--json` | Print the full lockfile as JSON |

## `tree`

Print the locked components as a tree.

```bash
hlock tree
```

## `harness list`

List every known harness (agent CLIs, frameworks, and project harnesses from
`[harnesses.<id>]`) with the installed version and install source. `●` marks the harness
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

Show the configured backend, and which `HL_*` variables and Flyte config the worker
will use. API keys are reported as set or unset, never printed.

```bash
hlock backend status
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
hlock run PROMPT [--json]
```

| Option | Description |
|---|---|
| `--json` | Capture the outcome and print it as JSON |

## `upgrade`

Find newer versions of locked components, re-measure the stack, and apply only on
request. See [Upgrading components](../guides/upgrade.md).

`upgrade` works on resolved locks. On a pinned lock it exits `1`: update your setup and
re-pin with `hlock pin` instead.

```bash
hlock upgrade [--all | --to TARGET] [--apply] [--force] [--rollback] [--workdir DIR]
```

| Option | Description |
|---|---|
| `--all` | Consider every upgradeable component |
| `--to TARGET` | Upgrade to a specific model/version |
| `--apply` | Perform the upgrade. Without it, only print the plan. |
| `--force` | Keep the upgrade even if the success rate drops |
| `--rollback` | Restore `harness.lock` from `harness.lock.prev` |
| `--workdir DIR` | Where trial artifacts go |

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
| `--harness NAME` | | Restrict to one harness |
| `--out FILE` | `harness.tune.lock` | Output file |
| `--strict` | | Fail unless the result beats the base lock |
| `--apply` | | Also adopt the result into `harness.lock` |
| `--max-trials N` | | Cap the number of trials |
| `--backend` | manifest `[backend] target` | `local`, `devbox`, or `cluster` |
| `--workdir DIR` | `.harnesslock` | Where trial artifacts go |
| `-d`, `--detach` | | Run in the background as a tune job; can't be combined with `--apply` |

The job commands below take a `JOB`: a job id or a unique prefix of one. Without it,
they use the newest job. See [Running in the background](../guides/tune.md#running-in-the-background).

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
| `hlock meta verify JOB` | Run only a job's verification (`--out`, `--count`, `--json`) |

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
