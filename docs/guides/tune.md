# Tuning

`hlock resolve` *chooses* among the components you list. `hlock tune` goes further
and *produces* improvements: tuned prompts, tuned hyperparameters, and, at the deepest
levels, fine-tuned small models. The result goes into a separate `harness.tune.lock`, so
your base lock stays untouched until you adopt it.

Tune starts from your `harness.lock`: its harness, models, skills, tools, instructions and
config stay as they are. It varies only what it tunes: retries, temperature, at `full` the
tool timeout, and the planner model when `[candidates.llm.planner]` lists more than one.
With no lock, it starts from the manifest's first candidates.

```console
$ hlock tune --depth hpo
✓ wrote harness.tune.lock
╭─ ⚙ tune ─────────────────────────────────────────────────────────────────────────────────────────╮
│                                                                                                  │
│  depth    hpo                                                                                    │
│  backend  offline                                                                                │
│  data     user-traces                                                                            │
│  grid     12 configurations                                                                      │
│  stages   data ✗ not on offline: use local, flyte or another backend  ·  prompt ✗ not on         │
│           offline: use local, flyte or another backend  ·  hpo ✓                                 │
│                                                                                                  │
│  Best configuration                                                                              │
│  model                 reference:qwen-7b                                                         │
│  retries               1                                                                         │
│  temperature           0.2                                                                       │
│  tool timeout seconds  120                                                                       │
│                                                                                                  │
│  success rate  100%  →  100%   +0%   (baseline: the lock's own configuration, measured)          │
│                                                                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
next → hlock tune apply
```

The baseline is your lock's own configuration, run on the same tasks and seeds before the
grid, not the number recorded in `harness.lock`, which may come from another split or
harness. When configurations tie, tune keeps the one closest to your lock: the fewest
retries, then your temperature, then your model. Here every configuration solved the demo
tasks, so it kept the lock's own `retries = 1` and `temperature = 0.2`.

`stages` lists what ran. On the `offline` backend only the HPO grid runs; the data, prompt
and fine-tune stages need `local`, `flyte`, or another [backend](backends.md) that runs
them. A stage that didn't run isn't recorded in the tune lock. When the prompt stage runs, its gain is
measured: the winner with the tuned prompt, minus the winner without it.

## Depth

`--depth` sets how far to go. Each level includes everything before it.

| Depth | Adds | Search grid |
|---|---|---|
| `probe` | Cheap policy probes, no retraining | models × retries `[0]` |
| `prompt` | System prompt and instructions search | models × retries `[0, 1]` |
| `hpo` | Hyperparameter search | models × retries `[0, 1, 3]` × temperature `[0.2, 0.7]` |
| `ft` | LoRA fine-tuning of a small model on your data | `hpo` grid |
| `full` | Holdout re-validation and a tool-timeout sweep | `ft` grid × tool timeout `[30, 120]` s |
| `auto` *(default)* | Currently the same as `hpo` | |

`ft` and `full` need the `local` or `flyte` [backend](backends.md) for real
training, and a GPU for anything beyond a tiny model. On `offline`, tune says so before it
runs, and runs only the grid. "models" is your lock's model, or the planner candidates
when there are several.

## Data

`--data` sets what tuning learns from:

| Value | Source |
|---|---|
| `user` *(default)* | Traces found in the current directory |
| `user:<paths>` | Comma-separated trace files or directories (CSV, JSONL, Parquet) |
| `synthetic[:n]` | `n` generated variants per task: failure seeding, tool-error recovery, prompt-injection attempts |
| `hub:<pack>` | A published trace pack *(roadmap)* |

Traces are normalized, redacted, split into train, validation, and test shards, and
content-hashed, so the tune lock records exactly what data produced it.

## Adopting the result

```console
$ hlock tune apply
✓ applied harness.tune.lock to harness.lock  (previous kept in harness.lock.prev)
next → hlock check  ·  hlock upgrade --rollback
```

