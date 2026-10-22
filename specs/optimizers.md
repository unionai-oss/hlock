# Optimizers and backends: what searches, and where it runs

Status: phase 1 implemented · Owner: Niels Bantilan · Last updated: 2026-10-21 · Builds on [backends](backends.md), [tune optimizers](tune-optimizers.md) (replaces its §3.1)

> `tune` has two independent choices that the code treats as one. The **optimizer** is the
> algorithm that proposes harness edits: hlock's built-in loop, Harness², and later GEPA,
> RRSI and whatever the community writes. The **backend** is where tune's work runs: this
> machine, or Flyte (a local devbox or a real cluster). Today an optimizer *is* a backend:
> Harness² is registered as `hlock.backends:harness2`, configured under `[backends.harness2]`,
> listed by `hlock backend list`, and picked with the same resolution code that picks Flyte.
> This spec gives optimizers their own protocol, registry, manifest table and CLI, and
> leaves backends to mean only *where*.

---

## 1. Problem

1. **One registry for two things.** `hlock backend list` shows `local`, `flyte` and
   `harness2` side by side, and `[backend] target = "harness2"` passes validation, then
   fails at the first trial: Harness² doesn't run trials.
2. **One config table for two things.** Harness²'s options live in `[backends.harness2]`,
   next to Flyte's cluster settings, and reach it as `backend_options`. A Harness² run on
   Flyte has two "backends" in one request, and the request can only carry one's options.
3. **The protocol is the wrong shape for an optimizer.** To plug in an optimizer, a
   community author subclasses `Backend`, sets `kinds = ("harness.optimize",)`, and
   implements `run(request)`, a dispatch over work items it doesn't run. `model_endpoint`,
   `backend.describe` and the trial preflight all apply to it and mean nothing.
4. **The built-in optimizer is tied to the backend.** `[tune] optimizer = "hlock"` means
   "whatever `harness.optimize` handler the trial backend has". Flyte special-cases
   `harness.optimize` to run it on this machine, so where the search ran isn't recorded.

## 2. The two axes

| | Optimizer | Backend |
|---|---|---|
| Question | Who proposes edits? | Where does the work run? |
| Built in | `hlock` (default), `harness2` | `offline`, `local`, `flyte` |
| Chosen by | `[tune] optimizer`, `--optimizer` | `[backend] target`, `--backend` |
| Configured in | `[optimizers.<name>]` | `[backends.<name>]` |
| Found as | built in, `[optimizers.<name>]`, `hlock-optimizer-<name>` on `PATH`, `hlock.optimizers` entry point | built in, `[backends.<name>]`, `hlock-backend-<name>` on `PATH`, `hlock.backends` entry point |
| Python API | `hlock.optimizers.Optimizer.optimize(request)` | `hlock.backends.Backend.run(request)` |
| Inspect | `hlock optimizer list`, `hlock optimizer status` | `hlock backend list`, `hlock backend status` |

Every pairing is valid: Harness² on `local`, the built-in loop on Flyte, a community
optimizer on a community backend.

```toml title="harness.toml"
[backend]
target = "flyte"                       # where trials run

[backends.flyte]
options = { config = "~/.flyte/devbox.yaml" }   # a devbox or a cluster: the config decides

[tune]
optimizer = "harness2"                 # who searches
edit = ["instructions", "skill"]

[optimizers.harness2]
options = { k = 2, mode = "sequential" }
```

A `devbox` and a cluster aren't separate backends. Both are `flyte`, and the Flyte config
file `options.config` names decides which one the tasks go to.

## 3. Design

### 3.1 The optimizer protocol

An optimizer gets one `harness.optimize` request ([tune optimizers](tune-optimizers.md)
§3.3: the seed bundle, the train and dev task ids, the session to evaluate through, the
budget) and returns one result (§3.5: the candidate bundle, the ledger, its name and
version). It doesn't run trials itself; it calls the session, and the session runs them on
the backend.

In Python:

```python
from hlock.optimizers import Optimizer
from hlock.optimize import Session

class Gepa(Optimizer):
    name = "gepa"
    about = "GEPA reflective Pareto search (gepa 0.9)"

    def optimize(self, request: dict) -> dict:
        opts = request["optimizer"]["options"]
        session = Session(request["optimize"]["session"])
        ...
        return {"candidate": str(best), "ledger": str(ledger), "optimizer": {"name": "gepa", "version": "0.9.2"}}
```

registered under the `hlock.optimizers` entry point group, or named in the manifest
(`[optimizers.gepa] python = "hlock_gepa:Gepa"`). Raise `hlock.optimizers.OptimizerError` to
fail the search with a message.

