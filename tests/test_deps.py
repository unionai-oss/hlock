"""Declared dependencies: `hlock add`, `remove`, `outdated`, `upgrade dep:`, `export`, and
how `sync` and `check` treat them, against local git repositories standing in for GitHub
(`HLOCK_GITHUB_URL`) and a local npm registry."""

import json
import os
import shutil
import stat
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import ClassVar

import pytest
from click.testing import CliRunner

from hlock.cli import main


def invoke(*args, input=None):
    return CliRunner().invoke(main, list(args), input=input)


def git(repo: Path, *args: str) -> str:
    env = {**os.environ, "GIT_AUTHOR_NAME": "a", "GIT_AUTHOR_EMAIL": "a@x", "GIT_COMMITTER_NAME": "a",
           "GIT_COMMITTER_EMAIL": "a@x"}
    return subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True, text=True).stdout.strip()


def skill(name: str, body: str) -> str:
    return f"---\nname: {name}\ndescription: d\n---\n{body}\n"


def make_repo(remotes: Path, owner_repo: str, versions: list[tuple[str, dict[str, str]]]) -> Path:
    r = remotes / f"{owner_repo}.git"
    r.mkdir(parents=True)
    git(r, "init", "-q", "-b", "main")
    for tag, files in versions:
        for p, text in files.items():
            (r / p).parent.mkdir(parents=True, exist_ok=True)
            (r / p).write_text(text)
        git(r, "add", "-A")
        git(r, "commit", "-q", "-m", tag or "change")
        if tag:
            git(r, "tag", tag)
    return r


class _Registry(BaseHTTPRequestHandler):
    packages: ClassVar[dict] = {}

    def do_GET(self):
        name = self.path.lstrip("/").replace("%2f", "/").replace("%2F", "/")
        body = self.packages.get(name)
        self.send_response(200 if body else 404)
        self.end_headers()
        self.wfile.write(json.dumps(body or {"error": "not found"}).encode())

    def log_message(self, *a):
        pass


@pytest.fixture
def world(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "GIT_")):
            monkeypatch.delenv(key)
    home, remotes, project, bindir = (tmp_path / n for n in ("home", "remotes", "project", "bin"))
    for d in (home, remotes, project, bindir):
        d.mkdir()
    claude = bindir / "claude"
    claude.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n')
    claude.chmod(claude.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    git_dir = Path(shutil.which("git")).parent
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{git_dir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("HLOCK_GITHUB_URL", remotes.as_uri())
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "hlock-home"))
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")
    monkeypatch.setenv("HLOCK_HARNESS", "claude-code")

    _Registry.packages = {"@modelcontextprotocol/server-github": {
        "dist-tags": {"latest": "2.0.0"},
        "versions": {v: {"dist": {"integrity": f"sha512-{v}"}} for v in ("1.0.0", "1.4.0", "2.0.0")},
    }}
    server = HTTPServer(("127.0.0.1", 0), _Registry)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    monkeypatch.setenv("HLOCK_NPM_REGISTRY", f"http://127.0.0.1:{server.server_port}")

    make_repo(remotes, "acme/skills", [
        ("v1.2.0", {"skills/sql/SKILL.md": skill("sql", "v1.2"), "rules/style.md": "be terse\n"}),
        ("v1.3.0", {"skills/sql/SKILL.md": skill("sql", "v1.3")}),
        ("v2.0.0", {"skills/sql/SKILL.md": skill("sql", "v2")}),
    ])
    monkeypatch.chdir(project)
    yield {"project": project, "remotes": remotes, "home": home}
    server.shutdown()


def lock() -> dict:
    from hlock import Lockfile

    return json.loads(Lockfile.load("harness.lock").to_json())


def test_add_declares_pins_and_installs(world):
    r = invoke("add", "skill", "github:acme/skills/skills/sql@^1.2")
    assert r.exit_code == 0, r.output
    assert "resolved skill sql" in r.output and "@ v1.3.0" in r.output and "declared skill sql in a new harness.toml" in r.output
    assert Path(".claude/skills/sql/SKILL.md").read_text() == skill("sql", "v1.3")
    assert 'sql = "github:acme/skills/skills/sql@^1.2"' in Path("harness.toml").read_text()
    s = next(x for x in lock()["skill"] if x["name"] == "sql")
    assert s["declared"] and s["constraint"] == "^1.2" and s["resolved-tag"] == "v1.3.0" and len(s["commit"]) == 40
    assert "vendored" not in s and not Path(".hlock/vendor").exists(), "a declared dependency is fetched, never vendored"

    r = invoke("check")
    assert r.exit_code == 0, r.output
    # declared, changed
    Path(".claude/skills/sql/SKILL.md").write_text("edited")
    r = invoke("check")
    assert r.exit_code == 1 and "changed" in r.output and "hlock sync" in r.output, r.output


