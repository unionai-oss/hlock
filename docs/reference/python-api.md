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

lock = hlock.resolve(space, backend="offline")   # also writes ./harness.lock
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
resolve(manifest=None, backend=None, workdir=None, harness=None, lock=None, on_event=None, no_cache=False) -> Lockfile
```

Run the resolver, check the winner on the holdout split, and write the lock.

| Parameter | Description |
|---|---|
| `manifest` | A `Manifest`, a `SearchSpace`, a path to a TOML file, or TOML text. Defaults to the nearest `harness.toml` at or above the working directory. |
| `backend` | `"offline"`, `"local"`, `"flyte"`, or a [custom backend](../guides/backends.md#write-your-own-backend). Overrides the manifest. |
| `workdir` | Where trial artifacts go. Defaults to `.hlock` in the project root (the manifest's directory). |
| `harness` | Target harness, overriding `[target] harness` |
| `lock` | Where to write the lock. Defaults to `harness.lock` in the project root. |
| `on_event` | Called with each progress event, a JSON string with a `kind`: `plan` (the trial counts), `trial` (one trial done, with `done`, `total`, `stack`, `task`, `seed`, `success`, `cached`, and `split` for the holdout), `note` (a warning, in `text`), and `done` (every stack's scores, the `holdout` check, and how many trials were `cached`) |
| `no_cache` | Run every trial again instead of reusing ones an earlier resolve ran |

### `resolve_plan`

```python
resolve_plan(manifest=None, backend=None, workdir=None, harness=None) -> str
```

What `resolve` would run, like `hlock resolve --dry-run`, without running anything.
Returns JSON with `stacks` (their labels), `tasks` and `holdout-tasks` (ids), `seeds`,
`trials` (after `max-trials`), `all-trials`, `holdout-trials`, `max-trials`,
`max-spend-usd`, `dropped-by-policy`, `adapter`, and `notes`. Raises the errors `resolve`
would raise before its first trial.

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

### `backend_describe`

```python
backend_describe(manifest=None, backend=None, workdir=None) -> str
```

The backend `backend` names (else the manifest's target), like
[`hlock backend status`](cli.md#backend-status). Returns JSON with `name`,
`renamed_from` (the old name it was selected by, or `null`), `source` (how it was found),
`options`, and either `describe` (its answer: `name`, `kinds`, `model_endpoint`, `about`)
or `error`. `offline` has neither, since it runs in process.

```python
>>> hlock.backend_describe(hlock.Manifest.from_str(""), backend="local")
'{"describe":{"about":"hlock\'s stock handlers, in process on this machine","kinds":["llm.run_trial","data.load","data.synthetic","grade","prompt.search","harness.optimize","ft.lora"],"model_endpoint":true,"name":"local"},"name":"local","options":{},"renamed_from":null,"source":"built in, Python worker"}'
```

### `optimizer_describe`

```python
optimizer_describe(manifest=None, optimizer=None) -> str
```

The optimizer `optimizer` names (else the manifest's `[tune] optimizer`, else `hlock`), like
[`hlock optimizer status`](cli.md#optimizer-status). Returns JSON with `name`, `source` (how
it was found), `options`, and the `object` or `command` that runs it (`null` for a built-in
or an entry point).

```python
>>> hlock.optimizer_describe(hlock.Manifest.from_str("[optimizers.harness2]\noptions = { k = 2 }\n"), optimizer="harness2")
'{"command":null,"name":"harness2","object":null,"options":{"k":2},"source":"built in"}'
```

### `tune`

```python
tune(
    manifest=None, depth="auto", data="user", harness=None, out=None,
    strict=False, apply=False, max_trials=None, backend=None, workdir=None, lock=None,
    optimizer=None, edit=None, budget_rollouts=None, budget_usd=None,
) -> str
```

Run tuning, like [`hlock tune`](cli.md#tune). Returns JSON with `path`
(the tune lock written), `summary`, and a `report` with `depth`, `backend`, `harness`,
`data_mode`, `grid`, `trials`, `stages` (each with `stage`, whether it `ran`, and a
`detail`), `best`, `baseline` (the lock's own configuration, measured, with its
`success_rate` and `trials`), `baseline_success_rate`, `gain_success_rate`, `simulated`,
`notes`, `applied`, `previous` (the `.prev` path when `apply` replaced a lock), and
`optimize` (when the optimize stage ran: `optimizer`, `acceptance`, the `edits` written, and
how many components it `proposed` changing). `optimizer`, `edit` (a list of kinds),
`budget_rollouts` and `budget_usd` override `[tune]`.

### `tune_eval`

```python
tune_eval(session, candidate, split="train", tasks=None, trials=None) -> str
```

Run a tune candidate's trials, like [`hlock tune eval`](cli.md#tune-eval). Returns JSON with
`candidate` (its digest), `split`, `results` (each with `task`, `trial`, `success`, `score`,
`feedback`, `workspace`, `cost_usd`, `latency_s`, `cached`), and `budget`. An optimizer
written in Python uses [`hlock.optimize.Session`](../guides/optimizers.md#write-an-optimizer),
which wraps it and raises `BudgetExhausted` when the budget is spent.

### `lock_diff`

```python
lock_diff(lock="harness.lock", old=None, new=None, fetch=False) -> str
```

Compare two locks, like [`hlock diff`](cli.md#diff). `old` and `new` are lock files or git
revisions. Returns JSON with `old`, `new`, `summary`, `components` (each with `kind`,
`name`, `change`, where its content came from, and its `files`' hunks, or why it's
`unavailable`), `evidence`, and `unresolved`.

### `tune_apply`

```python
tune_apply(path="harness.tune.lock", lock="harness.lock", manifest=None) -> str | None
```

Adopt a tune lock, like [`hlock tune apply`](cli.md#tune-apply): copy `lock` to
`<lock>.prev`, then write the tune lock there. With `manifest`, refuses a tune lock for
another harness than its `[target] harness`. Returns the `.prev` path, or `None` when there
was no lock.

### Tune jobs

`hlock.jobs` runs a tune in the background, like
[`hlock tune --detach`](../guides/tune.md#running-in-the-background). Job records are
plain dicts with `id`, `status` (`queued`, `running`, `succeeded`, `failed`, or
`cancelled`), timestamps, `options`, and, once the job succeeds, `report` and `result`
(the path of its tune lock).

```python
from hlock import jobs

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
check_eval_pack(path, run=False, manifest=None, judges=False) -> str
```

Load the [eval pack](../guides/eval-packs.md) at `path` and check it, like
[`hlock evals check`](cli.md#evals). With `run`, also try its graders on each task's
fixtures and solution, like `--run` (decision-model judges included, and LLM judges with
`judges`). `manifest` is the path of a `harness.toml` whose judge settings apply. Returns
JSON with `name`, `version`, `domain`, `path`, `hash` (a digest of every file in the pack), `tasks` (the task ids), `judge`
(its `model` and whether it's a `decider`, or `None`), and `issues`, each with `level`
(`error` or `warn`), `task` (or `None`), and `message`.

```python
>>> r = json.loads(hl.check_eval_pack("evals", run=True))
>>> r["tasks"], [i["level"] for i in r["issues"]]
(['hello_file', 'count_words', 'greet_people'], ['warn'])
```

### `grade_eval_task`

```python
grade_eval_task(path, task, workspace=None, manifest=None, lock=None, no_sandbox=False, trust=False) -> str
```

Grade a copy of `workspace` as the output of `task` in the pack at `path`, like
[`hlock evals grade`](cli.md#evals-grade); with no `workspace`, the task's solution.
`manifest` is a `harness.toml` whose judge settings apply. With `lock`, `tests` graders run
in its [sandbox](../guides/sandbox.md) when it covers `graders` (unless `no_sandbox`;
`trust` approves it). Returns JSON with `task`, `workspace`, `judge`, `success`, `score`
(the mean), `sandbox` (its summary, or null), `notes`, and `grades`, each with `grader`,
`success`, `score`, and `detail`, and for judges `parts` (each criterion's `label` and
`value`), `uncertain`, and `judged_by` (the model that answered).

```python
>>> r = json.loads(hl.grade_eval_task("evals", "greet_people", "run"))
>>> r["success"], round(r["score"], 2), [(g["grader"], g["success"]) for g in r["grades"]]
(False, 0.67, [('artifact:greetings.md', True), ('tests:greet_people/check.py', False)])
```

### `sandbox_info`

```python
sandbox_info(path="harness.lock", manifest=None, no_sandbox=False) -> str
```

The lock's [sandbox](../guides/sandbox.md) and how it stands on this machine, like
[`hlock sandbox`](cli.md#sandbox). Returns JSON with `spec` (the lock's `[policy.sandbox]`,
or null), `summary`, `declared` (`harness.toml`'s), `drift`, `weakened`, `off`, `runner`
(its `name`, `origin`, `program`, and whether it's `installed`), `trusted`, `error` (why it
can't run here, or null), `policy` (violations), `evidence` and `evidence_note`.

### `sandbox_control_snapshot`

```python
sandbox_control_snapshot(project) -> str
```

A digest of each control file in `project` (`harness.toml`, the locks, the policy,
`.envrc`, git hooks and config), or null where there's none, as JSON. Compare one taken
before a sandboxed session with one after to see what the session created or changed.

### `sandbox_session_begin` and `sandbox_session_end`

```python
sandbox_session_begin(session, env) -> str
sandbox_session_end(session, env) -> str
```

Run an [OpenShell](../guides/sandbox.md#run-the-lock-on-openshell) session around a
prepared launch: `session` is the JSON of `launch_prepare`'s `sandbox.session`, `env` its
`sandbox.env`. `begin` creates the sandbox, uploads what the run sees and attaches providers,
and returns JSON `{argv, session}`: run `argv`, then pass that `session` to `end`, which copies
back what the run changed, deletes the sandbox and its providers, and returns JSON `{notes}`.
`hlock.sandbox.run` does both.

### `prepare_eval_workspace`

```python
prepare_eval_workspace(path, task, dest) -> str
```

Make `dest` a fresh workspace for `task` in the pack at `path`, holding the task's fixtures,
as [`hlock evals run`](cli.md#evals-run) does before it runs the agent there. `dest` must not
exist yet, or be empty. Returns JSON with `task`, `prompt` (the prompt's text),
`timeout-seconds`, and `workspace`.

```python
>>> r = json.loads(hl.prepare_eval_workspace("evals", "count_words", "run"))
>>> r["task"], r["timeout-seconds"], os.listdir("run")
('count_words', 300, ['words.txt'])
```

### `eval_packs_digest`

```python
eval_packs_digest(manifest) -> str
```

The digest of every file in the eval packs the `harness.toml` at `manifest` names, as a
resolved lock records it in `[evidence] eval-pack-digest`.

```python
>>> hl.eval_packs_digest("harness.toml")
'sha256:1de30d21400e4ebc04033d646b314ba32a1d9011751712f92cba4ddce71d5e1e'
```

### `calibrate_eval_pack`

```python
calibrate_eval_pack(path, against, model=None, manifest=None, against_base_url=None) -> str
```

Compare the pack's judge (or `model`) with `against`, like
[`hlock evals calibrate`](cli.md#evals-calibrate). Returns JSON with `a`, `b`, `rows`
(each with `task`, `run`, `grader`, `criterion`, `a`, `b`, and `agree`), and `agreement`.

### Dependencies

The functions behind [`hlock add`, `remove`, `outdated` and `export`](../guides/dependencies.md).
Each takes `manifest` and `lock` paths (default `harness.toml` and `harness.lock`) and returns
JSON.

```python
import json
import hlock as hl

