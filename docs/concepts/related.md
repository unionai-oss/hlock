# hlock and related tools

A lot of good projects work on agent setups. Package managers install agent config,
dotfile managers and Nix reproduce it, eval frameworks measure it, and optimizers generate
it. hlock overlaps with several of them, and borrows from some. It isn't trying to replace
any of them. It has different devex goals:

- **Speed.** One command captures the setup you already have, with no manifest to write
  first (`hlock pin`), and the deterministic work runs in a Rust core.
- **Quick portability.** The whole stack goes in one file: the harness and the exact
  version and source it was installed from, the models and the local runtimes serving
  them, MCP servers, skills, plugins, instruction files, and config, with secrets replaced
  by placeholders. `sync` and `launch` turn that file back into a working, isolated setup
  on another machine, and `launch` can carry the portable parts into a different harness.
- **Tunability.** An [eval pack](../guides/eval-packs.md) of your own tasks turns the
  setup into something you can measure. `hlock resolve` runs candidate stacks against it
  and locks the winner with its success rate, cost and latency; `hlock tune` goes further
  into prompts and hyperparameters. A lock diff in a PR shows what changed in the agent
  *and* the evidence for it.
- **Let agents do the busywork.** Some packaging and syncing work needs judgment rather
  than deterministic code: installing what a lock needs on a machine you've never seen,
  filling in an eval pack with tasks from your repository, moving skills and MCP servers
  to their newest releases. [`hlock meta`](../guides/meta.md) hands those jobs to an
  agent, pinned in its own lock and kept out of your agent's config, then checks the
  result itself: a job verifies or exits non-zero.

Most of the tools below are good at things hlock doesn't attempt, and several work well
underneath or next to it. This page says what each one is good for and how hlock's goals
differ.

## At a glance

