# Finding the best stack

A [pinned lock](pin.md) records the setup you already have. `hlock resolve` finds a better
one: you list the models, decision models and harnesses it may combine, and it measures
every combination against an [eval pack](eval-packs.md) of your own tasks, then locks the
one that wins, along with the evidence. It doesn't change prompts or model weights; that's
[`hlock tune`](tune.md).

This walkthrough uses the built-in `reference` provider and harness, which are
deterministic and offline: no accounts, no API keys, and the same results on every machine.
`hlock init` writes a manifest for them unless you pass `--harness`, including when you run
it inside an agent such as Claude Code.

## 1. Write an eval pack

Write the hello-world pack:

```console
$ mkdir hlock-demo && cd hlock-demo
$ hlock evals init
✓ wrote evals/pack.toml  (hello-world, 3 tasks)
  10 files under evals
next → hlock evals check ./evals  ·  hlock init --pack ./evals
```

It holds three small tasks, `hello_file`, `count_words`, and `greet_people`. Each has a
prompt, an expected output file, a [grader](eval-packs.md#grading) that checks what's in
it, and a solution. Check that it's ready:

```console
$ hlock evals check
hlock-demo/evals 0.1.0  3 tasks · evals
  ! 3 tasks leaves 1 for the holdout split; add tasks until there are at least 10
✓ ready  (1 warning)
next → hlock evals check ./evals --run  ·  hlock init --pack ./evals
```

The warning is expected for a demo. `--run` would also run each task's graders on its
solution, to show they pass on a correct result. For your own work, start from a
[template](eval-packs.md#fill-in-a-template-with-your-agent) and have your agent fill it
in.

## 2. Write a manifest

```console
$ hlock init --name demo
✓ wrote harness.toml  (project 'demo', eval pack ./evals)
next → hlock resolve
```

`harness.toml` is the [manifest](../reference/manifest.md). It declares what the resolver
may try and what "best" means:

```toml title="harness.toml"
[project]
name = "demo"

[evals]
packs = ["./evals"]
seeds = 2                     # (1)!

[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]   # (2)!

[candidates]
decision = ["none", "rules:default"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"           # (3)!

[objective.constraints]
success-rate = ">=0.5"

[budget]
strategy = "grid"
max-trials = 200

[backend]
target = "local"              # (4)!
```

1.  Each candidate runs every task this many times.
2.  Candidate models for the `planner` role, written `provider:model`.
3.  Among the stacks that meet the constraints, pick the cheapest one.
4.  Where trials run. See [Backends](backends.md) for real LLMs and clusters.

## 3. Resolve the lock

See what a resolve would run before it spends anything:

```console
$ hlock resolve --dry-run
4 stacks × 2 tasks × 2 seeds = 16 trials, then 2 holdout trials for the winner  · on reference
  reference/reference:qwen-7b/d=none
  reference/reference:qwen-7b/d=rules:default
  reference/reference:qwen-32b/d=none
  reference/reference:qwen-32b/d=rules:default
search   greet_people, hello_file
holdout  count_words
next → hlock resolve
```

Then resolve. A progress bar shows each trial as it runs, and notes (a budget cut, a
stack that got no trials) appear as they happen:

```console
$ hlock resolve
Resolving harness.toml · 4 stacks × 2 tasks × 2 seeds = 16 trials, then 2 holdout

✓ wrote harness.lock
stack                                            success  cost/task    p50  trials
reference/reference:qwen-7b/d=none            ★     100%    $0.0001  0.59s       4
reference/reference:qwen-7b/d=rules:default         100%    $0.0001  0.59s       4
reference/reference:qwen-32b/d=none                 100%    $0.0004  0.45s       4
reference/reference:qwen-32b/d=rules:default        100%    $0.0004  0.45s       4
holdout  reference/reference:qwen-7b/d=none  100% on 2 trials it wasn't picked on
╭─ 🔒 harness.lock ──────────────────────────────╮
│                                                │
│  Stack                                         │
│  harness        reference                      │
│  llm · planner  reference:qwen-7b              │
│  decision       none                           │
│  backend        local                          │
│                                                │
│  Evidence                                      │
│  success rate  100%                            │
│  cost / task   $0.0001                         │
│  latency       p50 0.56s  ·  p95 0.66s         │
│  trials        2  (holdout)  low sample (n=2)  │
│  eval pack     hlock-demo/evals@0.1.0          │
│                                                │
│  lock-version 1 · manifest 9864f18edb96        │
│                                                │
╰────────────────────────────────────────────────╯
report: .hlock/report.md
next → hlock check  ·  hlock run "<prompt>"
```

The resolver ran every combination of models and decision models on the search split,
kept the [Pareto front](../concepts/index.md#selection), applied your constraints, and
chose the cheapest stack that passed. Both 7B and 32B solved the demo tasks, so the
cheaper 7B model won (★).

Then it ran the winner on the holdout split, the tasks it wasn't picked on, and the lock's
evidence comes from that run. The more stacks a search compares, the more the best one's
search score is luck; the holdout score isn't. The search score is kept too, as
`success-rate.search`. With no holdout tasks, the evidence is the search score and
`resolve` says it's in-sample. Fewer than 10 trials is marked `low sample`: a demo pack
of three tasks says little.

`hlock resolve --json` prints the lock and every stack's scores, and
`.hlock/report.md` keeps the table.

### Repeat runs

Trials are cached in `.hlock/trials`, keyed on the stack, the task, the seed, the eval
packs' content, the backend, and the judge. A second resolve reuses them and says how
many it reused; `--no-cache` runs them all again. A [code
harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts)'s trials always run,
since hlock can't tell when your agent's code changed.

### Budgets

`[budget] max-trials` and `max-spend-usd` cap a search. Trials go round-robin across the
stacks, every task once before any second seed, so a small budget measures every stack
a little rather than some fully. `max-spend-usd` stops before the trial that would pass
the cap, judging by what a trial has cost so far. The holdout check counts toward
`max-spend-usd` but not `max-trials`.

## 4. Verify and inspect

```console
$ hlock check
✓ harness.lock matches harness.toml  9864f18edb96

$ hlock tree
🔒 harness.lock
├── stack harness=reference
│   ├── llm
│   │   └── planner  reference:qwen-7b
│   ├── decision  none
│   └── harness  reference
└── provenance  backend=local
```

On a resolved lock, `hlock check` exits non-zero if `harness.toml` changed after the lock
was written, which makes it a [CI gate](ci.md). `hlock show` prints the same panel as
`resolve`, and `hlock show --json` prints the full lockfile.

## 5. Run the locked agent

```console
$ hlock run "clean ./data/orders.csv"
✓ success  model qwen-7b  cost $0.0001
$ hlock run "count the words in words.txt"
✗ failed  model qwen-7b  cost $0.0001
```

`hlock run` loads `harness.lock` and runs the pinned stack. It exits `0` on success and
`1` on failure, and takes `--json` for scripting. Here it runs the locked model on the
offline reference provider, which succeeds as often as the model's simulated quality
(75% for `qwen-7b`) and answers the same prompt the same way every time. When the lock is for an agent CLI it runs that harness for
real, and for a [code harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts)
it runs your agent.

## Resolve for your own harness

Set `[target] harness` to resolve for the harness you use, and list the models it may
try: `hlock init --harness claude-code` takes them from a `harness.lock` that pins Claude
Code. `resolve` refuses an agent CLI paired with the offline `reference:` models, since it
can't run them. See [Harnesses](harnesses.md#resolve-for-a-target).

!!! warning "Agent CLIs are measured on a stand-in"
    In `resolve` trials, only [code harnesses](harnesses.md#your-own-agent-frameworks-and-custom-scripts)
    run for real. An agent CLI's trials run on the reference harness, so the lock records
    `simulated = true` in its evidence, and `resolve`, `show` and `check` say the evidence
    is a stand-in. To see the real agent on your tasks, use
    [`hlock evals run`](eval-packs.md#try-tasks-on-your-agent).

## Next

- Go below the stack, into prompts, hyperparameters, and small models: [Tuning](tune.md)
- Move to newer models with evidence: [Upgrading components](upgrade.md)
- Run trials with real LLMs or on a cluster: [Backends](backends.md)
- Enforce the lock in CI: [CI](ci.md)