When the prompt stage finds a prompt, the tune lock carries its text, not only its digest:
`tune` adds it to the end of the project instruction file the harness reads (the base lock's
own, else the harness's first one, such as `CLAUDE.md`; `AGENTS.md` for the reference harness)
and writes the result to `.hlock/tuned/<digest>/`. The lock's `[[instructions]]` entry for that
file points at it, with `source = "tune"`, so `sync` and `launch` place it like any vendored
instruction file. Commit `.hlock/tuned/` with the lock, and read the change with
[`hlock diff`](diff.md):

```console
$ hlock diff
harness.lock.prev → harness.lock

+ instructions AGENTS.md  (tune)
    AGENTS.md  added  +3 -0
    @@ -0,0 +1,3 @@
    +Be careful.
    +
    +Be concise and verify each step before continuing.
~ evidence split: holdout → search

1 instruction file changed
```

`hlock tune apply [FILE]` adopts a tune lock you already have, without running the search
again, and keeps the lock it replaces in `harness.lock.prev` (`hlock upgrade --rollback`
restores it). It refuses a tune lock for another harness than `[target] harness`. `hlock
tune --apply` tunes and applies in one step, the same way.

```console
$ hlock tune --depth hpo --strict
✗ error: tuning error: --strict: tuned success-rate 1.00 does not beat the base configuration's 1.00, measured on the same tasks
```

`--strict` makes `tune` safe to run on a schedule: it exits non-zero, and writes no tune
lock, unless the tuned stack beats the measured baseline.

When the lock is for an agent CLI, tune's trials run on a stand-in for it, as
[`resolve`'s do](resolve.md#resolve-for-your-own-harness), and tune says the gains are a
stand-in.

## Running in the background

A tune can take minutes for a prompt or hyperparameter search, and hours or days with
fine-tuning. `--detach` runs it as a background *tune job*, so you get your terminal
back and can close it:

```console
$ hlock tune --detach --depth hpo
✓ started tune job cdda29ff  (depth=hpo, pid 91638)
log .hlock/tune/jobs/cdda29ff/tune.log
next → hlock tune show cdda29ff  ·  hlock tune fetch cdda29ff --wait
```

Check on your jobs with `hlock tune list`:

```console
$ hlock tune list
job       status     depth  backend  started  elapsed  success rate
cdda29ff  succeeded  hpo    local    2s ago   0s       100%
```

`hlock tune show <job>` adds the settings, the trials run so far, any Flyte run ids,
the error if it failed, and the end of its log. When the job succeeds, `fetch` writes
its result to `harness.tune.lock`:

```console
$ hlock tune fetch
✓ wrote harness.tune.lock  (from tune job cdda29ff)
```

A job writes its result into its own directory and never touches your locks, so you can
run several at once. `fetch` takes the same `--out` and `--apply` as `tune` (`--apply` keeps
`harness.lock.prev` too), and
`--wait` blocks until the job finishes, which suits scripts and CI. If `harness.lock`
changed after the job started, `fetch` warns that the job tuned against the older one.

Every job command takes a job id, or any unique prefix of one, and defaults to the
newest job. `hlock tune cancel <job>` stops a running job and the worker processes it
started.

Jobs live in `.hlock/tune/jobs/<id>/`: `job.json` with the settings, status, and
report; `tune.log`; the resulting `harness.tune.lock`; and the trial artifacts in
`work/`, unless you passed `--workdir`.

The job runs on this machine on every backend. On `flyte`, only its stages run on
Flyte, so the machine has to stay on until the job finishes.

## Other options

| Option | Description |
|---|---|
| `--harness NAME` | Tune for this harness instead of the lock's |
| `--max-trials N` | Cap the grid's trials in total, spread round-robin across the configurations (the baseline isn't counted) |
| `--out FILE` | Write somewhere other than `harness.tune.lock` |
| `--backend` | Override the manifest's backend |
| `--workdir DIR` | Where trial artifacts go (default `.hlock`) |
| `-d`, `--detach` | Run as a [background job](#running-in-the-background) |

## Fine-tuning settings

LoRA fine-tuning (`--depth ft` and `full`) reads these environment variables:

| Variable | Default | Description |
|---|---|---|
| `HL_FT_BASE_MODEL` | `Qwen/Qwen2-0.5B` | Base model to fine-tune |
| `HL_FT_STEPS` | `3` | Training steps |
| `HL_GPU_COST_PER_H` | `2.0` | GPU price per hour in USD, used for cost evidence |
