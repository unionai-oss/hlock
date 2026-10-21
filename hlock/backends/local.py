"""The ``local`` backend: hlock's stock handlers, in process on this machine."""

from __future__ import annotations

from hlock.backends import Backend, run_local


class LocalBackend(Backend):
    name = "local"
    about = "hlock's stock handlers, in process on this machine"

    def run(self, request: dict) -> dict:
        return run_local(request)
