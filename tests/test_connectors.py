"""Tests for hlock.connectors: inferring used connector MCP servers from agent transcripts."""

from __future__ import annotations

import json
from pathlib import Path

from hlock.connectors import (
    Use,
    _server_id,
    classify,
    infer,
    infer_connectors,
    uses_from,
)


def _transcript(p: Path, entries: list[dict]) -> None:
    with open(p, "w") as f:
        f.writelines(json.dumps(e) + "\n" for e in entries)


def test_server_id_parses_namespaced_tools() -> None:
    assert _server_id("mcp__slack__postMessage") == "slack"
    assert _server_id("mcp__slack") == "slack"
    assert _server_id("linear_getIssue") is None
    assert _server_id("Read") is None


def test_classify_maps_known_and_unknown() -> None:
    assert classify("mcp__slack__postMessage") == ("slack", True)
    assert classify("linear_getIssue") == ("linear", True)
    assert classify("github_create_issue") == ("github", True)
    # the exact namespaced server id wins over a generic prefix
    assert classify("mcp__linear__search") == ("linear", True)
    # a tool the catalog can't name is reported as its server / first word, unknown
    assert classify("mcp__internal__x") == ("internal", False)
    assert classify("my_custom_read") == ("my", False)
    # file / editor / system tools are never connectors
    assert classify("Read") == ("", False)
    assert classify("Bash") == ("", False)
    assert classify("write_file") == ("", False)
    assert classify("") == ("", False)


def test_uses_from_per_harness(tmp_path: Path) -> None:
    cc = tmp_path / "cc.jsonl"
    _transcript(cc, [
        {"type": "user", "sessionId": "s", "message": {"content": "hi"}},
        {"type": "assistant", "sessionId": "s", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "mcp__slack__x", "input": {}}]}},
    ])
    got = uses_from(cc)
    assert [(g.name, g.harness) for g in got] == [("mcp__slack__x", "claude-code")]

    pi = tmp_path / "pi.jsonl"
    _transcript(pi, [
        {"type": "session", "cwd": "/p"},
        {"type": "message", "message": {"role": "assistant", "content": [
            {"type": "toolCall", "name": "notion_searchPages", "arguments": {}}]}},
    ])
    assert [g.name for g in uses_from(pi)] == ["notion_searchPages"]

    cx = tmp_path / "cx.jsonl"
    _transcript(cx, [
        {"type": "response_item", "payload": {"type": "function_call", "name": "github_issue"}},
    ])
    assert [g.harness for g in uses_from(cx)] == ["codex"]

    # a CSV or unrecognized file yields nothing
    (tmp_path / "r.csv").write_text("prompt\nhi\n")
    assert uses_from(tmp_path / "r.csv") == []
    assert uses_from(tmp_path / "x.unknown") == []


def test_infer_aggregates_and_excludes_noise() -> None:
    uses = [
        Use("mcp__slack__a", "2025-01-01", "/p", "claude-code"),
        Use("mcp__slack__b", "2025-01-02", "/p", "claude-code"),
        Use("linear_getIssue", "2025-01-03", "/p", "claude-code"),
        Use("github_x", "2025-02-01", None, "codex"),
        Use("mcp__custom__t", "2025-03-01", None, "claude-code"),  # unknown server
        Use("Read", "2025-03-01", None, "claude-code"),            # not a connector
        Use("Bash", "2025-03-01", None, "claude-code"),
    ]
    r = infer_connectors(uses, transcripts=7)
    ids = [(c.id, c.uses) for c in r.connectors]
    assert ids == [("slack", 2), ("github", 1), ("linear", 1)]
    assert r.connectors[0].first == "2025-01-01" and r.connectors[0].last == "2025-01-02"
    assert r.connectors[2].harnesses == ["claude-code"]
    assert r.unknown == [("custom", 1)]  # the unknown mcp server surfaces for re-linking
    assert r.transcripts == 7 and r.uses_seen == len(uses)


def test_infer_scans_a_session_root(monkeypatch, tmp_path: Path) -> None:
    # a fake ~/.claude/projects tree
    projects = tmp_path / "home" / ".claude" / "projects" / "some-project"
    projects.mkdir(parents=True)
    _transcript(projects / "s.jsonl", [
        {"type": "user", "sessionId": "s", "message": {"content": "hi"}},
        {"type": "assistant", "sessionId": "s", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "mcp__slack__x", "input": {}}]}},
    ])
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
    r = infer()
    assert [c.id for c in r.connectors] == ["slack"]
    assert r.transcripts == 1
