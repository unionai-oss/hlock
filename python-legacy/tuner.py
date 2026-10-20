"""The tuner: turns a manifest into an ``harness.lock``.

Search over candidate stacks, run trials, grade, select a winner from the Pareto
front, and serialize the resolved stack plus evidence. Runs on the local backend by
default; devbox/cluster delegate to the Flyte pipeline (``hlock.flyte``).
"""

from __future__ import annotations

import asyncio
import itertools
import shutil
from pathlib import Path
from typing import Any

from harness_lock import backends
from hlock.adapters import ADAPTERS
from hlock.errors import AgentLockError
from harness.lockfile import (
    LLM,
    Context,
    Decision,
    Evidence,
    Harness,
    Instructions,
    Lockfile,
    Policy,
    Provenance,
    Skill,
    Tool,
    Verify,
)
from hlock.manifest import Manifest
from hlock.runcache import TrialCache
from hlock.search import Stack, StackScore, TrialResult, select_winner
from hlock.store import Store
from hlock.tasks import Task, TaskPack


class Tuner:
    def __init__(self, manifest: Manifest, workdir: Path | None = None) -> None:
        self.manifest = manifest
        self.workdir = Path(workdir) if workdir else Path(".hlock")
        self.store = Store(self.workdir / "store")
        self.run_dir = self.workdir / "runs"
        self.cache = TrialCache(self.workdir / "trial-cache.json")
        self.packs: list[TaskPack] = []
        self._pack_by_task: dict[str, TaskPack] = {}

    # ------------------------------------------------------------------ #
    def load_packs(self) -> list[Task]:
        """Load manifest task packs (searched from cwd) and keep search tasks."""
        search_tasks: list[Task] = []
        for ref in self.manifest.task_packs():
            pack = TaskPack.load(ref, base=Path.cwd())
            self.packs.append(pack)
            for t in pack.tasks:
                self._pack_by_task[t.id] = pack
            search, _holdout = pack.split(0.7, seed=self.manifest.budget.random_seed)
            search_tasks.extend(search)
        return search_tasks

    def enumerate_stacks(self) -> list[Stack]:
        """Cartesian product over the manifest's candidate axes (bounded)."""
        c = self.manifest.candidates
        llm_choices = _llm_combos(c.llm) or [{"planner": "reference:qwen-7b"}]
        decision_choices = _fix(c.decision, ["none"])
        harness_choices = _harness_combos(c.harness) or [("reference", "single-agent")]
        policy = c.policy or {}

        stacks: list[Stack] = []
        for llm in llm_choices:
            for decision in decision_choices:
                for harness, pattern in harness_choices:
                    if not ADAPTERS.supports_decision(harness, decision):
                        continue
                    stacks.append(
                        Stack(
                            llm=llm,
                            harness=harness,
                            pattern=pattern,
                            decision=decision,
                            policy=_policy_from_options(policy),
                            tools=list(self.manifest.tools.keys()),
                            skills=list(self.manifest.skills.keys()),
                            instructions=list(self.manifest.instructions.get("files", [])),
                        )
                    )
        # bound to avoid pathological blow-ups in local mode
        max_stacks = 200
        if len(stacks) > max_stacks:
            stacks = stacks[:max_stacks]
        return stacks

    # ------------------------------------------------------------------ #
    def run_trial(self, stack: Stack, task: Task, seed: int, ws_root: Path) -> TrialResult:
        key = f"{stack.hash()}::{task.id}::{seed}"
        cached = self.cache.get(key)
        if cached is not None:
            return TrialResult(**cached)

        ws = ws_root / f"{stack.hash()[:10]}_{task.id}_{seed}"
        if ws.exists():
            shutil.rmtree(ws)
        ws.mkdir(parents=True, exist_ok=True)

        from hlock.traces import TraceRecorder

        recorder = TraceRecorder(stack.hash(), task.id, seed)
        adapter = ADAPTERS.get(stack.harness)
        result = asyncio.run(adapter.run(stack, task, ws, recorder))

        pack = self._pack_by_task.get(task.id)
        grades = pack.grade(task, ws, recorder.trace) if pack else []
        grades_d = [{"grader": g.detail, "success": g.success, "score": g.score} for g in grades]
        success = result.success and all(g.success for g in grades)
        trace = recorder.finish({"artifacts": list(task.expected.get("artifacts", []))}, result.metrics, grades_d)

        tr = TrialResult(
            stack_hash=stack.hash(),
            task_id=task.id,
            seed=seed,
            success=success,
            score=sum(g.score for g in grades) / max(1, len(grades)),
            cost_usd=result.metrics.get("cost_usd", 0.0),
            latency_s=result.metrics.get("latency_s", 0.0),
            steps=result.metrics.get("steps", 0),
            trace=trace.to_dict(),
            stall=False,
        )
        self.cache.put(key, _trial_to_dict(tr))
        self.store.put_text(trace.to_json())
        return tr

    # ------------------------------------------------------------------ #
    def tune(self, backend: backends.Backend | None = None) -> Lockfile:
        search_tasks = self.load_packs()
        stacks = self.enumerate_stacks()
        seeds = range(self.manifest.seeds())

        candidates = [
            backends.Candidate(stack=s, task=t, seed=seed)
            for s in stacks
            for t in search_tasks
            for seed in seeds
        ]
        # respect max-trials
        if self.manifest.budget.max_trials:
            candidates = candidates[: self.manifest.budget.max_trials]

        def run(c: backends.Candidate, ws_root: Path) -> TrialResult:
            return self.run_trial(c.stack, c.task, c.seed, ws_root)

        bk = backend or backends.make_backend(self.manifest.backend.target)

        if bk.target == "local":
            results = asyncio.run(bk.run_sweep(candidates, self.run_dir, run))
        else:
            from hlock.flyte import run_search  # provisional Layer 3

            results = run_search(self, candidates, backend=bk)

        scores = _aggregate(stacks, results)
        winner = select_winner(scores, self.manifest.objective)

        if winner is None:
            raise AgentLockError("no viable stack found — check the manifest search space")

        # write the lock
        lock = _build_lockfile(self, winner, scores)
        lock.save("harness.lock")
        self._write_report(lock, scores)
        return lock

    def _write_report(self, lock: Lockfile, scores: list[StackScore]) -> None:
        lines = ["# harness-lock report", ""]
        lines.append("## Locked stack")
        lines.append(f"```\n{lock.summary()}\n```")
        lines.append("")
        lines.append("## Leaderboard (Pareto front)")
        lines.append("")
        lines.append("| harness | llm | decision | success-rate | cost |")
        lines.append("|---|---|---|---|---|")
        from hlock.search import pareto_front

        for s in pareto_front(scores):
            lines.append(
                f"| {s.stack.harness} | {s.stack.llm.get('planner', '?')} | {s.stack.decision} "
                f"| {s.success_rate:.2f} | ${s.cost_usd:.3f} |"
            )
        lines.append("")
        report = self.workdir / "report.md"
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text("\n".join(lines))


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #
def _trial_to_dict(tr: TrialResult) -> dict[str, Any]:
    return {
        "stack_hash": tr.stack_hash,
        "task_id": tr.task_id,
        "seed": tr.seed,
        "success": tr.success,
        "score": tr.score,
        "cost_usd": tr.cost_usd,
        "latency_s": tr.latency_s,
        "steps": tr.steps,
        "trace": tr.trace,
        "stall": tr.stall,
    }


