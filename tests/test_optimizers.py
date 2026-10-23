"""Optimizers apart from backends (specs/optimizers.md): the `hlock.optimizers` API, finding
an optimizer by name, and an optimizer in `[optimizers.<name>]` searching on a backend."""

import json
import os
import sys
from pathlib import Path

import pytest
from click.testing import CliRunner
from test_optimize import project, tune_lock  # noqa: F401 - the fixture

import hlock
from hlock.backends.flyte import FlyteBackend
from hlock.cli import main
from hlock.optimizers import Optimizer, OptimizerError, load, run
from hlock.optimizers.harness2 import Harness2Optimizer
from hlock.optimizers.hlock import HlockOptimizer

# A command optimizer: it adds a line of guidance, evaluates it through the session, and
# says what it got, in the backend contract's envelope.
COMMAND = '''
import json, shutil, sys
from pathlib import Path
from hlock.optimize import Session

req = json.loads(Path(sys.argv[-2]).read_text())
opt, ws = req["optimize"], Path(req["workdir"])
cand = ws / "best"
shutil.copytree(opt["seed"], cand)
f = cand / "instructions" / "AGENTS.md"
f.write_text(f.read_text() + req["optimizer"]["options"]["line"] + "\\n")
Session(opt["session"]).evaluate(cand, "train")
(ws / "ledger.jsonl").write_text(json.dumps({"component": "instructions/AGENTS.md", "op": "modify", "rationale": "cmd"}) + "\\n")
Path(sys.argv[-1]).write_text(json.dumps({"version": req["version"], "ok": True, "result": {
    "candidate": str(cand), "ledger": str(ws / "ledger.jsonl"), "optimizer": {"name": "cmd", "version": "2.0"},
    "seen": {"backend": req["backend"], "optimizer": req["optimizer"]["name"]}}}))
'''


def test_load_finds_built_ins_objects_and_says_what_is_missing():
    assert isinstance(load("hlock"), HlockOptimizer)
    assert isinstance(load("harness2"), Harness2Optimizer)
    assert load("x", "hlock.optimizers.hlock:HlockOptimizer").name == "hlock"
    with pytest.raises(OptimizerError, match="no optimizer named 'nowhere': not built in, not in \\[optimizers\\]"):
        load("nowhere")
    with pytest.raises(OptimizerError, match="isn't an optimizer: it has no optimize"):
        load("x", "json:dumps")
    with pytest.raises(NotImplementedError):
        Optimizer().optimize({})


def test_run_runs_a_command_optimizer_and_reports_its_failure(tmp_path):
    ok = tmp_path / "ok.py"
    ok.write_text("import json, sys\nfrom pathlib import Path\n"
                  "req = json.loads(Path(sys.argv[-2]).read_text())\n"
                  "Path(sys.argv[-1]).write_text(json.dumps({'ok': True, 'result': {'got': req['optimizer']['options']}}))\n")
    req = {"kind": "harness.optimize", "optimizer": {"name": "x", "command": [sys.executable, str(ok)], "options": {"k": 1}}}
    assert run(req) == {"got": {"k": 1}}
    bad = tmp_path / "bad.py"
    bad.write_text("import json, sys\nfrom pathlib import Path\n"
                   "Path(sys.argv[-1]).write_text(json.dumps({'ok': False, 'error': 'no luck'}))\n")
    with pytest.raises(OptimizerError, match="no luck"):
        run({"optimizer": {"name": "x", "command": [sys.executable, str(bad)]}})
    with pytest.raises(OptimizerError, match="wrote no response"):
        run({"optimizer": {"name": "x", "command": [sys.executable, "-c", "pass"]}})


def test_flyte_runs_trials_but_does_not_host_the_search():
    kinds = FlyteBackend().describe({})["kinds"]
    assert "llm.run_trial" in kinds and "harness.optimize" not in kinds


def test_optimizers_are_resolved_apart_from_backends(tmp_path):
    m = tmp_path / "harness.toml"
    m.write_text('[tune]\noptimizer = "harness2"\n\n[optimizers.harness2]\noptions = { k = 2 }\n\n'
                 '[optimizers.gepa]\npython = "hlock_gepa:Gepa"\n\n[optimizers.ace]\ncommand = ["ace", "--fast"]\n\n'
                 '[backends.ray]\ncommand = "ray-backend"\n')
    h2 = json.loads(hlock.optimizer_describe(str(m)))
    assert (h2["name"], h2["source"], h2["options"]) == ("harness2", "built in", {"k": 2})
    gepa = json.loads(hlock.optimizer_describe(str(m), optimizer="gepa"))
    assert (gepa["object"], gepa["source"]) == ("hlock_gepa:Gepa", "Python object hlock_gepa:Gepa")
    ace = json.loads(hlock.optimizer_describe(str(m), optimizer="ace"))
    assert ace["command"] == ["ace", "--fast"] and ace["object"] is None
    other = json.loads(hlock.optimizer_describe(str(m), optimizer="mine"))
    assert other["source"] == "looked up as 'mine' in the hlock.optimizers entry points"
    with pytest.raises(ValueError, match=r"names a backend: optimizers are declared under \[optimizers.ray\]"):
        hlock.optimizer_describe(str(m), optimizer="ray")
    m.write_text('[optimizers.hlock]\npython = "a:b"\n')
    with pytest.raises(ValueError, match=r"\[optimizers.hlock\] can only set options: hlock is built in"):
        hlock.optimizer_describe(str(m), optimizer="hlock")


def test_a_command_optimizer_searches_on_local_and_tune_records_both(project, monkeypatch):  # noqa: F811
    path, _ = project
    (path / "cmd_opt.py").write_text(COMMAND)
    monkeypatch.setenv("PYTHONPATH", os.pathsep.join([str(Path(hlock.__file__).parents[1]), os.environ.get("PYTHONPATH", "")]))
    with open(path / "harness.toml", "a") as f:
        f.write(f'\n[optimizers.cmd]\ncommand = [{json.dumps(sys.executable)}, "cmd_opt.py"]\n'
                'options = { line = "Check your result before you finish." }\n')
    report = json.loads(hlock.tune(backend="local", depth="prompt", max_trials=2, optimizer="cmd"))["report"]
    assert {"stage": "optimize", "ran": True, "detail": "1 edit accepted"} in report["stages"], report
    opt = tune_lock(path)["tune"]["optimizer"]
    assert (opt["name"], opt["version"], opt["backend"], opt["evaluator"]) == ("cmd", "2.0", "local", "hlock")


def test_the_optimizer_commands(project, monkeypatch):  # noqa: F811
    monkeypatch.setenv("NO_COLOR", "1")
    flat = lambda r: " ".join(r.output.split())
    r = CliRunner().invoke(main, ["optimizer", "list"])
    assert r.exit_code == 0, r.output
    assert "hlock built in" in flat(r) and "harness2 built in" in flat(r)
    r = CliRunner().invoke(main, ["optimizer", "status"])
    assert r.exit_code == 0, r.output
    assert "optimizer hlock (from harness.toml)" in flat(r) and "about hlock's built-in reflective loop" in flat(r)
    r = CliRunner().invoke(main, ["optimizer", "status", "nowhere"])
    assert "no optimizer named 'nowhere'" in flat(r)
    assert "harness2" not in CliRunner().invoke(main, ["backend", "list"]).output
