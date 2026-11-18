"""Deletion first: ``[tune] optimizer = "ablate"``.

Anthropic's "Prompting Claude Fable 5.2" guide finds the biggest wins in moving a harness
to a newer model come from deleting text written for an earlier one (think-less lines,
requests for visible reasoning, token budgets, standing delegation, stale cautions), then
adding guidance one change at a time. This optimizer does the first half: it asks
``hlock lint`` which lines of the editable instruction files, skills and subagents the
target model would follow too literally, and tries deleting them one finding at a time,
warnings before info. A deletion stays when the candidate's success on train and on dev
holds within the noise band of the lock's own text, and the next deletion is tried on top
of it. Every step is compared with the lock's text, not the previous step, so small dips
can't add up.

What it returns is never larger than the lock's text, so ``tune`` judges it as
``[tune] accept = "no-worse"`` (the default for this optimizer): on the held-out tasks it
must not fall below the lock's success by more than the noise band, nor lose significantly.

Options (``[optimizers.ablate] options``): ``model``, the model to lint for (default: the
lock's task model); ``severities``, the lint severities to try, in order (default
``["warning", "info"]``); ``rules``, only these rule ids (default: every rule that applies).
"""

from __future__ import annotations

import json
import shutil
import sys
from pathlib import Path

from hlock.optimizers import Optimizer, OptimizerError

SEVERITIES = ("warning", "info")
# the components whose text hlock lint reads, and the file in each
LINTED = ("instructions", "skill", "subagent")


def _text_file(comp: dict) -> str:
    """The file in a bundle that holds a component's text: an instruction or subagent
    file, or a skill's SKILL.md."""
    return f"{comp['id']}/SKILL.md" if comp["kind"] == "skill" else comp["id"]


def findings(bundle: Path, comps: list[dict], model: str | None, severities: tuple[str, ...] = SEVERITIES,
             rules: set[str] | None = None) -> list[dict]:
    """The lint findings in ``comps`` of ``bundle`` for ``model``, in the order ablate tries
    them: by severity in ``severities``, then component, then position. Each also carries
    the bundle file it's in (``rel``) and the component's ``component`` id."""
    from hlock import lint_text

    out = []
    for order, comp in enumerate(comps):
        rel = _text_file(comp)
        path = bundle / rel
        if not path.is_file():
            continue
        for f in json.loads(lint_text(path.read_text(), file=Path(rel).name, model=model)):
            if f["severity"] in severities and (rules is None or f["rule"] in rules):
                out.append({**f, "component": comp["id"], "rel": rel, "_order": order})
    out.sort(key=lambda f: (severities.index(f["severity"]), f["_order"], f["line_start"]))
    return out


def delete(text: str, finding: dict) -> tuple[str, str]:
    """``text`` without the whole lines ``finding`` is on, and what was deleted. Offsets
    are bytes."""
    b = text.encode()
    return (b[: finding["line_start"]] + b[finding["line_end"]:]).decode(), b[finding["line_start"]: finding["line_end"]].decode()


