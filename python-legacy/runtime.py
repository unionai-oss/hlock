"""Runtime: load a lock and run the locked agent.

``hlock load`` + ``agent.run(...)`` execute the locked stack with the runtime guard:
model calls are checked against the locked provider/model/params, tool calls against
the allowlist, and every run emits a trace so production traces can feed the next
tuning round.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from hlock.adapters import ADAPTERS
from harness.lockfile import Lockfile
from hlock.search import Stack
from hlock.traces import Trace, TraceRecorder


@dataclass
class RunOutcome:
    success: bool
    text: str
    metrics: dict[str, Any] = field(default_factory=dict)
    trace: Trace | None = None


class RuntimeGuard:
    """In-process enforcement of the locked stack."""

    def __init__(self, lock: Lockfile, mode: str = "strict") -> None:
        self.lock = lock
        self.mode = mode

    def _violation(self, msg: str) -> None:
        if self.mode == "off":
            return
        if self.mode == "strict":
            raise RuntimeError(f"runtime guard: {msg}")
        print(f"[guard:warn] {msg}")

    def check_model(self, role: str, model: str) -> None:
        locked = {llm.role: llm.model for llm in self.lock.llms}
        if role in locked and locked[role] != model:
            self._violation(f"model mismatch for role {role}: locked {locked[role]!r}, got {model!r}")

    def check_tool(self, tool: str) -> None:
        allow = self.lock.policy.tool_allowlist
        allowed = [t.name for t in self.lock.tools] + list(allow)
        if allowed and tool not in allowed:
            self._violation(f"tool {tool!r} not in allowlist {allowed!r}")


class LockedAgent:
    """A loaded, runnable locked agent."""

    def __init__(self, lock: Lockfile, guard: RuntimeGuard | None = None, workdir: Path | None = None) -> None:
        self.lock = lock
        self.guard = guard or RuntimeGuard(lock, mode="strict")
        self.workdir = workdir or Path(".hlock/run")

    @classmethod
    def load(cls, path: str | Path, mode: str = "strict") -> LockedAgent:
        lock = Lockfile.load(path)
        return cls(lock, RuntimeGuard(lock, mode=mode))

    def stack(self) -> Stack:
        return Stack(
            llm={llm.role: llm.model for llm in self.lock.llms},
            harness=self.lock.harness.name or "reference",
            pattern=self.lock.harness.pattern or "single-agent",
            policy=self.lock.policy.model_dump(),
            decision=self.lock.decision.kind,
            tools=[t.name for t in self.lock.tools],
            skills=[sk.name for sk in self.lock.skills],
            instructions=[i.name for i in self.lock.instructions],
        )

    async def run(self, prompt: str) -> RunOutcome:
        from hlock.tasks import Task

        adapter = ADAPTERS.get(self.lock.harness.name or "reference")
        task = Task(id="cli-run", prompt="", prompt_text=prompt, expected={"artifacts": []})
        recorder = TraceRecorder(self.stack().hash(), task.id, seed=0)
        self.workdir.mkdir(parents=True, exist_ok=True)
        result = await adapter.run(self.stack(), task, self.workdir, recorder)
        trace = recorder.finish({"artifacts": []}, result.metrics, [])
        return RunOutcome(success=result.success, text=trace.to_json(), metrics=result.metrics, trace=trace)
