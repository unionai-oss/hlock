"""Scaffold eval packs: a runnable hello-world pack, and fill-in templates.

A template writes a pack whose tasks are slots marked ``TODO(hlock)``, plus an Agent
Skill that tells the user's own agent how to fill them from local context (the repo,
its history, the issue tracker). ``hlock evals check`` reports the slots still open.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path

PLACEHOLDER = "TODO(hlock)"  # same marker as harness_lock_core::tasks::PLACEHOLDER
SKILL_NAME = "hlock-eval-pack"


@dataclass(frozen=True)
class Slot:
    """One kind of task in a template."""

    kind: str  # id prefix and tag, e.g. "bug_fix"
    title: str  # what the slot asks for, shown in prompt.md
    guide: str  # how to pick and write a real task of this kind
    artifact: str  # default expected artifact
    tools: tuple[str, ...] = ("filesystem",)
    fixtures: bool = True
    timeout: int = 600


@dataclass(frozen=True)
class Template:
    name: str
    summary: str
    domain: str
    sources: tuple[str, ...]  # where the agent should look for real tasks
    slots: tuple[Slot, ...] = field(default_factory=tuple)


_CODE_TOOLS = ("shell", "filesystem")

TEMPLATES: dict[str, Template] = {
    t.name: t
    for t in [
        Template(
            "hello-world",
            "A complete three-task pack that runs as is. Start here to see the loop work.",
            "hello-world",
            (),
        ),
        Template(
            "coding",
            "Software work in a codebase: bug fixes, features, tests, refactors, debugging.",
            "coding",
            (
                "`git log` for recent fixes and features, with the diff that resolved each",
                "merged pull requests and closed issues (`gh pr list --state merged`, `gh issue list --state closed`)",
                "`TODO` and `FIXME` comments, and flaky or failing tests in CI",
            ),
            (
                Slot("bug_fix", "Fix a real bug",
                     "Pick a bug that was fixed recently. Put the code as it was *before* the fix in "
                     "`workspace/`, describe the symptom the way the reporter did, and ask for the fix "
                     "as a patch.", "fix.patch", _CODE_TOOLS, timeout=900),
                Slot("feature", "Add a small feature",
                     "Pick a small feature that was merged recently. Snapshot the code before it landed, "
                     "and write the request the way the issue or ticket put it.", "feature.patch",
                     _CODE_TOOLS, timeout=900),
                Slot("tests", "Write tests for existing code",
                     "Pick a module with thin test coverage. Ask for tests that pin its current "
                     "behavior, written to a file the grader can find.", "test_added.py", _CODE_TOOLS),
                Slot("refactor", "Refactor without changing behavior",
                     "Pick code someone has complained about or marked with a TODO. Say what should "
                     "change and what must stay the same.", "refactor.patch", _CODE_TOOLS, timeout=900),
                Slot("debug", "Find the root cause of a failure",
                     "Pick a failure with a known cause: a stack trace, a failing CI log, a wrong "
                     "output. Include the evidence in `workspace/` and ask for the root cause in "
                     "writing.", "root_cause.md", _CODE_TOOLS),
            ),
        ),
        Template(
            "data",
            "Data work: cleaning, analysis, transformation, queries, and charts.",
            "data-analysis",
            (
                "notebooks and analysis scripts in the repo, and the data files they read",
                "recent reports or dashboards someone asked for, and the questions behind them",
                "SQL in the repo or shell history, and the tables it touches",
            ),
            (
                Slot("clean", "Clean a messy dataset",
                     "Take a small, real sample of a dataset that needed cleaning (a few hundred rows "
                     "is plenty), with the problems it actually had. Remove anything sensitive.",
                     "cleaned.csv", ("python", "filesystem")),
                Slot("analyze", "Answer a question from data",
                     "Pick a question someone asked of this data and that was answered. Include the "
                     "data and ask for the answer with the numbers behind it.", "analysis.md",
                     ("python", "filesystem")),
                Slot("transform", "Reshape data for a downstream use",
                     "Pick a transformation the project does: a join, a pivot, an aggregation. Say "
                     "the output schema exactly.", "output.csv", ("python", "filesystem")),
                Slot("query", "Write a query",
                     "Pick a query that was written for a real request. Include the schema (and a "
                     "small database file if you can) and ask for the SQL.", "query.sql",
                     ("python", "filesystem")),
                Slot("chart", "Make a chart",
                     "Pick a chart that went into a report. Include the data and say what the chart "
                     "must show.", "chart.png", ("python", "filesystem")),
            ),
        ),
        Template(
            "research",
            "Reading and writing: summaries, comparisons, answers with sources, memos.",
            "research-analyst",
            (
                "documents, notes, and reports in the project (`docs/`, `notes/`, PDFs)",
                "questions that came up in issues, design docs, or meeting notes",
                "memos or summaries someone wrote by hand that an agent could have drafted",
            ),
            (
                Slot("summarize", "Summarize a document",
                     "Pick a real document and the audience the summary was for. Say how long the "
                     "summary should be.", "summary.md"),
                Slot("compare", "Compare options",
                     "Pick a decision that was made between a few options (tools, vendors, designs). "
                     "Include the material that was available and ask for a comparison.",
                     "comparison.md"),
                Slot("answer", "Answer a question with sources",
                     "Pick a question that was answered from the documents in `workspace/`. Ask for "
                     "an answer that cites which document each claim came from.", "answer.md"),
                Slot("extract", "Extract structured facts",
                     "Pick documents that someone pulled facts out of (dates, amounts, names). Give "
                     "the exact JSON shape to produce.", "extracted.json"),
                Slot("memo", "Draft a memo or brief",
                     "Pick a memo that was written for a real reader. Include the inputs it drew on.",
                     "memo.md"),
            ),
        ),
        Template(
            "support",
            "Support work: triage, replies, reproductions, escalations, help articles.",
            "support-triage",
            (
                "exported or closed tickets, and the issue tracker (`gh issue list --state closed`)",
                "saved replies, macros, and the help center or FAQ in the repo",
                "escalations to engineering and the bug reports they became",
            ),
            (
                Slot("triage", "Triage a ticket",
                     "Pick a closed ticket. Include the customer's message, with names, emails and "
                     "account ids replaced. Give the exact fields to fill in (severity, category, team).",
                     "triage.json"),
                Slot("reply", "Draft a reply",
                     "Pick a ticket that got a good reply. Include the ticket and the help articles "
                     "the reply relied on.", "reply.md"),
                Slot("reproduce", "Reproduce a reported bug",
                     "Pick a report that was reproduced. Include what the customer sent and ask for "
                     "minimal reproduction steps.", "repro.md", ("shell", "filesystem")),
                Slot("escalate", "Write an escalation",
                     "Pick a ticket that went to engineering. Ask for the summary an engineer needs to "
                     "act on it.", "escalation.md"),
                Slot("article", "Write a help article",
                     "Pick a question customers ask often. Include the tickets that asked it.",
                     "article.md"),
            ),
        ),
        Template(
            "blank",
            "Generic slots for any domain. Use it when no other template fits.",
            "general",
            (
                "the work you hand to your agent most often, and where its inputs come from",
                "recent tasks that had a clear, checkable result",
            ),
            (
                Slot("task", "A task from your own work",
                     "Pick a task you or your team did recently and would hand to an agent. Include "
                     "the inputs it needs and say what file it should produce.", "output.md"),
            ),
        ),
    ]
}


def templates() -> list[Template]:
    return list(TEMPLATES.values())


def _toml_list(items) -> str:
    return "[" + ", ".join(f'"{i}"' for i in items) + "]"


def _header(name: str, domain: str, comment: str) -> str:
    return f"""# {comment}
# Format: https://docs.hlock.ai/guides/eval-packs/
[pack]
name = "{name}"
version = "0.1.0"        # bump it whenever the tasks change
domain = "{domain}"
"""


HELLO_TASKS = [
    {
        "id": "hello_file",
        "prompt": "# Say hello\n\nCreate a file named `hello.txt` containing exactly `Hello, world!`.\n",
        "fixtures": {},
        "artifact": "hello.txt",
        "tags": ["warm-up"],
    },
    {
        "id": "count_words",
        "prompt": "# Count the words\n\n`words.txt` holds a short paragraph. Count its words and write the "
                  "number, and nothing else, to `count.txt`.\n",
        "fixtures": {"words.txt": "The quick brown fox jumps over the lazy dog.\n"},
        "artifact": "count.txt",
        "tags": ["files"],
    },
    {
        "id": "greet_people",
        "prompt": "# Greet everyone\n\n`names.csv` lists people under a `name` column. Write `greetings.md` "
                  "with one line per person: `Hello, <name>!`, in the order they appear.\n",
        "fixtures": {"names.csv": "name\nAda\nGrace\nLinus\n"},
        "artifact": "greetings.md",
        "tags": ["files"],
    },
]


def _hello_world(name: str) -> dict[str, str]:
    files = {}
    blocks = [_header(name, "hello-world", "A hello-world eval pack written by `hlock evals init`.")]
    for t in HELLO_TASKS:
        lines = [f'\n[[task]]\nid = "{t["id"]}"', f'prompt = "{t["id"]}/prompt.md"']
        if t["fixtures"]:
            lines.append(f'fixtures = "{t["id"]}/workspace/"')
        lines += ['tools = ["filesystem"]', f'expected = {{ artifacts = ["{t["artifact"]}"] }}',
                  f"tags = {_toml_list(t['tags'])}", "timeout-seconds = 300"]
        blocks.append("\n".join(lines) + "\n")
        files[f"{t['id']}/prompt.md"] = t["prompt"]
        for fname, text in t["fixtures"].items():
            files[f"{t['id']}/workspace/{fname}"] = text
    files["pack.toml"] = "".join(blocks)
    return files


def _slot_prompt(slot: Slot) -> str:
    fixtures = (
        "Put the files the task starts from in `workspace/` next to this prompt. Copy only what the "
        "task needs, and nothing secret.\n\n"
        if slot.fixtures
        else ""
    )
    return f"""<!-- {PLACEHOLDER}: replace this whole file with a real task. -->
