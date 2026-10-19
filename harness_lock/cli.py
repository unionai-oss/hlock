"""The `hlock` command line interface.

Thin Python layer over the compiled Rust bindings (`hlock`), so the CLI story stays
consistent with the Python API. Install the wheel (pip / uv / uvx) and run `hlock`.

Output is rendered with `rich`: color and spinners on a terminal, plain text when piped
(or with ``NO_COLOR`` set). Commands that read state also take ``--json`` for scripting.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

import rich_click as click
from rich.console import Console, Group
from rich.markup import escape
from rich.panel import Panel
from rich.table import Table
from rich.text import Text
from rich.theme import Theme
from rich.tree import Tree

from harness_lock import (
    LOCK_VERSION,
    Lockfile,
    Manifest,
    __version__,
    agent_run,
    harness_profile,
    harnesses,
    is_pull_source,
    launch_prepare,
    load,
    meta_check,
    meta_config,
    meta_init,
    meta_pin,
    meta_prepare,
    meta_verify,
    run_code_harness,
    runtimes,
    secret_delete,
    secret_set,
    secret_status,
    sync_dockerfile,
    sync_ignore_env,
    sync_plan,
    sync_run_step,
    sync_skill,
)
from harness_lock import calibrate_eval_pack as _calibrate_eval_pack
from harness_lock import check_eval_pack as _check_eval_pack
from harness_lock import check_pin as _check_pin
from harness_lock import decide as _decide
from harness_lock import decide_hook as _decide_hook
from harness_lock import decider_drift as _decider_drift
from harness_lock import evals as _evals
from harness_lock import grade_eval_task as _grade_eval_task
from harness_lock import jobs as _jobs
from harness_lock import localize as _localize
from harness_lock import pin as _pin
from harness_lock import pull as _pull
from harness_lock import pull_env as _pull_env
from harness_lock import pull_find as _pull_find
from harness_lock import pull_list as _pull_list
from harness_lock import pull_remove as _pull_remove
from harness_lock import resolve as _resolve
from harness_lock import select_harness as _select_harness
from harness_lock import traces as _traces
from harness_lock import tune as al_tune
from harness_lock import upgrade as al_upgrade

MANIFEST = "harness.toml"
LOCK = "harness.lock"


class _Project:
    """Where this invocation's manifest and lock live (set by the top-level options)."""

    manifest: Path = Path(MANIFEST)
    lock: Path = Path(LOCK)


def _find_up(name: str, start: Path) -> Path | None:
    for d in (start, *start.parents):
        if (d / name).is_file():
            return d / name
    return None


def _set_project(manifest: str | None, lock: str | None) -> None:
    """`-m` / `HLOCK_MANIFEST`, else the nearest harness.toml at or above the working
    directory, else ./harness.toml. The lock defaults to harness.lock next to the
    manifest (or the nearest harness.lock when there's no manifest). `--lock NAME`
    names a pulled lock when no such file exists."""
    cwd = Path.cwd()
    m = Path(manifest).expanduser() if manifest else _find_up(MANIFEST, cwd)
    if m is not None and not m.is_absolute():
        m = cwd / m
    if lock:
        lk = Path(lock).expanduser()
        lk = lk if lk.is_absolute() else cwd / lk
        if not lk.is_file() and (stored := _pull_find(lock)):
            lk = Path(stored)
    elif m is not None:
        lk = m.parent / LOCK
    else:
        lk = _find_up(LOCK, cwd) or cwd / LOCK
    _Project.manifest = m if m is not None else cwd / MANIFEST
    _Project.lock = lk


def _m() -> str:
    """The manifest path."""
    return str(_Project.manifest)


def _l() -> str:
    """The lock path."""
    return str(_Project.lock)


def _show(path: str | Path) -> str:
    """A path for messages: relative to the working directory when it's inside it."""
    try:
        return str(Path(path).resolve().relative_to(Path.cwd().resolve()))
    except ValueError:
        return str(path)


def _md() -> str:
    return _show(_Project.manifest)


def _ld() -> str:
    return _show(_Project.lock)

THEME = Theme(
    {
        "brand": "bold magenta",
        "ok": "bold green",
        "fail": "bold red",
        "warn": "bold yellow",
        "key": "cyan",
        "path": "bold blue",
        "cmd": "bold cyan",
        "muted": "dim",
    }
)
console = Console(theme=THEME, highlight=False)
err_console = Console(theme=THEME, stderr=True, highlight=False)

click.rich_click.TEXT_MARKUP = "rich"
click.rich_click.STYLE_OPTION = "bold cyan"
click.rich_click.STYLE_COMMAND = "bold cyan"
click.rich_click.STYLE_SWITCH = "bold green"
click.rich_click.STYLE_METAVAR = "yellow"
click.rich_click.STYLE_USAGE = "bold magenta"
click.rich_click.MAX_WIDTH = 100
click.rich_click.COMMAND_GROUPS = {
    "hlock": [
        {"name": "Lock", "commands": ["pin", "check"]},
        {"name": "Rehydrate & run", "commands": ["sync", "launch", "run"]},
        {"name": "Optimize", "commands": ["evals", "init", "resolve", "tune", "upgrade", "decide"]},
        {"name": "Go local", "commands": ["localize"]},
        {"name": "Harness", "commands": ["harness", "runtime"]},
        {"name": "Inspect", "commands": ["show", "tree", "backend"]},
        {"name": "Meta", "commands": ["meta"]},
    ],
    "hlock meta": [
        {"name": "Set up", "commands": ["init", "pin", "check", "show", "sync", "launch"]},
        {"name": "Jobs", "commands": ["jobs", "run", "verify"]},
    ],
}


# Code harnesses and the devbox worker default to the interpreter running hlock.
os.environ.setdefault("HL_PYTHON", sys.executable)


def _fail(message: str, hint: str | None = None) -> None:
    """Exit 1 with a one-line error (and an optional hint), never a traceback."""
    err_console.print(f"[fail]✗ error:[/] {message}", soft_wrap=True)
    if hint:
        err_console.print(f"  [muted]hint:[/] {hint}")
    sys.exit(1)


def _hint_for(message: str) -> str | None:
    if "cannot read" in message and Path(_m()).name in message:
        return "create one with [cmd]hlock init[/], or point at one with [cmd]-m PATH[/]"
    if "cannot read" in message and Path(_l()).name in message:
        return "write one with [cmd]hlock resolve[/]"
    if "no tasks loaded" in message or "no viable stack" in message:
        return f"check that [key]\\[evals] packs[/] in [path]{_md()}[/] point at a directory with a pack.toml"
    if "eval pack" in message:
        return f"check the [key]\\[evals] packs[/] paths in [path]{_md()}[/]"
    if "unknown harness" in message:
        return "list the known harnesses with [cmd]hlock harness list[/]"
    if "several harnesses are installed" in message or "no supported harness" in message:
        return f"pass [cmd]--harness <name>[/] or set [key]\\[target] harness[/] in [path]{_md()}[/]"
    if "resolved lock with evidence" in message:
        return "use [cmd]hlock pin -o pinned.lock[/] to keep both"
    if "python worker" in message:
        return "set [key]HL_PYTHON[/] to an interpreter that has hlock installed"
    return None


@contextmanager
def _friendly_errors():
    """Turn binding errors (ValueError from the Rust core) into a clean CLI error."""
    try:
        yield
    except ValueError as e:
        msg = str(e).strip()
        _fail(escape(msg), _hint_for(msg))  # core messages name tables like [deciders.stop]


@contextmanager
def _working(message: str):
    with _friendly_errors(), console.status(f"[brand]{message}[/]", spinner="dots"):
        yield


def _next(*steps: str) -> None:
    console.print(Text.from_markup("[muted]next →[/] " + "[muted]  ·  [/]".join(f"[cmd]{s}[/]" for s in steps)))


def _ok(message: str, soft_wrap: bool = False) -> None:
    console.print(f"[ok]✓[/] {message}", soft_wrap=soft_wrap)


def _pct(v) -> str:
    return "—" if v is None else f"{float(v):.0%}"


def _usd(v) -> str:
    return "—" if v is None else f"${float(v):.4f}"


def _secs(v) -> str:
    return "—" if v is None else f"{float(v):.2f}s"


def _signed_pct(v) -> Text:
    v = float(v or 0.0)
    style = "ok" if v > 0 else ("fail" if v < 0 else "muted")
    return Text(f"{v:+.0%}", style=style)


def _kv_table() -> Table:
    t = Table.grid(padding=(0, 2))
    t.add_column(style="key", no_wrap=True)
    t.add_column()
    return t


def _lock_panel(lf, title: str | None = None) -> Panel:
    title = title or _ld()
    ev = json.loads(lf.evidence)
    stack = _kv_table()
    version = f" [muted]{lf.harness_version}[/]" if lf.harness_version else ""
    stack.add_row("harness", f"[bold]{lf.harness}[/]{version}")
    for role, spec in json.loads(lf.llm):
        stack.add_row(f"llm · {role}", f"[bold]{spec}[/]")
    stack.add_row("decision", lf.decision)
    for d in json.loads(lf.to_json()).get("decider") or []:
        stack.add_row(f"decider · {d['use']}", _decider_cell(d))
    stack.add_row("backend", lf.provenance)

    evidence = _kv_table()
    sr = (ev.get("success_rate") or {}).get("mean")
    evidence.add_row("success rate", Text(_pct(sr), style="ok" if (sr or 0) >= 0.5 else "warn"))
    evidence.add_row("cost / task", _usd((ev.get("cost_usd_per_task") or {}).get("mean")))
    lat = ev.get("latency_s_per_task") or {}
    evidence.add_row("latency", f"p50 {_secs(lat.get('p50'))}  ·  p95 {_secs(lat.get('p95'))}")
    evidence.add_row("trials", str(ev.get("trials", "—")))
    judge = ev.get("judge")
    if judge:
        judge = judge if isinstance(judge, dict) else {"model": judge}
        extra = []
        if judge.get("answered") and judge["answered"] not in judge["model"]:
            extra.append(f"answered by {judge['answered']}")
        if judge.get("uncertain_grades") is not None:
            extra.append(f"{_pct(judge['uncertain_grades'])} uncertain")
        evidence.add_row("judge", f"{escape(judge['model'])}" + (f"  [muted]{' · '.join(extra)}[/]" if extra else ""))

    digest = (lf.manifest_hash or "—").split(":", 1)[-1][:12]
    return Panel(
        Group(
            Text("Stack", style="bold"), stack, Text(""),
            Text("Evidence", style="bold"), evidence, Text(""),
            Text(f"lock-version {LOCK_VERSION} · manifest {digest}", style="muted"),
        ),
        title=f"[brand]🔒 {title}[/]",
        title_align="left",
        border_style="magenta",
        expand=False,
        padding=(1, 2),
    )


def _decider_cell(d: dict) -> str:
    """`ollama:kev:4b  hook: PermissionRequest · may deny, ask`, for the lock panels."""
    where = ", ".join(w["event"] for w in d.get("wiring") or [])
    how = f"{d['via']}: {where}" if where else d["via"]
    may = f" · may {', '.join(d['may'])}" if d.get("may") else ""
    return f"[bold]{escape(d['provider'])}:{escape(d['model'])}[/]  [muted]{escape(how)}{may}[/]"


def _count_by(items, key) -> str:
    counts: dict[str, int] = {}
    for it in items:
        counts[it.get(key) or "?"] = counts.get(it.get(key) or "?", 0) + 1
    return ", ".join(f"{n} {k}" for k, n in sorted(counts.items(), key=lambda kv: -kv[1]))


def _pin_panel(lock: dict, title: str | None = None) -> Panel:
    title = title or _ld()
    """The summary panel for a pinned lock (no evidence; components instead)."""
    h = lock.get("harness") or {}
    t = _kv_table()
    t.add_row("harness", f"[bold]{h.get('name')}[/] {h.get('version') or ''}  [muted]{h.get('source') or ''}[/]")
    for llm in lock.get("llm") or []:
        where = f"  [muted]via {llm['runtime']}[/]" if llm.get("runtime") else (
            f"  [muted]{llm['endpoint']}[/]" if llm.get("endpoint") else "")
        t.add_row(f"llm · {llm['role']}", f"[bold]{llm['provider']}:{llm['model']}[/]{where}")
    for rt in lock.get("runtime") or []:
        models = rt.get("model") or []
        present = sum(1 for m in models if m.get("present"))
        loc = "local" if rt.get("location") == "local" else "network"
        detail = f"{len(models)} model{'s' if len(models) != 1 else ''}" + (f", {present} on disk" if loc == "local" else "")
        t.add_row("runtime", f"[bold]{rt['name']}[/] {rt.get('version') or ''}  [muted]({loc}, {detail})[/]")
    tools = lock.get("tool") or []
    if tools:
        t.add_row("mcp", ", ".join(f"{x['name']} [muted]({x.get('transport', '?')})[/]" for x in tools))
    skills = lock.get("skill") or []
    if skills:
        t.add_row("skills", f"{len(skills)}  [muted]({_count_by(skills, 'origin')})[/]")
    natives = lock.get("native") or []
    if natives:
        kinds = _count_by([{"k": n["kind"].split(".")[-1]} for n in natives], "k")
        t.add_row("plugins", f"{len(natives)}  [muted]({kinds})[/]")
    ins = lock.get("instructions") or []
    if ins:
        t.add_row("instructions", ", ".join(i["name"] for i in ins))
    cfg = lock.get("config") or []
    if cfg:
        t.add_row("config", ", ".join(c["path"] + (f" [muted]{','.join(c['pointers'])}[/]" if c.get("pointers") else "")
                                      for c in cfg))
    secrets = lock.get("secret") or []
    if secrets:
        t.add_row("secrets", f"{len(secrets)} redacted  [muted](supplied as HLOCK_SECRET_* when syncing)[/]")
    for d in lock.get("decider") or []:
        t.add_row(f"decider · {d['use']}", _decider_cell(d))
    pulled = lock.get("pulled") or {}
    if pulled.get("from"):
        t.add_row("pulled", f"{escape(pulled['from'])} [muted]@ {pulled.get('commit', '')[:7]}[/]")
    return Panel(t, title=f"[brand]📌 {title} · pinned[/]", title_align="left", border_style="magenta",
                 expand=False, padding=(1, 2))


def _print_lock(lf, title: str | None = None) -> None:
    if lf.kind == "pin":
        console.print(_pin_panel(json.loads(lf.to_json()), title))
    else:
        console.print(_lock_panel(lf, title))


_GLOBAL_OPTS = ("-m", "--manifest", "--lock")
_PASSTHROUGH = ("launch",)  # arguments after these commands belong to the harness


def _hoist_global_options(args: list[str]) -> list[str]:
    """Let `-m/--manifest` and `--lock` appear after the subcommand too
    (`hlock resolve -m agent/harness.toml`) by moving them in front of it."""
    sub = next((i for i, a in enumerate(args) if not a.startswith("-") and (i == 0 or args[i - 1] not in _GLOBAL_OPTS)), None)
    if sub is None or args[sub] in _PASSTHROUGH or args[sub:sub + 2] == ["meta", "launch"]:
        return args
    front, rest, i = args[:sub], [], sub
    while i < len(args):
        a = args[i]
        if a == "--":
            rest.extend(args[i:])
            break
        if a in _GLOBAL_OPTS and i + 1 < len(args):
            front += [a, args[i + 1]]
            i += 2
            continue
        if a.startswith(("--manifest=", "--lock=")):
            front.append(a)
        else:
            rest.append(a)
        i += 1
    return front + rest


class _Group(click.RichGroup):
    def parse_args(self, ctx, args):
        return super().parse_args(ctx, _hoist_global_options(list(args)))


@click.group(cls=_Group, context_settings={"help_option_names": ["-h", "--help"]})
@click.version_option(__version__, "-V", "--version", prog_name="hlock")
@click.option("-m", "--manifest", envvar="HLOCK_MANIFEST", default=None, metavar="PATH",
              help="The harness.toml to use.  [dim]\\[default: the nearest one at or above this directory][/]")
