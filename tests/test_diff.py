"""`hlock diff`: the text behind a lock's changed digests (specs/reviewable-diffs.md)."""

import hashlib
import json
import subprocess

import pytest
from click.testing import CliRunner

import hlock
from hlock.cli import main

LOCK_HEAD = """lock-version = 1
generated-by = "test"
generated-at = ""

[harness]
name = "claude-code"
"""


def digest(text: str) -> str:
    return "sha256:" + hashlib.sha256(text.encode()).hexdigest()


def write_lock(path, text: str, vendored: str, success: float = 0.5) -> None:
    (path.parent / vendored).parent.mkdir(parents=True, exist_ok=True)
    (path.parent / vendored).write_text(text)
    path.write_text(LOCK_HEAD + f"""
[[instructions]]
name = "CLAUDE.md"
digest = "{digest(text)}"
scope = "project"
vendored = "{vendored}"

[evidence]
trials = 4

[evidence.success-rate]
mean = {success}
""")


@pytest.fixture
def project(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    write_lock(tmp_path / "harness.lock.prev", "Be brief.\nRun tests.\n", ".hlock/vendor/a/CLAUDE.md", 0.5)
    write_lock(tmp_path / "harness.lock", "Be brief.\nRun the tests first.\n", ".hlock/vendor/b/CLAUDE.md", 0.75)
    return tmp_path


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def test_diff_shows_the_text_and_the_evidence(project):
    r = invoke("diff")
    assert r.exit_code == 0, r.output
    assert "~ instructions CLAUDE.md" in r.output
    assert "-Run tests." in r.output and "+Run the tests first." in r.output
    assert "~ evidence success-rate: 0.50 → 0.75" in r.output
    assert "1 instruction file changed" in r.output


def test_diff_json_and_stat(project):
    d = json.loads(invoke("diff", "--json").output)
    c = d["components"][0]
    assert (c["kind"], c["change"], c["new_from"]) == ("instructions", "~", "vendored")
    assert c["files"][0]["added"] == 1 and c["files"][0]["removed"] == 1
    r = invoke("diff", "--stat")
    assert "+1 -1" in r.output and "+Run the tests first." not in r.output


def test_diff_markdown_for_a_pr_comment(project):
    r = invoke("diff", "--format", "markdown")
    assert r.exit_code == 0, r.output
    assert r.output.startswith("### Lock changes:")
    assert "- success-rate: 0.50 → 0.75" in r.output
    assert "<details><summary>instructions CLAUDE.md: `CLAUDE.md` modified (+1 −1)</summary>" in r.output
    assert "```diff" in r.output


def test_diff_never_shows_content_that_does_not_match(project):
    (project / ".hlock/vendor/b/CLAUDE.md").write_text("edited by hand\n")
    r = invoke("diff")
    assert r.exit_code == 0
    assert "new content unavailable" in r.output and "edited by hand" not in r.output
    assert invoke("diff", "--ci").exit_code == 1


def test_diff_against_a_git_revision(project):
    git = lambda *a: subprocess.run(["git", *a], cwd=project, check=True, capture_output=True)
    git("init", "-q")
    git("-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "empty")
    git("add", "harness.lock", ".hlock/vendor/b")
    git("-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "lock")
    write_lock(project / "harness.lock", "Be brief.\nRun the tests first.\nThen comment.\n", ".hlock/vendor/c/CLAUDE.md")
    d = json.loads(hlock.lock_diff("harness.lock", old="HEAD"))
    assert d["old"] == "HEAD:harness.lock"
    assert "+Then comment." in d["components"][0]["files"][0]["hunks"]


def test_pin_points_at_diff_when_content_changed(project, monkeypatch):
    from hlock import cli

    printed = []
    monkeypatch.setattr(cli._Project, "lock", project / "harness.lock")
    monkeypatch.setattr(cli.console, "print", lambda *a, **k: printed.append(str(a[0]) if a else ""))
    cli._diff_hint(["~ skill review (project): a6cec63126 → 0b91f2e7c4"], str(project / "harness.lock"))
    cli._diff_hint(["~ llm default: anthropic:sonnet → anthropic:opus"], str(project / "harness.lock"))
    assert printed == ["[muted]see the content:[/] [cmd]hlock diff[/]"]
