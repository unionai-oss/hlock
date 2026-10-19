"""Generate the tables in docs/reference/harnesses.md from hlock's built-in profiles.

    python scripts/harness_table.py           # rewrites the generated blocks
    python scripts/harness_table.py --check   # exits 1 if the page is out of date

Each table sits between `<!-- generated:<name> -->` and `<!-- /generated:<name> -->`
markers; the prose around them is hand-written. Profiles are read through hlock's own
registry in a throwaway home and working directory, so your own profile overrides and
project harnesses don't leak into the page.
"""

from __future__ import annotations

import json
import os
import re
import sys
import tempfile
from pathlib import Path

import harness_lock

REPO = Path(__file__).resolve().parent.parent
PAGE = REPO / "docs" / "reference" / "harnesses.md"

YES, NO = "✓", "—"


def builtin_profiles() -> tuple[list[dict], list[dict]]:
    """(harness profiles, runtime rows), built-ins only, in registry order."""
    saved_env, saved_cwd = dict(os.environ), os.getcwd()
    with tempfile.TemporaryDirectory() as tmp:
        try:
            os.environ["HOME"] = tmp
            os.environ.pop("HLOCK_PROFILE_PATH", None)
            os.chdir(tmp)
            rows = json.loads(harness_lock.harnesses(probe=False))
            harnesses = [json.loads(harness_lock.harness_profile(r["id"]))["profile"]
                         for r in rows if r["origin"] == "builtin"]
            runtimes = [r for r in json.loads(harness_lock.runtimes(probe=False))
                        if r["origin"] == "builtin"]
        finally:
            os.chdir(saved_cwd)
            os.environ.clear()
            os.environ.update(saved_env)
    return harnesses, runtimes


def code(items) -> str:
    return ", ".join(f"`{i}`" for i in items) or NO


def mark(flag) -> str:
    return YES if flag else NO


def name_cell(p: dict) -> str:
    return f"[{p['name']}]({p['homepage']})" if p.get("homepage") else p["name"]


def id_cell(p: dict) -> str:
    aliases = p.get("aliases") or []
    return f"`{p['id']}`" + (f" ({code(aliases)})" if aliases else "")


def project_instructions(p: dict) -> list[str]:
    """Instruction files in the project (the ones in the config root are implied)."""
    return [i for i in p.get("instructions") or [] if not i.startswith(("{root}", "~"))]


def all_instructions(p: dict) -> str:
    """Every instruction file, grouped by directory, with `{root}` spelled as the default
    config root: `AGENTS.md`; `~/.hermes/`: `SOUL.md`."""
    root = (p.get("config-root") or {}).get("default") or "{root}"
    dirs: dict[str, list[str]] = {}
    for i in p.get("instructions") or []:
        d, _, name = i.replace("{root}", root).rpartition("/")
        dirs.setdefault(d, []).append(name)
    return "; ".join(f"`{d}/`: {code(names)}" if d else code(names) for d, names in dirs.items()) or NO


def table(header: list[str], rows: list[list[str]]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(lines)


def localize_cell(p: dict) -> str:
    # Mirrors `is_local_harness` in crates/harness-lock-core/src/localize.rs.
    if not p.get("local-models"):
        return NO
    if not p.get("open-source"):
        return "with `--harness`"
    # `localize` picks it, but without `local-provider` you point it at the endpoint
    return YES if p.get("local-provider") else "by hand"


def deciders_cell(p: dict) -> str:
    """The decision-model uses a harness takes, and how: `approve, stop (hooks)`."""
    deciders = p.get("deciders") or {}
    hooked = [u for u, w in deciders.items() if w.get("hook")]
    read = [u for u, w in deciders.items() if w.get("native") and not w.get("hook")]
    fmt = {(w.get("hook") or {}).get("format") for w in deciders.values()}
    how = "plugin" if fmt & {"amp", "opencode", "pi"} else "hooks"
    cells = ([f"{', '.join(hooked)} ({how})"] if hooked else []) + ([f"{', '.join(read)} (recorded)"] if read else [])
    return "; ".join(cells) or NO


def render(harnesses: list[dict], runtimes: list[dict]) -> dict[str, str]:
    clis = [p for p in harnesses if p["kind"] == "cli" and not p.get("autonomous")]
    autonomous = [p for p in harnesses if p["kind"] == "cli" and p.get("autonomous")]
    frameworks = [p for p in harnesses if p["kind"] == "framework"]

    def overview(group: list[dict]) -> str:
        return table(
            ["Harness", "Vendor", "Name in hlock", "Support", "Open source", "Installs from"],
            [[name_cell(p), p.get("vendor") or NO, id_cell(p), p.get("support") or "basic",
              mark(p.get("open-source")),
              code(dict.fromkeys(i["method"] for i in p.get("install") or []))]
             for p in group],
        )

    def capture(group: list[dict], instructions) -> str:
        return table(
            ["Harness", "Model", "Providers", "MCP servers", "Skills", "Plugins", "Instructions", "`localize`", "Deciders"],
            [[p["name"], mark(p["model"]["default"]), mark((p.get("providers") or {}).get("source")),
              mark(p["mcp"]["sources"]), mark(p["skills"]["dirs"]),
              ", ".join(dict.fromkeys(b["kind"] + "s" for b in p.get("bundles") or [])) or NO,
              instructions(p), localize_cell(p), deciders_cell(p)]
             for p in group],
        )

    return {
        "clis": overview(clis),
        "capture": capture(clis, lambda p: code(project_instructions(p))),
        "autonomous": overview(autonomous),
        "autonomous-capture": capture(autonomous, all_instructions),
        "frameworks": table(
            ["Framework", "Vendor", "Name in hlock", "Packages pinned", "Instructions", "Skills"],
            [[name_cell(p), p.get("vendor") or NO, id_cell(p),
              code(pkg.split(":", 1)[-1] for pkg in p.get("packages") or []),
              code(project_instructions(p)), code(p["skills"]["dirs"])]
             for p in frameworks],
        ),
        "runtimes": table(
            ["Runtime", "Name in hlock", "Default port"],
            [[r["name"], id_cell(r), code(r.get("ports") or [])] for r in runtimes],
        ),
    }


def update(text: str, blocks: dict[str, str]) -> str:
    for name, body in blocks.items():
        pattern = re.compile(rf"(<!-- generated:{name} -->\n).*?(<!-- /generated:{name} -->)", re.DOTALL)
        if not pattern.search(text):
            raise SystemExit(f"{PAGE.relative_to(REPO)}: no <!-- generated:{name} --> block")
        text = pattern.sub(lambda m, body=body: m.group(1) + body + "\n" + m.group(2), text)
    return text


def main() -> int:
    old = PAGE.read_text()
    new = update(old, render(*builtin_profiles()))
    if "--check" in sys.argv[1:]:
        if new != old:
            print(f"{PAGE.relative_to(REPO)} is out of date; run `uv run python scripts/harness_table.py`")
            return 1
        return 0
    if new != old:
        PAGE.write_text(new)
        print(f"wrote {PAGE.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