def test_target_also_puts_servers_in_a_desktop_apps_own_config(world, tmp_path, monkeypatch):
    # Claude Desktop installed on a Mac, not running, with no config yet
    apps = tmp_path / "Applications"
    (apps / "Claude.app/Contents").mkdir(parents=True)
    (apps / "Claude.app/Contents/Info.plist").write_text(
        "<plist><dict><key>CFBundleShortVersionString</key><string>3.1623.1</string></dict></plist>")
    for k, v in {"HLOCK_OS": "macos", "HLOCK_APP_DIRS": str(apps), "HLOCK_APPS_RUNNING": ""}.items():
        monkeypatch.setenv(k, v)
    npx = Path(os.environ["PATH"].split(os.pathsep)[0]) / "npx"
    npx.write_text("#!/bin/sh\necho 10\n")
    npx.chmod(0o755)
    config = world["home"] / "Library/Application Support/Claude/claude_desktop_config.json"
    Path("harness.toml").write_text('[project]\nname = "demo"\n\n[target]\nharness = "claude-code"\nalso = ["claude-desktop"]\n')
    assert invoke("add", "mcp", "npm:@modelcontextprotocol/server-github@^1").exit_code == 0
    assert invoke("trust", "--yes").exit_code == 0
    r = invoke("sync", "--yes")
    # Claude Code is in sync; Claude Desktop waits for --apps, without failing the sync
    assert r.exit_code == 0, r.output
    assert "for hlock sync --apps" in r.output and not config.exists(), r.output
    r = invoke("sync", "--apps", "--yes")
    assert r.exit_code == 0, r.output
    assert json.loads(config.read_text())["mcpServers"]["github"]["command"] == "npx"


def test_dry_run_changes_nothing(world):
    r = invoke("add", "skill", "github:acme/skills/skills/sql@v1.2.0", "--dry-run")
    assert r.exit_code == 0, r.output
    assert "would write" in r.output and "[skills] sql" in r.output and ".claude/skills/sql" in r.output
    assert not Path("harness.toml").exists() and not Path("harness.lock").exists() and not Path(".claude").exists()


def test_mcp_servers_from_npm_and_urls(world):
    r = invoke("add", "mcp", "npm:@modelcontextprotocol/server-github@^1")
    assert r.exit_code == 0, r.output
    servers = json.loads(Path(".mcp.json").read_text())["mcpServers"]
    assert servers["github"] == {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github@1.4.0"]}
    t = next(x for x in lock()["tool"] if x["name"] == "github")
    assert (t["version"], t["integrity"], t["constraint"]) == ("1.4.0", "sha512-1.4.0", "^1")
    r = invoke("add", "mcp", "https://mcp.linear.app/mcp")
    assert r.exit_code == 0, r.output
    assert json.loads(Path(".mcp.json").read_text())["mcpServers"]["linear"]["url"] == "https://mcp.linear.app/mcp"
    assert invoke("check").exit_code == 0, invoke("check").output

    r = invoke("outdated")
    assert r.exit_code == 0 and "github" in r.output and "2.0.0" in r.output, r.output

    r = invoke("remove", "mcp", "github")
    assert r.exit_code == 0, r.output
    assert "github" not in json.loads(Path(".mcp.json").read_text())["mcpServers"]
    assert invoke("check").exit_code == 0


def test_instructions_go_in_the_rules_directory(world):
    r = invoke("add", "instructions", "github:acme/skills/rules/style.md")
    assert r.exit_code == 0, r.output
    assert Path(".claude/rules/style.md").read_text() == "be terse\n"
    assert invoke("check").exit_code == 0


def test_a_teammate_syncs_what_was_added(world, tmp_path, monkeypatch):
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    # another machine: the repository's files, but nothing installed and an empty git cache
    other = tmp_path / "clone"
    other.mkdir()
    for name in ("harness.toml", "harness.lock"):
        shutil.copy(name, other / name)
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "other-home"))
    monkeypatch.chdir(other)
    assert invoke("check").exit_code == 1
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    assert "Fetch skill sql" in r.output
    assert Path(".claude/skills/sql/SKILL.md").read_text() == skill("sql", "v1.3")
    assert invoke("check").exit_code == 0