# {PLACEHOLDER}: {slot.title}

{slot.guide}

{fixtures}Write the prompt the way you'd ask your agent for this, in your own words. End by naming
the file a successful run leaves in the workspace, for example: "Write the result to
`{slot.artifact}`." Keep that name in sync with `expected` in pack.toml.
"""


def _template_files(tpl: Template, name: str, n_tasks: int) -> dict[str, str]:
    files = {}
    blocks = [_header(name, tpl.domain, f"A `{tpl.name}` eval pack template written by `hlock evals init`.\n"
                                         "# Fill in the marked slots, then run `hlock evals check`.")]
    counts: dict[str, int] = {}
    for i in range(n_tasks):
        slot = tpl.slots[i % len(tpl.slots)]
        counts[slot.kind] = counts.get(slot.kind, 0) + 1
        tid = f"{slot.kind}_{counts[slot.kind]}"
        lines = [
            "",
            "[[task]]",
            f"# {PLACEHOLDER}: {slot.title.lower()}. Rename the id to say what it is, e.g. \"{slot.kind}_<topic>\".",
            f'id = "{tid}"',
            f'prompt = "{tid}/prompt.md"',
        ]
        if slot.fixtures:
            lines.append(f'fixtures = "{tid}/workspace/"')
            files[f"{tid}/workspace/.gitkeep"] = ""
        lines += [
            f"tools = {_toml_list(slot.tools)}",
            f"# {PLACEHOLDER}: the file a successful run leaves in the workspace.",
            f'expected = {{ artifacts = ["{slot.artifact}"] }}',
            f'tags = ["{slot.kind.replace("_", "-")}"]',
            f"timeout-seconds = {slot.timeout}",
        ]
        blocks.append("\n".join(lines) + "\n")
        files[f"{tid}/prompt.md"] = _slot_prompt(slot)
    files["pack.toml"] = "".join(blocks)
    return files


def skill(tpl: Template, pack_dir: str) -> str:
    """An Agent Skill that guides an agent through filling in a template."""
    sources = "\n".join(f"  - {s}" for s in tpl.sources)
    kinds = "\n".join(f"- **{s.title}** (`{s.kind}_*`): {s.guide}" for s in tpl.slots)
    return f"""---
name: {SKILL_NAME}
description: Fill in the hlock eval pack template in {pack_dir} with real tasks from this project. Use when the user asks to write, fill in, or finish their eval pack, or to create tasks for hlock.
---

# Fill in the eval pack in `{pack_dir}`

`hlock evals init --template {tpl.name}` wrote an eval pack template to `{pack_dir}`. hlock runs every
candidate agent stack against these tasks and keeps the one that does best, so the tasks
should look like the work this user actually hands to an agent. Your job is to replace each
`{PLACEHOLDER}` slot with a real task, drawn from what you can see in this project.

## Rules

