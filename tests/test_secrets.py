"""Where secret values come from (environment, secrets file, keychain), and where they land."""

import json
import os
import stat
import subprocess
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock.cli import MANUAL_EXIT, main

GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"
NAME = "HLOCK_SECRET_GITHUB_GITHUB_TOKEN"
ENV_STATE = Path(".hlock/env/claude-code/config/.claude.json")

# A stand-in for macOS `security` and Linux `secret-tool`, keeping items in $FAKE_KEYCHAIN.
FAKE_KEYCHAIN = r'''#!/usr/bin/env python3
import os, pathlib, shlex, sys
store = pathlib.Path(os.environ["FAKE_KEYCHAIN"])
store.mkdir(exist_ok=True)
args = sys.argv[1:]
def opt(flag, argv):
    return argv[argv.index(flag) + 1]
if pathlib.Path(sys.argv[0]).name == "security":
    if args == ["-i"]:
        for line in sys.stdin:
            argv = shlex.split(line)
            if argv and argv[0] == "add-generic-password":
                (store / opt("-a", argv)).write_text(opt("-w", argv))
        sys.exit(0)
    item = store / opt("-a", args)
    if args[0] == "find-generic-password":
        if not item.exists():
            sys.exit(44)
        print(item.read_text())
    elif args[0] == "delete-generic-password":
        item.unlink()
else:
    item = store / args[args.index("account") + 1]
    if args[0] == "lookup":
        if not item.exists():
            sys.exit(1)
        sys.stdout.write(item.read_text())
    elif args[0] == "store":
        item.write_text(sys.stdin.read())
    elif args[0] == "clear":
        item.unlink(missing_ok=True)
'''


def invoke(*args, input=None):
    return CliRunner().invoke(main, list(args), input=input)


def executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def pinned(tmp_path, monkeypatch):
    """Claude Code pinned on one machine, then a fresh home with a fake keychain."""
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_")):
            monkeypatch.delenv(key)
    a, b, project, bindir = (tmp_path / n for n in ("home-a", "home-b", "project", "bin"))
    for d in (a, b, project, bindir):
        d.mkdir()
    executable(bindir / "claude", '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; fi\n')
    executable(bindir / "npx", "#!/bin/sh\necho 10\n")
    for tool in ("security", "secret-tool"):
        executable(bindir / tool, FAKE_KEYCHAIN)
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("FAKE_KEYCHAIN", str(tmp_path / "keychain"))
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)
    (a / ".claude").mkdir()
    (a / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (a / ".claude.json").write_text(json.dumps({"mcpServers": {"github": {
        "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": GH_TOKEN}}}}))
    monkeypatch.setenv("HOME", str(a))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    monkeypatch.setenv("HOME", str(b))
    return {"home": b, "keychain": tmp_path / "keychain", "tmp": tmp_path}


def mode(p: Path) -> int:
    return stat.S_IMODE(p.stat().st_mode)


