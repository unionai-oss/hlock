# Pluggable backends

Status: implemented (the `flyte` backend tested on a local `flyte start devbox`) · Owner: Niels Bantilan · Last updated: 2026-10-21 · Builds on [v0](v0.md) §8

> A backend decides where hlock's work runs. Today the three backends are hard-coded,
> and the only remote one is Flyte. This spec renames the built-ins after what they do
> (`offline`, `local`, `flyte`) and defines a protocol that anyone can implement, in
> Rust or Python, to add their own: Ray, Modal, Kubernetes Jobs, Slurm, a company's
> internal runner. Flyte becomes one backend built on that protocol, not a special case.

---

## 1. Problem

1. **The names say the wrong thing.** `local` and `devbox` both run on this machine. What
   separates them is a simulated model versus real models. `cluster` doesn't say Flyte.
   [v0](v0.md) §8.2 planned `devbox` as a local Flyte devbox, but the code never did
   that: the worker runs every stage in process on `devbox` (`hlock/tasks/worker.py`,
   `_maybe_flyte` lifts only on `cluster`). Users read the name and expect Flyte.
2. **Backends are a closed enum.** `Backend` is `Local | Devbox | Cluster`
   (`worker.rs`). `Backend::parse` turns any other name into `Local`, so a typo such as
   `target = "clsuter"` silently runs offline. The CLI's `--backend` is a fixed
   `click.Choice`.
3. **Flyte is wired into the worker.** The Flyte lift is a branch inside
   `hlock.tasks.worker.main` (`_maybe_flyte`). Adding Ray would mean another branch in
   hlock itself. Nobody outside the project can add a backend.
4. **What a backend can run is hard-coded.** `tune` decides that the data, prompt and
   fine-tune stages need "devbox or cluster" from the enum (`ops.rs`,
   `Backend::needs_worker`). A backend that only runs trials, or only fine-tunes, has no
   way to say so.

## 2. Goals and non-goals

Goals:

- Built-in names that describe what each backend does.
- One wire protocol for running a piece of hlock's work, which a backend in any language
  can speak, with first-class helpers for Rust and Python.
- Backends that live outside this repo, installed and selected without changing hlock.
- Flyte rebuilt on the protocol, as the worked example.
- Errors, not silent fallbacks, for unknown backend names.

Non-goals:

- Shipping other backends (Ray, Modal, Kubernetes). The protocol has to make them
  possible; this repo doesn't include them.
- Moving the search loop, aggregation or lock construction out of the Rust core. A
  backend runs work items; the core still decides what to run and what the results mean.
- Running code harnesses through a backend. A code harness's trials already run its own
  entrypoint for real on every backend (`resolver.rs`, `code_harnesses`).
- Parallel fan-out inside the core. A backend may parallelize internally; the core still
  sends one work item at a time.

## 3. Names

| Name | Was | What runs the work |
|---|---|---|
| `offline` *(default)* | `local` | The Rust core, in process, on the deterministic reference harness. No Python, no network, no keys |
| `local` | `devbox` | hlock's Python worker on this machine: real models through `HL_LLM_BASE_URL`, trace loading, LoRA on a local GPU |
| `flyte` | `cluster` | The same work, each item lifted to a Flyte 2 task on the cluster in `~/.flyte/config.yaml` |
| *anything else* | | A backend someone wrote, found as described in §5 |

`devbox` and `cluster` stay as deprecated aliases of `local` and `flyte` for one minor
release. Using one prints a note (`backend 'devbox' is now 'local'`).

`local` changes meaning. A manifest with an explicit `target = "local"`, which used to
run offline, now runs the Python worker. With no `HL_LLM_BASE_URL`, `reference:…`
candidates still run on the offline reference model, so the lock is the same; candidates
that name a real model now fail preflight with "the local backend needs a model endpoint"
instead of being simulated. hlock is pre-1.0 and the default stays offline, so we take
the break and call it out in the release notes rather than pick a worse name.

