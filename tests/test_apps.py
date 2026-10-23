"""Desktop apps (`kind = "app"`): Claude Desktop, VS Code and the rest, pinned and restored, never started."""

import json
import os
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import Lockfile
from hlock.cli import main


def invoke(*args, input=None):
    return CliRunner().invoke(main, list(args), input=input)


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def bundle(apps: Path, name: str, version: str) -> None:
    contents = apps / name / "Contents"
    contents.mkdir(parents=True)
    (contents / "Info.plist").write_text(
        "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict>\n"
        f"<key>CFBundleShortVersionString</key>\n<string>{version}</string>\n</dict></plist>\n")


@pytest.fixture
def machine(tmp_path, monkeypatch):
    """macOS, with Claude Desktop installed and one MCP server in its config; no app running."""
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "CURSOR_", "HLOCK_", "XDG_", "APPDATA")):
            monkeypatch.delenv(key)
    home, project, bindir, apps = tmp_path / "home", tmp_path / "project", tmp_path / "bin", tmp_path / "Applications"
    for d in (home, project, bindir, apps):
        d.mkdir()
    executable(bindir / "npx", "#!/bin/sh\necho 10\n")
    bundle(apps, "Claude.app", "3.1623.1")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HLOCK_OS", "macos")
    monkeypatch.setenv("HLOCK_APP_DIRS", str(apps))
    monkeypatch.setenv("HLOCK_APPS_RUNNING", "")
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "hlock-home"))
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")
    monkeypatch.chdir(project)
    config = home / "Library/Application Support/Claude"
    config.mkdir(parents=True)
    (config / "claude_desktop_config.json").write_text(json.dumps({
        "mcpServers": {"fs": {"command": "npx", "args": ["-y", "fs-mcp"]}},
        "preferences": {"sidebarMode": "chat"},
    }))
    return config / "claude_desktop_config.json"


def test_pin_takes_the_apps_user_config_by_default(machine):
    r = invoke("pin", "--harness", "claude-desktop")
    assert r.exit_code == 0, r.output
    lock = json.loads(Lockfile.load("harness.lock").to_json())
    assert lock["harness"]["version"] == "3.1623.1" and lock["harness"]["source"] == "app@3.1623.1"
    assert [t["name"] for t in lock["tool"]] == ["fs"]
    # only /mcpServers: the rest of the file is the app's own state
    assert [c["pointers"] for c in lock["config"]] == [["/mcpServers"]]
    assert invoke("check").exit_code == 0


def test_sync_writes_the_apps_config_only_with_apps(machine, tmp_path):
    assert invoke("pin", "--harness", "claude-desktop").exit_code == 0
    assert invoke("trust", "--yes").exit_code == 0
    machine.unlink()  # a new machine: the app is installed, its config isn't
    r = invoke("sync", "--yes")
    assert r.exit_code == 3, r.output
    assert "(needs --apps)" in r.output and "reads only your own config" in r.output
    assert "hlock sync --apps" in r.output and "not restored: 3 parts of Claude Desktop" in r.output
    assert not machine.exists()
    r = invoke("sync", "--apps", "--yes")
    assert r.exit_code == 0, r.output
    assert "quit and reopen Claude Desktop" in r.output
    assert json.loads(machine.read_text())["mcpServers"]["fs"]["args"] == ["-y", "fs-mcp"]
    # a newer app is fine: it updates itself
    bundle_dir = tmp_path / "Applications/Claude.app"
    (bundle_dir / "Contents/Info.plist").write_text((bundle_dir / "Contents/Info.plist").read_text().replace("3.1623.1", "3.1700.0"))
    assert invoke("check").exit_code == 0
    # --apps writes your own config: it doesn't combine with a prefix
    r = invoke("sync", "--apps", "--prefix", "x")
    assert r.exit_code != 0 and "doesn't work with --prefix" in r.output


def test_sync_asks_before_writing_an_apps_config(machine, monkeypatch):
    assert invoke("pin", "--harness", "claude-desktop").exit_code == 0
    assert invoke("trust", "--yes").exit_code == 0
    machine.unlink()
    # the runner's stdin, as a terminal
    monkeypatch.setattr("click.testing._NamedTextIOWrapper.isatty", lambda self: True, raising=False)
    r = invoke("sync", input="y\ny\n")
    assert "Claude Desktop reads only its own config. Write it?" in r.output, r.output
    assert r.exit_code == 0, r.output
    assert machine.exists()


def test_a_running_app_isnt_written(machine, monkeypatch):
    assert invoke("pin", "--harness", "claude-desktop").exit_code == 0
    assert invoke("trust", "--yes").exit_code == 0
    machine.unlink()
    monkeypatch.setenv("HLOCK_APPS_RUNNING", "claude-desktop")
    r = invoke("sync", "--apps", "--yes")
    assert r.exit_code == 3 and "Claude Desktop is running" in r.output, r.output
    assert not machine.exists()


def test_apps_are_listed_and_never_started(machine):
    r = invoke("harness", "list")
    assert r.exit_code == 0 and "Desktop apps" in r.output and "claude-desktop" in r.output, r.output
    r = invoke("harness", "show", "claude-desktop")
    assert "desktop app" in r.output and "Connectors (remote MCP servers)" in r.output, r.output
    assert invoke("pin", "--harness", "claude-desktop").exit_code == 0
    for args in (["run", "hi"], ["launch"], ["init", "--force", "--harness", "claude-desktop"]):
        r = invoke(*args)
        assert r.exit_code != 0 and "desktop app" in r.output, (args, r.output)


@pytest.mark.parametrize("app, bundle_name, path, text", [
    ("cursor-ide", "Cursor.app", ".cursor/mcp.json", '{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "fs-mcp"]}}}'),
    ("chatgpt", "ChatGPT.app", ".codex/config.toml", '[mcp_servers.fs]\ncommand = "npx"\nargs = ["-y", "fs-mcp"]\n'),
    ("devin-desktop", "Devin.app", ".config/devin/mcp_config.json", '{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "fs-mcp"]}}}'),
    ("kiro", "Kiro.app", ".kiro/settings/mcp.json", '{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "fs-mcp"]}}}'),
    ("antigravity", "Antigravity.app", ".gemini/config/mcp_config.json", '{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "fs-mcp"]}}}'),
    ("trae", "Trae.app", "Library/Application Support/Trae/User/mcp.json", '{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "fs-mcp"]}}}'),
])
def test_more_apps_pin_and_restore_their_mcp_servers(machine, tmp_path, app, bundle_name, path, text):
    bundle(tmp_path / "Applications", bundle_name, "2.0.1")
    config = tmp_path / "home" / path
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(text)
    r = invoke("pin", "--harness", app)
    assert r.exit_code == 0, r.output
    lock = json.loads(Lockfile.load("harness.lock").to_json())
    assert lock["harness"]["name"] == app and lock["harness"]["source"] == "app@2.0.1"
    assert [(t["name"], t["command"]) for t in lock["tool"]] == [("fs", "npx")]
    assert invoke("check").exit_code == 0
    assert invoke("trust", "--yes").exit_code == 0
    config.unlink()
    r = invoke("sync", "--yes")
    assert r.exit_code == 3 and "hlock sync --apps" in r.output and not config.exists(), r.output
    r = invoke("sync", "--apps", "--yes")
    assert r.exit_code == 0, r.output
    if path.endswith(".toml"):
        assert '[mcp_servers.fs]' in config.read_text() and 'command = "npx"' in config.read_text()
    else:
        assert json.loads(config.read_text()) == json.loads(text)
    assert invoke("check").exit_code == 0
