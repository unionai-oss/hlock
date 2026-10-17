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
  6 files under evals
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
reports until you fill it in. The drafts are a start, not a finished pack: a request often
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

### Fill in a template with your agent

For a real pack, start from a template. Each one writes task slots for one kind of work,
marked `TODO(hlock)`:

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
  21 files under evals
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
- give every task an output file that can be checked, and remove each `TODO(hlock)`;
- run `hlock evals check` until it reports no errors.

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
  ✗ pack.toml still has 20 TODO(hlock) (lines 10, 15, 21, 26, 32, …)
  ✗ bug_fix_1  bug_fix_1/prompt.md still has 2 TODO(hlock)
  ✗ feature_1  feature_1/prompt.md still has 2 TODO(hlock)
  …
✗ 11 errors, 0 warnings
```

| Errors (the pack isn't usable yet) | Warnings (it runs, but the evidence is weak) |
|---|---|
| `[pack]` is missing `name`, `version`, or `domain` | A task has no expected artifacts or graders, so it always passes |
| A `TODO(hlock)` is left in `pack.toml` or a prompt | An expected artifact is already in the fixtures, so it passes without any work |
| A task id is missing or duplicated | A grader type that isn't supported yet |
| A prompt file is missing or empty, or a fixtures directory is missing | Fewer than 10 tasks, so the holdout split is too small |

`hlock resolve` also warns about any pack with errors before it runs.

## Layout

A pack is a directory with a `pack.toml` and one folder per task:

```text
evals/
├── pack.toml
├── clean_csv/
│   ├── prompt.md
│   └── workspace/
│       └── orders.csv
└── summarize_report/
    └── prompt.md
```

## `pack.toml`

```toml title="evals/pack.toml"
[pack]
name = "harness-lock/demo"
version = "0.1.0"
license = "Apache-2.0"
domain = "research-analyst"

[[task]]
id = "clean_csv"
prompt = "clean_csv/prompt.md"
fixtures = "clean_csv/workspace/"
tools = ["python"]
expected = { artifacts = ["cleaned.csv"] }
tags = ["data-cleaning"]
timeout-seconds = 600

[[task]]
id = "summarize_report"
prompt = "summarize_report/prompt.md"
tools = ["python", "filesystem"]
expected = { artifacts = ["summary.md"] }
tags = ["writing"]
```

### `[pack]`

| Key | Required | Description |
|---|---|---|
| `name` | yes | Pack identifier, conventionally `owner/name` |
| `version` | yes | Pack version. Change it when you change the tasks. |
| `domain` | yes | The domain the pack represents |
| `license` | no | License of the pack contents |

### `[[task]]`

| Key | Required | Default | Description |
|---|---|---|---|
| `id` | yes | | Unique task id within the pack |
| `prompt` | yes | | Path to the prompt file, relative to the pack directory |
| `fixtures` | no | | Directory copied into the task's workspace before the run |
| `tools` | no | `[]` | Tools the task may use |
| `expected` | no | `{}` | What a successful run produces, e.g. `{ artifacts = ["out.csv"] }` |
| `graders` | no | `[]` | Grader ids. Empty means artifact checks on `expected.artifacts`. |
| `tags` | no | `[]` | Free-form labels for slicing results |
| `timeout-seconds` | no | `600` | Per-task timeout |

## Grading

Today, a task succeeds when every file in `expected.artifacts` exists in the task's
workspace after the run. This is the `artifact:` grader, and it's the default when
`graders` is empty. Richer graders (tests, rubric judges) are on the
[roadmap](https://github.com/unionai-oss/hlock/blob/main/specs/v0.md).

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
  without a human.
- **Version the pack.** Bump `version` when tasks change, so evidence from different
  versions isn't compared by accident.
