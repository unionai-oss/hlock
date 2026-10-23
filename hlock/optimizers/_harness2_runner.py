"""Run Harness² on hlock's tasks, as a black box. Runs in Harness²'s own interpreter.

This file imports only Harness² and the standard library: it runs in the Python 3.12+
environment Harness² is installed in (``harness2-setup`` done), not in hlock's. It gives
Harness² what its benchmark seam asks for (``harness2.core.spec.BenchmarkAdapter``) about
hlock's tasks, and leaves everything else to Harness²: its proposer and selector, its
library, and its own OpenCode or Codex executor and their placement of the harness
(``harness2.benchmarks.jobbench.render_opencode_tree``, ``CodexSubstrate.materialise``).
Nothing in Harness² is changed. It never grades: Harness² refines without labels, and hlock
measures the result.

    python _harness2_runner.py job.json result.json

``job.json`` (written by ``hlock.optimizers.harness2``): ``root``, ``substrate`` (``oc`` or
``cx``), ``tree`` (rel path -> text, the seed harness), ``tasks`` (``id``, ``prompt``,
``fixtures``, ``expected``), ``domain``, ``work``, ``k``, ``mode``, ``timeout``, and for tests
``proposer`` / ``launcher`` (``module:attr``). ``result.json``: each task's adopted tree,
whether it's the base, and the lesson Harness² recorded.
"""

from __future__ import annotations

import importlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# this file sits next to hlock's own harness2.py, which must not shadow Harness²
sys.path = [p for p in sys.path if Path(p or ".").resolve() != Path(__file__).resolve().parent]

from harness2 import config
from harness2.core import tree as HT
from harness2.core.proposer import Proposer
from harness2.core.schedule import mode_token, run_domain
from harness2.core.spec import BenchPrompts, BenchSpec, resolve_channel_model
from harness2.substrate.codex import CodexSubstrate
from harness2.substrate.opencode import OpencodeSubstrate


def _load(ref: str):
    module, _, attr = ref.partition(":")
    return getattr(importlib.import_module(module), attr)


