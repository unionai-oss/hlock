# Quickstart

This walkthrough takes about five minutes. It uses the built-in `reference` provider
and harness, which are deterministic and offline: no accounts, no API keys, and the
same results on every machine.

## 1. Get a task pack

A [task pack](../guides/task-packs.md) is the set of domain tasks every candidate stack
is measured against. Grab the demo pack from the repository:

```bash
mkdir hlock-demo && cd hlock-demo
git clone --depth 1 https://github.com/unionai-oss/hlock /tmp/hlock
cp -r /tmp/hlock/tasks ./tasks
```

It holds two tasks, `clean_csv` and `summarize_report`, each with a prompt and an
expected output artifact.

## 2. Write a manifest

```console
$ hlock init --name demo --pack ./tasks
✓ wrote harness.toml  (project 'demo', task pack ./tasks)
next → hlock resolve
```

`harness.toml` is the [manifest](../reference/manifest.md). It declares what the resolver
may try and what "best" means:

```toml title="harness.toml"
[project]
name = "demo"

[tasks]
packs = ["./tasks"]
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
target = "local"
```

1.  Each candidate runs every task this many times.
2.  Candidate models for the `planner` role, written `provider:model`.
3.  Among the stacks that meet the constraints, pick the cheapest one.

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
│  trials        2                         │
│                                          │
│  lock-version 1 · manifest e6471474cb15  │
│                                          │
╰──────────────────────────────────────────╯
next → hlock check  ·  hlock run "<prompt>"
```

The resolver ran every combination of models and decision models on the task pack, kept
the [Pareto front](../concepts/index.md#selection), applied your constraints, and chose
the cheapest stack that passed. Both 7B and 32B solved the demo tasks, so the cheaper
7B model won.

## 4. Verify and inspect

```console
$ hlock check
✓ harness.lock matches harness.toml  e6471474cb15

$ hlock tree
🔒 harness.lock
├── stack harness=reference
│   ├── llm
│   │   └── planner  reference:qwen-7b
│   ├── decision  none
│   └── harness  reference
└── provenance  backend=local
```

`hlock check` exits non-zero if `harness.toml` changes after the lock was written. That
makes it a [CI gate](../guides/ci.md). `hlock show` prints the same panel as `resolve`,
and `hlock show --json` prints the full lockfile.

## 5. Run the locked agent

```console
$ hlock run "clean ./data/orders.csv"
✓ success  model qwen-7b  cost $0.0001
```

`hlock run` loads `harness.lock` and runs the pinned stack. It exits `0` on success
and `1` on failure, and takes `--json` for scripting. In v0.1 it runs the locked model
on the offline reference provider.

## 6. Commit both files

```bash
git add harness.toml harness.lock
git commit -m "Lock the demo agent"
```

Like `uv.lock` or `Cargo.lock`, `harness.lock` belongs in version control. The lock
diff in a PR shows exactly what changed in the agent and what evidence supports it.

## Next steps

- Write tasks for your own domain: [Task packs](../guides/task-packs.md)
- Call real models: [Backends](../guides/backends.md)
- Move to newer models safely: [Upgrading components](../guides/upgrade.md)
- Tune prompts, hyperparameters, and small models: [Tuning](../guides/tune.md)
- Do it all from Python: [Python API](../reference/python-api.md)
