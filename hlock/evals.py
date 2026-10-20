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
    artifact: str  # default expected artifact; "" when the agent changes files in place
    check: str  # what the grader should look for
    grader: str = "judge"  # "tests" (a check.py script) or "judge" (a rubric.md for a judge model)
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
                     "`workspace/`, describe the symptom the way the reporter did, and ask for the fix. "
                     "The fixed code is its solution.", "", "Run the tests that cover the bug, such as the test added with the fix.", "tests", _CODE_TOOLS, timeout=900),
                Slot("feature", "Add a small feature",
                     "Pick a small feature that was merged recently. Snapshot the code before it landed, "
                     "and write the request the way the issue or ticket put it.", "",
                     "Run tests for the new behavior, such as the ones merged with the feature.", "tests",
                     _CODE_TOOLS, timeout=900),
                Slot("tests", "Write tests for existing code",
                     "Pick a module with thin test coverage. Ask for tests that pin its current "
                     "behavior, written to a file the grader can find.", "test_added.py",
                     "Run the new tests: they should pass on the code as it is. Keep a broken copy of the module "
                     "next to this script and check the tests fail on it.", "tests", _CODE_TOOLS),
                Slot("refactor", "Refactor without changing behavior",
                     "Pick code someone has complained about or marked with a TODO. Say what should "
                     "change and what must stay the same.", "",
                     "Run the existing tests (they must still pass) and check the change you asked for was made.",
                     "tests", _CODE_TOOLS, timeout=900),
                Slot("debug", "Find the root cause of a failure",
                     "Pick a failure with a known cause: a stack trace, a failing CI log, a wrong "
                     "output. Include the evidence in `workspace/` and ask for the root cause in "
                     "writing.", "root_cause.md",
                     "Name the actual root cause, and the evidence in the workspace that shows it.", "judge",
                     _CODE_TOOLS),
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
                     "cleaned.csv",
                     "Compare with a cleaned copy kept next to this script, or check the rules it must follow: "
                     "no duplicate ids, one date format, no empty required fields.", "tests", ("python", "filesystem")),
                Slot("analyze", "Answer a question from data",
                     "Pick a question someone asked of this data and that was answered. Include the "
                     "data and ask for the answer with the numbers behind it.", "analysis.md",
                     "The numbers the answer must contain, and the conclusion it must reach.", "judge",
                     ("python", "filesystem")),
                Slot("transform", "Reshape data for a downstream use",
                     "Pick a transformation the project does: a join, a pivot, an aggregation. Say "
                     "the output schema exactly.", "output.csv",
                     "Check the columns, the row count, and a few rows you know the right values of.", "tests",
                     ("python", "filesystem")),
                Slot("query", "Write a query",
                     "Pick a query that was written for a real request. Include the schema (and a "
                     "small database file if you can) and ask for the SQL.", "query.sql",
                     "Run the query against the database in the workspace and compare its rows with the right "
                     "answer.", "tests", ("python", "filesystem")),
                Slot("chart", "Make a chart",
                     "Pick a chart that went into a report. Include the data and say what the chart "
                     "must show.", "chart.png",
                     "Check the PNG opens and has a sensible size; a judge can't see images.", "tests",
                     ("python", "filesystem")),
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
                     "summary should be.", "summary.md",
                     "The points the summary must keep, what it must leave out, and its length."),
                Slot("compare", "Compare options",
                     "Pick a decision that was made between a few options (tools, vendors, designs). "
                     "Include the material that was available and ask for a comparison.",
                     "comparison.md",
                     "The criteria the comparison must cover, and the facts it must get right about each option."),
                Slot("answer", "Answer a question with sources",
                     "Pick a question that was answered from the documents in `workspace/`. Ask for "
                     "an answer that cites which document each claim came from.", "answer.md",
                     "The answer, and which document each claim must cite."),
                Slot("extract", "Extract structured facts",
                     "Pick documents that someone pulled facts out of (dates, amounts, names). Give "
                     "the exact JSON shape to produce.", "extracted.json",
                     "Load the JSON and compare each field with the right value.", "tests"),
                Slot("memo", "Draft a memo or brief",
                     "Pick a memo that was written for a real reader. Include the inputs it drew on.",
                     "memo.md",
                     "What the reader needed to learn or decide, and the facts the memo must get right."),
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
                     "triage.json",
                     "Load the JSON and compare severity, category, and team with how the ticket was triaged.",
                     "tests"),
                Slot("reply", "Draft a reply",
                     "Pick a ticket that got a good reply. Include the ticket and the help articles "
                     "the reply relied on.", "reply.md",
                     "What the reply must tell the customer, which article it should point to, and its tone."),
                Slot("reproduce", "Reproduce a reported bug",
                     "Pick a report that was reproduced. Include what the customer sent and ask for "
                     "minimal reproduction steps.", "repro.md",
                     "The steps must reproduce the bug from a clean start, and stay minimal.",
                     tools=("shell", "filesystem")),
                Slot("escalate", "Write an escalation",
                     "Pick a ticket that went to engineering. Ask for the summary an engineer needs to "
                     "act on it.", "escalation.md",
                     "The facts an engineer needs: impact, reproduction, what was already tried."),
                Slot("article", "Write a help article",
                     "Pick a question customers ask often. Include the tickets that asked it.",
                     "article.md",
                     "The question it must answer, the steps it must give, and what it must not promise."),
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
                     "the inputs it needs and say what file it should produce.", "output.md",
                     "What a good result must get right, the way you'd review it yourself."),
            ),
        ),
    ]
}


