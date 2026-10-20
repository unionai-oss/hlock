# Going local with localize

`hlock localize` turns any lock (pinned, resolved, or tuned) into its best equivalent
that runs entirely on your own machine: a local-capable harness, local models served by a
runtime like Ollama or LM Studio, and the same MCP servers, skills and instructions.

```console
$ hlock localize
✓ wrote harness.local.lock  (from harness.lock)
╭─ 🏠 harness.local.lock · localized ─────────────────────────────────────────────────╮
│  harness        opencode  the most preferred local-capable harness installed        │
│  runtime        ollama 0.35.1  http://localhost:11434/v1                            │
│  llm · default  ollama:gemma3:12b                                                   │
│  memory         20.0 GB for weights  (128.0 GB, unified memory (75%))               │
│  candidates     gemma3:12b ✓, devstral:24b, gpt-oss:20b, gemma3:4b ✓, …  (✓ on disk) │
│  evidence       not evaluated (no eval packs); ranked by fit, tier, and what's on disk │
╰─────────────────────────────────────────────────────────────────────────────────────╯
  ~ harness claude-code → opencode
  ~ llm default: anthropic:opus → ollama:gemma3:12b
next → hlock sync harness.local.lock --pull  ·  hlock launch --lock harness.local.lock
```

The source lock isn't touched. `-o` names the output; `harness.local.lock` is the default.

## What changes

**The harness**, if it's proprietary or can't use a local model endpoint. Each harness
profile says whether it's `open-source` and whether it supports `local-models`. hlock keeps
the lock's harness when it qualifies; otherwise it picks the first installed one of OpenCode,
Pi, Codex, Hermes, and Goose. `--harness` chooses one yourself. A proprietary harness that can
use local models, like Claude Code, is allowed with a note. One that can't, like Cursor, is
refused.

**The models.** LLM roles and an LLM decision model on API providers move to a local runtime:
`--runtime ollama|lmstudio|llama-cpp|…`, or the first installed one. Candidates come from the
runtime's model catalog and from weights already on disk, and only models that fit this
machine's memory are considered: 75% of unified memory on Apple silicon, GPU memory with an
NVIDIA card, otherwise half of RAM, or `--memory-gb`. Ranking prefers stronger agentic models,
then weights you already have, then larger ones. The small and decision roles get the
smallest capable model. `--models a,b` replaces the catalog with your own list.

Self-hosted runtimes on your network count as non-local and are replaced, unless you pass
`--allow-network`.

**Decision models.** A hosted [decider](deciders.md) (`typesafe:`, `openai-decisions:`, …)
moves to Kev on the local runtime, when its catalog has Kev (Ollama's does): the 9B with
16 GB or more to spare, else the 4B. The localize report notes that its scores may move;
re-resolve to measure.

**What carries over:** MCP servers (written in the new harness's config shape), skills, and
instruction files. Remote MCP servers still need the network, so `localize` flags them;
`--strict` drops them. The old harness's plugins and settings don't carry over, and `localize`
lists them.

The new lock also configures the harness to use the local endpoint. For OpenCode, it adds an
`ollama` OpenAI-compatible provider and sets `model = "ollama/gemma3:12b"`. These settings are
merged into the harness's config when you sync or launch. When the harness stays the same,
they go into the config file the source lock already restores, so the new lock has one entry
per file, however many times you localize.

## Evaluate instead of guess

With eval packs, `localize` measures the top candidates instead of ranking them. Pass
`--evals ./evals`, or run it next to a `harness.toml` that has `[evals]`:

```bash
hlock localize --evals ./evals
```

It runs the candidates through the resolver with an all-local search space and records the
evidence, next to the source lock's, in the new lock. A [code harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts)
runs for real. Agent CLIs run on the backend's adapter for now, and `localize` says the
evidence is a stand-in.

## Then

The localized lock is an ordinary lock:

```bash
hlock sync harness.local.lock --pull      # install what's missing and pull the weights
hlock launch --lock harness.local.lock    # start the local harness
```

`launch` keeps each lock's environments separate (`.hlock/env/harness.local/` here), so the
localized setup never mixes with the original.

## Extend the catalogs

Model catalogs live in the runtime profiles (`[[catalog]]`), and the local-endpoint settings
live in the harness profiles (`[[local-provider]]`). Both are plain data you can override; see
[Harness profiles](harness-profiles.md).