def test_files_with_secret_values_are_owner_only(pinned, monkeypatch):
    monkeypatch.setenv(NAME, GH_TOKEN)
    assert invoke("sync", "--yes").exit_code == 0
    assert json.loads(ENV_STATE.read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
    assert mode(ENV_STATE) == 0o600
    assert mode(ENV_STATE.parent / "settings.json") != 0o600  # no secrets in it
    assert invoke("sync", "--in-place", "--yes").exit_code == 0
    assert mode(pinned["home"] / ".claude.json") == 0o600


def test_keychain(pinned):
    r = invoke("sync", "--yes")
    assert r.exit_code == MANUAL_EXIT and "hlock secret set GITHUB_GITHUB_TOKEN" in r.output, r.output
    r = invoke("secret", "list")
    assert "not set" in r.output and ("security" in r.output or "secret-tool" in r.output), r.output

    r = invoke("secret", "set", "GITHUB_GITHUB_TOKEN", input=GH_TOKEN + "\n")
    assert r.exit_code == 0 and f"stored {NAME}" in r.output, r.output
    assert GH_TOKEN not in r.output
    r = invoke("secret", "list")
    assert "keychain" in r.output and "not set" not in r.output and GH_TOKEN not in r.output, r.output

    plan = json.loads(invoke("sync", "--json").output)
    secret = next(s for s in plan["steps"] if s["kind"] == "secret")
    assert secret["status"] == "satisfied" and secret["reason"] == "from the keychain"
    assert GH_TOKEN not in json.dumps(plan)
    assert invoke("sync", "--yes").exit_code == 0
    assert json.loads(ENV_STATE.read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN

    assert "removed" in invoke("secret", "rm", "GITHUB_GITHUB_TOKEN").output
    assert "has no" in invoke("secret", "rm", "GITHUB_GITHUB_TOKEN").output


def test_keychain_can_be_turned_off(pinned, monkeypatch):
    invoke("secret", "set", NAME, input=GH_TOKEN)
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")
    assert "not set" in invoke("secret", "list").output


def test_secrets_file_references(pinned, monkeypatch):
    tok = pinned["tmp"] / "token"
    tok.write_text(GH_TOKEN + "\n")
    secrets = pinned["tmp"] / "secrets.toml"
    secrets.write_text(f'[secrets]\nGITHUB_GITHUB_TOKEN = {{ command = "cat {tok}" }}\n')
    monkeypatch.setenv("HLOCK_SECRETS_FILE", str(secrets))
    assert "secrets file (command)" in invoke("secret", "list").output
    plan = json.loads(invoke("sync", "--json").output)
    assert next(s for s in plan["steps"] if s["kind"] == "secret")["reason"] == "from the secrets file (command)"
    assert invoke("sync", "--yes").exit_code == 0
    assert json.loads(ENV_STATE.read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN

    # the environment wins over the file
    monkeypatch.setenv(NAME, "ghp_from_the_environment_0123456789")
    assert next(s for s in json.loads(invoke("sync", "--json").output)["steps"]
                if s["kind"] == "secret")["reason"] == "from the environment"

    # values don't belong in the file
    secrets.write_text('[secrets]\nGITHUB_GITHUB_TOKEN = "ghp_literal"\n')
    r = invoke("secret", "list")
    assert r.exit_code == 1 and "references only" in r.output and "hlock secret set GITHUB_GITHUB_TOKEN" in r.output


def test_docker_reads_mounted_secrets(pinned, tmp_path):
    assert invoke("sync", "--docker", "--yes").exit_code == 0
    ctx = Path(".hlock/docker/claude-code")
    assert "/run/secrets" in json.dumps(json.loads(invoke("sync", "--docker", "--json").output))
    mounted = tmp_path / "run-secrets"
    mounted.mkdir()
    (mounted / "GITHUB_GITHUB_TOKEN").write_text(GH_TOKEN + "\n")  # Compose names it without the prefix
    env = {"HOME": str(ctx / "home"), "HLOCK_SECRETS_DIR": str(mounted), "PATH": os.environ["PATH"]}
    subprocess.run(["sh", str(ctx / "hlock-entrypoint"), "true"], env=env, check=True)
    state = ctx / "home/.claude.json"
    assert json.loads(state.read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
    assert mode(state) == 0o600


def test_launch_leaves_no_values_on_disk(pinned, monkeypatch):
    monkeypatch.setenv(NAME, GH_TOKEN)
    assert invoke("sync", "--yes").exit_code == 0  # sync writes values, for a harness you start yourself
    assert GH_TOKEN in ENV_STATE.read_text()
    r = invoke("launch", "--dry-run")  # launch passes them in the environment instead
    assert r.exit_code == 0, r.output
    on_disk = "".join(f.read_text(errors="ignore") for f in Path(".hlock").rglob("*") if f.is_file())
    assert GH_TOKEN not in on_disk and "${" + NAME + "}" in on_disk  # no backup kept the old value
    assert invoke("sync", "--yes").exit_code == 0  # and a later sync fills the reference again
    assert json.loads(ENV_STATE.read_text())["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN
