"""Generated docs stay in sync with what they're generated from."""

import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def test_harness_tables_match_the_builtin_profiles():
    r = subprocess.run([sys.executable, "scripts/harness_table.py", "--check"],
                       check=False, cwd=REPO, capture_output=True, text=True)
    assert r.returncode == 0, r.stdout + r.stderr
