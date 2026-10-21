"""`hlock resolve` (dry run, progress, the stack table, --json) and `hlock tune` (the base
lock, a measured baseline, stages that ran, `tune apply`). See specs/resolve-tune-dx.md."""

import json
import os
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock.cli import main

HARNESS_ENV_PREFIXES = ("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_AGENT", "GEMINI_CLI", "HLOCK_")


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(HARNESS_ENV_PREFIXES):
            monkeypatch.delenv(key)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    assert invoke("evals", "init").exit_code == 0  # three tasks: two to search, one held out
    assert invoke("init", "--pack", "./evals").exit_code == 0
    return tmp_path


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def flat(text: str) -> str:
    """Output with its line wrapping (and panel borders) undone, for phrase matching."""
    return " ".join(text.replace("│", " ").split())


def test_init_writes_kebab_case_budget_keys(project):
    text = (project / "harness.toml").read_text()
    assert "max-trials = 200" in text and "max_trials" not in text


def test_resolve_dry_run_runs_nothing(project):
    r = invoke("resolve", "--dry-run")
    assert r.exit_code == 0, r.output
    assert flat("4 stacks × 2 tasks × 2 seeds = 16 trials") in flat(r.output) and flat(", then 2 holdout trials for the winner") in flat(r.output)
    assert "holdout  count_words" in r.output
    assert not (project / "harness.lock").exists() and not (project / ".hlock/runs").exists()
    plan = json.loads(invoke("resolve", "--dry-run", "--json").output)
    assert len(plan["stacks"]) == 4 and plan["holdout-tasks"] == ["count_words"]


def test_resolve_shows_every_stack_and_the_holdout_check(project):
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    assert "stack" in r.output and "success" in r.output and "★" in r.output
    assert r.output.count("reference/reference:qwen-") >= 4
    assert flat("holdout  reference/reference:qwen-7b/d=none  100% on 2 trials it wasn't picked on") in flat(r.output)
    assert flat("trials        2  (holdout)  low sample (n=2)") in flat(r.output)
    assert "report: .hlock/report.md" in r.output
    # a second run reuses the trials
    r = invoke("resolve")
    assert flat("18 trials came from the cache; --no-cache runs them again") in flat(r.output), r.output
    assert "came from the cache" not in invoke("resolve", "--no-cache").output


def test_resolve_json_has_the_lock_and_the_scores(project):
    r = invoke("resolve", "--json")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["lock"]["evidence"]["split"] == "holdout"
    assert len(out["report"]["stacks"]) == 4 and out["report"]["holdout"]["trials"] == 2


def test_manifest_problems_stop_resolve_before_any_trial(project):
    manifest = project / "harness.toml"
    manifest.write_text(manifest.read_text().replace('select = "min-cost"', 'select = "max-qualty"')
                        .replace("max-trials = 200", "max-trial = 200"))
    r = invoke("resolve")
    assert r.exit_code == 1, r.output
    assert "unknown key `max-trial` in [budget] (did you mean `max-trials`?)" in r.output
    assert 'select = "max-qualty" isn\'t supported' in r.output
    assert not (project / ".hlock/runs").exists()


def test_tune_starts_from_the_lock_and_picks_the_simplest_of_equals(project):
    assert invoke("resolve").exit_code == 0
    r = invoke("tune", "--depth", "hpo")
    assert r.exit_code == 0, r.output
    assert flat("data ✗ needs devbox or cluster, not local") in flat(r.output) and "hpo ✓" in r.output
    assert flat("baseline: the lock's own configuration, measured") in flat(r.output)
    assert "next → hlock tune apply" in r.output
    tuned = json.loads(invoke("--lock", "harness.tune.lock", "show", "--json").output)
    # every configuration scores 100%, so the winner is the one closest to the base lock's
    assert tuned["policy"]["retries"] == 1 and tuned["llm"][0]["params"]["temperature"] == 0.2
    assert tuned["llm"][0]["provider"] == "reference" and tuned["llm"][0]["model"] == "qwen-7b"
    # stages that didn't run aren't recorded; the backend is
    assert set(tuned["tune"]) == {"hpo", "provenance"}
    assert tuned["provenance"]["backend"] == "local" and tuned["evidence"]["latency-s-per-task"]["p50"] > 0


def test_tune_max_trials_caps_the_whole_grid(project):
    assert invoke("resolve").exit_code == 0
    r = invoke("tune", "--depth", "hpo", "--max-trials", "5")
    assert r.exit_code == 0, r.output
    assert flat("--max-trials 5 cut the grid") in flat(r.output)


def test_tune_apply_keeps_the_previous_lock_and_the_target(project):
    assert invoke("resolve").exit_code == 0
    before = (project / "harness.lock").read_text()
    assert invoke("tune", "--depth", "probe").exit_code == 0
    r = invoke("tune", "apply")
    assert r.exit_code == 0, r.output
    assert flat("applied harness.tune.lock to harness.lock  (previous kept in harness.lock.prev)") in flat(r.output)
    assert (project / "harness.lock.prev").read_text() == before
    assert invoke("check").exit_code == 0
    # a tune lock for another harness than [target] is refused
    manifest = project / "harness.toml"
    manifest.write_text(manifest.read_text().replace('# [target]\n# harness = "claude-code"', '[target]\nharness = "pi"'))
    r = invoke("tune", "apply")
    assert r.exit_code == 1 and 'harness.toml targets "pi"' in r.output, r.output
    r = invoke("tune", "apply", "harness.lock.prev")
    assert r.exit_code == 1 and "isn't a tune lock" in r.output


def test_tune_apply_flag_keeps_prev_too(project):
    assert invoke("resolve").exit_code == 0
    r = invoke("tune", "--depth", "probe", "--apply")
    assert r.exit_code == 0, r.output
    assert flat("applied to harness.lock  (previous kept in harness.lock.prev)") in flat(r.output)
    assert Path("harness.lock.prev").is_file()