`offline` is reserved: it's in-process Rust, not a protocol backend, and can't be
redefined. `local` and `flyte` are built in but take options like any other backend (§5).

## 4. The protocol

A backend is anything that runs **work items**. The Rust core sends one work item per
call and gets one result back. The protocol is the JSON that crosses that boundary, and
the way it's carried: a process invocation with two files.

### 4.1 Invocation

```
<backend command…> <request.json> <response.json>
```

The core writes the request, runs the command with stdin closed, and waits up to
`HLOCK_WORKER_TIMEOUT` seconds (default 3600). The command must write the response file
before exiting. Exit status 0 means `ok: true`; non-zero means it failed, and the
response's `error`, or else stderr, is shown to the user. Stdout is ignored and stderr is
kept for errors, so a backend may log freely to either.

Files rather than stdin/stdout keep large payloads (traces, adapters' metadata) out of
pipes and let a remote backend read the request and write the response from a different
process than the one the core started.

### 4.2 Request

```json
{
  "version": 3,
  "kind": "llm.run_trial",
  "backend": "ray",
  "backend_options": {"address": "ray://head:10001"},
  "stack": {"harness": "reference", "llm": {"planner": "openai:gpt-5"}, "...": "..."},
  "task": {"id": "count_words", "prompt_text": "...", "expected": ["answer.md"], "fixtures": "count_words/workspace"},
  "seed": 0,
  "workdir": "/abs/path/.hlock/runs/…",
  "plan": {"depth": "probe", "data": "user", "...": "..."}
}
```

| Field | Meaning |
|---|---|
| `version` | The contract version, `3` in this spec. A backend refuses a version it doesn't know |
| `kind` | The work item (§4.4) |
| `backend` | The backend's name as the user selected it, after aliases |
| `backend_options` | The `options` table from `[backends.<name>]`, or `{}` |
| `backend_object` | `[backends.<name>] python`, when set: the `module:attr` hlock's worker loads. Absent otherwise |
| `stack`, `task`, `seed` | The trial's configuration, task and seed (empty objects for stage items) |
| `workdir` | An absolute directory the item may read and write. For a trial it holds the task's fixtures, and the core grades what's there afterwards |
| `plan` | The tune plan: depth, data source, axes, trainset digest |

Unknown fields are ignored on both sides, so a minor addition doesn't need a version bump.

### 4.3 Response

```json
{"version": 3, "ok": true, "result": {"success": true, "score": 1.0, "cost_usd": 0.0021, "simulated": false,
 "gpu_h": 0.0, "latency_s": 3.2, "steps": 1, "trace": [], "artifact_digests": [], "model_digests": {}}}
```

```json
{"version": 3, "ok": false, "error": "ray cluster unreachable at ray://head:10001"}
```

`result` fields all have defaults. `cost_usd` is `null` when unknown, never a made-up 0.
`simulated` is true when a stand-in ran the trial instead of a model. `model_digests` is
free-form stage metadata (rows, digests, adapter digest); a `run` key in it is recorded
as the run id in the tune lock's provenance, so a remote backend should set it (for
Flyte, the run name).

### 4.4 Work items

| `kind` | Used by | Does |
|---|---|---|
| `backend.describe` | every command, before the first item | Says what the backend runs (§4.5) |
| `llm.run_trial` | `resolve`, `tune` | Runs one `(stack, task, seed)` trial into `workdir` |
| `data.load` | `tune` data stage | Loads and splits user traces |
| `data.synthetic` | `tune` data stage, `--data synthetic` | Generates task variants |
| `prompt.search` | `tune --depth prompt` and deeper | Searches system prompts |
| `ft.lora` | `tune --depth ft` and deeper | Trains a LoRA adapter |
| `grade` | (reserved) | Grades a workspace with a model rubric |

A backend needn't run every kind. It lists the ones it runs in `describe`.

### 4.5 `backend.describe`

