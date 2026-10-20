"""`hlock localize`: an all-local equivalent of a lock."""

import json
import os
import shutil
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import Lockfile
from hlock.cli import main

REPO = Path(__file__).resolve().parent.parent


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def machine(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    executable(bindir / "claude", '#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    executable(bindir / "opencode", "#!/bin/sh\necho 1.18.0\n")
    executable(bindir / "ollama", "#!/bin/sh\necho 'ollama version is 0.12.0'\n")
    executable(bindir / "npx", "#!/bin/sh\necho 10\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)
    # Claude Code on an API model, with one local and one remote MCP server
    (home / ".claude").mkdir()
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (home / ".claude.json").write_text(json.dumps({"mcpServers": {
        "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]},
        "docs": {"type": "http", "url": "https://docs.example.com/mcp"},
    }}))
    # qwen3:8b is already in Ollama's store
    m = home / ".ollama/models/manifests/registry.ollama.ai/library/qwen3/8b"
    m.parent.mkdir(parents=True)
    m.write_text(json.dumps({"config": {"size": 1}, "layers": [{"size": 5_200_000_000}]}))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    return home


def load(path: str) -> dict:
    return json.loads(Lockfile.load(path).to_json())


def test_localize_replaces_harness_and_models(machine):
    r = invoke("localize", "--memory-gb", "10")
    assert r.exit_code == 0, r.output
    assert "harness claude-code → opencode" in r.output
    assert "anthropic:opus → ollama:qwen3:8b" in r.output
    assert "remote MCP servers still need the network: docs" in r.output
    lock = load("harness.local.lock")
    assert lock["kind"] == "localized" and lock["base-lock"] == "harness.lock"
    assert lock["harness"]["name"] == "opencode"
    (llm,) = lock["llm"]
    assert (llm["provider"], llm["model"], llm["endpoint"]) == ("ollama", "qwen3:8b", "http://localhost:11434/v1")
    (rt,) = lock["runtime"]
    assert rt["name"] == "ollama" and rt["model"][0]["present"] is True
    assert {t["name"] for t in lock["tool"]} == {"github", "docs"}
    assert not any(n["kind"].startswith("claude-code") for n in lock.get("native", []))
    rep = lock["localize"]
    assert rep["evaluated"] is False and rep["memory"]["usable-gb"] == 10.0
    # no model over the budget was considered
    assert all(c.get("min-memory-gb", 0) <= 10 for c in rep["models"]["candidates"])

    # the localized lock rehydrates and launches like any other
    plan = json.loads(invoke("sync", "harness.local.lock", "--json").output)
    model = next(s for s in plan["steps"] if s["kind"] == "model")
    assert model["status"] == "satisfied"
    prep = json.loads(invoke("launch", "--lock", "harness.local.lock", "--dry-run", "--json").output)
    assert ".hlock/env/harness.local/opencode" in prep["root"]
    config = json.loads((Path(prep["root"]) / "opencode.json").read_text())
    assert config["model"] == "ollama/qwen3:8b"
    assert config["provider"]["ollama"]["options"]["baseURL"] == "http://localhost:11434/v1"
    assert config["mcp"]["github"]["type"] == "local" and config["mcp"]["docs"]["type"] == "remote"


def test_localize_options(machine):
    r = invoke("localize", "--memory-gb", "10", "--strict", "--runtime", "lmstudio", "-o", "lms.lock")
    assert r.exit_code == 0, r.output
    lock = load("lms.lock")
    assert lock["llm"][0]["provider"] == "lmstudio" and lock["llm"][0]["model"] == "qwen/qwen3-8b"
    assert [t["name"] for t in lock["tool"]] == ["github"]
    assert "LM Studio isn't installed" in r.output

    r = invoke("localize", "--harness", "cursor", "-o", "x.lock")
    assert r.exit_code == 1 and "can't use a local model" in r.output
    r = invoke("localize", "--harness", "claude", "--models", "qwen3:8b", "-o", "c.lock")
    assert r.exit_code == 0 and "proprietary" in r.output
    assert load("c.lock")["harness"]["name"] == "claude-code"
    r = invoke("localize", "--memory-gb", "0.5", "-o", "y.lock")
    assert r.exit_code == 1 and "--models" in r.output


def test_localize_evaluates_with_evals(machine):
    shutil.copytree(REPO / "evals", "evals")
    r = invoke("localize", "--memory-gb", "30", "--evals", "./evals", "-o", "eval.lock")
    assert r.exit_code == 0, r.output
    lock = load("eval.lock")
    assert lock["localize"]["evaluated"] is True
    assert lock["evidence"]["trials"] > 0
    assert "stand-in" in r.output  # OpenCode trials run on the reference adapter today
