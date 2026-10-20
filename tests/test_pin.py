"""`hlock pin` / pinned `check` against a fake home with fake harness binaries."""

import json
import os
import re
import shutil
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import Lockfile
from hlock.cli import main

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


def plain(output: str) -> str:
    """CLI output as one line of words: no colors (rich-click forces them on GitHub Actions),
    no box borders, and no line wrapping."""
    return " ".join(re.sub(r"\x1b\[[0-9;]*m", "", output).replace("│", " ").split())


def all_text(root: Path) -> str:
    return "\n".join(p.read_text(errors="ignore") for p in root.rglob("*") if p.is_file())


def test_pin_claude_code(home):
    r = invoke("pin", "--user", "--harness", "claude")
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
    assert "re-pin with hlock pin --user" in r.output

    r = invoke("pin", "--user", "--harness", "claude")
    assert "changes since the last pin" in r.output and "+ mcp fs" in r.output


def project_setup(home: Path) -> None:
    """A repository with its own Claude Code setup, next to the user-level one in `home`."""
    Path(".claude/skills/team-skill").mkdir(parents=True)
    Path(".claude/skills/team-skill/SKILL.md").write_text("---\nname: team-skill\ndescription: d\n---\nbody\n")
    Path(".claude/settings.json").write_text(json.dumps({"permissions": {"allow": ["Bash(uv run pytest)"]}}))
    Path("CLAUDE.md").write_text("run the tests\n")
    Path(".mcp.json").write_text(json.dumps({"mcpServers": {"linear": {"type": "http", "url": "https://mcp.linear.app/mcp"}}}))
    # a "local" MCP server: Claude Code keeps it in ~/.claude.json under this project, not in the repo
    state = home / ".claude.json"
    data = json.loads(state.read_text())
    data["projects"] = {str(Path.cwd()): {"mcpServers": {"scratch": {"command": "uvx", "args": ["mcp-scratch"]}}}}
    state.write_text(json.dumps(data))


def test_pin_defaults_to_the_project(home):
    project_setup(home)
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0, r.output
    out = " ".join(r.output.split())
    assert "left out user-level: 1 skill, 2 MCP servers, 1 instruction file, 2 config files" in out
    assert "to pin it too: hlock pin --user, or pick with -i GLOB" in out
    lock = json.loads(invoke("show", "--json").output)
    assert lock["provenance"]["scopes"] == ["project"]
    # the stack itself is still pinned, wherever it's set
    assert lock["harness"]["version"] == "2.1.0" and lock["llm"][0]["model"] == "opus"
    assert [(t["name"], t["scope"]) for t in lock["tool"]] == [("linear", "project")]
    assert [(s["name"], s["origin"], s["scope"]) for s in lock["skill"]] == [("team-skill", "first-party", "project")]
    assert [i["name"] for i in lock["instructions"]] == ["CLAUDE.md"]
    assert {c["id"] for c in lock["config"]} == {"project-mcp", "project-settings"}
    assert all(c["restore"] == "record" for c in lock["config"])
    # nothing of the user's is copied into the repository
    assert not Path(".hlock/vendor").exists() and not lock.get("secret")
    assert GH_TOKEN not in all_text(Path.cwd())

    assert invoke("check").exit_code == 0
    # user-level changes aren't drift for a project pin…
    (home / ".claude/skills/another").mkdir()
    (home / ".claude/skills/another/SKILL.md").write_text("---\nname: another\ndescription: d\n---\n")
    assert invoke("check").exit_code == 0
    # …project changes are
    Path(".claude/skills/team-skill/SKILL.md").write_text("---\nname: team-skill\ndescription: d\n---\nchanged\n")
    r = invoke("check")
    assert r.exit_code == 1 and "~ skill team-skill" in r.output, r.output
    assert "re-pin with hlock pin," in r.output


def test_pin_user_adds_the_user_level_setup(home):
    project_setup(home)
    r = invoke("pin", "--user", "--harness", "claude")
    assert r.exit_code == 0, r.output
    assert "project + user" in r.output and "left out" not in r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["provenance"]["scopes"] == ["project", "user"]
    assert {(t["name"], t["scope"]) for t in lock["tool"]} == {("github", "user"), ("scratch", "user"), ("linear", "project")}
    assert {s["name"] for s in lock["skill"]} == {"my-skill", "team-skill"}
    assert {i["name"] for i in lock["instructions"]} == {"CLAUDE.md", "{root}/CLAUDE.md"}

    # dropping --user on a re-pin is allowed, but said out loud
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0 and "previous pin included user-level setup; pass --user to keep it" in r.output, r.output
    assert "- skill my-skill (user)" in r.output