def templates() -> list[Template]:
    return list(TEMPLATES.values())


def _toml_list(items) -> str:
    return "[" + ", ".join(f'"{i}"' for i in items) + "]"


# A decision model: it answers each criterion as a yes/no question, cheaply. A local Kev
# (`ollama:kev:4b`) or an LLM works too.
JUDGE_MODEL = "typesafe:jev-1.13.0"


def _header(name: str, domain: str, comment: str, judge: bool = False) -> str:
    head = f"""# {comment}
# Format: https://docs.hlock.ai/guides/eval-packs/
[pack]
name = "{name}"
version = "0.1.0"        # bump it whenever the tasks change
domain = "{domain}"
"""
    if judge:
        head += f"""
[pack.judge]             # scores judge: graders; reads TYPESAFE_API_KEY (harness.toml [evals.judge] overrides it)
model = "{JUDGE_MODEL}"
"""
    return head


GREET_CHECK = '''"""Grader for greet_people: one `Hello, <name>!` line per name in names.csv, in order.

hlock runs this in the task's workspace after the agent finishes. Exit 0 passes. The
score written to $HLOCK_GRADE gives partial credit for the lines that are right.
"""

import csv
import json
import os
import sys

want = [f"Hello, {row['name']}!" for row in csv.DictReader(open("names.csv"))]
try:
    got = [line.strip() for line in open("greetings.md") if line.strip()]
except FileNotFoundError:
    sys.exit("greetings.md is missing")
right = sum(a == b for a, b in zip(got, want))
detail = "" if got == want else f"{right} of {len(want)} lines right: expected {want}, got {got}"
if "HLOCK_GRADE" in os.environ:
    with open(os.environ["HLOCK_GRADE"], "w") as f:
        json.dump({"score": right / len(want), "detail": detail}, f)
sys.exit(0 if got == want else 1)
'''


