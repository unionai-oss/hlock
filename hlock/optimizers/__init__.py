"""Optimizers: what searches for harness edits in ``tune`` (see ``specs/optimizers.md``).

An optimizer and a backend are separate choices. The optimizer (``[tune] optimizer``)
proposes edits; the backend (``[backend] target``) is where the work runs. A backend that
hosts the search gets a ``harness.optimize`` work item naming the optimizer, and runs it with
:func:`run`. The optimizer evaluates candidates through the tune session
(:class:`hlock.optimize.Session`), whose trials run on the backend.

A Python optimizer subclasses :class:`Optimizer` and is registered under the
``hlock.optimizers`` entry point group, or named in the manifest
(``[optimizers.<name>] python = "module:attr"``)::

    from hlock.optimize import Session
    from hlock.optimizers import Optimizer

    class Gepa(Optimizer):
        name = "gepa"
        about = "GEPA reflective Pareto search"

        def optimize(self, request: dict) -> dict:
            opts = request["optimizer"]["options"]
            session = Session(request["optimize"]["session"])
            ...
            return {"candidate": str(best), "ledger": str(ledger), "optimizer": {"name": "gepa"}}

An optimizer in another language is a command (``[optimizers.<name>] command``, or
``hlock-optimizer-<name>`` on ``PATH``) that reads the request and writes the response
files, in the backend contract's envelope.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from importlib.metadata import entry_points
from pathlib import Path

from hlock.backends import BackendError, _import

__all__ = ["BUILTIN", "GROUP", "Optimizer", "OptimizerError", "available", "load", "run"]

#: The entry point group Python optimizers register under.
GROUP = "hlock.optimizers"
#: The built-in optimizers.
BUILTIN = {"hlock": "hlock.optimizers.hlock:HlockOptimizer", "harness2": "hlock.optimizers.harness2:Harness2Optimizer"}


class OptimizerError(BackendError):
    """The search failed; the message is what the user sees."""


class Optimizer:
    """An optimizer: set ``name`` and ``about``, and override :meth:`optimize`."""

    #: The optimizer's name (else the name it was selected by).
    name: str = ""
    #: One line for ``hlock optimizer status``: what it is, its version.
    about: str = ""

    def optimize(self, request: dict) -> dict:
        """Search from ``request["optimize"]["seed"]`` and return the best candidate bundle
        and its ledger (raise :class:`OptimizerError` to fail the search)."""
        raise NotImplementedError(f"{type(self).__name__} doesn't implement optimize()")


def _entry_points() -> dict[str, str]:
    try:
        eps = entry_points(group=GROUP)
    except TypeError:  # Python 3.9: entry_points() takes no arguments and returns a dict
        eps = entry_points().get(GROUP, ())
    return {ep.name: ep.value for ep in eps}


def available() -> dict[str, str]:
    """Every Python optimizer this interpreter can load: name -> ``module:attr``."""
    return {**_entry_points(), **BUILTIN}


def load(name: str, object: str | None = None) -> Optimizer:
    """The optimizer ``object`` (``module:attr``) names, else the built-in or entry point
    called ``name``. A class is instantiated with no arguments."""
    spec = object or BUILTIN.get(name) or _entry_points().get(name)
    if spec is None:
        raise OptimizerError(
            f"no optimizer named {name!r}: not built in, not in [optimizers], no hlock-optimizer-{name} on PATH, "
            f"and no {GROUP!r} entry point (see Optimizers)"
        )
    try:
        obj = _import(spec)
    except Exception as exc:
        raise OptimizerError(f"can't load optimizer {name!r} from {spec}: {exc}") from exc
    optimizer = obj() if isinstance(obj, type) else obj
    if not callable(getattr(optimizer, "optimize", None)):
        raise OptimizerError(f"{spec} isn't an optimizer: it has no optimize(request)")
    return optimizer


def _run_command(argv: list[str], request: dict) -> dict:
    """Run a command optimizer on ``request`` and return its result."""
    with tempfile.TemporaryDirectory() as tmp:
        req, resp = Path(tmp) / "request.json", Path(tmp) / "response.json"
        req.write_text(json.dumps(request))
        try:
            proc = subprocess.run([*argv, str(req), str(resp)], stdin=subprocess.DEVNULL, stdout=sys.stderr,
                                  stderr=sys.stderr, check=False)
        except OSError as exc:
            raise OptimizerError(f"can't run optimizer {argv[0]}: {exc}") from exc
        try:
            out = json.loads(resp.read_text())
        except (OSError, ValueError):
            raise OptimizerError(f"optimizer {argv[0]} wrote no response (exit {proc.returncode})") from None
    if not out.get("ok"):
        raise OptimizerError(out.get("error") or f"optimizer {argv[0]} failed (exit {proc.returncode})")
    return out.get("result") or {}


def run(request: dict) -> dict:
    """Run the optimizer a ``harness.optimize`` request names, and return its result."""
    spec = request.get("optimizer") or {}
    name = spec.get("name") or "hlock"
    if spec.get("command"):
        return _run_command(list(spec["command"]), request)
    result = load(name, spec.get("object")).optimize(request)
    if not isinstance(result, dict):
        raise OptimizerError(f"optimizer {name!r} returned no result")
    return result