def test_sync_locks_new_declarations_and_frozen_refuses(world):
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    with open("harness.toml", "a") as f:
        f.write('\n[instructions]\nstyle = "github:acme/skills/rules/style.md"\n')
    r = invoke("check")
    assert r.exit_code == 1 and "unlocked" in r.output, r.output
    r = invoke("sync", "--frozen", "--yes")
    assert r.exit_code == 1 and "--frozen" in r.output, r.output
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    assert "+ instructions style" in r.output
    assert Path(".claude/rules/style.md").is_file()
    assert invoke("check").exit_code == 0


def test_upgrade_dep_moves_to_the_newest_allowed(world):
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    remote = world["remotes"] / "acme/skills.git"
    (remote / "skills/sql/SKILL.md").write_text(skill("sql", "v1.9"))
    git(remote, "commit", "-qam", "1.9")
    git(remote, "tag", "v1.9.0")
    r = invoke("outdated")
    assert "v1.9.0" in r.output and "v2.0.0" in r.output, r.output
    r = invoke("upgrade", "dep:sql")
    assert r.exit_code == 0 and "v1.3.0 → v1.9.0" in r.output, r.output
    assert Path(".claude/skills/sql/SKILL.md").read_text() == skill("sql", "v1.3"), "planning changes nothing"
    r = invoke("upgrade", "dep:sql", "--apply")
    assert r.exit_code == 0, r.output
    assert Path(".claude/skills/sql/SKILL.md").read_text() == skill("sql", "v1.9")
    assert Path("harness.lock.prev").is_file()
    r = invoke("upgrade", "--rollback")
    assert r.exit_code == 0, r.output
    assert next(x for x in lock()["skill"] if x["name"] == "sql")["resolved-tag"] == "v1.3.0"


def test_adopt_declares_what_pin_found(world):
    sk = Path(".claude/skills/local-one")
    sk.mkdir(parents=True)
    (sk / "SKILL.md").write_text(skill("local-one", "mine"))
    assert invoke("pin").exit_code == 0
    r = invoke("add", "--adopt", "local-one")
    assert r.exit_code == 0, r.output
    assert 'local-one = "./.claude/skills/local-one"' in Path("harness.toml").read_text()
    assert invoke("check").exit_code == 0, invoke("check").output


def test_apm_package_and_export(world):
    make_repo(world["remotes"], "acme/base", [
        ("v1.0.0", {"apm.yml": "name: base\nversion: 1.0.0\nlicense: MIT\n", "skills/base-skill/SKILL.md": skill("base-skill", "b")}),
    ])
    make_repo(world["remotes"], "acme/app", [
        ("v1.0.0", {"apm.yml": "name: app\nversion: 1.0.0\ndependencies:\n  apm:\n    - acme/base#^1.0.0\n",
                    ".apm/instructions/style.instructions.md": "terse\n"}),
    ])
    r = invoke("add", "apm:acme/app#^1.0.0")
    assert r.exit_code == 0, r.output
    assert "via app → base" in r.output and "package base depth 2" in r.output, r.output
    assert Path(".claude/skills/base-skill/SKILL.md").is_file() and Path(".claude/rules/style.md").is_file()
    assert lock()["provenance"]["openapm"] == "0.1"

    r = invoke("export", "--format", "apm-lock")
    assert r.exit_code == 0, r.output
    assert "2 packages" in r.output and "not exported: harness" in r.output
    text = Path("apm.lock.yaml").read_text()
    assert text.startswith("# generated_by: hlock") and "repo_url: acme/base" in text and "depth: 2" in text
    assert ".claude/skills/base-skill/SKILL.md: sha256:" in text
    out = invoke("export", "--format", "apm-lock", "-o", "-").output
    assert out.splitlines()[1:] == text.splitlines()[1:]
    assert invoke("check").exit_code == 0

    # an apm.lock.yaml someone updated with `apm install` isn't overwritten
    Path("apm.lock.yaml").write_text(text.replace(next(x["commit"] for x in lock()["package"] if x["name"] == "base"), "f" * 40))
    r = invoke("export", "--format", "apm-lock")
    assert r.exit_code == 1 and "--force" in r.output, r.output


def test_apm_yml_is_a_manifest(world):
    make_repo(world["remotes"], "acme/base", [("v1.0.0", {"SKILL.md": skill("base", "b")})])
    Path("apm.yml").write_text("name: proj\nversion: 0.1.0\ndependencies:\n  apm:\n    - acme/base#v1.0.0\n")
    assert invoke("pin").exit_code == 0
    r = invoke("check")
    assert r.exit_code == 1 and "unlocked" in r.output and "apm.yml" in r.output, r.output
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    assert Path(".claude/skills/base/SKILL.md").is_file()
    assert invoke("check").exit_code == 0


