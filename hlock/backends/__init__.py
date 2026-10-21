"""Backends: where hlock's work runs (see ``specs/backends.md``).

The Rust core sends a backend one *work item* at a time (a trial, a tune stage) and
gets one result back. hlock's worker (``python -m hlock.tasks.worker``) is the process
for Python backends: it finds the backend the request names and calls it.

A Python backend subclasses :class:`Backend` and is registered under the
``hlock.backends`` entry point group, or named in the manifest
(``[backends.<name>] python = "module:attr"``)::

    from hlock.backends import Backend, run_local

    class RayBackend(Backend):
        name = "ray"
        kinds = ("llm.run_trial", "data.load")

        def run(self, request: dict) -> dict:
            import ray
            return ray.get(ray.remote(run_local).remote(request))
"""

from __future__ import annotations

import importlib
from importlib.metadata import entry_points

__all__ = ["BUILTIN", "DESCRIBE", "GROUP", "KINDS", "Backend", "BackendError", "available", "load", "run_local"]

#: The work items hlock's stock handlers run.
KINDS = ("llm.run_trial", "data.load", "data.synthetic", "grade", "prompt.search", "ft.lora")
#: The work item that asks a backend what it runs.
DESCRIBE = "backend.describe"
#: The entry point group Python backends register under.
GROUP = "hlock.backends"
#: The built-in Python backends (``offline`` runs in the Rust core, not here).
BUILTIN = {"local": "hlock.backends.local:LocalBackend", "flyte": "hlock.backends.flyte:FlyteBackend"}


class BackendError(Exception):
    """A work item failed; the message is what the user sees."""


class Backend:
    """A backend: set ``name`` and ``kinds``, and override :meth:`run`."""

    #: The backend's name (else the name it was selected by).
    name: str = ""
    #: The work items it runs.
    kinds: tuple[str, ...] = KINDS
    #: Its trials call real models at ``HL_LLM_BASE_URL`` (true for hlock's stock
    #: handlers), so the core checks a real candidate has one before any trial runs.
    model_endpoint: bool = True
    #: One line for ``hlock backend status``: its version, where it runs.
    about: str = ""

    def describe(self, request: dict) -> dict:
        """What this backend runs: the answer to ``backend.describe``."""
        return {
            "name": self.name or request.get("backend", ""),
            "kinds": list(self.kinds),
            "model_endpoint": self.model_endpoint,
            "about": self.about,
        }

    def run(self, request: dict) -> dict:
        """Run one work item and return its result (raise :class:`BackendError` to fail it)."""
        raise NotImplementedError(f"{type(self).__name__} doesn't implement run()")


def run_local(request: dict) -> dict:
    """Run a work item with hlock's stock handler, in this process, and return its result."""
    from hlock.tasks.worker import HANDLERS

    kind = request.get("kind", "")
    handler = HANDLERS.get(kind)
    if handler is None:
        raise BackendError(f"unknown task kind: {kind!r}")
    payload = handler(request)
    if not payload:
        raise BackendError(f"{kind} produced no output")
    if "error" in payload:
        raise BackendError(payload["error"])
    return payload["result"]


def _entry_points() -> dict[str, str]:
    return {ep.name: ep.value for ep in entry_points(group=GROUP)}


def available() -> dict[str, str]:
    """Every Python backend this interpreter can load: name -> ``module:attr``."""
    return {**_entry_points(), **BUILTIN}


def _import(spec: str):
    module, _, attr = spec.partition(":")
    obj = importlib.import_module(module)
    for part in filter(None, attr.split(".")):
        obj = getattr(obj, part)
    return obj


def load(name: str, object: str | None = None) -> Backend:
    """The backend ``object`` (``module:attr``) names, else the built-in or entry point
    called ``name``. A class is instantiated with no arguments."""
    spec = object or BUILTIN.get(name) or _entry_points().get(name)
    if spec is None:
        raise BackendError(
            f"no backend named {name!r}: not built in, not in [backends], no hlock-backend-{name} on PATH, "
            f"and no {GROUP!r} entry point (see Backends)"
        )
    try:
        obj = _import(spec)
    except Exception as exc:
        raise BackendError(f"can't load backend {name!r} from {spec}: {exc}") from exc
    backend = obj() if isinstance(obj, type) else obj
    if not callable(getattr(backend, "run", None)):
        raise BackendError(f"{spec} isn't a backend: it has no run(request)")
    return backend