The core sends `describe` once per command, before any other item. It's both the
capability check and the "is this backend installed and working" check.

```json
{"version": 3, "ok": true, "result": {
  "name": "ray",
  "kinds": ["llm.run_trial", "data.load"],
  "model_endpoint": true,
  "about": "hlock-ray 0.2.0 on ray://head:10001"
}}
```

The envelope is the same as every other item's; only `result` differs.

| Field | Meaning |
|---|---|
| `name` | The backend's name |
| `kinds` | The work items it runs |
| `model_endpoint` | Its trials call real models at `HL_LLM_BASE_URL`, so the core's preflight checks that a non-`reference` candidate has an endpoint before any trial runs. hlock's stock handlers set it; a backend serving its own models leaves it false |
| `about` | One line for `hlock backend status`: version, where it runs |

What the core does with it:

- `resolve` fails before any trial when the backend doesn't list `llm.run_trial` and
  some candidate isn't a code harness: "the ray backend doesn't run trials".
- `tune` skips a stage whose kind isn't listed and records why ("the ray backend doesn't
  run prompt.search"), as it does for `offline` today. A skipped stage isn't recorded in
  the lock.
- A failed `describe` (command not found, bad version, an error) stops the command with
  that error before any work runs.

### 4.6 Versioning

`CONTRACT_VERSION` goes from 2 to 3: the request gains `backend_options`, the backend
names change, and `backend.describe` is new. The Rust core and hlock's worker move
together, as before (`worker.rs`, `hlock/tasks/worker.py`). A third-party backend that
gets a version it doesn't know answers `ok: false` with
`"unsupported contract version N"`, which the core shows as is.

## 5. Choosing and finding a backend

The manifest selects a backend by name and can define or configure one:

```toml title="harness.toml"
[backend]
target = "ray"

[backends.ray]
command = ["hlock-backend-ray", "--verbose"]   # any executable that speaks §4
options = { address = "ray://head:10001" }      # sent as backend_options

[backends.modal]
python = "hlock_modal:ModalBackend"             # a Python object, run by hlock's worker

[backends.flyte]
options = { project = "agents", domain = "development" }
```

A `[backends.<name>]` table has at most one of `command` or `python`, plus `options`. For
`local` and `flyte` only `options` is allowed; `offline` can't appear. `--backend NAME`
on `resolve` and `tune` overrides `[backend] target`, as today.

The core resolves a name in this order:

1. `offline`, `local`, `flyte` (and the `devbox`/`cluster` aliases): built in.
2. `[backends.<name>]` in the manifest: its `command`, or hlock's Python worker loading
   its `python` object.
3. An executable named `hlock-backend-<name>` on `PATH`, the way `git` and `cargo` find
   subcommands. This is how a Rust backend installed with `cargo install` is found.
4. A Python entry point named `<name>` in the `hlock.backends` group, loaded by hlock's
   worker. This is how a backend installed with `pip install` is found.

Step 4 is checked by the worker during `describe`. If nothing matches, `describe` fails
with: `no backend named 'ray': not built in, not in [backends], no hlock-backend-ray on
PATH, and no 'hlock.backends' entry point (see Backends)`.

## 6. Writing a backend in Python

hlock ships a base class and the stock handlers, so most Python backends only decide
*where* a work item runs:

```python
# hlock_ray/__init__.py
from hlock.backends import Backend, run_local

class RayBackend(Backend):
    name = "ray"
    kinds = ("llm.run_trial", "data.load")

    def run(self, request: dict) -> dict:
        import ray
        ray.init(address=request["backend_options"].get("address", "auto"), ignore_reinit_error=True)
        return ray.get(ray.remote(run_local).remote(request))
```

```toml title="pyproject.toml of hlock-ray"
[project.entry-points."hlock.backends"]
ray = "hlock_ray:RayBackend"
```

The API in `hlock.backends`:

