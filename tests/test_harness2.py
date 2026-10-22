"""Harness² as a tune optimizer (hlock.optimizers.harness2).

The conversion tests run everywhere. The end-to-end test runs the real Harness² library
(its recursion, selection, library and tree handling) in its own interpreter, set with
HARNESS2_PYTHON; only the model calls and the OpenCode rollout are stubbed, through the
runner's test seams, so it needs no credentials.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

import hlock
from hlock.optimizers.harness2 import bundle_to_tree, tree_to_bundle


def seed(tmp_path: Path, components: list[dict], files: dict[str, str], may_add=("instructions",)) -> Path:
    s = tmp_path / "seed"
    for rel, text in files.items():
        (s / rel).parent.mkdir(parents=True, exist_ok=True)
        (s / rel).write_text(text)
    spec = {"harness": "claude-code", "components": [{"editable": True, "digest": "", **c} for c in components],
            "may_add": list(may_add), "limits": {"max_component_bytes": 15360, "max_changed_lines": 120}}
    (s / "bundle.json").write_text(json.dumps(spec))
    return s


def claude_seed(tmp_path: Path) -> Path:
    return seed(tmp_path, [
        {"id": "instructions/CLAUDE.md", "kind": "instructions", "name": "CLAUDE.md"},
        {"id": "instructions/.claude/rules/guardrails.md", "kind": "instructions", "name": ".claude/rules/guardrails.md", "role": "guardrails"},
        {"id": "skills/triage", "kind": "skill", "name": "triage"},
        {"id": "subagents/reviewer.md", "kind": "subagent", "name": "reviewer"},
        {"id": "scripts/check.py", "kind": "script", "name": "check.py"},
    ], {
        "instructions/CLAUDE.md": "Be terse. Run .agents/scripts/check.py first.\n\n## Memory\n\nThe API is v2.\n",
        "instructions/.claude/rules/guardrails.md": "Never push to main.\n",
        "skills/triage/SKILL.md": "---\nname: triage\ndescription: d\n---\nTriage.\n",
        "subagents/reviewer.md": "---\nname: reviewer\n---\nRead the diff.\n",
        "scripts/check.py": "print('ok')\n",
    }, may_add=("instructions", "skill", "script"))


def test_a_bundle_becomes_harness2s_eight_parts(tmp_path):
    tree, left_out = bundle_to_tree(claude_seed(tmp_path), "oc")
    assert tree["systemprompt.md"] == "Be terse. Run scripts/check.py first.\n"  # its path in Harness²'s tree
    assert tree["guardrails.md"] == "Never push to main.\n"  # from the role file
    assert tree["LongTermMEMORY.md"] == "The API is v2.\n"  # from the section
    assert tree["tool_descriptions/tool_guidance.md"] == ""
    assert tree["skills/triage/SKILL.md"].endswith("Triage.\n")
    assert tree["sub_agents/reviewer/AGENT.md"].endswith("Read the diff.\n")
    assert tree["scripts/check.py"] == "print('ok')\n"
    assert left_out == []
    # Codex has one always-on file: everything is composed into AGENTS.md
    tree, _ = bundle_to_tree(claude_seed(tmp_path), "cx")
    assert tree["AGENTS.md"].startswith("Be terse.") and "## Guardrails\n\nNever push to main." in tree["AGENTS.md"]
    assert ".agents/skills/triage/SKILL.md" in tree and not any(r.startswith("sub_agents/") for r in tree)


def test_an_adopted_tree_comes_back_as_bundle_edits(tmp_path):
    s = claude_seed(tmp_path)
    tree, _ = bundle_to_tree(s, "oc")
    tree["systemprompt.md"] = "Be terse. Run scripts/check.py first, then scripts/lint.py.\n"
    tree["guardrails.md"] = "Never push to main.\nAsk before deleting files.\n"
    tree["tool_descriptions/tool_guidance.md"] = "Prefer rg over grep.\n"
    tree["scripts/lint.py"] = "print('lint')\n"
    tree["plugin/guard.ts"] = "export default {}\n"  # middleware isn't addable here: dropped
    out = tmp_path / "cand"
    changed = tree_to_bundle(tree, s, out, "oc")
    assert changed == ["instructions/.claude/rules/guardrails.md", "instructions/CLAUDE.md", "scripts/lint.py"]
    main = (out / "instructions/CLAUDE.md").read_text()
    # script paths go back to hlock's place for them; a role without a file is a section
    assert main.startswith("Be terse. Run .agents/scripts/check.py first, then .agents/scripts/lint.py.")
    assert "## Memory\n\nThe API is v2." in main and "## Tool guidance\n\nPrefer rg over grep." in main
    assert (out / "instructions/.claude/rules/guardrails.md").read_text() == "Never push to main.\nAsk before deleting files.\n"
    assert not (out / "middleware").exists()
    # and hlock's own check accepts it as a candidate
    sdir = tmp_path / "sess"
    sdir.mkdir()
    assert tree_to_bundle(bundle_to_tree(s, "oc")[0], s, tmp_path / "same", "oc") == []


SEAMS = '''
from pathlib import Path

from harness2.core.proposer import Proposer, Reply


class Scripted(Proposer):
    """Harness²'s editor, scripted: each proposal adds a check to the system prompt."""

    def call(self, mission, context_dir, label, **kwargs):
        context, workspace = Path(context_dir), None
        if label.startswith("experience."):
            text = ("<experience>\\ncue: When the answer needs checking\\nlesson: Tell the agent to check its result.\\n"
                    "evidence: the probe with the check answered OK\\nconfidence: high\\n</experience>")
        elif label.startswith("select."):
            chosen = sorted((context / "candidates").glob("*.md"))[0].stem
            text = f"<why>first</why>\\n```choice\\n{chosen}\\n```"
        else:
            workspace = context / "harness"
            p = workspace / "systemprompt.md"
            p.write_text(p.read_text().rstrip() + "\\nCheck your result before you finish.\\n")
            text = "<notes>Added a check.</notes>"
        reply = Reply(label=label, text=text, ok=True, attempts=1, workspace=workspace)
        self._calls.append(reply)
        return reply


