"""Tests for background tune jobs: `hlock tune --detach` and `hlock tune list | show | fetch | cancel`."""

import json
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import jobs
from hlock.cli import main

REPO = Path(__file__).resolve().parent.parent


@pytest.fixture
def project(tmp_path, monkeypatch):
    shutil.copytree(REPO / "evals", tmp_path / "evals")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    for key in list(os.environ):
        if key.startswith("HLOCK_"):
            monkeypatch.delenv(key)
    assert invoke("init", "--pack", "./evals").exit_code == 0
    assert invoke("resolve").exit_code == 0
    return tmp_path


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def job_id(output: str) -> str:
    return output.split("started tune job ", 1)[1].split()[0]


def test_detach_then_fetch(project):
    base = (project / "harness.lock").read_text()
    r = invoke("tune", "--detach", "--depth", "probe")
    assert r.exit_code == 0, r.output
    jid = job_id(r.output)
    assert f"hlock tune show {jid}" in r.output
    # a detached job never writes the project's locks itself
    assert not (project / "harness.tune.lock").exists()

    r = invoke("tune", "fetch", jid[:4], "--wait")
    assert r.exit_code == 0, r.output
    assert f"from tune job {jid}" in r.output and "Best configuration" in r.output
    assert (project / "harness.tune.lock").exists()
    assert (project / "harness.lock").read_text() == base

    r = invoke("tune", "list", "--json")
    rows = json.loads(r.output)
    assert [j["id"] for j in rows] == [jid]
    assert rows[0]["status"] == "succeeded"
    assert rows[0]["options"]["backend"] == "offline"
    assert rows[0]["report"]["depth"] == "probe"

    r = invoke("tune", "show")
    assert r.exit_code == 0, r.output
    assert jid in r.output and "succeeded" in r.output and "probe" in r.output

    r = invoke("tune", "fetch", "--apply", "--out", "elsewhere.tune.lock")
    assert r.exit_code == 0, r.output
    assert "adopted into harness.lock" in r.output
    assert (project / "harness.lock").read_text() == (project / "elsewhere.tune.lock").read_text()
    assert (project / "harness.lock.prev").read_text() == base  # the lock it replaced is kept


def test_failed_job(project):
    # the base lock already scores 100%, so --strict can't beat it
    r = invoke("tune", "--detach", "--depth", "probe", "--strict")
    jid = job_id(r.output)
    job = jobs.wait(project, jid, poll=0.1, timeout=60)
    assert job["status"] == "failed" and "--strict" in job["error"]

    r = invoke("tune", "fetch", jid)
    assert r.exit_code == 1 and "is failed" in r.output

    r = invoke("tune", "show", jid)
    assert "does not beat" in r.output and "tune.log" in r.output


def test_detach_rejects_apply_and_missing_manifest(project):
    r = invoke("tune", "--detach", "--apply")
    assert r.exit_code == 1 and "hlock tune fetch <job> --apply" in r.output
    (project / "harness.toml").unlink()
    r = invoke("tune", "--detach")
    assert r.exit_code == 1 and "hlock init" in r.output


def test_no_jobs(project):
    r = invoke("tune", "list")
    assert r.exit_code == 0 and "no tune jobs yet" in r.output
    for cmd in (["show"], ["fetch"], ["cancel", "abc"]):
        r = invoke("tune", *cmd)
        assert r.exit_code == 1 and "hlock tune --detach" in r.output, r.output


def fake_job(root: Path, job_id: str, pid: int, status: str = "running") -> Path:
    d = jobs.jobs_dir(root) / job_id
    d.mkdir(parents=True)
    (d / "job.json").write_text(json.dumps({
        "id": job_id, "kind": "tune", "status": status, "created_at": jobs._now(), "started_at": jobs._now(),
        "finished_at": None, "host": socket.gethostname(), "workdir": str(d / "work"),
        "options": {"depth": "ft", "backend": "flyte"}, "error": None,
    }))
    (d / "pid").write_text(f"{pid}\n")
    (d / jobs.LOG).write_text("training step 1/3\n")
    (d / "work" / "runs" / "abc_t1_0").mkdir(parents=True)
    (d / "work" / "runs" / "stage_ft").mkdir()
    return d


def test_running_job_progress_and_cancel(project):
    proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
    try:
        fake_job(project, "aa11", proc.pid)
        job = jobs.get(project, "aa")
        assert job["status"] == "running" and job["trials"] == 1

        r = invoke("tune", "show", "aa11")
        assert "running" in r.output and "1 so far" in r.output and "training step 1/3" in r.output
        r = invoke("tune", "fetch", "aa11")
        assert r.exit_code == 1 and "--wait" in r.output

        r = invoke("tune", "cancel", "aa11")
        assert r.exit_code == 0 and "cancelled" in r.output, r.output
        assert proc.wait(timeout=10) != 0
        r = invoke("tune", "cancel", "aa11")
        assert r.exit_code == 1 and "already cancelled" in r.output
    finally:
        proc.kill()


def test_lost_process_is_failed(project):
    proc = subprocess.Popen([sys.executable, "-c", "pass"])
    proc.wait()
    fake_job(project, "bb22", proc.pid)
    job = jobs.get(project, "bb22")
    assert job["status"] == "failed" and "exited without reporting" in job["error"]


def test_ambiguous_prefix(project):
    fake_job(project, "cc01", 0, status="succeeded")
    time.sleep(0.01)
    fake_job(project, "cc02", 0, status="succeeded")
    r = invoke("tune", "show", "cc")
    assert r.exit_code == 1 and "ambiguous" in r.output and "hlock tune list" in r.output
    assert jobs.get(project, "cc02")["id"] == "cc02"
