"""Decision models end to end: `hlock decide`, deciders in pinned locks and the hooks
`sync` writes, `check`, `localize`, decision-model judges in `evals`, and drafting."""

import json
import os
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner
from fake_models import Fake, systemone

from harness_lock import Lockfile
from harness_lock.cli import main


def invoke(*args, input=None):
    return CliRunner().invoke(main, list(args), input=input)


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def fake():
    servers = []

    def start(answer):
        f = Fake(answer)
        servers.append(f)
        return f

    yield start
    for s in servers:
        s.close()


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_",
                           "TYPESAFE_")):
            monkeypatch.delenv(key)
    home, proj, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, proj, bindir):
        d.mkdir()
    executable(bindir / "claude", '#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    executable(bindir / "hermes", "#!/bin/sh\necho 'Hermes Agent v0.9.0'\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HLOCK_JUDGE_AUTO", "0")
    monkeypatch.setenv("HLOCK_BIN", "hlock")
    monkeypatch.chdir(proj)
    (home / ".claude").mkdir()
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    return {"home": home, "project": proj}


def lock_json(path="harness.lock") -> dict:
    return json.loads(Lockfile.load(path).to_json())


def test_decide_ask(project, fake):
    f = fake(lambda path, body: systemone(body, noul=0.81, choice="billing"))
    Path("harness.toml").write_text(f'[deciders.default]\nmodel = "system-one:kev-4b"\nbase-url = "{f.url}"\n')
    r = invoke("decide", "ask", "My payouts failed for 3 days", "--noul", "Is it urgent?",
               "--choice", "team=billing,auth", "--score", "severity=cosmetic|broken|outage")
    assert r.exit_code == 0, r.output
    assert "system-one:kev-4b-q8" in r.output and "42 input tokens" in r.output
    assert "Is it urgent?  yes  P(yes) 0.81" in r.output
    assert "team  billing  P 0.95" in r.output and "severity  outage  level 2.00 of 2" in r.output
    path, body = f.requests[0]
    assert path == "/v1/systemone" and body["state"] == "My payouts failed for 3 days"
    assert body["questions"]["q2"]["criteria"] == {"billing": "billing", "auth": "auth"}

    r = invoke("decide", "ask", "x", "--noul", "y", "--json")
    assert json.loads(r.output)["decided"]["answers"][0]["value"] == 0.81
    assert "needs options" in invoke("decide", "ask", "x", "--choice", "team").output
    os.remove("harness.toml")
    r = invoke("decide", "ask", "x", "--noul", "y")
    assert r.exit_code == 1 and "no decision model" in r.output


def write_deciders(url: str) -> None:
    Path(".hlock").mkdir(exist_ok=True)
    Path(".hlock/approve.md").write_text("Never push to main.\n")
    Path("harness.toml").write_text(
        f'[deciders.approve]\nmodel = "system-one:kev-4b"\nbase-url = "{url}"\npolicy = ".hlock/approve.md"\n\n'
        f'[deciders.stop]\nmodel = "system-one:kev-4b"\nbase-url = "{url}"\nmin-probability = 0.8\n')


ENV_SETTINGS = Path(".hlock/env/claude-code/config/settings.json")


def test_pin_locks_deciders_and_sync_wires_hooks(project, fake):
    f = fake(lambda path, body: systemone(body, choice="deny", p=0.97))
    write_deciders(f.url)
    # the user's own hook is part of their setup, and pinned with it
    lint = {"matcher": "Bash", "hooks": [{"type": "command", "command": "./lint.sh"}]}
    (project["home"] / ".claude/settings.json").write_text(json.dumps({"model": "opus", "hooks": {"PreToolUse": [lint]}}))
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0, r.output
    assert "decider · approve" in r.output and "hook: PermissionRequest" in r.output
    deciders = {d["use"]: d for d in lock_json()["decider"]}
    assert deciders["approve"]["policy"]["path"] == ".hlock/approve.md"
    assert deciders["stop"]["wiring"] == [{"file": "settings", "event": "Stop"}]

    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    settings = json.loads(ENV_SETTINGS.read_text())
    lock = str(Path("harness.lock").resolve())
    assert settings["model"] == "opus"
    assert settings["hooks"]["PermissionRequest"][0]["hooks"][0]["command"] == \
        f"hlock decide hook approve --format claude-code --lock {lock}"
    assert settings["hooks"]["Stop"][0]["hooks"][0]["command"].startswith("hlock decide hook stop")

    # a second sync has nothing to do, and the isolated root matches the lock
    plan = json.loads(invoke("sync", "--plan", "--json").output)
    steps = {s["id"]: s["status"] for s in plan["steps"]}
    assert steps["decider:settings"] == "satisfied" and steps["config:settings"] == "satisfied", steps
    r = invoke("check", "--prefix", ".hlock/env")
    assert r.exit_code == 0, r.output

    # the hook runs the locked decider
    event = json.dumps({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "git push -f"}})
    r = invoke("decide", "hook", "approve", "--format", "claude-code", "--lock", lock, input=event)
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["hookSpecificOutput"]["decision"]["behavior"] == "deny"
    assert "Never push to main." in f.requests[-1][1]["questions"]["approve"]["instructions"]

    assert settings["hooks"]["PreToolUse"] == [lint]

    # dropping a decider removes its hook, and leaves the user's own hooks alone
    Path("harness.toml").write_text(Path("harness.toml").read_text().split("[deciders.stop]")[0])
    assert invoke("pin", "--harness", "claude").exit_code == 0
    assert invoke("sync", "--yes").exit_code == 0
    hooks = json.loads(ENV_SETTINGS.read_text())["hooks"]
    assert "Stop" not in hooks and hooks["PermissionRequest"] and hooks["PreToolUse"] == [lint]