@click.option("--lock", "lock_path", envvar="HLOCK_LOCKFILE", default=None, metavar="PATH",
              help="The lockfile to use.  [dim]\\[default: harness.lock next to the manifest][/]")
def main(manifest: str | None, lock_path: str | None) -> None:
    """[brand]hlock[/] · make your agent harness pinnable, portable, and tunable.

    Pin your harness, models, MCP servers, skills, and config in a
    [path]harness.lock[/], version it, and rehydrate it anywhere. Then tune it
    against your own evals.

    Get started: [cmd]hlock pin[/] → [cmd]hlock check[/] → [cmd]hlock sync[/]
    """
    _restore_meta_env()
    # the hooks `sync` writes for deciders run this hlock, however it was installed
    if "HLOCK_BIN" not in os.environ:
        found = shutil.which("hlock")
        if found:
            os.environ["HLOCK_BIN"] = found
    _set_project(manifest, lock_path)


def _restore_meta_env() -> None:
    """Inside a meta harness session, undo its config isolation for this process and
    everything it starts, so hlock acts on the user's real setup (see `hlock meta`)."""
    marker = os.environ.pop("HLOCK_META_RESTORE", None)
    if not marker:
        return
    try:
        changed = json.loads(marker)
    except ValueError:
        return
    for key, value in changed.items():
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = value


@main.command()
@click.option("--name", default="demo", show_default=True, help="Project name for the manifest.")
@click.option("--pack", default="./evals", show_default=True, help="Path to an eval pack directory.")
@click.option("--harness", "harness", default=None,
              help="Target harness for \\[target]. [dim]\\[default: the one you're running in, if any][/]")
@click.option("--force", is_flag=True, help="Overwrite an existing harness.toml.")
def init(name: str, pack: str, harness: str | None, force: bool) -> None:
    """Write a starter [path]harness.toml[/] manifest."""
    p = Path(_m())
    if p.exists() and not force:
        _fail(f"{_md()} already exists", "pass [cmd]--force[/] to overwrite it")
    target = None
    if harness:
        with _friendly_errors():
            target = json.loads(_select_harness(harness, manifest=_m()))
    else:
        # Only an unambiguous signal (running inside a harness) picks one silently.
        try:
            sel = json.loads(_select_harness(manifest=_m()))
            target = sel if sel["reason"] == "running inside it" else None
        except ValueError:
            target = None
    p.write_text(_default_manifest(name, pack, target["id"] if target else None))
    _ok(f"wrote [path]{_show(p)}[/]  [muted](project {name!r}, eval pack {pack})[/]")
    if target:
        console.print(f"[key]target[/] [bold]{target['id']}[/]  [muted]({target['reason']})[/]")
    if not (Path(pack) / "pack.toml").exists():
        console.print(f"[warn]![/] no [path]pack.toml[/] found in [path]{pack}[/]; add one before resolving")
        _next(f"hlock evals init {pack}", "hlock resolve")
        return
    _next("hlock resolve")


@main.group()
def evals() -> None:
    """Write and check [bold]eval packs[/], the tasks every candidate stack is measured on."""


@evals.command("init")
@click.argument("directory", default="./evals")
@click.option("-t", "--template", default="hello-world", show_default=True,
              type=click.Choice(list(_evals.TEMPLATES)), help="What to write. See [cmd]hlock evals templates[/].")
@click.option("--name", default=None, help="Pack name. [dim]\\[default: <project>/<directory>][/]")
@click.option("--tasks", "n_tasks", default=10, show_default=True, type=click.IntRange(1, 200),
              help="How many task slots a template writes. Ignored by hello-world.")
@click.option("--no-skill", is_flag=True, help="Don't write the Agent Skill that guides your agent through a template.")
@click.option("--skill-dir", default=f".agents/skills/{_evals.SKILL_NAME}", show_default=True,
              help="Where the Agent Skill's SKILL.md goes.")
@click.option("--force", is_flag=True, help="Overwrite an existing pack.toml.")
@click.option("--from-traces", "traces", is_flag=False, flag_value="", default=None, metavar="[PATH]",
              help="Draft the tasks from agent traces: the sessions Claude Code, Codex and Pi logged for "
                   "this project, or a file or directory of JSONL/CSV traces.")
def evals_init(directory: str, template: str, name: str | None, n_tasks: int, no_skill: bool, skill_dir: str,
               force: bool, traces: str | None) -> None:
    """Write a starter eval pack to [path]DIRECTORY[/].

    The default [bold]hello-world[/] pack is complete and runs as is. The other templates
    write task slots marked [key]TODO(hlock)[/], plus an Agent Skill your agent uses to
    fill them with real tasks from this project. [cmd]--from-traces[/] drafts the tasks from
    the requests you've made to your agent instead.
    """
    if traces is not None:
        _evals_from_traces(directory, traces or None, name, n_tasks, no_skill, skill_dir, force)
        return
    with _friendly_errors():
        written = _evals.init(directory, template, name=name, n_tasks=n_tasks, force=force)
    if template == "hello-world":
        _ok(f"wrote [path]{_show(written[0])}[/]  [muted](hello-world, {len(_evals.HELLO_TASKS)} tasks)[/]")
    else:
        _ok(f"wrote [path]{_show(written[0])}[/]  [muted]({template} template, {n_tasks} task slots)[/]")
    console.print(f"  [muted]{len(written)} files under {_show(directory)}[/]")
    if template == "hello-world":
        _next(f"hlock evals check {directory}", f"hlock init --pack {directory}" if not Path(_m()).exists() else "hlock resolve")
        return
    if not no_skill:
        path = Path(skill_dir) / "SKILL.md"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(_evals.skill(_evals.TEMPLATES[template], directory))
        _ok(f"wrote [path]{_show(path)}[/]  [muted](Agent Skill {_evals.SKILL_NAME})[/]")
        console.print(f"[muted]open your agent here and ask it to fill in {directory} "
                      f"with the {_evals.SKILL_NAME} skill[/]")
    _next(f"hlock evals check {directory}")


def _evals_from_traces(directory: str, source: str | None, name: str | None, n_tasks: int, no_skill: bool,
                       skill_dir: str, force: bool) -> None:
    project = Path(_m()).parent
    with _friendly_errors():
        sessions = _traces.load(source, project)
        if not sessions:
            where = source or f"agent sessions for {_show(project)} (Claude Code, Codex, Pi)"
            _fail(f"no requests found in {where}",
                  "pass [cmd]--from-traces PATH[/] with session logs or JSONL/CSV records that have a prompt")
        decider = Manifest.load(_m()).decider("draft") if Path(_m()).exists() else None
        decide = None
        if decider:
            console.print(f"[muted]reviewing the requests with {escape(decider)} ([key]\\[deciders.draft][/] or "
                          "[key]\\[deciders.default][/]); a hosted model sees the redacted prompts[/]")
            def decide(state, questions):
                return _decide(state, questions, model=decider, use_="draft", manifest=_m())
        try:
            written, n = _evals.draft_from_traces(directory, sessions, name=name, n_tasks=n_tasks, project=project,
                                                  force=force, decide=decide)
        except OSError as e:
            _fail(f"cannot write {directory}: {e.strerror}")
    counts: dict[str, int] = {}
    for s in sessions:
        counts[s.harness] = counts.get(s.harness, 0) + 1
    origin = ", ".join(f"{v} {k} {'record' if k == 'traces' else 'session'}{'s' * (v != 1)}"
                       for k, v in sorted(counts.items()))
    _ok(f"wrote [path]{_show(written[0])}[/]  [muted]({n} draft task{'s' * (n != 1)} from {origin})[/]")
    todo = sum(1 for p in written if p.name == "pack.toml" for line in p.read_text().splitlines()
               if line.startswith(f"# {_evals.PLACEHOLDER}"))
    if todo:
        console.print(f"  [muted]{todo} {'tasks need' if todo != 1 else 'task needs'} an expected artifact; "
                      "prompts are redacted drafts to review[/]")
    if not no_skill:
        path = Path(skill_dir) / "SKILL.md"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(_evals.trace_skill(directory))
        _ok(f"wrote [path]{_show(path)}[/]  [muted](Agent Skill {_evals.SKILL_NAME})[/]")
        console.print(f"[muted]open your agent here and ask it to review {directory} with the {_evals.SKILL_NAME} skill[/]")
    _next(f"hlock evals check {directory}")


@evals.command("templates")
@click.option("--json", "as_json", is_flag=True, help="Print the templates as JSON.")
def evals_templates(as_json: bool) -> None:
    """List the eval pack templates [cmd]hlock evals init[/] can write."""
    rows = _evals.templates()
    if as_json:
        console.print_json(data=[{"name": t.name, "domain": t.domain, "summary": t.summary,
                                  "kinds": [s.kind for s in t.slots]} for t in rows])
        return
    t = Table(box=None, header_style="bold", pad_edge=False)
    t.add_column("template", style="key")
    t.add_column("task kinds")
    t.add_column("", style="muted")
    for tpl in rows:
        kinds = ", ".join(s.kind for s in tpl.slots) or "[muted]complete pack[/]"
        t.add_row(tpl.name, kinds, tpl.summary)
    console.print(t)
    _next("hlock evals init -t <template>")


@evals.command("check")
@click.argument("directory", default="./evals")
@click.option("--run", is_flag=True,
              help="Also run each task's graders, judges aside, on its fixtures alone (they should fail) and on "
                   "its solution (they should pass). Runs the pack's test commands.")
@click.option("--judges", is_flag=True,
              help="With --run, also run LLM judges, which cost more and can't be compared run to run. "
                   "Decision-model judges always run with --run.")
@click.option("--json", "as_json", is_flag=True, help="Print the result as JSON.")
def evals_check(directory: str, run: bool, judges: bool, as_json: bool) -> None:
    """Check that an eval pack is ready: required fields, prompts, fixtures, graders, and
    no [key]TODO(hlock)[/] slots left. Exits 1 on errors; warnings don't fail."""
    manifest = _m() if Path(_m()).exists() else None
    with _friendly_errors():
        result = json.loads(_check_eval_pack(directory, run=run, manifest=manifest, judges=judges))
    issues = result["issues"]
    errors = [i for i in issues if i["level"] == "error"]
    if as_json:
        console.print_json(data=result)
        sys.exit(1 if errors else 0)
    judge = result.get("judge")
    judged = f" · judge {escape(judge['model'])}" if judge and run else ""
    head = f"[bold]{result['name'] or '(unnamed)'}[/] {result['version']}  [muted]{len(result['tasks'])} tasks · {_show(directory)}{judged}[/]"
    console.print(head)
    for i in issues:
        mark = "[fail]✗[/]" if i["level"] == "error" else "[warn]![/]"
        where = f"[key]{i['task']}[/]  " if i["task"] else ""
        console.print(f"  {mark} {where}{escape(i['message'])}", soft_wrap=True)
    warns = len(issues) - len(errors)
    plural = lambda n, word: f"{n} {word}{'' if n == 1 else 's'}"
    if errors:
        err_console.print(f"[fail]✗[/] {plural(len(errors), 'error')}, {plural(warns, 'warning')}")
        sys.exit(1)
    _ok(f"ready  [muted]({plural(warns, 'warning')})[/]" if warns else "ready")
    if not run:
        _next(f"hlock evals check {directory} --run", "hlock resolve" if manifest else f"hlock init --pack {directory}")
    else:
        _next("hlock resolve" if manifest else f"hlock init --pack {directory}")


@evals.command("grade")
@click.argument("task")
@click.argument("workspace", required=False)
@click.option("--pack", "pack", default="./evals", show_default=True, help="The eval pack directory.")
@click.option("--json", "as_json", is_flag=True, help="Print the result as JSON.")
def evals_grade(task: str, workspace: str | None, pack: str, as_json: bool) -> None:
    """Grade a directory as if a run of TASK left it, with every grader the task has,
    judges included. With no WORKSPACE, grades the task's solution. Exits 1 if a grader
    fails."""
    manifest = _m() if Path(_m()).exists() else None
    with _working(f"Grading {task}…"):
        result = json.loads(_grade_eval_task(pack, task, workspace, manifest=manifest))
    if as_json:
        console.print_json(data=result)
        sys.exit(0 if result["success"] else 1)
    what = _show(workspace) if workspace else "its solution"
    console.print(f"[bold]{escape(task)}[/]  [muted]{escape(what)} · score {result['score']:.2f}[/]")
    for g in result["grades"]:
        mark = "[ok]✓[/]" if g["success"] else "[fail]✗[/]"
        unsure = "  [warn]uncertain[/]" if g.get("uncertain") else ""
        console.print(f"  {mark} {escape(g['grader'])}  [muted]{g['score']:.2f}[/]{unsure}", soft_wrap=True)
        for part in g.get("parts") or []:
            value = f"{part['value']:.2f}" if not part["label"].startswith("level") else f"{part['value']:.1f}"
            console.print(f"      [muted]{value}[/]  {escape(part['label'])}", soft_wrap=True)
        for line in g["detail"].splitlines():
            console.print(f"      [muted]{escape(line)}[/]", soft_wrap=True)
    if not result["success"]:
        failed = sum(not g["success"] for g in result["grades"])
        err_console.print(f"[fail]✗[/] {failed} of {len(result['grades'])} graders failed")
        sys.exit(1)
    _ok("passed")


@evals.command("calibrate")
@click.argument("directory", default="./evals")
@click.option("--against", required=True, metavar="MODEL", help="The judge to compare with, as provider:model (an LLM, usually).")
@click.option("--against-base-url", default=None, metavar="URL",
              help="An OpenAI-compatible endpoint for --against, for a local LLM (Ollama's is http://localhost:11434/v1).")
@click.option("--model", default=None, metavar="MODEL", help="The judge to calibrate.  [dim]\\[default: the pack's judge][/]")
@click.option("--json", "as_json", is_flag=True, help="Print the comparison as JSON.")
def evals_calibrate(directory: str, against: str, against_base_url: str | None, model: str | None, as_json: bool) -> None:
    """Judge each task's solution, and its run with no work done, with two judges, and
    list the criteria they disagree on: the ones to reword, or give [key]true[/] and
    [key]false[/] descriptions."""
    manifest = _m() if Path(_m()).exists() else None
    with _working("Judging with two judges…"):
        result = json.loads(_calibrate_eval_pack(directory, against, model=model, manifest=manifest,
                                                 against_base_url=against_base_url))
    if as_json:
        console.print_json(data=result)
        return
    rows = result["rows"]
    console.print(f"[bold]{escape(result['a'])}[/] vs [bold]{escape(result['b'])}[/]  [muted]{len(rows)} comparisons[/]")
    if not rows:
        console.print("[muted]no judge graders to compare[/]")
        return
    t = Table(box=None, pad_edge=False, show_header=True, header_style="muted")
    for col in ("task", "run", "criterion", result["a"], result["b"], ""):
        t.add_column(escape(col))
    for r in rows:
        if r["agree"]:
            continue
        crit = r["criterion"] if not r.get("note") else f"{r['criterion']}: {r['note']}"
        t.add_row(escape(r["task"]), r["run"], escape(crit), f"{r['a']:.2f}", f"{r['b']:.2f}", "[warn]differ[/]")
    if t.row_count:
        console.print(t)
    _ok(f"they agree on {_pct(result['agreement'])}")


@main.group()
def decide() -> None:
    """Ask decision models typed questions, and answer harness hooks with them."""


def _read_state(state: str) -> str:
    if state == "-":
        return sys.stdin.read()
    if state.startswith("@"):
        return Path(state[1:]).read_text()
    return state


