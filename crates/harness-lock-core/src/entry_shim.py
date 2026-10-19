"""Run a Python agent entrypoint under hlock's code-harness contract.

hlock runs this file as ``python -c <this source> module:callable`` (so the agent's
interpreter needs nothing from hlock installed). It builds a ``RunContext`` from
the ``HLOCK_*`` environment variables, calls ``callable(prompt, ctx)``, and writes
the result to ``HLOCK_OUTPUT``:

- a ``dict`` is written as-is (``success`` defaults to ``True``);
- a ``bool`` becomes ``{"success": <bool>}``;
- anything else becomes ``{"success": True, "output": str(result)}``.

An exception writes ``{"success": False, "error": ...}`` and exits 1. The
callable may also be ``async``.
"""

from __future__ import annotations

import asyncio
import importlib
import inspect
import json
import os
import sys
import traceback
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class RunContext:
    """What hlock gives a code harness for one run."""

    model: str = ""
    """The locked model as ``provider:model``."""
    base_url: str | None = None
    """The model's base URL, when the lock or ``HL_LLM_BASE_URL`` sets one."""
    workspace: Path = field(default_factory=Path.cwd)
    """Working directory for artifacts."""
    mcp_config: Path | None = None
    """A ``{"mcpServers": …}`` file for the locked MCP servers."""
    skills_dir: Path | None = None
    """A directory holding the locked skills."""
    lock: Path | None = None
    """The lockfile this run came from."""
    seed: int = 0
    harness: str = ""

    def decide(self, use: str, **payload) -> dict:
        """Ask the locked decider for ``use`` (``approve``, ``stop``, ``route``,
        ``tool``), through ``hlock decide hook``. Returns its answer, such as
        ``{"answer": "planner", "reason": …}``, or ``{}`` when there's no decider or it
        isn't sure: then do what you would have done.

        ``route``: ``options`` (model roles) and ``state``. ``tool``: ``tools`` (names)
        and ``state``; the answer is the ones to offer. ``approve``: ``tool`` and
        ``input``. ``stop``: ``request`` and ``last_message``.
        """
        lock = os.environ.get("HLOCK_DECIDE_LOCK")
        if not lock:
            return {}
        import subprocess

        event = {"approve": "tool", "tool": "tools"}.get(use, use)
        try:
            r = subprocess.run(
                [os.environ.get("HLOCK_BIN", "hlock"), "decide", "hook", use, "--format", "plugin",
                 "--lock", lock, "--input", json.dumps({"event": event, **payload})],
                capture_output=True, text=True, timeout=30,
            )
            return json.loads(r.stdout) if r.stdout.strip() else {}
        except Exception:
            return {}

    @property
    def provider(self) -> str:
        return self.model.split(":", 1)[0] if ":" in self.model else ""

    @property
    def model_name(self) -> str:
        return self.model.split(":", 1)[1] if ":" in self.model else self.model

    @classmethod
    def from_env(cls) -> RunContext:
        def path(name: str) -> Path | None:
            value = os.environ.get(name)
            return Path(value) if value else None

        return cls(
            model=os.environ.get("HLOCK_MODEL", ""),
            base_url=os.environ.get("HLOCK_LLM_BASE_URL") or None,
            workspace=path("HLOCK_WORKSPACE") or Path.cwd(),
            mcp_config=path("HLOCK_MCP_CONFIG"),
            skills_dir=path("HLOCK_SKILLS_DIR"),
            lock=path("HLOCK_LOCK"),
            seed=int(os.environ.get("HLOCK_SEED", "0") or 0),
            harness=os.environ.get("HLOCK_HARNESS", ""),
        )


def _load(spec: str):
    module_name, _, attr = spec.partition(":")
    if not attr:
        raise SystemExit(f"entrypoint must look like module:callable, got {spec!r}")
    obj = importlib.import_module(module_name)
    for part in attr.split("."):
        obj = getattr(obj, part)
    return obj


def _result(value) -> dict:
    if isinstance(value, dict):
        return {"success": True, **value}
    if isinstance(value, bool):
        return {"success": value}
    return {"success": True, "output": "" if value is None else str(value)}


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1:
        print("usage: python -c <shim> module:callable", file=sys.stderr)
        return 2
    fn = _load(args[0])
    ctx = RunContext.from_env()
    prompt = os.environ.get("HLOCK_PROMPT", "")
    out = os.environ.get("HLOCK_OUTPUT")
    try:
        value = fn(prompt, ctx)
        if inspect.isawaitable(value):
            value = asyncio.run(value)
        result, code = _result(value), 0
    except Exception as e:  # report, don't crash without a result
        traceback.print_exc()
        result, code = {"success": False, "error": f"{type(e).__name__}: {e}"}, 1
    if out:
        Path(out).write_text(json.dumps(result, default=str))
    if "output" in result:
        print(result["output"])
    return code if result.get("success", True) else max(code, 1)


if __name__ == "__main__":
    sys.exit(main())
