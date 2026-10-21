# Tune optimizers: pluggable harness search behind one protocol

Status: phase 1 implemented · Owner: Niels Bantilan · Last updated: 2026-10-21 · Builds on [backends](backends.md), [reviewable diffs](reviewable-diffs.md), [resolve and tune DX](resolve-tune-dx.md)

> `tune` can only search system prompts today, with a stand-in that never runs the real
> harness. Harness optimizers (Harness², GEPA, ACE, RRSI, AutoSaddler and others) edit more
> of the harness and need more from their host: a typed bundle of the components they may
> edit, a way to evaluate candidates on the pack, a budget, and a held-out split they never
> see. This spec adds a `harness.optimize` work item that any optimizer can implement, an
> evaluation callback that keeps grading and budget in hlock, and an acceptance gate in the
> core that decides what reaches the lock. Every accepted edit lands as a reviewable file
> with its rationale and measured effect.

---

## 1. Problem

1. **The prompt stage is a stand-in.** `prompt.search` (`hlock/tasks/worker.py`) scores
   three fixed templates and one LLM draft on one task, with a string-prefix check, and
   never runs the harness. Its "gain" doesn't measure the agent.
2. **Only one component is tunable.** Harness optimizers now edit system prompts,
   guardrails, tool guidance, skills, scripts, subagents, memory, hooks and tools (§2). `tune`
   has nowhere to put any of that.
3. **An optimizer can't plug in.** The [backend protocol](backends.md) decides *where* work
   runs. `prompt.search` is a single opaque item that returns one string. An optimizer
   needs to run hundreds of trials *during* its search, against hlock's graders, inside a
   budget, and the protocol has no way to give it that.
4. **Nothing guards generalization.** The literature's main failure (§2.3) is an edit that
   fits the tasks it was tuned on and regresses elsewhere. `tune` measures the prompt gain
   on the same tasks it searched with.
5. **The result isn't reviewable or reproducible.** See [reviewable diffs](reviewable-diffs.md)
   §1: a tuned prompt is recorded as two digests and its text is lost.

## 2. What the optimizers do (October 2026)

### 2.1 Survey