def test_eval_measures_before_keeping(world):
    assert invoke("evals", "init", "--no-skill").exit_code == 0
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    Path("harness.toml").write_text(Path("harness.toml").read_text() + '\n[evals]\npacks = ["./evals"]\nseeds = 1\n')
    # say no: nothing changes
    before = Path("harness.lock").read_text()
    r = invoke("add", "instructions", "github:acme/skills/rules/style.md", "--eval", input="n\n")
    assert "without style" in r.output and "with style" in r.output, r.output
    assert Path("harness.lock").read_text() == before and not Path(".claude/rules/style.md").exists()
    # say yes: kept, with the measurement in the lock
    r = invoke("add", "instructions", "github:acme/skills/rules/style.md", "--eval", "--yes")
    assert r.exit_code == 0, r.output
    ev = next(x for x in lock()["instructions"] if x.get("source", "").endswith("style.md"))["evidence"]
    assert ev["split"] == "search" and ev["trials"] > 0 and "baseline-success-rate" in ev
    assert "cached" in invoke("add", "instructions", "github:acme/skills/rules/style.md", "--eval", input="n\n").output


def test_target_also_installs_portable_deps_into_more_harnesses(world):
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    Path("harness.toml").write_text(Path("harness.toml").read_text().replace('harness = "claude-code"', 'harness = "claude-code"\nalso = ["codex"]'))
    plan = json.loads(invoke("sync", "--json").output)
    also = next(s for s in plan["steps"] if s["id"] == "also:codex:skill:sql")
    assert also["status"] == "auto" and also["title"].startswith("Codex CLI: "), also
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    assert Path(".agents/skills/sql/SKILL.md").read_text() == skill("sql", "v1.3")
    assert Path(".claude/skills/sql/SKILL.md").is_file()


def test_eval_runs_a_code_harness_with_and_without_the_dependency(world, monkeypatch):
    """A project harness that says hello only when the greeter skill is in its skills
    directory: --eval measures the difference, on the harness itself."""
    monkeypatch.delenv("HLOCK_HARNESS")
    make_repo(world["remotes"], "acme/greeter", [("v1.0.0", {"SKILL.md": skill("greeter", "say hello")})])
    Path("agent.sh").write_text('#!/bin/sh\n[ -f "$HLOCK_SKILLS_DIR/greeter/SKILL.md" ] && printf "Hello, world!" > "$1/hello.txt"\nexit 0\n')
    tasks = "".join(f'[[task]]\nid = "hello{i}"\nprompt = "p.md"\nexpected = {{ artifacts = ["hello.txt"] }}\n'
                    f'graders = [{{ type = "artifact", path = "hello.txt", equals = "Hello, world!" }}]\n\n' for i in range(4))
    Path("evals").mkdir()
    Path("evals/p.md").write_text("Write hello.txt.\n")
    Path("evals/pack.toml").write_text(f'[pack]\nname = "hello"\nversion = "0.1.0"\n\n{tasks}')
    Path("harness.toml").write_text(
        '[project]\nname = "p"\n\n[target]\nharness = "agent"\n\n[evals]\npacks = ["./evals"]\nseeds = 1\n\n'
        f'[harnesses.agent]\ncommand = ["sh", "{Path("agent.sh").resolve()}", "{{workspace}}"]\nskills = {{ dirs = [".agent/skills"] }}\n')
    r = invoke("add", "skill", "github:acme/greeter", "--eval", "--yes")
    assert r.exit_code == 0, r.output
    assert "code harness agent" in r.output and "+1.00 success" in r.output, r.output
    ev = next(x for x in lock()["skill"] if x["name"] == "greeter")["evidence"]
    assert (ev["baseline-success-rate"], ev["success-rate"]) == (0.0, 1.0)
    assert Path(".agent/skills/greeter/SKILL.md").is_file()


def test_never_overwrites_what_hlock_didnt_put_there(world):
    """A user-scope instruction file would land on your own CLAUDE.md: refused, untouched."""
    own = world["home"] / ".claude/CLAUDE.md"
    own.parent.mkdir(parents=True)
    own.write_text("MY OWN GLOBAL INSTRUCTIONS\n")
    r = invoke("add", "instructions", "github:acme/skills/rules/style.md", "--scope", "user")
    assert r.exit_code == 1 and "won't replace a file it didn't put there" in r.output, r.output
    assert own.read_text() == "MY OWN GLOBAL INSTRUCTIONS\n"
    # a skill directory that's already there and isn't a dependency
    Path(".claude/skills/sql").mkdir(parents=True)
    Path(".claude/skills/sql/SKILL.md").write_text(skill("sql", "mine"))
    r = invoke("add", "skill", "github:acme/skills/skills/sql@^1.2")
    assert r.exit_code == 1 and "--as" in r.output, r.output
    assert Path(".claude/skills/sql/SKILL.md").read_text() == skill("sql", "mine")


