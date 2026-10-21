"""Tests for `hlock evals`: the hello-world pack, templates, the Agent Skill, and check."""

import json
import os
import re

import pytest
from click.testing import CliRunner

import hlock as hl
from hlock import evals
from hlock.cli import main

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
        artifact = re.search(r'`([^`]+)`\." Keep', prompt.read_text())
        done = f"write the result to `{artifact.group(1)}`" if artifact else "make the tests pass"
        prompt.write_text(f"# A real task\n\nDo the work and {done}.\n")
    for check in pack_dir.glob("*/check.py"):
        check.write_text("import sys\n\nsys.exit(0)\n")
    for rubric in pack_dir.glob("*/rubric.md"):
        rubric.write_text("- Gets the facts right.\n")


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
    lock = (project / "harness.lock").read_text()
    assert 'eval-pack = "./evals"' in lock
    # the reference harness leaves each task's solution, so the content graders pass
    assert "[evidence.success-rate]\nmean = 1.0" in lock


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


def test_hello_world_graders_check_content(project):
    assert invoke("evals", "init").exit_code == 0
    r = invoke("evals", "check", "--run")
    assert r.exit_code == 0, r.output
    assert "solution fails" not in r.output and "without any work" not in r.output

    r = invoke("evals", "grade", "greet_people")
    assert r.exit_code == 0, r.output
    assert "its solution · score 1.00" in r.output and "✓ tests:greet_people/check.py" in r.output

    run = project / "run"
    run.mkdir()
    (run / "names.csv").write_text("name\nAda\nGrace\nLinus\n")
    (run / "greetings.md").write_text("Hello, Ada!\nHi, Grace!\n")
    r = invoke("evals", "grade", "greet_people", "run", "--json")
    assert r.exit_code == 1, r.output
    tests = next(g for g in json.loads(r.output)["grades"] if g["grader"].startswith("tests:"))
    assert tests["score"] == pytest.approx(1 / 3) and "1 of 3 lines right" in tests["detail"]
    assert sorted(p.name for p in run.iterdir()) == ["greetings.md", "names.csv"]  # graded a copy

    r = invoke("evals", "grade", "count_words", "run")
    assert r.exit_code == 1 and "count.txt is missing" in r.output

    # a wrong solution is caught
    (project / "evals/count_words/solution/count.txt").write_text("10\n")
    r = invoke("evals", "check", "--run")
    assert r.exit_code == 1
    assert 'count_words  its solution fails artifact:count.txt: count.txt should be "9", but is "10"' in r.output


def test_check_run_says_when_it_cant_run_the_graders(project):
    assert invoke("evals", "init", "-t", "support", "--no-skill").exit_code == 0
    r = invoke("evals", "check", "--run", "--json")
    assert r.exit_code == 1
    issues = json.loads(r.output)["issues"]
    assert {"level": "warn", "task": None, "message": "the graders weren't run; fix the errors first"} in issues


def test_grade_reports_unknown_tasks_and_prints_grader_names_as_is(project):
    assert invoke("evals", "init").exit_code == 0
    r = invoke("evals", "grade", "nope")
    assert r.exit_code == 1 and 'no task "nope"' in r.output and "hello_file" in r.output
    toml = (project / "evals" / "pack.toml").read_text()
    (project / "evals" / "pack.toml").write_text(toml.replace(
        'graders = [{ type = "artifact", path = "hello.txt", equals = "Hello, world!" }]',
        'graders = [{ type = "tests", command = "test -s hello.txt  # [bold]x[/bold]" }]'))
    r = invoke("evals", "grade", "hello_file")
    assert r.exit_code == 0, r.output
    assert "✓ tests:test -s hello.txt  # [bold]x[/bold]" in r.output


def test_template_slots_have_graders(project):
    assert invoke("evals", "init", "-t", "coding", "--no-skill", "--tasks", "5").exit_code == 0
    toml = (project / "evals" / "pack.toml").read_text()
    assert 'graders = ["tests:bug_fix_1/check.py"]' in toml and 'graders = ["judge:debug_1/rubric.md"]' in toml
    assert 'solution = "bug_fix_1/solution/"' in toml and f'model = "{evals.JUDGE_MODEL}"' in toml
    r = invoke("evals", "check")
    assert "bug_fix_1  tests grader file bug_fix_1/check.py still has 2 TODO(hlock)" in r.output
    assert "debug_1  judge grader file debug_1/rubric.md still has 2 TODO(hlock)" in r.output


def test_a_judge_in_the_manifest_counts(project):
    assert invoke("evals", "init", "-t", "research", "--no-skill").exit_code == 0
    fill_in(project / "evals")
    toml = (project / "evals" / "pack.toml").read_text()
    (project / "evals" / "pack.toml").write_text(toml.replace("[pack.judge]", "").replace(f'model = "{evals.JUDGE_MODEL}"', ""))
    assert "judge graders need a model" in invoke("evals", "check").output
    (project / "harness.toml").write_text('[evals]\npacks = ["./evals"]\n\n[evals.judge]\nmodel = "openai:gpt-x"\n')
    r = invoke("evals", "check")
    assert r.exit_code == 0 and "judge graders need a model" not in r.output, r.output


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


# ---- pack.toml keys, finding the pack, hints ----------------------------------------


def test_check_reports_unknown_keys(project):
    invoke("evals", "init")
    toml = (project / "evals/pack.toml").read_text()
    toml = toml.replace("timeout-seconds = 300", "timeout_seconds = 300", 1).replace("tags = [\"files\"]", "tag = [\"files\"]", 1)
    (project / "evals/pack.toml").write_text(toml + '\n[pack.judge]\nmodle = "x"\n')
    r = invoke("evals", "check")
    assert r.exit_code == 1, r.output
    assert "hello_file  unknown key `timeout_seconds` in [[task]] (did you mean `timeout-seconds`?)" in r.output
    assert "count_words  unknown key `tag` in [[task]] (did you mean `tags`?)" in r.output
    assert "unknown key `modle` in [pack.judge] (did you mean `model`?)" in r.output