@decide.command("ask")
@click.argument("state")
@click.option("--noul", "nouls", multiple=True, metavar="TEXT", help="A yes/no question. Repeat for more.")
@click.option("--choice", "choices", multiple=True, metavar="Q=A,B,…", help="Pick one of the options after `=`.")
@click.option("--score", "scores", multiple=True, metavar="Q=L0|L1|…", help="Place it on levels, worst first, after `=`.")
@click.option("--model", default=None, metavar="MODEL", help="provider:model.  [dim]\\[default: \\[deciders.default], else one hlock finds][/]")
@click.option("--use", "use_", default=None, help="Use [key]\\[deciders.<use>][/]'s model and settings.")
@click.option("--json", "as_json", is_flag=True, help="Print the answers as JSON.")
def decide_ask(state: str, nouls, choices, scores, model: str | None, use_: str | None, as_json: bool) -> None:
    """Ask a decision model about STATE: text, [key]@file[/], or [key]-[/] for stdin.

    \b
      hlock decide ask @ticket.txt --noul "Is it urgent?" --choice "team=billing,auth,infra"
    """
    questions = [{"id": f"q{i + 1}", "type": "noul", "instructions": q} for i, q in enumerate(nouls)]
    for spec in choices:
        q, _, opts = spec.partition("=")
        options = [o.strip() for o in opts.split(",") if o.strip()]
        if not options:
            _fail(f"--choice {spec!r} needs options after `=`", "e.g. --choice \"team=billing,auth\"")
        questions.append({"id": f"q{len(questions) + 1}", "type": "choice", "instructions": q, "criteria": {o: o for o in options}})
    for spec in scores:
        q, _, levels = spec.partition("=")
        levels = [lv.strip() for lv in levels.split("|") if lv.strip()]
        if len(levels) < 2:
            _fail(f"--score {spec!r} needs at least two levels after `=`", "e.g. --score \"severity=cosmetic|broken|outage\"")
        questions.append({"id": f"q{len(questions) + 1}", "type": "score", "instructions": q, "criteria": levels})
    if not questions:
        _fail("ask at least one question", "--noul, --choice, or --score")
    manifest = _m() if Path(_m()).exists() else None
    with _working("Asking…"):
        result = json.loads(_decide(_read_state(state), json.dumps(questions), model=model, use_=use_, manifest=manifest))
    if as_json:
        console.print_json(data=result)
        return
    d = result["decided"]
    cost = "" if d.get("cost_usd") is None else " · no cost (local)" if d["cost_usd"] == 0 else f" · ${d['cost_usd']:.6f}"
    console.print(f"[bold]{escape(result['provider'])}:{escape(d['model'])}[/]  [muted]{d['input_tokens']} input tokens{cost}[/]")
    for q, a in zip(questions, d["answers"]):
        if a["type"] == "noul":
            ans = "yes" if a["value"] >= 0.5 else "no"
            detail = f"P(yes) {a['value']:.2f}"
        elif a["type"] == "choice":
            ans = str(a["value"])
            detail = f"P {a['probabilities'].get(ans, 0):.2f}" + (f" · confidence {a['confidence']:.2f}" if a.get("confidence") is not None else "")
        else:
            levels = q["criteria"]
            ans = levels[min(len(levels) - 1, max(0, round(a["value"])))]
            detail = f"level {a['value']:.2f} of {len(levels) - 1}" + (f" · confidence {a['confidence']:.2f}" if a.get("confidence") is not None else "")
        console.print(f"  {escape(q['instructions'])}  [bold]{escape(ans)}[/]  [muted]{detail}[/]", soft_wrap=True)


@decide.command("hook")
@click.argument("use_", metavar="USE", type=click.Choice(["approve", "stop"]))
@click.option("--format", "fmt", required=True, type=click.Choice(["claude-code", "codex"]), help="The harness's hook format.")
@click.option("--verbose", "-v", is_flag=True, help="Say what happened on stderr.")
def decide_hook(use_: str, fmt: str, verbose: bool) -> None:
    """Answer one harness hook event on stdin with the lock's decider for USE (the lock
    is the global [cmd]--lock[/], which [cmd]hlock sync[/] writes into the hook). It
    defers, printing nothing, when it's unsure, slow, or can't reach the model."""
    out = json.loads(_decide_hook(use_, fmt, sys.stdin.read(), _l()))
    if verbose or os.environ.get("HLOCK_DECIDE_VERBOSE"):
        print(f"hlock decide hook {use_}: {out['note']}", file=sys.stderr)
    if out["stdout"]:
        print(out["stdout"])
    sys.exit(out["exit"])


@main.command()
@click.option("--backend", default=None, type=click.Choice(["local", "devbox", "cluster"]),
              help="Override the manifest's \\[backend] target.")
@click.option("--harness", "harness", default=None,
              help="Target harness, overriding \\[target] harness (e.g. claude, codex, pi, opencode).")
@click.option("--workdir", default=None, help="Where to put trial artifacts.  [dim]\\[default: .harnesslock][/]")
def resolve(backend: str | None, harness: str | None, workdir: str | None) -> None:
    """Search the space and pin the winner in [path]harness.lock[/]."""
    with _working(f"Resolving {_md()} on the {backend or 'configured'} backend…"):
        lf = _resolve(_m(), backend=backend, workdir=workdir, harness=harness, lock=_l())
    _ok(f"wrote [path]{_ld()}[/]")
    console.print(_lock_panel(lf))
    lock = json.loads(lf.to_json())
    adapter = (lock.get("harness") or {}).get("adapter")
    if adapter and adapter != lf.harness:
        console.print(
            f"[warn]![/] trials ran on the [bold]{adapter}[/] adapter; native {lf.harness} execution "
            "isn't built yet, so the evidence is a stand-in"
        )
    if harness:
        with _friendly_errors():
            m = Manifest.load(_m())
        if m.target_harness is None or json.loads(_select_harness(m.target_harness, manifest=_m()))["id"] != lf.harness:
            console.print(
                f"[warn]![/] --harness isn't recorded in [path]{_md()}[/]; add "
                f'[key]\\[target] harness = "{lf.harness}"[/] so [cmd]hlock check[/] can enforce it'
            )
    _next("hlock check", 'hlock run "<prompt>"')


@main.command()
@click.option("--harness", "harness", default=None,
              help="Harness to pin. [dim]\\[default: \\[target], else the one you're running in][/]")
@click.option("-o", "--out", default=None, help="Where to write the pinned lock.  [dim]\\[default: the project's lock][/]")
@click.option("--no-vendor", is_flag=True, help="Don't copy unfetchable skills and config into .hlock/vendor.")
@click.option("--force", is_flag=True, help="Replace a resolved lock (one with evidence).")
@click.option("--json", "as_json", is_flag=True, help="Print the result as JSON.")
def pin(harness: str | None, out: str | None, no_vendor: bool, force: bool, as_json: bool) -> None:
    """Pin your current harness setup into [path]harness.lock[/].

    Captures the harness version and install source, the default and small models,
    local inference runtimes and their models, MCP servers, skills, plugins,
    instruction files, and config. Secrets are replaced with
    [key]HLOCK_SECRET_*[/] placeholders; content another machine can't fetch is
    vendored into [path].hlock/vendor/[/]. No [path]harness.toml[/] needed.
    """
    with _working("Pinning your harness setup…"):
        out = out or _l()
        result = json.loads(_pin(harness=harness, out=out, vendor=not no_vendor, force=force, manifest=_m()))
    if as_json:
        console.print_json(data=result)
        return
    lock, sel = result["lock"], result["selection"]
    h = lock["harness"]
    _ok(f"pinned [bold]{h['name']}[/] {h.get('version') or ''} → [path]{_show(out)}[/]  [muted]({sel['reason']})[/]")
    for w in result["warnings"]:
        console.print(f"[warn]![/] {w}")
    console.print(_pin_panel(lock, _show(out)))
    if result["vendored"]:
        console.print(f"[muted]vendored {len(result['vendored'])} files into[/] [path]{_vendor_dir(result['vendored'], h['name'])}[/]  "
                      "[muted]· review before committing[/]")
    if result["previous"]:
        if result["diff"]:
            console.print("[bold]changes since the last pin[/]")
            for line in result["diff"]:
                style = {"+": "ok", "-": "fail", "~": "warn"}.get(line[:1], "muted")
                console.print(f"  [{style}]{line}[/]")
        else:
            console.print("[muted]no changes since the last pin[/]")
    _next("hlock check", "hlock sync")


def _vendor_dir(vendored: list[str], harness: str) -> str:
    """The directory a pin vendored into: `.hlock/vendor/[<lock name>/]<harness>/`."""
    parts = Path(vendored[0]).parts
    cut = parts.index(harness) + 1 if harness in parts else len(parts) - 1
    return "/".join(parts[:cut]) + "/"


def _short(commit: str | None) -> str:
    return (commit or "")[:7]


def _mcp_commands(lock: dict) -> list[str]:
    """The executables a lock's stdio MCP servers start (they run when the harness does)."""
    seen: list[str] = []
    for t in lock.get("tool") or []:
        cmd = t.get("command")
        if t.get("kind", "mcp") == "mcp" and cmd and cmd not in seen:
            seen.append(cmd)
    return seen


def _pull_label(result: dict) -> str:
    """How to refer to a pull's lock: its store name, or its path in the project."""
    return result["name"] if result.get("stored") else _show(result["path"])


def _print_pull(result: dict) -> None:
    lock, label = result["lock"], _pull_label(result)
    src = result["source"]["spec"]
    prev = result.get("previous_commit")
    if prev and prev == result["commit"] and not result["diff"]:
        _ok(f"[path]{label}[/] is up to date with {escape(src)} [muted]@ {_short(prev)}[/]", soft_wrap=True)
        return
    if prev:
        _ok(f"updated [path]{label}[/] from {escape(src)}  [muted]{_short(prev)} → {_short(result['commit'])}[/]", soft_wrap=True)
    else:
        _ok(f"pulled {escape(src)} [muted]@ {_short(result['commit'])}[/] → [path]{label}[/]", soft_wrap=True)
    for w in result["warnings"]:
        console.print(f"[warn]![/] {w}")
    if lock.get("kind") == "pin":
        console.print(_pin_panel(lock, label))
    else:
        console.print(_lock_panel(Lockfile.load(result["path"]), label))
    if not result.get("stored") and result["vendored"]:
        n = len(result["vendored"])
        console.print(f"[muted]copied {n} vendored {'entry' if n == 1 else 'entries'} into[/] "
                      f"[path]{_vendor_dir(result['vendored'], lock['harness']['name'])}[/]")
    if prev and result["diff"]:
        console.print("[bold]changes[/]")
        for line in result["diff"]:
            style = {"+": "ok", "-": "fail", "~": "warn"}.get(line[:1], "muted")
            console.print(f"  [{style}]{line}[/]")
    cmds = _mcp_commands(lock)
    if cmds:
        console.print(f"[warn]![/] starts MCP servers with: [cmd]{', '.join(cmds)}[/]  "
                      "[muted]· review before syncing into your own config[/]")
    for other in result["others"]:
        console.print(f"[muted]also in the repo:[/] {other['path']}  [muted]→[/] "
                      f"[cmd]hlock pull {escape(other['source'])}[/]", soft_wrap=True)


def _lock_ref(arg: str, as_json: bool = False) -> tuple[str, str | None]:
    """A lock argument → (lock path, store name). An existing file is a path; a source
    (`github:…`) is pulled into the store first; otherwise a pulled lock's name."""
    if Path(arg).expanduser().is_file():
        return arg, None
    if is_pull_source(arg):
        with _working(f"Pulling {arg}…"):
            result = json.loads(_pull(arg))
        if not as_json:
            prev = result.get("previous_commit")
            change = f"{_short(prev)} → {_short(result['commit'])}" if prev and prev != result["commit"] else _short(result["commit"])
            _ok(f"pulled {escape(arg)} [muted]@ {change}[/] → [path]{result['name']}[/]", soft_wrap=True)
            cmds = _mcp_commands(result["lock"])
            if cmds:
                console.print(f"[warn]![/] starts MCP servers with: [cmd]{', '.join(cmds)}[/]")
        return result["path"], result["name"]
    if stored := _pull_find(arg):
        return stored, arg
    return arg, None


@main.command()
@click.argument("sources", nargs=-1, metavar="[SOURCE | NAME | LOCK]...")
@click.option("--as", "name", default=None, metavar="NAME",
              help="Name to store the lock under.  [dim]\\[default: the repo owner][/]")
@click.option("-o", "--out", default=None, metavar="FILE",
              help="Write the lock into the project as FILE (e.g. harness.alice.lock) instead of the store.")
@click.option("--force", is_flag=True, help="Replace an existing lock of that name that came from somewhere else.")
@click.option("--json", "as_json", is_flag=True, help="Print the result as JSON.")
def pull(sources: tuple[str, ...], name: str | None, out: str | None, force: bool, as_json: bool) -> None:
    """Pull someone else's [path]harness.lock[/] from a git repository.

    SOURCE is [cmd]github:OWNER/REPO\\[/PATH]\\[@REF][/], [cmd]gitlab:GROUP/REPO\\[//PATH]\\[@REF][/],
    [cmd]git:URL\\[//PATH]\\[@REF][/], or a repository or file URL copied from the browser. The lock
    and the files it vendors are stored under [path]~/.hlock/pulls/NAME/[/], so you can use it
    from any project by name: [cmd]hlock launch --lock NAME[/], [cmd]hlock sync NAME[/]. Nothing from
    the lock runs until you do.

    With no SOURCE, refreshes every pulled lock; with NAMEs or LOCK files, refreshes those.
    """
    remote = [s for s in sources if is_pull_source(s)]
    if remote and len(sources) > 1:
        _fail("pull one source at a time", "run [cmd]hlock pull[/] once per source")
    if name and out:
        _fail("--as and -o both name the lock", "use [cmd]--as NAME[/] for the store, or [cmd]-o FILE[/] for the project")
    if remote:
        with _working(f"Pulling {remote[0]}…"):
            result = json.loads(_pull(remote[0], out=out, name=name, force=force))
        if as_json:
            console.print_json(data=result)
            return
        _print_pull(result)
        label = _pull_label(result)
        _next(f"hlock launch --lock {label}", f"hlock sync {label} --in-place --plan")
        return

    if name or out:
        _fail("--as and -o need a SOURCE", "e.g. [cmd]hlock pull github:alice/agent-setup --as alice[/]")
    # (lock path, store name or None for a project file)
    targets: list[tuple[str, str | None]] = []
    if sources:
        for s in sources:
            path, stored = (s, None) if Path(s).is_file() else (_pull_find(s), s)
            if path is None:
                _fail(f"no pulled lock named {s!r}", "see what's pulled with [cmd]hlock list[/]")
            targets.append((path, stored))
    else:
        targets = [(e["path"], e["name"]) for e in json.loads(_pull_list()) if not e.get("error")]
        targets += [(str(p), None) for p in sorted(_Project.lock.parent.glob("harness.*.lock"))]
    jobs: list[tuple[str, str | None, str]] = []
    for path, stored in targets:
        with _friendly_errors():
            pulled = json.loads(Lockfile.load(path).to_json()).get("pulled") or {}
        if pulled.get("from"):
            jobs.append((path, stored, pulled["from"]))
        elif sources:
            _fail(f"{_show(path)} wasn't pulled, so there's nothing to refresh it from",
                  "pull a lock with [cmd]hlock pull github:OWNER/REPO[/]")
    if not jobs:
        _fail("no pulled locks to refresh", "pull one with [cmd]hlock pull github:OWNER/REPO[/]")
    results = []
    for path, stored, spec in jobs:
        with _working(f"Refreshing {stored or _show(path)} from {spec}…"):
            results.append(json.loads(_pull(spec, out=None if stored else path, name=stored, force=force)))
    if as_json:
        console.print_json(data=results)
        return
    for r in results:
        _print_pull(r)


