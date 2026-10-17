"""Read agent traces into sessions, the raw material for drafting an eval pack.

A session is one conversation: the request that opened it, and the files the agent created
along the way. Readers understand the session logs agent CLIs keep on disk (Claude Code,
Codex, Pi), and generic JSONL or CSV records with a prompt column.
"""

from __future__ import annotations

import csv
import json
import os
import re
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from pathlib import Path

# user turns that are harness plumbing, not requests
_NOISE = ("<command-", "<local-command", "<system-reminder", "<environment_context", "<user_instructions",
          "Caveat:", "[Request interrupted")
_PROMPT_KEYS = ("prompt", "input", "question", "query", "instruction", "request", "task", "user_message")
# tools that create a file, and the argument naming it
_WRITE_TOOLS = {"Write": "file_path", "write": "path", "create_file": "path", "write_file": "path"}


@dataclass
class Session:
    harness: str  # claude-code, codex, pi, or "traces" for generic records
    source: Path
    prompt: str
    started: str = ""  # ISO timestamp when known
    cwd: str | None = None
    created: list[str] = field(default_factory=list)  # files the agent wrote, as given


def _jsonl(path: Path) -> Iterator[dict]:
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if line:
                try:
                    e = json.loads(line)
                except ValueError:
                    continue
                if isinstance(e, dict):
                    yield e


