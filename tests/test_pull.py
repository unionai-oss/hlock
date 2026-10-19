"""`hlock pull`: pin in an author's repo, pull it into the store (or a project), use it."""

import json
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock import is_pull_source
from harness_lock.cli import MANUAL_EXIT, main

GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def git(repo: Path, *args: str) -> str:
    env = {**os.environ, "GIT_AUTHOR_NAME": "a", "GIT_AUTHOR_EMAIL": "a@x", "GIT_COMMITTER_NAME": "a",
           "GIT_COMMITTER_EMAIL": "a@x"}
    return subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True, text=True).stdout.strip()


def commit(repo: Path, message: str = "pin") -> str:
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message)
    return git(repo, "rev-parse", "HEAD")


@pytest.fixture
def world(tmp_path, monkeypatch):
    """Alice pins her Claude Code setup into github:alice/agent-setup (a local repo
    served through HLOCK_GITHUB_URL); we're in our own project on another machine."""
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "GIT_")):
            monkeypatch.delenv(key)
    alice, me, remotes, project, bindir = (tmp_path / n for n in ("home-alice", "home-me", "remotes", "project", "bin"))
    repo = remotes / "alice/agent-setup.git"
    for d in (alice, me, repo, project, bindir):
        d.mkdir(parents=True)
    claude = bindir / "claude"
    claude.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n')
    claude.chmod(claude.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    npx = bindir / "npx"
    npx.write_text("#!/bin/sh\necho 10.0.0\n")
    npx.chmod(0o755)
    git_dir = Path(shutil.which("git")).parent
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{git_dir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HLOCK_GITHUB_URL", remotes.as_uri())
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "hlock-home"))
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")

    (alice / ".claude/skills/review").mkdir(parents=True)
    (alice / ".claude/skills/review/SKILL.md").write_text("---\nname: review\ndescription: d\n---\nbody\n")
    (alice / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (alice / ".claude.json").write_text(json.dumps({
        "mcpServers": {"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                                  "env": {"GITHUB_TOKEN": GH_TOKEN}}},
    }))
    monkeypatch.setenv("HOME", str(alice))
    monkeypatch.chdir(repo)
    assert invoke("pin", "--harness", "claude").exit_code == 0
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "uploadpack.allowFilter", "true")  # like GitHub: lazy blob fetches
    head = commit(repo)

    monkeypatch.setenv("HOME", str(me))
    monkeypatch.chdir(project)
    return {"repo": repo, "me": me, "project": project, "head": head, "store": tmp_path / "hlock-home/pulls",
            "home": tmp_path / "hlock-home"}


def test_sources_are_recognized():
    assert is_pull_source("github:alice/agent-setup") and is_pull_source("https://gitlab.com/a/b")
    assert not is_pull_source("harness.alice.lock") and not is_pull_source("./locks/x.lock")


def show(lock: str) -> dict:
    r = invoke("--lock", lock, "show", "--json")
    assert r.exit_code == 0, r.output
    return json.loads(r.output)


def test_pull_into_the_store(world):
    (world["project"] / "harness.lock").write_text("# my own lock\n")
    r = invoke("pull", "github:alice/agent-setup")
    assert r.exit_code == 0, r.output
    assert f"pulled github:alice/agent-setup @ {world['head'][:7]} → alice" in r.output
    assert "starts MCP servers with: npx" in r.output
    assert "hlock launch --lock alice" in r.output and "hlock sync alice --in-place --plan" in r.output
    # the project is untouched
    assert sorted(p.name for p in world["project"].iterdir()) == ["harness.lock"]
    assert (world["project"] / "harness.lock").read_text() == "# my own lock\n"

    stored = world["store"] / "alice"
    lock = show("alice")  # --lock NAME finds the stored lock
    assert lock["pulled"]["from"] == "github:alice/agent-setup"
    assert lock["pulled"]["commit"] == world["head"]
    assert lock["pulled"]["path"] == "harness.lock"
    vendored = [s["vendored"] for s in lock["skill"] if s.get("vendored")]
    vendored += [c["vendored"] for c in lock["config"] if c.get("vendored")]
    assert vendored and all(v.startswith(".hlock/vendor/claude-code/") for v in vendored)
    assert all((stored / v).exists() for v in vendored)
    assert GH_TOKEN not in (stored / "harness.lock").read_text()


def test_pull_into_the_project(world):
    r = invoke("pull", "github:alice/agent-setup", "-o", "harness.alice.lock")
    assert r.exit_code == 0, r.output
    assert "→ harness.alice.lock" in r.output and "hlock sync harness.alice.lock" in r.output
    assert "copied 3 vendored entries into .hlock/vendor/alice/claude-code/" in r.output
    lock = show("harness.alice.lock")
    vendored = [s["vendored"] for s in lock["skill"] if s.get("vendored")]
    assert vendored and all(v.startswith(".hlock/vendor/alice/claude-code/") for v in vendored)
    assert all((world["project"] / v).exists() for v in vendored)
    assert not world["store"].exists()
    r = invoke("pull", "harness.alice.lock")  # refresh the project file
    assert r.exit_code == 0 and "harness.alice.lock is up to date" in r.output, r.output


def test_pulled_lock_syncs_into_its_env_or_in_place(world):
    assert invoke("pull", "github:alice/agent-setup").exit_code == 0
    r = invoke("sync", "alice", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output  # the GitHub token is still needed
    root = world["home"] / "env/alice/claude-code/config"  # the root `launch --lock alice` uses
    assert (root / "skills/review/SKILL.md").is_file()
    assert not any(world["me"].iterdir()) and not (world["project"] / ".hlock").exists()

    r = invoke("sync", "alice", "--in-place", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    assert (world["me"] / ".claude/skills/review/SKILL.md").is_file()
    assert json.loads((world["me"] / ".claude/settings.json").read_text())["model"] == "opus"


def test_sync_accepts_a_source(world):
    r = invoke("sync", "github:alice/agent-setup", "--plan")
    assert r.exit_code == 0, r.output
    assert "pulled github:alice/agent-setup" in r.output and "Copy skill review" in r.output
    assert (world["store"] / "alice/harness.lock").is_file()


def test_launch_accepts_a_source_and_keeps_its_env_in_the_store(world):
    r = invoke("launch", "--lock", "github:alice/agent-setup", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    assert (world["store"] / "alice/harness.lock").is_file()
    assert str(world["home"] / "env/alice") in r.output
    assert not (world["project"] / ".hlock").exists()


def test_list_and_rm(world):
    r = invoke("list")
    assert r.exit_code == 0 and "no pulled locks yet" in r.output
    assert invoke("pull", "github:alice/agent-setup").exit_code == 0
    r = invoke("list")
    assert r.exit_code == 0, r.output
    assert "alice" in r.output and "claude-code 2.1.0" in r.output and world["head"][:7] in r.output
    assert json.loads(invoke("list", "--json").output)[0]["name"] == "alice"
    r = invoke("rm", "alice")
    assert r.exit_code == 0 and "removed alice" in r.output
    assert not (world["store"] / "alice").exists()
    r = invoke("rm", "alice")
    assert r.exit_code == 1 and "no pulled lock named" in r.output


def test_refresh_and_up_to_date(world):
    assert invoke("pull", "github:alice/agent-setup").exit_code == 0
    r = invoke("pull")
    assert r.exit_code == 0 and "alice is up to date" in r.output, r.output

    lock = world["repo"] / "harness.lock"
    lock.write_text(lock.read_text().replace('model = "opus"', 'model = "sonnet"'))
    new = commit(world["repo"], "switch model")
    r = invoke("pull", "alice")
    assert r.exit_code == 0, r.output
    assert f"{world['head'][:7]} → {new[:7]}" in r.output
    assert "llm default" in r.output and "sonnet" in r.output
    r = invoke("pull", "bob")
    assert r.exit_code == 1 and "no pulled lock named 'bob'" in r.output


def test_named_locks_ref_and_url(world):
    repo = world["repo"]
    (repo / "harness.lock").rename(repo / "harness.fast.lock")
    git(repo, "checkout", "-q", "-b", "fast")
    commit(repo, "fast")

    r = invoke("pull", "github:alice/agent-setup@fast")
    assert r.exit_code == 0, r.output
    assert "→ alice-fast" in r.output
    lock = show("alice-fast")
    assert lock["pulled"]["ref"] == "fast"
    assert all(s["vendored"].startswith(".hlock/vendor/claude-code/") for s in lock["skill"] if s.get("vendored"))

    # two named locks and no harness.lock: pull lists them
    (repo / "harness.slow.lock").write_text((repo / "harness.fast.lock").read_text())
    commit(repo, "slow")
    r = invoke("pull", "github:alice/agent-setup@fast")
    assert r.exit_code == 1
    assert "hlock pull github:alice/agent-setup/harness.slow.lock@fast" in r.output

    r = invoke("pull", "github:alice/agent-setup/harness.slow.lock@fast", "--as", "slow")
    assert r.exit_code == 0 and "→ slow" in r.output, r.output


def test_lists_other_locks_next_to_harness_lock(world):
    repo = world["repo"]
    (repo / "harness.fast.lock").write_text((repo / "harness.lock").read_text())
    commit(repo, "fast")
    r = invoke("pull", "github:alice/agent-setup")
    assert r.exit_code == 0, r.output
    assert "also in the repo: harness.fast.lock" in r.output
    assert "hlock pull github:alice/agent-setup/harness.fast.lock" in r.output


def test_name_collisions(world):
    mine = world["store"] / "alice/harness.lock"
    mine.parent.mkdir(parents=True)
    mine.write_text("# mine\n")
    r = invoke("pull", "github:alice/agent-setup")
    assert r.exit_code == 1 and "--as NAME" in r.output, r.output
    assert mine.read_text() == "# mine\n"
    assert invoke("pull", "github:alice/agent-setup", "--force").exit_code == 0
    r = invoke("pull", "github:alice/agent-setup", "--as", "Not OK")
    assert r.exit_code == 1 and "letters, digits" in r.output
    (world["project"] / "harness.alice.lock").write_text("# mine\n")
    r = invoke("pull", "github:alice/agent-setup", "-o", "harness.alice.lock")
    assert r.exit_code == 1 and "-o FILE" in r.output, r.output


def test_refuses_vendored_paths_outside_the_repo(world):
    lock = world["repo"] / "harness.lock"
    text = lock.read_text()
    vendored = next(line for line in text.splitlines() if line.startswith("vendored = "))
    lock.write_text(text.replace(vendored, 'vendored = "../../home-me/.ssh/id_rsa"', 1))
    commit(world["repo"], "evil")
    r = invoke("pull", "github:alice/agent-setup")
    assert r.exit_code == 1 and "isn't a path inside the repository" in r.output, r.output
    assert not (world["store"] / "alice").exists()


def test_missing_vendor_dir_is_a_warning(world):
    git(world["repo"], "rm", "-rq", ".hlock")
    commit(world["repo"], "no vendor")
    r = invoke("pull", "github:alice/agent-setup")
    assert r.exit_code == 0, r.output
    assert "isn't in the repository" in r.output


def test_unknown_repo_fails_with_a_hint(world):
    r = invoke("pull", "github:alice/nope")
    assert r.exit_code == 1
    assert "can't fetch github:alice/nope" in r.output and "git ls-remote" in r.output
