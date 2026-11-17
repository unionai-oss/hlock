# Upgrading components

New models come out all the time. `hlock upgrade` tells you which locked components
have newer versions available, re-measures the stack with them, and only changes your
lock when you ask. It always keeps the previous lock, so you can roll back.

`upgrade` works on a [resolved lock](resolve.md), because it re-measures against your
tasks. To update a [pinned lock](pin.md), change your setup (install a newer harness,
switch the model), then run `hlock pin` again.

## Plan

By default `upgrade` only plans:

```console
$ hlock upgrade --all
component  locked      available
planner    qwen-7b  →  qwen-32b
plan: 1 upgrade(s) available — rerun with --apply to perform.
next → hlock upgrade --all --apply
```

A locked [decision model](deciders.md) with a newer version is listed too, and `--apply`
re-resolves with it:

```console
$ hlock upgrade --all
component       locked                  available
planner         qwen-7b              →  qwen-32b
decider · stop  typesafe:jev-1.13.0  →  typesafe:jev-1.14.0
plan: 2 upgrade(s) available — rerun with --apply to perform.
next → hlock upgrade --all --apply
```

Upgrade one target instead of everything with `--to`:

```bash
hlock upgrade --to qwen-32b
```

## Apply

```console
$ hlock upgrade --all --apply
component  locked      available
planner    qwen-7b  →  qwen-32b
✓ upgraded  success rate 100% → 100%  (previous lock kept at harness.lock.prev)
```

Applying re-runs the eval pack on the upgraded stack and compares success rates. If
the success rate drops, the upgrade is rolled back automatically. Pass `--force` to
keep it anyway.

## Roll back

```console
$ hlock upgrade --rollback
✓ restored harness.lock from harness.lock.prev
```

## Move to a new model

Swapping the model name is rarely the whole job. Anthropic's
[Prompting Claude Fable 5.2](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5-2)
guide gives an order for moving a harness to a newer model ("Where to start"):

1. Set the reasoning effort explicitly, starting at `high`, and also measure `medium` and `xhigh`.
2. Review the system prompts, skills, agent files and memory you carried over, and remove
   text written for the earlier model.
3. Run your tasks and look at what happens.
4. Add changes one at a time, measuring each one's effect and token cost.

`hlock upgrade --to MODEL --retune` does the first three in one run, and writes the result
to `harness.tune.lock`. `harness.lock` isn't touched:

1. **Lint.** [`hlock lint`](lint.md) checks the lock's text for the new model: what the lock
   places, and the harness's own instruction file (`AGENTS.md`, `CLAUDE.md`) that trials read.