prepared = hl.deps_prepare("github:acme/skills/skills/sql@^1.2", kind="skill")
entry = json.loads(prepared)["resolved"][0]["entry"]["skill"]
print(entry["resolved-tag"], entry["path"])   # v1.3.0 .claude/skills/sql

out = json.loads(hl.deps_commit(prepared))
print(out["installed"])                         # ['copied skill sql → .claude/skills/sql']
print(json.loads(hl.deps_check()))              # []
```

| Function | Does |
|---|---|
| `deps_prepare(source, kind=None, name=None, scope=None, harness=None, manifest=…, lock=…)` | Resolve and fetch a dependency into the store; nothing in the project changes (`--dry-run`) |
| `deps_commit(prepared, install=True, measurement=None)` | Declare, pin and install a prepared dependency, with `deps_measure`'s result as its evidence |
| `deps_measure(prepared)` | Run the eval pack with and without it (`--eval`) |
| `deps_adopt(name, …)` | Declare something `pin` recorded (`add --adopt`) |
| `deps_remove(kind, name, …, uninstall=True)` | Undeclare, unpin and uninstall (`remove`) |
| `deps_check(lock=…, manifest=…)` | How the declared dependencies differ from the lock and the harness: a list of `{problem, kind, name, detail, fix, fails}` |
| `deps_refresh(lock=…, manifest=…, frozen=False, write=True)` | Lock what's newly declared or changed (what `sync` does first): `{"changes": […]}` |
| `deps_outdated(lock=…, manifest=…, kind=None)` | Newer commits, tags and versions upstream |
| `deps_upgrade(name, …)` | Prepare a declared dependency at the newest it allows (`upgrade dep:NAME`), for `deps_commit` |
| `export_apm_lock(lock=…, out=None, force=False, manifest=…)` | `apm.lock.yaml` from the lock: written to `out`, or only returned (`text`) |

### Audit and trust

The functions behind [`hlock audit`, `trust` and the SBOM export](../guides/audit.md). Each
takes a `lock` path (default `harness.lock`) and returns JSON, except `trust_approve`.

| Function | Does |
|---|---|
| `audit(lock=…, ci=False, strip=False, verbose=False, format="json")` | The audit report; `format="sarif"` for SARIF 2.1.0 |
| `scan_lock(lock=…)` | Hidden characters in what the lock places, and whether each entry allowed them |
| `trust_list(lock=…)` | What the lock would run here, with each one's digest and status |
| `trust_approve(lock=…, only=None, deny=False)` | Approve (or deny) those in `only` (names or digests), else every one not yet trusted; returns how many changed |
| `policy_check(lock=…)` | The project policy's `enforcement`, `files` and `violations` for the lock |
| `export_sbom(lock=…, format="cyclonedx")` | The lock as CycloneDX 1.6 or (`"spdx"`) SPDX 2.3 JSON |

### Decision models

The functions behind [`hlock decide`](cli.md#decide). Each returns JSON, except
`decider_drift`.

| Function | Does |
|---|---|
| `decide(state, questions, model=None, use_=None, manifest=None)` | Ask `questions` (JSON: a list of `{id, type, instructions, criteria}`, `type` being `noul`, `choice` or `score`) about `state` (JSON, or text; text that starts with `{` or `[` must be JSON). Returns `provider` and `decided`: the `model` that answered, `answers` (each with `id`, `type`, `value`, `probabilities`, `confidence`), `input_tokens`, and `cost_usd`. |
| `decide_hook(use_, format, input, lock)` | Answer a hook event (JSON) with the lock's decider for `use_`. Returns `stdout`, `exit`, and `note`; it never raises, and defers (with a note saying why) when `input` isn't JSON. |
| `decider_drift(lock)` | A list of what changed in the lock's deciders: hlock's questions, policy files |

Without `model`, `decide` asks the model `[deciders.<use_>]` names, else
`[deciders.default]`'s, else one hlock finds. With `model`, it takes the base URL, key
variable and protocol from that table only when the table's model is from the same
provider. A `manifest` path that doesn't exist raises `ManifestError`, here and in the
eval functions.

```python
questions = json.dumps([{"id": "urgent", "type": "noul", "instructions": "Is it urgent?"}])
r = json.loads(hl.decide("My payouts failed for three days.", questions, model="ollama:kev:4b"))
r["decided"]["answers"][0]["value"]   # P(urgent)
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

Each JSON argument must be a JSON object; anything else raises `HarnessLockError`.

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
| `.backend` | Configured backend target (default `"offline"`) |
| `.backends()` | `[backends]` as JSON: each backend the project defines or configures |
| `.manifest_hash()` | `"sha256:…"` hash of the text the manifest was parsed from, as recorded in the lock |
| `.eval_packs()` | List of eval pack paths (`.task_packs()` is the old name) |
| `.decider(use)` | The model `[deciders.<use>]` names, else `[deciders.default]`'s (also when the use lists only `any-of` candidates), or `None` |
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

## `hlock.backends`

The API for [writing a backend](../guides/backends.md#write-your-own-backend) in Python.

| Name | Description |
|---|---|
| `Backend` | Base class. Set `name`, `kinds` (the work items it runs; default `KINDS`), `model_endpoint` (its trials call `HL_LLM_BASE_URL`; default `True`), and `about`, and override `run(request) -> dict`, which returns the item's result. `describe(request)` answers `backend.describe` from those attributes |
| `run_local(request) -> dict` | Run a work item with hlock's stock handler, in this process, and return its result. Raises `BackendError` when it fails |
| `BackendError` | Raise it from `run` to fail the item with a message |
| `load(name, object=None) -> Backend` | The backend `object` (`"module:attr"`) names, else the built-in or `hlock.backends` entry point called `name`. A class is instantiated with no arguments |
| `available() -> dict` | Every Python backend this interpreter can load, by name |
| `KINDS` | The work items hlock's stock handlers run |
| `GROUP` | `"hlock.backends"`, the entry point group |

## Constants

| Name | Description |
|---|---|
| `__version__` | Package version |
| `LOCK_VERSION` | Lockfile schema version this release reads and writes (`1`) |

## Errors

Invalid input and failed operations raise a `HarnessLockError`, a subclass of
`ValueError`, so `except ValueError` still catches them. Catch a subclass to handle one
kind:

| Exception | Raised for |
|---|---|
| `ManifestError` | A `harness.toml` that's missing (when a path is passed) or invalid, including a bad `[deciders]` or `[evals.judge]` table |
| `LockfileError` | A lock that's missing or invalid, or that hlock refuses (see [checks on a lock](lockfile.md#checks)) |
| `EvalPackError` | An eval pack that's missing or invalid |
| `StoreError` | A blob missing from the content-addressed store |
| `TuningError` | No viable stack, or a failed tuning run |
| `ProfileError` | A harness or runtime profile, harness selection, or a sync or launch step that failed |
| `WorkerError` | The Python worker failed |
| `PullError` | A [`pull`](../guides/pull.md) source hlock can't read or won't fetch, or `git` failed |
| `RegistryError` | A [registry](../guides/registry.md) isn't configured or reachable, or it refused the request |
| `DependencyError` | A [declared dependency](../guides/dependencies.md) hlock can't read, resolve, fetch or install |
| `AuditError` | [Hidden characters](../guides/audit.md#hidden-characters), a [policy](../guides/audit.md#policy) violation, or a bad trust store stopped a step |

Messages keep their prefix (`manifest error:`, `lockfile error:`, …). A file hlock can't
read or write raises the matching `OSError` (`FileNotFoundError`, `PermissionError`, …),
not a `ValueError`. Arguments that should be JSON (`SearchSpace`'s, `decide`'s
`questions`) raise `HarnessLockError` when they aren't.

```python
import hlock as hl

try:
    hl.Lockfile.load("harness.lock")
except hl.LockfileError as e:
    print(e)   # lockfile error: cannot read harness.lock: No such file or directory (os error 2)
```

Calls that run commands or wait on the network (`resolve`, `tune`, `upgrade`, `pull`,
`sync_plan`, `sync_run_step`, `launch_prepare`, `localize`, `migrate`, `decide`, the eval and meta
functions, and probes) release the GIL, and Ctrl-C stops them: they raise
`KeyboardInterrupt` and stop the commands they started. `agent_run` re-raises the first
exception its `say` or `ask` callback raises, after stopping the run.