@main.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print the pulled locks as JSON.")
def list_(as_json: bool) -> None:
    """List the locks you've pulled, from any project."""
    entries = json.loads(_pull_list())
    if as_json:
        console.print_json(data=entries)
        return
    if not entries:
        console.print("[muted]no pulled locks yet[/]")
        _next("hlock pull github:OWNER/REPO")
        return
    t = Table(box=None, header_style="bold", pad_edge=False)
    for col in ("name", "harness", "source", "commit", "pulled"):
        t.add_column(col)
    for e in entries:
        if e.get("error"):
            t.add_row(f"[key]{e['name']}[/]", f"[fail]{escape(e['error'])}[/]", "", "", "")
            continue
        p = e.get("pulled") or {}
        t.add_row(f"[key]{e['name']}[/]", f"{e['harness']} {e.get('version') or ''}".strip(),
                  escape(p.get("from", "—")), _short(p.get("commit")) or "—", _ago((p.get("at") or "").replace("Z", "+00:00") or None))
    console.print(t)
    _next(f"hlock launch --lock {entries[0]['name']}", "hlock pull [muted](refresh all)[/]")


@main.command()
@click.argument("names", nargs=-1, required=True, metavar="NAME...")
def rm(names: tuple[str, ...]) -> None:
    """Remove pulled locks (and the environments [cmd]launch[/] made for them)."""
    for n in names:
        with _friendly_errors():
            _pull_remove(n)
        _ok(f"removed [path]{n}[/]")


_STATUS = {"satisfied": "[ok]✓[/]", "auto": "[brand]●[/]", "manual": "[warn]○[/]"}
MANUAL_EXIT = 3


def _plan_table(plan: dict, verbose: bool = False) -> Table:
    t = Table(box=None, show_header=False, pad_edge=False)
    t.add_column(no_wrap=True)
    t.add_column(style="key", no_wrap=True)
    t.add_column(overflow="fold")
    by_kind: dict[tuple[str, str], list[dict]] = {}
    for st in plan["steps"]:
        if st.get("optional") and st["status"] != "satisfied" and not verbose:
            continue
        by_kind.setdefault((st["kind"], st["status"]), []).append(st)
    order = {"satisfied": 0, "auto": 1, "manual": 2}
    for (kind, status), steps in sorted(by_kind.items(), key=lambda kv: order[kv[0][1]]):
        if len(steps) > 6 and not verbose:
            verb = {"satisfied": "match the lock", "auto": "to restore", "manual": "need you"}[status]
            t.add_row(_STATUS[status], kind, f"{len(steps)} {kind}s {verb}")
            continue
        for st in steps:
            detail = st["title"]
            if st["status"] != "satisfied" and st.get("reason"):
                detail += f"  [muted]{st['reason']}[/]"
            t.add_row(_STATUS[st["status"]], kind, detail)
    return t


def _rel(path: str) -> str:
    """A path relative to the working directory when it's inside it."""
    try:
        return str(Path(path).resolve().relative_to(Path.cwd().resolve()))
    except ValueError:
        return path


def _tilde(path: str) -> str:
    """A path with the home directory shown as `~`."""
    try:
        return "~/" + str(Path(path).relative_to(Path.home()))
    except ValueError:
        return path


def _manual_steps(plan: dict) -> list[dict]:
    return [st for st in plan["steps"] if st["status"] == "manual" and not st.get("optional")]


@main.command()
@click.argument("lock", default=None, required=False)
@click.option("--plan", "plan_only", is_flag=True, help="Print the plan and change nothing.")
@click.option("--json", "as_json", is_flag=True, help="Print the plan as JSON and change nothing.")
@click.option("-y", "--yes", is_flag=True, help="Don't ask before running automatic steps.")
@click.option("--install", is_flag=True, help="Let hlock run package managers (npm, brew, uv, install scripts).")
@click.option("--pull", is_flag=True, help="Let hlock download model weights through installed runtimes.")
@click.option("--all-models", is_flag=True, help="Include models a provider lists but no locked role uses.")
@click.option("--prefix", default=None, help="Where isolated config roots live.  [dim]\\[default: .hlock/env][/]")
@click.option("--in-place", is_flag=True, help="Write into your own config (~/.claude, …) instead of an isolated root.")
@click.option("--docker", "docker", is_flag=True, help="Write a Docker build context and Dockerfile to .hlock/docker/<harness>.")
@click.option("--build", is_flag=True, help="With --docker: also run docker build.")
@click.option("--skill", "skill", is_flag=True, help="Write an Agent Skill that guides an agent through the remaining steps.")
@click.option("--skill-dir", default=".agents/skills/hlock-rehydrate", show_default=True, help="Where --skill writes SKILL.md.")
@click.option("--exact", is_flag=True, help="Also remove skills that aren't in the lock (moved aside, not deleted).")
@click.option("-v", "--verbose", is_flag=True, help="List every step, including optional ones.")
def sync(lock: str, plan_only: bool, as_json: bool, yes: bool, install: bool, pull: bool, all_models: bool,
         prefix: str | None, in_place: bool, docker: bool, build: bool, skill: bool, skill_dir: str, exact: bool,
         verbose: bool) -> None:
    """Rehydrate the setup in [path]harness.lock[/] on this machine.

    Builds a plan in which each step is [ok]✓ satisfied[/], [brand]● automatic[/] (hlock does it), or
    [warn]○ manual[/] (hlock prints what to run and why). Automatic steps restore config, skills
    and instruction files and install plugins through the harness's own CLI. Package
    installs need [cmd]--install[/] and model downloads need [cmd]--pull[/]. Exits 3 while manual
    steps remain, so it's safe to re-run until it's clean.

    The setup goes into an isolated config root in [path].hlock/env/[/] (the one [cmd]hlock launch[/]
    uses), so your own config isn't touched. Pass [cmd]--in-place[/] to write into it instead.
    """
    if in_place and (prefix or docker):
        _fail("--in-place doesn't work with --prefix or --docker", "pick one place to write the setup")
    lock, stored = _lock_ref(lock, as_json) if lock else (_l(), None)
    if stored and prefix is None and not in_place and not docker:
        prefix = _pull_env(stored)  # the same isolated root `hlock launch --lock NAME` uses
    docker_dir = None
    if docker:
        with _friendly_errors():
            harness_id = Lockfile.load(lock).harness
        docker_dir = str(Path(".hlock/docker") / harness_id)
    with _working("Planning…"):
        plan = json.loads(sync_plan(lock, prefix=prefix, in_place=in_place, docker=docker_dir, install=install, pull=pull,
                                    all_models=all_models, exact=exact, manifest=_m()))
    if as_json:
        console.print_json(data=plan)
        return
    where = {"home": f"your config [path]{_tilde(plan['root'])}[/]", "prefix": f"[path]{_tilde(_rel(plan['root']))}[/]",
             "docker": f"[path]{docker_dir}[/]"}[plan["mode"]]
    console.print(f"[bold]Sync plan[/] for {plan['harness_name']} {plan.get('version') or ''} → {where}")
    console.print(_plan_table(plan, verbose))

    if skill:
        path = Path(skill_dir) / "SKILL.md"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(sync_skill(json.dumps(plan), _show(lock)))
        _ok(f"wrote [path]{path}[/]")
        console.print("[muted]open your agent here and ask it to rehydrate the harness with the hlock-rehydrate skill[/]")
        return
    if plan_only:
        return

    auto = [st for st in plan["steps"] if st["status"] == "auto"]
    if docker:
        auto = [st for st in auto if st["kind"] in ("config", "skill", "instructions")]
    if auto and not yes and not docker:
        if not sys.stdin.isatty():
            _fail(f"{len(auto)} automatic steps to run", "pass [cmd]--yes[/] to run them, or [cmd]--plan[/] to just look")
        if not click.confirm(f"Run {len(auto)} automatic step{'s' if len(auto) != 1 else ''}?", default=True):
            sys.exit(1)
    sync_ignore_env(json.dumps(plan))  # an isolated root is restored from the lock: keep it out of git
    failed = 0
    groups: dict[str, list[dict]] = {}
    for st in auto:
        groups.setdefault(st["kind"], []).append(st)
    for kind, steps in groups.items():
        quiet = len(steps) > 6
        done = 0
        for st in steps:
            with console.status(f"[brand]{st['title']}…[/]", spinner="dots"):
                try:
                    msg = sync_run_step(json.dumps(st))
                except ValueError as e:
                    failed += 1
                    err_console.print(f"[fail]✗[/] {st['title']}\n  [muted]{e}[/]")
                    continue
            done += 1
            if not quiet:
                notes = []
                if "previous:" in msg:
                    notes.append("kept the old file as .hlock-bak")
                if "placeholders left" in msg:
                    notes.append("placeholders for missing secrets")
                if st["kind"] in ("plugin", "harness", "runtime", "model", "package"):
                    notes.append(msg)
                console.print(f"[ok]✓[/] {st['title']}" + (f"  [muted]{'; '.join(notes)}[/]" if notes else ""))
        if quiet and done:
            console.print(f"[ok]✓[/] restored {done} {kind}s")

    if docker:
        files = json.loads(sync_dockerfile(json.dumps(plan), lock, manifest=_m()))
        ctx = Path(docker_dir)
        (ctx / "home").mkdir(parents=True, exist_ok=True)
        (ctx / "Dockerfile").write_text(files["dockerfile"])
        entry = ctx / "hlock-entrypoint"
        entry.write_text(files["entrypoint"])
        entry.chmod(0o755)
        _ok(f"wrote [path]{ctx / 'Dockerfile'}[/]")
        tag = f"hlock-{plan['harness']}"
        secrets = " ".join(f"-e {st['id'].split(':', 1)[1]}" for st in plan["steps"] if st["kind"] == "secret")
        if build:
            import subprocess

            console.print(f"[brand]docker build -t {tag} {ctx}[/]")
            if subprocess.run(["docker", "build", "-t", tag, str(ctx)], check=False).returncode != 0:
                _fail("docker build failed")
            _ok(f"built [bold]{tag}[/]")
        console.print("[muted]next →[/]")
        if not build:
            console.print(f"  [cmd]docker build -t {tag} {ctx}[/]", soft_wrap=True)
        run_cmd = f"docker run -it --rm {secrets + ' ' if secrets else ''}-v \"$PWD:/workspace\" {tag}"
        console.print(f"  [cmd]{run_cmd}[/]", soft_wrap=True)
        sys.exit(1 if failed else 0)

    remaining = _manual_steps(plan)
    again = "hlock sync --in-place" if in_place else "hlock sync"
    if remaining:
        secrets = [st for st in remaining if st["kind"] == "secret"]
        others = [st for st in remaining if st["kind"] != "secret"]
        console.print(f"\n[warn]{len(remaining)} step{'s need' if len(remaining) != 1 else ' needs'} you:[/]")
        for i, st in enumerate(others, 1):
            console.print(f"  {i}. {st['title']}" + (f"  [muted]{st['reason']}[/]" if st.get("reason") else ""))
            if st.get("command"):
                console.print(f"     [cmd]{st['command']}[/]")
        if secrets:
            n = len(others) + 1
            console.print(f"  {n}. Store {len(secrets)} secret{'s' if len(secrets) != 1 else ''} in your keychain, then re-run "
                          f"[cmd]{again}[/] [muted](values stay out of the lock)[/]")
            for st in secrets:
                name, _, used = st["title"].partition(" (")
                short = name.removeprefix("HLOCK_SECRET_")
                console.print(f"     [cmd]hlock secret set {short}[/]  [muted]# {used.rstrip(')')}[/]")
            console.print("     [muted]or export each variable, or reference it in ~/.config/hlock/secrets.toml[/]")
        hints = []
        if any("--install" in (st.get("reason") or "") for st in remaining):
            hints.append(f"{again} --install")
        if any("--pull" in (st.get("reason") or "") for st in remaining):
            hints.append(f"{again} --pull")
        hints.append(f"{again} --skill [muted](let your agent do it)[/]")
        _next(*hints)
    if failed:
        sys.exit(1)
    if remaining:
        sys.exit(MANUAL_EXIT)
    if plan["mode"] == "prefix":
        env = " ".join(f"{k}={_rel(v)}" for k, v in plan["isolation"].items())
        _ok(f"in sync at [path]{_rel(plan['root'])}[/]  [muted]your own config wasn't touched[/]")
        console.print(f"[muted]use it:[/] [cmd]{env} {plan.get('binary') or plan['harness']}[/]", soft_wrap=True)
        if not prefix:
            console.print("[muted]to restore into your own config instead:[/] [cmd]hlock sync --in-place[/]")
    else:
        _ok(f"in sync with [path]{_show(lock)}[/]")


@main.command()
@click.option("--json", "as_json", is_flag=True, help="Print the lockfile as JSON.")
def show(as_json: bool) -> None:
    """Show the locked stack and the evidence behind it."""
    with _friendly_errors():
        lf = Lockfile.load(_l())
    if as_json:
        console.print_json(lf.to_json())
        return
    _print_lock(lf)


@main.command()
@click.option("--prefix", default=None,
              help="For a pinned lock: check the isolated root [cmd]hlock sync --prefix DIR[/] writes, not your own config.")
def check(prefix: str | None) -> None:
    """Fail if your setup, or [path]harness.toml[/], has drifted from [path]harness.lock[/].

    For a pinned lock, re-capture the harness and fail if the setup changed.
    """
    if not Path(_l()).exists() and not Path(_m()).exists():
        _fail(f"no {MANIFEST} in this directory or above, and no {LOCK}",
              "create one with [cmd]hlock init[/], or point at one with [cmd]-m PATH[/]")
    with _friendly_errors():
        lf = Lockfile.load(_l())
    if lf.kind == "pin":
        with _working(f"Re-capturing {lf.harness}…"):
            drift = json.loads(_check_pin(_l(), prefix=prefix, manifest=_m()))
        with _friendly_errors():
            drift += _decider_drift(_l())
        which = f"the {lf.harness} setup in [path]{prefix}[/]" if prefix else f"your {lf.harness} setup"
        if drift:
            err_console.print(f"[fail]✗ drift:[/] {which} changed since {_ld()} was pinned")
            for line in drift:
                err_console.print(f"  {line}")
            restore = f"hlock sync --prefix {prefix}" if prefix else "hlock sync --in-place"
            err_console.print(f"  [muted]hint:[/] re-pin with [cmd]hlock pin[/], or restore it with [cmd]{restore}[/]")
            sys.exit(1)
        _ok(f"{which} matches [path]{_ld()}[/]")
        return
    with _friendly_errors():
        m = Manifest.load(_m())
    if lf.manifest_hash is None:
        _fail(f"{_ld()} has no manifest-hash", "re-create it with [cmd]hlock resolve[/]")
    live = m.manifest_hash()
    locked = lf.manifest_hash
    if locked.split(":", 1)[-1] != live.split(":", 1)[-1]:
        err_console.print(f"[fail]✗ drift:[/] {_md()} changed since {_ld()} was written")
        grid = _kv_table()
        grid.add_row("locked", f"[muted]{locked}[/]")
        grid.add_row("live", f"[muted]{live}[/]")
        err_console.print(grid)
        err_console.print("  [muted]hint:[/] re-lock with [cmd]hlock resolve[/], then commit both files")
        sys.exit(1)
    if m.target_harness:
        with _friendly_errors():
            want = json.loads(_select_harness(m.target_harness, manifest=_m()))["id"]
        if want != lf.harness:
            _fail(f"{_ld()} is for harness {lf.harness!r} but {_md()} targets {want!r}",
                  "re-lock with [cmd]hlock resolve[/]")
    if lf.harness_version:
        probe = json.loads(harness_profile(lf.harness, manifest=_m()))["probe"] if _known_harness(lf.harness) else None
        if probe and probe["installed"] and probe.get("version") and probe["version"] != lf.harness_version:
            _fail(f"installed {lf.harness} is {probe['version']}, {_ld()} pins {lf.harness_version}",
                  f"install {lf.harness} {lf.harness_version}, or re-lock with [cmd]hlock resolve[/]")
    with _friendly_errors():
        decider_drift = _decider_drift(_l())
    if decider_drift:
        err_console.print(f"[fail]✗ drift:[/] {_ld()}'s deciders changed")
        for line in decider_drift:
            err_console.print(f"  {line}")
        err_console.print("  [muted]hint:[/] re-lock with [cmd]hlock resolve[/]")
        sys.exit(1)
    _ok(f"[path]{_ld()}[/] matches [path]{_md()}[/]  [muted]{live.split(':', 1)[-1][:12]}[/]")


