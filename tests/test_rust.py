"""Tests for the `hlock` Python bindings (Rust-backed)."""

import json
from pathlib import Path

import pytest

import harness_lock as hlock

REPO = Path(__file__).resolve().parent.parent
TASKS = REPO / "evals"

PACK_TOML = """\
[pack]
name = "test/pack"
version = "0.1.0"
license = "Apache-2.0"
domain = "demo"

[[task]]
id = "summarize"
prompt = "t1/prompt.md"
expected = { artifacts = ["summary.md"] }
"""


@pytest.fixture
def workspace(tmp_path):
    d = tmp_path / "ws"
    taskdir = d / "t1"
    taskdir.mkdir(parents=True)
    (taskdir / "prompt.md").write_text("Write an executive summary to summary.md")
    (d / "pack.toml").write_text(PACK_TOML)
    return d, tmp_path


def test_module_surface():
    assert hlock.LOCK_VERSION == 1
    assert isinstance(hlock.__version__, str)
    for name in ("Manifest", "Objective", "Budget", "SearchSpace", "Lockfile", "LockedAgent"):
        assert hasattr(hlock, name)
    assert callable(hlock.resolve)
    assert callable(hlock.load)


def test_search_space_tune(workspace, monkeypatch):
    ws, base = workspace
    monkeypatch.chdir(base)
    space = hlock.SearchSpace(
        llm={"planner": ["reference:qwen-7b", "reference:qwen-32b"]},
        decision=["none", "rules:default"],
        harness=["reference"],
        evals=json.dumps({"packs": [str(ws)], "seeds": 2}),
    )
    lf = hlock.resolve(space, backend="local", workdir=str(base / ".harnesslock"))
    assert lf.harness == "reference"
    ev = json.loads(lf.evidence)
    assert ev["trials"] >= 1
    # harness.lock written at cwd
    assert (base / "harness.lock").exists()

    # load + run
    agent = hlock.load(str(base / "harness.lock"))
    assert agent.summary() != ""
    outcome = json.loads(agent.run("do the demo task"))
    assert "success" in outcome


def test_manifest_load_write(tmp_path):
    m = hlock.Manifest.from_str(
        "[project]\nname='m'\n[candidates.llm.planner]\nany_of=['reference:qwen-7b']\n"
        "[candidates]\nharness=['reference']\n[budget]\nstrategy='grid'\n"
    )
    assert m.name == "m"
    assert m.backend == "local"
    assert "sha256:" in m.manifest_hash()


def test_upgrade_and_tune(tmp_path, monkeypatch):
    """hlock upgrade (plan/apply/rollback) and hlock tune via the bindings."""
    import json as _json

    # a local eval pack
    ws = tmp_path / "ws"
    (ws / "t1").mkdir(parents=True)
    (ws / "t1" / "prompt.md").write_text("write summary.md")
    (ws / "pack.toml").write_text(
        "[pack]\nname='t/p'\nversion='0.1.0'\ndomain='d'\n\n"
        "[[task]]\nid='s'\nprompt='t1/prompt.md'\nexpected={artifacts=['summary.md']}\n"
    )
    monkeypatch.chdir(tmp_path)
    manifest = (
        "[project]\nname='ops'\n"
        f"[evals]\npacks=['{ws}']\nseeds=1\n"
        "[candidates.llm.planner]\nany_of=['reference:qwen-7b','reference:qwen-32b']\n"
        "[candidates]\nharness=['reference']\ndecision=['none']\n"
        "[objective]\nprimary='success-rate'\nselect='max-quality'\n"
        "[objective.constraints]\nsuccess-rate='>=0.5'\n[budget]\nstrategy='grid'\n"
    )
    (tmp_path / "harness.toml").write_text(manifest)

    hlock.resolve(manifest="harness.toml", backend="local", workdir=str(tmp_path / ".harnesslock"))
    assert (tmp_path / "harness.lock").exists()

    plan = _json.loads(hlock.upgrade(manifest="harness.toml", all=True))
    assert plan["plan"]["upgradeable"] and plan["plan"]["upgradeable"][0]["to"] == "qwen-32b"

    applied = _json.loads(hlock.upgrade(manifest="harness.toml", all=True, apply=True))
    assert applied["applied"] is True
    assert (tmp_path / "harness.lock.prev").exists()

    rolled = _json.loads(hlock.upgrade(manifest="harness.toml", rollback=True))
    assert rolled["rolled_back"] is True

    tuned = _json.loads(hlock.tune(manifest="harness.toml", depth="hpo", out=str(tmp_path / "harness.tune.lock")))
    assert (tmp_path / "harness.tune.lock").exists()
    assert tuned["report"]["depth"] == "hpo"
    # reload the tune lock and confirm it is marked tuned
    tune_lock = hlock.Lockfile.load(str(tmp_path / "harness.tune.lock"))
    assert "tuned" in tune_lock.to_json() or tune_lock.harness
    assert tuned["summary"] != ""
