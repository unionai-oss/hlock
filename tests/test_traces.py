"""`hlock evals init --from-traces`: draft an eval pack from agent session logs and trace files."""

import json
import os
import re
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import evals, traces
from hlock.cli import main

GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "HLOCK_")):
            monkeypatch.delenv(key)
    home, proj = tmp_path / "home", tmp_path / "proj"
    home.mkdir()
    proj.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(proj)
    return proj.resolve()


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def write_jsonl(path, entries):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(json.dumps(e) for e in entries) + "\n")


def claude_session(proj, sid, ts, request, writes=()):
    d = os.environ["HOME"] + "/.claude/projects/" + re.sub(r"[^A-Za-z0-9]", "-", str(proj))
    base = {"sessionId": sid, "cwd": str(proj)}
    write_jsonl(Path(d) / f"{sid}.jsonl", [
        {**base, "type": "user", "timestamp": ts, "message": {"role": "user", "content": "<command-name>/clear</command-name>"}},
        {**base, "type": "user", "isMeta": True, "timestamp": ts, "message": {"role": "user", "content": "x" * 40}},
        {**base, "type": "user", "timestamp": ts, "message": {"role": "user", "content": request}},
        {**base, "type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "Write", "input": {"file_path": w, "content": ""}} for w in writes]}},
        {**base, "type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}},
    ])


def test_drafts_a_pack_from_claude_code_sessions(project):
    claude_session(project, "a", "2026-10-01T10:00:00Z",
                   f"Write a weekly summary of data/orders.csv to reports/weekly.md, token {GH_TOKEN}",
                   writes=[str(project / "reports/weekly.md"), "/tmp/elsewhere.txt"])
    claude_session(project, "b", "2026-10-02T10:00:00Z", "Investigate why the nightly import job is slow")
    claude_session(project, "c", "2026-09-30T10:00:00Z", "investigate why the nightly   import job is slow")  # dup
    r = invoke("evals", "init", "--from-traces")
    assert r.exit_code == 0, r.output
    assert "2 draft tasks from 3 claude-code sessions" in r.output
    pack = (project / "evals/pack.toml").read_text()
    # newest first; files the session created inside the project become artifacts
    assert pack.index('id = "investigate_why_nightly_import_job"') < pack.index('id = "write_weekly_summary_data_orders"')
    assert 'expected = { artifacts = ["reports/weekly.md"] }' in pack and "elsewhere" not in pack
    prompt = (project / "evals/write_weekly_summary_data_orders/prompt.md").read_text()
    assert GH_TOKEN not in prompt and "[redacted]" in prompt
    # the task with no created file is a slot to fill, which check reports
    assert pack.count(evals.PLACEHOLDER) == 1
    assert (project / ".agents/skills/hlock-eval-pack/SKILL.md").is_file()
    r = invoke("evals", "check")
    assert r.exit_code == 1 and "1 TODO(hlock)" in r.output


def test_other_projects_sessions_are_left_out(project, tmp_path):
    claude_session(tmp_path / "proj-other", "z", "2026-10-01T10:00:00Z", "Something in a different project")
    r = invoke("evals", "init", "--from-traces")
    assert r.exit_code == 1 and "no requests found" in r.output


def test_reads_pi_and_codex_logs(project, tmp_path):
    write_jsonl(Path(os.environ["HOME"]) / ".pi/agent/sessions/--proj--/s.jsonl", [
        {"type": "session", "cwd": str(project), "timestamp": "2026-10-03T00:00:00Z"},
        {"type": "message", "timestamp": "2026-10-03T00:00:01Z",
         "message": {"role": "user", "content": [{"type": "text", "text": "Add a CLI flag to export results as CSV"}]}},
        {"type": "message", "message": {"role": "assistant", "content": [
            {"type": "toolCall", "name": "write", "arguments": {"path": "export.py", "content": ""}}]}},
    ])
    write_jsonl(Path(os.environ["HOME"]) / ".codex/sessions/2026/10/04/rollout-1.jsonl", [
        {"type": "session_meta", "timestamp": "2026-10-04T00:00:00Z", "payload": {"cwd": str(project)}},
        {"type": "response_item", "timestamp": "2026-10-04T00:00:01Z", "payload": {
            "type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>…</environment_context>"}]}},
        {"type": "response_item", "timestamp": "2026-10-04T00:00:02Z", "payload": {
            "type": "message", "role": "user", "content": [{"type": "input_text", "text": "Document the retry policy in docs/retries.md"}]}},
    ])
    sessions = traces.discover(project)
    assert {s.harness for s in sessions} == {"pi", "codex"}
    pi = next(s for s in sessions if s.harness == "pi")
    assert pi.created == ["export.py"]
    r = invoke("evals", "init", "--from-traces", "--no-skill")
    assert r.exit_code == 0 and "1 codex session, 1 pi session" in r.output, r.output
    assert 'artifacts = ["export.py"]' in (project / "evals/pack.toml").read_text()


def test_generic_jsonl_and_csv(project, tmp_path):
    (tmp_path / "t.jsonl").write_text(json.dumps({"prompt": "Refactor the billing module to the new tax API",
                                                  "artifacts": ["billing.py"]}) + "\n")
    (tmp_path / "t.csv").write_text('question,answer\n"What changed in the Q3 pricing memo, in brief?",x\n')
    r = invoke("evals", "init", "--from-traces", str(tmp_path / "t.jsonl"), "--no-skill")
    assert r.exit_code == 0 and "1 draft task from 1 traces record" in r.output, r.output
    assert 'artifacts = ["billing.py"]' in (project / "evals/pack.toml").read_text()
    sessions = traces.load(str(tmp_path / "t.csv"), project)
    assert [s.prompt for s in sessions] == ["What changed in the Q3 pricing memo, in brief?"]
    r = invoke("evals", "init", "--from-traces", str(tmp_path / "missing"))
    assert r.exit_code == 1 and "no traces at" in r.output