def test_check_reports_a_changed_policy(project, fake):
    f = fake(lambda path, body: systemone(body))
    write_deciders(f.url)
    assert invoke("pin", "--harness", "claude").exit_code == 0
    assert invoke("check").exit_code == 0
    Path(".hlock/approve.md").write_text("Never push anywhere.\n")
    r = invoke("check")
    assert r.exit_code == 1 and "decider approve: policy .hlock/approve.md changed" in r.output


def test_hook_defers_without_a_lock_or_a_model(project):
    # no decision: exit 0 and print nothing (older click mixes -v's stderr into the output)
    r = invoke("decide", "hook", "stop", "--format", "codex", "--lock", "nope.lock", input="{}")
    assert r.exit_code == 0 and r.output == ""


def test_pin_records_hermes_native_approvals(project):
    home = project["home"]
    (home / ".hermes").mkdir()
    (home / ".hermes/config.yaml").write_text(
        "model:\n  default: qwen3\n  provider: ollama\nauxiliary:\n  approval:\n    provider: typesafe-jev\n"
        "    model: jev-latest\napprovals:\n  mode: smart\n")
    r = invoke("pin", "--harness", "hermes")
    assert r.exit_code == 0, r.output
    (d,) = lock_json()["decider"]
    assert (d["use"], d["via"], d["provider"], d["model"]) == ("approve", "native", "typesafe-jev", "jev-latest")
    assert d["resolved-from"] == "config#/auxiliary/approval"


def test_localize_moves_hosted_deciders_to_kev(project, fake):
    Path("harness.toml").write_text('[deciders.approve]\nmodel = "typesafe:jev-1.13.0"\n')
    executable(project["home"].parent / "bin" / "ollama", "#!/bin/sh\necho 'ollama version is 0.40.4'\n")
    assert invoke("pin", "--harness", "claude").exit_code == 0
    assert lock_json()["decider"][0]["key-env"] == "TYPESAFE_API_KEY"
    r = invoke("localize", "--memory-gb", "8", "--harness", "claude")
    assert r.exit_code == 0, r.output
    (d,) = lock_json("harness.local.lock")["decider"]
    assert (d["provider"], d["model"], d["runtime"]) == ("ollama", "kev:4b", "ollama")
    assert "key-env" not in d


PACK = """[pack]
name = "t/judged"
version = "0.1.0"
domain = "writing"

[pack.judge]
model = "system-one:kev-4b"
base-url = "{url}"

[[task]]
id = "summary"
prompt = "summary/prompt.md"
fixtures = "summary/workspace/"
solution = "summary/solution/"
expected = {{ artifacts = ["summary.md"] }}
graders = [{{ type = "judge", criteria = ["Names the revenue figure", "Is one paragraph"] }}]
"""


def judged_pack(url: str) -> None:
    for rel, text in {
        "evals/pack.toml": PACK.format(url=url),
        "evals/summary/prompt.md": "Summarize report.txt in summary.md.\n",
        "evals/summary/workspace/report.txt": "Revenue: $1.2M.\n",
        "evals/summary/solution/summary.md": "Revenue was $1.2M.\n",
    }.items():
        Path(rel).parent.mkdir(parents=True, exist_ok=True)
        Path(rel).write_text(text)


def by_file(revenue: float, other: float = 0.9):
    """P(met): criterion 1 by whether the summary mentions revenue, criterion 2 `other`."""
    def noul(qid, q, body):
        if qid == "c2":
            return other
        return revenue if "Revenue" in (body["state"]["files"].get("summary.md") or "") else 0.05
    return noul


