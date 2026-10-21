# Resolve and tune developer experience

Status: implemented · Owner: Niels Bantilan · Last updated: 2026-10-20 · Builds on [v0](v0.md) §5.9, §7

> `hlock tune --apply` keeps the base lock's setup instead of replacing it with a
> reference stack. The `[budget]` keys the docs show take effect. `resolve` reports
> numbers from tasks it didn't pick the winner on, shows what it compared, and says
> plainly when trials were simulated. A tune lock records only stages that actually ran.

---

## 1. Problem

I walked through `hlock init` → `hlock resolve` → `hlock show`/`check` → `hlock tune`
(probe, prompt, hpo, full) → `tune --apply` in scratch projects, using the hello-world pack
on the `local` backend (and `devbox` for one run). Then I read `resolver.rs`, `search.rs`,
`ops.rs`, `manifest.rs`, `hlock/tasks/worker.py` and the `resolve`/`tune` commands in
`hlock/cli.py`. The gaps below are verified, ranked by impact.

1. **`tune --apply` replaces `harness.lock` with a reference stack.** `tune` ignores the
   base lock's harness and models. Every grid stack is `reference:<model>` on the
   `reference` harness (`ops.rs` `tune`, `harness: spec.harness…unwrap_or("reference")`).
   `build_tuned_lock` starts from `Lockfile::default()` and carries over nothing from the
   base: no skills, MCP servers, config, deciders or harness version. `--apply` then saves
   that over `harness.lock`, with no backup. On a resolved `claude-code` lock:

    ```console
    $ hlock tune --depth probe --apply && hlock check
    ✗ error: harness.lock is for harness 'reference' but harness.toml targets 'claude-code'
    $ hlock show
      harness        reference
      llm · planner  reference:reference:qwen-7b
      backend
    ```

    The provider is doubled because `model` is stored as `stack.llm["planner"]`
    (`"reference:qwen-7b"`) under `provider = "reference"`. On a pinned lock the same
    `--apply` drops everything `pin` captured.

2. **The `[budget]` keys in the docs are ignored.** `Budget` is
   `#[serde(rename_all = "kebab-case")]` with no aliases (`manifest.rs:20`), so it reads
   `max-trials`, `max-spend-usd` and `random-seed`. `hlock init`, `docs/guides/resolve.md`,
   `docs/reference/manifest.md` and `docs/concepts/index.md` all write `max_trials`,
   `max_spend_usd` and `random_seed`. With `max_trials = 5`, `resolve` ran all 16 trials
   and printed nothing. With `max-trials = 5`, it ran 5 and warned. Unknown manifest keys
   are dropped silently. In addition:
    - `max-spend-usd` isn't enforced even when spelled correctly. Nothing outside
      `manifest.rs` reads it.
    - `budget.strategy` is only recorded (`resolver.rs:431`). Any value, including the
      code default `"auto"`, runs a grid search.
    - `objective.primary` is ignored (`search.rs:240`, `_primary`).
    - An unknown `objective.select` value (for example `"max-qualty"`) silently means
      `min-cost` (`search.rs:258`).

3. **`resolve` reports in-sample numbers and never runs the holdout split.**
   `resolve_lock` runs only the search tasks. `holdout()` is read only by `tune --depth
   full` (`ops.rs:536`). The winner is chosen on the same trials its success rate is
   reported from (`split = "search"`), so the lock overstates it: the more stacks
   compared, the more the best one's score is luck. `docs/concepts/index.md:141` says the
   holdout is "used to confirm the gain is real". On the hello-world pack, `count_words`
   never runs in `resolve`.

4. **The `devbox` backend can't pass a graded task.** The worker's `llm_run_trial`
   (`worker.py:130`) doesn't copy fixtures or a solution, and writes no files. It counts a
   trial as a success when the model's reply starts with `ANSWER_OK`, which only the
   reference stub does. Every trial with an expected artifact fails. `HL_LLM_MODEL`
   replaces the model for every candidate (`worker.py:62`), so "qwen-7b vs qwen-32b" both
   call the same model and get different labels. Real calls record a cost of `0.0`.

    ```console
    $ HL_PYTHON=… hlock resolve --backend devbox
    ✗ error: tuning error: no stack meets [objective.constraints]; the closest, … has
      success-rate 0.0000 …, missing success-rate >=0.5
    ```

    Nothing explains why every trial failed.

