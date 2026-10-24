"""`hlock env suggest`, the hint `hlock pin` prints, and the meta harness's `environment` check."""

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


def plain(output: str) -> str:
    return " ".join(re.sub(r"\x1b\[[0-9;]*m", "", output).split())


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "HLOCK_", "XDG_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)
    claude = bindir / "claude"
    claude.write_text("#!/bin/sh\necho '2.1.0 (Claude Code)'\n")
    claude.chmod(claude.stat().st_mode | stat.S_IXUSR)
    # an MCP server started with npx, and a hook that pipes through jq and rg
    (project / ".mcp.json").write_text(json.dumps({"mcpServers": {"github": {
        "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]}}}))
    (project / ".claude").mkdir()
    (project / ".claude/settings.json").write_text(json.dumps({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
        {"type": "command", "command": "jq -r .tool_input.command | rg -q 'rm -rf /' && exit 2 || exit 0"}]}]}}))
    (project / "harness.toml").write_text("# the bot\n[project]\nname = \"bot\"\n\n[target]\nharness = \"claude-code\"\n")
    return project


def test_pin_hints_at_programs_the_environment_doesnt_declare(project):
    r = invoke("pin")
    assert r.exit_code == 0, r.output
    out = plain(r.output)
    assert "the stack runs 3 programs [environment] doesn't declare: jq, npx, rg" in out, out
    assert "hlock env suggest" in out


def test_suggest_lists_then_writes_the_missing_packages(project):
    assert invoke("pin").exit_code == 0
    r = invoke("env", "suggest", "--json")
    assert r.exit_code == 0, r.output
    rows = {c["program"]: c for c in json.loads(r.output)}
    assert set(rows) == {"jq", "npx", "rg"}
    assert rows["npx"]["sources"] == ["mcp github"] and rows["npx"]["confidence"] == "high"
    assert rows["rg"]["packages"]["apt"] == "ripgrep" and rows["rg"]["status"] == "missing"

    r = invoke("env", "suggest", "--write")
    assert r.exit_code == 0, r.output
    assert "added to [environment] apt: ripgrep" in plain(r.output)
    text = (project / "harness.toml").read_text()
    assert text.startswith("# the bot\n"), text
    assert 'apt = ["jq", "npm", "ripgrep"]' in text and 'brew = ["jq", "node", "ripgrep"]' in text, text
    rows = {c["program"]: c for c in json.loads(invoke("env", "suggest", "--json").output)}
    assert {c["status"] for c in rows.values()} == {"declared"}
    # and pin no longer hints
    assert "doesn't declare" not in plain(invoke("pin", "--force").output)


def test_meta_verify_environment(project):
    assert invoke("pin").exit_code == 0
    r = invoke("meta", "verify", "environment")
    assert r.exit_code == 1, r.output
    assert "3 programs the stack runs are neither declared nor ignored" in plain(r.output)
    with Path("harness.toml").open("a") as f:
        f.write('\n[environment]\napt = ["jq", "npm", "ripgrep"]\nignore = []\n')
    r = invoke("meta", "verify", "environment")
    assert r.exit_code == 0, r.output
    assert "3 programs declared, 0 ignored" in plain(r.output)


def test_write_needs_a_manifest(project):
    assert invoke("pin").exit_code == 0
    (project / "harness.toml").unlink()
    r = invoke("env", "suggest", "--write")
    assert r.exit_code == 1 and "doesn't exist" in plain(r.output), r.output