class HlockAdapter:
    """hlock's tasks behind Harness²'s benchmark seam."""

    def __init__(self, job: dict):
        self.job = job
        self.root = Path(job["root"])
        self.tasks_by_id = {t["id"]: t for t in job["tasks"]}
        launcher = _load(job["launcher"])(self) if job.get("launcher") else self._launch
        cx = job["substrate"] == "cx"
        self.substrate = CodexSubstrate(launcher=launcher) if cx else OpencodeSubstrate(launcher=launcher)
        if job.get("channel"):
            ch = resolve_channel_model(job["channel"]["kind"], job["channel"]["model"], effort="high")
        else:
            ch = resolve_channel_model(None, None, effort="high")
        self.spec = BenchSpec(
            name="hlock",
            runs_root=self.root / "runs",
            install_mode="codex_root" if cx else "workspace_agents_md",
            prompts=BenchPrompts(work=job["work"]),
            channels={"proposer": ch, "selector": ch},
            allow_subagents=not cx,
        )
        HT.write_tree(self.substrate.surface(), job["tree"], self.base_harness())

    # ---- what the seam asks for ----
    def tasks(self, split: str = "all") -> list[str]:
        return [t["id"] for t in self.job["tasks"]]

    def domain_of(self, task_id: str) -> str:
        return self.job["domain"]

    def task_prompt(self, task_id: str) -> str:
        return self.tasks_by_id[task_id]["prompt"]

    def task_files(self, task_id: str) -> Path | None:
        f = self.tasks_by_id[task_id].get("fixtures")
        return Path(f) if f else None

    def base_harness(self) -> Path:
        return self.root / "base"

    def validate_tree(self, tree) -> None:
        return None

    def run_dir(self, run_id: str) -> Path:
        return self.root / "executions" / run_id

    def execute(self, task_id: str, harness_dir: Path | None, run_id: str, *, timeout: int | None = None) -> Path:
        run_dir = self.run_dir(run_id)
        if (run_dir / "done").is_file():
            return run_dir
        install = self.substrate.materialise(Path(harness_dir or self.base_harness()), self.spec, {})
        try:
            dirs = self.substrate.execute(task_id, install, [run_id], int(timeout or self.job["timeout"]))
        finally:
            shutil.rmtree(install.staging_dir, ignore_errors=True)
        (dirs[0] / "done").write_text("")
        return dirs[0]

    def _launch(self, req, install) -> list[Path]:
        """One rollout under Harness²'s executor: the task's files in a fresh workspace, the
        harness placed the way Harness² places it, then its OpenCode or Codex command."""
        task = self.tasks_by_id[req.task_ref]
        run_dir = self.run_dir(req.run_ids[0])
        ws = run_dir / "workspace"
        shutil.rmtree(run_dir, ignore_errors=True)
        ws.mkdir(parents=True)
        if task.get("fixtures") and Path(task["fixtures"]).is_dir():
            shutil.copytree(task["fixtures"], ws, dirs_exist_ok=True)
        if self.substrate.name == "cx":
            shutil.copytree(install.staging_dir, ws, dirs_exist_ok=True)
            argv = self.substrate.exec_argv(task["prompt"], ws, config.SOLVER_MODEL_CX)
            env = self.substrate.exec_env(os.environ, codex_home=config.CODEX_HOME_DIR)
        else:
            from harness2.benchmarks.jobbench import render_opencode_tree

            render = Path(tempfile.mkdtemp(prefix="hlock-h2-render-"))
            try:
                render_opencode_tree(install.staging_dir, render, {})
                for entry in (".opencode", "AGENTS.md", "scripts"):
                    src = render / entry
                    if src.is_dir():
                        shutil.copytree(src, ws / entry, dirs_exist_ok=True)
                    elif src.is_file():
                        shutil.copy(src, ws / entry)
            finally:
                shutil.rmtree(render, ignore_errors=True)
            argv = self.substrate.run_argv(task["prompt"], ws, config.SOLVER_MODEL_OC)
            env = self.substrate.run_env(os.environ, ws)
        with open(run_dir / "stream.jsonl", "w") as out:
            try:
                subprocess.run(argv, env=env, cwd=ws, stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                               timeout=req.timeout, check=False)
            except subprocess.TimeoutExpired:
                pass
        return [run_dir]

    def _outputs(self, run_dir: Path) -> list[Path]:
        ws = Path(run_dir) / "workspace"
        expected = self.tasks_by_id.get(self._task_of(run_dir), {}).get("expected") or []
        return [ws / e for e in expected]

    def _task_of(self, run_dir: Path) -> str:
        # executions/<ns>/<task>/<label>
        return Path(run_dir).parent.name

    def delivered(self, run_dir: Path) -> bool:
        outs = self._outputs(run_dir)
        return all(p.is_file() for p in outs) if outs else (Path(run_dir) / "workspace").is_dir()

    def trajectory(self, run_dir: Path, *, obs_trunc: int) -> str:
        stream = Path(run_dir) / "stream.jsonl"
        if (Path(run_dir) / "trajectory.md").is_file():
            return (Path(run_dir) / "trajectory.md").read_text(errors="replace")
        if not stream.is_file():
            return "(no trajectory)"
        return self.substrate.read_trajectory(stream, obs_trunc=obs_trunc, max_chars=2_000_000)

    def deliverable_files(self, run_dir: Path) -> dict[str, str]:
        return {p.name: p.read_text(errors="replace") for p in self._outputs(run_dir) if p.is_file()}

    def deliverable_text(self, run_dir: Path) -> str:
        return "\n\n".join(self.deliverable_files(run_dir).values())

    def score(self, run_dir: Path, task_id: str):
        return None


def main(argv: list[str]) -> int:
    job = json.loads(Path(argv[0]).read_text())
    adapter = HlockAdapter(job)
    root = adapter.root
    k = int(job["k"])
    mode = mode_token(job["mode"])
    channel = adapter.spec.channels["proposer"]
    prop = _load(job["proposer"])(channel) if job.get("proposer") else Proposer(channel, audit_dir=root / "audit")
    result = run_domain(
        job["domain"],
        adapter.tasks(),
        k,
        adapter=adapter,
        prop=prop,
        runs_root=adapter.spec.runs_root,
        library_root=root / "library" / f"k{k}",
        modes=(mode,),
    )
    surface = adapter.substrate.surface()
    adopted = []
    for summary in sorted(adapter.spec.runs_root.glob("*/*/summary.json")):
        s = json.loads(summary.read_text())
        sel = (s.get("selections") or {}).get(mode) or {}
        label = sel.get("chosen_label")
        if not label:
            continue
        tree_dir = summary.parent / "harnesses" / label
        adopted.append({
            "task": s.get("task_id"),
            "label": label,
            "base": bool(sel.get("chosen_is_base") or sel.get("chosen_tree_is_base")),
            "tree": HT.read_tree(surface, tree_dir) if tree_dir.is_dir() else None,
            "why": sel.get("why", ""),
            "experience": sel.get("experience") or {},
            "usd": sel.get("usd", 0.0),
        })
    from importlib.metadata import PackageNotFoundError, version

    try:
        v = version("harness2")
    except PackageNotFoundError:
        v = ""
    out = {"done": result.get("done", 0), "failed": result.get("failed", 0), "adopted": adopted, "version": v}
    Path(argv[1]).write_text(json.dumps(out, indent=2, default=str))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