def test_a_credential_in_an_mcp_url_is_refused(world):
    r = invoke("add", "mcp", "https://mcp.example.com/mcp?api_key=sk-ant-api03-0123456789abcdefABCDEF")
    assert r.exit_code == 1 and "credential" in r.output, r.output
    assert not Path("harness.toml").exists()


def test_upgrade_sets_old_copies_aside_outside_the_skill_directory(world):
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    Path("harness.toml").write_text(Path("harness.toml").read_text().replace("@^1.2", "@^2"))
    r = invoke("upgrade", "dep:sql", "--apply")
    assert r.exit_code == 0 and "v1.3.0 → v2.0.0" in r.output, r.output
    assert [p.name for p in Path(".claude/skills").iterdir()] == ["sql"], "no .hlock-bak copy read as a skill"
    assert any(Path(".hlock/bak/.claude/skills").iterdir())
    assert invoke("check").exit_code == 0, invoke("check").output


def test_removing_a_package_keeps_what_another_still_needs(world):
    make_repo(world["remotes"], "acme/base", [("v1.0.0", {"SKILL.md": skill("base", "b")})])
    for app in ("app1", "app2"):
        make_repo(world["remotes"], f"acme/{app}", [("v1.0.0", {"apm.yml": f"name: {app}\nversion: 1.0.0\ndependencies:\n  apm:\n    - acme/base\n"})])
    assert invoke("add", "apm:acme/app1").exit_code == 0
    assert invoke("add", "apm:acme/app2").exit_code == 0
    r = invoke("remove", "package", "app1")
    assert r.exit_code == 0, r.output
    assert Path(".claude/skills/base/SKILL.md").is_file()
    assert [p["name"] for p in lock()["package"]] == ["app2", "base"]
    assert invoke("check").exit_code == 0, invoke("check").output


def test_apm_yml_mcp_servers_are_the_projects_own(world, tmp_path):
    server = tmp_path / "bin" / "my-server"
    server.write_text("#!/bin/sh\n")
    server.chmod(0o755)
    Path("apm.yml").write_text("name: proj\nversion: 0.1.0\ndependencies:\n  mcp:\n    - name: local\n      registry: false\n"
                               "      transport: stdio\n      command: my-server\n")
    assert invoke("pin").exit_code == 0
    # a server declared by a change to apm.yml runs a program: not until it's trusted here
    r = invoke("sync", "--yes")
    assert r.exit_code == 3 and "not yet trusted" in r.output, r.output
    assert not Path(".mcp.json").exists()
    r = invoke("sync", "--yes", "--trust")
    assert r.exit_code == 0, r.output
    assert json.loads(Path(".mcp.json").read_text())["mcpServers"]["local"]["command"] == "my-server"
    assert invoke("check").exit_code == 0, invoke("check").output


def test_resolve_keeps_what_add_pinned(world):
    assert invoke("evals", "init").exit_code == 0
    assert invoke("add", "skill", "github:acme/skills/skills/sql@^1.2").exit_code == 0
    assert invoke("add", "mcp", "npm:@modelcontextprotocol/server-github@^1").exit_code == 0
    manifest = Path("harness.toml")
    manifest.write_text(manifest.read_text() + '\n[evals]\npacks = ["./evals"]\nseeds = 1\n\n'
                        '[candidates.llm.planner]\nany_of = ["anthropic:sonnet", "anthropic:haiku"]\n')
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    lf = lock()
    assert lf["evidence"]["split"] == "holdout"
    s = next(x for x in lf["skill"] if x["name"] == "sql")
    assert s["declared"] and s["resolved-tag"] == "v1.3.0" and len(s["commit"]) == 40
    t = next(x for x in lf["tool"] if x["name"] == "github")
    assert t["declared"] and t["version"] == "1.4.0"
    r = invoke("check")
    assert "not in the lock" not in r.output, r.output
    # a second resolve keeps them too
    assert invoke("resolve").exit_code == 0
    assert next(x for x in lock()["skill"] if x["name"] == "sql")["resolved-tag"] == "v1.3.0"
