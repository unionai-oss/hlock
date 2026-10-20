"""Tuning backends.

Maps the tuning execution loop onto a target:

- ``local`` — native async runner (bounded concurrency, local run cache). Flyte is
  imported lazily and only for devbox/cluster, so Layer 0-2 user code never needs it.
- ``devbox`` / ``cluster`` — a Flyte 2 pipeline (see ``hlock.flyte``) run through
  ``flyte.run``; reads ``~/.flyte/config.yaml`` for cluster targets.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from hlock.search import Stack, TrialResult
from hlock.tasks import Task


@dataclass
class Candidate:
    stack: Stack
    task: Task
    seed: int


class Backend(Protocol):
    target: str

    async def run_sweep(self, candidates: list[Candidate], workspace_root: Path,
                        run_trial: Callable[[Candidate, Path], TrialResult]) -> list[TrialResult]: ...


# --------------------------------------------------------------------------- #
# Local backend
# --------------------------------------------------------------------------- #
class LocalBackend:
    """Runs trials in-process with bounded concurrency and a local run cache."""

    target = "local"

    def __init__(self, concurrency: int = 8, run_dir: Path | None = None, cache_db: Any = None) -> None:
        self.concurrency = concurrency
        self.run_dir = run_dir or Path(".hlock/runs")
        self.run_dir.mkdir(parents=True, exist_ok=True)
        self.cache_db = cache_db

    def describe(self) -> str:
        return f"LocalBackend(concurrency={self.concurrency}, runs={self.run_dir})"

    def _cache_key(self, c: Candidate) -> str:
        return f"{c.stack.hash()}::{c.task.id}::{c.seed}"

    async def run_one(self, c: Candidate, run_trial: Callable[[Candidate, Path], TrialResult]) -> TrialResult:
        if self.cache_db is not None:
            hit = self.cache_db.get(self._cache_key(c))
            if hit is not None:
                return hit
        loop = asyncio.get_event_loop()
        result = await loop.run_in_executor(None, run_trial, c, self.run_dir)
        if self.cache_db is not None:
            self.cache_db.put(self._cache_key(c), result)
        return result

    async def run_sweep(self, candidates: list[Candidate], workspace_root: Path,
                        run_trial: Callable[[Candidate, Path], TrialResult]) -> list[TrialResult]:
        sem = asyncio.Semaphore(self.concurrency)

        async def worker(c: Candidate) -> TrialResult:
            async with sem:
                return await self.run_one(c, run_trial)

        return list(await asyncio.gather(*(worker(c) for c in candidates)))


# --------------------------------------------------------------------------- #
# Flyte backend (devbox / cluster)
# --------------------------------------------------------------------------- #
class FlyteBackend:
    """Runs the tuning pipeline as a Flyte 2 program.

    ``local`` maps onto flyte passthrough; ``devbox`` and ``cluster`` read the
    Flyte config (``~/.flyte/config.yaml`` by default) and call ``flyte.run`` on the
    pipeline defined in ``hlock.flyte``.
    """

    def __init__(self, target: str = "cluster", config: str | Path | None = None) -> None:
        self.target = target
        self.config = Path(config) if config else _default_flyte_config()

    def available(self) -> bool:
        try:
            import flyte  # noqa: F401
        except ImportError:
            return False
        return True

    def describe(self) -> str:
        if not self.available():
            return f"FlyteBackend({self.target}) — flyte SDK not importable"
        return f"FlyteBackend({self.target}, config={self.config})"

    def init_kwargs(self) -> dict[str, Any]:
        if self.target == "local":
            return {}
        cfg = _parse_flyte_config(self.config)
        kwargs = {
            "endpoint": cfg.get("admin", {}).get("endpoint", "").replace("dns:///", ""),
            "org": cfg.get("task", {}).get("org"),
            "project": cfg.get("task", {}).get("project"),
            "domain": cfg.get("task", {}).get("domain"),
        }
        return {k: v for k, v in kwargs.items() if v}


def _default_flyte_config() -> Path:
    return Path.home() / ".flyte" / "config.yaml"


def _parse_flyte_config(path: Path) -> dict[str, Any]:
    if not path or not path.exists():
        return {}
    import yaml

    with path.open("r") as f:
        return yaml.safe_load(f) or {}


def make_backend(target: str, config: str | Path | None = None, **kw: Any) -> Backend:
    if target == "local":
        return LocalBackend(**kw)
    if target in ("devbox", "cluster"):
        return FlyteBackend(target=target, config=config)
    raise ValueError(f"unknown backend target: {target!r}")
