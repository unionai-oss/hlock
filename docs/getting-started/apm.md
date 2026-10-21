# Coming from APM

**Keep `apm.yml`. hlock locks the rest of your agent, and tells you which setup works
best on your tasks.**

If you use [APM](https://github.com/microsoft/apm), you already treat your agent's context
like code: declared in `apm.yml`, pinned in `apm.lock.yaml`, reproduced with
`apm install`. hlock doesn't ask you to give any of that up. It reads your APM files as
they are, and adds two things APM doesn't attempt:

- **Measurement.** An [eval pack](../guides/eval-packs.md) of your own tasks lets hlock
  compare stacks (models, harnesses, settings, a new dependency) and lock the winner along
  with its evidence.
- **The rest of the stack.** The harness version, the models, the local runtimes serving
  them, and the harness's own settings all change how your agent behaves, and no package
  manager installs them. `hlock pin` records them next to what APM installed.

## What each one records

| | APM | hlock |
|---|---|---|
| Skills, instructions, MCP servers from git | ✓ `apm.lock.yaml`, by commit and file hash | ✓ reads `apm.yml` and `apm.lock.yaml`, or resolves `apm:` packages itself |
| Prompts, agents, hooks, compiled per harness | ✓ | Not installed from packages |
| Setup you built by hand (`claude mcp add`, your own skills, `settings.json` edits) | Once you declare it | ✓ `hlock pin` captures it as it is |
| Harness version and install source | | ✓ |
| Models and local runtimes (an Ollama model's digest, say) | | ✓ |
| Secrets | | Replaced by `HLOCK_SECRET_*` placeholders |
| Whether the setup works on your tasks | | ✓ success rate, cost and latency, in the lock |
| Org policy and install-time security gates | ✓ the more complete of the two | [`hlock-policy.toml`](../guides/audit.md#policy), the same pattern grammar |

APM is forward-looking: from the moment you route installs through it, it tracks them. hlock
also captures what's already there, so a setup that grew by hand over weeks gets a lock in
one command, with no manifest to write first.

## What hlock adds

### Measure before you choose

APM makes your context reproducible. It doesn't say which setup is best, and that question
has a real answer: the [SkillsBench](https://github.com/benchflow-ai/skillsbench) paper
found curated skills help on average but hurt on about one task in five. hlock measures on
*your* tasks:

- **[Eval packs](../guides/eval-packs.md)**: a small directory of your own tasks and
  graders. `hlock evals init --from-traces` drafts one from the sessions your agent has
  already logged for the project.
- **[`hlock add --eval`](../guides/dependencies.md#measure-before-you-keep-it)**: runs the
  eval pack with and without a new dependency (an `apm:` package included) and asks before
  keeping it. The lock records both scores.
- **[`hlock resolve`](../guides/resolve.md)**: list the models, harnesses and settings to
  try in `harness.toml`, and it measures every combination, then locks the winner by success
  rate, cost and latency:

    ```toml title="harness.toml"
    [evals]
    packs = ["./evals"]

    [candidates.llm.planner]
    any_of = ["anthropic:claude-opus-5-5", "anthropic:claude-sonnet-5-5"]

    [candidates]
    harness = ["claude-code"]

    [objective]
    primary = "success-rate"
    select = "min-cost"
    ```

    Real models run on a worker backend; see [Backends](../guides/backends.md).

- **[`hlock tune`](../guides/tune.md)**: goes further, into prompts, hyperparameters and
  fine-tuned small models, locally or on a Flyte cluster for GPUs.
- **[`hlock upgrade`](../guides/upgrade.md)**: re-measures when a new model or a new
  version of a dependency comes out, so you switch on data.

### Lock the parts APM doesn't see

`hlock pin` records the harness version and how it was installed, the default model,
local runtimes, MCP servers, skills, plugins, instruction files and settings, with each
file digested. The files `apm install` deployed are recorded as dependencies of their APM
package, so `sync` fetches them rather than copying them into the lock.

### Drift beyond packages

`apm audit` tells you when deployed context no longer matches `apm.lock.yaml`.
[`hlock check`](../guides/pin.md#keep-it-honest-with-check) covers what APM doesn't track,
in terms of what changed for the harness:

```console
$ hlock check
✗ drift: your claude-code setup changed since harness.lock was pinned
  ~ llm default: anthropic:opus → anthropic:sonnet
```

### Same context, another harness

[`hlock launch <harness>`](../guides/launch.md#launch-a-different-harness) starts a
different harness with the lock's skills, MCP servers and instructions carried over,
in an isolated config directory, so you can try `opencode` or `codex` on your APM-installed
context without changing your own setup.

## Use them together

Keep installing context with APM. Add hlock around it:

```bash
apm install                  # APM deploys skills and MCP servers, with its security gates
hlock pin                    # lock them with the harness version, models and settings
hlock check                  # later: has anything drifted?
hlock resolve                # with an eval pack and harness.toml: find the best stack
git add apm.yml apm.lock.yaml harness.lock
```

Commit `apm.lock.yaml` and `harness.lock` together. A pull request's diff then shows what
changed in the agent and, for a resolved lock, the evidence for the change.

hlock treats the `dependencies` in your `apm.yml` as declared, exactly as if they were in
`harness.toml`. In a project with only an `apm.yml`, `hlock pin` then `hlock check` notices
what isn't locked yet, and `hlock sync` resolves it:

```console
$ hlock check
✗ dependencies: 1 declared dependency differs
  unlocked declared in apm.yml, not in the lock  → hlock sync
$ hlock sync --yes
updated harness.lock for the declared dependencies
  + package context-engineering apm:github/awesome-copilot/plugins/context-engineering
Sync plan for Claude Code 2.1.285 → .hlock/env/claude-code/config
✓  harness  Claude Code 2.1.285
✓ in sync at .hlock/env/claude-code/config  your own config wasn't touched
use it: CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config claude
to restore into your own config instead: hlock sync --in-place
```

When `apm.lock.yaml` agrees with `apm.yml`, hlock takes its commits as given, so the two
tools never disagree about versions. See
[A project that already uses APM](../guides/dependencies.md#a-project-that-already-uses-apm).

### When some teammates use only APM

hlock can resolve `apm:` packages itself, in Rust, with no APM or Python on the path
(`hlock add apm:OWNER/REPO`). To keep teammates on `apm install` in step, write
`apm.lock.yaml` from the lock:

```console
$ hlock export --format apm-lock
✓ wrote apm.lock.yaml  (1 packages, 0 files, lockfile_version 1, target claude)
  not exported: harness, 1 model (APM has no place for them)
next → apm install --frozen   (should change nothing)
```

See [Teammates who use APM](../guides/dependencies.md#teammates-who-use-apm).

## What stays with APM

- **Governance.** APM's install-time security gates and org policy are ahead of hlock's.
  hlock [audits](../guides/audit.md) too (the same hidden-Unicode tiers and rule ids, and
  its own `hlock-policy.toml`), but it doesn't read APM's policy files.
- **Prompts, agents and hooks.** hlock doesn't install an APM package's prompts, agents
  or hooks; the lock counts what it left out. Keep `apm install` for those.
- **Compiling context per harness.** APM writes one package's context in each harness's
  own format, across the broadest set of harnesses we know of.

For the wider picture, see [hlock and related tools](../concepts/related.md).
