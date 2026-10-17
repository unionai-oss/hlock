# Finding the best stack

A [pinned lock](pin.md) records the setup you already have. `hlock resolve` finds a better
one: you list the models, decision models and harnesses it may combine, and it measures
every combination against an [eval pack](eval-packs.md) of your own tasks, then locks the
one that wins, along with the evidence. It doesn't change prompts or model weights; that's
[`hlock tune`](tune.md).

This walkthrough uses the built-in `reference` provider and harness, which are
deterministic and offline: no accounts, no API keys, and the same results on every machine.

## 1. Write an eval pack

Write the hello-world pack:

```console
$ mkdir hlock-demo && cd hlock-demo
$ hlock evals init
✓ wrote evals/pack.toml  (hello-world, 3 tasks)
  6 files under evals
next → hlock evals check ./evals  ·  hlock init --pack ./evals
```

It holds three small tasks, `hello_file`, `count_words`, and `greet_people`, each with a
prompt and an expected output file. Check that it's ready:

```console
$ hlock evals check
hlock-demo/evals 0.1.0  3 tasks · evals
  ! 3 tasks leaves 1 for the holdout split; add tasks until there are at least 10
✓ ready  (1 warning)
next → hlock init --pack ./evals
```

The warning is expected for a demo. For your own work, start from a
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
max_trials = 200

[backend]
target = "local"              # (4)!
```

1.  Each candidate runs every task this many times.
2.  Candidate models for the `planner` role, written `provider:model`.
3.  Among the stacks that meet the constraints, pick the cheapest one.
4.  Where trials run. See [Backends](backends.md) for real LLMs and clusters.

## 3. Resolve the lock

```console
$ hlock resolve
✓ wrote harness.lock
╭─ 🔒 harness.lock ────────────────────────╮
│                                          │
│  Stack                                   │
│  harness        reference                │
│  llm · planner  reference:qwen-7b        │
│  decision       none                     │
│  backend        local                    │
│                                          │
│  Evidence                                │
│  success rate  100%                      │
│  cost / task   $0.0001                   │
│  latency       p50 0.26s  ·  p95 0.40s   │
│  trials        4                         │
│                                          │
│  lock-version 1 · manifest 2216eb06dca5  │
│                                          │
╰──────────────────────────────────────────╯
next → hlock check  ·  hlock run "<prompt>"
```

The resolver ran every combination of models and decision models on the eval pack, kept
the [Pareto front](../concepts/index.md#selection), applied your constraints, and chose
the cheapest stack that passed. Both 7B and 32B solved the demo tasks, so the cheaper
7B model won.

## 4. Verify and inspect

```console
$ hlock check
✓ harness.lock matches harness.toml  2216eb06dca5

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
$ hlock run "count the words in words.txt"
✓ success  model qwen-7b  cost $0.0001
```

`hlock run` loads `harness.lock` and runs the pinned stack. It exits `0` on success and
`1` on failure, and takes `--json` for scripting. For a
[code harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts) it runs your
agent; for agent CLIs and the reference harness it runs the locked model on the offline
reference provider.

## Resolve for your own harness

Set `[target] harness` to resolve for the harness you use, and list the models it may
try. See [Harnesses](harnesses.md#resolve-for-a-target).

## Next

- Go below the stack, into prompts, hyperparameters, and small models: [Tuning](tune.md)
- Move to newer models with evidence: [Upgrading components](upgrade.md)
- Run trials with real LLMs or on a cluster: [Backends](backends.md)
- Enforce the lock in CI: [CI](ci.md)