5. **A tune lock records stages that didn't run.** On `local` there is no worker, so the
   prompt, data and fine-tune stages are skipped. `build_tuned_lock` still writes them:
    - `[[tune.prompt.applied]]` with `_placeholder = true` and
      `gain.success-rate = "+1.00"`. That's the tuned stack's *absolute* success rate,
      formatted as a gain (`ops.rs`, `format!("{success:+.2}")`).
    - `[[tune.finetune]]` with `lora = "store://sha256:PLAN"`.
    - `[tune.data]` with `rows = 0`, even though `--data user` found no traces.

    The panel mentions none of this. `--depth full` on `local` prints the same panel as
    `hpo`. The tune lock also has `[provenance] backend = ""`, empty latency and
    `isolation = ""`, so `hlock show` prints a blank backend and `p50 —`.

6. **The tune baseline can be 0%, so `--strict` passes trivially.** The baseline is the
   base lock's recorded success rate. A pinned lock has none, so it's 0. Against a pinned
   `claude-code` lock, `hlock tune --depth probe --strict` passes with
   `success rate 0% → 100% +100%`. Against a resolved lock, it compares numbers from a
   different harness, grid and split.

7. **Tune's selection and budget are off.**
    - On a tie, `max_by` keeps the *last* grid entry. At `hpo` every configuration scored
      100%, and tune picked `retries = 3, temperature = 0.7`: the most retries, at no
      gain.
    - `--max-trials` ("Cap the number of trials") resets for each configuration (`n` is
      declared inside the grid loop), so the real cap is grid size × N.

8. **`hlock init` inside an agent pins a lock no harness can run.** Inside Claude Code,
   `hlock init` adds `[target] harness = "claude-code"` but keeps the reference candidates.
   `resolve` then writes a `claude-code` lock whose `llm` is `reference:qwen-7b`.
   `hlock launch --dry-run` passes no model flag and gives no note, so Claude Code runs on
   its own model while the lock says qwen-7b. The only hint is the post-resolve line
   "native claude-code execution isn't built yet, so the evidence is a stand-in". `hlock
   show` and `check` never repeat it, and the lock records it only as
   `adapter = "reference"`. The `docs/guides/resolve.md` walkthrough shows
   `harness reference`, which is what you get *outside* an agent. The hlock skill tells
   agents to run this walkthrough.

9. **`resolve` shows only the winner.** `build_resolve` takes the per-stack scores and
   ignores them (`_scores`). Output is one spinner, then the winner's panel: no progress
   count, no table of candidates, no runner-up. The guide's "Both 7B and 32B solved the
   demo tasks, so the cheaper 7B model won" can't be checked from the output.
   `.hlock/report.md` is written but never mentioned, and holds one line.
   `[provenance.search] candidates` records the winner's *trial count*, not the number of
   stacks (`resolver.rs:432`): with `seeds = 3` it says 6 for 4 stacks. Core warnings are
   bare `eprintln!("warn: …")` lines printed under the spinner. There's no `--json` and
   no `--dry-run` showing the stacks and trial count before spending.

10. **Repeat runs, workspace names and budget cuts.**
    - `resolve` doesn't use the trial cache that `hlock add --eval` has (`deps.rs`
      `measure`), so re-resolving after a manifest tweak re-runs every trial. With a code
      harness, that's every agent run again.
    - Trial workspaces are named `&hash[..10]`, and the hash starts with `sha256:`, giving
      directories like `.hlock/runs/sha256:253_greet_people_0`: three hex digits and a
      colon, which isn't valid in Windows paths.
    - More than 200 stacks are cut to 200 without a word (`resolver.rs:137`).
    - When `max-trials` cuts the run, the winner can be picked on 2 trials. The lock
      records `trials = 2` with no low-sample marker, and the warning goes only to
      stderr.

Out of scope, noted for later:

- Running agent CLIs (Claude Code, Codex, …) for real in `resolve` trials. This spec only
  makes the stand-in visible (§2.8). The eval-pack spec's `hlock evals run` covers trying
  a real agent on a task.
- Confidence intervals in evidence. §2.3 adds a sample-size marker only.
- Unverified: whether `tune --backend cluster` stages produce real digests. They need
  Flyte, which I didn't run.

## 2. Design

### 2.1 Tune starts from the base lock, and `--apply` keeps it

- The tune grid varies only what tune tunes: retries, temperature and tool timeout, plus
  the model when `[candidates.llm.planner]` lists more than one. Everything else comes
  from the base lock: harness and version, skills, tools, instructions, config, deciders
  and policy. With no base lock it falls back to the manifest's candidates, as today.
  Models keep their provider (`anthropic:…`), not `reference:` plus a stripped name.
- `build_tuned_lock` clones the base lock and overwrites `llm`, `policy`, `evidence`,
  `tune` and `kind`, setting `provenance.backend` and latency.