| Name | What it is |
|---|---|
| `Backend` | Base class. Set `name` and `kinds` (default: every stock kind), override `run(request) -> result`. `describe(request)` has a default built from `name`, `kinds`, `model_endpoint` (default true) and `about` |
| `run_local(request) -> result` | Runs a work item with hlock's stock handler, in this process. What the `local` backend does, and what a remote backend runs on the other side |
| `BackendError` | Raise it from `run` to fail the item with a message, without a traceback |
| `load(name, object=None) -> Backend` | Finds a backend: a `module:attr` object, else a built-in, else an entry point |

`run` returns the `result` object (§4.3); the worker wraps it in the response. An
exception other than `BackendError` fails the item with its message. hlock's worker
(`python -m hlock.tasks.worker`) is the process: it reads the request, finds the backend
by `backend` (and the `python` object, when the manifest names one), calls `describe` or
`run`, and writes the response. A Python backend never parses argv or writes files.

The built-in `local` and `flyte` backends are written this way and registered through
the same `hlock.backends` entry points in hlock's own `pyproject.toml`.

## 7. Writing a backend in Rust

A Rust backend is an executable. The core crate (re-exported by `hlock` and
`harness-lock`) has the protocol types and a `serve` helper that does the file handling:

```rust
// src/main.rs of hlock-backend-slurm
use hlock::worker::{serve, Describe, Executor, TaskRequest, TaskResult};
use hlock::Result;

struct Slurm;

impl Executor for Slurm {
    fn describe(&self, _req: &TaskRequest) -> Result<Describe> {
        Ok(Describe::new("slurm", ["ft.lora"]).about("sbatch on the login node"))
    }

    fn call(&self, req: &TaskRequest) -> Result<TaskResult> {
        // submit, wait, read the adapter's digest...
        todo!()
    }
}

fn main() {
    std::process::exit(serve(&Slurm));
}
```

`cargo install`ing it as `hlock-backend-slurm` is enough for `target = "slurm"` (§5
step 3). `serve` reads the two paths from argv, checks the version, answers `describe`,
calls `call` for everything else, and writes the response; an `Err` becomes `ok: false`.

The same `Executor` trait is what the core calls internally, so a Rust program that
embeds the core can also hand one to the resolver in process, with no subprocess:
`Resolver::with_executor(manifest, workdir, "slurm", Box::new(Slurm))`. The built-in
process backends are `Executor`s too (`ProcessExecutor`, which runs a command per item).

## 8. Changes in the core

- `worker::Backend` becomes a struct: the resolved `name`, an optional `renamed_from`
  for the aliases, and how it runs (`Offline`, the Python worker with an optional
  object, or a command), plus its `options`. `Backend::resolve(&Manifest, Option<&str>)`
  replaces `Backend::parse` and returns an error for an unknown name instead of falling
  back to `local`. (`hlock-backend-<name>` on `PATH` and Python entry points are tried
  before that error; the entry point check happens in `describe`.)
- `Executor` trait (`describe`, `call`) and `ProcessExecutor`, which replaces
  `PythonWorker`: an argv prefix plus the request/response file handling it has today.
  The Python worker is `ProcessExecutor` over `[$HL_PYTHON, -m, hlock.tasks.worker]`.
  A failure the backend explains in its response is shown as it wrote it.
- `Worker` holds a backend, its `Executor`, and its `describe` answer, asked once and
  kept. `PythonAdapter` becomes `WorkerAdapter` over a shared `Worker`, and the resolver
  keeps the same `Worker` for `tune`'s stages.
- `Resolver` and `tune` use the `describe` answer's `kinds` instead of `needs_worker()`
  for what can run. The endpoint preflight runs when `model_endpoint` is true.
- `Runner::Embedded` marks an `Executor` handed to `Resolver::with_executor`.
- `Error::Worker` reads "backend error: …" instead of "python worker error: …".
- `Manifest` parses `[backends.<name>]` and validates it: one of `command`/`python`,
  nothing but `options` on the built-ins, no `offline`.
