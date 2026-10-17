"""`hlock launch`, cross-harness translation, and the code-harness contract."""

import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock.cli import main

REPO = Path(__file__).resolve().parent.parent
GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def clean_env(monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_")):
            monkeypatch.delenv(key)
    monkeypatch.setenv("NO_COLOR", "1")


@pytest.fixture
def pinned(tmp_path, monkeypatch, clean_env):
    """Claude Code pinned on this machine, plus a fake OpenCode to translate into."""
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    executable(bindir / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "${FAKE_CLAUDE_VERSION:-2.1.0} (Claude Code)"; exit 0; fi\n'
               'echo "CONFIG=$CLAUDE_CONFIG_DIR ARGS=$*"\n')
    executable(bindir / "opencode", "#!/bin/sh\necho 1.18.0\n")
    executable(bindir / "npx", "#!/bin/sh\necho 10\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.chdir(project)
    (home / ".claude/skills/my-skill").mkdir(parents=True)
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\ndescription: d\n---\nbody\n")
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (home / ".claude/CLAUDE.md").write_text("be terse\n")
    (home / ".claude.json").write_text(json.dumps({"mcpServers": {"github": {
        "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": GH_TOKEN}}}}))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    return {"home": home, "bin": bindir}


def test_launch_same_harness_isolated(pinned):
    r = invoke("launch", "--dry-run", "--json", "--", "--resume")
    assert r.exit_code == 0, r.output
    prep = json.loads(r.output)
    root = Path(prep["root"])
    assert prep["env"] == {"CLAUDE_CONFIG_DIR": str(root)}
    assert prep["argv"][0].endswith("/claude") and prep["argv"][-1] == "--resume"
    assert "--model" not in prep["argv"]  # a pinned lock restores the model through config
    state = json.loads((root / ".claude.json").read_text())
    # on the machine that pinned it, the secret comes from the live config
    assert state["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
    assert (root / "skills/my-skill/SKILL.md").is_file() and (root / "CLAUDE.md").is_file()
    assert not prep["missing_secrets"]


def test_launch_execs_the_harness(pinned):
    env = {**os.environ, "HL_PYTHON": sys.executable}
    out = subprocess.run([sys.executable, "-m", "harness_lock.cli", "launch", "--", "-p", "hi"],
                         capture_output=True, text=True, env=env, check=False)
    assert out.returncode == 0, out.stderr + out.stdout
    assert "CONFIG=" in out.stdout and ".hlock/env/claude-code/config" in out.stdout and "ARGS=-p hi" in out.stdout


def test_run_drives_the_locked_cli(pinned):
    r = invoke("run", "--json", "summarize README.md")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["success"] and out["harness"] == "claude-code"
    assert ".hlock/env/claude-code/config" in out["output"] and "ARGS=-p summarize README.md" in out["output"]

    env = {**os.environ, "HL_PYTHON": sys.executable}  # streamed, not captured, without --json
    out = subprocess.run([sys.executable, "-m", "harness_lock.cli", "run", "hi"],
                         capture_output=True, text=True, env=env, check=False)
    assert out.returncode == 0 and "ARGS=-p hi" in out.stdout and "✓ success" in out.stdout, out.stdout


def test_run_reports_a_failing_cli(pinned):
    executable(pinned["bin"] / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\nexit 3\n')
    r = invoke("run", "--json", "hi")
    assert r.exit_code == 1 and json.loads(r.output)["exit_code"] == 3


def test_launch_refuses_a_version_mismatch(pinned, monkeypatch):
    monkeypatch.setenv("FAKE_CLAUDE_VERSION", "2.2.0")
    r = invoke("launch", "--dry-run")
    assert r.exit_code == 1 and "lock pins 2.1.0" in r.output
    r = invoke("launch", "--dry-run", "--allow-version-mismatch")
    assert r.exit_code == 0 and "running Claude Code 2.2.0" in r.output


def test_launch_translates_into_another_harness(pinned):
    r = invoke("launch", "opencode", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    prep = json.loads(r.output)
    assert prep["translated"] and prep["harness"] == "opencode"
    root = Path(prep["root"])
    assert prep["env"] == {"XDG_CONFIG_HOME": str(root.parent)}
    config = json.loads((root / "opencode.json").read_text())
    gh = config["mcp"]["github"]
    assert gh["type"] == "local" and gh["command"][:2] == ["npx", "-y"]
    assert gh["environment"]["GITHUB_TOKEN"] == GH_TOKEN  # from Claude Code's live config
    assert (root / "skills/my-skill/SKILL.md").is_file()
    assert (root / "AGENTS.md").read_text() == "be terse\n"  # user CLAUDE.md -> OpenCode's AGENTS.md
    assert prep["argv"][1:3] == ["--model", "anthropic/opus"]


def test_launch_unknown_or_missing_harness(pinned):
    (pinned["bin"] / "opencode").unlink()
    r = invoke("launch", "opencode", "--dry-run")
    assert r.exit_code == 1 and "isn't installed" in r.output


# ---- code harnesses ---------------------------------------------------------------

AGENT = '''
from pathlib import Path

def run(prompt, ctx):
    for name in ("summary.md", "cleaned.csv"):
        (ctx.workspace / name).write_text(prompt)
    return {"output": f"{ctx.model} says: {prompt[:20]}", "cost_usd": 0.001, "steps": 2}
'''


@pytest.fixture
def code_project(tmp_path, monkeypatch, clean_env):
    shutil.copytree(REPO / "evals", tmp_path / "evals")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    (tmp_path / "triage.py").write_text(AGENT)
    invoke("init", "--pack", "./evals")
    manifest = tmp_path / "harness.toml"
    text = manifest.read_text().replace('# [target]\n# harness = "claude-code"', '[target]\nharness = "triage"')
    manifest.write_text(text + '\n[harnesses.triage]\nentrypoint = "triage:run"\n')
    return tmp_path


def test_resolve_runs_a_code_harness_for_real(code_project):
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    assert "stand-in" not in r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["harness"]["name"] == "triage" and lock["harness"]["adapter"] == "triage"
    assert lock["evidence"]["success-rate"]["mean"] == 1.0
    assert lock["evidence"]["cost-usd-per-task"]["mean"] == pytest.approx(0.001)

    r = invoke("run", "hello there")
    assert r.exit_code == 0, r.output
    assert "says: hello there" in r.output and "success" in r.output


def test_command_harness_and_failures(code_project):
    executable(code_project / "agent.sh", '#!/bin/sh\necho "got: $HLOCK_PROMPT"\n'
               'echo \'{"success": false, "steps": 1}\' > "$HLOCK_OUTPUT"\n')
    manifest = code_project / "harness.toml"
    manifest.write_text(manifest.read_text().replace('harness = "triage"', 'harness = "loop"')
                        + '\n[harnesses.loop]\ncommand = ["./agent.sh", "{prompt}"]\n')
    invoke("resolve")
    r = invoke("run", "--json", "do it")
    assert r.exit_code == 1
    result = json.loads(r.output)
    assert result["success"] is False and "got: do it" in result["log"]
