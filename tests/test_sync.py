"""`hlock sync`: pin on one "machine" (a fake HOME), rehydrate on another."""

import json
import os
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock.cli import MANUAL_EXIT, main

GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def fake_claude(bindir: Path, version: str = "2.1.0") -> None:
    """A `claude` that prints a version and logs every other invocation."""
    p = bindir / "claude"
    p.write_text(
        "#!/bin/sh\n"
        f'if [ "$1" = "--version" ]; then echo "{version} (Claude Code)"; exit 0; fi\n'
        f'echo "$CLAUDE_CONFIG_DIR|$*" >> "{bindir}/claude.log"\n'
    )
    p.chmod(p.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def machines(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_")):
            monkeypatch.delenv(key)
    a, b, project, bindir = (tmp_path / n for n in ("home-a", "home-b", "project", "bin"))
    for d in (a, b, project, bindir):
        d.mkdir()
    fake_claude(bindir)
    npx = bindir / "npx"  # MCP servers launch through npx
    npx.write_text("#!/bin/sh\necho 10.0.0\n")
    npx.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)

    (a / ".claude/skills/my-skill").mkdir(parents=True)
    (a / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\ndescription: d\n---\nbody\n")
    (a / ".claude/settings.json").write_text(json.dumps({"model": "opus", "enabledPlugins": {"lsp@market": True}}))
    (a / ".claude/plugins").mkdir()
    (a / ".claude/plugins/installed_plugins.json").write_text(json.dumps(
        {"plugins": {"lsp@market": [{"version": "1.0.0", "gitCommitSha": "abc123", "scope": "user"}]}}))
    (a / ".claude/plugins/known_marketplaces.json").write_text(json.dumps(
        {"market": {"source": {"source": "github", "repo": "acme/market"}}}))
    (a / ".claude.json").write_text(json.dumps({
        "oauthAccount": {"emailAddress": "me@example.com"},
        "mcpServers": {"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                                  "env": {"GITHUB_TOKEN": GH_TOKEN}}},
    }))
    monkeypatch.setenv("HOME", str(a))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    monkeypatch.setenv("HOME", str(b))  # now we're on the new machine
    return {"a": a, "b": b, "bin": bindir}


def test_rehydrate_on_a_new_machine(machines, monkeypatch):
    b = machines["b"]
    r = invoke("sync", "--plan")
    assert r.exit_code == 0, r.output
    assert "Write" in r.output and "Copy skill my-skill" in r.output

    r = invoke("sync", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    assert "export HLOCK_SECRET_GITHUB_GITHUB_TOKEN" in r.output
    state = json.loads((b / ".claude.json").read_text())
    assert state["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
    assert "oauthAccount" not in state
    assert (b / ".claude/skills/my-skill/SKILL.md").is_file()
    assert json.loads((b / ".claude/settings.json").read_text())["model"] == "opus"
    log = (machines["bin"] / "claude.log").read_text()
    assert "plugin marketplace add acme/market" in log and "plugin install lsp@market" in log

    monkeypatch.setenv("HLOCK_SECRET_GITHUB_GITHUB_TOKEN", GH_TOKEN)
    r = invoke("sync", "--yes")
    assert json.loads((b / ".claude.json").read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
    # the fake claude doesn't really install plugins, so those steps stay automatic; everything else is in sync
    plan = json.loads(invoke("sync", "--json").output)
    pending = {s["id"] for s in plan["steps"] if s["status"] != "satisfied"}
    assert pending == {"claude-code.plugin:lsp@market", "claude-code.marketplace:market"}, pending


def test_synced_config_is_not_drift(machines, monkeypatch):
    # sync reformats the JSON and fills in this machine's secret; neither is a change to the setup
    monkeypatch.setenv("HLOCK_SECRET_GITHUB_GITHUB_TOKEN", "ghp_another_machines_token_0123456789")
    invoke("sync", "--yes")
    r = invoke("check")
    assert "config" not in r.output and "mcp" not in r.output, r.output


def test_sync_keeps_secrets_already_configured(machines, monkeypatch):
    monkeypatch.setenv("HOME", str(machines["a"]))
    plan = json.loads(invoke("sync", "--json").output)
    assert {s["status"] for s in plan["steps"]} == {"satisfied"}
    secret = next(s for s in plan["steps"] if s["kind"] == "secret")
    assert "kept the value already configured" in secret["reason"]


def test_missing_harness_is_a_manual_step(machines):
    (machines["bin"] / "claude").unlink()
    r = invoke("sync", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    assert "Install Claude Code 2.1.0" in r.output
    plan = json.loads(invoke("sync", "--json").output)
    harness = next(s for s in plan["steps"] if s["id"] == "harness")
    assert harness["status"] == "manual" and harness["command"]


def test_prefix_is_isolated(machines):
    r = invoke("sync", "--prefix", "env", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    root = Path("env/claude-code/config")
    assert (root / "settings.json").is_file() and (root / ".claude.json").is_file()
    assert (root / "skills/my-skill/SKILL.md").is_file()
    assert not (machines["b"] / ".claude.json").exists()
    # plugin commands ran against the isolated root
    assert f"{root.resolve()}|plugin install lsp@market" in (machines["bin"] / "claude.log").read_text()


def test_skill_for_agent_guided_rehydration(machines):
    (machines["bin"] / "claude").unlink()
    r = invoke("sync", "--skill")
    assert r.exit_code == 0, r.output
    text = Path(".agents/skills/hlock-rehydrate/SKILL.md").read_text()
    assert text.startswith("---\nname: hlock-rehydrate\n")
    assert "Install Claude Code 2.1.0" in text and "Never print" in text and "hlock check" in text
    assert GH_TOKEN not in text


def test_docker_context(machines):
    r = invoke("sync", "--docker", "--yes")
    assert r.exit_code == 0, r.output
    ctx = Path(".hlock/docker/claude-code")
    dockerfile = (ctx / "Dockerfile").read_text()
    assert "npm install -g @anthropic-ai/claude-code@2.1.0" in dockerfile
    assert "COPY home/ /root/" in dockerfile and 'CMD ["claude"]' in dockerfile
    assert (ctx / "home/.claude/settings.json").is_file()
    assert os.access(ctx / "hlock-entrypoint", os.X_OK)
    assert GH_TOKEN not in "".join(p.read_text(errors="ignore") for p in ctx.rglob("*") if p.is_file())
    assert "docker run" in r.output and "-e HLOCK_SECRET_GITHUB_GITHUB_TOKEN" in r.output


def test_sync_refuses_without_yes_when_not_a_tty(machines):
    r = invoke("sync")
    assert r.exit_code == 1 and "--yes" in r.output
