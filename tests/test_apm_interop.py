"""`hlock export --format apm-lock` read by the real `apm` CLI: a frozen install changes
nothing, and `apm audit` finds no drift.

Needs `apm` on PATH and network access to GitHub, so it runs only with HLOCK_TEST_APM=1.
"""

import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock.cli import main

pytestmark = pytest.mark.skipif(not (os.environ.get("HLOCK_TEST_APM") and shutil.which("apm")),
                                reason="set HLOCK_TEST_APM=1 and put apm on PATH")


def test_apm_install_frozen_is_a_no_op(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "HLOCK_GITHUB", "XDG_")):
            monkeypatch.delenv(key)
    bindir = tmp_path / "bin"
    bindir.mkdir()
    claude = bindir / "claude"
    claude.write_text('#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    claude.chmod(claude.stat().st_mode | stat.S_IXUSR)
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "hlock-home"))
    monkeypatch.setenv("NO_COLOR", "1")
    project = tmp_path / "project"
    (project / ".claude").mkdir(parents=True)
    monkeypatch.chdir(project)
    Path("apm.yml").write_text("name: interop\nversion: 0.1.0\ntargets: [claude]\ndependencies:\n  apm:\n"
                               "    - anthropics/skills/skills/pdf\n")
    run = CliRunner().invoke
    assert run(main, ["pin", "--harness", "claude-code"]).exit_code == 0
    r = run(main, ["sync", "--yes"])
    assert r.exit_code == 0, r.output
    r = run(main, ["export", "--format", "apm-lock"])
    assert r.exit_code == 0, r.output
    before = Path("apm.lock.yaml").read_text()
    files = {p: p.read_bytes() for p in Path(".claude").rglob("*") if p.is_file()}
    out = subprocess.run(["apm", "install", "--frozen"], capture_output=True, text=True, check=False)
    # the first frozen install fills APM's own cache (apm_modules/); nothing hlock placed changes
    assert out.returncode == 0 and "files unchanged" in out.stdout, out.stdout + out.stderr
    assert Path("apm.lock.yaml").read_text() == before
    assert {p: p.read_bytes() for p in Path(".claude").rglob("*") if p.is_file()} == files
    audit = subprocess.run(["apm", "audit"], capture_output=True, text=True, check=False)
    assert audit.returncode == 0 and "No drift" in audit.stdout + audit.stderr, audit.stdout + audit.stderr
