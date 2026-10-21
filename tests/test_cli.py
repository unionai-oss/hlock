"""Tests for the `hlock` CLI: the init -> resolve -> check -> run loop, errors, --json."""

import json
import os
import shutil
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock.cli import main

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
    manifest.write_text(manifest.read_text().replace("max-trials = 200", "max-trials = 10"))

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


def test_init_inside_a_harness_resolves_on_the_reference_harness(project, monkeypatch):
    # inside an agent, init doesn't target it with models it can't run; it says how to
    monkeypatch.setenv("PI_CODING_AGENT", "true")
    r = invoke("init", "--pack", "./evals")
    assert r.exit_code == 0, r.output
    text = (project / "harness.toml").read_text()
    assert '# harness = "claude-code"' in text and 'harness = ["reference"]' in text
    assert "you're in pi" in r.output and "hlock init --force --harness pi" in r.output


def test_init_without_a_harness_leaves_target_commented(project):
    invoke("init", "--pack", "./evals")
    assert '# harness = "claude-code"' in (project / "harness.toml").read_text()


def test_resolve_for_a_target_harness(project):
    r = invoke("init", "--pack", "./evals", "--harness", "opencode")
    assert "list the models opencode runs" in r.output, r.output
    manifest = project / "harness.toml"
    r = invoke("resolve")
    assert r.exit_code == 1 and "list the models opencode runs in [candidates.llm.planner]" in r.output, r.output
    # the offline reference model isn't one an agent CLI runs
    text = manifest.read_text()
    planner = next(line for line in text.splitlines() if line.startswith("any_of"))
    manifest.write_text(text.replace(planner, 'any_of = ["reference:qwen-7b"]'))
    r = invoke("resolve")
    assert r.exit_code == 1 and "opencode can't run reference:qwen-7b" in r.output, r.output
    manifest.write_text(text.replace(planner, 'any_of = ["anthropic:claude-sonnet-5-5"]'))
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["harness"]["name"] == "opencode"
    assert lock["harness"]["adapter"] == "reference"
    assert lock["evidence"]["simulated"] is True
    assert "opencode itself didn't run these trials" in r.output and "stand-in" in r.output
    r = invoke("check")
    assert r.exit_code == 0 and "evidence is a stand-in" in r.output

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