class AblateOptimizer(Optimizer):
    """Deletes what ``hlock lint`` flags, one finding at a time, keeping each deletion that
    holds within the noise band on train and dev."""

    name = "ablate"
    about = "deletion first: removes lines hlock lint flags for the model, keeping each that doesn't hurt"

    def optimize(self, request: dict) -> dict:
        from hlock import __version__
        from hlock.optimize import BudgetExhausted, Session

        opts = (request.get("optimizer") or {}).get("options") or {}
        opt = request.get("optimize") or {}
        session = Session(opt["session"])
        seed = Path(opt["seed"])
        spec = json.loads((seed / "bundle.json").read_text())
        comps = [c for c in spec["components"] if c.get("editable") and c["kind"] in LINTED]
        model = opts.get("model") or (opt.get("models") or {}).get("task")
        severities = tuple(opts.get("severities") or SEVERITIES)
        if bad := [s for s in severities if s not in SEVERITIES]:
            raise OptimizerError(f"[optimizers.ablate] options.severities: {bad[0]!r} isn't a lint severity (warning or info)")
        rules = set(opts["rules"]) if opts.get("rules") else None
        noise = float((opt.get("noise") or {}).get("dev_success_sd") or 0.0)
        trials = int(opt.get("trials") or 1)
        splits = opt.get("tasks") or {}
        ws = Path(request.get("workdir") or ".").resolve()
        ws.mkdir(parents=True, exist_ok=True)
        incumbent = ws / "c0"
        shutil.rmtree(incumbent, ignore_errors=True)
        shutil.copytree(seed, incumbent)

        def rate(split: str, cand: Path) -> float:
            """The candidate's success rate on ``split``, all of it or not at all."""
            out = session.evaluate(cand, split, trials=trials)
            rs = out["results"]
            if len(rs) < len(splits.get(split) or []) * trials:
                raise BudgetExhausted(out["budget"].get("exhausted") or "budget exhausted")
            return sum(1.0 for r in rs if r["success"]) / max(len(rs), 1)

        kept: list[dict] = []
        tried: set[tuple[str, str]] = set()
        steps = 0
        try:
            base = {s: rate(s, seed) for s in ("train", "dev")}
            while True:
                todo = []
                for f in findings(incumbent, comps, model, severities, rules):
                    text = (incumbent / f["rel"]).read_text()
                    after, gone = delete(text, f)
                    if (f["rel"], gone) not in tried:
                        todo.append((f, after, gone))
                if not todo:
                    break
                f, after, gone = todo[0]
                tried.add((f["rel"], gone))
                steps += 1
                cand = ws / f"c{steps}"
                shutil.rmtree(cand, ignore_errors=True)
                shutil.copytree(incumbent, cand)
                (cand / f["rel"]).write_text(after)
                try:
                    got = {}
                    for s in ("train", "dev"):
                        got[s] = rate(s, cand)
                        if got[s] < base[s] - noise:
                            break
                except ValueError as exc:  # a candidate the bundle doesn't allow: try the next
                    sys.stderr.write(f"[ablate] {f['rel']}:{f['line']} can't be deleted: {exc}\n")
                    continue
                held = len(got) == 2 and all(got[s] >= base[s] - noise for s in got)
                sys.stderr.write(f"[ablate] {'deleted' if held else 'kept'} {f['rel']}:{f['line']} ({f['rule']}): "
                                 + ", ".join(f"{s} {got[s]:.2f} vs {base[s]:.2f}" for s in got) + "\n")
                if held:
                    kept.append({"component": f["component"], "rule": f["rule"], "severity": f["severity"],
                                 "line": f["line"], "text": gone.rstrip("\n"), "bytes": len(gone.encode()),
                                 "train": got["train"], "dev": got["dev"]})
                    incumbent = cand
        except BudgetExhausted as exc:
            sys.stderr.write(f"[ablate] {exc}\n")

        ledger_path = ws / "ledger.jsonl"
        by: dict[str, list[dict]] = {}
        for k in kept:
            by.setdefault(k["component"], []).append(k)
        ledger_path.write_text("".join(json.dumps({
            "component": cid, "op": "modify",
            "rationale": (f"deleted {len(ks)} line{'s' if len(ks) != 1 else ''} hlock lint flagged for {model or 'every model'} "
                          f"({', '.join(dict.fromkeys(k['rule'] for k in ks))}); each held within the noise band on train and dev"),
            "native": {"deleted": ks},
        }) + "\n" for cid, ks in by.items()))
        return {
            "success": True,
            "score": 1.0,
            "steps": steps,
            "candidate": str(incumbent),
            "ledger": str(ledger_path),
            "optimizer": {"name": "ablate", "version": __version__, "proposer": "hlock lint"},
            "usage": {"tried": steps, "deleted": len(kept), "bytes_deleted": sum(k["bytes"] for k in kept)},
        }
