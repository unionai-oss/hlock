"""The hlock Agent Skill in skills/hlock: valid frontmatter, real commands and flags, live links,
and installed copies that match the source."""

import filecmp
import re
import shlex
from pathlib import Path

import click
import pytest

from hlock.cli import main

REPO = Path(__file__).resolve().parent.parent
SKILL = REPO / "skills" / "hlock"
FILES = sorted(SKILL.rglob("*.md"))
# Where `hlock add skill ./skills/hlock` installed it for the repo's locks in .hlock/locks/.
INSTALLED = [REPO / ".claude" / "skills" / "hlock", REPO / ".pi" / "skills" / "hlock"]


def frontmatter(text: str) -> dict[str, str]:
    m = re.match(r"---\n(.*?)\n---\n", text, re.DOTALL)
    assert m, "SKILL.md must start with a --- frontmatter block"
    return dict(line.split(": ", 1) for line in m.group(1).splitlines())


def invocations(text: str) -> list[str]:
    """Every `hlock …` command in inline code and in code blocks."""
    found = re.findall(r"`(hlock [^`]+)`", text)
    for block in re.findall(r"```(?:bash|console)?\n(.*?)```", text, re.DOTALL):
        found += [line.removeprefix("$ ") for line in block.splitlines() if line.removeprefix("$ ").startswith("hlock ")]
    return [re.split(r"\s+#|\s*\|\s|\s&&\s", s)[0].strip() for s in found]


def check_invocation(line: str) -> None:
    tokens = shlex.split(line.replace("…", ""))[1:]
    cmd: click.Command = main
    known = {o for p in main.params for o in p.opts}
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        if tok == "--" or tok.startswith("<"):
            return
        if tok.startswith("-"):
            name = tok.split("=", 1)[0]
            assert name in known | {"--help", "-h"}, f"{line!r}: {cmd.name} has no option {name}"
            if not any(name in p.opts and getattr(p, "is_flag", False) for p in cmd.params):
                i += 1  # skip the option's value (or a global option's)
        elif isinstance(cmd, click.Group) and tok in cmd.commands:
            cmd = cmd.commands[tok]
            known = {o for p in cmd.params for o in p.opts}
        elif isinstance(cmd, click.Group):
            pytest.fail(f"{line!r}: {cmd.name} has no command {tok}")
        elif cmd.name in ("launch", "run"):
            return  # the rest goes to the harness, or is the prompt
        i += 1


def test_frontmatter_follows_the_agent_skills_spec():
    meta = frontmatter((SKILL / "SKILL.md").read_text())
    assert meta["name"] == SKILL.name
    assert re.fullmatch(r"[a-z0-9]+(-[a-z0-9]+)*", meta["name"]) and len(meta["name"]) <= 64
    assert 0 < len(meta["description"]) <= 1024


@pytest.mark.parametrize("path", FILES, ids=lambda p: str(p.relative_to(SKILL)))
def test_every_command_and_flag_exists(path):
    lines = invocations(path.read_text())
    assert lines, f"no hlock commands found in {path.name}"
    for line in lines:
        check_invocation(line)


@pytest.mark.parametrize("path", FILES, ids=lambda p: str(p.relative_to(SKILL)))
def test_links_resolve(path):
    text = path.read_text()
    for target in re.findall(r"\]\((?!https?:)([^)#]+)", text):
        assert (path.parent / target).exists(), f"{path.name} links to missing {target}"
    for page in re.findall(r"https://docs\.hlock\.ai/([a-z0-9/-]+?)/?(?:#[a-z0-9-]+)?[)\s,]", text):
        assert (REPO / "docs" / f"{page}.md").exists(), f"{path.name} links to missing docs page {page}"


@pytest.mark.parametrize("copy", INSTALLED, ids=lambda p: str(p.relative_to(REPO)))
def test_installed_copies_match_the_source(copy):
    cmp = filecmp.dircmp(SKILL, copy)

    def differs(c: filecmp.dircmp) -> bool:
        return bool(c.left_only or c.right_only or c.diff_files) or any(differs(s) for s in c.subdirs.values())

    assert not differs(cmp), (
        f"{copy.relative_to(REPO)} is stale: run "
        "`hlock --lock .hlock/locks/harness.<harness>.lock upgrade dep:hlock --apply` for each lock")
