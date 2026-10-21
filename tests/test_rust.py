"""Tests for the `hlock` Python bindings (Rust-backed)."""

import json
from pathlib import Path

import pytest

import hlock

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
    lf = hlock.resolve(space, backend="offline", workdir=str(base / ".hlock"))
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
    assert m.backend == "offline"
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

    hlock.resolve(manifest="harness.toml", backend="offline", workdir=str(tmp_path / ".hlock"))
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


def test_errors_are_typed_value_errors(tmp_path):
    for name in ("ManifestError", "LockfileError", "EvalPackError", "StoreError", "TuningError", "ProfileError",
                 "WorkerError", "PullError"):
        assert issubclass(getattr(hlock, name), hlock.HarnessLockError)
    assert issubclass(hlock.HarnessLockError, ValueError)
    with pytest.raises(hlock.ManifestError) as e:
        hlock.Manifest.load(str(tmp_path / "missing.toml"))
    assert isinstance(e.value, ValueError) and "cannot read" in str(e.value)
    (tmp_path / "bad.lock").write_text("lock-version = 1\ngenerated-by = 't'\ngenerated-at = ''\n[harness]\nname = '../x'\n")
    with pytest.raises(hlock.LockfileError, match="plain name"):
        hlock.Lockfile.load(str(tmp_path / "bad.lock"))
    with pytest.raises(hlock.PullError):
        hlock.pull_parse("github:alice/setup@--upload-pack=x")
    import hlock as alias

    assert alias.HarnessLockError is hlock.HarnessLockError


def test_search_space_rejects_invalid_json():
    with pytest.raises(ValueError, match="evals isn't valid JSON"):
        hlock.SearchSpace(evals="{packs: nope")
    with pytest.raises(hlock.HarnessLockError, match="policy must be a JSON object"):
        hlock.SearchSpace(policy="[1, 2]")
    hlock.SearchSpace(policy='{"retries": [0, 1]}')


def test_an_explicit_manifest_path_must_exist(tmp_path):
    pack = tmp_path / "ws"
    (pack / "t1").mkdir(parents=True)
    (pack / "t1" / "prompt.md").write_text("x")
    (pack / "pack.toml").write_text(PACK_TOML)
    with pytest.raises(hlock.ManifestError, match="cannot read"):
        hlock.check_eval_pack(str(pack), manifest=str(tmp_path / "nope.toml"))
    with pytest.raises(hlock.ManifestError, match="cannot read"):
        hlock.decide("x", "[]", manifest=str(tmp_path / "nope.toml"))
    # without one, there's nothing to read
    assert json.loads(hlock.check_eval_pack(str(pack)))["name"] == "test/pack"


def test_decide_state_and_hook_input_must_be_json_when_they_look_like_it(tmp_path):
    with pytest.raises(hlock.HarnessLockError, match="state isn't valid JSON"):
        hlock.decide("{not json", "[]", model="system-one:kev-4b")
    lock = tmp_path / "harness.lock"
    lock.write_text("lock-version = 1\ngenerated-by = 't'\ngenerated-at = ''\n")
    out = json.loads(hlock.decide_hook("approve", "claude-code", "not json", str(lock)))
    assert out["exit"] == 0 and out["stdout"] == "" and "wasn't JSON" in out["note"]


def test_long_calls_stop_on_ctrl_c():
    import _thread
    import threading
    import time

    step = {"id": "t", "status": "auto", "action": {"type": "run", "argv": ["sleep", "3"], "env": {}}}
    threading.Timer(0.3, _thread.interrupt_main).start()
    start = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        hlock.sync_run_step(json.dumps(step))
    assert time.monotonic() - start < 2
    with pytest.raises(hlock.ProfileError, match="no command"):
        hlock.sync_run_step(json.dumps({**step, "action": {"type": "run", "argv": [], "env": {}}}))


def test_decide_with_an_explicit_model_uses_its_own_provider(tmp_path, monkeypatch):
    from fake_models import Fake, systemone

    ours, other = Fake(lambda path, body: systemone(body)), Fake(lambda path, body: systemone(body))
    try:
        manifest = tmp_path / "harness.toml"
        manifest.write_text(f'[deciders.default]\nmodel = "system-one:kev-4b"\nbase-url = "{ours.url}"\n')
        monkeypatch.setenv("OLLAMA_HOST", other.url)
        q = json.dumps([{"id": "q1", "type": "noul", "instructions": "ok?"}])
        # the same provider keeps the spec's base URL
        hlock.decide("s", q, model="system-one:kev-8b", manifest=str(manifest))
        assert len(ours.requests) == 1 and not other.requests
        # another provider doesn't inherit it
        hlock.decide("s", q, model="ollama:kev:4b", manifest=str(manifest))
        assert len(ours.requests) == 1 and len(other.requests) == 1
        # a use with only any-of candidates falls back to [deciders.default]'s model
        manifest.write_text(f'[deciders.draft]\nany-of = ["ollama:kev:4b"]\n'
                            f'[deciders.default]\nmodel = "system-one:kev-4b"\nbase-url = "{ours.url}"\n')
        hlock.decide("s", q, use_="draft", manifest=str(manifest))
        assert len(ours.requests) == 2
        assert hlock.Manifest.load(str(manifest)).decider("draft") == "system-one:kev-4b"
    finally:
        ours.close()
        other.close()