def test_evals_commands_find_the_pack(project):
    invoke("evals", "init", "tasks")
    (project / "harness.toml").write_text('[project]\nname = "p"\n\n[evals]\npacks = ["./tasks"]\n')
    r = invoke("evals", "check")
    assert r.exit_code == 0 and "3 tasks · tasks" in r.output, r.output
    assert invoke("evals", "grade", "greet_people").exit_code == 0
    os.chdir(project / "tasks")  # inside the pack, it's this directory
    r = invoke("evals", "check")
    assert r.exit_code == 0 and "3 tasks · ." in r.output, r.output


def test_pack_errors_get_a_hint_that_fits(project):
    r = invoke("evals", "check", "./nope")
    assert r.exit_code == 1 and "hint: write one with hlock evals init ./nope" in r.output, r.output
    assert "harness.toml" not in r.output
    (project / "bad").mkdir()
    (project / "bad/pack.toml").write_text("[[task]\n")
    r = invoke("evals", "check", "bad")
    assert r.exit_code == 1 and "TOML parse error" in r.output and "hint:" not in r.output, r.output


# ---- hlock evals run ----------------------------------------------------------------

AGENT = """#!/bin/sh
# does hello_file right, gets count_words wrong, never finishes greet_people
case "$1" in
  *hello.txt*) printf 'Hello, world!\\n' > hello.txt ;;
  *words*) echo 8 > count.txt ;;
  *names*) sleep 30 ;;
esac
echo "ran in $(pwd)"
"""


@pytest.fixture
def code_harness(project):
    invoke("evals", "init")
    toml = (project / "evals/pack.toml").read_text()
    (project / "evals/pack.toml").write_text(toml.replace("timeout-seconds = 300", "timeout-seconds = 2"))
    (project / "agent.sh").write_text(AGENT)
    (project / "agent.sh").chmod(0o755)
    (project / "harness.toml").write_text('[project]\nname = "p"\n\n[target]\nharness = "loop"\n\n'
                                          '[harnesses.loop]\ncommand = ["./agent.sh", "{prompt}"]\n\n'
                                          '[evals]\npacks = ["./evals"]\n')
    assert invoke("pin").exit_code == 0
    return project


def test_evals_run_needs_a_locked_agent(project):
    invoke("evals", "init")
    r = invoke("evals", "run")
    assert r.exit_code == 1 and "hint: pin your setup with hlock pin" in r.output, r.output
    invoke("init", "--pack", "./evals")
    assert invoke("resolve").exit_code == 0
    r = invoke("evals", "run")
    assert r.exit_code == 1 and "simulated reference harness" in r.output, r.output


def test_evals_run_runs_and_grades_each_task(code_harness, tmp_path_factory):
    work = tmp_path_factory.mktemp("runs")
    r = invoke("evals", "run", "--trust", "--workdir", str(work))
    assert r.exit_code == 1, r.output
    assert "✓ hello_file  1.00" in r.output
    assert "✗ count_words  0.00" in r.output and 'count.txt should be "9", but is "8"' in r.output
    assert "stopped after the task's 2s timeout" in r.output
    assert "✗ 1 of 3 runs passed  (loop, harness.lock)" in r.output
    assert f"next → hlock evals grade count_words {work / 'count_words-0'}" in r.output.replace("\n", "")
    # workspaces are kept, with the fixtures, outside the project; the log sits next to each
    assert (work / "count_words-0/words.txt").is_file() and (work / "hello_file-0/hello.txt").is_file()
    assert f"ran in {(work / 'hello_file-0').resolve()}" in (work / "hello_file-0.log").read_text()
    # an earlier run's workspace isn't overwritten
    r = invoke("evals", "run", "hello_file", "--trust", "--workdir", str(work))
    assert r.exit_code == 1 and "is left from an earlier run" in r.output, r.output


def test_evals_run_json_and_task_selection(code_harness):
    r = invoke("evals", "run", "hello_file", "--seeds", "2", "--trust", "--json")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["harness"] == "loop" and [(x["task"], x["seed"]) for x in out["runs"]] == [("hello_file", 0), ("hello_file", 1)]
    assert all(x["success"] and x["score"] == 1.0 and x["grade"]["success"] for x in out["runs"])
    assert not out["workdir"].startswith(str(code_harness))  # a temporary directory outside the project
    r = invoke("evals", "run", "nope")
    assert r.exit_code == 1 and "no task 'nope'" in r.output and "hello_file, count_words, greet_people" in r.output


# ---- the lock's evidence names the pack ----------------------------------------------


def test_evidence_names_the_pack_and_check_notices_a_change(project):
    invoke("evals", "init")
    invoke("init", "--pack", "./evals")
    assert invoke("resolve").exit_code == 0
    ev = json.loads(invoke("show", "--json").output)["evidence"]
    assert ev["eval-pack"] == "./evals" and ev["eval-pack-version"] == f"{project.name}/evals@0.1.0"
    assert ev["eval-pack-digest"]
    assert "eval pack" in invoke("show").output
    r = invoke("check")
    assert r.exit_code == 0 and "eval packs changed" not in r.output, r.output
    # a prompt is part of the pack: editing one makes the evidence describe an older pack
    (project / "evals/hello_file/prompt.md").write_text("Write hello.txt, politely.\n")
    r = invoke("check")
    assert r.exit_code == 0, r.output
    assert "the eval packs changed since harness.lock's evidence was measured" in r.output
    assert "re-measure with hlock resolve" in r.output