@main.command()
def tree() -> None:
    """Print the locked component dependency graph."""
    with _friendly_errors():
        lf = Lockfile.load(_l())
    if lf.kind == "pin":
        console.print(_pin_tree(json.loads(lf.to_json())))
        return
    root = Tree(f"[brand]🔒 {_ld()}[/]", guide_style="magenta")
    stack = root.add(f"[bold]stack[/] [muted]harness={lf.harness}[/]")
    llms = stack.add("[key]llm[/]")
    for role, spec in json.loads(lf.llm):
        llms.add(f"{role}  [bold]{spec}[/]")
    stack.add(f"[key]decision[/]  {lf.decision}")
    stack.add(f"[key]harness[/]  [bold]{lf.harness}[/]")
    root.add(f"[key]provenance[/]  backend={lf.provenance}")
    console.print(root)


def _pin_tree(lock: dict) -> Tree:
    h = lock.get("harness") or {}
    root = Tree(f"[brand]📌 {_ld()}[/] [muted]pinned[/]", guide_style="magenta")
    hn = root.add(f"[bold]{h.get('name')}[/] {h.get('version') or ''}  [muted]{h.get('source') or ''}[/]")
    if lock.get("llm"):
        node = hn.add("[key]llm[/]")
        for llm in lock["llm"]:
            node.add(f"{llm['role']}  [bold]{llm['provider']}:{llm['model']}[/]")
    for rt in lock.get("runtime") or []:
        node = root.add(f"[key]runtime[/] [bold]{rt['name']}[/] {rt.get('version') or ''} [muted]({rt.get('location')})[/]")
        for m in rt.get("model") or []:
            mark = "" if rt.get("location") != "local" else ("  [ok]on disk[/]" if m.get("present") else "  [warn]missing[/]")
            node.add(f"{m['id']}  [muted]{m.get('source') or ''}[/]{mark}")
    if lock.get("tool"):
        node = hn.add("[key]mcp[/]")
        for x in lock["tool"]:
            node.add(f"{x['name']}  [muted]{x.get('transport', '')} {x.get('source') or x.get('url') or x.get('command') or ''}[/]")
    if lock.get("skill"):
        node = hn.add(f"[key]skills[/] [muted]({len(lock['skill'])})[/]")
        for sk in lock["skill"]:
            node.add(f"{sk['name']}  [muted]{sk.get('origin')}[/]")
    if lock.get("native"):
        node = hn.add("[key]plugins[/]")
        for n in lock["native"]:
            off = "" if n.get("enabled", True) else "  [muted](disabled)[/]"
            node.add(f"{n['name']} {n.get('version') or ''}  [muted]{n['kind']}[/]{off}")
    return root


@main.command()
@click.argument("prompt")
@click.option("--json", "as_json", is_flag=True, help="Print the raw outcome as JSON.")
def run(prompt: str, as_json: bool) -> None:
    """Run the locked agent on PROMPT.

    An agent CLI runs for real, in the locked environment [cmd]hlock launch[/] uses, through
    its own non-interactive mode ([cmd]claude -p[/], [cmd]codex exec[/], [cmd]opencode run[/], …).
    """
    with _friendly_errors():
        lf = Lockfile.load(_l())
        agent = load(_l())
    kind = _harness_kind(lf.harness)
    if kind == "custom":
        _run_code_harness(prompt, as_json)
        return
    if kind == "cli":
        _run_cli_harness(prompt, as_json)
        return
    with _working("Running the locked agent…"):
        raw = agent.run(prompt)
    outcome = json.loads(raw)
    ok = bool(outcome.get("success"))
    if as_json:
        console.print_json(raw)
    else:
        mark = "[ok]✓ success[/]" if ok else "[fail]✗ failed[/]"
        console.print(
            f"{mark}  [muted]model[/] {outcome.get('model')}  "
            f"[muted]cost[/] {_usd(outcome.get('cost_usd', 0))}"
        )
    sys.exit(0 if ok else 1)


def _harness_kind(name: str) -> str | None:
    """`cli`, `custom` (a code harness), or None for the reference harness."""
    try:
        return json.loads(harness_profile(name, manifest=_m()))["profile"]["kind"]
    except ValueError:
        return None


def _run_cli_harness(prompt: str, as_json: bool) -> None:
    with _working("Preparing the locked environment…"):
        prep = json.loads(launch_prepare(_l(), prompt=prompt, manifest=_m()))
    secret_env = prep.pop("secret_env", {})  # values for the config's references; never shown
    env = {**os.environ, **prep["env"], **secret_env}
    if not as_json:
        _print_prep(prep)
        sys.stdout.flush()
    start = time.monotonic()
    try:
        r = subprocess.run(prep["argv"], env=env, capture_output=as_json, text=True, check=False)
    except OSError as e:
        _fail(f"cannot start {prep['argv'][0]}: {e}")
    latency = time.monotonic() - start
    ok = r.returncode == 0
    if as_json:
        console.print_json(data={
            "harness": prep["harness"], "success": ok, "exit_code": r.returncode, "output": r.stdout,
            "stderr": r.stderr, "latency_s": round(latency, 3), "notes": prep["notes"],
            "missing_secrets": prep["missing_secrets"],
        })
    else:
        mark = "[ok]✓ success[/]" if ok else f"[fail]✗ failed[/] [muted](exit {r.returncode})[/]"
        console.print(f"{mark}  [muted]harness[/] {prep['harness_name']}  [muted]latency[/] {_secs(latency)}")
    sys.exit(0 if ok else 1)


def _run_code_harness(prompt: str, as_json: bool) -> None:
    with _working("Running the locked harness…"):
        r = json.loads(run_code_harness(prompt, _l(), manifest=_m()))
    if as_json:
        console.print_json(data=r)
    else:
        if r.get("log"):
            console.print(r["log"].rstrip(), markup=False, highlight=False)
        mark = "[ok]✓ success[/]" if r["success"] else f"[fail]✗ failed[/] [muted](exit {r.get('exit_code')})[/]"
        console.print(f"{mark}  [muted]latency[/] {_secs(r.get('latency_s'))}  [muted]cost[/] {_usd(r.get('cost_usd'))}")
    sys.exit(0 if r["success"] else 1)


@main.command(context_settings={"ignore_unknown_options": True, "allow_interspersed_args": False})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
@click.option("--lock", "lock", default=None, help="Lock to launch.  [dim]\\[default: the project's lock][/]")
@click.option("--prefix", default=None, help="Where isolated roots live.  [dim]\\[default: .hlock/env][/]")
@click.option("--in-place", is_flag=True, help="Use your own config instead of an isolated root (same harness only).")
@click.option("--allow-version-mismatch", is_flag=True, help="Launch even if the installed version differs from the lock.")
@click.option("--dry-run", is_flag=True, help="Prepare everything, print the command, and don't start it.")
@click.option("--json", "as_json", is_flag=True, help="With --dry-run: print the launch plan as JSON.")
def launch(args: tuple[str, ...], lock: str | None, prefix: str | None, in_place: bool, allow_version_mismatch: bool,
           dry_run: bool, as_json: bool) -> None:
    """Start a session in a harness that matches the lock.

    [cmd]hlock launch[/] starts the lock's harness; [cmd]hlock launch pi[/] starts another one with
    the lock's portable parts (MCP servers, skills, instructions, model) translated for it.
    Other arguments are passed through: [cmd]hlock launch claude --resume[/]. Put arguments
    after [cmd]--[/] to pass one that hlock would otherwise take, like [cmd]-- --dry-run[/].

    The session runs against an isolated config root in [path].hlock/env/[/], restored from
    the lock, so it sees exactly the locked setup and none of your unlocked config.
    """
    # hlock's own flags count anywhere before `--`; everything else goes to the harness.
    harness = None
    rest: list[str] = []
    items = list(args)
    i = 0
    while i < len(items):
        a = items[i]
        if a == "--":
            rest.extend(items[i + 1:])
            break
        if a == "--dry-run":
            dry_run = True
        elif a == "--json":
            as_json = True
        elif a == "--in-place":
            in_place = True
        elif a == "--allow-version-mismatch":
            allow_version_mismatch = True
        elif a in ("--lock", "--prefix") and i + 1 < len(items):
            if a == "--lock":
                lock = items[i + 1]
            else:
                prefix = items[i + 1]
            i += 1
        elif harness is None and not rest and not a.startswith("-") and _known_harness(a):
            harness = a
        else:
            rest.append(a)
        i += 1
    if lock:
        lock, stored = _lock_ref(lock, as_json)
        if stored and prefix is None:
            prefix = _pull_env(stored)  # a pulled lock's environments live with it, not in this project
    with _working("Preparing the locked environment…"):
        prep = json.loads(launch_prepare(lock or _l(), harness=harness, prefix=prefix,
                                         allow_version_mismatch=allow_version_mismatch,
                                         in_place=in_place, args=rest, manifest=_m()))
    secret_env = prep.pop("secret_env", {})  # values for the config's references; never shown
    if as_json:
        console.print_json(data=prep)
        return
    _print_prep(prep)
    if dry_run:
        return
    env = {**os.environ, **prep["env"], **secret_env}
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        os.execvpe(prep["argv"][0], prep["argv"], env)
    except OSError as e:
        _fail(f"cannot start {prep['argv'][0]}: {e}")


def _print_prep(prep: dict) -> None:
    """What preparing a locked environment did, and the command it's about to run."""
    copies = [line for line in prep["done"] if line.startswith("Copy skill")]
    for line in prep["done"]:
        if len(copies) <= 6 or not line.startswith("Copy skill"):
            console.print(f"[ok]✓[/] {line}")
    if len(copies) > 6:
        console.print(f"[ok]✓[/] copied {len(copies)} skills")
    for line in prep["notes"]:
        console.print(f"[warn]![/] {line}")
    if prep["missing_secrets"]:
        console.print(f"[warn]![/] {len(prep['missing_secrets'])} secret(s) not set; their placeholders stay: "
                      + ", ".join(prep["missing_secrets"]))
    env_txt = " ".join(f"{k}={_rel(v)}" for k, v in prep["env"].items() if not k.startswith(("HLOCK_", "PYTHON")))
    shown = " ".join(prep["argv"][:1] + [a if len(a) < 60 else a[:57] + "…" for a in prep["argv"][1:]])
    console.print(f"[brand]▶[/] {prep['harness_name']}  [muted]{env_txt}[/] [cmd]{shown}[/]", soft_wrap=True)


@main.command()
@click.option("--to", default=None, help="Target model/version to upgrade to.")
@click.option("--all", "all_", is_flag=True, help="Upgrade every upgradeable component.")
@click.option("--apply", is_flag=True, help="Perform the upgrade (default is plan-only).")
@click.option("--rollback", is_flag=True, help="Restore harness.lock from harness.lock.prev.")
@click.option("--force", is_flag=True, help="Apply even if the success rate regresses.")
@click.option("--workdir", default=None, help="Where to put trial artifacts.")
def upgrade(to: str | None, all_: bool, apply: bool, rollback: bool, force: bool, workdir: str | None) -> None:
    """Plan, apply, or roll back upgrades to locked components."""
    if not rollback and Path(_l()).exists():
        with _friendly_errors():
            if Lockfile.load(_l()).kind == "pin":
                _fail(f"{_ld()} is a pinned lock; upgrade re-measures a resolved one against your tasks",
                      "update your setup (install a newer harness, change the model), then re-pin with "
                      "[cmd]hlock pin[/]")
    label = "Rolling back…" if rollback else ("Applying upgrade…" if apply else "Planning upgrades…")
    with _working(label):
        result = json.loads(
            al_upgrade(_m(), to=to, all=all_, apply=apply, rollback=rollback, force=force, workdir=workdir,
                       lock=_l())
        )
    items = result.get("plan", {}).get("upgradeable", [])
    if items:
        t = Table(box=None, header_style="bold", pad_edge=False)
        t.add_column("component", style="key")
        t.add_column("locked")
        t.add_column("")
        t.add_column("available", style="bold green")
        for item in items:
            t.add_row(item["role"], item["from"], "→", item["to"])
        console.print(t)
    if result.get("rolled_back"):
        _ok(f"restored [path]{_ld()}[/] from [path]{_ld()}.prev[/]")
    elif result.get("applied"):
        _ok(
            f"upgraded  [muted]success rate[/] {_pct(result.get('before'))} → "
            f"[bold]{_pct(result.get('after'))}[/]  [muted](previous lock kept at {_ld()}.prev)[/]"
        )
    else:
        console.print(f"[muted]{result.get('message', '')}[/]")
        if items and not apply:
            _next("hlock upgrade --all --apply")


@main.group(invoke_without_command=True)
@click.option("--depth", default="auto", show_default=True,
              type=click.Choice(["probe", "prompt", "hpo", "ft", "full", "auto"]), help="How deep to tune.")
@click.option("--data", default="user", show_default=True, help="user | user:<paths> | synthetic\\[:n] | hub:<pack>")
@click.option("--harness", default=None, help="Restrict to one harness.")
@click.option("--out", default="harness.tune.lock", show_default=True, help="Output tune lock filename.")
@click.option("--strict", is_flag=True, help="Fail unless the tune lock beats the base.")
@click.option("--apply", is_flag=True, help="Also adopt the result into harness.lock.")
@click.option("--max-trials", default=None, type=int, help="Cap the number of trials.")
@click.option("--backend", default=None, type=click.Choice(["local", "devbox", "cluster"]),
              help="Override the manifest's \\[backend] target.")
@click.option("--workdir", default=None, help="Where to put trial artifacts.")
@click.option("-d", "--detach", is_flag=True,
              help="Run in the background as a tune job. See [cmd]hlock tune list[/].")
@click.pass_context
def tune(ctx: click.Context, depth: str, data: str, harness: str | None, out: str, strict: bool, apply: bool,
         max_trials: int | None, backend: str | None, workdir: str | None, detach: bool) -> None:
    """Tune prompts, hyperparameters and small models into [path]harness.tune.lock[/].

    Long tunes can run in the background: [cmd]hlock tune --detach[/] starts a tune job,
    and [cmd]hlock tune fetch[/] collects its result when it finishes.
    """
    if ctx.invoked_subcommand is not None:
        return
    if detach:
        _tune_detach(depth=depth, data=data, harness=harness, strict=strict, apply=apply,
                     max_trials=max_trials, backend=backend, workdir=workdir)
        return
    with _working(f"Tuning (depth={depth}, data={data})…"):
        result = json.loads(
            al_tune(_m(), depth=depth, data=data, harness=harness, out=out, strict=strict, apply=apply,
                    max_trials=max_trials, backend=backend, workdir=workdir, lock=_l())
        )
    _ok(f"wrote [path]{result.get('path')}[/]")
    console.print(_tune_panel(result.get("report", {})))
    if apply:
        _ok(f"adopted into [path]{_ld()}[/]")
    else:
        _next("hlock tune --apply")


def _tune_panel(r: dict, title: str = "[brand]⚙ tune[/]") -> Panel:
    best = r.get("best") or {}
    run_info = _kv_table()
    run_info.add_row("depth", str(r.get("depth")))
    run_info.add_row("backend", str(r.get("backend")))
    run_info.add_row("data", str(r.get("data_mode") or r.get("data")))
    run_info.add_row("grid", f"{r.get('grid')} configurations")

    chosen = _kv_table()
    for k, v in best.items():
        if k in ("success_rate", "cost_usd", "trials"):
            continue
        chosen.add_row(k.replace("_", " "), f"[bold]{v}[/]")

    result_row = Text.assemble(
        ("success rate  ", "key"), _pct(r.get("baseline_success_rate")), "  →  ",
        (_pct(best.get("success_rate")), "bold"), "   ", _signed_pct(r.get("gain_success_rate")),
    )
    return Panel(
        Group(run_info, Text(""), Text("Best configuration", style="bold"), chosen, Text(""), result_row),
        title=title, title_align="left", border_style="magenta", expand=False, padding=(1, 2),
    )


