"""Render the README / docs terminal screenshot from a real `hlock` session.

    python scripts/cli_screenshot.py          # writes docs/assets/quickstart.svg

Runs evals init -> init -> resolve -> check -> run in a temp dir,
recording the CLI's own rich console, so the picture always matches the real output.
"""

from __future__ import annotations

import io
import os
import tempfile
from pathlib import Path

from click.testing import CliRunner
from rich.console import Console
from rich.terminal_theme import MONOKAI

from harness_lock import cli

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "docs" / "assets" / "quickstart.svg"

SESSION = [
    ["evals", "init"],
    ["init", "--name", "demo"],
    ["resolve"],
    ["check"],
    ["run", "count the words in words.txt"],
]


def main() -> None:
    rec = Console(file=io.StringIO(), record=True, force_terminal=True, width=72, theme=cli.THEME, highlight=False)
    cli.console = cli.err_console = rec
    cli._working = lambda _message: cli._friendly_errors()  # spinners don't belong in a still
    with tempfile.TemporaryDirectory() as tmp:
        cwd = os.getcwd()
        os.chdir(tmp)
        try:
            for i, args in enumerate(SESSION):
                if i:
                    rec.print()
                shown = " ".join(f'"{a}"' if " " in a else a for a in args)
                rec.print(f"[bold green]$[/] [bold]hlock {shown}[/]")
                CliRunner().invoke(cli.main, args, catch_exceptions=False)
        finally:
            os.chdir(cwd)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(rec.export_svg(title="hlock", theme=MONOKAI))
    print(f"wrote {OUT.relative_to(REPO)}")


if __name__ == "__main__":
    main()
