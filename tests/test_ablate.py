"""The ablate optimizer: deleting what `hlock lint` flags, one finding at a time, and the
`no-worse` acceptance check that takes a smaller text that holds."""

from __future__ import annotations

import json

import pytest
from click.testing import CliRunner
from test_optimize import project, tune_lock  # noqa: F401 - the fixture

import hlock
from hlock.cli import main
from hlock.optimizers import load
from hlock.optimizers.ablate import AblateOptimizer, delete, findings

# The endpoint answers OK only when the system prompt says "Check your result": the line
# that carries it is flagged too, and deleting it is the one deletion that hurts.
STALE = (
    "Be careful.\n"
    "Think step by step and show your reasoning.\n"
    "Always delegate research to subagents.\n"
    "Always Check your result.\n"
)


def _setup(path, options: str = 'model = "claude-fable-5-2"', accept: str | None = None) -> None:
    (path / "AGENTS.md").write_text(STALE)
    toml = (path / "harness.toml").read_text()
    if accept:
        toml = toml.replace("[tune]\n", f'[tune]\naccept = "{accept}"\n', 1)
    (path / "harness.toml").write_text(toml + f"\n[optimizers.ablate]\noptions = {{ {options} }}\n")


def _tune() -> dict:
    return json.loads(hlock.tune(backend="local", depth="prompt", max_trials=2, optimizer="ablate"))["report"]


def test_findings_come_warnings_first_and_delete_whole_lines(tmp_path):
    (tmp_path / "instructions").mkdir()
    (tmp_path / "instructions" / "AGENTS.md").write_text(STALE)
    comps = [{"id": "instructions/AGENTS.md", "kind": "instructions"}]
    fs = findings(tmp_path, comps, "claude-fable-5-2")
    assert [(f["severity"], f["rule"], f["line"]) for f in fs] == [
        ("warning", "visible-reasoning", 2), ("info", "standing-delegation", 3), ("info", "unscoped-absolute", 4)]
    assert [f["rule"] for f in findings(tmp_path, comps, "claude-fable-5-2", ("info",))] == [
        "standing-delegation", "unscoped-absolute"]
    assert [f["rule"] for f in findings(tmp_path, comps, "claude-fable-5-2", rules={"unscoped-absolute"})] == [
        "unscoped-absolute"]
    after, gone = delete(STALE, fs[0])
    assert gone == "Think step by step and show your reasoning.\n"
    assert after == "Be careful.\nAlways delegate research to subagents.\nAlways Check your result.\n"
    # offsets are bytes
    text = "Café — ok.\nThink step by step and show your reasoning.\n"
    f = next(f for f in json.loads(hlock.lint_text(text, model="claude-fable-5-2")) if f["rule"] == "visible-reasoning")
    assert delete(text, f) == ("Café — ok.\n", "Think step by step and show your reasoning.\n")


def test_ablate_is_built_in():
    assert isinstance(load("ablate"), AblateOptimizer)


def test_ablate_deletes_what_does_not_hurt_and_hlock_accepts_it_as_no_worse(project, monkeypatch):  # noqa: F811
    path, _ = project
    _setup(path)
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("COLUMNS", "200")
    r = CliRunner().invoke(main, ["tune", "--backend", "local", "--depth", "prompt", "--max-trials", "2",
                                  "--optimizer", "ablate"])
    assert r.exit_code == 0, r.output
    out = " ".join(r.output.split())
    assert "ablate on local 1 edit accepted holdout: 100% → 100% on" in out
    assert "p(worse) = 1.0, text 121 → 38 bytes" in out
    d = tune_lock(path)
    acc = d["tune"]["acceptance"]
    assert acc["accepted"] is True and acc["mode"] == "no-worse"
    assert acc["test"]["base"] == acc["test"]["candidate"] == 1.0 and acc["test"]["worse"] == 0
    assert acc["size-bytes"]["candidate"] < acc["size-bytes"]["base"]
    edit = d["tune"]["edit"][0]
    assert edit["component"] == "instructions/AGENTS.md"
    assert ("deleted 2 lines hlock lint flagged for claude-fable-5-2 (visible-reasoning, standing-delegation)"
            in edit["rationale"])
    # the line the model needs stayed; the two it doesn't went
    assert (path / edit["path"]).read_text() == "Be careful.\nAlways Check your result.\n"
    opt = d["tune"]["optimizer"]
    assert opt["name"] == "ablate" and opt["proposer"] == "hlock lint"


def test_the_default_gate_still_needs_a_gain(project):  # noqa: F811
    path, _ = project
    _setup(path, 'model = "claude-fable-5-2", severities = ["warning"]', accept="better")
    report = _tune()
    assert {"stage": "optimize", "ran": True, "detail": "no edits accepted"} in report["stages"]
    acc = tune_lock(path)["tune"]["acceptance"]
    assert acc["accepted"] is False and "isn't above the noise band" in acc["reason"] and "mode" not in acc


def test_an_unknown_accept_mode_or_severity_is_an_error(project):  # noqa: F811
    path, _ = project
    original = (path / "harness.toml").read_text()
    _setup(path, accept="maybe")
    with pytest.raises(Exception, match=r"\[tune\] accept"):
        _tune()
    (path / "harness.toml").write_text(original)
    _setup(path, 'severities = ["error"]')
    report = _tune()
    assert any("isn't a lint severity" in n for n in report["notes"])
