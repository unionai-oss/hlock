# Python API

```python
import hlock          # identical to: import harness_lock
```

The Python API and the CLI are built on the same Rust bindings, so anything the CLI does
you can do from Python. Dict-shaped results (evidence, outcomes, reports) are returned
as JSON strings. Parse them with `json.loads`.

## Example

```python
import json
import hlock
from hlock import SearchSpace, Objective, Budget

space = SearchSpace(
    llm={"planner": ["reference:qwen-7b", "reference:qwen-32b"]},
    decision=["none", "rules:default"],
    harness=["reference"],
    objective=Objective(select="min-cost", constraints={"success-rate": ">=0.5"}),
    budget=Budget(max_trials=50),
    evals=json.dumps({"packs": ["./evals"], "seeds": 2}),
)

lock = hlock.resolve(space, backend="local")     # also writes ./harness.lock
print(lock.summary())
# stack="agent" harness=reference llm=qwen-7b decision=none success-rate=1.00 trials=2

evidence = json.loads(lock.evidence)
print(evidence["success_rate"]["mean"], evidence["cost_usd_per_task"]["mean"])

agent = hlock.load("harness.lock")
outcome = json.loads(agent.run("clean ./data/orders.csv"))
print(outcome)   # {'cost_usd': 0.0001, 'model': 'qwen-7b', 'success': True}
```

## Functions

### `resolve`

```python
resolve(manifest=None, backend=None, workdir=None, harness=None, lock=None) -> Lockfile
```

Run the resolver and write the lock.

