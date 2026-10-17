"""Tests for `hlock evals`: the hello-world pack, templates, the Agent Skill, and check."""

import json
import os
import re

import pytest
from click.testing import CliRunner

import harness_lock as hl
from harness_lock import evals
from harness_lock.cli import main

HARNESS_ENV_PREFIXES = ("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_AGENT", "GEMINI_CLI", "HLOCK_")


@pytest.fixture(autouse=True)
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(HARNESS_ENV_PREFIXES):
            monkeypatch.delenv(key)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    return tmp_path


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def fill_in(pack_dir):
    """Do what the skill asks an agent to do: replace every slot with a real task."""
    toml = (pack_dir / "pack.toml").read_text()
    toml = "\n".join(line for line in toml.splitlines() if evals.PLACEHOLDER not in line) + "\n"
    (pack_dir / "pack.toml").write_text(toml)
    for prompt in pack_dir.glob("*/prompt.md"):
        artifact = re.search(r'`([^`]+)`\." Keep', prompt.read_text()).group(1)
        prompt.write_text(f"# A real task\n\nDo the work and write the result to `{artifact}`.\n")


def test_hello_world_pack_runs_end_to_end(project):
    r = invoke("evals", "init")
    assert r.exit_code == 0, r.output
    assert "wrote evals/pack.toml" in r.output and "hlock init --pack ./evals" in r.output
    assert (project / "evals" / "count_words" / "workspace" / "words.txt").is_file()
    assert not (project / ".agents").exists()  # the complete pack needs no skill

    r = invoke("evals", "check")
    assert r.exit_code == 0, r.output
    assert "3 tasks" in r.output and "holdout" in r.output and "ready" in r.output

    assert invoke("init").exit_code == 0
    assert '[evals]\npacks = ["./evals"]' in (project / "harness.toml").read_text()
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    assert 'eval-pack = "./evals"' in (project / "harness.lock").read_text()


def test_template_writes_slots_and_a_skill(project):
    r = invoke("evals", "init", "support-evals", "-t", "support", "--tasks", "7")
    assert r.exit_code == 0, r.output
    assert "7 task slots" in r.output and "hlock-eval-pack" in r.output

    skill = (project / ".agents/skills/hlock-eval-pack/SKILL.md").read_text()
    assert skill.startswith("---\nname: hlock-eval-pack\n")
    assert "hlock evals check support-evals" in skill and "gh issue list" in skill

    result = json.loads(hl.check_eval_pack("support-evals"))
    assert result["domain"] == "support-triage"
    # five kinds, cycled: the sixth and seventh slots are a second triage and reply
    assert result["tasks"] == ["triage_1", "reply_1", "reproduce_1", "escalate_1", "article_1", "triage_2", "reply_2"]

    r = invoke("evals", "check", "support-evals")
    assert r.exit_code == 1
    assert "pack.toml still has 14 TODO(hlock)" in r.output
    assert "triage_1  triage_1/prompt.md still has 2 TODO(hlock)" in r.output

    fill_in(project / "support-evals")
    r = invoke("evals", "check", "support-evals", "--json")
    assert r.exit_code == 0, r.output
    assert [i["level"] for i in json.loads(r.output)["issues"]] == ["warn"]  # 7 < 10 tasks


@pytest.mark.parametrize("template", [t for t in evals.TEMPLATES if t != "hello-world"])
def test_every_template_fills_in_cleanly(project, template):
    assert invoke("evals", "init", "-t", template, "--no-skill").exit_code == 0
    assert not (project / ".agents").exists()
    fill_in(project / "evals")
    result = json.loads(hl.check_eval_pack("evals"))
    assert len(result["tasks"]) == 10
    assert result["issues"] == [], result["issues"]


def test_init_refuses_to_overwrite(project):
    assert invoke("evals", "init").exit_code == 0
    r = invoke("evals", "init", "-t", "coding")
    assert r.exit_code == 1 and "already exists" in r.output
    assert invoke("evals", "init", "-t", "coding", "--force").exit_code == 0


def test_templates_lists_them(project):
    r = invoke("evals", "templates", "--json")
    assert r.exit_code == 0, r.output
    rows = {t["name"]: t for t in json.loads(r.output)}
    assert set(rows) == set(evals.TEMPLATES)
    assert rows["coding"]["kinds"] == ["bug_fix", "feature", "tests", "refactor", "debug"]


def test_init_points_at_evals_init_when_the_pack_is_missing(project):
    r = invoke("init")
    assert r.exit_code == 0, r.output
    assert "no pack.toml found in ./evals" in r.output and "hlock evals init ./evals" in r.output


def test_old_tasks_names_still_work(project):
    assert invoke("evals", "init").exit_code == 0
    (project / "harness.toml").write_text('[tasks]\npacks = ["./evals"]\nseeds = 1\n')
    m = hl.Manifest.load("harness.toml")
    assert m.eval_packs() == m.task_packs() == ["./evals"]
    old = hl.SearchSpace(tasks=json.dumps({"packs": ["./evals"]})).to_manifest("x")
    assert old.eval_packs() == ["./evals"]
