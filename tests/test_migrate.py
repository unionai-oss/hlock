"""`hlock migrate`: a lock moved onto another harness."""

import json
import os
import stat
from pathlib import Path

import pytest
import tomllib
from click.testing import CliRunner

from hlock import Lockfile
from hlock.cli import main


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    executable(bindir / "claude", '#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    executable(bindir / "codex", '#!/bin/sh\necho "codex-cli 0.50.0"\n')
    executable(bindir / "npx", "#!/bin/sh\necho 10\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)
    # Claude Code on Opus, with a project skill, CLAUDE.md and two MCP servers
    (home / ".claude").mkdir()
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (project / ".mcp.json").write_text(json.dumps({"mcpServers": {
        "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]},
        "docs": {"type": "http", "url": "https://docs.example.com/mcp"},
    }}))
    skill = project / ".claude/skills/review"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: review\ndescription: Review a diff.\n---\nLook for bugs.\n")
    (project / "CLAUDE.md").write_text("Run the tests before you say you're done.\n")
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0, r.output
    return project


def load(path: str) -> dict:
    return json.loads(Lockfile.load(path).to_json())


def test_claude_code_to_codex(project):
    r = invoke("migrate", "--to", "codex")
    assert r.exit_code == 0, r.output
    assert "harness claude-code → codex" in r.output
    assert "anthropic:opus → openai:gpt-5.5" in r.output
    assert "skill review: .claude/skills/review → .agents/skills/review" in r.output
    assert "instructions CLAUDE.md → AGENTS.md" in r.output
    assert "hlock sync harness.codex.lock" in r.output

    lock = load("harness.codex.lock")
    assert lock["kind"] == "migrated" and lock["base-lock"] == "harness.lock"
    assert lock["harness"]["name"] == "codex" and lock["harness"]["version"] == "0.50.0"
    (llm,) = lock["llm"]
    assert (llm["provider"], llm["model"]) == ("openai", "gpt-5.5")
    (skill,) = lock["skill"]
    assert skill["path"] == ".agents/skills/review"
    assert (project / skill["vendored"] / "SKILL.md").is_file()
    (ins,) = lock["instructions"]
    assert ins["name"] == "AGENTS.md" and (project / ins["vendored"]).read_text().startswith("Run the tests")
    assert {t["name"] for t in lock["tool"]} == {"github", "docs"}
    assert lock["migrate"]["carried"] == {"mcp": 2, "skills": 1, "instructions": 1, "deciders": 0}
    assert not any(c["id"] in ("settings", "project-mcp") for c in lock["config"])
    assert load("harness.lock")["harness"]["name"] == "claude-code"  # the source isn't touched

    # another lock for the same harness doesn't touch this one's vendored files
    assert invoke("migrate", "--to", "codex", "--model", "openai:gpt-5.5-mini", "-o", "other.lock").exit_code == 0

    # sync puts every moved part where Codex reads it
    plan = json.loads(invoke("sync", "harness.codex.lock", "--json").output)
    steps = {s["id"]: s for s in plan["steps"]}
    assert steps["skill:review"]["status"] == "auto", steps["skill:review"]
    assert steps["instructions:AGENTS.md"]["status"] == "auto", steps["instructions:AGENTS.md"]
    r = invoke("sync", "harness.codex.lock", "--yes")
    assert r.exit_code == 0, r.output
    assert (project / ".agents/skills/review/SKILL.md").is_file()
    assert (project / "AGENTS.md").read_text().startswith("Run the tests")

    # and launch configures Codex: the model and both MCP servers
    prep = json.loads(invoke("launch", "--lock", "harness.codex.lock", "--dry-run", "--json").output)
    config = tomllib.loads((Path(prep["root"]) / "config.toml").read_text())
    assert config["model"] == "gpt-5.5" and config["model_provider"] == "openai"
    assert config["mcp_servers"]["github"]["command"] == "npx"
    assert config["mcp_servers"]["docs"]["url"] == "https://docs.example.com/mcp"


def test_migrate_options(project):
    r = invoke("migrate", "--to", "codex", "--model", "openai:gpt-5.5-mini", "-o", "c.lock")
    assert r.exit_code == 0, r.output
    assert load("c.lock")["llm"][0]["model"] == "gpt-5.5-mini"
    # the project's own AGENTS.md is what Codex reads, so it's pinned instead
    (project / "AGENTS.md").write_text("Shared rules.\n")
    r = invoke("migrate", "--to", "codex", "-o", "a.lock", "--json")
    assert r.exit_code == 0, r.output
    (ins,) = load("a.lock")["instructions"]
    assert ins["name"] == "AGENTS.md" and "vendored" not in ins
    assert "instructions CLAUDE.md → AGENTS.md (the project's own)" in json.loads(r.output)["replaced"]
    # OpenCode reads .claude/skills and CLAUDE.md, so they stay
    r = invoke("migrate", "--to", "opencode")
    assert r.exit_code == 0, r.output
    lock = load("harness.opencode.lock")
    assert lock["skill"][0]["path"] == ".claude/skills/review" and lock["instructions"][0]["name"] == "CLAUDE.md"
    assert lock["llm"][0]["provider"] == "anthropic"  # OpenCode talks to Anthropic

    r = invoke("migrate", "--to", "claude")
    assert r.exit_code == 1 and "already on Claude Code" in r.output
    r = invoke("migrate", "--to", "codex", "--model", "anthropic:opus")
    assert r.exit_code == 1 and "doesn't talk to" in r.output
    r = invoke("migrate", "--to", "pydantic-ai")
    assert r.exit_code == 1 and "between agent CLIs" in r.output
    r = invoke("migrate")
    assert r.exit_code == 1 and "--to" in r.output


def test_migrate_local_is_localize(project):
    executable(Path(os.environ["PATH"].split(os.pathsep)[0]) / "ollama", "#!/bin/sh\necho 'ollama version is 0.12.0'\n")
    r = invoke("migrate", "--to", "codex", "--local", "--runtime", "ollama")
    assert r.exit_code == 0, r.output
    lock = load("harness.local.lock")
    assert lock["kind"] == "localized" and lock["harness"]["name"] == "codex"
    assert lock["llm"][0]["provider"] == "ollama"
    # localize moves project parts where the new harness reads them, too
    assert lock["skill"][0]["path"] == ".agents/skills/review"
    assert lock["instructions"][0]["name"] == "AGENTS.md"