def test_include_user_scope_picks_matching_items(home):
    project_setup(home)
    (home / ".claude/skills/write-tests").mkdir()
    (home / ".claude/skills/write-tests/SKILL.md").write_text("---\nname: write-tests\ndescription: d\n---\n")
    r = invoke("pin", "-i", "skills/my-*", "-i", "CLAUDE.md", "--harness", "claude")
    assert r.exit_code == 0, r.output
    out = " ".join(r.output.split())
    assert "project + user (skills/my-*, CLAUDE.md)" in out
    assert "left out user-level: 1 skill, 2 MCP servers, 2 config files" in out
    lock = json.loads(invoke("show", "--json").output)
    assert lock["provenance"]["scopes"] == ["project", "user"]
    assert lock["provenance"]["user-include"] == ["skills/my-*", "CLAUDE.md"]
    assert {s["name"] for s in lock["skill"]} == {"my-skill", "team-skill"}
    assert {i["name"] for i in lock["instructions"]} == {"CLAUDE.md", "{root}/CLAUDE.md"}
    assert [t["name"] for t in lock["tool"]] == ["linear"]
    assert {c["id"] for c in lock["config"]} == {"project-mcp", "project-settings"}
    assert Path(".hlock/vendor/claude-code/skills/my-skill/SKILL.md").is_file()

    # check re-captures the same selection: an unpicked skill isn't drift, a picked one is
    assert invoke("check").exit_code == 0
    (home / ".claude/skills/write-tests/SKILL.md").write_text("---\nname: write-tests\n---\nchanged\n")
    assert invoke("check").exit_code == 0
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\n---\nchanged\n")
    r = invoke("check")
    assert r.exit_code == 1 and "~ skill my-skill" in r.output, r.output
    assert "re-pin with hlock pin -i 'skills/my-*' -i CLAUDE.md" in " ".join(r.output.split())

    # MCP servers come with the file that defines them; a pattern that matches nothing says so
    r = invoke("pin", "-i", "~/.claude.json", "-i", "skills/nope-*", "--harness", "claude")
    assert r.exit_code == 0, r.output
    assert "pass -i 'skills/my-*' -i CLAUDE.md to keep it" in " ".join(r.output.split())
    assert "matched nothing" in r.output and "skills/nope-*" in r.output
    lock = json.loads(invoke("show", "--json").output)
    assert {t["name"] for t in lock["tool"]} == {"github", "scratch", "linear"}
    assert {c["id"] for c in lock["config"]} == {"state", "project-mcp", "project-settings"}

    r = invoke("pin", "--user", "-i", "skills", "--harness", "claude")
    assert r.exit_code == 2 and "--user already pins everything" in plain(r.output), r.output


def test_locks_from_before_scopes_check_everything(home):
    assert invoke("pin", "--user", "--harness", "claude").exit_code == 0
    text = Path("harness.lock").read_text()
    assert 'scopes = ["project", "user"]' in text
    Path("harness.lock").write_text(text.replace('scopes = ["project", "user"]\n', ""))
    assert invoke("check").exit_code == 0
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\n---\nchanged\n")
    r = invoke("check")
    assert r.exit_code == 1 and "~ skill my-skill" in r.output, r.output


def test_pin_opencode_with_local_runtime(home):
    r = invoke("pin", "--user", "--harness", "opencode", "-o", "oc.lock")
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
    r = invoke("pin", "--user", "--harness", "claude")
    assert r.exit_code == 1 and "resolved lock" in r.output and "-o" in r.output
    assert invoke("pin", "--user", "--harness", "claude", "-o", "pinned.lock").exit_code == 0
    assert invoke("pin", "--user", "--harness", "claude", "--force").exit_code == 0


def test_upgrade_explains_a_pinned_lock(home):
    assert invoke("pin", "--user", "--harness", "claude").exit_code == 0
    r = invoke("upgrade", "--all")
    assert r.exit_code == 1 and "pinned lock" in r.output and "hlock pin" in r.output, r.output