| Parameter | Description |
|---|---|
| `manifest` | A `Manifest`, a `SearchSpace`, a path to a TOML file, or TOML text. Defaults to the nearest `harness.toml` at or above the working directory. |
| `backend` | `"local"`, `"devbox"`, or `"cluster"`. Overrides the manifest. |
| `workdir` | Where trial artifacts go. Defaults to `.harnesslock` in the project root (the manifest's directory). |
| `harness` | Target harness, overriding `[target] harness` |
| `lock` | Where to write the lock. Defaults to `harness.lock` in the project root. |

### `load`

```python
load(path) -> LockedAgent
```

Load a lockfile as a runnable agent.

### `upgrade`

```python
upgrade(
    manifest=None, to=None, all=False, apply=False,
    rollback=False, force=False, workdir=None, lock=None,
) -> str
```

Plan, apply, or roll back upgrades, like [`hlock upgrade`](cli.md#upgrade). Returns JSON:

```json
{
  "plan": {"upgradeable": [{"role": "planner", "from": "qwen-7b", "to": "qwen-32b"}]},
  "applied": false, "rolled_back": false,
  "before": 1.0, "after": null,
  "message": "plan: 1 upgrade(s) available — rerun with --apply to perform.",
  "lock_path": "/path/to/harness.lock"
}
```

### `tune`

```python
tune(
    manifest=None, depth="auto", data="user", harness=None, out=None,
    strict=False, apply=False, max_trials=None, backend=None, workdir=None, lock=None,
) -> str
```

Run tuning, like [`hlock tune`](cli.md#tune). Returns JSON with `path`
(the tune lock written), `summary`, and a `report` with `depth`, `backend`, `data_mode`,
`grid`, `best`, `baseline_success_rate`, and `gain_success_rate`.

### Tune jobs

`harness_lock.jobs` runs a tune in the background, like
[`hlock tune --detach`](../guides/tune.md#running-in-the-background). Job records are
plain dicts with `id`, `status` (`queued`, `running`, `succeeded`, `failed`, or
`cancelled`), timestamps, `options`, and, once the job succeeds, `report` and `result`
(the path of its tune lock).

```python
from harness_lock import jobs

job = jobs.submit("harness.toml", lock="harness.lock", options={"depth": "probe"})
jobs.wait(".", job["id"])                   # block until it finishes
jobs.fetch(".", job["id"], out="harness.tune.lock")
print([(j["id"], j["status"]) for j in jobs.list_jobs(".")])
# [('df4c53a6', 'succeeded')]
```

| Function | Description |
|---|---|
| `submit(manifest, *, lock, options, cwd=None)` | Start a tune job. `options` are `tune` keyword arguments (`depth`, `data`, `harness`, `strict`, `max_trials`, `backend`, `workdir`). |
| `list_jobs(root)` | The project's jobs, newest first. `root` is the manifest's directory. |
| `get(root, ref=None)` | A job by id or unique id prefix, or the newest job |
| `wait(root, ref, poll=2.0, timeout=None)` | Block until a job finishes |
| `fetch(root, ref, out, apply=False)` | Copy a succeeded job's tune lock to `out`, and to the base lock with `apply` |
| `cancel(root, ref)` | Stop a queued or running job |

They raise `jobs.JobError`, a `ValueError`, for a job that doesn't exist, is ambiguous,
or is in the wrong state.

### `check_eval_pack`

```python
check_eval_pack(path, run=False, manifest=None) -> str
```

Load the [eval pack](../guides/eval-packs.md) at `path` and check it, like
[`hlock evals check`](cli.md#evals). With `run`, also try its graders on each task's
fixtures and solution, like `--run`. `manifest` is the path of a `harness.toml` whose
`[evals.judge]` counts as the judge model. Returns JSON with `name`, `version`, `domain`,
`path`, `hash`, `tasks` (the task ids), and `issues`, each with `level` (`error` or
`warn`), `task` (or `None`), and `message`.

```python
>>> r = json.loads(hl.check_eval_pack("evals", run=True))
>>> r["tasks"], [i["level"] for i in r["issues"]]
(['hello_file', 'count_words', 'greet_people'], ['warn'])
```

### `grade_eval_task`

```python
grade_eval_task(path, task, workspace=None, manifest=None) -> str
```

Grade a copy of `workspace` as the output of `task` in the pack at `path`, like
[`hlock evals grade`](cli.md#evals-grade); with no `workspace`, the task's solution.
`manifest` is a `harness.toml` whose `[evals.judge]` scores judge graders. Returns JSON with
`task`, `workspace`, `success`, `score` (the mean), and `grades`, each with `grader`,
`success`, `score`, and `detail`.

```python
>>> r = json.loads(hl.grade_eval_task("evals", "greet_people", "run"))
>>> r["success"], round(r["score"], 2), [(g["grader"], g["success"]) for g in r["grades"]]
(False, 0.67, [('artifact:greetings.md', True), ('tests:greet_people/check.py', False)])
```

### Meta harness

The functions behind [`hlock meta`](cli.md#meta). Each returns JSON.

| Function | Returns |
|---|---|
| `meta_init(harness=None, provider=None, model=None, runtime=None, memory_gb=None, base_url=None, manifest=None)` | The chosen harness and model (`choice`) and the `harness.meta.toml` text to write (`manifest`) |
| `meta_pin(out="harness.meta.lock", meta_manifest="harness.meta.toml", vendor=True, force=False, manifest=None)` | The pinned meta lock, its `diff` from the previous one, `vendored` files, and `warnings` |
| `meta_check(path="harness.meta.lock", meta_manifest=None, manifest=None)` | A list of drift lines; empty when everything matches |
| `meta_config(meta_manifest=None)` | `target`, `model`, the default `mode`, and every job |
| `meta_prepare(job, lock="harness.lock", meta_lock="harness.meta.lock", meta_manifest=None, manifest=None, out=None, count=10, instructions="", guide=None, headless=None, allow_version_mismatch=False, dry_run=False)` | The run plan: `prompt`, `argv`, `env`, `mode`, `notes` |
| `meta_verify(job, lock="harness.lock", meta_lock="harness.meta.lock", meta_manifest=None, manifest=None, out=None, count=0)` | The verdict: `status` (`verified`, `failed`, `needs-you`, `unverified`), `summary`, `details`, `secrets`, `diff` |
| `agent_run(prompt, meta_lock="harness.meta.lock", interactive=False, say=None, ask=None, project=None)` | Run the built-in agent pinned in `meta_lock`. `say(kind, text)` gets its progress (`text`, `tool`, `result`, `denied`, `error`) and returns whether to keep going; `ask(command)` returns `y`, `n` or `a`. Returns the outcome: `status` (`done`, `max-steps`, `stopped`, `error`), `steps`, `summary`, token counts |

## Classes

### `SearchSpace`

A search space built in code instead of a `harness.toml`.

```python
SearchSpace(llm=None, decision=None, harness=None, policy=None, tools=None,
            skills=None, objective=None, budget=None, evals=None)
```

| Parameter | Type | Description |
|---|---|---|
| `llm` | `dict[str, list[str]]` | Candidate models per role, as `"provider:model"` |
| `decision` | `list[str]` | Candidate decision models |
| `harness` | `list[str]` | Candidate harnesses |
| `objective` | `Objective` | What "best" means |
| `budget` | `Budget` | How much to search |
| `evals` | `str` (JSON) | `{"packs": [...], "seeds": n}`. Defaults to `{"seeds": 2}`. `tasks` is the old name. |
| `policy`, `tools`, `skills` | `str` (JSON) | Matching manifest sections |

`SearchSpace.to_manifest(name)` returns the equivalent `Manifest`. A lock resolved from
a `SearchSpace` gets the stack name `"agent"`.

### `Objective`

```python
Objective(primary="success-rate", select="min-cost", constraints=None)
```

| Parameter | Description |
|---|---|
| `primary` | The metric being optimized |
| `select` | `"min-cost"` or `"max-quality"` |
| `constraints` | `dict[str, str]`, e.g. `{"success-rate": ">=0.8", "cost-usd-per-task": "<=0.05"}` |

### `Budget`

```python
Budget(max_spend_usd=None, max_trials=None, strategy="grid", random_seed=0)
```

### `Manifest`

| Member | Description |
|---|---|
| `Manifest.load(path)` | Parse a `harness.toml` |
| `Manifest.from_str(text)` | Parse TOML text |
| `.name` | Project name |
| `.backend` | Configured backend target |
| `.manifest_hash()` | `"sha256:…"` hash, as recorded in the lock |
| `.eval_packs()` | List of eval pack paths (`.task_packs()` is the old name) |
| `.to_json()` | The manifest as JSON |

### `Lockfile`

| Member | Description |
|---|---|
| `Lockfile.load(path)` | Read a `harness.lock` or `harness.tune.lock` |
| `.summary()` | One-line summary |
| `.harness` | Locked harness name |
| `.decision` | Locked decision model |
| `.llm` | JSON list of `[role, "provider:model"]` pairs |
| `.evidence` | JSON: `trials`, `success_rate`, `cost_usd_per_task`, `latency_s_per_task` |
| `.provenance` | The backend that produced the lock |
| `.manifest_hash` | The manifest hash the lock was resolved from, or `None` |
| `.to_json()` | The whole lockfile as JSON |

### `LockedAgent`

| Member | Description |
|---|---|
| `.summary()` | One-line summary of the locked stack |
| `.run(prompt)` | Run the locked stack. Returns JSON: `success`, `model`, `cost_usd`. In v0.1 this uses the offline reference provider. |

## Constants

| Name | Description |
|---|---|
| `__version__` | Package version |
| `LOCK_VERSION` | Lockfile schema version this release reads and writes (`1`) |

## Errors

Invalid input and failed operations raise `ValueError` with a message prefixed by the
error kind: `manifest error:`, `lockfile error:`, `eval pack error:`, `tuning error:`,
`python worker error:`, and so on.