- `--apply` (and `tune fetch --apply`) writes `harness.lock.prev` first, as `upgrade`
  does, and refuses when the result's harness differs from `[target] harness`:

    ```console
    $ hlock tune --depth hpo --apply
    ✓ wrote harness.tune.lock
    ✓ applied to harness.lock  (previous kept in harness.lock.prev)
    ```

- `hlock tune apply [FILE]` adopts an existing `harness.tune.lock` without re-running.
  The `next →` hint after a tune points at it instead of `hlock tune --apply`, which
  re-runs the whole search.

### 2.2 Manifest keys take effect, or are reported

- `Budget` accepts both spellings (`#[serde(alias = "max_trials")]` and so on), and the
  docs and `hlock init` write the kebab-case form used everywhere else
  (`timeout-seconds`, `key-env`).
- `max-spend-usd` is enforced. Trials stop once spend so far plus the next trial's
  expected cost (the mean so far) would exceed it, with the same round-robin rule as
  `max-trials`, and the same warning.
- `Manifest::check` (called by `resolve`, `tune` and `hlock check`) reports unknown keys,
  naming the closest match, the same way `evals check` does for `pack.toml`.
- `strategy` other than `grid`, `primary` other than `success-rate`, and `select` other
  than `min-cost` or `max-quality` are errors that list the supported values. The code
  default becomes `"grid"`, matching the docs.

### 2.3 `resolve` validates the winner on the holdout

After selection, `resolve` runs the winner, and only the winner, on the holdout tasks for
every seed. Evidence reports the holdout result, with the search result alongside:

```toml
[evidence]
split = "holdout"
trials = 6

[evidence.success-rate]
mean = 0.83
search = 1.0          # what the winner scored where it was picked
```

With no holdout tasks (fewer than 2 tasks, or `splits.search = 1.0`), evidence stays
`split = "search"`, and `resolve` warns that the numbers are in-sample. When the winner
ran fewer than 10 trials, `resolve` and `show` mark the evidence `low sample (n=4)`.

### 2.4 The worker grades real work, or says it can't

`llm_run_trial` gets the task's fixtures, copied into the trial workspace by the Rust side
before the call, as the reference adapter does. Then:

- **With no `HL_LLM_BASE_URL`**, `devbox` fails before any trial: `the devbox backend
  needs a model endpoint: set HL_LLM_BASE_URL (see Backends)`. It doesn't silently drop to
  the reference stub, where every graded trial fails.
- **With an endpoint**, the trial is an LLM answer, not an agent run. The reply is written
  to the task's first expected artifact (or `answer.md`), and success comes from the
  graders, not from an `ANSWER_OK` prefix. The guide says this is a single completion, not
  an agent loop, and points to code harnesses for agent runs.
- `HL_LLM_MODEL` applies only when the candidate's provider is `reference`. Real
  candidates call their own model.
- Cost comes from the response's usage and the price table `decide` already uses, or is
  recorded as unknown, not `0.0`.
- When every trial fails, the "no stack meets" error adds the first trial's grade detail
  (for example, `greetings.md is missing`).

### 2.5 A tune lock records what ran

A stage that didn't run isn't written. `[tune.prompt]`, `[tune.finetune]` and
`[tune.data]` appear only with real results. The panel lists skipped stages:

```console
  stages   data ✗ no traces in . · prompt ✗ needs devbox · hpo ✓ 12 configs
```

`gain.success-rate` is the tuned score minus the score without the tuned prompt,
measured, or omitted. `--depth ft`/`full` on `local` warns before running that only the
HPO grid will run. `--data user` with no traces found warns, naming where it looked.

### 2.6 A tune baseline is measured, not assumed

Before the grid, tune runs the base lock's own configuration on the same tasks and seeds
and uses that as the baseline. `--strict` compares against it. The panel says where the
baseline came from (`baseline 75%, measured`). `harness.lock`'s recorded number from
another split isn't used.

### 2.7 Tune picks the simplest of equals, and caps total trials

Ties go to the configuration closest to the base: fewer retries, then the base
temperature, then the base model. `--max-trials` caps the total, spent round-robin across
configurations like `resolve`'s `max-trials`.

### 2.8 Init and resolve agree on the target

- Inside a harness, `hlock init` writes `[target]` together with candidates that harness
  can run: its models from the profile, with a comment pointing to the reference demo
  (`--harness reference` for the offline walkthrough).
- `resolve` refuses a winning stack whose models the target can't run (a `reference:`
  model on an agent CLI), naming the conflict, unless `[target] harness = "reference"`.
- When trials ran on the reference adapter for an agent CLI, `hlock show` and `hlock
  check` repeat the stand-in note, and `--json` carries `"simulated": true`.
- The resolve walkthrough states it uses `hlock init --harness reference`, so its output
  matches inside an agent too.