def _llm_combos(cands: dict[str, Any]) -> list[dict[str, str]]:
    if not cands:
        return []
    roles = list(cands.keys())
    per_role = {}
    for role in roles:
        opts = cands[role].any_of or []
        per_role[role] = [o for o in opts] or ["reference:qwen-7b"]
    combos = list(itertools.product(*(per_role[r] for r in roles)))
    return [dict(zip(roles, combo)) for combo in combos]


def _fix(v: list[str] | None, default: list[str]) -> list[str]:
    return list(v) if v else list(default)


def _harness_combos(cands: list[Any]) -> list[tuple[str, str]]:
    out = []
    for h in cands:
        patterns = h.pattern or ["single-agent"]
        for p in patterns:
            out.append((h.name, p))
    return out


def _policy_from_options(opts: dict[str, list[Any]]) -> dict[str, Any]:
    """Pick the first option for each policy axis (v0.1: fixed per-axis value)."""
    return {k: (v[0] if isinstance(v, list) and v else v) for k, v in opts.items()}


def _aggregate(stacks: list[Stack], results: list[TrialResult]) -> list[StackScore]:
    by_stack: dict[str, list[TrialResult]] = {}
    stack_map = {s.hash(): s for s in stacks}
    for r in results:
        by_stack.setdefault(r.stack_hash, []).append(r)
    scores = []
    for h, trials in by_stack.items():
        s = stack_map.get(h)
        if s is None:
            continue
        valid = [t for t in trials if not t.stall]
        if not valid:
            continue
        sr = sum(1 for t in valid if t.success) / len(valid)
        scores.append(
            StackScore(
                stack=s,
                trials=valid,
                success_rate=sr,
                cost_usd=sum(t.cost_usd for t in valid) / len(valid),
                latency_s=sum(t.latency_s for t in valid) / len(valid),
                n=len(valid),
            )
        )
    return scores


def _build_lockfile(tuner: Tuner, winner: StackScore, scores: list[StackScore]) -> Lockfile:
    m = tuner.manifest
    s = winner.stack
    llms = [
        LLM(
            role=role,
            provider=_split_spec(model)[0],
            model=_split_spec(model)[1],
            resolved_from=model,
            fingerprint=f"sha256:{model}",
        )
        for role, model in s.llm.items()
    ]
    ev = Evidence(
        task_pack=",".join(m.task_packs()),
        split="search",
        trials=winner.n,
        success_rate={"mean": round(winner.success_rate, 4)},
        cost_usd_per_task={"mean": round(winner.cost_usd, 6)},
        latency_s_per_task={"p50": round(winner.latency_s, 2), "p95": round(winner.p95("latency_s"), 2)},
    )
    return Lockfile(
        manifest_hash=m.manifest_hash(),
        stack={"name": m.name, "selection": m.objective.select},
        llms=llms,
        decision=Decision(kind=s.decision, decisions=["retry", "stop"] if s.decision != "none" else []),
        harness=Harness(name=s.harness, pattern=s.pattern),
        tools=[Tool(name=t) for t in s.tools],
        skills=[Skill(name=sk, origin="first-party", editable=True, evidence_digest="") for sk in s.skills],
        instructions=[Instructions(name=i) for i in s.instructions],
        policy=Policy(**{**{f: v for f, v in _default_policy().items()}, **(s.policy or {})}),
        context=Context(isolation=m.context.get("isolation", "locked-only")),
        evidence=ev,
        verify=Verify(),
        provenance=Provenance(backend=m.backend.target, search={"strategy": m.budget.strategy, "candidates": len(scores)}),
    )


def _default_policy() -> dict[str, Any]:
    return {
        "retries": 1,
        "max_steps": 40,
        "human_approval": "never",
        "sandbox": {"enabled": False},
    }


def _split_spec(spec: str) -> tuple[str, str]:
    """Split a ``provider:model`` manifest spec, defaulting provider to reference."""
    if ":" in spec:
        prov, model = spec.split(":", 1)
        return prov, model
    return "reference", spec