As a command: `[optimizers.<name>] command = [...]`, or an `hlock-optimizer-<name>`
executable on `PATH`. It's called with the request and response paths after its own
arguments and speaks the [backend envelope](backends.md) §4.2 (`{"version": 5, "ok": true,
"result": {...}}`), so any language can implement one.

`[optimizers.<name>]` takes the same keys as `[backends.<name>]`: `python` or `command`
(one), and `options`. The built-ins accept only `options`.

### 3.2 The wire: contract version 5

`harness.optimize` names the optimizer separately from the backend:

```json
{
  "version": 5,
  "kind": "harness.optimize",
  "backend": "local",
  "backend_options": {},
  "optimizer": {"name": "harness2", "options": {"k": 2}, "object": null, "command": null},
  "workdir": "/abs/.hlock/tune/7c1e/opt",
  "optimize": {"session": "…", "seed": "…", "tasks": {"train": […], "dev": […]}, "…": "…"}
}
```

`backend_options` are the backend's own; the optimizer's are under `optimizer.options`.
`object` is `[optimizers.<name>] python`, `command` its `command` (or the `PATH` executable).
Both are null for a built-in or an entry point, which the worker looks up by `name`.

### 3.3 Where the search runs

A backend that lists `harness.optimize` in its `backend.describe` answer hosts the search:
it runs the named optimizer, and the optimizer's evaluations run on the backend's trials.
hlock's Python backends do this with `hlock.optimizers.run(request)`, which loads the
optimizer (or runs its command) and calls it.

A backend that doesn't list `harness.optimize` (a backend that only runs trials, a command
backend from before contract 5, and `flyte` in phase 1) still gets the optimizer's trials,
and the search runs on this machine in hlock's Python worker, as the `local` backend runs it.
`offline` runs no optimize stage, as before.

The tune lock records both: `[tune.optimizer] name` is the optimizer and `backend` the
backend that hosted the search. The trials' backend is the lock's `provenance`, as before.

### 3.4 Flyte

Phase 1: the search runs on this machine and every trial it evaluates is a Flyte task (on
the devbox or cluster the config names). This is today's behavior, now explicit: `flyte`
doesn't list `harness.optimize`, and the core sends the search to the local worker.

Phase 2: the search itself as a Flyte task. The session (`session.json`, the base lock, the
pack and the manifest) goes with the request, as a work item's files go today, and the
task runs the optimizer and its trials in the pod (trials in process, or as child tasks).
The session's usage and trial cache come back with the candidate and merge into the local
session, so the budget holds across the boundary. Needed before an optimizer with a long
search (Harness²'s hours of rollouts) can run without this machine staying up; until then,
`hlock tune --detach` covers it.

### 3.5 CLI

```console
$ hlock tune --backend flyte --optimizer harness2
$ hlock optimizer list
optimizer  found
hlock      built in
harness2   built in
gepa       Python entry point hlock_gepa:Gepa
$ hlock optimizer status harness2
optimizer  harness2  (from --optimizer)
found      built in
options    k=2, mode=sequential
about      Harness² (google-research/harness2), run as a black box in its own environment
```

`hlock backend list` no longer shows optimizers. `hlock tune`'s report shows
`optimizer  harness2 on local …` so where the search ran is visible.

### 3.6 Breaking changes

No compatibility shims:

- `[backends.harness2]` → `[optimizers.harness2]`. A `[tune] optimizer` that names a
  `[backends.<name>]` table and no `[optimizers.<name>]` table fails with that hint.
- `hlock.backends.harness2` → `hlock.optimizers.harness2`; `Harness2Backend` →
  `Harness2Optimizer`.
- An optimizer written as a `Backend` with `kinds = ("harness.optimize",)` must become an
  `Optimizer`.
- Contract version 4 → 5 in both `hlock/tasks/worker.py` and the core.

## 4. Non-goals

- Running several optimizers in one tune ([tune optimizers](tune-optimizers.md) §5).
- Per-optimizer backends (Harness²'s own rollouts on one backend, hlock's acceptance runs on
  another). Harness² brings its own executor today and hlock's gate always runs on the
  tune's backend.

## 5. Phasing

1. `hlock.optimizers` (protocol, registry, `hlock` and `harness2` built in),
   `[optimizers.<name>]`, contract 5 with `optimizer` in the request, the search on the
   local worker for a backend that doesn't host it, `backend` in `[tune.optimizer]`,
   `hlock optimizer list|status`.
2. The search as a Flyte task (§3.4).
