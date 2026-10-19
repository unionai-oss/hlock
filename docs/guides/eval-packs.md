# Eval packs

An eval pack is the set of tasks that defines what "works" means for your domain. Every
candidate stack runs against it, and the evidence in `harness.lock` comes from it, so the
pack matters more than any other input. A good pack looks like the work you actually want
the agent to do.

Eval packs and [skills](pin.md) sit on opposite sides of the resolver. A skill is something
the agent uses, so it's one of the things hlock can tune. An eval pack is how hlock scores
each candidate, so you can tell whether a skill helped.

## Start a pack

`hlock evals init` writes a pack to `./evals`. With no options, it writes a complete
hello-world pack that runs as is:

```console
$ hlock evals init
✓ wrote evals/pack.toml  (hello-world, 3 tasks)
  10 files under evals
next → hlock evals check ./evals  ·  hlock init --pack ./evals
```

[Finding the best stack](resolve.md) takes it from there. Three tasks are
enough to watch the loop work, but too few to trust a result.

### Draft a pack from your traces

The requests you've already made to your agent are the best source of tasks.
`hlock evals init --from-traces` reads the sessions your agent CLI logged for this project
and drafts one task per distinct request, newest first:

```console
$ hlock evals init --from-traces
✓ wrote evals/pack.toml  (4 draft tasks from 4 claude-code sessions)
  2 tasks need an expected artifact; prompts are redacted drafts to review
✓ wrote .agents/skills/hlock-eval-pack/SKILL.md  (Agent Skill hlock-eval-pack)
open your agent here and ask it to review ./evals with the hlock-eval-pack skill
next → hlock evals check ./evals
```

Each task's prompt is the request that opened a session, with credential-looking values
replaced by `[redacted]` and your home directory by `~`. Files the session created inside
the project (read from Claude Code and Pi sessions, and from an `artifacts` column) become
the task's expected artifacts, so the task is checkable:

```toml title="evals/pack.toml"
[[task]]
id = "fix_off_by_one_pagination"
prompt = "fix_off_by_one_pagination/prompt.md"
fixtures = "fix_off_by_one_pagination/workspace/"
tools = ["shell", "filesystem"]
expected = { artifacts = ["tests/test_pagination.py"] }  # files the session created
tags = ["from-traces", "claude-code"]
timeout-seconds = 900
```

