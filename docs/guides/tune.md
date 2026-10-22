# Tuning

`hlock resolve` *chooses* among the components you list. `hlock tune` goes further
and *produces* improvements: edited instruction files and skills, tuned hyperparameters,
and, at the deepest levels, fine-tuned small models. The result goes into a separate `harness.tune.lock`, so
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
│  stages   data ✗ not on offline: use local, flyte or another backend  ·  optimize ✗ not on       │
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

`stages` lists what ran. On the `offline` backend only the HPO grid runs; the data, optimize
and fine-tune stages need `local`, `flyte`, or another [backend](backends.md) that runs
them. A stage that didn't run isn't recorded in the tune lock.

## Depth

`--depth` sets how far to go. Each level includes everything before it.

| Depth | Adds | Search grid |
|---|---|---|
| `probe` | Cheap policy probes, no retraining | models × retries `[0]` |
| `prompt` | An [optimizer](#tune-the-harness-text) edits the instruction files | models × retries `[0, 1]` |
| `hpo` | Hyperparameter search | models × retries `[0, 1, 3]` × temperature `[0.2, 0.7]` |
| `ft` | LoRA fine-tuning of a small model on your data | `hpo` grid |
| `full` | Holdout re-validation and a tool-timeout sweep | `ft` grid × tool timeout `[30, 120]` s |
| `auto` *(default)* | Currently the same as `hpo` | |

`ft` and `full` need the `local` or `flyte` [backend](backends.md) for real
training, and a GPU for anything beyond a tiny model. On `offline`, tune says so before it
runs, and runs only the grid. "models" is your lock's model, or the planner candidates
when there are several.

## Tune the harness text

From `--depth prompt` on, or whenever `[tune] edit` is set, an **optimizer** edits the
lock's instruction files (and with `edit = ["instructions", "skill"]`, its skills), and hlock
keeps the edits only if they beat the lock on tasks the optimizer never saw:

```toml title="harness.toml"
[tune]
edit = ["instructions"]          # what it may change: instructions, skill, subagent, script, middleware
optimizer = "hlock"              # the built-in one, or a backend that runs harness.optimize
proposer = "anthropic:opus"      # the model that drafts edits (optional for the built-in one)
trials = 1                       # trials per task, in each evaluation
alpha = 0.2                      # the significance level edits must reach
budget = { rollouts = 100 }      # the most trials the optimizer may run (default 100); usd too
```

```console
$ hlock tune --backend local
! the pack holds out 4 tasks: a gain measured on fewer than 20 is unlikely to be real
✓ wrote harness.tune.lock
╭─ ⚙ tune ─────────────────────────────────────────────────────────────────────────────────╮
│                                                                                          │
│  depth      hpo                                                                          │
│  backend    local                                                                        │
│  data       synthetic                                                                    │
│  grid       6 configurations                                                             │
│  stages     data ✓  ·  optimize ✓  ·  hpo ✓                                              │
│  optimizer  hlock  1 edit accepted  holdout: 0% → 100% on 4 tasks, p = 0.0625            │
│                                                                                          │
│  Best configuration                                                                      │
│  model                 reference:qwen-7b                                                 │
│  retries               1                                                                 │
│  temperature           0.2                                                               │
│  tool timeout seconds  120                                                               │
│                                                                                          │
│  success rate  0%  →  100%   +100%   (baseline: the lock's own configuration, measured)  │
│                                                                                          │
╰──────────────────────────────────────────────────────────────────────────────────────────╯
next → hlock tune apply
```

How it works:

1. **The bundle.** Tune writes the lock's instruction files (with their roles), skills,
   subagents, scripts and middleware as files, with a `bundle.json` that says which the optimizer may edit, and size limits (15 KiB per
   component, 120 changed lines). The harness's own instruction file (`CLAUDE.md`, `AGENTS.md`)
   is always there, empty when the lock has none. A declared dependency isn't editable: change
   it at its source.
2. **The splits.** The pack's search tasks are split into train and dev for the optimizer.
   The holdout tasks are the test split, which only hlock runs. Without a holdout, hlock
   checks the edits on dev and says so.
3. **The noise band.** Before the search, hlock runs the lock's own text on dev, once per
   trial seed, and records the spread.
4. **The search.** The optimizer proposes candidates and evaluates them through
   [`hlock tune eval`](#evaluating-a-candidate), inside the budget.
5. **The checks.** hlock drops any edit that copies text from the eval pack (task ids, the
   strings graders check for, fixture file names), then runs the lock's text and the candidate
   on the test split, paired. It accepts the edits only when the candidate's success rate
   beats the lock's by more than the noise band, a sign test over the tasks gives p ≤ `alpha`
   (default 0.05), and cost per trial rose no more than `max-cost-increase` (default 25%).
6. **The result.** Accepted edits are written under `.hlock/tuned/<digest>/`, the tune lock's
   `[[instructions]]` and `[[skill]]` entries point at them, and the grid that follows runs
   with them. Rejected or not, `[tune.optimizer]` and `[tune.acceptance]` record what ran and
   why.

A small pack rarely gets past the sign test: with 4 held-out tasks, even 4 wins and no losses
give p = 0.0625. The example above sets `alpha = 0.2` for that reason. A real gain needs
enough held-out tasks to show it.

```toml title="harness.tune.lock"
[tune.acceptance]
accepted = true
noise-band = 0.0
runs = 8

[tune.acceptance.cost-usd-per-trial]
base = 0.0
candidate = 0.0

[tune.acceptance.test]
base = 0.0
better = 4
candidate = 1.0
p = 0.0625
split = "holdout"
tasks = 4
trials = 1
worse = 0

[[tune.edit]]
after = "sha256:c0a118cc4aa915e05467ff49e0be0e8b281fa705b6fcfe730e71a9834bd9146c"
component = "instructions/AGENTS.md"
kind = "instructions"
op = "add"
path = ".hlock/tuned/c0a118cc/AGENTS.md"
rationale = """built-in guidance, tried after: artifact:answer.md: answer.md doesn't contain "OK""""
```

After `hlock tune apply`, [`hlock diff`](diff.md) shows the edit's text:

```console
$ hlock diff
harness.lock.prev → harness.lock

+ instructions AGENTS.md  (tune)
    AGENTS.md  added  +3 -0
    @@ -0,0 +1,3 @@
    +Be careful.
    +
    +Check your result against what the task asks for before you finish.
~ evidence success-rate: 0.00 → 1.00  (n=12 → 24)
~ evidence split: holdout → search

1 instruction file changed
```

Commit `.hlock/tuned/` with the lock.

### The built-in optimizer

`optimizer = "hlock"` runs on the trial backend's worker (`local`, or `flyte`, where the search
runs on your machine and its trials on the cluster). It runs the lock's text on the train
split, picks an editable component in turn, and asks the `proposer` model for a few lines of
guidance from the failures and the graders' feedback. It reruns the same tasks, and keeps the
edit when it does better there and no worse on dev. The lines it may add per step decay from
3 to 1. Without a `proposer` (or without `HL_LLM_BASE_URL`), it tries a fixed list of general
guidance instead. It's small on purpose: a better optimizer is a [backend](backends.md#write-an-optimizer).

### Harness²

[Harness²](https://github.com/google-research/harness2) (Google Cloud AI Research and
Northwestern) refines a harness without labels: for each task it proposes edits, probes them
on that task, composes what helped, and keeps lessons for the tasks after it. hlock runs it as
a black box, in the environment Harness² is installed in, with its own proposer, selector,
library, and OpenCode or Codex executor:

```toml title="harness.toml"
[tune]
optimizer = "harness2"
edit = ["instructions", "skill", "subagent", "script", "middleware"]

[backends.harness2]
options = { python = "~/src/harness2/.venv/bin/python", k = 1, mode = "parallel" }
```

```console
$ hlock backend status --backend harness2
backend    harness2  (from --backend)
found      looked up as 'harness2' in the hlock.backends entry points
runs       harness.optimize
about      Harness² (google-research/harness2), run as a black box in its own environment
```

Install Harness² and its executor as its README says (`uv pip install -e .`, then
`harness2-setup`), and set its models the way it reads them (`HARNESS2_SOLVER_MODEL`,
`HARNESS2_IMPROVER_MODEL`, or its `.env`). `python` (or `HARNESS2_PYTHON`) is that
environment's interpreter; Harness² needs Python 3.12 or later, and hlock doesn't import it.

hlock hands Harness² the lock's [components](pin.md#harness-components) as its eight-part
tree: the instruction file is the system prompt; guardrails, memory and tool guidance come
from their role files or sections; skills, subagents (OpenCode), scripts (in Harness²'s
`scripts/`, mapped back to `.agents/scripts/`) and middleware (OpenCode plugin files, Codex's
hooks file) go where Harness² keeps them. A part Harness² has no place for, such as a second
rules file, is left as it is. Harness² runs over the train and dev tasks; each task's adopted
harness is a candidate, and when they differ hlock runs them on dev and keeps the best.
Harness² never sees the graders: it refines without labels, and hlock's
[checks](#tune-the-harness-text) decide what's accepted, on the held-out tasks, as for any
optimizer. The tune lock records `evaluator = "optimizer"`, since Harness² ran its own
rollouts, and each `[[tune.edit]]`'s `rationale` is the lesson Harness² recorded.

| Option | Default | |
|---|---|---|
| `python` | `HARNESS2_PYTHON` | The interpreter Harness² is installed in |
| `substrate` | `cx` for a Codex lock, else `oc` | Harness²'s executor: OpenCode or Codex |
| `k` | `1` | Its task-level recursion budget |
| `mode` | `parallel` | `parallel` or `sequential` recursion |
| `timeout` | `1800` | Seconds per rollout |
| `work` | | A description of the work, for Harness²'s prompts |

### What a trial sees

On the `local` and `flyte` backends a trial is one completion, and the candidate's instruction
files reach the model as its system prompt. Skills reach only a
[code harness](harnesses.md)'s trials, which get the candidate's skills in `HLOCK_SKILLS_DIR`
and its instruction files in `HLOCK_INSTRUCTIONS_DIR`. After an optimize stage, the baseline
and every grid configuration run with the lock's text, or the accepted edits, the same way.

### Evaluating a candidate

`hlock tune eval` is the optimizer's way to run trials: a candidate bundle, on the train or
dev split, graded by hlock, counted against the budget, and cached.

```console
$ hlock tune eval --session .hlock/tune/7d6d9a18 .hlock/tune/7d6d9a18/final --split dev
✓ ran 2 trials on dev: 2 passed  candidate 3c2071b983e1
  ✓ question_11 #0  passed
  ✓ question_4 #0  passed
budget: 37 rollouts used, 63 left
```

It refuses the test split, a task that isn't in the split, a candidate that changes what
`bundle.json` doesn't allow, and new trials once the budget is spent. `--json` prints each
result with the grader's feedback and the trial's workspace, for a reflective optimizer to
read.

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