def launcher(adapter):
    """A rollout without OpenCode: answers OK when the harness tells the agent to check."""
    def run(req, install):
        run_dir = adapter.run_dir(req.run_ids[0])
        ws = run_dir / "workspace"
        ws.mkdir(parents=True, exist_ok=True)
        prompt = (Path(install.staging_dir) / "systemprompt.md").read_text()
        answer = "OK" if "Check your result" in prompt else "NO"
        for e in adapter.tasks_by_id[req.task_ref]["expected"]:
            (ws / e).write_text(answer + "\\n")
        (run_dir / "trajectory.md").write_text(f"[ACTION] answer\\n[OBSERVATION]\\n{answer}\\n")
        return [run_dir]
    return run
'''


@pytest.mark.skipif(not os.environ.get("HARNESS2_PYTHON"), reason="set HARNESS2_PYTHON to an interpreter with Harness² installed")
def test_tune_with_harness2_end_to_end(tmp_path, monkeypatch):
    sys.path.insert(0, str(Path(__file__).parent))
    from test_optimize import _Endpoint

    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    (tmp_path / "h2_seams.py").write_text(SEAMS)
    monkeypatch.setenv("PYTHONPATH", str(tmp_path))
    pack = tmp_path / "evals"
    pack.mkdir()
    lines = ['[pack]\nname = "say-ok"\nversion = "0.1.0"\n']
    for i in range(9):
        (pack / f"q{i}.md").write_text("Answer the question.\n")
        lines.append(f'[[task]]\nid = "question_{i}"\nprompt = "q{i}.md"\nexpected = {{ artifacts = ["answer.md"] }}\n'
                     'graders = [{ type = "artifact", path = "answer.md", contains = "OK" }]\n')
    (pack / "pack.toml").write_text("\n".join(lines))
    (tmp_path / "AGENTS.md").write_text("Be careful.\n")
    (tmp_path / "harness.toml").write_text("""[project]
name = "h2"

[evals]
packs = ["./evals"]

[candidates.llm.planner]
any_of = ["reference:qwen-7b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[tune]
optimizer = "harness2"
edit = ["instructions"]
trials = 1
alpha = 0.2

[optimizers.harness2]
options = { k = 1, mode = "parallel", proposer = "h2_seams:Scripted", launcher = "h2_seams:launcher", channel = { kind = "opencode", model = "local/scripted" } }
""")
    endpoint = _Endpoint("Check your result")
    monkeypatch.setenv("HL_LLM_BASE_URL", endpoint.url)
    try:
        report = json.loads(hlock.tune(backend="local", max_trials=2))["report"]
    finally:
        endpoint.server.shutdown()
    assert {"stage": "optimize", "ran": True, "detail": "1 edit accepted"} in report["stages"], report
    d = json.loads(hlock.Lockfile.load(str(tmp_path / "harness.tune.lock")).to_json())
    opt, edit = d["tune"]["optimizer"], d["tune"]["edit"][0]
    assert opt["name"] == "harness2" and opt["evaluator"] == "optimizer"  # Harness² ran its own rollouts
    assert edit["component"] == "instructions/AGENTS.md" and edit["rationale"].startswith("Harness²: Tell the agent to check")
    assert "Check your result before you finish." in (tmp_path / edit["path"]).read_text()
    assert d["tune"]["acceptance"]["accepted"] is True