- The trial cache key uses the resolved backend name and its options, so `devbox` and
  `local` share a cache and changing an option doesn't reuse stale trials.

## 9. Changes in the Python package

- `hlock/backends/__init__.py`: `Backend`, `BackendError`, `run_local`, `load`.
- `hlock/backends/local.py`: `LocalBackend`, which is `run_local`.
- `hlock/backends/flyte.py`: `FlyteBackend`, moved out of the worker. The old lift could
  never run on a cluster, so every `cluster` item silently ran in process: it called
  `asyncio.run(flyte.run(...))` (`flyte.run` is synchronous and returns a `Run`), never
  called `flyte.init_from_config()`, read the `Run` as the task's output, and passed
  `flyte.Resources` a `gpu_model` it doesn't take. The new backend, run end to end on a
  local `flyte start devbox`:
    - initializes the SDK from `options.config` (with `options.project`/`domain`), with
      `root_dir` at the directory holding `hlock`, so tasks are named
      `hlock.backends.flyte.run_item` however hlock is installed
    - runs module-level tasks (`run_item` on CPU, `run_gpu_item` for `ft.lora`, overridden
      to `options.gpu`) with `copy_style="none"`: they come from the `hlock` in the image,
      never a bundle of the caller's directory (bundling a checkout shipped a macOS
      `_hlock.so` to Linux). The image installs `HLOCK_FLYTE_HLOCK`, a pip requirement or a
      Linux wheel (built for the wheel's platform), by default `hlock` at this version
    - sends the item's `workdir` files with the request and writes back the files the task
      wrote (8 MB cap each way), since hlock grades the local workspace
    - puts `HL_LLM_BASE_URL`, `HL_LLM_MODEL` and the hlock build in the task inputs, so
      `cache="auto"` keys on them (without that, a run with a real endpoint got the cached
      results of an earlier simulated run). `HL_LLM_API_KEY` is never sent: the task reads
      it from the Flyte secret `options.api_key_secret`, and a key set with no secret
      named stops the item
    - fails the item with the run's error when a run fails. It falls back to in process
      only when the SDK isn't installed; re-running a cluster failure here would hide it
  The `flyte` extra now installs the `flyte` SDK.
- `hlock/tasks/worker.py` keeps the stock handlers and becomes the process for Python
  backends: it finds the backend (§6) and dispatches. The Flyte code leaves it.
- CLI: `--backend` takes any name. `hlock backend status` shows the resolved backend,
  how it was found, and its `describe` (kinds, `about`). A new `hlock backend list` shows
  the built-ins, the manifest's `[backends]`, `hlock-backend-*` executables on `PATH`,
  and installed `hlock.backends` entry points.

## 10. Testing

- Rust: a fake command backend (a shell script, as the worker tests use today) that
  lists only `llm.run_trial`; `tune --depth prompt` on it skips the prompt stage with the
  reason. An unknown name errors. The aliases resolve with a note. `serve` round-trips a
  request through a test `Executor`, including errors and a bad version.
- Python: a test backend given as `python = "module:attr"` runs a resolve end to end; an
  entry point backend is found by `load`; `run_local` matches what `local` returns;
  `describe` of `local` lists every stock kind.
- The existing worker and tune tests move to the new names.

## 11. Open questions

- **Parallel items.** The core sends items one at a time, so a backend can't fan a
  resolve out across a cluster. A batch kind (`llm.run_trials`, many trials per request)
  would fix that without changing the transport. Left for a follow-up once there is a
  backend that needs it.
- **Long-lived backends.** A process per item is simple and isolates failures, but costs
  a Python start per trial. A `serve --stdio` mode that reads requests line by line would
  remove that cost. The file protocol can stay as the baseline either way.
- **Remote workspaces.** `workdir` is a local path. A backend that runs elsewhere has to
  ship the directory there and back itself; `flyte` sends the files inline, capped at
  8 MB. A future version could let `describe` ask the core to send a tarball, or an
  object-store URI, instead.
