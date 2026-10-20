"""Content-addressed blob store.

Large content (prompts, tool schemas, traces, report blobs) is referenced by
``sha256:<digest>`` in the lockfile rather than inlined. Locally these blobs live
under ``.hlock/store/`` addressed by digest. On devbox/cluster the same
interface is backed by an object store via fsspec-style paths ``store://...``.
"""

from __future__ import annotations

import shutil
from pathlib import Path
from typing import Any

from hlock.errors import AgentLockError, canon_json, sha256_bytes


class StoreError(AgentLockError):
    pass


class Store:
    """A write-once, read-by-hash blob store rooted at a local directory.

    ``put`` returns a ``sha256:<digest>`` reference; ``get`` resolves it. The store
    is write-once per digest so repeated writes of identical content are cheap and
    immutable.
    """

    def __init__(self, root: Path):
        self.root = root

    # --- lifecycle ---------------------------------------------------------
    def __repr__(self) -> str:  # pragma: no cover - cosmetic
        return f"Store({self.root})"

    # --- put ---------------------------------------------------------------
    def put_bytes(self, data: bytes) -> str:
        digest = sha256_bytes(data)
        self.root.mkdir(parents=True, exist_ok=True)
        target = self.root / digest.split(":", 1)[1]
        if not target.exists():
            target.write_bytes(data)
        return digest

    def put_text(self, text: str) -> str:
        return self.put_bytes(text.encode("utf-8"))

    def put_obj(self, obj: Any) -> str:
        """Store a JSON-serializable object; return its canonical JSON digest."""
        return self.put_text(canon_json(obj))

    def put_from(self, path: Path) -> str:
        return self.put_bytes(path.read_bytes())

    # --- get ---------------------------------------------------------------
    def get_bytes(self, ref: str) -> bytes:
        digest = ref.split(":", 1)[1] if ":" in ref else ref
        p = self.root / digest
        if not p.exists():
            raise StoreError(f"blob not in store: {digest}")
        return p.read_bytes()

    def get_text(self, ref: str) -> str:
        return self.get_bytes(ref).decode("utf-8")

    def get_path(self, ref: str) -> Path:
        digest = ref.split(":", 1)[1] if ":" in ref else ref
        p = self.root / digest
        if not p.exists():
            raise StoreError(f"blob not in store: {digest}")
        return p

    def exists(self, ref: str) -> bool:
        digest = ref.split(":", 1)[1] if ":" in ref else ref
        return (self.root / digest).exists()

    # --- misc --------------------------------------------------------------
    def wipe(self) -> None:
        if self.root.exists():
            shutil.rmtree(self.root)

    @property
    def count(self) -> int:
        if not self.root.exists():
            return 0
        return sum(1 for p in self.root.iterdir() if p.is_file())
