"""Harness² as a tune optimizer: ``[tune] optimizer = "harness2"``.

`Harness² <https://github.com/google-research/harness2>`_ (Google Cloud AI Research and
Northwestern) refines an agent's harness without labels: for each task it proposes edits,
probes them on the same task, composes what helped, and keeps lessons for later tasks. hlock
runs it as a black box: Harness²'s own proposer, selector, library and OpenCode or Codex
executor do the work, in the environment Harness² is installed in, and hlock only translates.

1. The bundle ``tune`` hands an optimizer becomes Harness²'s eight-component tree
   (:func:`bundle_to_tree`): the instruction file is the system prompt; guardrails, memory
   and tool guidance come from role files or sections; subagents, skills, scripts and
   middleware map to their places.
2. ``_harness2_runner.py`` runs Harness² over the train and dev tasks, in Harness²'s Python.
3. Each task's adopted tree comes back as a candidate bundle (:func:`tree_to_bundle`). When
   tasks adopted different harnesses, hlock evaluates them on dev and keeps the best.
4. ``tune`` then checks the candidate as it checks any optimizer's: held-out tasks, noise
   band, significance, cost, and edits that copy the pack.

Options (``[optimizers.harness2] options``): ``python``, an interpreter with Harness²
installed (else ``HARNESS2_PYTHON``); ``substrate``, ``oc`` or ``cx`` (default: ``cx`` for a
Codex lock, else ``oc``); ``k`` (1); ``mode`` (``parallel`` or ``sequential``); ``timeout``
per rollout in seconds (1800); ``work``, a description of the work for Harness²'s prompts.
Harness²'s models come from its own settings (``HARNESS2_SOLVER_MODEL``,
``HARNESS2_IMPROVER_MODEL``, …, or its ``.env``).
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

from hlock.optimizers import Optimizer, OptimizerError

RUNNER = Path(__file__).with_name("_harness2_runner.py")

# Harness²'s always-on files, by role, for each substrate. Codex has one: AGENTS.md.
OC_ROLE_FILES = {"guardrails": "guardrails.md", "memory": "LongTermMEMORY.md", "tool-guidance": "tool_descriptions/tool_guidance.md"}
HLOCK_SCRIPTS = ".agents/scripts/"
H2_SCRIPT = re.compile(r"^[a-z0-9_][a-z0-9_]*\.py$")
H2_NAME = re.compile(r"^[a-z0-9][a-z0-9_\-]*$")

# The headings hlock uses for role sections (hlock_core::components).
HEADINGS = {"guardrails": "## Guardrails", "memory": "## Memory", "tool-guidance": "## Tool guidance"}
ROLES = ("guardrails", "memory", "tool-guidance")


def split_sections(text: str) -> tuple[str, dict[str, str]]:
    """An instruction file's own text and its role sections (as hlock's core splits them)."""
    base, sections, current = [], {}, None
    for line in text.splitlines(keepends=True):
        role = next((r for r in ROLES if line.rstrip() == HEADINGS[r]), None)
        if role:
            current = role
            sections.setdefault(role, "")
        elif current:
            sections[current] += line
        else:
            base.append(line)
    return "".join(base).rstrip(), {r: b.strip() for r, b in sections.items() if b.strip()}


def compose(base: str, sections: dict[str, str]) -> str:
    """An instruction file from its text and role sections (as hlock's core composes them)."""
    out = base.rstrip()
    for role in ROLES:
        body = (sections.get(role) or "").strip()
        if body:
            out += ("\n\n" if out else "") + f"{HEADINGS[role]}\n\n{body}"
    return out + "\n" if out else ""


def _to_h2(text: str) -> str:
    return text.replace(HLOCK_SCRIPTS, "scripts/")


def _from_h2(text: str, scripts: set[str]) -> str:
    for name in sorted(scripts, key=len, reverse=True):
        text = re.sub(rf"(?<![\w./]){re.escape('scripts/' + name)}", HLOCK_SCRIPTS + name, text)
    return text


def _main_file(spec: dict) -> dict | None:
    """The bundle's system prompt: its first instruction file without a role."""
    return next((c for c in spec["components"] if c["kind"] == "instructions" and not c.get("role")), None)


def bundle_to_tree(seed: Path, substrate: str) -> tuple[dict[str, str], list[str]]:
    """The seed bundle as a Harness² tree (rel path -> text), and what it left out."""
    spec = json.loads((seed / "bundle.json").read_text())
    tree: dict[str, str] = {}
    left_out: list[str] = []
    main = _main_file(spec)
    own, sections = split_sections((seed / main["id"]).read_text()) if main else ("", {})
    for c in spec["components"]:
        role = c.get("role")
        if c["kind"] == "instructions" and role:
            sections[role] = (seed / c["id"]).read_text().strip()
        elif c["kind"] == "instructions" and c is not main:
            left_out.append(c["id"])
    if substrate == "cx":
        tree["AGENTS.md"] = _to_h2(compose(own, sections)) or "\n"
    else:
        tree["systemprompt.md"] = _to_h2(own) + "\n"
        for role, rel in OC_ROLE_FILES.items():
            tree[rel] = _to_h2(sections.get(role, "")) + ("\n" if sections.get(role) else "")
    skill_root = ".agents/skills" if substrate == "cx" else "skills"
    for c in spec["components"]:
        path = seed / c["id"]
        if c["kind"] == "skill":
            if not H2_NAME.match(c["name"]):
                left_out.append(c["id"])
                continue
            for f in sorted(p for p in path.rglob("*") if p.is_file()):
                tree[f"{skill_root}/{c['name']}/{f.relative_to(path).as_posix()}"] = _to_h2(f.read_text(errors="replace"))
        elif c["kind"] == "subagent" and substrate == "oc" and H2_NAME.match(c["name"]):
            tree[f"sub_agents/{c['name']}/AGENT.md"] = _to_h2(path.read_text(errors="replace"))
        elif c["kind"] == "script" and H2_SCRIPT.match(c["name"]):
            tree[f"scripts/{c['name']}"] = path.read_text(errors="replace")
        elif c["kind"] == "middleware" and substrate == "oc" and path.name.endswith(".ts") and H2_NAME.match(c["name"]):
            tree[f"plugin/{c['name']}.ts"] = path.read_text(errors="replace")
        elif c["kind"] == "middleware" and substrate == "cx" and path.name == "hooks.json":
            tree[".codex/hooks.json"] = path.read_text(errors="replace")
        elif c["kind"] in ("subagent", "script", "middleware"):
            left_out.append(c["id"])
    return tree, left_out


def tree_to_bundle(tree: dict[str, str], seed: Path, out: Path, substrate: str) -> list[str]:
    """Write Harness²'s adopted ``tree`` back as a bundle at ``out`` (a copy of the seed with
    its edits), and return the components it changed."""
    spec = json.loads((seed / "bundle.json").read_text())
    shutil.rmtree(out, ignore_errors=True)
    shutil.copytree(seed, out)
    scripts = {Path(r).name for r in tree if r.startswith("scripts/") and H2_SCRIPT.match(Path(r).name)}
    scripts |= {c["name"] for c in spec["components"] if c["kind"] == "script"}
    back = lambda t: _from_h2(t, scripts)
    may_add = set(spec.get("may_add") or [])
    changed: set[str] = set()

    def put(rel: str, text: str) -> None:
        p = out / rel
        old = p.read_text() if p.is_file() else None
        if old != text:
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
            changed.add(rel.split("/")[0] + "/" + rel.split("/")[1] if rel.startswith("skills/") else rel)

    # always-on text: the system prompt, and each role in its file or as a section
    main = _main_file(spec)
    if substrate == "cx":
        own, sections = split_sections(back(tree.get("AGENTS.md", "")))
    else:
        own = back(tree.get("systemprompt.md", "")).rstrip()
        sections = {role: back(tree.get(rel, "")).strip() for role, rel in OC_ROLE_FILES.items()}
    role_files = {c["role"]: c for c in spec["components"] if c["kind"] == "instructions" and c.get("role")}
    keep_in_main = {}
    for role in ROLES:
        if role in role_files:
            body = sections.get(role, "")
            put(role_files[role]["id"], body + "\n" if body else "")
        elif sections.get(role):
            keep_in_main[role] = sections[role]
    if main:
        put(main["id"], compose(own, keep_in_main))

    # skills, subagents, scripts, middleware
    skill_root = ".agents/skills/" if substrate == "cx" else "skills/"
    names = {c["name"]: c for c in spec["components"] if c["kind"] == "skill"}
    for rel, text in tree.items():
        if rel.startswith(skill_root):
            name, _, sub = rel[len(skill_root):].partition("/")
            if name in names or "skill" in may_add:
                put(f"skills/{name}/{sub}", back(text))
        elif rel.startswith("sub_agents/") and rel.endswith("/AGENT.md"):
            name = rel.split("/")[1]
            existing = next((c for c in spec["components"] if c["kind"] == "subagent" and c["name"] == name), None)
            if existing or "subagent" in may_add:
                put(existing["id"] if existing else f"subagents/{name}.md", back(text))
        elif rel.startswith("scripts/") and H2_SCRIPT.match(Path(rel).name):
            name = Path(rel).name
            existing = any(c["kind"] == "script" and c["name"] == name for c in spec["components"])
            if existing or "script" in may_add:
                put(f"scripts/{name}", text)
        elif rel.startswith("plugin/") or rel == ".codex/hooks.json":
            name = "hooks" if rel == ".codex/hooks.json" else Path(rel).stem
            existing = next((c for c in spec["components"] if c["kind"] == "middleware" and c["name"] == name), None)
            if existing or "middleware" in may_add:
                put(existing["id"] if existing else f"middleware/{Path(rel).name}", text)
    return sorted(changed)


def _python(opts: dict) -> str:
    py = opts.get("python") or os.environ.get("HARNESS2_PYTHON")
    if not py:
        raise OptimizerError("the harness2 optimizer needs Harness²'s Python: set [optimizers.harness2] options.python "
                           "or HARNESS2_PYTHON to the interpreter it's installed in (see its setup guide)")
    return os.path.expanduser(py)


class Harness2Optimizer(Optimizer):
    """Runs ``harness.optimize`` with Harness², as a black box."""

    name = "harness2"
    about = "Harness² (google-research/harness2), run as a black box in its own environment"

    def optimize(self, request: dict) -> dict:
        from hlock.optimize import BudgetExhausted, Session

        opts = (request.get("optimizer") or {}).get("options") or {}
        opt = request.get("optimize") or {}
        seed = Path(opt["seed"])
        spec = json.loads((seed / "bundle.json").read_text())
        substrate = opts.get("substrate") or ("cx" if spec.get("harness") == "codex" else "oc")
        if substrate not in ("oc", "cx"):
            raise OptimizerError(f"[optimizers.harness2] options.substrate must be oc or cx, not {substrate!r}")
        ws = Path(request.get("workdir") or ".").resolve()
        ws.mkdir(parents=True, exist_ok=True)
        tree, left_out = bundle_to_tree(seed, substrate)
        if left_out:
            sys.stderr.write(f"[harness2] not editable by Harness²: {', '.join(left_out)}\n")
        meta = opt.get("task_meta") or {}
        ids = list(dict.fromkeys((opt.get("tasks") or {}).get("train", []) + (opt.get("tasks") or {}).get("dev", [])))
        job = {
            "root": str(ws / "h2"),
            "substrate": substrate,
            "tree": tree,
            "tasks": [{"id": t, "prompt": (meta.get(t) or {}).get("prompt_text", ""), "fixtures": (meta.get(t) or {}).get("fixtures"),
                       "expected": (meta.get(t) or {}).get("expected") or []} for t in ids],
            "domain": opts.get("domain") or "project",
            "work": opts.get("work") or "the tasks this project's agent is given",
            "k": int(opts.get("k") or 1),
            "mode": opts.get("mode") or "parallel",
            "timeout": int(opts.get("timeout") or 1800),
        }
        for key in ("proposer", "launcher", "channel"):  # test seams
            if opts.get(key):
                job[key] = opts[key]
        (ws / "job.json").write_text(json.dumps(job, indent=2))
        result_path = ws / "result.json"
        argv = [_python(opts), str(RUNNER), str(ws / "job.json"), str(result_path)]
        proc = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=sys.stderr, check=False)
        if proc.returncode != 0 or not result_path.is_file():
            raise OptimizerError(f"Harness² failed (exit {proc.returncode}); its log is above, and its runs are in {ws / 'h2'}")
        result = json.loads(result_path.read_text())

        # each task's adopted harness is a candidate; the same tree twice is one
        candidates: list[tuple[Path, list[str], dict]] = []
        seen: set[str] = set()
        for i, a in enumerate(r for r in result.get("adopted") or [] if not r.get("base") and r.get("tree")):
            key = json.dumps(a["tree"], sort_keys=True)
            if key in seen:
                continue
            seen.add(key)
            out = ws / "candidates" / f"c{i}"
            changed = tree_to_bundle(a["tree"], seed, out, substrate)
            if changed:
                candidates.append((out, changed, a))
        session = Session(opt["session"])
        best, note = (seed, [], {}), "no task adopted an edited harness"
        if len(candidates) == 1:
            best, note = candidates[0], "the one harness Harness² adopted"
        elif candidates:
            def rate(c) -> float:
                rs = session.evaluate(c[0], "dev")["results"]
                return sum(r["success"] for r in rs) / max(len(rs), 1)

            scored = []
            for c in candidates:
                try:
                    scored.append((rate(c), c))
                except BudgetExhausted:
                    break
            if scored:
                best = max(scored, key=lambda s: s[0])[1]
                note = f"the best on dev of {len(candidates)} harnesses Harness² adopted"
            else:
                best, note = candidates[-1], "the last harness Harness² adopted (no budget to compare them)"
        out, changed, adopted = best
        exp = adopted.get("experience") or {}
        why = "; ".join(x for x in (exp.get("lesson"), exp.get("cue")) if x) or adopted.get("why") or note
        ledger = ws / "ledger.jsonl"
        ledger.write_text("".join(json.dumps({"component": c, "op": "modify", "rationale": f"Harness²: {why}",
                                              "native": {"task": adopted.get("task"), "label": adopted.get("label"), "picked": note}}) + "\n"
                                  for c in changed))
        return {
            "success": True,
            "score": 1.0,
            "candidate": str(out),
            "ledger": str(ledger),
            "optimizer": {"name": "harness2", "version": result.get("version") or "", "proposer": os.environ.get("HARNESS2_IMPROVER_MODEL") or "Harness² default"},
            "usage": {"tasks": result.get("done", 0), "failed": result.get("failed", 0),
                      "harness2_usd": round(sum(float(a.get("usd") or 0) for a in result.get("adopted") or []), 6)},
        }