def _root() -> Path:
    return _Project.manifest.parent


def _tune_detach(*, apply: bool, **options) -> None:
    if apply:
        _fail("--apply doesn't work with --detach",
              "adopt a finished job with [cmd]hlock tune fetch <job> --apply[/]")
    with _friendly_errors():
        job = _jobs.submit(_m(), lock=_l(), options=options)
    _ok(f"started tune job [bold]{job['id']}[/]  [muted](depth={options['depth']}, pid {job['pid']})[/]")
    console.print(f"[key]log[/] [path]{_show(Path(job['dir']) / _jobs.LOG)}[/]")
    _next(f"hlock tune show {job['id']}", f"hlock tune fetch {job['id']} --wait")


_STATUS_STYLE = {"queued": "muted", "running": "warn", "succeeded": "ok", "failed": "fail", "cancelled": "muted"}


def _status(s: str) -> str:
    return f"[{_STATUS_STYLE.get(s, 'muted')}]{s}[/]"


def _ago(ts: str | None) -> str:
    if not ts:
        return "—"
    secs = (datetime.now(timezone.utc) - datetime.fromisoformat(ts)).total_seconds()
    return f"{_duration(secs)} ago"


def _duration(secs: float | None) -> str:
    if secs is None:
        return "—"
    d, rest = divmod(max(0, int(secs)), 86400)
    h, rest = divmod(rest, 3600)
    m, s = divmod(rest, 60)
    parts = [f"{v}{u}" for v, u in ((d, "d"), (h, "h"), (m, "m"), (s, "s")) if v]
    return " ".join(parts[:2]) or "0s"


def _elapsed(job: dict) -> float | None:
    if not job.get("started_at"):
        return None
    end = datetime.fromisoformat(job["finished_at"]) if job.get("finished_at") else datetime.now(timezone.utc)
    return (end - datetime.fromisoformat(job["started_at"])).total_seconds()


@contextmanager
def _job_errors():
    try:
        yield
    except _jobs.JobError as e:
        msg = str(e)
        hint = None
        if "no tune jobs" in msg:
            hint = "start one with [cmd]hlock tune --detach[/]"
        elif "no tune job" in msg or "ambiguous" in msg:
            hint = "list jobs with [cmd]hlock tune list[/]"
        elif " is queued" in msg or " is running" in msg:
            hint = "wait for it with [cmd]--wait[/], or check on it with [cmd]hlock tune show[/]"
        _fail(msg, hint)


@tune.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print the jobs as JSON.")
def tune_list(as_json: bool) -> None:
    """List this project's tune jobs, newest first."""
    jobs = _jobs.list_jobs(_root())
    if as_json:
        console.print_json(data=jobs)
        return
    if not jobs:
        console.print("[muted]no tune jobs yet[/]")
        _next("hlock tune --detach")
        return
    t = Table(box=None, header_style="bold", pad_edge=False)
    for col in ("job", "status", "depth", "backend", "started", "elapsed", "success rate"):
        t.add_column(col)
    for j in jobs:
        o, r = j.get("options") or {}, j.get("report") or {}
        sr = (r.get("best") or {}).get("success_rate")
        t.add_row(f"[key]{j['id']}[/]", _status(j["status"]), str(o.get("depth")),
                  str(o.get("backend")), _ago(j.get("started_at") or j.get("created_at")),
                  _duration(_elapsed(j)), _pct(sr) if sr is not None else "—")
    console.print(t)


@tune.command("show")
@click.argument("job", required=False)
@click.option("--log", "log_lines", default=10, show_default=True, type=int, help="How many log lines to show.")
@click.option("--json", "as_json", is_flag=True, help="Print the job as JSON.")
def tune_show(job: str | None, log_lines: int, as_json: bool) -> None:
    """Show a tune job's status, settings and result. [dim]\\[default: the newest job][/]"""
    with _job_errors():
        j = _jobs.get(_root(), job)
    if as_json:
        console.print_json(data=j)
        return
    o = j.get("options") or {}
    info = _kv_table()
    info.add_row("status", _status(j["status"]))
    info.add_row("depth", str(o.get("depth")))
    info.add_row("data", str(o.get("data")))
    info.add_row("backend", str(o.get("backend")))
    if o.get("max_trials"):
        info.add_row("max trials", str(o["max_trials"]))
    info.add_row("started", _ago(j.get("started_at") or j.get("created_at")))
    info.add_row("elapsed", _duration(_elapsed(j)))
    if j["status"] == "running":
        info.add_row("trials", f"{j.get('trials', 0)} so far")
    for run in j.get("runs") or []:
        info.add_row("flyte run", run)
    if j.get("error"):
        info.add_row("error", f"[fail]{j['error']}[/]")
    info.add_row("log", f"[path]{_show(Path(j['dir']) / _jobs.LOG)}[/]")
    console.print(Panel(info, title=f"[brand]⚙ tune job {j['id']}[/]", title_align="left",
                        border_style="magenta", expand=False, padding=(1, 2)))
    if j.get("report"):
        console.print(_tune_panel(j["report"], title="[brand]⚙ result[/]"))
    if log_lines > 0 and j["status"] != "succeeded":
        try:
            lines = (Path(j["dir"]) / _jobs.LOG).read_text(errors="replace").splitlines()[-log_lines:]
        except OSError:
            lines = []
        if lines:
            console.print(Text("\n".join(lines), style="muted"))
    if j["status"] == "succeeded":
        _next(f"hlock tune fetch {j['id']}", f"hlock tune fetch {j['id']} --apply")
    elif j["status"] in _jobs.ACTIVE:
        _next(f"hlock tune fetch {j['id']} --wait", f"hlock tune cancel {j['id']}")


@tune.command("fetch")
@click.argument("job", required=False)
@click.option("--out", default="harness.tune.lock", show_default=True, help="Where to write the tune lock.")
@click.option("--apply", is_flag=True, help="Also adopt the result into harness.lock.")
@click.option("--wait", is_flag=True, help="Wait for the job to finish first.")
def tune_fetch(job: str | None, out: str, apply: bool, wait: bool) -> None:
    """Write a finished job's result to [path]harness.tune.lock[/]. [dim]\\[default: the newest job][/]"""
    root = _root()
    with _job_errors():
        if wait:
            ref = _jobs.get(root, job)["id"]
            with console.status(f"[brand]Waiting for tune job {ref}…[/]", spinner="dots"):
                _jobs.wait(root, ref)
            job = ref
        j = _jobs.fetch(root, job, out=out, apply=apply)
    _ok(f"wrote [path]{_show(j['fetched'])}[/]  [muted](from tune job {j['id']})[/]")
    console.print(_tune_panel(j.get("report") or {}))
    if j.get("base_changed"):
        console.print(f"[warn]![/] [path]{_ld()}[/] changed since this job started, so it was tuned "
                      "against an older base lock")
    if apply:
        _ok(f"adopted into [path]{_ld()}[/]")
    else:
        _next(f"hlock tune fetch {j['id']} --apply")


@tune.command("cancel")
@click.argument("job")
def tune_cancel(job: str) -> None:
    """Stop a running tune job."""
    with _job_errors():
        j = _jobs.cancel(_root(), job)
    _ok(f"tune job [bold]{j['id']}[/] {_status(j['status'])}")


def _known_harness(name: str) -> bool:
    try:
        _select_harness(name, manifest=_m())
        return True
    except ValueError:
        return False


def _support_badge(support: str) -> str:
    return "[ok]full[/]" if support == "full" else "[muted]basic[/]"


@main.group()
def harness() -> None:
    """Inspect the agent harnesses hlock knows about (Claude Code, Codex, Pi, …)."""


@harness.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print the list as JSON.")
def harness_list(as_json: bool) -> None:
    """List known harnesses and what's installed on this machine.

    [bold]cli[/] harnesses are agent CLIs, [bold]framework[/] harnesses are agent libraries a
    project builds on, and [bold]custom[/] harnesses come from [key]\\[harnesses.<id>][/] in
    [path]harness.toml[/].
    """
    with _working("Probing installed harnesses…"):
        rows = json.loads(harnesses(manifest=_m()))
    if as_json:
        console.print_json(data=rows)
        return
    for kind, title in (("cli", "Agent CLIs"), ("framework", "Frameworks"), ("custom", f"Project harnesses ({_md()})")):
        group = [r for r in rows if r["kind"] == kind]
        if not group:
            continue
        t = Table(box=None, header_style="bold", pad_edge=False, title=title, title_justify="left",
                  title_style="bold")
        t.add_column("")
        t.add_column("harness", style="key", no_wrap=True)
        t.add_column("installed", no_wrap=True)
        t.add_column("source", style="muted", overflow="fold")
        if kind == "cli":
            t.add_column("support")
        for r in group:
            p = r["probe"]
            installed = f"[bold]{p.get('version') or 'yes'}[/]" if p["installed"] else "[muted]—[/]"
            cells = ["[ok]●[/]" if p["active"] else "", r["id"], installed, p.get("source") or ""]
            if kind == "cli":
                cells.append(_support_badge(r["support"]))
            t.add_row(*cells)
        console.print(t)
        console.print()
    active = next((r for r in rows if r["probe"]["active"]), None)
    if active:
        console.print(f"[ok]●[/] [muted]you're running inside {active['name']}[/]")
    _next("hlock harness show <name>")


@harness.command("show")
@click.argument("name", required=False)
@click.option("--json", "as_json", is_flag=True, help="Print the profile as JSON.")
def harness_show(name: str | None, as_json: bool) -> None:
    """Show a harness profile and the paths it reads on this machine.

    NAME defaults to the target harness (see [cmd]hlock harness list[/]).
    """
    with _friendly_errors():
        if name is None:
            name = json.loads(_select_harness(None, _manifest_target()))["id"]
        info = json.loads(harness_profile(name, manifest=_m()))
    if as_json:
        console.print_json(data=info)
        return
    prof, probe, paths = info["profile"], info["probe"], info["paths"]
    t = _kv_table()
    t.add_row("id", f"[bold]{prof['id']}[/]  [muted]{', '.join(prof['aliases'])}[/]")
    t.add_row("kind", prof["kind"] + (f"  [muted](framework {prof['framework']})[/]" if prof.get("framework") else ""))
    if prof["kind"] == "cli":
        t.add_row("support", _support_badge(prof["support"]))
    run = prof.get("run") or {}
    if run.get("entrypoint"):
        t.add_row("entrypoint", f"[bold]{run['entrypoint']}[/]")
    if run.get("command"):
        t.add_row("command", " ".join(run["command"]))
    for pkg in prof.get("packages") or []:
        have = (probe.get("packages") or {}).get(pkg)
        t.add_row("package", f"{pkg}  " + (f"[bold]{have}[/]" if have else "[muted]not installed[/]"))
    if prof["kind"] == "cli":
        if probe["installed"]:
            t.add_row("installed", f"[bold]{probe.get('version') or '?'}[/]  [muted]{probe.get('binary')}[/]")
            t.add_row("source", probe.get("source") or "[muted]unknown install method[/]")
        else:
            t.add_row("installed", "[muted]no[/]")
        root_env = info.get("isolation_env")
        relocate = f"  [muted](${root_env})[/]" if root_env else ""
        t.add_row("config root", f"[path]{paths['config_root']}[/]{relocate}")
    for fid, f in paths["files"].items():
        mark = "" if f["exists"] else "  [muted](missing)[/]"
        t.add_row(f"file · {fid}", f"{f['path']}{mark}")
    for d in paths["skills"]:
        mark = "" if d["exists"] else "  [muted](missing)[/]"
        t.add_row("skills", f"{d['path']}{mark}")
    t.add_row("profile", "[muted]built in[/]" if prof["origin"] == "builtin" else f"[path]{prof['origin']}[/]")
    console.print(Panel(t, title=f"[brand]{prof['name']}[/]", title_align="left", border_style="magenta",
                        expand=False, padding=(1, 2)))


@main.group()
def runtime() -> None:
    """Inspect local inference runtimes (Ollama, LM Studio, vLLM, …)."""


@runtime.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print the list as JSON.")
def runtime_list(as_json: bool) -> None:
    """List known inference runtimes and what's installed on this machine."""
    with _working("Probing inference runtimes…"):
        rows = json.loads(runtimes())
    if as_json:
        console.print_json(data=rows)
        return
    t = Table(box=None, header_style="bold", pad_edge=False)
    t.add_column("runtime", style="key")
    t.add_column("installed")
    t.add_column("port", style="muted")
    t.add_column("models dir", style="muted")
    for r in rows:
        p = r["probe"]
        installed = f"[bold]{p.get('version') or 'yes'}[/]" if p["installed"] else "[muted]—[/]"
        t.add_row(r["id"], installed, ", ".join(str(x) for x in r["ports"]), r.get("models_dir") or "")
    console.print(t)


@main.group()
def secret() -> None:
    """Store and inspect the secrets a lock needs ([cmd]HLOCK_SECRET_*[/]).

    A lock holds placeholders, never values. hlock fills one from the environment
    variable of the same name, else from a reference in [path]~/.config/hlock/secrets.toml[/]
    (a command, another variable, or a file), else from the OS keychain. Values are
    never printed.
    """


@secret.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print the list as JSON.")
def secret_list(as_json: bool) -> None:
    """List the lock's secrets and where each value would come from."""
    with _friendly_errors():
        st = json.loads(secret_status(_l(), manifest=_m()))
    if as_json:
        console.print_json(data=st)
        return
    if not st["secrets"]:
        console.print(f"[muted]{_ld()} needs no secrets[/]")
        return
    t = Table(box=None, header_style="bold", pad_edge=False)
    t.add_column("secret", style="key")
    t.add_column("from")
    t.add_column("used by", style="muted")
    for r in st["secrets"]:
        t.add_row(r["name"], r["source"] or "[warn]not set[/]", r["used_by"])
    console.print(t)
    keychain = st["keychain"] or "none found"
    console.print(f"[muted]secrets file[/] {_tilde(st['file'])}  [muted]· keychain[/] {keychain}")
    if any(not r["source"] for r in st["secrets"]):
        _next("hlock secret set NAME")


@secret.command("set")
@click.argument("name")
def secret_set_cmd(name: str) -> None:
    """Store a secret in the OS keychain (macOS Keychain, or libsecret on Linux).

    NAME is the placeholder variable, with or without its [cmd]HLOCK_SECRET_[/] prefix. The value
    is read without echo, or from stdin when it isn't a terminal:
    [cmd]op read op://Work/GitHub/token | hlock secret set GITHUB_GITHUB_TOKEN[/]
    """
    if sys.stdin.isatty():
        value = click.prompt(f"value for {name}", hide_input=True)
    else:
        value = sys.stdin.read().strip()
    if not value:
        _fail("no value given")
    with _friendly_errors():
        full = secret_set(name, value)
    _ok(f"stored [key]{full}[/] in the keychain")


@secret.command("rm")
@click.argument("name")
def secret_rm(name: str) -> None:
    """Remove a secret from the OS keychain."""
    with _friendly_errors():
        removed = secret_delete(name)
    if removed:
        _ok(f"removed [key]{name}[/] from the keychain")
    else:
        console.print(f"[muted]the keychain has no {name}[/]")


def _manifest_target() -> str | None:
    try:
        return Manifest.load(_m()).target_harness
    except ValueError:
        return None