2. **Effort.** It sets `high` and sweeps `medium`, `high` and `xhigh` for models that take an
   effort (Claude after Claude 3, OpenAI's GPT-5 and o-series, and the offline reference). The
   levels in `[candidates.llm.<role>] effort`, if any, win. See
   [Reasoning effort](tune.md#reasoning-effort).
3. **Delete.** The [`ablate`](tune.md#delete-first-ablate) optimizer tries deleting each line
   lint flagged, on the new model, and keeps a deletion only if the tasks hold. It edits the
   instruction files and skills, or what `[tune] edit` lists.
4. **Measure.** It runs your lock as it is (old model, old text) and the migrated stack on
   the same tasks, and records both.

Step 4 of the guide, adding guidance, stays yours: apply the result, then
[tune](tune.md#tune-the-harness-text) with `optimizer = "hlock"` or your own optimizer.

On the default `offline` backend, the lint and the effort sweep run, but ablate doesn't, and
the offline reference ignores the effort, so every level ties:

```console
$ hlock upgrade --to anthropic:claude-fable-5-2 --retune
! only the HPO grid runs on the offline backend; the data, prompt and fine-tune stages need local, flyte or another backend
! lint: 3 lines in the lock's text to review for anthropic:claude-fable-5-2  (1 warning, 2 info: standing-delegation ×1, unscoped-absolute ×1, visible-reasoning ×1)
    AGENTS.md:2  Think step by step  visible-reasoning
    AGENTS.md:3  Always delegate  standing-delegation
    AGENTS.md:4  Always Check your result.  unscoped-absolute
! ablate didn't run on offline, so no line was tried: deleting them needs --backend local or flyte
✓ wrote harness.tune.lock  (harness.lock is unchanged)
╭─ ⚙ upgrade · planner reference:qwen-7b → anthropic:claude-fable-5-2 ─────────────────────────────╮
│                                                                                                  │
│  depth    prompt                                                                                 │
│  backend  offline                                                                                │
│  data     user-traces                                                                            │
│  grid     6 configurations                                                                       │
│  stages   data ✗ not on offline: use local, flyte or another backend  ·  optimize ✗ not on       │
│           offline: use local, flyte or another backend  ·  hpo ✓                                 │
│                                                                                                  │
│                   model                         effort         success    cost/task              │
│  lock             reference:qwen-7b             —                 100%      $0.0001              │
│  effort medium    anthropic:claude-fable-5-2    medium            100%      $0.0001              │
│  effort high      anthropic:claude-fable-5-2    high  (set)       100%      $0.0001              │
│  effort xhigh     anthropic:claude-fable-5-2    xhigh             100%      $0.0001              │
│  chosen           anthropic:claude-fable-5-2    high              100%      $0.0001              │
│                                                                                                  │
│  success rate  100%  →  100%   +0%   (lock on the old model → the migrated stack, same tasks)    │
│                                                                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
next → hlock diff harness.lock harness.tune.lock  ·  hlock tune apply
```

The real work needs a backend that calls the model: `--backend local` (with
`HL_LLM_BASE_URL` pointing at an endpoint that serves it; see [Backends](backends.md)) or
`flyte`. Below, the endpoint is a stand-in that passes a task only when the system prompt says
"Check your result", and charges more per answer at higher effort. All three levels pass,
so the cheapest wins; ablate deletes the two flagged lines the model doesn't need and keeps
the one it does:

```console
$ hlock upgrade --to anthropic:claude-fable-5-2 --retune --backend local
! eval pack ./evals has 1 problem(s); run `hlock evals check ./evals`
! --data user found no traces, so there's no data to tune on
! the pack holds out 3 tasks: a gain measured on fewer than 20 is unlikely to be real
! lint: 3 lines in the lock's text to review for anthropic:claude-fable-5-2  (1 warning, 2 info: standing-delegation ×1, unscoped-absolute ×1, visible-reasoning ×1)
    AGENTS.md:2  Think step by step  visible-reasoning
    AGENTS.md:3  Always delegate  standing-delegation
    AGENTS.md:4  Always Check your result.  unscoped-absolute
✓ wrote harness.tune.lock  (harness.lock is unchanged)
╭─ ⚙ upgrade · planner reference:qwen-7b → anthropic:claude-fable-5-2 ─────────────────────────────╮
│                                                                                                  │
│  depth      prompt                                                                               │
│  backend    local                                                                                │
│  data       user-traces                                                                          │
│  grid       6 configurations                                                                     │
│  stages     data ✗ 0 rows  ·  optimize ✓  ·  hpo ✓                                               │
│  optimizer  ablate on local  1 edit accepted  holdout: 100% → 100% on 3 tasks, p(worse) = 1.0,   │
│             text 121 → 38 bytes                                                                  │
│                                                                                                  │
│                   model                         effort         success    cost/task              │
│  lock             reference:qwen-7b             —                 100%      $0.0020              │
│  effort medium    anthropic:claude-fable-5-2    medium            100%      $0.0020              │
│  effort high      anthropic:claude-fable-5-2    high  (set)       100%      $0.0030              │
│  effort xhigh     anthropic:claude-fable-5-2    xhigh             100%      $0.0050              │
│  chosen           anthropic:claude-fable-5-2    medium            100%      $0.0020              │
│                                                                                                  │
│  Deleted 2 lines                                                                                 │
│  AGENTS.md:2  Think step by step and show your reasoning.  visible-reasoning                     │
│  AGENTS.md:3  Always delegate research to subagents.  standing-delegation                        │
│                                                                                                  │
│  success rate  100%  →  100%   +0%   (lock on the old model → the migrated stack, same tasks)    │
│                                                                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
next → hlock diff harness.lock harness.tune.lock  ·  hlock tune apply
```

The winner is the highest success rate, then the lowest cost per task, then the
configuration nearest the lock on the new model at `high`, so a tie between levels goes to
`high`. Cost is what the endpoint reports for each trial (`usage.cost`, as OpenRouter's
does); trials don't record token counts, so cost is the measure of token cost here. An
endpoint that reports no cost shows `$0.0000` for every level.

The tune lock records the evidence in `[tune.migrate]`, next to the usual
[`[tune.acceptance]`](tune.md#delete-first-ablate) for the deletions:

```toml title="harness.tune.lock"
[tune.migrate]
effort-set = "high"
efforts-from = "default"
from = "reference:qwen-7b"
role = "planner"
to = "anthropic:claude-fable-5-2"

[tune.migrate.after]
cost-usd-per-task = 0.002
effort = "medium"
model = "anthropic:claude-fable-5-2"
success-rate = 1.0
trials = 6

[tune.migrate.before]
cost-usd-per-task = 0.002
model = "reference:qwen-7b"
success-rate = 1.0
trials = 6

[[tune.migrate.deleted]]
component = "instructions/AGENTS.md"
line = 2
rule = "visible-reasoning"
text = "Think step by step and show your reasoning."

[[tune.migrate.deleted]]
component = "instructions/AGENTS.md"
line = 3
rule = "standing-delegation"
text = "Always delegate research to subagents."

[[tune.migrate.effort]]
cost-usd-per-task = 0.002
level = "medium"
success-rate = 1.0
trials = 6

[[tune.migrate.effort]]
cost-usd-per-task = 0.003
level = "high"
success-rate = 1.0
trials = 6

[[tune.migrate.effort]]
cost-usd-per-task = 0.005
level = "xhigh"
success-rate = 1.0
trials = 6

[tune.migrate.lint]
files = 1
info = 2
model = "anthropic:claude-fable-5-2"
warnings = 1

[tune.migrate.lint.rules]
standing-delegation = 1
unscoped-absolute = 1
visible-reasoning = 1
```

`hlock diff harness.lock harness.tune.lock` shows the new model, the effort
(`~ effort planner: unset → medium`) and the edited instruction file's text. Adopt it with
[`hlock tune apply`](tune.md#adopting-the-result), which keeps the old lock in
`harness.lock.prev`; `hlock upgrade --rollback` restores it.

Without `--to`, `--retune` moves to the model `hlock upgrade` would plan. A bare model
(`--to claude-fable-5-2`) keeps the lock's provider. `--backend`, `--out` and `--max-trials`
work as they do for `hlock tune`. From Python it's `hlock.tune(to=MODEL, …)`, where
`optimizer=` can pick another optimizer than ablate. `--retune` doesn't go with `--apply`:
it writes a tune lock for you to review.

What runs where:

| Step | `offline` | `local`, `flyte` |
|---|---|---|
| Lint for the new model | ✓ | ✓ |
| Effort sweep | ✓, but the reference ignores the level | ✓, sent as `reasoning_effort` |
| ablate (deleting lines) | ✗ | ✓ |
| Before/after on your tasks | ✓, on the reference model | ✓, on the models |

## Reviewing an upgrade

Because the lock is a committed file, an upgrade is a reviewable diff:

```bash
hlock upgrade --all --apply
git diff harness.lock     # model, fingerprint, and evidence changes side by side
hlock diff                # the same, against harness.lock.prev, with the evidence change
```
