"""harness_lock: the long-form alias of ``hlock``.

``import harness_lock`` is the same API as ``import hlock``. The implementation lives
in the ``hlock`` package (and its compiled ``hlock._hlock`` module); this package
re-exports it under the ``harness-lock`` alias name.
"""

from hlock import *
from hlock import __all__, __version__  # noqa: F401