HELLO_TASKS = [
    {
        "id": "hello_file",
        "prompt": "# Say hello\n\nCreate a file named `hello.txt` containing exactly `Hello, world!`.\n",
        "fixtures": {},
        "artifact": "hello.txt",
        "grader": '{ type = "artifact", path = "hello.txt", equals = "Hello, world!" }',
        "solution": {"hello.txt": "Hello, world!\n"},
        "tags": ["warm-up"],
    },
    {
        "id": "count_words",
        "prompt": "# Count the words\n\n`words.txt` holds a short paragraph. Count its words and write the "
                  "number, and nothing else, to `count.txt`.\n",
        "fixtures": {"words.txt": "The quick brown fox jumps over the lazy dog.\n"},
        "artifact": "count.txt",
        "grader": '{ type = "artifact", path = "count.txt", equals = "9" }',
        "solution": {"count.txt": "9\n"},
        "tags": ["files"],
    },
    {
        "id": "greet_people",
        "prompt": "# Greet everyone\n\n`names.csv` lists people under a `name` column. Write `greetings.md` "
                  "with one line per person: `Hello, <name>!`, in the order they appear.\n",
        "fixtures": {"names.csv": "name\nAda\nGrace\nLinus\n"},
        "artifact": "greetings.md",
        "grader": '"tests:greet_people/check.py"',
        "files": {"check.py": GREET_CHECK},
        "solution": {"greetings.md": "Hello, Ada!\nHello, Grace!\nHello, Linus!\n"},
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
        lines += [f'solution = "{t["id"]}/solution/"', 'tools = ["filesystem"]',
                  f'expected = {{ artifacts = ["{t["artifact"]}"] }}', f"graders = [{t['grader']}]",
                  f"tags = {_toml_list(t['tags'])}", "timeout-seconds = 300"]
        blocks.append("\n".join(lines) + "\n")
        files[f"{t['id']}/prompt.md"] = t["prompt"]
        for sub, group in [("workspace/", t["fixtures"]), ("solution/", t["solution"]), ("", t.get("files", {}))]:
            for fname, text in group.items():
                files[f"{t['id']}/{sub}{fname}"] = text
    files["pack.toml"] = "".join(blocks)
    return files


def _slot_prompt(slot: Slot) -> str:
    fixtures = (
        "Put the files the task starts from in `workspace/` next to this prompt. Copy only what the "
        "task needs, and nothing secret.\n\n"
        if slot.fixtures
        else ""
    )
    if slot.artifact:
        end = (f"End by naming\nthe file a successful run leaves in the workspace, for example: \"Write the result to\n"
               f"`{slot.artifact}`.\" Keep that name in sync with `expected` in pack.toml.")
    else:
        end = "Say what done\nlooks like, the way you would to a colleague; `check.py` tests exactly that."
    return f"""<!-- {PLACEHOLDER}: replace this whole file with a real task. -->
# {PLACEHOLDER}: {slot.title}

{slot.guide}

{fixtures}Write the prompt the way you'd ask your agent for this, in your own words. {end}
"""


def _slot_check(slot: Slot, tid: str) -> str:
    """A check.py stub for a ``tests`` slot."""
    return f'''"""Grader for {tid}. {PLACEHOLDER}: write the check, then remove this marker.

{slot.check}

hlock runs this script in the task's workspace after the agent finishes, with the files the
run left there. Exit 0 when the task was done right; exit non-zero, with a message, when it
wasn't. For partial credit, write {{"score": 0.5, "detail": "why"}} as JSON to the path in
$HLOCK_GRADE. $HLOCK_PACK_DIR is the pack directory: keep reference data and tests the
agent mustn't see next to this script, not in workspace/.
"""

import sys

sys.exit("{PLACEHOLDER}: check the result")
'''


def _slot_rubric(slot: Slot, tid: str) -> str:
    """A rubric.md stub for a ``judge`` slot."""
    return f"""<!-- {PLACEHOLDER}: replace this whole file with the rubric for {tid}. -->
# Rubric: {slot.title.lower()}

A judge reads the files the run leaves and checks each `- ` item below; a decision model
answers each one as a yes/no question, and the score is the share met (0.7 passes). Write
one concrete point per item, as you'd review it yourself: {slot.check.lower()}

To also place the result on a scale, add a `## Levels` section with a numbered list,
worst first (`0. …`, `1. …`).

- {PLACEHOLDER}: the first thing a good result gets right
"""


def _template_files(tpl: Template, name: str, n_tasks: int) -> dict[str, str]:
    files = {}
    picked = [tpl.slots[i % len(tpl.slots)] for i in range(n_tasks)]
    blocks = [_header(name, tpl.domain, f"A `{tpl.name}` eval pack template written by `hlock evals init`.\n"
                                         "# Fill in the marked slots, then run `hlock evals check --run`.",
                      judge=any(slot.grader == "judge" for slot in picked))]
    counts: dict[str, int] = {}
    for slot in picked:
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
        grader_file = f"{tid}/check.py" if slot.grader == "tests" else f"{tid}/rubric.md"
        what = ("the file a successful run leaves in the workspace, and the check in" if slot.artifact
                else "the agent changes files in place; write the check in")
        lines += [
            f'solution = "{tid}/solution/"',
            f"tools = {_toml_list(slot.tools)}",
            f"# {PLACEHOLDER}: {what} {grader_file}.",
        ]
        if slot.artifact:
            lines.append(f'expected = {{ artifacts = ["{slot.artifact}"] }}')
        lines += [
            f'graders = ["{slot.grader}:{grader_file}"]',
            f'tags = ["{slot.kind.replace("_", "-")}"]',
            f"timeout-seconds = {slot.timeout}",
        ]
        blocks.append("\n".join(lines) + "\n")
        files[f"{tid}/prompt.md"] = _slot_prompt(slot)
        files[f"{tid}/solution/.gitkeep"] = ""
        files[grader_file] = _slot_check(slot, tid) if slot.grader == "tests" else _slot_rubric(slot, tid)
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
- Every task must produce something that can be checked: a file named in `expected.artifacts`
  in `pack.toml` and at the end of `prompt.md`, or (for code) changes its tests can check. An
  expected file must not already be in `workspace/`.
- Grade the result, not just that a file exists. Each slot has a grader to fill in:
  `<task>/check.py`, a script run in the workspace after the agent that exits 0 when the work
  is right, for anything a program can check (tests, values, schemas); or `<task>/rubric.md`,
  a list of criteria (`- ` items) a judge checks, for writing and judgment. The pack's judge
  is a decision model that answers each criterion as a yes/no question, so make each one
  concrete and checkable on its own. Switch a slot to the other kind when it fits better: edit its `graders` line.
- Put a known-good result in `<task>/solution/`: the files a correct run leaves, such as the
  code after the real fix or the reply that was actually sent. It shows the graders pass on
  good work.
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
4. Run `hlock evals check {pack_dir} --run` and fix what it reports until it shows no
   errors. It runs each task's graders on its fixtures alone (they must fail) and on its
   solution (they must pass). `hlock evals grade <task> [dir]` shows each grader's result,
   judges included.
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


# What a decision model asks about each request when drafting (`review` below).
DRAFT_KINDS = {
    "code": "Changing or debugging code in a repository",
    "data": "Cleaning, analyzing or transforming data",
    "writing": "Writing or summarizing: a report, a reply, a memo",
    "other": "Something else",
}


def review_requests(prompts: list[str], decide) -> list[dict]:
    """Ask a decision model about each drafted request: does it stand alone, is its
    result checkable, what kind of work is it, and does it hold a secret. ``decide`` is
    called as ``decide(state, questions_json)`` and returns the JSON
    ``hlock.decide`` does."""
    questions = json.dumps([
        {"id": "alone", "type": "noul", "instructions": "This request stands on its own: it doesn't refer to "
         "an earlier conversation (\"like before\", \"fix it\")."},
        {"id": "checkable", "type": "noul", "instructions": "A program or a short rubric could check whether "
         "the result of this request is right."},
        {"id": "kind", "type": "choice", "instructions": "What kind of work is this request?", "criteria": DRAFT_KINDS},
        {"id": "secret", "type": "noul", "instructions": "The request contains a credential, a token, or "
         "personal data about someone."},
    ])
    out = []
    for p in prompts:
        answers = {a["id"]: a for a in json.loads(decide(p, questions))["decided"]["answers"]}
        out.append({
            "alone": answers["alone"]["value"],
            "checkable": answers["checkable"]["value"],
            "kind": answers["kind"]["value"],
            "secret": answers["secret"]["value"],
        })
    return out


def draft_from_traces(dest: str | Path, sessions, name: str | None = None, n_tasks: int = 10,
                      project: Path | None = None, force: bool = False, decide=None) -> tuple[list[Path], int]:
    """Write a draft eval pack to ``dest`` from trace sessions, newest first, one task per
    distinct request. Prompts are redacted. A task whose session created files expects
    them as artifacts; the rest get a ``TODO(hlock)`` to fill in. With ``decide`` (see
    `review_requests`), a decision model skips requests whose result can't be checked,
    marks ones that lean on earlier context or may hold a secret, and picks each task's
    grader stub. Returns the files written (``pack.toml`` first) and the number of tasks."""
    from hlock import redact_text

    dest, project = Path(dest), (project or Path.cwd()).resolve()
    if (dest / "pack.toml").exists() and not force:
        raise ValueError(f"{dest / 'pack.toml'} already exists")
    seen, picked = set(), []
    for s in sorted(sessions, key=lambda s: s.started, reverse=True):
        key = " ".join(s.prompt.lower().split())
        if key not in seen:
            seen.add(key)
            picked.append(s)
    home = str(Path.home())
    reviews: dict[int, dict] = {}
    if decide is not None:
        prompts = [redact_text(s.prompt).replace(home, "~")[:4000] for s in picked]
        kept = []
        for s, r in zip(picked, review_requests(prompts, decide)):
            if r["checkable"] >= 0.3:  # drop only the clear cases
                reviews[id(s)] = r
                kept.append(s)
        picked = kept
    picked = picked[:n_tasks]
    if not picked:
        raise ValueError("no requests found in the traces")
    name = name or f"{project.name}/{dest.resolve().name}"
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
        review = reviews.get(id(s))
        tags = ["from-traces", s.harness]
        notes = []
        if review is not None:
            tags.append(review["kind"])
            stub = f"{tid}/check.py" if review["kind"] in ("code", "data") else f"{tid}/rubric.md"
            lines.append(f"# {PLACEHOLDER}: write {stub} so the grader checks the result, not just the file.")
            lines.append(f'graders = ["{"tests" if stub.endswith(".py") else "judge"}:{stub}"]')
            slot = Slot("task", "the drafted task", "", arts[0] if arts else "result.md",
                        "What a good result must get right.", "tests" if stub.endswith(".py") else "judge")
            files[stub] = _slot_check(slot, tid) if stub.endswith(".py") else _slot_rubric(slot, tid)
            if review["alone"] < 0.5:
                notes.append(f"{PLACEHOLDER}: this request may lean on earlier context; make it stand alone.")
            if review["secret"] >= 0.5:
                notes.append(f"{PLACEHOLDER}: this request may hold a secret or personal data that redaction missed.")
        lines += [f"tags = {_toml_list(tags)}", "timeout-seconds = 900"]
        blocks.append("\n".join(lines) + "\n")
        note = "".join(f"<!-- {n} -->\n" for n in notes)
        files[f"{tid}/prompt.md"] = (
            f"<!-- drafted from a {s.harness} session ({when}); secrets redacted. Review before use. -->\n{note}{prompt}\n")
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
- Grade the result, not just that the file exists: add a `graders` entry to each task. Use
  `"tests:<task>/check.py"`, a script run in the workspace after the agent that exits 0 when
  the work is right, when a program can check it, and `"judge:<task>/rubric.md"`, a rubric a
  judge model scores, for writing. Judges need `[pack.judge] model` in `pack.toml`.
- Put the files a correct run leaves in `<task>/solution/` and set `solution` on the task,
  such as what the session actually produced, once the user confirms it was right.
- Keep ids unique and in `snake_case`, and keep each directory named after its id.
- Aim for at least 10 tasks, so hlock can hold some back to check the winner.

## Steps

1. Read `{pack_dir}/pack.toml` and the `prompt.md` files.
2. Agree with the user on which tasks to keep.
3. For each task: rewrite `prompt.md` to stand alone, fill `workspace/`, and check its
   `[[task]]` entry.
4. Run `hlock evals check {pack_dir} --run` and fix what it reports until it shows no
   errors. It runs each task's graders on its fixtures alone (they must fail) and on its
   solution (they must pass).
5. Tell the user the pack is ready, and that `hlock init --pack {pack_dir}` (or the `[evals]`
   section of an existing `harness.toml`) points hlock at it.
"""
