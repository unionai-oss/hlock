# Eval pack developer experience

Status: implemented · Owner: Niels Bantilan · Last updated: 2026-10-20 · Builds on [v0](v0.md) §7.4

> An author can try a task on their real agent and see why it failed (`hlock evals run`).
> The lock's evidence names the exact pack it was measured on. `hlock evals check` catches
> mistyped keys, and the `evals` commands find the pack on their own.

---

## 1. Problem

A walk through the eval pack workflow as a new author (`evals init` → fill in tasks →
`evals check` → `resolve`) turned up these gaps:

1. **You can't watch your agent try a task.** The author's loop is: write a task, see how
   the agent does, then fix the prompt or the grader. `hlock evals grade TASK DIR` grades a
   directory, but nothing produces that directory. In `resolve`, only code harnesses run for
   real. Agent CLIs (Claude Code, Codex, Pi, …) run on the simulated reference harness, so
   an author who uses Claude Code never sees Claude Code attempt their tasks. `hlock run
   PROMPT` runs the CLI for real, but in the project directory, with no fixtures and no
   grading.
2. **Evidence isn't tied to the pack's content.** `resolve` writes `eval-pack = "./evals"`,
   the path and nothing else. `eval-pack-version` exists in the lock schema but nothing
   fills it in. `EvalPack::hash` digests `pack.toml` only, so editing a prompt, a
   `check.py`, or a fixture leaves it unchanged. The guide says "each pack's contents are
   hashed, and the hash is recorded", which is true for `hlock add --eval`'s
   `[*.evidence]` and false for `[evidence]`. Once a pack changes, nothing says the lock's
   numbers came from an older pack.
3. **Mistyped keys are silently ignored.** A grader table rejects unknown keys
   (`unknown key `equal``). `[pack]` and `[[task]]` don't: `timeout_seconds = 300` quietly
   leaves the 600-second default, and `expect = {…}` or `grader = […]` drops the check
   entirely. `evals check` reports the pack as ready.
4. **Pack errors come with the wrong hint.** Every eval pack error ends in "check the
   `[evals] packs` paths in harness.toml". That's wrong for `hlock evals check ./nope`
   (the path came from the command line, and there may be no `harness.toml`), and for a
   TOML syntax error.
5. **The `evals` commands don't find the pack.** `check`, `grade` and `calibrate` default
   to `./evals`, whatever `[evals] packs` says, and fail when run inside the pack
   directory.

Out of scope, noted for later:

- `hlock evals add ID`, to add one task (its `pack.toml` entry and directory) to an
  existing pack. Today you copy a `[[task]]` block, or ask the agent with the
  `hlock-eval-pack` skill.
- The repository's own `evals/` demo pack has no graders or solutions, while the guide's
  `pack.toml` example (same pack name) does. Many tests copy the demo pack and rely on its
  simulated results, so changing it is a separate change.

## 2. Design

### 2.1 `hlock evals run`

```text
hlock evals run [TASK...] [--pack DIR] [--seeds N] [--workdir DIR] [--trust] [--json]
```

It runs the agent `harness.lock` pins on each named task (default: every task), once per
seed (default 1), each in a fresh workspace. Then it grades each run with the task's
graders, judges included, the same way `hlock evals grade` does. The output is one row per
run: pass or fail, score, time, and where its workspace is. It exits 1 if any run fails.

```console
$ hlock evals run --workdir runs
✓ hello_file  1.00  0.23s  runs/hello_file-0
✓ count_words  1.00  0.01s  runs/count_words-0
✗ greet_people  0.67  0.01s  runs/greet_people-0
    ✗ tests:greet_people/check.py  0.33  1 of 3 lines right: expected ['Hello, Ada!', 'Hello, Grace!', 'Hello, Linus!'], got ['Hello, Ada!', 'Hi, Grace!']
✗ 2 of 3 runs passed  (loop, harness.lock)
next → hlock evals grade greet_people runs/greet_people-0
```

When the agent itself fails, the line shows why (`Claude Code exited 1`) and the last
line of its log (`Not logged in · Please run /login`).

It's for writing a pack, not for measuring a stack. It records nothing in the lock, and it
doesn't split tasks into search and holdout.

**What runs.** It runs the same agent `hlock run` does:

- an **agent CLI** runs through its non-interactive mode (`claude -p`, `codex exec`, …),
  with the lock's environment from `launch` (isolated config root, secrets, trust gate),
  and the workspace as its working directory;
- a **code harness** runs through the execution contract, in the workspace;
- the **reference** harness is refused: it's simulated, and `resolve` already runs it.
  The error says to pin the real setup with `hlock pin`.

