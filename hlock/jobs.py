"""Background tune jobs: `hlock tune --detach`, then `hlock tune list | show | fetch | cancel`.

A tune can take minutes (prompt and hyperparameter search) or days (fine-tuning), so a
detached tune runs in its own process, outside the terminal that started it. Each job
gets a directory in the project:

    .hlock/tune/jobs/<id>/
        job.json            what was asked for, status, timestamps, report
        pid                 the orchestrator's process id
        tune.log            the orchestrator's stdout and stderr
        harness.tune.lock   the result, once the job succeeds
        work/               trial artifacts (unless --workdir was given)

The orchestrator is the same Rust-core tune that `hlock tune` runs in the foreground. On
`devbox` and `cluster` it hands stages to the worker as usual, so on `cluster` the heavy
stages run as Flyte tasks and their run ids end up in the result lock. A job writes its
lock into its own directory, never over `harness.tune.lock` or `harness.lock`:
`fetch` copies it out (and `--apply` adopts it) when you're ready.

Run a job's orchestrator with ``python -m hlock.jobs <job-dir>``.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

JOBS_DIR = Path(".hlock") / "tune" / "jobs"
RESULT = "harness.tune.lock"
LOG = "tune.log"

ACTIVE = ("queued", "running")
DONE = ("succeeded", "failed", "cancelled")


class JobError(ValueError):
    """A job that doesn't exist, is ambiguous, or isn't in the right state."""


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def _digest(path: Path) -> str | None:
    try:
        return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def jobs_dir(root: str | Path) -> Path:
    """Where a project keeps its tune jobs."""
    return Path(root) / JOBS_DIR


def _write(job_dir: Path, job: dict) -> None:
    tmp = job_dir / "job.json.tmp"
    tmp.write_text(json.dumps(job, indent=2) + "\n")
    os.replace(tmp, job_dir / "job.json")


def _read(job_dir: Path) -> dict:
    job = json.loads((job_dir / "job.json").read_text())
    job["dir"] = str(job_dir)
    return _refresh(job_dir, job)


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


def _pid(job_dir: Path) -> int | None:
    try:
        return int((job_dir / "pid").read_text().strip())
    except (OSError, ValueError):
        return None


def _refresh(job_dir: Path, job: dict) -> dict:
    """A job whose process is gone without reporting has failed; say so."""
    if job.get("status") in ACTIVE and job.get("host") == socket.gethostname():
        pid = _pid(job_dir)
        if pid is not None and not _alive(pid):
            job.update(status="failed", finished_at=_now(),
                       error=f"the tune process exited without reporting; see {job_dir / LOG}")
            _write(job_dir, {k: v for k, v in job.items() if k != "dir"})
    if job.get("status") == "running":
        job["trials"] = _trials(Path(job["workdir"]))
    return job


def _trials(workdir: Path) -> int:
    """Trials started so far: one workspace per (stack, task, seed) under runs/."""
    runs = workdir / "runs"
    try:
        return sum(1 for p in runs.iterdir() if p.is_dir() and not p.name.startswith("stage_"))
    except OSError:
        return 0


def submit(manifest: str | Path, *, lock: str | Path, options: dict, cwd: str | Path | None = None) -> dict:
    """Start a tune in the background and return its job record.

    ``options`` are `tune` keyword arguments (depth, data, harness, strict, max_trials,
    backend, workdir). The job's result goes into its own directory; `out` and `apply`
    are for `fetch`. Raises ValueError if the manifest can't be read.
    """
    from hlock import Manifest

    manifest = Path(manifest).resolve()
    backend = options.get("backend") or Manifest.load(str(manifest)).backend
    lock = Path(lock).resolve()
    root = manifest.parent
    base = jobs_dir(root)
    base.mkdir(parents=True, exist_ok=True)
    while True:
        job_id = secrets.token_hex(4)
        job_dir = base / job_id
        try:
            job_dir.mkdir()
            break
        except FileExistsError:
            continue
    opts = {k: v for k, v in options.items() if k not in ("out", "apply", "manifest", "lock")}
    opts["backend"] = backend
    workdir = Path(opts.pop("workdir", None) or job_dir / "work")
    job = {
        "id": job_id,
        "kind": "tune",
        "status": "queued",
        "created_at": _now(),
        "started_at": None,
        "finished_at": None,
        "host": socket.gethostname(),
        "cwd": str(Path(cwd or os.getcwd()).resolve()),
        "manifest": str(manifest),
        "lock": str(lock),
        "base_lock_digest": _digest(lock),
        "workdir": str(workdir.resolve() if not workdir.is_absolute() else workdir),
        "options": opts,
        "result": None,
        "report": None,
        "error": None,
    }
    _write(job_dir, job)

    kwargs: dict = {"stdin": subprocess.DEVNULL, "stderr": subprocess.STDOUT, "cwd": job["cwd"]}
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.DETACHED_PROCESS
    else:
        kwargs["start_new_session"] = True  # survive the terminal closing
    with open(job_dir / LOG, "ab") as log:
        proc = subprocess.Popen([sys.executable, "-m", "hlock.jobs", str(job_dir)], stdout=log, **kwargs)
    (job_dir / "pid").write_text(f"{proc.pid}\n")
    job["dir"] = str(job_dir)
    job["pid"] = proc.pid
    return job


def run(job_dir: str | Path) -> int:
    """The orchestrator: run the job's tune and record the outcome."""
    from hlock import tune

    job_dir = Path(job_dir)
    job = json.loads((job_dir / "job.json").read_text())
    job.update(status="running", started_at=_now())
    _write(job_dir, job)
    print(f"[{_now()}] tune job {job['id']} started (pid {os.getpid()})", flush=True)
    try:
        out = json.loads(tune(job["manifest"], out=str(job_dir / RESULT), apply=False, workdir=job["workdir"],
                              lock=job["lock"], **job["options"]))
    except Exception as e:  # noqa: BLE001 - a job records any failure rather than raising
        job.update(status="failed", finished_at=_now(), error=str(e).strip() or type(e).__name__)
        _write(job_dir, job)
        print(f"[{_now()}] tune job {job['id']} failed: {job['error']}", flush=True)
        return 1
    job.update(status="succeeded", finished_at=_now(), result=str(job_dir / RESULT), report=out.get("report"),
               summary=out.get("summary"), runs=_runs(job_dir / RESULT))
    _write(job_dir, job)
    print(f"[{_now()}] tune job {job['id']} succeeded", flush=True)
    return 0


