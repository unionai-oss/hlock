"""Moving to a new model: `hlock upgrade --to MODEL --retune` lints the lock's text for the
model, sweeps its effort, deletes stale lines with ablate, and writes a tune lock with the
evidence, leaving harness.lock alone."""

from __future__ import annotations

import json

from click.testing import CliRunner
from test_ablate import STALE
from test_optimize import project, tune_lock  # noqa: F401 - the fixture

import hlock
from hlock.cli import main

FABLE = "anthropic:claude-fable-5-2"


def _locked(path) -> bytes:
    (path / "AGENTS.md").write_text(STALE)
    r = CliRunner().invoke(main, ["resolve"])
    assert r.exit_code == 0, r.output
    return (path / "harness.lock").read_bytes()


def _run(*args: str):
    return CliRunner().invoke(main, ["upgrade", *args])


def test_retune_moves_the_lock_to_the_model_in_a_tune_lock(project, monkeypatch):  # noqa: F811
    path, endpoint = project
    before = _locked(path)
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("COLUMNS", "200")
    r = _run("--to", FABLE, "--retune", "--backend", "local")
    assert r.exit_code == 0, r.output
    out = " ".join(r.output.split())
    assert (path / "harness.lock").read_bytes() == before, "harness.lock isn't touched"
    assert "lint: 3 lines in the lock's text to review for anthropic:claude-fable-5-2" in out
    assert "AGENTS.md:2 Think step by step visible-reasoning" in out
    assert "✓ wrote harness.tune.lock (harness.lock is unchanged)" in out
    assert "⚙ upgrade · planner reference:qwen-7b → anthropic:claude-fable-5-2" in out
    assert "ablate on local 1 edit accepted" in out
    assert "effort high anthropic:claude-fable-5-2 high (set)" in out
    assert "Deleted 2 lines" in out and "AGENTS.md:3 Always delegate research to subagents. standing-delegation" in out
    assert "next → hlock diff harness.lock harness.tune.lock · hlock tune apply" in out

    d = tune_lock(path)
    mg = d["tune"]["migrate"]
    assert (mg["from"], mg["to"], mg["role"]) == ("reference:qwen-7b", FABLE, "planner")
    assert [e["level"] for e in mg["effort"]] == ["medium", "high", "xhigh"]
    assert (mg["efforts-from"], mg["effort-set"]) == ("default", "high")
    assert mg["lint"]["warnings"] == 1 and mg["lint"]["rules"]["visible-reasoning"] == 1
    # the lock as it was passes (it says "Check your result"); so does the migrated stack,
    # without the two lines it doesn't need
    assert mg["before"]["success-rate"] == mg["after"]["success-rate"] == 1.0
    assert [(x["line"], x["rule"]) for x in mg["deleted"]] == [(2, "visible-reasoning"), (3, "standing-delegation")]
    llm = next(x for x in d["llm"] if x["role"] == "planner")
    assert (llm["provider"], llm["model"], llm["effort"]) == ("anthropic", "claude-fable-5-2", "high")
    edit = d["tune"]["edit"][0]
    assert (path / edit["path"]).read_text() == "Be careful.\nAlways Check your result.\n"
    # the trials asked the new model at each level, and the lock's at none
    asked = {(b["model"], b.get("reasoning_effort")) for b in endpoint.requests}
    assert {("claude-fable-5-2", e) for e in ("medium", "high", "xhigh")} <= asked
    assert ("qwen-7b", None) in asked

    # hlock diff shows the move, the effort, and the text (the lock didn't list AGENTS.md,
    # so the tuned file is an addition)
    r = CliRunner().invoke(main, ["diff", "harness.lock", "harness.tune.lock"])
    assert r.exit_code == 0, r.output
    assert "~ llm planner: reference:qwen-7b → anthropic:claude-fable-5-2" in r.output
    assert "~ effort planner: unset → high" in r.output
    assert "+ instructions AGENTS.md" in r.output and "+Always Check your result." in r.output


def test_offline_lints_and_sweeps_but_does_not_delete(project, monkeypatch):  # noqa: F811
    path, _ = project
    _locked(path)
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("COLUMNS", "200")
    r = _run("--to", FABLE, "--retune")
    assert r.exit_code == 0, r.output
    out = " ".join(r.output.split())
    assert "ablate didn't run on offline, so no line was tried" in out
    assert "optimize ✗ not on offline" in out
    assert tune_lock(path)["tune"]["migrate"]["deleted"] == []
    # without --to, it moves to the model upgrade plans, keeping the lock's provider
    r = _run("--retune", "--out", "next.tune.lock")
    assert r.exit_code == 0, r.output
    assert "next → hlock diff harness.lock next.tune.lock · hlock tune apply next.tune.lock" in " ".join(r.output.split())
    d = json.loads(hlock.Lockfile.load(str(path / "next.tune.lock")).to_json())
    assert d["tune"]["migrate"]["to"] == "reference:qwen-32b"
    # and the Python API is tune's, with `to`
    report = json.loads(hlock.tune(to="claude-fable-5-2", backend="offline", out="py.tune.lock"))["report"]
    assert report["migrate"]["to"] == "reference:claude-fable-5-2"
    assert report["migrate"]["lint"]["findings"][0]["rule"] == "visible-reasoning"


def test_retune_refuses_what_it_does_not_do(project):  # noqa: F811
    path, _ = project
    r = _run("--to", FABLE, "--retune")
    assert r.exit_code != 0 and "there's no harness.lock to move to a new model" in r.output
    _locked(path)
    r = _run("--to", FABLE, "--retune", "--apply")
    assert r.exit_code != 0 and "--retune doesn't go with" in r.output
    r = _run("--to", FABLE, "--backend", "local")
    assert r.exit_code != 0 and "go with --retune" in r.output
    r = _run("--to", "reference:qwen-7b", "--retune")
    assert r.exit_code != 0 and "already runs reference:qwen-7b" in r.output
    assert not (path / "harness.tune.lock").exists()
