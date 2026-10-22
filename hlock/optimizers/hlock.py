"""The built-in optimizer, ``[tune] optimizer = "hlock"`` (the default).

A small reflective loop (``specs/tune-optimizers.md`` §3.8), so ``tune`` can edit the harness
with nothing else installed. Anything better is an optimizer of its own.
"""

from __future__ import annotations

import json
import shutil
import sys
from pathlib import Path

from hlock.optimizers import Optimizer

# Guidance the built-in optimizer tries, in order, when it has no proposer model.
GUIDANCE = (
    "Read every file in the working directory before you answer.",
    "Check your result against what the task asks for before you finish.",
    "Reply with only what the task asks for, in the format it asks for.",
    "When a step fails, say why, then try a different approach.",
)


def _component_file(bundle: Path, comp: dict) -> Path:
    """The file the built-in optimizer edits for a component: an instruction file, or a
    skill's SKILL.md."""
    path = bundle / comp["id"]
    return path / "SKILL.md" if comp["kind"] == "skill" else path


def _propose(text: str, failures: list[dict], lines: int, proposer: str | None, step: int) -> tuple[str, str] | None:
    """Up to `lines` lines of guidance to add to `text`, and why: from the proposer model
    when one is configured, else the next of GUIDANCE that `text` doesn't have."""
    from hlock.tasks.worker import _complete, _llm_configured, _unfence

    why = "; ".join(sorted({f["feedback"] for f in failures})[:3]) or "no failures on the train split"
    if proposer and _llm_configured():
        provider, _, model = proposer.partition(":")
        if not model:
            provider, model = "reference", proposer
        ask = (
            "You improve an AI agent's instructions. Its current instruction file is between the markers.\n"
            f"<<<\n{text}\n>>>\nOn its training tasks these runs failed: {why}.\n"
            f"Write at most {lines} new lines of general guidance that would help on tasks like these. "
            "Don't mention specific tasks, file names or answers. Reply with only the lines to add."
        )
        try:
            got = _complete(model, ask, 0.7, max_tokens=300, provider=provider)
        except Exception as exc:  # noqa: BLE001 - the step proposes nothing, and the search goes on
            sys.stderr.write(f"[hlock optimizer] the proposer failed ({exc})\n")
            return None
        add = [ln.rstrip() for ln in _unfence(got["content"]).splitlines() if ln.strip()][:lines]
        return ("\n".join(add), f"proposed by {proposer} from: {why}") if add else None
    for i in range(len(GUIDANCE)):
        line = GUIDANCE[(step + i) % len(GUIDANCE)]
        if line not in text:
            return line, f"built-in guidance, tried after: {why}"
    return None


class HlockOptimizer(Optimizer):
    """It runs the incumbent on the train split through the session, picks an editable
    component in turn, asks for a few lines of guidance from the failures, reruns the same
    tasks, and keeps the edit when it does better there and no worse on dev. The lines it
    may add per step decay from 3 to 1. It stops at the session's budget.
    """

    name = "hlock"
    about = "hlock's built-in reflective loop: bounded edits to instruction files and skills"

    def optimize(self, request: dict) -> dict:
        from hlock import __version__
        from hlock.optimize import BudgetExhausted, Session

        opt = request.get("optimize") or {}
        session = Session(opt["session"])
        seed = Path(opt["seed"])
        spec = json.loads((seed / "bundle.json").read_text())
        editable = [c for c in spec["components"] if c.get("editable")]
        ws = Path(request.get("workdir") or ".")
        ws.mkdir(parents=True, exist_ok=True)
        trials = int(opt.get("trials") or 1)
        proposer = (opt.get("models") or {}).get("proposer")
        ledger_path = ws / "ledger.jsonl"
        ledger: list[dict] = []
        incumbent = ws / "c0"
        shutil.rmtree(incumbent, ignore_errors=True)
        shutil.copytree(seed, incumbent)

        def rate(out: dict) -> float:
            rs = out["results"]
            return sum(1.0 for r in rs if r["success"]) / max(len(rs), 1)

        steps = 0
        try:
            inc_train = session.evaluate(incumbent, "train", trials=trials)
            inc_dev = None
            for step in range(4 * max(len(editable), 1)):
                if not editable:
                    break
                comp = editable[step % len(editable)]
                path = _component_file(incumbent, comp)
                text = path.read_text() if path.is_file() else ""
                failures = [r for r in inc_train["results"] if not r["success"]]
                got = _propose(text, failures, max(1, 3 - step // 2), proposer, step)
                if got is None:
                    continue
                add, why = got
                steps += 1
                cand = ws / f"c{step + 1}"
                shutil.rmtree(cand, ignore_errors=True)
                shutil.copytree(incumbent, cand)
                target = _component_file(cand, comp)
                body = text.rstrip("\n")
                target.write_text(f"{body}\n\n{add}\n" if body else f"{add}\n")
                cand_train = session.evaluate(cand, "train", trials=trials)
                if rate(cand_train) <= rate(inc_train):
                    continue
                inc_dev = inc_dev or session.evaluate(incumbent, "dev", trials=trials)
                cand_dev = session.evaluate(cand, "dev", trials=trials)
                if rate(cand_dev) < rate(inc_dev):
                    continue
                ledger.append({
                    "component": comp["id"], "op": "modify", "rationale": why,
                    "evidence": {"split": "dev", "success_delta": round(rate(cand_dev) - rate(inc_dev), 4)},
                })
                incumbent, inc_train, inc_dev = cand, cand_train, cand_dev
        except BudgetExhausted as exc:
            sys.stderr.write(f"[hlock optimizer] {exc}\n")
        ledger_path.write_text("".join(json.dumps(e) + "\n" for e in ledger))
        return {
            "success": True,
            "score": 1.0,
            "steps": steps,
            "candidate": str(incumbent),
            "ledger": str(ledger_path),
            "optimizer": {"name": "hlock", "version": __version__, "proposer": proposer or "built-in guidance"},
            "usage": {"proposals": steps},
        }