| System | Edits | Edit form | Evaluation | Selection and guards | Code |
|---|---|---|---|---|---|
| [Harness²](https://arxiv.org/html/2610.14224v1) (Google, Northwestern) | 8 files: system prompt, guardrails, memory, tool guidance, skills, scripts, subagents, middleware plugin | LLM editor edits files; per-step record with diffs and evidence; `EXPERIENCE.md` lessons library | No labels: runs each candidate on the same task as a reference run and contrasts the behavior | Propose (invariant + domain candidate), probe, compose. No score gate, no held-out split | [google-research/harness2](https://github.com/google-research/harness2), Apache-2.0, CLI; OpenCode or Codex executor |
| [RRSI](https://arxiv.org/pdf/2609.24972) (Google, UNC, Stanford) | Anything: prompts, control flow, config, tools, skills, memory, subagents | Source diff per edit, tagged with component and hypothesis; git commit per accepted candidate | Verifiers, k trials per task, noise band δ measured on the base harness | Annealed edit budget (4 → 1 edits), ledger of rejected hypotheses, diff critic against benchmark-specific edits, accept only above the noise floor, cost gate, pruning of dead components | [google-research/rrsi](https://github.com/google-research/rrsi), Apache-2.0, CLI + per-domain adapter |
| [AutoSaddler](https://arxiv.org/pdf/2608.23041) (Microsoft et al.) | Prompt rules, tool descriptions, tools, hooks, loop logic. Not memory or skills | Agent edits source in a worktree; each edit records diagnosis, approach and files changed in a DAG | Mini-batch rerun, then dev set; splits by task group | Dev gate; capability (code) patches first, steering (text) patches after; test touched once | [microsoft/AutoSaddler](https://github.com/microsoft/AutoSaddler), MIT |
| [HarnessCompass](https://arxiv.org/html/2608.01918) | Tools, middleware, subagents (structural); prompt, tool descriptions, skills, memory (guidance) | Worktree branches + `change_manifest.json` (component, rationale, trace refs per edit) | 50 tasks to evolve, 450 held out | Greedy on the evolve set; a generalization gate rejects edits naming instances, tests or dataset terms | No public code |
| [RobustSGPO](https://arxiv.org/pdf/2609.09646) | Agent specs, contracts, routing, adding and removing agents | Typed operation (scope, op, targets), text by LLM; edits capped at 120 lines | 60/30/30 split by source; paired replay, same tasks and seeds | Must beat the incumbent by ε; scope widened on a schedule; graders and traces protected from edits | No public code |
| [GEPA](https://github.com/gepa-ai/gepa) | Any named text components (`dict[str, str]`) | Whole-component rewrite by a reflection model | Adapter: `evaluate(batch, candidate)` returns scores and traces; textual feedback per task | Pareto front over per-task val scores; budget in metric calls | MIT, Python API (`gepa.optimize`, `optimize_anything`) |
| [ACE](https://arxiv.org/abs/2510.04618) | One playbook (system prompt or memory) | Delta bullet ops, with an op log | Execution feedback; ground truth optional | Val selection; counters prune harmful bullets | [ace-agent/ace](https://github.com/ace-agent/ace), Apache-2.0 |
| [SkillOpt](https://arxiv.org/html/2605.23904) (Microsoft) | One skill document | Bounded add/delete/replace edits; edits per step decay | Train/selection/test split | Accept only on strict improvement on selection set; rejected edits fed back | aka.ms/SkillOpt |
| [auto-harness](https://github.com/neosigmaai/auto-harness), [AutoAgent](https://github.com/kevinrgu/autoagent), [DGM](https://github.com/jennyzzt/dgm) | The agent's own code | Code edits by a coding agent | Benchmark score | auto-harness: a regression suite that only grows; others: hill-climb or archive | MIT / Apache-2.0; each tied to its own agent |

### 2.2 What every optimizer needs, and what it gives back

In:

- **A seed harness as named, typed components**, each a file. The set has converged on
  system prompt, guardrails, tool guidance, skills (with scripts), subagents, memory,
  hooks/middleware, and tool code.
- **Which components it may edit.** RobustSGPO widens this on a schedule; AutoSaddler
  schedules code edits before text; HarnessCompass runs the two as separate tracks.
- **Train and dev tasks**, with test held back. Splits are by task group where tasks are
  related.
- **An evaluator it can call many times**, per candidate and per task, returning a score
  plus feedback and the trace. GEPA's adapter, AutoSaddler's mini-batch rerun and RRSI's k
  trials all assume this.
- **A budget** in rollouts, tokens or dollars, and a **proposer model** separate from the
  task model.

Out:

- **The final harness** as a directory of files.
- **A ledger of edits**: component, diff, hypothesis or rationale, and the measured effect.
  RRSI, AutoSaddler, HarnessCompass, ACE and Harness² all keep one, each in its own format.
- Sometimes a **lineage** (GEPA's parents, AutoSaddler's DAG, DGM's archive).

### 2.3 What goes wrong

- **Gains below the noise.** "Skill Issue" ([arXiv 2609.12742](https://arxiv.org/html/2609.12742))
  ran GEPA and SkillOpt on repository skills for Claude Code: +4.9 and +0.1 points, neither
  significant at 20 to 26 held-out tasks, after about $2,000. RRSI measures a noise band on
  the base harness first and accepts nothing inside it.
- **Overfitting to the tuning tasks.** Every system with a guard uses a held-out split.
  HarnessCompass and RRSI also reject edits that name specific instances or tests.
- **Cost creep.** RRSI found unregularized evolution grows token use; its cost gate cut
  policy tokens by 30%.
- **Editing the grader.** RobustSGPO and AutoSaddler hide graders, verifiers and traces
  from the editor.

hlock is in the right place to own all four: it already owns the pack, the graders, the
splits and the lock. The optimizer should own only the search.

## 3. Design

### 3.1 Optimizers are backends that run `harness.optimize`

An optimizer is a backend, found the same way ([backends](backends.md) §5): built in,
`[backends.<name>]`, `hlock-backend-<name>` on `PATH`, or an `hlock.backends` entry point.
It lists `harness.optimize` in `backend.describe`. `tune` picks it separately from where
trials run:

```toml title="harness.toml"
[backend]
target = "flyte"                 # where trials run

[tune]
optimizer = "gepa"               # who searches (default: "hlock", the built-in)
edit = ["instructions", "skills"]
proposer = "anthropic:opus"      # the optimizer's own model, not the task model
budget = { rollouts = 400, usd = 50 }
trials = 3                       # per task, per evaluation

[backends.harness2]
command = ["hlock-backend-harness2"]
options = { k = 2, mode = "sequential" }
```

`--optimizer`, `--edit`, `--budget-rollouts` and `--budget-usd` override them. `--depth
prompt` stays and means `edit = ["instructions"]` with the built-in optimizer.

Keeping optimizers as backends means one discovery path, one wire format, one
`hlock backend status`. Keeping `[tune] optimizer` separate from `[backend] target` means
Harness² can search while Flyte runs the trials.

### 3.2 The harness bundle

Before calling the optimizer, `tune` writes the base lock's editable surface as a directory:

```
.hlock/tune/<run>/seed/
  bundle.json
  instructions/AGENTS.md
  instructions/system-prompt.md
  skills/review/SKILL.md
  skills/review/scripts/check.sh
  tool-guidance/github.md
  subagents/reviewer.md
  guardrails/policy.md
```

```json title="bundle.json"
{
  "harness": "claude-code",
  "components": [
    {"id": "instructions/AGENTS.md", "kind": "instructions", "digest": "sha256:…", "editable": true},
    {"id": "skills/review", "kind": "skill", "digest": "sha256:…", "editable": true},
    {"id": "skills/review/scripts", "kind": "script", "digest": "sha256:…", "editable": false},
    {"id": "tool-guidance/github.md", "kind": "tool-guidance", "for": "mcp:github", "editable": false},
    {"id": "subagents/reviewer.md", "kind": "subagent", "editable": false},
    {"id": "guardrails/policy.md", "kind": "guardrails", "editable": false}
  ],
  "may_add": ["skill", "instructions"],
  "limits": {"max_component_bytes": 15360, "max_changed_lines": 120}
}
```

Component kinds and where they come from:

| Kind | From the lock | Editable in phase |
|---|---|---|
| `instructions` (system prompt) | `[[instructions]]`, a tuned prompt | 1 |
| `skill` | `[[skill]]` (SKILL.md and text files) | 1 |
| `tool-guidance` | New: a per-tool instructions file the harness loads with the tool | 2 |
| `guardrails` | Rendered from `[policy]` and `[[decider]]` policy files | 2 |
| `subagent` | New: needs subagent capture ([reviewable diffs](reviewable-diffs.md) §7) | 2 |
| `memory` | A memory seed ([memory](memory.md) §2.4), never the user's memory | 3 |
| `script`, `hook`, tool code | Skill scripts, hooks | 3, with a sandbox |

The bundle never contains the pack: no grader, no expected outputs, no test task, no
other trace. Text is redacted with `pin`'s rules before it's written.

A component's kind decides where it goes back: `tune` maps each file in the result to a lock
entry by its `id`, the same way `migrate` maps components onto a harness. A file of a kind
the target harness can't load (Codex has no subagents) is a preflight error, not a silent drop.

### 3.3 The request

`harness.optimize` uses the [backend envelope](backends.md) §4.2, contract version 4:

```json
{
  "version": 4,
  "kind": "harness.optimize",
  "backend": "gepa",
  "backend_options": {},
  "workdir": "/abs/.hlock/tune/7c1e/opt",
  "optimize": {
    "seed": "/abs/.hlock/tune/7c1e/seed",
    "tasks": {"train": ["count_words", "fix_typo", "…"], "dev": ["rename_fn", "…"]},
    "task_meta": {"count_words": {"prompt_text": "…", "group": "text"}},
    "evaluate": {"command": ["hlock", "tune", "eval", "--session", "7c1e"]},
    "budget": {"rollouts": 400, "usd": 50.0, "wall_s": 14400},
    "trials": 3,
    "models": {"task": "anthropic:sonnet", "proposer": "anthropic:opus"},
    "noise": {"dev_success_sd": 0.041}
  }
}
```

The held-out test split isn't in the request at all. The task list carries ids and prompts
for train and dev only; expected outputs stay with hlock.

### 3.4 The evaluation callback

The optimizer evaluates a candidate by calling hlock:

```console
$ hlock tune eval --session 7c1e CANDIDATE_DIR --split train --tasks count_words,fix_typo --trials 3 --json
```

```json
{"candidate": "sha256:4be1…", "results": [
  {"task": "count_words", "trial": 0, "success": true, "score": 1.0,
   "feedback": "answer.md matches", "trace": "/abs/.hlock/tune/7c1e/traces/4be1/count_words.0.jsonl",
   "cost_usd": 0.012, "tokens": 18342, "latency_s": 21.4}
], "budget": {"rollouts_used": 186, "rollouts_left": 214, "usd_used": 21.70}}
```

hlock overlays the candidate onto the base lock, runs the trials on `[backend] target`,
grades them with the pack's graders, and writes traces the optimizer may read. It:

- refuses `--split test`, and any task not in the request's train or dev list;
- counts rollouts and spend per session and refuses work past the budget
  (`budget exhausted: 400 of 400 rollouts`), so the budget holds whatever the optimizer does;
- caches by `(candidate digest, task, trial, seed)`, so re-evaluating the incumbent is free;
- returns the grader's textual feedback, which reflective optimizers (GEPA, ACE) need;
- validates the candidate against `bundle.json` first: only editable components changed,
  limits respected, no new component of a kind outside `may_add`. A violation is an error
  result, not a trial.

A Python optimizer can use `hlock.tune.Session(...).evaluate(candidate_dir, tasks, split)`
instead of the CLI, which is a thin wrapper over the same call. A GEPA adapter is then a
few dozen lines: `evaluate` maps to `session.evaluate`, and `make_reflective_dataset`
reads `feedback` and the trace.

An optimizer that brings its own executor (Harness² runs OpenCode or Codex itself) may skip
the callback. hlock then still owns the final gate (§3.6), and the tune lock records
`evaluator = "optimizer"` so a reviewer knows the search-time numbers came from elsewhere.

### 3.5 The response

```json
{"version": 4, "ok": true, "result": {
  "candidate": "/abs/.hlock/tune/7c1e/opt/best",
  "ledger": "/abs/.hlock/tune/7c1e/opt/ledger.jsonl",
  "optimizer": {"name": "gepa", "version": "0.9.2", "proposer": "anthropic:opus"},
  "usage": {"rollouts": 392, "proposer_usd": 6.40},
  "model_digests": {"run": "gepa-7c1e"}
}}
```

`candidate` is a bundle directory in the same layout as the seed. `ledger.jsonl` has one
line per accepted edit:

```json
{"component": "skills/review", "op": "modify", "parent": "sha256:…",
 "rationale": "Agents skipped tests before commenting; add an explicit step.",
 "evidence": {"split": "dev", "success_delta": 0.08, "tokens_delta": 1200},
 "native": {"gepa_candidate": 14}}
```

`component`, `op` (`modify`, `add`, `remove`) and `rationale` are required; the rest are
optional, and `native` holds the optimizer's own record (a DAG node, an ACE op, a Harness²
step id). hlock computes the diff itself from seed and candidate, so it doesn't trust the
optimizer's.

### 3.6 Acceptance belongs to hlock

The optimizer proposes; the core decides what reaches the tune lock:

1. **Noise band.** Before the search, `tune` re-runs the base lock on dev (`trials` per task)
   and records the spread. That's the `noise` sent in the request, and the threshold below.
2. **Paired test run.** The base lock and the candidate run on the held-out test split, same
   tasks, same seeds, `trials` each. The optimizer never saw these tasks.
3. **Accept** only when the candidate's success rate beats the base by more than the noise
   band and the paired difference is significant (a sign test over tasks, α = 0.05 by
   default), and its cost per task stays within `[tune] max_cost_increase` (default +25%).
4. **Leak check.** Before the test run, each changed component is scanned for task ids,
   fixture file names and expected-output strings from the pack. A hit rejects that
   component's edits, with the matched text shown.
5. **Ablate when it's cheap.** With `[tune] ablate = true`, `tune` reverts each changed
   component one at a time on the test split and keeps only the components that carry the
   gain. Off by default: it costs a test run per component.

A rejected candidate still writes the tune lock, with `accepted = false`, the numbers and
the reason, so a failed tune is evidence too. `tune apply` refuses a rejected tune unless
`--force`.

The test split comes from the pack's existing split (`evals check` already reports it). A pack
too small to hold out at least 20 tasks gets a warning naming the "Skill Issue" result: a
gain on fewer tasks is unlikely to be real.

### 3.7 What lands in the tune lock

Accepted edits are written as files under `.hlock/tuned/<digest>/` as
[reviewable diffs](reviewable-diffs.md) §2.1 describes, and recorded:

```toml
[tune.optimizer]
name = "gepa"
version = "0.9.2"
proposer = "anthropic:opus"
evaluator = "hlock"
edit = ["instructions", "skills"]
budget = { rollouts = 400, usd = 50.0 }
usage = { rollouts = 392, usd = 28.10 }
splits = "sha256:91aa…"            # train/dev/test task ids

[tune.acceptance]
accepted = true
noise-band = 0.041
test = { tasks = 32, trials = 3, base = 0.66, candidate = 0.75, p = 0.012 }
cost-usd-per-task = { base = 0.042, candidate = 0.047 }

[[tune.edit]]
component = "skills/review"
kind = "skill"
op = "modify"
before = "sha256:a6ce…"
after = "sha256:0b91…"
path = ".hlock/tuned/0b91f2e7/skills/review"
rationale = "Agents skipped tests before commenting; add an explicit step."
```

`hlock diff` shows each `[[tune.edit]]` as its text diff with the rationale above it, and
the acceptance block as the evidence line. `tune apply` turns each edit into the
corresponding lock entry (`[[instructions]]`, `[[skill]]` with `source = "tune"`).

### 3.8 The built-in optimizer

`hlock` (the default) replaces `prompt.search`. It is a small reflective loop in the
worker: run the incumbent on a train mini-batch through the callback, ask the proposer for
one bounded edit to one editable component from the failures and their feedback, rerun the
same mini-batch, keep the edit if it helps, and confirm on dev every few steps. Edits per
step start at 3 and decay to 1 (SkillOpt, RRSI). It exists so `tune` works with no extra
install; anything better is a backend.

`prompt.search` stays as a work item for one minor release. On contract version 4 the core
sends `harness.optimize` instead, and a backend that lists only `prompt.search` gets the old
item, with a deprecation note.

## 4. Backends we expect, outside this repo

| Optimizer | Package | Fit |
|---|---|---|
| GEPA | `hlock-gepa` (entry point) | Direct: components are `dict[str, str]`, `evaluate` is the callback. First external backend |
| Harness² | `hlock-backend-harness2` | Bundle kinds match its 8 components. Either replace its executor with the callback, or run with `evaluator = "optimizer"` and let hlock's gate decide. Its label-free search fits packs with weak graders |
| RRSI | `hlock-backend-rrsi` | A domain adapter over the callback; its ledger maps to `ledger.jsonl` |
| ACE | `hlock-ace` | Edits one `instructions` component; its op log goes in `native` |
| AutoSaddler | later | Needs code edits (phase 3) |

hlock documents the protocol and ships the GEPA adapter as the worked example, as Flyte is
for [backends](backends.md).

## 5. Non-goals

- **Editing tool code, hooks or the harness itself** before phase 3. Those are code
  execution by an optimizer, and need a sandbox and a review story of their own.
- **Training an editor model** (Harness-R1). A backend could do it; hlock doesn't.
- **Online, continual tuning** from production traffic. `tune` is a batch job over a pack.
- **Running several optimizers in one tune** and merging their results.

## 6. Phasing

1. Contract v4, `harness.optimize`, the bundle (`instructions`, `skill`), `hlock tune eval`
   with budget and caching, the acceptance gate, `[[tune.edit]]`, and the built-in
   optimizer in place of `prompt.search`. Depends on [reviewable diffs](reviewable-diffs.md) §2.1.
2. `tool-guidance`, `guardrails` and `subagent` kinds (needs subagent capture); the GEPA
   adapter; `ablate`.
3. `memory` seeds, scripts and hooks in a sandbox; code-editing optimizers.

## 7. As built (phase 1)

Where the implementation differs from §3:

- **§3.1 defaults.** `edit` defaults to `["instructions"]` from `--depth prompt` on, and the
  default depth (`auto`, which is `hpo`) is past `prompt`. So a plain `hlock tune` on `local` or
  `flyte` runs the built-in optimizer, within a 100-rollout default budget. `trials` defaults to 2.
- **§3.3 splits.** Train and dev come from the pack's search split: every third task by id is
  dev when there are at least 3, otherwise dev is train (with a note). Test is the pack's
  holdout split. A pack without one is checked on dev, and the acceptance records `split = "dev"`.
- **§3.4 the callback.** Results carry the trial's `workspace` directory, not a trace file.
  The Python wrapper is `hlock.optimize.Session`, not `hlock.tune.Session`: `hlock.tune` is
  already the tune function. Evaluations run one at a time; nothing locks the session against
  concurrent callers. The noise-band runs and the acceptance runs don't count against the budget.
- **§3.6 acceptance.** The noise band is the standard deviation, over trial seeds, of the
  lock's success rate on dev (0 with one trial). The leak check looks only at lines an edit
  added, for task ids, grader `equals`/`contains` strings and fixture file names, skipping
  strings under 6 characters unless they contain `_`. `ablate` isn't built (phase 2).
  A rejected candidate doesn't block `tune apply`: the tune lock carries no edits, and its
  grid result may still be worth adopting.
- **§3.7 recording.** `[tune.optimizer]` has `evaluator = "optimizer"` when the optimizer ran no
  trials through the session. An edit to an instruction file the lock had no entry for is
  recorded as `op = "add"`.
- **What a trial sees.** On `local` and `flyte`, a trial is one completion, so the candidate's
  instruction files are its system prompt (`stack.system_prompt`). Skills reach only code
  harness trials (`HLOCK_SKILLS_DIR`; instruction files in the new `HLOCK_INSTRUCTIONS_DIR`).
  Agent CLIs still run on the reference stand-in in `tune`, so an edit to a skill can't be
  measured for one yet. A declared dependency isn't editable.
- **§3.8 the built-in optimizer.** Without a proposer model or `HL_LLM_BASE_URL`, it tries a
  fixed list of general guidance, so it runs (deterministically) everywhere `local` does.

## 8. Open questions

- **Significance test.** A sign test over tasks is simple and assumption-free but weak on
  small packs. A paired bootstrap gives an interval to record. Which goes in the lock?
- **Who measures the noise band** when the optimizer evaluates itself (`evaluator =
  "optimizer"`): hlock, on dev, as usual, at extra cost?
- **Proposer model keys.** The proposer runs inside the optimizer's process. Does hlock pass
  it a key through the [secrets](secrets.md) mechanism, or does the backend bring its own?
- **Tool guidance on harnesses that can't override tool descriptions** (most of them, for MCP
  tools): an instructions section scoped to a tool may be the only portable form.