### 2.9 `resolve` shows its work

```console
$ hlock resolve
Resolving harness.toml · 4 stacks × 2 tasks × 2 seeds = 16 trials
  [██████████░░░░░░] 10/16  claude-code/qwen-32b/rules  greet_people#1
stack                         success  cost/task  p50     trials
reference/qwen-7b/none   ★    100%     $0.0001    0.59s   4
reference/qwen-7b/rules       100%     $0.0001    0.61s   4
reference/qwen-32b/none       100%     $0.0004    0.62s   4
reference/qwen-32b/rules      100%     $0.0004    0.60s   4
holdout: reference/qwen-7b/none 100% on 2 trials
✓ wrote harness.lock
```

- A progress callback from the core replaces the single spinner. Core warnings come back
  as structured notes that the CLI prints with `!`.
- `--dry-run` prints the stacks and trial count and exits. `--json` prints the lock plus
  per-stack scores.
- `[provenance.search]` records `candidates` (the number of stacks) and `trials`
  separately. `report.md` gets the table and the path is printed.

### 2.10 Cache, names, and cuts

- `resolve` uses the `hlock add --eval` trial cache, keyed on stack, task, seed and the
  eval-pack digest. `--no-cache` forces a re-run. The panel says how many trials were
  cached.
- Workspaces are named from the hash's hex digits (`<12 hex>_<task>_<seed>`).
- Cutting more than 200 stacks is an error that names the count and suggests narrowing the
  candidates, not a silent truncation.

## 3. Changes

| Where | What |
|---|---|
| `ops.rs` | tune from the base lock (§2.1); measured baseline (§2.6); tie-break and total `--max-trials` (§2.7); no placeholder stages, real gain (§2.5); provider kept; `.prev` on apply |
| `manifest.rs` | `[budget]` aliases; unknown-key check; strategy/primary/select validation (§2.2) |
| `resolver.rs` | holdout validation (§2.3); spend budget; progress and notes; trial cache; workspace names; `candidates`/`trials`; refuse >200 stacks; target/model conflict (§2.8–2.10) |
| `search.rs` | `select_winner` rejects unknown `select`/`primary`; low-sample marker |
| `lockfile.rs` | `success-rate.search`; `simulated` in evidence |
| `worker.rs`, `hlock/tasks/worker.py` | fixtures in the request, reply written as artifact, grader-based success, `HL_LLM_MODEL` scope, real cost, no silent stub (§2.4) |
| `harness-lock-py` | progress callback; per-stack scores in `resolve` output; `tune apply` binding |
| `hlock/cli.py` | `resolve --dry-run/--json`, progress and table; `tune apply`; stage list in tune panel; stand-in note in `show`/`check`; `init` candidates for the target |
| `hlock/jobs.py` | `tune fetch --apply` writes `.prev` |
| docs | `resolve.md` (walkthrough harness, holdout, table), `tune.md` (stages, baseline, apply), `backends.md` (devbox is a single completion), `manifest.md` and `concepts/index.md` (kebab-case budget keys, holdout) |
| `skills/hlock` | `tune apply`, and the resolve walkthrough's `--harness reference` |

## 4. As built

Where the implementation differs from §2, and what's left:

- **§2.8 `init`.** Profiles don't list the models a harness runs, so `init` can't write
  runnable candidates on its own. Inside an agent, `hlock init` writes the reference demo
  (no `[target]`) and says how to target the agent. `hlock init --harness <agent CLI>` takes
  the candidate models from a `harness.lock` that pins that harness, and otherwise writes an
  empty `any_of` with a comment. `resolve` then fails with "list the models … runs".
- **§2.10 cache.** A code harness's trials aren't cached, because hlock can't tell when
  the agent's code changed. The key covers the stack, task, seed, pack digest, backend,
  judges, and `HL_LLM_BASE_URL`/`HL_LLM_MODEL`.
- **§2.2/§2.3 budgets.** The holdout check counts toward `max-spend-usd`, not
  `max-trials`. Tune's `--max-trials` caps the grid and doesn't count the baseline.
- **§2.4 cost.** The worker reports `cost_usd: null` when the endpoint gives no usage
  cost, but `RunOutcome.cost_usd` is still a number, so evidence counts it as 0. Recording
  unknown cost needs an optional cost through `adapters.rs`, `search.rs` and the
  resolver. Left for later.
- **§2.5.** `--data user` with no traces warns, but without naming where it looked (the
  worker's `data.load` doesn't report that).
- `resolve` replaces a *pinned* `harness.lock` without asking, which drops what `pin`
  captured. That predates this spec and isn't covered by it; noted for later.
