"""Render the README / docs terminal screenshot from a real `hlock` session.

    python scripts/cli_screenshot.py          # writes docs/assets/quickstart.svg

Pins a hand-configured Claude Code setup (a fake `claude` binary in a throwaway home),
checks it, then syncs it into a second, empty home, recording the CLI's own rich console,
so the picture always matches the real output.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import tempfile
from pathlib import Path

from click.testing import CliRunner
from rich.console import Console
from rich.terminal_theme import MONOKAI

from harness_lock import cli

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "docs" / "assets" / "quickstart.svg"

SECRET = "HLOCK_SECRET_GITHUB_GITHUB_TOKEN"

# (comment shown above the command, home it runs in, args)
SESSION = [
    ("on your laptop", "laptop", ["pin"]),
    (None, "laptop", ["check"]),
    ("on another machine, after git clone", "fresh", ["sync", "--yes"]),
]


def fake_bin(bindir: Path, name: str, output: str) -> None:
    p = bindir / name
    p.write_text(f"#!/bin/sh\necho '{output}'\n")
    p.chmod(0o755)


def setup(tmp: Path) -> None:
    """A laptop with Claude Code configured by hand, and an empty second machine."""
    bindir, laptop = tmp / "bin", tmp / "laptop"
    (tmp / "project").mkdir()
    (tmp / "fresh").mkdir()
    bindir.mkdir()
    fake_bin(bindir, "claude", "2.1.0 (Claude Code)")
    fake_bin(bindir, "npx", "10.9.0")
    skill = laptop / ".claude/skills/review-pr"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: review-pr\ndescription: Review a pull request\n---\n")
    (laptop / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (laptop / ".claude/CLAUDE.md").write_text("Be terse.\n")
    (laptop / ".claude.json").write_text(json.dumps({"mcpServers": {"github": {
        "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
        "env": {"GITHUB_TOKEN": "ghp_0123456789abcdefABCDEF0123456789"}}}}))
    os.environ["PATH"] = f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin"
    os.environ[SECRET] = "ghp_…"
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "XDG_", "OLLAMA_")):
            del os.environ[key]


def main() -> None:
    rec = Console(file=io.StringIO(), record=True, force_terminal=True, width=80, theme=cli.THEME, highlight=False)
    cli.console = cli.err_console = rec
    cli._working = lambda _message: cli._friendly_errors()  # spinners don't belong in a still
    rec.status = lambda *_args, **_kwargs: contextlib.nullcontext()
    saved = dict(os.environ)
    cwd = os.getcwd()
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        setup(tmp)
        os.chdir(tmp / "project")
        try:
            for i, (comment, home, args) in enumerate(SESSION):
                if i:
                    rec.print()
                if comment:
                    rec.print(f"[dim]# {comment}[/]")
                os.environ["HOME"] = str(tmp / home)
                shown = " ".join(f'"{a}"' if " " in a else a for a in args)
                rec.print(f"[bold green]$[/] [bold]hlock {shown}[/]")
                CliRunner().invoke(cli.main, args, catch_exceptions=False)
        finally:
            os.chdir(cwd)
            os.environ.clear()
            os.environ.update(saved)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(rec.export_svg(title="hlock", theme=MONOKAI))
    print(f"wrote {OUT.relative_to(REPO)}")


if __name__ == "__main__":
    main()
