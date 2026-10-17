"""`hlock pin` / pinned `check` against a fake home with fake harness binaries."""

import json
import os
import shutil
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock import Lockfile
from harness_lock.cli import main

GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"
OC_KEY = "sk-local-0123456789ABCDEFabcdef"


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def fake_bin(bindir: Path, name: str, output: str) -> None:
    p = bindir / name
    p.write_text(f"#!/bin/sh\necho '{output}'\n")
    p.chmod(p.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def home(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)

    # Claude Code: a model, an MCP server with a secret, a hand-written skill, CLAUDE.md
    fake_bin(bindir, "claude", "2.1.0 (Claude Code)")
    (home / ".claude/skills/my-skill").mkdir(parents=True)
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\ndescription: d\n---\nbody\n")
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus", "theme": "dark"}))
    (home / ".claude/CLAUDE.md").write_text("be terse\n")
    (home / ".claude.json").write_text(json.dumps({
        "oauthAccount": {"emailAddress": "me@example.com"},
        "mcpServers": {"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                                  "env": {"GITHUB_TOKEN": GH_TOKEN}}},
    }))

    # OpenCode: JSONC config with a local Ollama provider, and the model in Ollama's store
    fake_bin(bindir, "opencode", "1.18.0")
    fake_bin(bindir, "ollama", "ollama version is 0.12.0")
    (home / ".config/opencode").mkdir(parents=True)
    (home / ".config/opencode/opencode.json").write_text("""{
      // my setup
      "model": "ollama/qwen3:8b",
      "provider": {"ollama": {"options": {"baseURL": "http://localhost:11434/v1", "apiKey": "OC_KEY"},
                              "models": {"qwen3:8b": {}, "gpt-oss:20b": {},}}},
      "plugin": ["opencode-safe-git@0.4.1"],
    }""".replace("OC_KEY", OC_KEY))
    manifest = home / ".ollama/models/manifests/registry.ollama.ai/library/qwen3/8b"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(json.dumps({"config": {"size": 10}, "layers": [{"size": 5000}, {"size": 90}]}))
    return home


def all_text(root: Path) -> str:
    return "\n".join(p.read_text(errors="ignore") for p in root.rglob("*") if p.is_file())


def test_pin_claude_code(home):
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0, r.output
    assert "pinned claude-code 2.1.0" in r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["kind"] == "pin"
    assert lock["harness"]["version"] == "2.1.0"
    assert lock["llm"][0] == {**lock["llm"][0], "role": "default", "provider": "anthropic", "model": "opus"}
    (gh,) = lock["tool"]
    assert gh["source"] == "npm:@modelcontextprotocol/server-github" and gh["scope"] == "user"
    assert gh["env"]["GITHUB_TOKEN"].startswith("${HLOCK_SECRET_")
    (skill,) = lock["skill"]
    assert skill["origin"] == "local" and skill["path"] == "{root}/skills/my-skill"
    assert (Path(skill["vendored"]) / "SKILL.md").is_file()
    assert any(i["name"] == "{root}/CLAUDE.md" and i["vendored"] for i in lock["instructions"])
    # only /mcpServers of ~/.claude.json is kept; secrets never reach disk
    state = next(c for c in lock["config"] if c["id"] == "state")
    assert state["restore"] == "pointers" and "oauthAccount" not in Path(state["vendored"]).read_text()
    text = all_text(Path.cwd())
    assert GH_TOKEN not in text and "me@example.com" not in text

    assert invoke("check").exit_code == 0

    # drift: edit the skill, add an MCP server
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\n---\nchanged\n")
    state_file = home / ".claude.json"
    data = json.loads(state_file.read_text())
    data["mcpServers"]["fs"] = {"command": "uvx", "args": ["mcp-server-fs"]}
    state_file.write_text(json.dumps(data))
    r = invoke("check")
    assert r.exit_code == 1, r.output
    assert "~ skill my-skill" in r.output and "+ mcp fs" in r.output

    r = invoke("pin", "--harness", "claude")
    assert "changes since the last pin" in r.output and "+ mcp fs" in r.output


def test_pin_opencode_with_local_runtime(home):
    r = invoke("pin", "--harness", "opencode", "-o", "oc.lock")
    assert r.exit_code == 0, r.output
    lock = json.loads(Lockfile.load("oc.lock").to_json())
    (llm,) = lock["llm"]
    assert (llm["provider"], llm["model"], llm["runtime"]) == ("ollama", "qwen3:8b", "ollama")
    (rt,) = lock["runtime"]
    assert rt["location"] == "local" and rt["version"] == "0.12.0"
    models = {m["id"]: m for m in rt["model"]}
    assert models["qwen3:8b"]["present"] and models["qwen3:8b"]["size-bytes"] == 5100
    assert not models["gpt-oss:20b"]["present"]
    assert lock["native"][0]["source"] == "npm:opencode-safe-git@0.4.1"
    assert OC_KEY not in all_text(Path.cwd())
    assert lock["secret"][0]["name"].startswith("HLOCK_SECRET_OPENCODE_CONFIG_PROVIDER_OLLAMA")


def test_pin_never_clobbers_a_resolved_lock(home, tmp_path):
    repo = Path(__file__).resolve().parent.parent
    shutil.copytree(repo / "evals", "evals")
    invoke("init", "--pack", "./evals")
    assert invoke("resolve").exit_code == 0
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 1 and "resolved lock" in r.output and "-o" in r.output
    assert invoke("pin", "--harness", "claude", "-o", "pinned.lock").exit_code == 0
    assert invoke("pin", "--harness", "claude", "--force").exit_code == 0


def test_upgrade_explains_a_pinned_lock(home):
    assert invoke("pin", "--harness", "claude").exit_code == 0
    r = invoke("upgrade", "--all")
    assert r.exit_code == 1 and "pinned lock" in r.output and "hlock pin" in r.output, r.output
