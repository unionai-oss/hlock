"""Tests for the Rust <-> Python worker bridge (harness_lock.tasks.worker).

These prove the seam ``hlock tune --backend devbox|cluster`` uses: the Rust core
shells a trial out to the Python worker over a versioned JSON contract and
consumes the response. They run entirely offline — the worker's LLM call falls
back to the deterministic reference RNG when no ``HL_LLM_BASE_URL`` is set, and
the Flyte lift degrades to in-process when the ``flyte`` SDK is absent.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[1]


def run_worker(req: dict) -> dict:
    """Invoke the worker exactly as the Rust core does."""
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        rq_path = td / "_al_request.json"
        rs_path = td / "_al_response.json"
        rq_path.write_text(json.dumps(req))
        proc = subprocess.run(
            [sys.executable, "-m", "harness_lock.tasks.worker", str(rq_path), str(rs_path)],
            capture_output=True,
            text=True,
            check=False,
        )
        if rs_path.exists():
            return json.loads(rs_path.read_text())
        assert proc.returncode == 0, proc.stderr
        raise AssertionError("worker wrote no response file")


def test_worker_contract_llm_trial():
    resp = run_worker(
        {
            "version": 1,
            "kind": "llm.run_trial",
            "backend": "local",
            "stack": {"llm": {"planner": "reference:qwen-7b"}, "retries": 0, "max_steps": 4},
            "task": {"id": "t1", "prompt_text": "Do the thing"},
            "seed": 0,
            "workdir": ".",
            "plan": {},
        }
    )
    assert resp["ok"] is True, resp
    r = resp["result"]
    assert "success" in r and "score" in r
    assert isinstance(r["success"], bool)
    assert isinstance(r["cost_usd"], float)
    assert isinstance(r["steps"], int)
    assert r["trace"]  # at least one llm.call event


def test_worker_contract_unknown_kind():
    resp = run_worker({"version": 1, "kind": "nope", "backend": "local"})
    assert resp["ok"] is False
    assert "unknown task kind" in resp["error"]


def test_worker_contract_bad_version():
    resp = run_worker({"version": 99, "kind": "llm.run_trial", "backend": "local"})
    assert resp["ok"] is False


@pytest.mark.parametrize(
    "kind,key",
    [
        ("data.load", "rows"),
        ("data.synthetic", "variants"),
        ("grade", "missing"),
        ("prompt.search", "available"),
        ("ft.lora", "available"),
    ],
)
def test_worker_contract_handlers(kind, key, tmp_path):
    resp = run_worker(
        {
            "version": 1,
            "kind": kind,
            "backend": "local",
            "data": {"source": "synthetic:3"},
            "task": {"id": "t", "expected": []},
            "stack": {"llm": {"planner": "reference:qwen-7b"}},
            "seed": 0,
            "workdir": str(tmp_path),
            "plan": {},
        }
    )
    assert resp["ok"] is True, resp
    assert key in resp["result"]["model_digests"]


def test_tune_backend_devbox(tmp_path, monkeypatch):
    """End-to-end: Rust core -> Python worker -> harness.tune.lock, offline."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)  # pin the interpreter the worker runs under

    manifest = f"""[project]
name = "worker-e2e"

[tasks]
packs = ["{REPO / 'tasks'}"]
seeds = 1

[candidates.llm.planner]
any_of = ["reference:qwen-7b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"

[budget]
strategy = "grid"
max_trials = 1
"""
    (tmp_path / "harness.toml").write_text(manifest)

    import harness_lock as hlock

    out = json.loads(hlock.tune(backend="devbox", depth="probe", data="synthetic:1", max_trials=1))
    report = out["report"]
    assert report["backend"] == "devbox"
    assert (tmp_path / "harness.tune.lock").exists()
    assert report["best"]["trials"] == 1  # evidence.trials follows the winner's real trial count

    lf = hlock.Lockfile.load(str(tmp_path / "harness.tune.lock"))
    assert json.loads(lf.to_json())["kind"] == "tuned"


def test_worker_contract_prompt_search_digests():
    """prompt.search must produce a real selected prompt + before/after digests."""
    resp = run_worker(
        {
            "version": 1,
            "kind": "prompt.search",
            "backend": "local",
            "task": {"id": "t", "prompt_text": "write a summary"},
            "seed": 0,
            "workdir": ".",
            "plan": {},
        }
    )
    assert resp["ok"] is True, resp
    md = resp["result"]["model_digests"]
    assert md["available"] is True
    assert md["best_prompt"] and md["best_prompt"].startswith("Be concise")  # chosen instruction
    assert str(md["before_digest"]).startswith("sha256:")
    assert str(md["after_digest"]).startswith("sha256:")
    assert md["after_digest"] != md["before_digest"]  # real (changed) prompt


def test_worker_contract_data_load_split():
    """data.load normalizes traces into a content-hashed train/val/test split."""
    resp = run_worker(
        {
            "version": 1,
            "kind": "data.load",
            "backend": "local",
            "data": {"source": "user"},
            "seed": 0,
            "workdir": "/tmp/hl_dataload_test",
            "plan": {},
        }
    )
    # no trace files -> empty but well-formed split
    assert resp["ok"] is True, resp
    md = resp["result"]["model_digests"]
    assert "rows" in md and "digest" in md and "split" in md
    assert set(md["split"]) == {"train", "val", "test"}


def test_tune_depth_prompt_devbox(tmp_path, monkeypatch):
    """depth=prompt drives the data + prompt.search stages, writing real digests."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    monkeypatch.setenv("HL_LLM_BASE_URL", "")  # ensure deterministic reference path

    manifest = f"""[project]
name = "prompt-e2e"

[tasks]
packs = ["{REPO / 'tasks'}"]
seeds = 1

[candidates.llm.planner]
any_of = ["reference:qwen-7b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"

[budget]
strategy = "grid"
max_trials = 2
"""
    (tmp_path / "harness.toml").write_text(manifest)

    import harness_lock as hlock

    json.loads(hlock.tune(backend="devbox", depth="prompt", data="synthetic:2", max_trials=2))
    d = json.loads(hlock.Lockfile.load(str(tmp_path / "harness.tune.lock")).to_json())
    applied = d["tune"]["prompt"]["applied"]
    assert applied and applied[0]["_placeholder"] is False  # real prompt search ran
    after = applied[0]["after"]
    assert after.startswith("sha256:") and after != applied[0]["before"]
    assert d["tune"]["data"]["mode"] == "synthetic"