- Draw tasks from real work, not invented examples. Look in:
{sources}
- Before writing anything, propose a list of tasks to the user: one line each, with where it
  came from. Write them only after the user agrees.
- Copy only the files a task needs into its `workspace/`. Never copy secrets, credentials,
  `.env` files, or personal data; replace names, emails, and ids with placeholders.
- Every task must produce a file a program can check: its name goes in `expected.artifacts`
  in `pack.toml` and at the end of `prompt.md`. The file must not already be in `workspace/`.
- Keep each task id unique and in `snake_case`, and keep the task's directory named after it:
  rename the directory and the `prompt` and `fixtures` paths together.
- Drop slots that don't fit this project and add tasks of other kinds when they do. Aim for
  at least 10 tasks, so hlock can hold some back to check the winner.

## Task kinds in this template

{kinds}

## Steps

1. Read `{pack_dir}/pack.toml` and the `prompt.md` files to see the slots.
2. Look through the sources above and propose tasks to the user.
3. For each agreed task: write `prompt.md`, fill `workspace/`, and update its `[[task]]` entry.
   Remove every `{PLACEHOLDER}` comment as you go.
4. Run `hlock evals check {pack_dir}` and fix what it reports until it shows no errors.
5. Tell the user the pack is ready, and that `hlock init --pack {pack_dir}` (or the `[evals]`
   section of an existing `harness.toml`) points hlock at it.
"""


def init(dest: str | Path, template: str = "hello-world", name: str | None = None, n_tasks: int = 10,
         force: bool = False) -> list[Path]:
    """Write an eval pack to ``dest``. Returns the files written, ``pack.toml`` first.

    Raises ``ValueError`` for an unknown template, or if ``dest`` already has a
    ``pack.toml`` and ``force`` is false.
    """
    tpl = TEMPLATES.get(template)
    if tpl is None:
        raise ValueError(f"unknown eval pack template {template!r}; choose one of {', '.join(TEMPLATES)}")
    dest = Path(dest)
    if (dest / "pack.toml").exists() and not force:
        raise ValueError(f"{dest / 'pack.toml'} already exists")
    if n_tasks < 1:
        raise ValueError("an eval pack needs at least one task")
    name = name or f"{Path.cwd().name}/{dest.resolve().name}"
    files = _hello_world(name) if template == "hello-world" else _template_files(tpl, name, n_tasks)
    written = []
    for rel in ["pack.toml", *sorted(k for k in files if k != "pack.toml")]:
        p = dest / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(files[rel])
        written.append(p)
    return written


# --------------------------------------------------------------------------- #
# Drafting a pack from traces
# --------------------------------------------------------------------------- #

_STOP = {"a", "an", "the", "to", "in", "of", "for", "and", "or", "on", "with", "my", "our", "this", "that",
         "please", "can", "you", "could", "would", "i", "we", "it", "is", "be", "me"}


def _slug(text: str, taken: set[str]) -> str:
    words = [w for w in re.findall(r"[a-z0-9]+", text.lower()) if w not in _STOP][:5] or ["task"]
    base = "_".join(words)[:48].strip("_")
    base = base if base[0].isalpha() else f"task_{base}"
    tid, n = base, 2
    while tid in taken:
        tid, n = f"{base}_{n}", n + 1
    taken.add(tid)
    return tid


def _artifacts(session, project: Path) -> list[str]:
    """Files the session created inside the project, relative to where it ran."""
    base = Path(session.cwd).resolve() if session.cwd else project
    out = []
    for f in session.created:
        p = Path(f)
        p = p.resolve() if p.is_absolute() else (base / p).resolve()
        try:
            rel = p.relative_to(base).as_posix()
        except ValueError:
            continue  # outside the project: not the task's output
        if not rel.startswith((".git/", ".hlock/")) and rel not in out:
            out.append(rel)
    return out[:3]


def _toml_str(s: str) -> str:
    return json.dumps(s)  # a JSON string is a valid TOML basic string


def draft_from_traces(dest: str | Path, sessions, name: str | None = None, n_tasks: int = 10,
                      project: Path | None = None, force: bool = False) -> tuple[list[Path], int]:
    """Write a draft eval pack to ``dest`` from trace sessions, newest first, one task per
    distinct request. Prompts are redacted. A task whose session created files expects
    them as artifacts; the rest get a ``TODO(hlock)`` to fill in. Returns the files
    written (``pack.toml`` first) and the number of tasks."""
    from harness_lock import redact_text

    dest, project = Path(dest), (project or Path.cwd()).resolve()
    if (dest / "pack.toml").exists() and not force:
        raise ValueError(f"{dest / 'pack.toml'} already exists")
    seen, picked = set(), []
    for s in sorted(sessions, key=lambda s: s.started, reverse=True):
        key = " ".join(s.prompt.lower().split())
        if key not in seen:
            seen.add(key)
            picked.append(s)
    picked = picked[:n_tasks]
    if not picked:
        raise ValueError("no requests found in the traces")
    name = name or f"{project.name}/{dest.resolve().name}"
    home = str(Path.home())
    files: dict[str, str] = {}
    blocks = [_header(name, "from-traces", "An eval pack drafted from agent traces by `hlock evals init --from-traces`.\n"
                                           "# Review every task and its marked slots, then run `hlock evals check`.")]
    taken: set[str] = set()
    for s in picked:
        prompt = redact_text(s.prompt).replace(home, "~")[:4000]
        tid = _slug(prompt, taken)
        arts = _artifacts(s, project)
        when = s.started[:10] or "unknown date"
        lines = ["", "[[task]]", f'id = "{tid}"', f'prompt = "{tid}/prompt.md"',
                 f'fixtures = "{tid}/workspace/"', 'tools = ["shell", "filesystem"]']
        if arts:
            lines.append(f"expected = {{ artifacts = [{', '.join(_toml_str(a) for a in arts)}] }}  # files the session created")
        else:
            lines += [f"# {PLACEHOLDER}: the file a successful run leaves in the workspace (the session created none).",
                      'expected = { artifacts = ["result.md"] }']
        lines += [f'tags = ["from-traces", "{s.harness}"]', "timeout-seconds = 900"]
        blocks.append("\n".join(lines) + "\n")
        files[f"{tid}/prompt.md"] = (
            f"<!-- drafted from a {s.harness} session ({when}); secrets redacted. Review before use. -->\n{prompt}\n")
        files[f"{tid}/workspace/.gitkeep"] = ""
    files["pack.toml"] = "".join(blocks)
    written = []
    for rel in ["pack.toml", *sorted(k for k in files if k != "pack.toml")]:
        p = dest / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(files[rel])
        written.append(p)
    return written, len(picked)


def trace_skill(pack_dir: str) -> str:
    """An Agent Skill that guides an agent through reviewing a pack drafted from traces."""
    return f"""---
