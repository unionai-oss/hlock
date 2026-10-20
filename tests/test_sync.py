"""`hlock sync`: pin on one "machine" (a fake HOME), rehydrate on another."""

import json
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock.cli import MANUAL_EXIT, main

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


ENV_ROOT = Path(".hlock/env/claude-code/config")


def test_sync_is_isolated_by_default(machines, monkeypatch):
    b = machines["b"]
    r = invoke("sync", "--plan")
    assert r.exit_code == 0, r.output
    assert "Write" in r.output and "Copy skill my-skill" in r.output and ".hlock/env/claude-code/config" in r.output

    r = invoke("sync", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    assert "hlock secret set GITHUB_GITHUB_TOKEN" in r.output
    state = json.loads((ENV_ROOT / ".claude.json").read_text())
    assert state["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
    assert (ENV_ROOT / "skills/my-skill/SKILL.md").is_file()
    assert json.loads((ENV_ROOT / "settings.json").read_text())["model"] == "opus"
    assert not any(b.iterdir())  # the global config is untouched
    log = (machines["bin"] / "claude.log").read_text()
    assert f"{ENV_ROOT.resolve()}|plugin install lsp@market" in log

    monkeypatch.setenv("HLOCK_SECRET_GITHUB_GITHUB_TOKEN", GH_TOKEN)
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    assert "CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config claude" in r.output and "hlock sync --in-place" in r.output
    assert json.loads((ENV_ROOT / ".claude.json").read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
    assert not any(b.iterdir())


def test_isolated_env_is_kept_out_of_git(machines):
    invoke("sync", "--plan")
    assert not Path(".hlock/env").exists()  # planning writes nothing
    invoke("sync", "--yes")
    ignore = Path(".hlock/env/claude-code/.gitignore")
    assert ignore.read_text().splitlines()[-1] == "*"
    invoke("sync", "--prefix", "env", "--yes")
    assert Path("env/claude-code/.gitignore").is_file()
    assert not Path("env/.gitignore").exists()  # only the directory hlock owns
    if shutil.which("git"):
        subprocess.run(["git", "init", "-q"], check=True)
        status = subprocess.run(["git", "status", "--porcelain", "--untracked-files=all"],
                                capture_output=True, text=True, check=True).stdout
        assert "harness.lock" in status and ".hlock/vendor/" in status  # these are committed
        assert ".hlock/env" not in status and "env/claude-code" not in status, status
    # a .gitignore you edited is left alone
    ignore.write_text("!keep-me\n")
    invoke("sync", "--yes")
    assert ignore.read_text() == "!keep-me\n"


def test_in_place_writes_your_config(machines, monkeypatch):
    b = machines["b"]
    r = invoke("sync", "--in-place", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    assert "re-run hlock sync --in-place" in r.output
    state = json.loads((b / ".claude.json").read_text())
    assert state["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
    assert "oauthAccount" not in state
    assert (b / ".claude/skills/my-skill/SKILL.md").is_file()
    assert json.loads((b / ".claude/settings.json").read_text())["model"] == "opus"
    log = (machines["bin"] / "claude.log").read_text()
    assert "plugin marketplace add acme/market" in log and "plugin install lsp@market" in log
    assert not Path(".hlock/env").exists() and not (b / ".claude/.gitignore").exists()

    monkeypatch.setenv("HLOCK_SECRET_GITHUB_GITHUB_TOKEN", GH_TOKEN)
    r = invoke("sync", "--in-place", "--yes")
    assert json.loads((b / ".claude.json").read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
    # the fake claude doesn't really install plugins, so those steps stay automatic; everything else is in sync
    plan = json.loads(invoke("sync", "--in-place", "--json").output)
    pending = {s["id"] for s in plan["steps"] if s["status"] != "satisfied"}
    assert pending == {"claude-code.plugin:lsp@market", "claude-code.marketplace:market"}, pending


def test_in_place_is_exclusive(machines):
    for other in (["--prefix", "env"], ["--docker"]):
        r = invoke("sync", "--in-place", *other)
        assert r.exit_code == 1 and "--in-place" in r.output, r.output


def test_synced_config_is_not_drift(machines, monkeypatch):
    # sync reformats the JSON and fills in this machine's secret; neither is a change to the setup
    monkeypatch.setenv("HLOCK_SECRET_GITHUB_GITHUB_TOKEN", "ghp_another_machines_token_0123456789")
    invoke("sync", "--yes")
    r = invoke("check", "--prefix", ".hlock/env")
    assert "config" not in r.output and "mcp" not in r.output and "skill" not in r.output, r.output
    # your own config wasn't synced, so it has drifted, and the hint restores it in place
    r = invoke("check")
    assert r.exit_code == 1 and "hlock sync --in-place" in r.output, r.output
    invoke("sync", "--in-place", "--yes")
    r = invoke("check")
    assert "config" not in r.output and "mcp" not in r.output, r.output


def test_sync_keeps_secrets_already_configured(machines, monkeypatch):
    monkeypatch.setenv("HOME", str(machines["a"]))
    plan = json.loads(invoke("sync", "--in-place", "--json").output)
    assert {s["status"] for s in plan["steps"]} == {"satisfied"}
    secret = next(s for s in plan["steps"] if s["kind"] == "secret")
    assert "kept the value already configured" in secret["reason"]
    # the isolated root takes the secret from your live config, without writing to it
    invoke("sync", "--yes")
    state = json.loads((ENV_ROOT / ".claude.json").read_text())
    assert state["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN


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
    assert "Install Claude Code 2.1.0" in text and "Never print" in text
    assert "hlock check --prefix .hlock/env" in text and "isolated config root under `.hlock/env`" in text
    assert GH_TOKEN not in text
    r = invoke("sync", "--in-place", "--skill")
    text = Path(".agents/skills/hlock-rehydrate/SKILL.md").read_text()
    assert "`hlock sync --in-place`" in text and "isolated" not in text


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