@main.command()
@click.argument("lock", default=None, required=False)
@click.option("-o", "--out", default="harness.local.lock", show_default=True, help="Where to write the localized lock.")
@click.option("--harness", default=None, help="Local harness to use. [dim]\\[default: the lock's if local-capable, else the best installed][/]")
@click.option("--runtime", default=None, help="Inference runtime (ollama, lmstudio, llama-cpp, …). [dim]\\[default: the best installed][/]")
@click.option("--models", default=None, help="Comma-separated model ids to consider instead of the runtime's catalog.")
@click.option("--memory-gb", type=float, default=None, help="Memory budget for model weights. [dim]\\[default: measured][/]")
@click.option("--evals", "--tasks", "evals", default=None,
              help="Eval pack to evaluate candidates on. [dim]\\[default: harness.toml's, if any][/]")
@click.option("--strict", is_flag=True, help="Drop remote MCP servers instead of flagging them.")
@click.option("--allow-network", is_flag=True, help="Keep self-hosted runtimes on your network instead of replacing them.")
@click.option("--json", "as_json", is_flag=True, help="Print the localize report as JSON.")
def localize(lock: str | None, out: str, harness: str | None, runtime: str | None, models: str | None,
             memory_gb: float | None, evals: str | None, strict: bool, allow_network: bool, as_json: bool) -> None:
    """Write an all-local equivalent of a lock to [path]harness.local.lock[/].

    A proprietary harness, or one that can't use a local model, becomes a local-capable
    one; models on API providers move to a local runtime, picked from its catalog and the
    weights on disk to fit this machine's memory. MCP servers, skills and instructions carry
    over. With eval packs, candidates are evaluated; otherwise they're ranked.
    """
    model_list = [m.strip() for m in models.split(",")] if models else []
    with _working("Localizing…"):
        lock = lock or _l()
        result = json.loads(_localize(lock, out, harness=harness, runtime=runtime, models=model_list,
                                      memory_gb=memory_gb, evals=evals, strict=strict, allow_network=allow_network,
                                      manifest=_m()))
    rep = result["report"]
    if as_json:
        console.print_json(data=rep)
        return
    _ok(f"wrote [path]{out}[/]  [muted](from {_show(lock)})[/]")
    t = _kv_table()
    h = rep["harness"]
    inst = "" if h["installed"] else "  [warn]not installed[/]"
    t.add_row("harness", f"[bold]{h['to']}[/]{inst}  [muted]{h['why']}[/]")
    rt = rep["runtime"]
    rinst = f"[bold]{rt['name']}[/] {rt.get('version') or ''}" + ("" if rt["installed"] else "  [warn]not installed[/]")
    t.add_row("runtime", f"{rinst}  [muted]{rt['endpoint']}[/]")
    for llm in result["lock"]["llm"]:
        t.add_row(f"llm · {llm['role']}", f"[bold]{llm['provider']}:{llm['model']}[/]")
    mem = rep.get("memory")
    if mem:
        t.add_row("memory", f"{mem['usable-gb']} GB for weights  [muted]({mem['total-gb']} GB, {mem['how']})[/]")
    cands = rep["models"]["candidates"]
    if cands:
        shown = ", ".join(f"{c['id']}{' ✓' if c['present'] else ''}" for c in cands[:6])
        t.add_row("candidates", f"{shown}  [muted](✓ on disk)[/]")
    if rep["evaluated"]:
        ev = result["lock"]["evidence"]
        base = rep["baseline"]
        sr = (ev.get("success-rate") or {}).get("mean")
        line = f"success rate {_pct(sr)}"
        if base.get("success-rate") is not None:
            line += f"  [muted](source lock: {_pct(base['success-rate'])})[/]"
        t.add_row("evidence", line)
    else:
        t.add_row("evidence", "[muted]not evaluated (no eval packs); ranked by fit, tier, and what's on disk[/]")
    console.print(Panel(t, title=f"[brand]🏠 {out} · localized[/]", title_align="left", border_style="magenta",
                        expand=False, padding=(1, 2)))
    for line in rep["replaced"]:
        console.print(f"  [ok]~[/] {line}")
    for line in rep["notes"]:
        console.print(f"  [warn]![/] {line}")
    _next(f"hlock sync {out} --pull", f"hlock launch --lock {out}")


@main.group()
def backend() -> None:
    """Inspect the tuning backend."""


@backend.command("status")
def backend_status() -> None:
    """Show the configured backend and the environment it will use."""
    try:
        target = Manifest.load(_m()).backend
        source = f"from {_md()}"
    except ValueError:
        target, source = "local", f"no {_md()}, using the default"

    t = _kv_table()
    t.add_row("backend", f"[bold]{target}[/]  [muted]({source})[/]")
    t.add_row("HL_PYTHON", os.environ.get("HL_PYTHON") or "[muted]python3 (default)[/]")
    llm = os.environ.get("HL_LLM_BASE_URL")
    t.add_row("HL_LLM_BASE_URL", llm or "[muted]unset: offline reference LLM[/]")
    if llm:
        t.add_row("HL_LLM_MODEL", os.environ.get("HL_LLM_MODEL") or "[muted]unset[/]")
        t.add_row("HL_LLM_API_KEY", "[ok]set[/]" if os.environ.get("HL_LLM_API_KEY") else "[muted]unset[/]")
    flyte_cfg = Path.home() / ".flyte" / "config.yaml"
    t.add_row("flyte config", f"[path]{flyte_cfg}[/]" if flyte_cfg.exists() else "[muted]not found[/]")
    console.print(t)


# --------------------------------------------------------------------------- #
# hlock meta
# --------------------------------------------------------------------------- #

META_MANIFEST = "harness.meta.toml"
META_LOCK = "harness.meta.lock"
NEEDS_YOU_EXIT = 3


def _mm() -> str:
    """The meta manifest path: `HLOCK_META_MANIFEST`, else next to the project's lock."""
    return os.environ.get("HLOCK_META_MANIFEST") or str(_Project.lock.parent / META_MANIFEST)


def _ml() -> str:
    """The meta lock path: `HLOCK_META_LOCK`, else next to the project's lock."""
    return os.environ.get("HLOCK_META_LOCK") or str(_Project.lock.parent / META_LOCK)


def _meta_prefix() -> str:
    """Where the meta harness's own config root lives (`hlock launch`'s isolated root)."""
    return str(_Project.lock.parent / ".hlock" / "env" / Path(_ml()).stem)


def _meta_cfg() -> dict:
    with _friendly_errors():
        return json.loads(meta_config(_mm()))


def _model_text(model: dict) -> str:
    name = model.get("name") or "[muted]the harness's default[/]"
    if model.get("runtime"):
        return f"[bold]{model['runtime']}:{name}[/]  [muted](local)[/]"
    if model.get("provider"):
        return f"[bold]{model['provider']}:{name}[/]"
    return name


def _key_text(model: dict) -> str | None:
    key = model.get("key-env")
    if not key:
        return None
    return f"[ok]{key} is set[/]" if os.environ.get(key) else f"[warn]{key} isn't set[/]  [muted]export {key}=…[/]"


def _meta_is_builtin() -> bool:
    try:
        return Lockfile.load(_ml()).harness in ("builtin", "hlock", "hlock-agent")
    except ValueError:
        return False


def _need_meta_lock() -> None:
    if not Path(_ml()).exists():
        _fail(f"no meta harness pinned: {_show(_ml())} doesn't exist",
              "set one up with [cmd]hlock meta init[/]" if not Path(_mm()).exists() else "pin it with [cmd]hlock meta pin[/]")


def _run_shell(command: str, what: str) -> bool:
    console.print(f"[brand]$ {command}[/]", soft_wrap=True)
    ok = subprocess.run(command, shell=True, check=False).returncode == 0
    if not ok:
        err_console.print(f"[fail]✗[/] {what} failed; run it yourself: [cmd]{command}[/]", soft_wrap=True)
    return ok


def _confirm(question: str, yes: bool) -> bool:
    if yes:
        return True
    if not sys.stdin.isatty():
        return False
    return click.confirm(question, default=True)


@main.group()
def meta() -> None:
    """The agent hlock uses for its own work: installs, eval packs, updates.

    hlock delegates work that needs judgment to a [bold]meta harness[/], pinned in
    [path]harness.meta.lock[/]: hlock's built-in agent by default (nothing to install), or
    an open-source harness like Pi, Codex or Goose with a config of its own.
    hlock writes each job's prompt, runs the agent, and checks the result itself.

    Get started: [cmd]hlock meta init[/] → [cmd]hlock meta run install[/]
    """


@meta.command("init")
@click.option("--harness", default=None, help="The meta harness: builtin, pi, codex, goose, opencode, hermes. [dim]\\[default: builtin, which ships with hlock][/]")
@click.option("--provider", default=None, help="An API provider (anthropic, openai, google, …). [dim]\\[default: the first whose API key is set][/]")
@click.option("--model", default=None, help="The model. [dim]\\[default: one per provider, or the best local model that fits][/]")
@click.option("--local", is_flag=True, help="Use open weights served by a local runtime instead of an API.")
@click.option("--runtime", default=None, help="The local runtime (ollama, lmstudio, …). Implies --local.")
@click.option("--memory-gb", type=float, default=None, help="Memory budget for local model weights. [dim]\\[default: measured][/]")
@click.option("--base-url", default=None, help="The model API's URL, e.g. an OpenAI-compatible gateway. [dim]\\[default: the provider's or runtime's][/]")
@click.option("-y", "--yes", is_flag=True, help="Install the harness and download model weights without asking.")
@click.option("--force", is_flag=True, help="Overwrite an existing harness.meta.toml.")
def meta_init_cmd(harness: str | None, provider: str | None, model: str | None, local: bool, runtime: str | None,
                  memory_gb: float | None, base_url: str | None, yes: bool, force: bool) -> None:
    """Set up the meta harness: choose it, give it a model, and pin it.

    Writes [path]harness.meta.toml[/], configures the harness in its own config root
    under [path].hlock/env/[/] (never your own config), and pins it in
    [path]harness.meta.lock[/]. API keys stay in your environment; only the variable's
    name is written.
    """
    mm = Path(_mm())
    if mm.exists() and not force:
        _fail(f"{_show(mm)} already exists", "pass [cmd]--force[/] to start over, or [cmd]hlock meta pin[/] to re-pin it")
    with _working("Looking for harnesses, API keys and local runtimes…"):
        res = json.loads(meta_init(harness=harness, provider="local" if local and not provider else provider, model=model,
                                   runtime=runtime, memory_gb=memory_gb, base_url=base_url, manifest=_m()))
    c = res["choice"]
    if force:
        import shutil

        shutil.rmtree(_meta_prefix(), ignore_errors=True)  # start over: the meta root is rebuilt from [model]
    mm.write_text(res["manifest"])
    _ok(f"wrote [path]{_show(mm)}[/]")
    t = _kv_table()
    t.add_row("harness", f"[bold]{c['harness_name']}[/]  [muted]{c['why']}[/]")
    t.add_row("model", f"{_model_text(c['model'])}  [muted]{c['model_why']}[/]")
    key = _key_text(c["model"])
    if key:
        t.add_row("api key", key)
    console.print(t)

    if not c["installed"]:
        if c.get("install") and _confirm(f"Install {c['harness_name']} now?", yes):
            c["installed"] = _run_shell(c["install"], f"installing {c['harness_name']}")
        elif not c.get("install"):
            console.print(f"[warn]![/] {c['harness_name']} isn't installed; see [cmd]hlock harness show {c['harness']}[/]")
    local_choice = c.get("local") or {}
    if local_choice and not local_choice.get("installed"):
        console.print(f"[warn]![/] {local_choice['runtime_name']} isn't installed; install it to serve {c['model']['name']}")
    elif local_choice.get("pull") and _confirm(f"Download {c['model']['name']} with {local_choice['runtime_name']} now?", yes):
        _run_shell(local_choice["pull"], f"downloading {c['model']['name']}")

    with _working("Pinning the meta harness…"):
        out = json.loads(meta_pin(out=_ml(), meta_manifest=str(mm), manifest=_m(), force=force))
    _ok(f"pinned [bold]{out['lock']['harness']['name']}[/] {out['lock']['harness'].get('version') or ''} → [path]{_show(_ml())}[/]")
    for w in out["warnings"]:
        console.print(f"[warn]![/] {w}")

    todo = []
    if not c["installed"]:
        todo.append(c.get("install") or f"install {c['harness_name']}")
    if c["model"].get("key-env") and not os.environ.get(c["model"]["key-env"]):
        todo.append(f"export {c['model']['key-env']}=…")
    if local_choice.get("pull") and not yes:
        todo.append(local_choice["pull"])
    if todo:
        console.print("[bold]Before the first job[/]")
        for step in todo:
            console.print(f"  [cmd]{step}[/]")
        if not c["installed"]:
            console.print("  [muted]then re-pin with[/] [cmd]hlock meta pin[/] [muted]to record its version[/]")
    _next("hlock meta run install", "hlock meta jobs")


@meta.command("pin")
@click.option("--no-vendor", is_flag=True, help="Don't copy unfetchable skills and config into .hlock/vendor/meta.")
@click.option("--force", is_flag=True, help="Replace a meta lock that can't be read.")
@click.option("--json", "as_json", is_flag=True, help="Print the result as JSON.")
def meta_pin_cmd(no_vendor: bool, force: bool, as_json: bool) -> None:
    """Pin the meta harness into [path]harness.meta.lock[/].

    Applies [path]harness.meta.toml[/]'s [key]\\[model][/] to the meta harness's own
    config, then captures that config: version, model, MCP servers, skills, plugins.
    """
    with _working("Pinning the meta harness…"):
        out = json.loads(meta_pin(out=_ml(), meta_manifest=_mm(), vendor=not no_vendor, force=force, manifest=_m()))
    if as_json:
        console.print_json(data=out)
        return
    lock = out["lock"]
    _ok(f"pinned [bold]{lock['harness']['name']}[/] {lock['harness'].get('version') or ''} → [path]{_show(_ml())}[/]")
    for w in out["warnings"]:
        console.print(f"[warn]![/] {w}")
    console.print(_pin_panel(lock, f"{_show(_ml())} · meta"))
    if out["vendored"]:
        console.print(f"[muted]vendored {len(out['vendored'])} files into[/] "
                      f"[path]{_vendor_dir(out['vendored'], lock['harness']['name'])}[/]")
    if out["previous"]:
        for line in out["diff"] or []:
            style = {"+": "ok", "-": "fail", "~": "warn"}.get(line[:1], "muted")
            console.print(f"  [{style}]{line}[/]")
        if not out["diff"]:
            console.print("[muted]no changes since the last pin[/]")
    _next("hlock meta check", "hlock meta run install")


@meta.command("check")
def meta_check_cmd() -> None:
    """Fail if the meta harness drifted from [path]harness.meta.lock[/] (CI-friendly).

    Checks that the lock is for [path]harness.meta.toml[/]'s harness and model, and that
    the meta harness's own config still matches it.
    """
    _need_meta_lock()
    with _working("Checking the meta harness…"):
        drift = json.loads(meta_check(_ml(), meta_manifest=_mm(), manifest=_m()))
    if drift:
        err_console.print(f"[fail]✗ drift:[/] the meta harness doesn't match {_show(_ml())}")
        for line in drift:
            err_console.print(f"  {escape(line)}")
        err_console.print("  [muted]hint:[/] re-pin with [cmd]hlock meta pin[/], or restore it with [cmd]hlock meta sync[/]")
        sys.exit(1)
    model = _meta_cfg()["model"]
    _ok(f"the meta harness matches [path]{_show(_ml())}[/]")
    key = _key_text(model)
    if key and not os.environ.get(model["key-env"]):
        console.print(f"[warn]![/] {key}")


@meta.command("show")
@click.option("--json", "as_json", is_flag=True, help="Print the meta lock as JSON.")
def meta_show_cmd(as_json: bool) -> None:
    """Show the pinned meta harness."""
    _need_meta_lock()
    with _friendly_errors():
        lf = Lockfile.load(_ml())
    if as_json:
        console.print_json(lf.to_json())
        return
    lock = json.loads(lf.to_json())
    console.print(_pin_panel(lock, f"{_show(_ml())} · meta"))
    if _meta_is_builtin():
        pol = lock.get("policy") or {}
        console.print(f"[key]allowed[/] {', '.join(pol.get('tool-allowlist') or [])}  "
                      f"[muted](without asking; at most {pol.get('max-steps')} steps a job)[/]")
    key = _key_text(_meta_cfg()["model"])
    if key:
        console.print(f"[key]api key[/] {key}")


