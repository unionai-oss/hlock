"""Infer which connector MCP servers a user actually uses, from their agent session
transcripts (the OAuth-connectors discussion in specs/migrate.md).

Claude Code connectors (Slack, Linear, …) are OAuth MCP servers whose *connection* is
authorized against the user's account; the authorization is per-client and can't be
ported. But the one local, factual record of which connectors a user works with is their
session transcripts: when a connector's tools are used, the harness logs a tool call that
is namespaced by the server (`mcp__slack__…`) or prefixed by it (`slack_…`).

Scanning those logs turns *"connectors you demonstrably use"* into a portable list — a
dry run hlock can show before a migrate writes anything. It is an inference of usage, not
a roster of what's connected on the account (that needs the account API): a connector
connected but never used on this machine won't appear, and one used on another device or
in the web app leaves no transcript here. The catalog maps tool/serving ids to connector
names for the ports it can suggest; servers it can't name are reported as-is for
re-linking by hand.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path

from hlock.traces import _jsonl

# connector id -> (display name, prefixes that identify it). A tool name matches a
# connector when its `mcp__<server>__…` server id or its bare name starts with one of the
# prefixes. Prefixes are kept distinctive so generic tool names aren't misread.
CONNECTORS: dict[str, tuple[str, tuple[str, ...]]] = {
    "slack": ("Slack", ("slack",)),
    "linear": ("Linear", ("linear",)),
    "github": ("GitHub", ("github",)),
    "notion": ("Notion", ("notion",)),
    "confluence": ("Confluence", ("confluence",)),
    "jira": ("Jira", ("jira",)),
    "gmail": ("Gmail", ("gmail",)),
    "google-calendar": ("Google Calendar", ("google_calendar", "googlecalendar", "calendar")),
    "google-docs": ("Google Docs", ("google_docs", "googledocs", "docs_")),
    "google-sheets": ("Google Sheets", ("google_sheets", "googlesheets", "sheets_")),
    "google-drive": ("Google Drive", ("google_drive", "googledrive", "drive_")),
    "sentry": ("Sentry", ("sentry",)),
    "rollbar": ("Rollbar", ("rollbar",)),
    "playwright": ("Playwright", ("playwright",)),
    "salesforce": ("Salesforce", ("salesforce",)),
    "hubspot": ("HubSpot", ("hubspot",)),
    "airtable": ("Airtable", ("airtable",)),
    "zendesk": ("Zendesk", ("zendesk",)),
    "stripe": ("Stripe", ("stripe",)),
    "figma": ("Figma", ("figma",)),
    "1password": ("1Password", ("1password", "onepassword")),
    "postgres": ("Postgres", ("postgres",)),
}

# tool names that are never connectors, so an unknown one isn't reported as a connector.
_NOT_CONNECTORS = (
    "read_file", "read", "list_dir", "list", "search", "grep", "write_file", "edit_file",
    "create_file", "code_execution", "task", "bash", "shell", "exit", "kill_bash",
    "web_search", "web_fetch", "execute_command", "run", "notify", "sleep", "filesystem",
    "get_terminal_output", "ask_user", "browser",
)


@dataclass
class Use:
    """One tool call that looks like it belongs to an MCP server."""

    name: str  # the full tool name from the transcript
    timestamp: str
    cwd: str | None
    harness: str


@dataclass
class Inferred:
    """A connector the user demonstrably uses, with how much evidence."""

    id: str
    name: str
    uses: int
    first: str
    last: str
    harnesses: list[str] = field(default_factory=list)


@dataclass
class InferResult:
    connectors: list[Inferred]
    unknown: list[tuple[str, int]]  # (server/name, uses) things the catalog can't name
    transcripts: int
    uses_seen: int


# --------------------------------------------------------------------------- #
# Scanning transcripts
# --------------------------------------------------------------------------- #

def _tool_blocks(content) -> list:
    return [b for b in content if isinstance(b, dict)] if isinstance(content, list) else []


def _claude_uses(path: Path) -> list[Use]:
    out = []
    for e in _jsonl(path):
        if e.get("isSidechain") or e.get("isMeta"):
            continue
        msg = e.get("message") if isinstance(e.get("message"), dict) else {}
        for b in _tool_blocks(msg.get("content")):
            if b.get("type") == "tool_use" and isinstance(b.get("name"), str) and b["name"]:
                out.append(Use(b["name"], str(e.get("timestamp", "")), e.get("cwd"), "claude-code"))
    return out


def _pi_uses(path: Path) -> list[Use]:
    out, cwd = [], None
    for e in _jsonl(path):
        if e.get("type") == "session":
            cwd = e.get("cwd")
        msg = e.get("message") if isinstance(e.get("message"), dict) else {}
        for b in _tool_blocks(msg.get("content")):
            if b.get("type") not in ("toolCall", "tool_use", "tool_call"):
                continue
            name = b.get("name") or (b.get("tool") or {}).get("name") if isinstance(b, dict) else None
            if isinstance(name, str) and name:
                out.append(Use(name, str(e.get("timestamp", "")), cwd, "pi"))
    return out


def _codex_uses(path: Path) -> list[Use]:
    out = []
    for e in _jsonl(path):
        p = e.get("payload") if isinstance(e.get("payload"), dict) else {}
        if e.get("type") != "response_item" or not isinstance(p, dict):
            continue
        if p.get("type") in ("function_call", "custom_tool_call"):
            name = p.get("name") or (p.get("arguments") or {}).get("name")
            if isinstance(name, str) and name:
                out.append(Use(name, str(e.get("timestamp", "")), p.get("cwd"), "codex"))
    return out


def _sniff(path: Path) -> str:
    """Which reader a JSONL file needs, from its first few entries (echoes traces)."""
    for i, e in enumerate(_jsonl(path)):
        if e.get("type") in ("session_meta", "response_item"):
            return "codex"
        if e.get("type") == "session" and "cwd" in e:
            return "pi"
        if e.get("type") in ("user", "assistant") and "sessionId" in e:
            return "claude-code"
        if i >= 5:
            break
    return ""


def uses_from(path: Path) -> list[Use]:
    """The server-ish tool calls in one transcript, by its format."""
    if path.suffix == ".csv" or not path.is_file():
        return []
    kind = _sniff(path)
    if kind == "claude-code":
        return _claude_uses(path)
    if kind == "pi":
        return _pi_uses(path)
    if kind == "codex":
        return _codex_uses(path)
    return []


def _session_roots(env: dict | None) -> list[Path]:
    """The session directories hlock knows to scan, that exist."""
    env = os.environ if env is None else env
    home = Path(env.get("HOME", str(Path.home())))
    dirs = []
    for root in (
        Path(env.get("CLAUDE_CONFIG_DIR") or home / ".claude") / "projects",
        Path(env.get("PI_CODING_AGENT_DIR") or home / ".pi/agent") / "sessions",
        Path(env.get("CODEX_HOME") or home / ".codex") / "sessions",
    ):
        if root.is_dir():
            dirs.append(root)
    return dirs


def _transcripts(roots: list[Path], project: Path | None, max_files: int) -> list[Path]:
    out = []
    for root in roots:
        for p in sorted(root.rglob("*.jsonl")):
            if not p.is_file():
                continue
            if project is not None and project.resolve() not in p.parents:
                continue
            out.append(p)
            if len(out) >= max_files:
                return out
    return out


def infer(env: dict | None = None, project: Path | None = None, max_files: int = 5000) -> InferResult:
    """Scan the machine's transcripts and classify their tool calls into connectors."""
    paths = _transcripts(_session_roots(env), project, max_files)
    uses: list[Use] = []
    for p in paths:
        uses.extend(uses_from(p))
    return infer_connectors(uses, len(paths))


