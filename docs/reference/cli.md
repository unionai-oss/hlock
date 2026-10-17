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
| [`init`](#init) | Write a starter `harness.toml` |
| [`resolve`](#resolve) | Search the space and pin the winner in `harness.lock` |
| [`pin`](#pin) | Pin your current harness setup into `harness.lock` |
| [`check`](#check) | Fail if `harness.toml` has drifted from `harness.lock`, or a pinned setup changed |
| [`sync`](#sync) | Rehydrate the setup in a lock on this machine, in a prefix, or in a Docker image |
| [`launch`](#launch) | Start a harness session that matches the lock |
| [`show`](#show) | Show the locked stack and its evidence |
| [`tree`](#tree) | Print the locked component graph |
| [`harness list`](#harness-list) | List known harnesses and what's installed |
| [`harness show`](#harness-show) | Show a harness profile and the paths it reads here |
| [`runtime list`](#runtime-list) | List local inference runtimes and what's installed |
| [`backend status`](#backend-status) | Show the configured backend and environment |
| [`run`](#run) | Run the locked agent on a prompt |
| [`upgrade`](#upgrade) | Plan, apply, or roll back component upgrades |
| [`tune`](#tune) | Tune prompts, hyperparameters, and small models into `harness.tune.lock` |
| [`localize`](#localize) | Write an all-local equivalent of a lock |

## Global options

| Option | Environment | Description |
|---|---|---|
| `-m`, `--manifest PATH` | `HLOCK_MANIFEST` | The `harness.toml` to use |
| `--lock PATH` | `HLOCK_LOCKFILE` | The lockfile to use |
| `-V`, `--version` | | Print the version |
| `-h`, `--help` | | Show help |

`-m` and `--lock` work before or after the command: `hlock -m agent/harness.toml resolve`
and `hlock resolve -m agent/harness.toml` are the same. The one exception is `launch`, where
everything after the command goes to the harness, so put them first.

**How hlock finds the project.** Without `-m`, hlock uses the nearest `harness.toml` in the
current directory or any parent, the way Cargo finds `Cargo.toml`, so commands work from
anywhere inside a project. The directory holding the manifest is the **project root**:

- relative paths in the manifest (`[tasks] packs`, instruction files) resolve against it;
- the lock defaults to `harness.lock` next to the manifest;
- `.harnesslock/` (trial artifacts), `.hlock/vendor/` and `.hlock/env/` live there.

Without a manifest (a project that only has a pinned lock), hlock uses the nearest
`harness.lock` instead. Paths you pass on the command line, like `-o` or `--tasks`, are
relative to where you run the command.

## `init`

Write a starter `harness.toml` that uses the reference provider.

```bash
hlock init [--name NAME] [--pack PATH] [--harness NAME] [--force]
```

| Option | Default | Description |
|---|---|---|
| `--name` | `demo` | Project name |
| `--pack` | `./tasks` | Task pack directory. Warns if it has no `pack.toml`. |
| `--harness` | the harness you're running inside | Write `[target] harness`. Without it, and outside a harness, `[target]` is left commented out. |
| `--force` | | Overwrite an existing `harness.toml` |

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
| `LOCK` | Lock to sync (default `harness.lock`) |
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
| `--lock FILE` | Lock to launch (default `harness.lock`) |
| `--prefix DIR` | Where isolated roots live (default `.hlock/env`) |
| `--in-place` | Use your own config instead of an isolated root (same harness only) |
| `--allow-version-mismatch` | Start even if the installed version differs from the lock |
| `--dry-run` | Prepare everything and print the command without starting it |
| `--json` | With `--dry-run`, print the launch plan as JSON |

Other arguments are passed to the harness. hlock's own options count anywhere before `--`.

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

Load `harness.lock` and run the locked stack on `PROMPT`. Exits `0` when the task
succeeds and `1` when it fails. For a [code harness](../guides/harnesses.md#how-hlock-runs-a-code-harness),
`run` calls its entrypoint or command; `--json` then prints `success`, `output`, `cost_usd`,
`latency_s`, `exit_code` and the captured `log`.

```bash
hlock run PROMPT [--json]
```

| Option | Description |
|---|---|
| `--json` | Print the raw outcome: `{"success": …, "model": …, "cost_usd": …}` |

!!! note "v0.1"

    For agent CLIs and the reference harness, `run` executes the locked model on the offline
    reference provider. To work in a locked agent CLI, use [`launch`](#launch).

## `upgrade`

Find newer versions of locked components, re-measure the stack, and apply only on
request. See [Upgrading components](../guides/upgrade.md).

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
| `--tasks` | `harness.toml`'s task packs | Task pack to evaluate candidates on |
| `--strict` | | Drop remote MCP servers instead of flagging them |
| `--allow-network` | | Keep self-hosted runtimes on your network |
| `--json` | | Print the localize report as JSON |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Failure: drift (`check`), a failed task (`run`), a failed sync step, a missing or invalid file, or a tuning error |
| `2` | Invalid command-line usage |
| `3` | `sync` finished its automatic steps, but manual steps remain |