**The workspace.** Each run gets `<workdir>/<task>-<seed>/`. `--workdir` defaults to a new
temporary directory *outside the project*. A task built from the project's history (the
code before a fix) must not be solvable by reading the fix in the project next door. The
task's fixtures are copied in, without following symlinks, as `resolve` does.

For an agent CLI, the lock's *project-scoped* entries (its skills, instructions,
`.mcp.json`, project config) are set up in the workspace, as `launch` would set them up in
a project. The agent sees the setup the lock describes, and nothing else from the project.
User-scoped entries go to the isolated config root as usual. To do this, `LaunchOptions`
gets a `project` field: where project-scoped entries go and where the agent runs. The
isolated config root stays under the real project's `.hlock/env`.

The agent's output (stdout and stderr) goes to `<workdir>/<task>-<seed>.log`, next to the
workspace rather than in it, so graders don't see it. A run that hits the task's
`timeout-seconds` is stopped and fails.

An agent CLI runs against the lock's isolated config root, as `hlock run` and `hlock
launch` do, so it needs credentials that root can reach (an API key in the environment, or
a secret the lock names). A login stored in your own config doesn't carry over. That's
existing launch behavior, out of scope here.

Workspaces are kept so you can open them, re-grade them with `hlock evals grade TASK DIR`,
or diff them against `solution/`.

### 2.2 Evidence names the pack

The lock's `[evidence]` gets the pack's identity and content:

```toml
[evidence]
eval-pack = "./evals"
eval-pack-version = "acme/evals@0.3.0"
eval-pack-digest = "9f2c…"          # tree digest of every pack directory
```

- `eval-pack-version` is `name@version` for each pack, comma-separated, in the
  manifest's order. The field already exists; `resolve` and `tune` now fill it in.
- `eval-pack-digest` is new. It's the same digest `hlock add --eval` records and keys its
  trial cache on (the tree digests of the pack directories, joined and hashed), moved from
  `deps.rs` to `tasks::packs_digest` so there's one definition.
- `EvalPack::hash` becomes the pack directory's tree digest, so `evals check --json`'s
  `hash` changes when any file in the pack does.

When the packs' digest no longer matches, `hlock check` on a resolved lock warns and still
exits 0:

```console
! the eval packs changed since harness.lock's evidence was measured
  hint: re-measure with hlock resolve
```

It's a warning because the stack hasn't drifted. The numbers just describe an older pack.
Locks written before this change have no digest, and nothing is reported for them.
`hlock show` lists the pack version under Evidence.

### 2.3 `evals check` reports unknown keys

`check` reads `pack.toml` again as plain TOML and reports a key it doesn't know in
`[pack]`, `[pack.judge]` or a `[[task]]` as an error. A close match is named:

```console
  ✗ count_words  unknown key `timeout_seconds` in [[task]] (did you mean `timeout-seconds`?)
```

Loading stays lenient, so `resolve` still runs such a pack with its usual "has N problems"
warning. An error, not a warning, because the usual cause is a typo that silently changes
or removes a check.

### 2.4 Hints that fit the error

For an eval pack error:

- "missing pack.toml at DIR": hint `hlock evals init DIR`, and when `[evals] packs` lists
  something else, name those paths;
- a TOML syntax error: no hint (the message points at the line);
- other pack errors: no `harness.toml` hint unless the command read the pack from
  `harness.toml`.

### 2.5 Finding the pack

`check`, `calibrate`, `grade` and `run` pick the pack the same way when none is given:

1. the current directory, if it holds a `pack.toml`;
2. else the first of `[evals] packs` in `harness.toml`;
3. else `./evals`.

## 3. Changes

| Where | What |
|---|---|
| `tasks.rs` | `packs_digest`; `EvalPack::hash` is a tree digest; unknown-key issues in `check_with`; `prepare_workspace` (fixtures into a fresh directory) |
| `deps.rs` | uses `tasks::packs_digest` |
| `lockfile.rs` | `Evidence::eval_pack_digest` |
| `resolver.rs`, `ops.rs` | fill in `eval-pack-version` and `eval-pack-digest` |
| `launch.rs` | `LaunchOptions::project` |
| `harness-lock-py` | `prepare_eval_workspace`, `eval_packs_digest`; `launch_prepare(project=…)`; evidence getter adds the pack fields |
| `hlock/cli.py` | `evals run`; pack discovery; hints; `check` warning; `show` row |
| `hlock/evals.py` | the `hlock-eval-pack` skill mentions `evals run` |
| docs | eval packs guide, CLI reference, lockfile reference |
| `skills/hlock` | the command table lists `hlock evals run` |
