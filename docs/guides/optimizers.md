# Optimizers

`hlock tune` makes two separate choices when it [edits the harness](tune.md#tune-the-harness-text):

| | Optimizer | [Backend](backends.md) |
|---|---|---|
| Decides | What searches for edits | Where the work runs |
| Built in | `hlock` (default), `harness2` | `offline`, `local`, `flyte` |
| Select with | `[tune] optimizer`, `--optimizer` | `[backend] target`, `--backend` |
| Configure in | `[optimizers.<name>] options` | `[backends.<name>] options` |
| Inspect with | `hlock optimizer list`, `hlock optimizer status` | `hlock backend list`, `hlock backend status` |

Any optimizer runs on any backend. hlock keeps the grading, the budget, the held-out tasks
and the decision to accept, so an optimizer only searches.

```toml title="harness.toml"
[backend]
target = "flyte"                       # trials run as Flyte tasks, on a devbox or a cluster

[tune]
optimizer = "harness2"                 # Harness² proposes the edits

[optimizers.harness2]
options = { python = "~/src/harness2/.venv/bin/python", k = 2, mode = "sequential" }
```

```console
$ hlock optimizer status
optimizer  harness2  (from harness.toml)
found      built in
options    k=2, mode=sequential, python=~/src/harness2/.venv/bin/python
about      Harness² (google-research/harness2), run as a black box in its own environment
```

## Where the search runs

A backend that lists `harness.optimize` in what it runs hosts the search. `local` does: the
optimizer runs in hlock's Python worker on this machine. For any other backend, `flyte`
included, the search runs on this machine in the same worker, and each trial it evaluates
runs on the backend. The tune lock records both: `[tune.optimizer] backend` is where the
search ran, and `provenance` where the trials ran. `hlock tune` shows it as
`optimizer  harness2 on local`.

`offline` runs no optimize stage.

## Finding an optimizer

`hlock` looks a name up in this order:

1. Built in: `hlock`, `harness2`. Their tables take only `options`.
2. `[optimizers.<name>]` with `python = "module:attr"` or `command = [...]`.
3. An `hlock-optimizer-<name>` executable on `PATH`.
4. A Python package's `hlock.optimizers` entry point.

```console
$ hlock optimizer list
optimizer    found
hlock        built in
harness2     built in
mine         harness.toml: Python object mine_opt:Mine
```

A name that's only a `[backends.<name>]` table is an error that says to move it to
`[optimizers.<name>]`.

## Write an optimizer

An optimizer gets one `harness.optimize` request and returns one result. The request
carries the optimizer's own `name` and `options` under `optimizer`, and an `optimize` object:

| Field | What it is |
|---|---|
| `session` | The tune session's directory, for `hlock tune eval --session` or `hlock.optimize.Session` |
| `seed` | The bundle: `instructions/<file>`, `skills/<name>/`, and `bundle.json` (each component's `id`, `kind`, `editable`; the kinds it may add; size limits) |
| `tasks` | Task ids: `train` and `dev`. The test split isn't there |
| `task_meta` | Each task's prompt |
| `evaluate` | The command that evaluates a candidate: `hlock tune eval --session <dir>` |
| `budget`, `trials` | The most trials (`rollouts`) or dollars (`usd`) it may spend, and trials per task |
| `models` | The task model, and the `proposer` from `[tune]` |
| `noise` | `dev_success_sd`: how much the lock's own text varies on dev |

A candidate is a directory in the seed's layout. The result names the best one, and a ledger:

```json title="result"
{"candidate": "/abs/.../best", "ledger": "/abs/.../ledger.jsonl",
 "optimizer": {"name": "mine", "version": "1.0", "proposer": "anthropic:opus"}}
```

Each ledger line is an edit hlock records with the change: `{"component": "skills/review",
"op": "modify", "rationale": "..."}`.

### In Python

Subclass `hlock.optimizers.Optimizer`. `hlock.optimize.Session` runs the trials, on the
tune's backend:

```python title="mine_opt.py"
import json
import shutil
from pathlib import Path

from hlock.optimize import BudgetExhausted, Session
from hlock.optimizers import Optimizer


class Mine(Optimizer):
    name = "mine"
    about = "adds one line of guidance"

    def optimize(self, request):
        opt = request["optimize"]
        session, ws = Session(opt["session"]), Path(request["workdir"])
        best = ws / "best"
        shutil.copytree(opt["seed"], best)
        f = best / "instructions" / "AGENTS.md"
        f.write_text(f.read_text() + "Check your result before you finish.\n")
        try:
            out = session.evaluate(best, "train")   # graded results, and the budget left
        except BudgetExhausted:
            pass
        (ws / "ledger.jsonl").write_text(json.dumps(
            {"component": "instructions/AGENTS.md", "op": "modify", "rationale": "..."}) + "\n")
        return {"candidate": str(best), "ledger": str(ws / "ledger.jsonl"),
                "optimizer": {"name": "mine", "version": "1.0"}}
```

```toml title="harness.toml"
[optimizers.mine]
python = "mine_opt:Mine"

[tune]
optimizer = "mine"
```

A package registers it instead under the `hlock.optimizers` entry point group. Raise
`hlock.optimizers.OptimizerError` to fail the search with a message.

`session.evaluate` raises `ValueError` for a candidate that changes what `bundle.json` doesn't
allow, a task outside the split, or the test split, and `BudgetExhausted` once the budget is
spent. Whatever the optimizer returns, hlock checks it again before accepting it (see
[Tuning](tune.md#tune-the-harness-text)).

### As a command

Any executable works: `[optimizers.<name>] command = ["my-optimizer", "--flag"]`, or
`hlock-optimizer-<name>` on `PATH`. It gets the request and response file paths after its
own arguments and answers in the [backend contract](../concepts/architecture.md#the-backend-contract)'s
envelope: `{"version": 5, "ok": true, "result": {...}}`, or `{"version": 5, "ok": false,
"error": "..."}`. It evaluates candidates with `hlock tune eval --session <dir>`, the
request's `evaluate` command.

The design is in
[`specs/optimizers.md`](https://github.com/unionai-oss/hlock/blob/main/specs/optimizers.md)
and [`specs/tune-optimizers.md`](https://github.com/unionai-oss/hlock/blob/main/specs/tune-optimizers.md).