A session that created no file gets a `TODO(hlock)` slot for one, which `hlock evals check`
reports until you fill it in. A drafted task only checks that its files exist, and `hlock
evals check` warns about that too: add [graders](#grading) that check what's in them. The drafts are a start, not a finished pack: a request often
leans on its session ("fix it like before"), and each task's `workspace/` starts empty. The
`hlock-eval-pack` skill written next to the pack tells your agent how to review them with
you: which tasks to keep, how to make each prompt stand on its own, and which files to copy
into its workspace.

| Source | Where hlock looks |
|---|---|
| Claude Code | `~/.claude/projects/<project>/*.jsonl` (or under `CLAUDE_CONFIG_DIR`) |
| Codex | `~/.codex/sessions/**/*.jsonl` (or under `CODEX_HOME`), sessions started in this project |
| Pi | `~/.pi/agent/sessions/*/*.jsonl` (or under `PI_CODING_AGENT_DIR`), sessions started in this project |
| A file or directory | `--from-traces PATH`: any of the session logs above, or JSONL or CSV records with a `prompt` (or `input`, `question`, `query`, `instruction`, `request`, `task`) column and an optional `artifacts` list |

`--tasks N` caps how many tasks are drafted (default 10). Sessions from other projects are
left out, and nothing leaves your machine.

With a decision model in `[deciders.draft]` (or `[deciders.default]`) in `harness.toml`,
drafting asks it about each request: whether its result can be checked (requests that
clearly can't are skipped), whether it stands alone, whether it may hold a secret the
redaction missed, and what kind of work it is, which picks the task's grader stub
(`check.py` for code and data, `rubric.md` for the rest). It says which model it used; a
hosted one sees the redacted prompts.

### Fill in a template with your agent

For a real pack, start from a template. Each one writes task slots for one kind of work,
marked `TODO(hlock)`, and a grader stub for each: a `check.py` [test script](#tests) for work a
program can check, or a `rubric.md` for a [judge](#judges):

```console
$ hlock evals templates
template     task kinds
hello-world  complete pack                         A complete three-task pack that runs
                                                   as is. Start here to see the loop
                                                   work.
coding       bug_fix, feature, tests, refactor,    Software work in a codebase: bug
             debug                                 fixes, features, tests, refactors,
                                                   debugging.
data         clean, analyze, transform, query,     Data work: cleaning, analysis,
             chart                                 transformation, queries, and charts.
research     summarize, compare, answer, extract,  Reading and writing: summaries,
             memo                                  comparisons, answers with sources,
                                                   memos.
support      triage, reply, reproduce, escalate,   Support work: triage, replies,
             article                               reproductions, escalations, help
                                                   articles.
blank        task                                  Generic slots for any domain. Use it
                                                   when no other template fits.
next → hlock evals init -t <template>
```

A template also writes an [Agent Skill](https://agentskills.io), `hlock-eval-pack`, that
tells the agent you already use (Claude Code, Codex, OpenCode, Pi, …) how to fill the
slots with real tasks from your project:

```console
$ hlock evals init -t coding
✓ wrote evals/pack.toml  (coding template, 10 task slots)
  41 files under evals
✓ wrote .agents/skills/hlock-eval-pack/SKILL.md  (Agent Skill hlock-eval-pack)
open your agent here and ask it to fill in ./evals with the hlock-eval-pack skill
next → hlock evals check ./evals
```

Then open your agent in the project and ask it to fill in `./evals` with the
`hlock-eval-pack` skill. The skill tells it to:

- look for real work in the places that fit the template, such as `git log`, merged pull
  requests, and closed issues for `coding`;
- propose a list of tasks and wait for you to agree before writing anything;
- copy only the files each task needs into its `workspace/`, with no secrets or personal
  data;
- give every task a grader that checks the result, not just that a file exists, and a
  known-good result in its `solution/` directory, such as the code after the real fix;
- remove each `TODO(hlock)`;
- run `hlock evals check --run` until it reports no errors.

To have hlock run the agent and check the pack itself, use the
[meta harness](meta.md#bootstrap-evals): `hlock meta run bootstrap-evals -t coding` writes
the template if there's no pack yet, hands the slots to a pinned open-source agent, and
runs the same check when it's done.

The skill goes in `.agents/skills/`, which most harnesses read. Use `--skill-dir` to put
it somewhere else, such as `.claude/skills/hlock-eval-pack`, or `--no-skill` to fill the
pack in yourself. `--tasks N` sets how many slots the template writes (default 10), and
`--name` sets the pack name (default `<project>/<directory>`).

### Check a pack

`hlock evals check` confirms that a pack is ready to measure stacks with. It exits `1`
on errors:

```console
$ hlock evals check
my-service/evals 0.1.0  10 tasks · evals
  ✗ pack.toml still has 20 TODO(hlock) (lines 13, 19, 25, 31, 37, …)
  ✗ bug_fix_1  bug_fix_1/prompt.md still has 2 TODO(hlock)
  ✗ bug_fix_1  tests grader file bug_fix_1/check.py still has 2 TODO(hlock)
  ✗ feature_1  feature_1/prompt.md still has 2 TODO(hlock)
  ✗ feature_1  tests grader file feature_1/check.py still has 2 TODO(hlock)
  …
✗ 21 errors, 0 warnings
```

| Errors (the pack isn't usable yet) | Warnings (it runs, but the evidence is weak) |
|---|---|
| `[pack]` is missing `name`, `version`, or `domain` | A task has no expected artifacts or graders, so it always passes |
| A `TODO(hlock)` is left in `pack.toml`, a prompt, a test script, or a rubric | A task only checks that its files exist |
| A task id is missing or duplicated | An expected artifact is already in the fixtures, so it passes without any work |
| A prompt file is missing or empty, or a fixtures or solution directory is missing | The pack has `judge` graders but no judge model |
| A grader doesn't parse, or a file it names is missing | A `trace` grader, which isn't supported yet and is skipped |
| | Fewer than 10 tasks, so the holdout split is too small |

`hlock resolve` also warns about any pack with errors before it runs. To check the graders
themselves, run `check --run` (see [Check your graders](#check-your-graders)).

## Layout

A pack is a directory with a `pack.toml` and one folder per task:

```text
evals/
├── pack.toml
├── clean_csv/
│   ├── prompt.md
│   ├── check.py            # a tests grader
│   ├── workspace/          # fixtures: what the run starts from
│   │   └── orders.csv
│   └── solution/           # what a correct run leaves
│       └── cleaned.csv
└── summarize_report/
    ├── prompt.md
    └── rubric.md           # a judge grader's rubric
```

## `pack.toml`

```toml title="evals/pack.toml"
[pack]
name = "harness-lock/demo"
version = "0.1.0"
license = "Apache-2.0"
domain = "research-analyst"

[pack.judge]
model = "anthropic:claude-sonnet-5-5"

[[task]]
id = "clean_csv"
prompt = "clean_csv/prompt.md"
fixtures = "clean_csv/workspace/"
solution = "clean_csv/solution/"
tools = ["python"]
expected = { artifacts = ["cleaned.csv"] }
graders = ["tests:clean_csv/check.py"]
tags = ["data-cleaning"]
timeout-seconds = 600

[[task]]
id = "summarize_report"
prompt = "summarize_report/prompt.md"
tools = ["python", "filesystem"]
expected = { artifacts = ["summary.md"] }
graders = ["judge:summarize_report/rubric.md"]
tags = ["writing"]
```

### `[pack]`

| Key | Required | Description |
|---|---|---|
| `name` | yes | Pack identifier, conventionally `owner/name` |
| `version` | yes | Pack version. Change it when you change the tasks. |
| `domain` | yes | The domain the pack represents |
| `license` | no | License of the pack contents |
| `judge` | no | The model that scores `judge` and `trace` graders: a table with `model`, and optionally `base-url`, `key-env`, `protocol`, `fallback` and `uncertain`. See [Judges](#judges). |

### `[[task]]`

| Key | Required | Default | Description |
|---|---|---|---|
| `id` | yes | | Unique task id within the pack |
| `prompt` | yes | | Path to the prompt file, relative to the pack directory |
| `fixtures` | no | | Directory copied into the task's workspace before the run |
| `solution` | no | | Directory with the files a correct run leaves, used to [check the graders](#check-your-graders) |
| `tools` | no | `[]` | Tools the task may use |
| `expected` | no | `{}` | What a successful run produces, e.g. `{ artifacts = ["out.csv"] }`. Every file listed must exist after the run. |
| `graders` | no | `[]` | How the run is scored: strings or tables, see [Grading](#grading). Empty means only the `expected.artifacts` check. |
| `tags` | no | `[]` | Free-form labels for slicing results |
| `timeout-seconds` | no | `600` | Per-task timeout |

## Grading

A run passes when every grader on its task passes. Each grader also scores the run from 0
to 1, and the run's score is the mean, so a near miss counts for more than nothing. Every
file in `expected.artifacts` must exist after the run, so a task with no `graders` is graded
on that alone.

`graders` is a list. Each entry is a short string, such as `"tests:clean_csv/check.py"`, or a
table with a `type`. There are four types:

| Type | Checks | Use it for |
|---|---|---|
| `artifact` | Output files exist and match expected content | Exact answers, required phrases, golden files |
| `tests` | A script or command, run in the workspace after the agent, exits 0 | Code, data, and anything a program can check |
| `judge` | A model scores the output against criteria or levels | Writing and judgment calls a program can't check |
| `trace` | A decision model checks the run's tool calls | How the agent worked: asking first, staying in bounds |

Prefer the first two: they're fast, free, and give the same answer every time. Use a judge
only where a program can't decide. Paths to grader files are relative to the pack directory.

### Artifact checks

```toml
graders = [
  { type = "artifact", path = "count.txt", equals = "9" },
  { type = "artifact", path = "report.md", contains = ["Q3 revenue", "$1.2M"] },
  { type = "artifact", path = "cleaned.csv", same-as = "clean_csv/expected.csv" },
]
```

| Key | Description |
|---|---|
| `path`, `paths` | The workspace files to check. Default: `expected.artifacts`. |
| `equals` | The file's text must be exactly this, ignoring whitespace around it and line-ending style |
| `same-as` | The file must match this file in the pack, the same way |
| `contains` | Strings the file must contain (a string or a list) |

With no content keys, it checks that the files exist. `"artifact:out.csv"` is the short
form for that. The score is the fraction of files that pass.

### Tests

A `tests` grader runs a script or a command in the task's workspace after the agent
finishes. Exit code 0 passes:

```toml
graders = [
  "tests:greet_people/check.py",
  { type = "tests", command = "python -m pytest -q tests/", files = ["fix_pagination/tests"], timeout-seconds = 120 },
]
```

| Key | Default | Description |
|---|---|---|
| `script` | | A file in the pack. A `.py` file runs with `python3` (or `HL_PYTHON`); anything else runs directly. `"tests:<path>"` is the short form. |
| `command` | | A shell command, instead of a `script` |
| `files` | `[]` | Files or directories in the pack that are copied into the workspace, by name, before it runs. They replace anything the agent wrote with the same name. |
| `timeout-seconds` | `300` | The command is stopped, and fails, after this long |

Keep tests the agent shouldn't see out of `workspace/`, and copy them in with `files`.
Then the agent can't read them or change them.

The command gets these environment variables:

| Variable | Value |
|---|---|
| `HLOCK_WORKSPACE` | The workspace, also the working directory |
| `HLOCK_PACK_DIR` | The pack directory, for reference data kept next to the script |
| `HLOCK_TASK` | The task id |
| `HLOCK_GRADE` | A path where the command may write `{"score": 0.5, "success": false, "detail": "…"}` |
| `HLOCK_TRACE` | The run's trace as JSON (in `resolve`; unset when you grade a directory by hand) |

Without a `HLOCK_GRADE` file, the score is 1 on exit code 0 and 0 otherwise. Each key in
the file is optional, and `success` defaults to the exit code. The last lines of output
from a failing command become the grade's detail. Here is the hello-world pack's script,
which gives partial credit:

```python title="evals/greet_people/check.py"
"""Grader for greet_people: one `Hello, <name>!` line per name in names.csv, in order.

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
```

!!! warning "Test commands run on your machine"
    A `tests` grader runs with your permissions, the same as the agent being measured, and
    it often runs code the agent wrote. Read the graders in a pack you didn't write before
    you resolve against it.

### Judges

A `judge` grader asks a model about the files the run left (the first 20,000 characters
of each), with the task's prompt. The judge can be a [decision model](deciders.md), which
answers each criterion as a yes/no question with a probability, or an LLM, which is asked
for JSON. A decision model is the better default for checklists: it's fast, calibrated,
and costs around a hundredth of a cent a task, so judges can run on every trial.

```toml
graders = [
  "judge:summarize_report/rubric.md",
  { type = "judge", criteria = [
      "States Q3 revenue and its change from Q2",
      { text = "Cites the report", true = "Names the report each claim comes from", false = "Makes claims with no source" },
  ], pass = 0.6 },
  { type = "judge", artifacts = ["reply.md"], levels = [
      "Doesn't address the customer's problem",
      "Addresses it, with wrong or unsafe steps",
      "Correct steps, but no link to the help article",
      "Correct steps and the right help article",
  ] },
]
```

| Key | Default | Description |
|---|---|---|
| `criteria` | | Points a good result meets: strings, or tables with `text` and `true`/`false` descriptions of each answer. The score is the mean P(met) (a decider) or the fraction met (an LLM). |
| `levels` | | 2 to 10 ordered descriptions, worst first. The model places the result on them, and the score is the level reached over the top level. With `criteria`, the score is the mean of the two. |
| `rubric` | | A file in the pack instead: its `- ` items are criteria, and a numbered list under a `## Levels` heading is levels. An LLM judge also reads a free-form rubric and scores it 0 to 1. `"judge:<path>"` is the short form. |
| `artifacts` | `expected.artifacts` | The workspace files the model reads |
| `pass` | `0.7` | The lowest score that passes |

The grade's detail names the unmet criteria (and the level reached), and `hlock evals
grade` prints each criterion's probability.

#### Which judge

The first of these that's set: `[evals.judge]` in `harness.toml`, `[pack.judge]` in
`pack.toml` (so a pack can name the judge its criteria were written for), and
`[deciders.judge]` or `[deciders.default]` in `harness.toml`. When none is, `resolve`,
`evals grade` and `evals check --run` use a Kev model a local Ollama or Ollaya is serving,
else `typesafe:jev-1.13.0` when `TYPESAFE_API_KEY` is set; `HLOCK_JUDGE_AUTO=0` turns that
off. Templates write `[pack.judge] model = "typesafe:jev-1.13.0"`.

```toml title="harness.toml"
[evals.judge]
model = "ollama:kev:4b"                    # a decision model, or an LLM: "anthropic:claude-sonnet-5-5"
fallback = "anthropic:claude-sonnet-5-5"   # re-judges a decision model's uncertain grades
uncertain = 0.15                           # how close to `pass` counts as uncertain
# base-url = "http://localhost:11434/v1"   # for an LLM hlock doesn't know, or a local one
# key-env = "MY_JUDGE_KEY"                 # where the API key is read from
# fallback-base-url = "…"                  # the fallback's endpoint, likewise
```

Decision-model references are listed in [Decision models](deciders.md#name-a-decision-model).
For an LLM judge:

| Provider | API key variable |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `google` | `GEMINI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |

For any other LLM provider, set `base-url` to an OpenAI-compatible endpoint. `protocol =
"chat"` makes hlock treat a model as an LLM when its name looks like a decision model's.

#### Uncertain grades

A decision model's grade is *uncertain* when its score is within `uncertain` (0.15) of
`pass`, or one of its answers is close to a coin flip. With a `fallback`, an LLM re-judges
uncertain grades and its verdict counts, so you pay for the LLM only on the close calls.
Without one, the grade stands, marked uncertain. The lock's evidence records the judge,
the model that answered, the share of uncertain grades, and what judging cost:

```toml title="harness.lock"
[evidence.judge]
model = "typesafe:jev-1.13.0"
answered = "jev-1.13.0"
protocol = "systemone"
uncertain-grades = 0.04
cost-usd-per-task = 0.0000042            # what judging cost a trial, when there's a price
```

A judge grader fails when no judge model is set or the model can't be reached, and the
grade's detail says why.

### Trace graders

A `trace` grader asks a decision model about the run's tool calls, not its files:

```toml
graders = [
  { type = "trace", criteria = ["Asked the user before deleting files", "Never ran a command with sudo"], pass = 1.0 },
  "trace:Never pushed to main",
]
```

It needs a decision model as the judge, and runs where there's a trace: in `resolve`, not
when you grade a directory by hand. For exact checks, such as a pattern in the commands,
use a `tests` grader: it can read the trace from `$HLOCK_TRACE`.

### Check your graders

A grader can be wrong in two ways: it passes work that wasn't done, or it fails work that
was. Give each task a `solution` directory holding what a correct run leaves, such as the
code after the real fix or the reply that was sent, and `hlock evals check --run` tests the
graders both ways. On the fixtures alone the graders should fail. On the fixtures plus the
solution, they should pass:

```console
$ hlock evals check --run
hlock-demo/evals 0.1.0  3 tasks · evals
  ! 3 tasks leaves 1 for the holdout split; add tasks until there are at least 10
✓ ready  (1 warning)
next → hlock init --pack ./evals
```

A solution its graders fail is an error, and a task that passes with no work is a warning.
`--run` runs the pack's test commands, in temporary directories. It runs decision-model
judges too, since they're cheap and give the same answer each time, and warns when one is
uncertain on a solution: the criterion to reword, or give `true` and `false` descriptions.
LLM judges run only with `--judges`. When the pack has a judge, the first line names it.

`hlock evals grade` grades one directory with all of a task's graders, judges included, and
shows each result. Point it at the output of a real run to see why it failed, or leave the
directory out to grade the task's solution:

```console
$ hlock evals grade greet_people
greet_people  its solution · score 1.00
  ✓ artifact:greetings.md  1.00
  ✓ tests:greet_people/check.py  1.00
✓ passed

$ hlock evals grade greet_people run
greet_people  run · score 0.67
  ✓ artifact:greetings.md  1.00
  ✗ tests:greet_people/check.py  0.33
      1 of 3 lines right: expected ['Hello, Ada!', 'Hello, Grace!', 'Hello, Linus!'], got ['Hello, Ada!', 'Hi, Grace!']
✗ 1 of 2 graders failed
```

It grades a copy, so the directory isn't changed. Try a judge on the solution and on a
weak answer before you trust it.

`hlock evals calibrate --against MODEL` judges each task's solution, and its run with no
work done, with the pack's judge and with a second one (an LLM, usually), and lists the
criteria they disagree on, with the share they agree on:

```bash
hlock evals calibrate --against anthropic:claude-sonnet-5-5
hlock evals calibrate --against ollama:qwen3:8b --against-base-url http://localhost:11434/v1
```

On the offline `reference` harness, a simulated successful run leaves the task's solution
in the workspace, so a pack's graders run for real even in the [demo](resolve.md).

## Using a pack

Point the manifest at one or more packs:

```toml title="harness.toml"
[evals]
packs = ["./evals", "../shared-packs/support-tickets"]
seeds = 3               # runs per task per candidate; default 3

[evals.splits]
search = 0.7            # fraction of tasks used to pick the winner; the rest is holdout
```

Pack paths are relative to the project root (the directory holding `harness.toml`). Each
pack's contents are hashed, and the hash is recorded so evidence can be traced back to the
exact tasks.

!!! note "Renamed from task packs"
    Eval packs used to be called task packs. Manifests with a `[tasks]` section and locks
    with `task-pack` evidence keys still load. New manifests and locks use `[evals]` and
    `eval-pack`.

## Writing good tasks

- **Sample real work.** Pull tasks from tickets, logs, or traces you already have.
  Synthetic tasks are useful for filling gaps, but they shouldn't make up the core.
- **Keep the split meaningful.** With fewer than about ten tasks, the holdout split is
  too small to say much. Add tasks before you trust a narrow win.
- **Make success checkable.** Every task should produce something a grader can check
  without a human, and its graders should check what's in it, not just that it exists.
- **Keep a solution.** A known-good result for each task lets `hlock evals check --run`
  prove the graders work.
- **Version the pack.** Bump `version` when tasks change, so evidence from different
  versions isn't compared by accident.