name: {SKILL_NAME}
description: Review and finish the hlock eval pack in {pack_dir}, drafted from this project's agent sessions. Use when the user asks to review, finish, or fix their eval pack.
---

# Finish the eval pack in `{pack_dir}`

`hlock evals init --from-traces` drafted an eval pack in `{pack_dir}` from the requests this
user made to their agent in past sessions. hlock runs every candidate agent stack against
these tasks, so each one must run on its own in an empty workspace and be checkable.

## Rules

- Show the user the drafted tasks, one line each, and ask which to keep. Delete the rest
  (the `[[task]]` entry and its directory).
- A drafted prompt may lean on context from its session ("fix it", "like before"). Rewrite it
  so it stands alone, keeping the user's intent and wording where you can.
- Each task starts from its `workspace/`. Copy in only the files it needs, from this
  project. Never copy secrets, credentials, `.env` files, or personal data; replace names,
  emails, and ids with placeholders. Prompts were redacted automatically: check that
  nothing sensitive is left.
- Every task must produce a file a program can check, named in `expected.artifacts` and at
  the end of `prompt.md`. The file must not already be in `workspace/`. Resolve each
  `{PLACEHOLDER}`.
- Keep ids unique and in `snake_case`, and keep each directory named after its id.
- Aim for at least 10 tasks, so hlock can hold some back to check the winner.

## Steps

1. Read `{pack_dir}/pack.toml` and the `prompt.md` files.
2. Agree with the user on which tasks to keep.
3. For each task: rewrite `prompt.md` to stand alone, fill `workspace/`, and check its
   `[[task]]` entry.
4. Run `hlock evals check {pack_dir}` and fix what it reports until it shows no errors.
5. Tell the user the pack is ready, and that `hlock init --pack {pack_dir}` (or the `[evals]`
   section of an existing `harness.toml`) points hlock at it.
"""