# --------------------------------------------------------------------------- #
# Classifying into connectors
# --------------------------------------------------------------------------- #

def _server_id(tool: str) -> str | None:
    """The `mcp__<server>__<tool>` server id, when the tool is namespaced."""
    m = re.match(r"^mcp__(.+?)(?:__.*)?$", tool)
    return m.group(1).strip().lower() if m else None


def classify(tool: str) -> tuple[str, bool]:
    """Map a tool name to a `(connector id, known)` pair.

    `known=False` names the connector the catalog can't map (the `mcp__` server id, the
    first word, or the name), or `""` for a tool that's not a connector.
    """
    if not tool:
        return ("", False)
    low = tool.lower()
    server = _server_id(tool)
    # an exact, namespaced server id is the strongest signal
    if server and server in CONNECTORS:
        return server, True
    for cid, (name, prefixes) in CONNECTORS.items():
        for pre in prefixes:
            if low.startswith(pre) or (server and server.startswith(pre)):
                return cid, True
    if low.startswith(_NOT_CONNECTORS):
        return ("", False)
    return (server or low.split("_")[0] or low, False)


def infer_connectors(uses: list[Use], transcripts: int = 0) -> InferResult:
    """Aggregate tool calls into the connectors they evidence."""
    known: dict[str, list[Use]] = {}
    unknown: dict[str, int] = {}
    for u in uses:
        cid, is_connector = classify(u.name)
        if cid == "":
            continue
        if is_connector:
            known.setdefault(cid, []).append(u)
        else:
            unknown[cid] = unknown.get(cid, 0) + 1
    connectors = []
    for cid, us in known.items():
        connectors.append(Inferred(
            id=cid, name=CONNECTORS[cid][0], uses=len(us),
            first=min((u.timestamp for u in us if u.timestamp), default=""),
            last=max((u.timestamp for u in us if u.timestamp), default=""),
            harnesses=sorted({u.harness for u in us}),
        ))
    connectors.sort(key=lambda c: (-c.uses, c.name))
    return InferResult(connectors, sorted(unknown.items(), key=lambda kv: -kv[1]), transcripts, len(uses))
