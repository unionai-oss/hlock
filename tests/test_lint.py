"""`hlock lint`: instruction text written for an earlier model, found in what a lock places
and in --file paths; model-aware rules; JSON, SARIF and --ci; and the Python API the
findings come back through (exact byte spans)."""

import json
import os
import shutil
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

import hlock
from hlock.cli import main

STALE_SKILL = """---
name: review
description: Review the diff.
---
Review the diff. Answer directly without deliberating.

- Always delegate research to subagents.
- Never push to main.

```sh
# never retry: shown, not said
```
"""


def invoke(*args):
    return CliRunner().invoke(main, list(args))


@pytest.fixture
def setup(tmp_path, monkeypatch):
    """Claude Code on Fable 5.2 with a user skill written for an earlier model, pinned."""
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "GIT_")):
            monkeypatch.delenv(key)
    home, project, bindir = (tmp_path / n for n in ("home", "project", "bin"))
    for d in (home, project, bindir):
        d.mkdir()
    claude = bindir / "claude"
    claude.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "2.1.3 (Claude Code)"; fi\n')
    claude.chmod(claude.stat().st_mode | stat.S_IXUSR)
    git_dir = Path(shutil.which("git")).parent
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{git_dir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("COLUMNS", "200")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")
    monkeypatch.setenv("HLOCK_HARNESS", "claude-code")
    (home / ".claude/skills/review").mkdir(parents=True)
    (home / ".claude/skills/review/SKILL.md").write_text(STALE_SKILL)
    (home / ".claude/settings.json").write_text(json.dumps({"model": "claude-fable-5-2"}))
    monkeypatch.chdir(project)
    assert invoke("pin", "--user").exit_code == 0
    return {"home": home, "project": project}


def test_lint_uses_the_locks_model(setup):
    r = invoke("lint", "--format", "json")
    assert r.exit_code == 0, r.output
    report = json.loads(r.output)
    assert (report["model"], report["model_from"]) == ("claude-fable-5-2", "lock")
    rules = [f["rule"] for f in report["findings"]]
    assert rules == ["think-less", "standing-delegation", "unscoped-absolute"], rules
    assert report["warnings"] == 1 and report["infos"] == 2 and not report["passed"]
    f = report["findings"][0]
    assert (f["kind"], f["entry"], f["line"], f["severity"]) == ("skill", "review", 5, "warning")
    assert f["file"].endswith("SKILL.md") and f["matched"] == "Answer directly without deliberating"
    text = Path(f["path"]).read_text()
    assert text[f["start"]:f["end"]] == f["matched"]
    assert text[f["line_start"]:f["line_end"]] == "Review the diff. Answer directly without deliberating.\n"


def test_lint_model_flag_ci_and_table(setup):
    # another model: only the rules for every model apply
    r = invoke("lint", "--model", "claude-opus-5-5", "--ci", "--format", "json")
    report = json.loads(r.output)
    assert r.exit_code == 0 and report["rules"] == ["unscoped-absolute"] and report["passed"]
    # the delegation line, a delegation finding for Fable, is an absolute rule here
    assert [(f["rule"], f["line"]) for f in report["findings"]] == [("unscoped-absolute", 7), ("unscoped-absolute", 8)]
    # --ci fails on warnings
    r = invoke("lint", "--ci")
    assert r.exit_code == 1, r.output
    assert "think-less" in r.output and "Answer directly without deliberating" in r.output
    assert "unscoped-absolute" not in r.output, "info findings only with -v"
    assert "-v lists them" in r.output
    r = invoke("lint", "-v")
    assert r.exit_code == 0 and "unscoped-absolute" in r.output and "Never push to main." in r.output


def test_lint_files_without_a_lock(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    Path("AGENTS.md").write_text("You have a budget of 200k tokens.\nTreat that answer as done.\n")
    r = invoke("lint", "--file", "AGENTS.md", "--model", "claude-fable-5-2", "--format", "json")
    report = json.loads(r.output)
    assert r.exit_code == 0 and report["lock"] is None
    assert [(f["rule"], f["line"], f["kind"]) for f in report["findings"]] == [
        ("token-budget-in-prompt", 1, "file"), ("treat-answer-as-done", 2, "file")]
    sarif = json.loads(invoke("lint", "--file", "AGENTS.md", "--model", "claude-fable-5-2", "--format", "sarif").output)
    res = sarif["runs"][0]["results"]
    assert [x["ruleId"] for x in res] == ["hlock/lint/token-budget-in-prompt", "hlock/lint/treat-answer-as-done"]
    assert res[0]["locations"][0]["physicalLocation"]["region"]["startLine"] == 1
    r = invoke("lint", "--file", "missing.md")
    assert r.exit_code != 0 and "missing.md" in r.output


def test_python_api():
    found = json.loads(hlock.lint_text("Think step by step and show your reasoning.\n", "p.md", "claude-fable-5-2"))
    assert [f["rule"] for f in found] == ["visible-reasoning"]
    assert (found[0]["start"], found[0]["end"], found[0]["path"]) == (0, 18, "")
    assert json.loads(hlock.lint_text("Think step by step.\n", "p.md", "claude-opus-5-5")) == []
    every = json.loads(hlock.lint_rules())
    assert {"think-less", "unscoped-absolute"} <= {r["id"] for r in every}
    assert "examples" not in every[0]
    assert [r["id"] for r in json.loads(hlock.lint_rules(model="gpt-5"))] == ["unscoped-absolute"]
    assert len(json.loads(hlock.lint_rules(model="all"))) == len(every)


def test_lint_rules_listing():
    r = invoke("lint", "--rules", "--model", "claude-fable-5-2")
    assert r.exit_code == 0 and "think-less" in r.output and "unscoped-absolute" in r.output


def test_this_repos_instructions_have_no_warnings():
    """AGENTS.md and the hlock skill are what the rules were tuned on: no warnings."""
    root = Path(__file__).resolve().parent.parent
    for path in [root / "AGENTS.md", *sorted((root / "skills/hlock").rglob("*.md"))]:
        found = json.loads(hlock.lint_text(path.read_text(), path.name, "all"))
        assert [f for f in found if f["severity"] == "warning"] == [], path
