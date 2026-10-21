"""Write a tune optimizer in Python (specs/tune-optimizers.md).

An optimizer is a backend that runs ``harness.optimize``. It gets a bundle (the lock's
instruction files and skills, and ``bundle.json``, which says what it may edit) and a tune
session. :class:`Session` runs a candidate's trials on the train or dev split, graded by
hlock, inside the session's budget::

    from hlock.optimize import BudgetExhausted, Session

    session = Session(request["optimize"]["session"])
    out = session.evaluate("candidates/7", split="train", trials=3)
    for r in out["results"]:
        r["task"], r["success"], r["feedback"], r["workspace"]

It's the same call as ``hlock tune eval --session DIR CANDIDATE``.
"""

from __future__ import annotations

import json
from pathlib import Path

from ._hlock import tune_eval as _tune_eval

__all__ = ["BudgetExhausted", "Session"]


class BudgetExhausted(Exception):
    """The session's budget is spent: no more trials will run."""


class Session:
    """A tune session, at the directory ``harness.optimize``'s request names."""

    def __init__(self, path: str | Path):
        self.path = str(path)

    def evaluate(self, candidate: str | Path, split: str = "train", tasks: list[str] | None = None,
                 trials: int | None = None) -> dict:
        """Run ``candidate`` (a bundle directory) on ``tasks`` of ``split``, ``trials`` times
        each. Returns the graded results and what's left of the budget. Raises
        :class:`BudgetExhausted` when the budget is spent before any trial could run, and
        ``ValueError`` for a candidate the bundle doesn't allow or a split it can't use."""
        try:
            return json.loads(_tune_eval(self.path, str(candidate), split, tasks, trials))
        except ValueError as e:
            if "budget exhausted" in str(e):
                raise BudgetExhausted(str(e)) from e
            raise
