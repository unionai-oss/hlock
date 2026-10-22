"""The lock tracks every harness component Harness² edits: role instruction files
(guardrails, memory, tool guidance), subagents, scripts and middleware."""

from __future__ import annotations

import json
import os
import re
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock.cli import main


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def fake_bin(bindir: Path, name: str, output: str) -> None:
    p = bindir / name
    p.write_text(f"#!/bin/sh\necho '{output}'\n")
    p.chmod(p.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def plain(output: str) -> str:
    return " ".join(re.sub(r"\x1b\[[0-9;]*m", "", output).replace("│", " ").split())


def lock(path: str = "harness.lock") -> dict:
    from hlock import Lockfile

    return json.loads(Lockfile.load(path).to_json())


def write(path: str | Path, text: str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text)


@pytest.fixture
def home(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)
    fake_bin(bindir, "claude", "2.1.0 (Claude Code)")
    fake_bin(bindir, "opencode", "1.18.0")
    fake_bin(bindir, "codex", "codex-cli 0.50.0")
    # a Claude Code project with all eight parts
    write("CLAUDE.md", "Be terse.\n")
    write(".claude/rules/guardrails.md", "Never push to main.\n")
    write(".claude/rules/memory.md", "The API is v2.\n")
    write(".claude/rules/style.md", "Use snake_case.\n")
    write(".claude/agents/reviewer.md", "---\nname: reviewer\ndescription: reviews diffs\n---\nRead the diff.\n")
    write(".claude/skills/triage/SKILL.md", "---\nname: triage\ndescription: d\n---\nTriage it.\n")
    write(".agents/scripts/check.py", "print('ok')\n")
    write(home / ".claude/agents/helper.md", "---\nname: helper\ndescription: helps\n---\nHelp.\n")
    write(home / ".claude/settings.json", json.dumps({"model": "opus"}))
    return home


def test_pin_tracks_role_files_subagents_and_scripts(home):
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0, r.output
    d = lock()
    roles = {i["name"]: i.get("role") for i in d["instructions"]}
    assert roles == {"CLAUDE.md": None, ".claude/rules/guardrails.md": "guardrails",
                     ".claude/rules/memory.md": "memory", ".claude/rules/style.md": None}
    assert [(s["name"], s["path"], s["scope"]) for s in d["subagent"]] == [("reviewer", ".claude/agents/reviewer.md", "project")]
    assert "vendored" not in d["subagent"][0]  # the project holds it
    assert [(s["name"], s["path"]) for s in d["script"]] == [("check.py", ".agents/scripts/check.py")]
    assert "middleware" not in d  # Claude Code's hooks are in its settings, pinned as config

    # a user-level subagent is vendored, like a user-level skill
    assert invoke("pin", "--user", "--harness", "claude").exit_code == 0
    helper = next(s for s in lock()["subagent"] if s["name"] == "helper")
    assert helper["scope"] == "user" and helper["path"] == "{root}/agents/helper.md"
    assert Path(helper["vendored"]).read_text().endswith("Help.\n")


def test_opencode_sections_subagents_and_plugins(home):
    write("AGENTS.md", "Be terse.\n\n## Guardrails\n\nNever push to main.\n")
    write(".opencode/agent/helper.md", "---\nmode: subagent\ndescription: helps\n---\nHelp.\n")
    write(".opencode/plugin/guard.ts", "export const Guard = async () => ({})\n")
    write(".opencode/plugin/hlock-deciders.js", "// hlock's own\n")
    r = invoke("pin", "--harness", "opencode")
    assert r.exit_code == 0, r.output
    d = lock()
    agents = next(i for i in d["instructions"] if i["name"] == "AGENTS.md")
    assert agents["sections"] == ["guardrails"]
    assert [s["name"] for s in d["subagent"]] == ["helper"]
    assert [(m["name"], m["format"]) for m in d["middleware"]] == [("guard", "opencode-plugin")]


def test_codex_hooks_file_is_middleware(home):
    write(".codex/hooks.json", json.dumps({"hooks": {"PreToolUse": []}}))
    assert invoke("pin", "--harness", "codex").exit_code == 0
    assert [(m["name"], m["path"], m["format"]) for m in lock()["middleware"]] == [("hooks", ".codex/hooks.json", "codex-hooks")]


def test_check_diff_and_audit_see_them(home):
    import subprocess

    git = lambda *a: subprocess.run(["git", "-c", "user.email=t@example.com", "-c", "user.name=t", *a],
                                    check=True, capture_output=True)
    assert invoke("pin", "--harness", "claude").exit_code == 0
    git("init", "-q")
    git("add", "-A")
    git("commit", "-qm", "pin")
    write(".claude/agents/reviewer.md", "---\nname: reviewer\ndescription: reviews diffs\n---\nRead the diff, then the tests.\n")
    r = invoke("check")
    assert r.exit_code == 1 and "subagent reviewer (project)" in plain(r.output), r.output

    # a re-pin points at diff, which shows the subagent's text against the committed lock
    r = invoke("pin", "--harness", "claude")
    assert "see the content: hlock diff" in r.output, r.output
    out = json.loads(invoke("diff", "--json").output)
    c = next(c for c in out["components"] if c["kind"] == "subagent")
    assert c["change"] == "~" and (c["old_from"], c["new_from"]) == ("project", "project")
    assert "-Read the diff.\n+Read the diff, then the tests." in c["files"][0]["hunks"]

    # the helper script runs, so audit lists it as an executable
    report = json.loads(invoke("audit", "--format", "json").output)
    assert ("script", "check.py") in {(e["kind"], e["name"]) for e in report["executables"]}


def test_sync_places_a_vendored_subagent_and_gates_scripts(home, tmp_path, monkeypatch):
    assert invoke("pin", "--user", "--harness", "claude").exit_code == 0
    fresh = tmp_path / "home-b"
    fresh.mkdir()
    monkeypatch.setenv("HOME", str(fresh))
    os.remove(".agents/scripts/check.py")
    plan = json.loads(invoke("sync", "--json").output)
    steps = {s["id"]: s for s in plan["steps"]}
    assert steps["subagent:helper"]["status"] == "auto"
    assert steps["subagent:reviewer"]["status"] == "satisfied"
    assert steps["script:check.py"]["status"] == "manual"  # the repo's file is missing, and nothing vendors it
    untrusted = {(e["kind"], e["name"]) for e in plan["executables"] if e["status"] == "untrusted"}
    assert ("script", "check.py") in untrusted


def test_migrate_moves_subagents_and_composes_roles(home):
    assert invoke("pin", "--harness", "claude").exit_code == 0
    r = invoke("migrate", "--to", "opencode")
    assert r.exit_code == 0, r.output
    d = lock("harness.opencode.lock")
    agent = next(s for s in d["subagent"] if s["name"] == "reviewer")
    assert agent["path"] == ".opencode/agent/reviewer.md"
    text = Path(agent["vendored"]).read_text()
    assert text.startswith("---\nmode: subagent\n") and "Read the diff." in text
    # OpenCode reads CLAUDE.md too, so the roles become sections of it
    main_file = next(i for i in d["instructions"] if i["name"] == "CLAUDE.md")
    assert main_file["sections"] == ["guardrails", "memory"]
    composed = Path(main_file["vendored"]).read_text()
    assert "## Guardrails\n\nNever push to main." in composed and "## Memory\n\nThe API is v2." in composed
    assert not any(i.get("role") for i in d["instructions"])
    assert [s["name"] for s in d["script"]] == ["check.py"]
