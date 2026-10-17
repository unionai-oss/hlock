"""Tests for the `hlock` CLI: the init -> resolve -> check -> run loop, errors, --json."""

import json
import os
import shutil
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock.cli import main

REPO = Path(__file__).resolve().parent.parent

# Variables harnesses set in their child processes; tests mustn't inherit "running inside X".
HARNESS_ENV_PREFIXES = ("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_AGENT", "GEMINI_CLI", "HLOCK_")


@pytest.fixture(autouse=True)
def no_harness_env(monkeypatch):
    for key in list(os.environ):
        if key.startswith(HARNESS_ENV_PREFIXES):
            monkeypatch.delenv(key)


@pytest.fixture
def project(tmp_path, monkeypatch):
    shutil.copytree(REPO / "evals", tmp_path / "evals")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    return tmp_path


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def test_lock_loop(project):
    r = invoke("init", "--name", "demo", "--pack", "./evals")
    assert r.exit_code == 0, r.output
    assert "wrote harness.toml" in r.output
    assert "next" in r.output and "hlock resolve" in r.output

    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    assert (project / "harness.lock").exists()
    assert "Evidence" in r.output and "success rate" in r.output

    r = invoke("check")
    assert r.exit_code == 0, r.output
    assert "matches" in r.output

    r = invoke("show", "--json")
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["lock-version"] == 1

    r = invoke("tree")
    assert r.exit_code == 0 and "planner" in r.output

    r = invoke("run", "--json", "clean ./data/orders.csv")
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["success"] is True


def test_check_detects_drift(project):
    invoke("init", "--pack", "./evals")
    invoke("resolve")
    manifest = project / "harness.toml"
    manifest.write_text(manifest.read_text().replace("max_trials = 200", "max_trials = 10"))

    r = invoke("check")
    assert r.exit_code == 1
    assert "drift" in r.output and "hlock resolve" in r.output


def test_errors_are_friendly(project):
    r = invoke("show")
    assert r.exit_code == 1
    assert "Traceback" not in r.output
    assert "hint" in r.output and "hlock resolve" in r.output

    invoke("init")
    r = invoke("init")
    assert r.exit_code == 1 and "--force" in r.output


def test_missing_manifest_is_friendly(project):
    for cmd in (["resolve"], ["upgrade", "--all"], ["tune"]):
        r = invoke(*cmd)
        assert r.exit_code == 1, r.output
        assert "cannot read" in r.output and "hlock init" in r.output, r.output
        assert "TOML parse error" not in r.output


def test_upgrade_plan(project):
    invoke("init", "--pack", "./evals")
    invoke("resolve")
    r = invoke("upgrade", "--all")
    assert r.exit_code == 0, r.output
    assert "qwen-32b" in r.output
    assert "hlock upgrade --all --apply" in r.output


def test_harness_list_and_show(project, monkeypatch):
    monkeypatch.setenv("CLAUDECODE", "1")
    r = invoke("harness", "list", "--json")
    assert r.exit_code == 0, r.output
    rows = {row["id"]: row for row in json.loads(r.output)}
    for hid in ("claude-code", "codex", "pi", "opencode", "hermes", "cursor-agent", "pydantic-ai", "deepagents"):
        assert hid in rows
    assert rows["claude-code"]["probe"]["active"] is True
    assert rows["pydantic-ai"]["kind"] == "framework"

    r = invoke("harness", "show", "claude")
    assert r.exit_code == 0, r.output
    assert "Claude Code" in r.output and "CLAUDE_CONFIG_DIR" in r.output

    r = invoke("harness", "show", "nope")
    assert r.exit_code == 1 and "hlock harness list" in r.output


def test_init_detects_the_running_harness(project, monkeypatch):
    monkeypatch.setenv("PI_CODING_AGENT", "true")
    r = invoke("init", "--pack", "./evals")
    assert r.exit_code == 0, r.output
    assert 'harness = "pi"' in (project / "harness.toml").read_text()
    assert "running inside it" in r.output


def test_init_without_a_harness_leaves_target_commented(project):
    invoke("init", "--pack", "./evals")
    assert '# harness = "claude-code"' in (project / "harness.toml").read_text()


def test_resolve_for_a_target_harness(project):
    invoke("init", "--pack", "./evals", "--harness", "opencode")
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["harness"]["name"] == "opencode"
    assert lock["harness"]["adapter"] == "reference"
    assert "stand-in" in r.output
    assert invoke("check").exit_code == 0

    # a lock for another harness than [target] is drift
    manifest = project / "harness.toml"
    manifest.write_text(manifest.read_text().replace('harness = "opencode"', 'harness = "pi"'))
    r = invoke("resolve", "--harness", "opencode")
    assert "isn't recorded in harness.toml" in r.output
    r = invoke("check")
    assert r.exit_code == 1 and "targets 'pi'" in r.output


def test_project_harness_in_manifest(project):
    invoke("init", "--pack", "./evals")
    manifest = project / "harness.toml"
    manifest.write_text(manifest.read_text() + """
[harnesses.triage]
framework = "pydantic-ai"
entrypoint = "triage.agent:run"
""")
    r = invoke("harness", "list", "--json")
    assert r.exit_code == 0, r.output
    rows = {row["id"]: row for row in json.loads(r.output)}
    assert rows["triage"]["kind"] == "custom"
    r = invoke("harness", "show", "triage")
    assert "triage.agent:run" in r.output and "pypi:pydantic-ai" in r.output