def _text(content) -> str:
    """The text of a message's content: a string, or text blocks."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b.get("text", "") for b in content
                         if isinstance(b, dict) and b.get("type") in ("text", "input_text"))
    return ""


def _is_request(text: str) -> bool:
    t = text.strip()
    return len(t) >= 20 and not t.startswith(_NOISE)


def _tool_writes(blocks, name_key: str, args_key: str) -> list[str]:
    out = []
    for b in blocks if isinstance(blocks, list) else []:
        if isinstance(b, dict) and b.get("type") in ("tool_use", "toolCall"):
            arg = _WRITE_TOOLS.get(b.get(name_key, ""))
            path = (b.get(args_key) or {}).get(arg) if arg else None
            if isinstance(path, str) and path not in out:
                out.append(path)
    return out


def read_claude_code(path: Path) -> Session | None:
    s = None
    for e in _jsonl(path):
        if e.get("isSidechain"):
            continue
        msg = e.get("message") if isinstance(e.get("message"), dict) else {}
        if e.get("type") == "user" and s is None and not e.get("isMeta"):
            text = _text(msg.get("content"))
            if _is_request(text):
                s = Session("claude-code", path, text.strip(), e.get("timestamp", ""), e.get("cwd"))
        elif e.get("type") == "assistant" and s is not None:
            s.created += [p for p in _tool_writes(msg.get("content"), "name", "input") if p not in s.created]
    return s


def read_pi(path: Path) -> Session | None:
    s, cwd = None, None
    for e in _jsonl(path):
        if e.get("type") == "session":
            cwd = e.get("cwd")
        msg = e.get("message") if isinstance(e.get("message"), dict) else {}
        if msg.get("role") == "user" and s is None:
            text = _text(msg.get("content"))
            if _is_request(text):
                s = Session("pi", path, text.strip(), e.get("timestamp", ""), cwd)
        elif msg.get("role") == "assistant" and s is not None:
            s.created += [p for p in _tool_writes(msg.get("content"), "name", "arguments") if p not in s.created]
    return s


def read_codex(path: Path) -> Session | None:
    s, cwd = None, None
    for e in _jsonl(path):
        p = e.get("payload") if isinstance(e.get("payload"), dict) else {}
        if e.get("type") == "session_meta":
            cwd = p.get("cwd")
        elif e.get("type") == "response_item" and p.get("type") == "message" and p.get("role") == "user" and s is None:
            text = _text(p.get("content"))
            if _is_request(text):
                s = Session("codex", path, text.strip(), e.get("timestamp", ""), cwd)
    return s


def _records(path: Path) -> Iterator[dict]:
    if path.suffix == ".csv":
        with open(path, encoding="utf-8", errors="replace", newline="") as fh:
            yield from csv.DictReader(fh)
    else:
        yield from _jsonl(path)


def read_records(path: Path) -> list[Session]:
    """Generic records, one session each: a prompt column, and optionally `artifacts`."""
    out = []
    for r in _records(path):
        text = next((r[k] for k in _PROMPT_KEYS if isinstance(r.get(k), str)), None)
        if text is None and isinstance(r.get("messages"), list):
            text = next((_text(m.get("content")) for m in r["messages"]
                         if isinstance(m, dict) and m.get("role") == "user"), None)
        if text and _is_request(text):
            arts = r.get("artifacts")
            arts = [a for a in arts if isinstance(a, str)] if isinstance(arts, list) else []
            out.append(Session("traces", path, text.strip(), str(r.get("timestamp", "")), None, arts))
    return out


def _sniff(path: Path) -> str:
    """Which reader a JSONL file needs, from its first few entries."""
    for i, e in enumerate(_jsonl(path)):
        if e.get("type") == "session_meta" or e.get("type") == "response_item":
            return "codex"
        if e.get("type") == "session" and "cwd" in e:
            return "pi"
        if e.get("type") in ("user", "assistant") and "sessionId" in e:
            return "claude-code"
        if i >= 5:
            break
    return "records"


_READERS = {"claude-code": read_claude_code, "pi": read_pi, "codex": read_codex}


def read(paths: Iterable[Path]) -> list[Session]:
    sessions = []
    for p in paths:
        kind = "records" if p.suffix == ".csv" else _sniff(p)
        if kind == "records":
            sessions += read_records(p)
        elif (s := _READERS[kind](p)) is not None:
            sessions.append(s)
    return sessions


def _files(root: Path) -> list[Path]:
    if root.is_file():
        return [root]
    return sorted(p for p in root.rglob("*") if p.suffix in (".jsonl", ".csv") and p.is_file())


def _within(cwd: str | None, project: Path) -> bool:
    if not cwd:
        return False
    try:
        Path(cwd).resolve().relative_to(project)
        return True
    except ValueError:
        return False


def discover(project: Path, env: dict | None = None) -> list[Session]:
    """The sessions agent CLIs on this machine logged for ``project`` (or a directory in it)."""
    env = os.environ if env is None else env
    home = Path(env.get("HOME", str(Path.home())))
    project = project.resolve()
    found: list[Session] = []
    claude = Path(env.get("CLAUDE_CONFIG_DIR") or home / ".claude") / "projects"
    if claude.is_dir():
        prefix = re.sub(r"[^A-Za-z0-9]", "-", str(project))
        for d in claude.iterdir():
            if d.is_dir() and d.name.startswith(prefix):
                found += [s for p in sorted(d.glob("*.jsonl")) if (s := read_claude_code(p)) and _within(s.cwd, project)]
    pi = Path(env.get("PI_CODING_AGENT_DIR") or home / ".pi/agent") / "sessions"
    if pi.is_dir():
        for p in sorted(pi.glob("*/*.jsonl")):
            if (s := read_pi(p)) and _within(s.cwd, project):
                found.append(s)
    codex = Path(env.get("CODEX_HOME") or home / ".codex") / "sessions"
    if codex.is_dir():
        for p in sorted(codex.rglob("*.jsonl")):
            if (s := read_codex(p)) and _within(s.cwd, project):
                found.append(s)
    return found


def load(source: str | None, project: Path, env: dict | None = None) -> list[Session]:
    """Sessions from ``source`` (a file or directory of traces), or discovered for ``project``."""
    if source is None:
        return discover(project, env)
    root = Path(source).expanduser()
    if not root.exists():
        raise ValueError(f"no traces at {source}")
    return read(_files(root))