def _runs(result: Path) -> list[str]:
    """Flyte run ids the cluster stages recorded in the tune lock."""
    from hlock import Lockfile

    try:
        run = ((json.loads(Lockfile.load(str(result)).to_json()).get("tune") or {}).get("provenance") or {}).get("run")
    except (OSError, ValueError):
        return []
    return [r for r in str(run or "").split(",") if r and not r.startswith("worker://")]


def list_jobs(root: str | Path) -> list[dict]:
    """Every tune job in the project, newest first."""
    base = jobs_dir(root)
    if not base.is_dir():
        return []
    jobs = []
    for d in base.iterdir():
        if (d / "job.json").is_file():
            try:
                jobs.append(_read(d))
            except (OSError, ValueError):
                continue
    return sorted(jobs, key=lambda j: j.get("created_at") or "", reverse=True)


def get(root: str | Path, ref: str | None = None) -> dict:
    """A job by id or unique id prefix; the newest job when ``ref`` is None."""
    jobs = list_jobs(root)
    if not jobs:
        raise JobError("no tune jobs in this project")
    if ref is None:
        return jobs[0]
    matches = [j for j in jobs if j["id"] == ref] or [j for j in jobs if j["id"].startswith(ref)]
    if not matches:
        raise JobError(f"no tune job {ref!r}")
    if len(matches) > 1:
        raise JobError(f"tune job {ref!r} is ambiguous: " + ", ".join(j["id"] for j in matches))
    return matches[0]


def wait(root: str | Path, ref: str, poll: float = 2.0, timeout: float | None = None) -> dict:
    """Block until a job finishes, and return it."""
    deadline = None if timeout is None else time.monotonic() + timeout
    while True:
        job = get(root, ref)
        if job["status"] in DONE:
            return job
        if deadline is not None and time.monotonic() >= deadline:
            raise JobError(f"tune job {ref} is still {job['status']}")
        time.sleep(poll)


def fetch(root: str | Path, ref: str | None, out: str | Path, apply: bool = False) -> dict:
    """Copy a finished job's tune lock to ``out``, and into the base lock with ``apply``.

    Returns the job, with ``fetched`` (the path written) and ``base_changed`` (whether the
    base lock changed since the job started, so the job tuned against an older one).
    """
    job = get(root, ref)
    if job["status"] != "succeeded":
        detail = f": {job['error']}" if job.get("error") else ""
        raise JobError(f"tune job {job['id']} is {job['status']}{detail}")
    src = Path(job["result"])
    if not src.is_file():
        raise JobError(f"tune job {job['id']} has no result at {src}")
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, out)
    lock = Path(job["lock"])
    job["base_changed"] = _digest(lock) != job.get("base_lock_digest")
    if apply:
        shutil.copyfile(src, lock)
    job["fetched"] = str(out)
    return job


def cancel(root: str | Path, ref: str) -> dict:
    """Stop a queued or running job (and the worker processes it started)."""
    job = get(root, ref)
    if job["status"] not in ACTIVE:
        raise JobError(f"tune job {job['id']} already {job['status']}")
    job_dir = Path(job["dir"])
    pid = _pid(job_dir)
    if job.get("host") != socket.gethostname():
        raise JobError(f"tune job {job['id']} runs on {job['host']}; cancel it there")
    if pid is not None and _alive(pid):
        try:
            if os.name == "nt":
                os.kill(pid, signal.SIGTERM)
            else:
                os.killpg(pid, signal.SIGTERM)  # the job leads its own session
        except ProcessLookupError:
            pass
    job = json.loads((job_dir / "job.json").read_text())
    if job["status"] in ACTIVE:
        job.update(status="cancelled", finished_at=_now())
        _write(job_dir, job)
    job["dir"] = str(job_dir)
    return job


if __name__ == "__main__":  # pragma: no cover - exercised through submit()
    sys.exit(run(sys.argv[1]))