def test_evals_grade_shows_each_criterion(project, fake):
    f = fake(lambda path, body: systemone(body, noul=by_file(0.97)))
    judged_pack(f.url)
    r = invoke("evals", "grade", "summary")
    assert r.exit_code == 0, r.output
    assert "✓ judge:2 criteria (system-one:kev-4b)  0.94" in r.output
    assert "0.97  Names the revenue figure" in r.output and "0.90  Is one paragraph" in r.output
    run = Path("run")
    run.mkdir()
    (run / "summary.md").write_text("It went fine.\n")
    r = invoke("evals", "grade", "summary", "run", "--json")
    assert r.exit_code == 1
    judge = next(g for g in json.loads(r.output)["grades"] if g["grader"].startswith("judge:"))
    assert judge["detail"].startswith("unmet: Names the revenue figure (0.05)") and judge["judged_by"] == "kev-4b-q8"


def test_check_run_runs_decider_judges(project, fake):
    f = fake(lambda path, body: systemone(body, noul=by_file(0.66)))
    judged_pack(f.url)
    r = invoke("evals", "check", "--run")
    assert "judge system-one:kev-4b" in r.output
    assert 'summary  judge:2 criteria (system-one:kev-4b) is uncertain on its solution: "Names the revenue figure" P=0.66' in r.output
    assert len(f.requests) == 2  # the fixtures alone, and the solution


def test_calibrate_against_an_llm(project, fake):
    decider = fake(lambda path, body: systemone(body, noul=by_file(0.97)))
    llm = fake(lambda path, body: {"choices": [{"message": {"content": '{"criteria": [true, false], "reason": "x"}'}}]})
    judged_pack(decider.url)
    r = invoke("evals", "calibrate", "--against", "local:tiny", "--against-base-url", llm.url)
    assert r.exit_code == 0, r.output
    assert "system-one:kev-4b vs local:tiny" in r.output
    assert "summary  solution  Is one paragraph" in r.output and "differ" in r.output
    data = json.loads(invoke("evals", "calibrate", "--against", "local:tiny", "--against-base-url", llm.url, "--json").output)
    assert 0 < data["agreement"] < 1


def test_drafting_from_traces_with_a_decider(project, fake):
    def answer(path, body):
        unchecked = "how are you" in body["state"]
        return systemone(body, noul=lambda qid, q, b: 0.1 if (qid == "checkable" and unchecked) else
                         0.2 if qid in ("alone", "secret") else 0.9,
                         choice=lambda qid, q, b: "code")
    f = fake(answer)
    Path("harness.toml").write_text(f'[deciders.draft]\nmodel = "system-one:kev-4b"\nbase-url = "{f.url}"\n')
    Path("traces.jsonl").write_text(
        json.dumps({"prompt": "Fix the off-by-one in paginate()"}) + "\n" + json.dumps({"prompt": "how are you"}) + "\n")
    r = invoke("evals", "init", "--from-traces", "traces.jsonl", "--no-skill")
    assert r.exit_code == 0, r.output
    assert "reviewing the requests with system-one:kev-4b" in r.output and "1 draft task" in r.output
    toml = Path("evals/pack.toml").read_text()
    assert 'graders = ["tests:fix_off_by_one_paginate/check.py"]' in toml and '"code"' in toml
    assert Path("evals/fix_off_by_one_paginate/check.py").is_file()
    assert "may lean on earlier context" in Path("evals/fix_off_by_one_paginate/prompt.md").read_text()


def test_codex_hooks_go_in_its_config_toml(project, fake):
    f = fake(lambda path, body: systemone(body))
    home = project["home"]
    executable(home.parent / "bin" / "codex", "#!/bin/sh\necho 'codex-cli 0.163.0'\n")
    (home / ".codex").mkdir()
    (home / ".codex/config.toml").write_text('model = "gpt-5.5"\n')
    Path("harness.toml").write_text(f'[deciders.stop]\nmodel = "system-one:kev-4b"\nbase-url = "{f.url}"\n')
    assert invoke("pin", "--harness", "codex").exit_code == 0
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    text = Path(".hlock/env/codex/config/config.toml").read_text()
    assert 'model = "gpt-5.5"' in text and "[[hooks.Stop]]" in text, text
    assert "decide hook stop --format codex" in text
    plan = json.loads(invoke("sync", "--plan", "--json").output)
    assert {s["id"]: s["status"] for s in plan["steps"]}["decider:config"] == "satisfied"
