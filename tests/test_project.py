"""Finding the manifest and lock: -m/--manifest, --lock, HLOCK_MANIFEST, and the upward search."""

import json
import os
import shutil
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock.cli import main

REPO = Path(__file__).resolve().parent.parent


def invoke(*args):
    return CliRunner().invoke(main, list(args))


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_AGENT", "HLOCK_")):
            monkeypatch.delenv(key)
    monkeypatch.setenv("NO_COLOR", "1")
    shutil.copytree(REPO / "evals", tmp_path / "evals")
    monkeypatch.chdir(tmp_path)
    return tmp_path


def test_commands_find_the_project_from_a_subdirectory(project, monkeypatch):
    assert invoke("init", "--pack", "./evals").exit_code == 0
    sub = project / "src" / "agent"
    sub.mkdir(parents=True)
    monkeypatch.chdir(sub)

    r = invoke("resolve")
    assert r.exit_code == 0, r.output  # ./evals resolved against the project root
    assert (project / "harness.lock").exists() and not (sub / "harness.lock").exists()
    assert (project / ".harnesslock").is_dir() and not (sub / ".harnesslock").exists()
    assert invoke("check").exit_code == 0
    assert json.loads(invoke("show", "--json").output)["lock-version"] == 1


def test_manifest_flag_in_either_position(project):
    agent = project / "agents" / "triage"
    agent.mkdir(parents=True)
    r = invoke("-m", "agents/triage/harness.toml", "init", "--pack", "../../evals")
    assert r.exit_code == 0, r.output
    assert (agent / "harness.toml").is_file() and not (project / "harness.toml").exists()

    r = invoke("resolve", "-m", "agents/triage/harness.toml")  # after the subcommand
    assert r.exit_code == 0, r.output
    assert (agent / "harness.lock").exists()  # the lock lives next to its manifest
    assert invoke("check", "--manifest=agents/triage/harness.toml").exit_code == 0

    r = invoke("check")  # no harness.toml here or above
    assert r.exit_code == 1 and "hlock init" in r.output


def test_lock_flag_and_env_vars(project, monkeypatch):
    invoke("init", "--pack", "./evals")
    assert invoke("resolve", "--lock", "locks/main.lock").exit_code == 0
    assert (project / "locks/main.lock").exists() and not (project / "harness.lock").exists()
    assert invoke("--lock", "locks/main.lock", "check").exit_code == 0
    assert invoke("check").exit_code == 1  # default lock doesn't exist

    monkeypatch.setenv("HLOCK_LOCKFILE", "locks/main.lock")
    assert invoke("check").exit_code == 0
    os.rename("harness.toml", "agent.toml")
    assert invoke("check").exit_code == 1  # no harness.toml anymore
    monkeypatch.setenv("HLOCK_MANIFEST", "agent.toml")
    assert invoke("check").exit_code == 0  # same bytes, found through the env var