@meta.command("sync", context_settings={"ignore_unknown_options": True})
@click.option("--plan", "plan_only", is_flag=True, help="Print the plan and change nothing.")
@click.option("--json", "as_json", is_flag=True, help="Print the plan as JSON and change nothing.")
@click.option("-y", "--yes", is_flag=True, help="Don't ask before running automatic steps.")
@click.option("--install", is_flag=True, help="Let hlock run package managers to install the meta harness.")
@click.option("--pull", is_flag=True, help="Let hlock download model weights through installed runtimes.")
@click.option("-v", "--verbose", is_flag=True, help="List every step, including optional ones.")
@click.pass_context
def meta_sync_cmd(ctx, plan_only: bool, as_json: bool, yes: bool, install: bool, pull: bool, verbose: bool) -> None:
    """Rehydrate the meta harness from [path]harness.meta.lock[/] on this machine.

    Like [cmd]hlock sync[/], into the meta harness's own config root.
    """
    _need_meta_lock()
    if _meta_is_builtin():
        _ok("the built-in agent ships with hlock; there's nothing to install or restore")
        return
    ctx.invoke(sync, lock=_ml(), plan_only=plan_only, as_json=as_json, yes=yes, install=install, pull=pull,
               prefix=_meta_prefix(), verbose=verbose)


@meta.command("launch", context_settings={"ignore_unknown_options": True, "allow_interspersed_args": False})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
@click.pass_context
def meta_launch_cmd(ctx, args: tuple[str, ...]) -> None:
    """Start an interactive session in the locked meta harness.

    Arguments go to the harness, as with [cmd]hlock launch[/]. Use it to sign in once,
    or to configure the meta harness before [cmd]hlock meta pin[/].
    """
    _need_meta_lock()
    if _meta_is_builtin():
        _fail("the built-in agent runs jobs rather than open sessions",
              "run one with [cmd]hlock meta run <job>[/] (interactive by default)")
    if "--in-place" in args or (args and _known_harness(args[0])):
        _fail("the meta harness always runs from its own lock and config root",
              "drop --in-place and the harness name; change the harness with [cmd]hlock meta init --force[/]")
    key = _key_text(_meta_cfg()["model"])
    if key and "isn't set" in key:
        console.print(f"[warn]![/] {key}")
    ctx.invoke(launch, args=args, lock=_ml())


@meta.command("jobs")
@click.option("--json", "as_json", is_flag=True, help="Print the jobs as JSON.")
def meta_jobs_cmd(as_json: bool) -> None:
    """List the jobs the meta harness can run."""
    jobs = _meta_cfg()["jobs"]
    if as_json:
        console.print_json(data=jobs)
        return
    t = Table(box=None, header_style="bold", pad_edge=False)
    t.add_column("job", style="key", no_wrap=True)
    t.add_column("")
    t.add_column("description")
    for j in jobs:
        kind = "[muted]built in[/]" if j["builtin"] else ("custom" if j["verifies"] else "custom [muted](unverified)[/]")
        t.add_row(j["name"], kind, j["description"])
    console.print(t)
    _next("hlock meta run <job>")


def _verdict_exit(v: dict) -> int:
    return {"verified": 0, "needs-you": NEEDS_YOU_EXIT}.get(v["status"], 1)


def _print_verdict(v: dict) -> None:
    mark = {"verified": "[ok]✓ verified[/]", "needs-you": "[warn]! needs you[/]", "failed": "[fail]✗ failed[/]",
            "unverified": "[warn]? unverified[/]"}[v["status"]]
    console.print(f"{mark}  {escape(v['summary'])}")
    for line in v.get("details") or []:
        console.print(f"  [muted]{escape(line)}[/]", soft_wrap=True)
    for line in v.get("diff") or []:
        style = {"+": "ok", "-": "fail", "~": "warn"}.get(line[:1], "muted")
        console.print(f"  [{style}]{escape(line)}[/]")
    for name in v.get("secrets") or []:
        console.print(f"  [cmd]export {name}=…[/]")


def _verify(job: str, out: str | None, count: int) -> dict:
    with _working(f"Checking the {job} job…"):
        return json.loads(meta_verify(job, lock=_l(), meta_lock=_ml(), meta_manifest=_mm(), manifest=_m(),
                                      out=out, count=count))


@meta.command("run", context_settings={"ignore_unknown_options": True})
@click.argument("job")
@click.argument("instructions", nargs=-1)
@click.option("--headless/--interactive", "headless", default=None,
              help="Run non-interactively, or as a session you watch. [dim]\\[default: \\[run] mode, else interactive][/]")
@click.option("--out", default=None, help="bootstrap-evals: the pack directory. [dim]\\[default: the manifest's first eval pack, else ./evals][/]")
@click.option("--count", default=10, show_default=True, help="bootstrap-evals: how many tasks to aim for.")
@click.option("-t", "--template", default="coding", show_default=True,
              type=click.Choice([t for t in _evals.TEMPLATES if t != "hello-world"]),
              help="bootstrap-evals: the template to scaffold when there's no pack yet.")
@click.option("--pin", "repin", is_flag=True, help="update: re-pin the project's lock when something changed.")
@click.option("--allow-version-mismatch", is_flag=True, help="Run even if the installed meta harness differs from its lock.")
@click.option("--dry-run", is_flag=True, help="Print the command and the prompt, and run nothing.")
@click.option("--json", "as_json", is_flag=True, help="With --dry-run: print the run plan as JSON.")
def meta_run_cmd(job: str, instructions: tuple[str, ...], headless: bool | None, out: str | None, count: int,
                 template: str, repin: bool, allow_version_mismatch: bool, dry_run: bool, as_json: bool) -> None:
    """Run JOB in the meta harness, then check the result.

    hlock renders the job's prompt from the project's state, runs the locked meta
    harness on it, and verifies the outcome itself. Extra words are added to the
    prompt as instructions: [cmd]hlock meta run install "use brew"[/]. Exits 0 when
    verified, 3 when only you can finish (for example, secrets to set), else 1.
    """
    _need_meta_lock()
    started = datetime.now(timezone.utc)
    run_dir = _Project.lock.parent / ".hlock" / "meta" / "runs" / f"{job}-{started.strftime('%Y%m%dT%H%M%SZ')}"
    run_dir.mkdir(parents=True, exist_ok=True)
    ignore = run_dir.parent.parent / ".gitignore"
    if not ignore.exists():
        ignore.write_text("# hlock meta run records (prompts and agent output); not for version control\n*\n")
    guide = None
    if job == "bootstrap-evals":
        guide = _scaffold_evals(Path(out) if out else _default_pack_dir(), template, count, run_dir, dry_run, as_json)
    with _working(f"Preparing the {job} job…"):
        prep = json.loads(meta_prepare(job, lock=_l(), meta_lock=_ml(), meta_manifest=_mm(), manifest=_m(), out=out,
                                       count=count, instructions=" ".join(instructions),
                                       guide=str(guide) if guide else None, headless=headless,
                                       allow_version_mismatch=allow_version_mismatch, dry_run=dry_run))
    secret_env = prep.pop("secret_env", {})  # values for the config's references; never shown
    prompt_file = run_dir / "prompt.md"
    prompt_file.write_text(prep["prompt"])
    if dry_run and as_json:
        console.print_json(data={**prep, "prompt_file": str(prompt_file)})
        return
    for line in prep["notes"]:
        console.print(f"[warn]![/] {escape(line)}")
    shown = " ".join(a if a != prep["prompt"] else f"<{_show(prompt_file)}>" for a in prep["argv"])
    env_txt = " ".join(f"{k}={_rel(v)}" for k, v in prep["env"].items() if not k.startswith(("HLOCK_", "PYTHON")))
    console.print(f"[brand]▶[/] {prep['harness_name']} · {job} · {prep['mode']}  [muted]{env_txt}[/] [cmd]{shown}[/]",
                  soft_wrap=True)
    if dry_run:
        console.print(Panel(Text(prep["prompt"]), title=f"[brand]{_show(prompt_file)}[/]", title_align="left",
                            border_style="magenta"))
        return
    if prep["paste"]:
        console.print(f"[warn]![/] {prep['harness_name']} can't take a first message; when it starts, tell it:")
        console.print(f"  [cmd]Read {_show(prompt_file)} and do the job it describes.[/]", soft_wrap=True)

    env = {**os.environ, **prep["env"], **secret_env}
    # the agent runs `hlock meta verify`: make it the same hlock, however this one was installed
    exe = Path(sys.argv[0])
    dirs = [str(exe.resolve().parent)] if exe.name in ("hlock", "harness-lock") and exe.is_file() else []
    if (Path(sys.executable).parent / "hlock").exists():
        dirs.append(str(Path(sys.executable).parent))
    if dirs:
        env["PATH"] = os.pathsep.join([*dict.fromkeys(dirs), env.get("PATH", "")])
    code = None
    sys.stdout.flush()
    try:
        if prep["mode"] == "headless":
            with open(run_dir / "output.log", "w") as log:
                proc = subprocess.Popen(prep["argv"], env=env, cwd=_Project.lock.parent, stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, text=True)
                for line in proc.stdout:
                    log.write(line)
                    click.echo(line, nl=False)
                code = proc.wait()
        else:
            code = subprocess.run(prep["argv"], env=env, cwd=_Project.lock.parent, check=False).returncode
    except KeyboardInterrupt:
        console.print("[muted]interrupted; checking what got done[/]")
    except OSError as e:
        _fail(f"cannot start {prep['argv'][0]}: {e}")

    v = _verify(job, out, count)
    if v["status"] == "unverified" and code not in (None, 0):
        v["status"] = "failed"
        v["summary"] = f"{prep['harness_name']} exited with {code}"
    if job == "update" and repin and v.get("diff"):
        with _working("Re-pinning the project's lock…"):
            lf = Lockfile.load(_l())
            _pin(harness=lf.harness, out=_l(), vendor=True, force=False, manifest=_m())
        v["details"] = [*(v.get("details") or []), f"re-pinned {_ld()}"]
    (run_dir / "result.json").write_text(json.dumps({
        "job": job, "harness": prep["harness"], "harness_version": prep.get("harness_version"), "mode": prep["mode"],
        "argv": [a if a != prep["prompt"] else str(prompt_file) for a in prep["argv"]], "exit_code": code,
        "verdict": v, "started": started.isoformat(),
        "finished": datetime.now(timezone.utc).isoformat(),
    }, indent=2) + "\n")
    _print_verdict(v)
    console.print(f"[muted]run record:[/] [path]{_show(run_dir)}[/]")
    if job == "update" and v.get("diff") and not repin:
        _next("hlock pin", "hlock check")
    sys.exit(_verdict_exit(v))


@meta.command("agent", hidden=True)
@click.argument("prompt")
@click.option("--meta-lock", "meta_lock", default=None, help="The meta lock pinning the built-in agent.")
@click.option("--headless/--interactive", "headless", default=False, help="Refuse commands that aren't allowed, or ask.")
def meta_agent_cmd(prompt: str, meta_lock: str | None, headless: bool) -> None:
    """Run hlock's built-in agent on PROMPT (what [cmd]hlock meta run[/] starts)."""
    interactive = not headless and sys.stdin.isatty()

    def say(kind: str, text: str) -> bool:
        if kind == "text":
            console.print(Text(text))
        elif kind == "tool":
            console.print(Text.assemble(("→ ", "brand"), (text, "cmd")), soft_wrap=True)
        elif kind == "result":
            console.print(Text("  " + text, style="muted"), soft_wrap=True)
        elif kind == "denied":
            console.print(Text.assemble(("  ✗ refused ", "warn"), text), soft_wrap=True)
        else:
            err_console.print(Text.assemble(("✗ ", "fail"), text), soft_wrap=True)
        return True

    def ask(command: str) -> str:
        console.print(Text.assemble(("? ", "warn"), "the agent wants to run ", (command, "cmd")), soft_wrap=True)
        try:
            return click.prompt("  run it? (y)es, (n)o, (a)lways for this program", default="y",
                                type=click.Choice(["y", "n", "a"]), show_choices=False)
        except (click.Abort, EOFError):
            return "n"

    with _friendly_errors():
        out = json.loads(agent_run(prompt, meta_lock=meta_lock or _ml(), interactive=interactive, say=say, ask=ask,
                                   project=str(Path.cwd())))
    tokens = out["input_tokens"] + out["output_tokens"]
    usage = f"{out['steps']} steps" + (f", {tokens:,} tokens" if tokens else "")
    if out["status"] == "done":
        console.print(f"[muted]built-in agent finished ({usage})[/]")
        return
    reason = {"max-steps": "it hit its step limit", "stopped": "it was stopped"}.get(out["status"], out.get("error") or "it failed")
    err_console.print(f"[fail]✗[/] the built-in agent didn't finish: {reason} [muted]({usage})[/]")
    sys.exit(1)


def _default_pack_dir() -> Path:
    """The manifest's first eval pack, else ./evals next to the project's lock."""
    try:
        packs = Manifest.load(_m()).eval_packs if Path(_m()).exists() else []
    except ValueError:
        packs = []
    if packs:
        p = Path(packs[0])
        return p if p.is_absolute() else Path(_m()).parent / p
    return _Project.lock.parent / "evals"


def _scaffold_evals(pack_dir: Path, template: str, count: int, run_dir: Path, dry_run: bool, quiet: bool) -> Path:
    """Write the template's slots when there's no pack yet (not on a dry run), and the
    template's guide for the agent into the run record. Returns the guide's path."""
    tpl = _evals.TEMPLATES[template]
    if not (pack_dir / "pack.toml").exists():
        if dry_run and not quiet:
            console.print(f"[muted]dry run: a real run first writes the {template} template ({count} task slots) "
                          f"to {_show(pack_dir)}, and the prompt asks the agent to fill it in[/]")
        elif not dry_run:
            with _friendly_errors():
                written = _evals.init(pack_dir, template, n_tasks=count)
            _ok(f"wrote [path]{_show(written[0])}[/]  [muted]({template} template, {count} task slots)[/]")
    guide = run_dir / "guide.md"
    guide.write_text(_evals.skill(tpl, _show(pack_dir)))
    return guide


@meta.command("verify")
@click.argument("job")
@click.option("--out", default=None, help="bootstrap-evals: the pack directory.")
@click.option("--count", default=0, help="bootstrap-evals: the number of tasks expected.")
@click.option("--json", "as_json", is_flag=True, help="Print the verdict as JSON.")
def meta_verify_cmd(job: str, out: str | None, count: int, as_json: bool) -> None:
    """Run only JOB's check: what [cmd]hlock meta run[/] does after the agent finishes.

    The meta harness runs it to know when it's done; CI can run it too.
    """
    v = _verify(job, out, count)
    if as_json:
        console.print_json(data=v)
    else:
        _print_verdict(v)
    sys.exit(_verdict_exit(v))


def _default_manifest(name: str, pack: str, target: str | None = None) -> str:
    target_block = (
        f'[target]\nharness = "{target}"            # the harness this lock is for\n'
        if target
        else '# [target]\n# harness = "claude-code"      # the harness this lock is for (see: hlock harness list)\n'
    )
    return f"""# hlock manifest — declare what the resolver may try and what 'best' means.
[project]
name = "{name}"

{target_block}
[evals]
packs = ["{pack}"]
seeds = 2

[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]

[candidates]
decision = ["none", "rules:default"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"

[objective.constraints]
success-rate = ">=0.5"

[budget]
strategy = "grid"
max_trials = 200

[backend]
target = "local"
"""


if __name__ == "__main__":  # pragma: no cover
    main()