| Tool | Great for | What it pins | hlock's angle |
|---|---|---|---|
| [APM](https://github.com/microsoft/apm) | Declaring and installing agent context from git, across many harnesses, with org policy | Package commits and file hashes | Pins models, harness versions and runtimes too, and measures whether the setup works |
| [rulesync](https://github.com/dyoshikawa/rulesync), [Ruler](https://github.com/intellectronica/ruler) | One source of rules, fanned out to dozens of tools | Nothing (they translate) | A lock, with digests and drift checks |
| [`npx skills`](https://github.com/vercel-labs/skills) | The easiest way into the open skills ecosystem | Its own skills lock | Skills as one part of a whole stack |
| Claude Code [plugin marketplaces](https://code.claude.com/docs/en/plugin-marketplaces) and `claude plugin eval` | Team distribution inside Claude Code, and measuring one plugin with and without | Plugin `ref`/`sha` | Any harness, and whole-stack comparisons |
| [chezmoi](https://github.com/twpayne/chezmoi), [yadm](https://github.com/yadm-dev/yadm), [Stow](https://www.gnu.org/software/stow/) | Your dotfiles, on every machine | The bytes you wrote | Knows what the bytes mean to a harness |
| [Nix](https://github.com/NixOS/nix), [home-manager](https://github.com/nix-community/home-manager), [Devbox](https://github.com/jetify-com/devbox), [mise](https://github.com/jdx/mise) | Reproducible toolchains and binaries | Inputs, by hash | Pins outcomes: which stack, and how well it did |
| [Dev Containers](https://containers.dev), Docker | Isolated, consistent environments | An image | `hlock sync --docker` writes a Dockerfile from a lock |
| [Harbor](https://github.com/harbor-framework/harbor), [Inspect](https://inspect.aisi.org.uk), [promptfoo](https://github.com/promptfoo/promptfoo) | Running evals of agents, at scale, with good reporting | Nothing (they measure) | Search, selection, and a lock that records the result |
| [DSPy](https://github.com/stanfordnlp/dspy), [GEPA](https://github.com/gepa-ai/gepa), [Meta-Harness](https://github.com/stanford-iris-lab/meta-harness) | Generating better prompts and programs | Nothing (they optimize) | Chooses among existing harnesses and models, and makes the choice reproducible |

## Agent package and config managers

**[APM](https://github.com/microsoft/apm)** (Agent Package Manager, from Microsoft) is
the closest project to hlock, and the one we'd point you to first for distributing agent
context. An `apm.yml` declares skills, instructions, prompts, agents, hooks and MCP
servers from any git repository, subdirectory or single file. `apm install` resolves them
to commits and records per-file hashes in `apm.lock.yaml`, then deploys them into Copilot,
Claude Code, Cursor, Codex, OpenCode, Gemini and more. It also blocks hidden-Unicode
attacks on install, gates executables behind an explicit approval, and lets an
organization enforce an allow/deny policy in CI.

APM pins *what's installed*. It doesn't record which model runs, which harness version
runs it, or which local runtime serves it, and it doesn't measure whether a package helps.
hlock's lock covers those, and `resolve` measures. The two compose: hlock reads `apm.yml`
and `apm.lock.yaml`, resolves `apm:` packages by the same OpenAPM rules, and writes an
`apm.lock.yaml` that `apm install --frozen` accepts, so one team can use both
([APM packages](../guides/dependencies.md#apm-packages)). If you already use APM, start with
[Coming from APM](../getting-started/apm.md).

**[rulesync](https://github.com/dyoshikawa/rulesync)** and
**[Ruler](https://github.com/intellectronica/ruler)** keep one set of rules, MCP servers
and skills, and write each tool's native files from it. rulesync covers the most tools
we know of. They're the right answer when your team uses several agents and wants one
source of truth. They translate config rather than pin it: there's no lockfile and no
drift check against what's installed.

**[`npx skills`](https://github.com/vercel-labs/skills)** (Vercel) finds, installs and
updates Agent Skills across a long list of agents, and is the easiest way into the open
skills ecosystem. Skills it installs are picked up by `hlock pin` like any others.

**Claude Code [plugin marketplaces](https://code.claude.com/docs/en/plugin-marketplaces)**
distribute bundles of skills, agents, hooks and MCP servers to a team, pinned to a git
`ref` or `sha`. Its `claude plugin eval` runs eval cases against one plugin, with and
without it, and reports the difference and the cost. That's the right tool for "does this
plugin help Claude Code?". hlock pins and syncs those plugins (`[[native]]` in the lock),
and its question is the wider one: which whole stack wins on your tasks, across harnesses
and models.

**MCP registries** such as the [official MCP Registry](https://github.com/modelcontextprotocol/registry)
and [Smithery](https://github.com/arcadeai-labs/smithery-cli) are for discovering
servers. A lock records the package or URL each server came from.

**[AGENTS.md](https://agents.md)** is a file format, not a tool. hlock pins it like any
other instruction file, and `launch` carries it over between harnesses.

## Dotfiles and reproducible environments

**[chezmoi](https://github.com/twpayne/chezmoi)**, **[yadm](https://github.com/yadm-dev/yadm)**
and **[GNU Stow](https://www.gnu.org/software/stow/)** put the same files on every machine,
with templates, encryption and secrets-manager integration (chezmoi in particular is
excellent). They're a fine way to carry `~/.claude` or `~/.codex` around.

**[Nix](https://github.com/NixOS/nix)** is the strongest way to reproduce binaries, and
**[home-manager](https://github.com/nix-community/home-manager)** has modules for Claude
Code and Codex settings, skills and MCP servers. **[Devbox](https://github.com/jetify-com/devbox)**
(Jetify) gives you Nix-backed environments without learning Nix; it's unrelated to hlock's
`devbox` [backend](../guides/backends.md). **[mise](https://github.com/jdx/mise)** pins tool
versions, including npm-distributed agent CLIs, per project.
**[Dev Containers](https://containers.dev)** and Docker isolate the whole environment.

Use them for your toolchain and the rest of your dotfiles. hlock handles the agent part on
top: it knows which files a harness reads and what they mean.

## Evals

**[Harbor](https://github.com/harbor-framework/harbor)** (the harness behind
Terminal-Bench 2.0) runs agents such as Claude Code and Codex on benchmark tasks, from local
Docker to hundreds of cloud sandboxes. **[Inspect](https://inspect.aisi.org.uk)** (UK AISI)
is a rigorous eval framework with sandboxes, agent bridges to Claude Code and Codex, and an
excellent log viewer. **[promptfoo](https://github.com/promptfoo/promptfoo)** gives
declarative evals, red-teaming and side-by-side matrices, with providers for the Claude
Agent SDK and Codex. **[SWE-bench](https://github.com/SWE-bench/SWE-bench)** and
**[SkillsBench](https://github.com/benchflow-ai/skillsbench)** are public benchmarks; the
SkillsBench paper found curated skills help on average and hurt on about one task in five,
which is the case for measuring on your own tasks.

These are measurement engines. You write out the variants, they run them, and the decision
and its reproduction are up to you. hlock's [eval packs](../guides/eval-packs.md) are
smaller and focused on *your* tasks, and what it adds is the rest of the loop: a search
space in `harness.toml`, a selection rule (Pareto front over success and cost, constraints,
then cheapest or best), and a lock that ties the winner to its evidence.

## Optimizers

**[DSPy](https://github.com/stanfordnlp/dspy)** compiles prompts and few-shot examples for
programs written in DSPy. **[GEPA](https://github.com/gepa-ai/gepa)** evolves any text a
metric can score, including system prompts and tool descriptions.
**[Meta-Harness](https://github.com/stanford-iris-lab/meta-harness)** (Stanford) searches over
harness *code* with an agent as the proposer. They generate new candidates, and they're often
the right tool for writing a better prompt.

hlock mostly chooses among existing, discrete options: harnesses, models, decision models
and settings, and (with [`hlock tune`](../guides/tune.md)) prompts and hyperparameters. It
then makes the choice reproducible on another machine.

## Common questions

### Why not just keep my dotfiles in git, or use chezmoi?

If one machine's setup is all you need to copy, chezmoi does it well. What it copies is
bytes. hlock reads them through a [profile](../guides/harness-profiles.md) of each
harness, so it also:

- pins the harness version and how it was installed, and the models and local runtimes
  (an Ollama model's digest, say) that config points at;
- replaces secrets with placeholders and fills them from your keychain on the other side;
- reports drift as "the model changed from opus to sonnet", not as a changed file;
- restores into an isolated config root, so trying a setup never touches your own;
- carries skills, MCP servers and instructions over to a *different* harness
  (`hlock launch opencode` on a Claude Code lock).

Keep chezmoi for everything else. By default `hlock sync` writes into an isolated
directory (`.hlock/env/`), so it doesn't touch the files chezmoi manages unless you pass
`--in-place`.

### Why not Nix?

Nix pins inputs and reproduces bytes, better than anything else. An agent's behavior
depends on things outside any derivation: a model behind an API, which skills a team
actually uses, whether a new harness release made your tasks fail. A `flake.lock` can't say
whether the setup is any good. A resolved `harness.lock` carries the evidence for its
choice, and `hlock upgrade` re-measures when a newer model appears. Pin the toolchain with
Nix and the agent stack with hlock.

### Why not APM?

Use APM to distribute agent context: it's the most complete tool for that, with the
broadest harness coverage (prompts, agents and hooks, compiled per harness) and real
governance features. hlock's devex goals are different: speed, quick portability, and
tunability. [`hlock add`](../guides/dependencies.md) declares skills, MCP servers,
instruction files and APM packages with no Python or Node on the path, and `--eval`
answers whether one helps on your tasks. Its lock also covers the rest of the stack
(harness version, models, runtimes, config, secrets). Projects can use both: APM users keep
their `apm.yml`, hlock reads it, and `hlock export --format apm-lock` keeps `apm.lock.yaml`
in step. [Coming from APM](../getting-started/apm.md) walks through using the two together.

### Why not rulesync or Ruler?

They're the right tool for keeping one set of rules in step across many agents at once.
hlock pins and measures one stack per lock. If your team is split across harnesses, you can
use rulesync to generate each harness's files and hlock to pin the one you run, or run
`hlock launch <harness>` to try a lock's skills, MCP servers and instructions in another
harness.

### Why not promptfoo, Inspect or Harbor?

They're stronger eval engines than hlock's built-in one, and better for public benchmarks,
red-teaming and large-scale runs. They don't decide or pin: you get a table and pick by
hand, and nothing records that choice in a form another machine can rehydrate. hlock
closes that loop for the stacks it can run. Its eval packs are plain directories of tasks
and graders, so you can keep using the others alongside.

### Why not `claude plugin eval`?

If the question is "does this plugin make Claude Code better?", use it: it's built for
exactly that. hlock asks which harness, model and settings to run, across harnesses, and
locks the answer.

### Why not DSPy or GEPA?

They write new prompts and programs; hlock picks among stacks you can install and pins the
pick. A good combination: generate candidate instructions with GEPA, measure them alongside
your other options with hlock on a holdout split, and lock the winner.

## What we've learned from them

Several of these projects do things hlock should do too:

- [Declared dependencies](../guides/dependencies.md): APM's `install`-and-lock flow for
  skills and MCP servers, with a dependency resolver compatible with APM's `apm.yml` and
  `apm.lock.yaml`, plus measuring a dependency before you keep it.
- [Audit and trust](../guides/audit.md): APM's hidden-Unicode gate (the same tiers, and
  SARIF with APM's rule ids), executable approval and org policy, extended to models,
  providers, harness versions and evidence.

If you maintain one of these projects and something here is out of date or unfair,
[open an issue](https://github.com/unionai-oss/hlock/issues) and we'll fix it.
