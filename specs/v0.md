# harness-lock — Specification (draft v0)

> **Lock your agent into place.**
> Bring your domain tasks, the models you're allowed to use, and the harness you like.
> `harness-lock` searches the stack, measures it, and writes an `harness.lock`: a pinned,
> version-controlled, enforceable description of the agent that actually works for your domain.

Status: draft · Owner: Niels Bantilan · Last updated: 2026-10-16

> **Implementation note (rust pivot).** The core is implemented in **Rust**
> (`crates/harness-lock-core` and the `crates/harness-lock` bindings crate, published
> to crates.io), with **Python bindings** (PyO3) exposing the same Layer 0/1 API
> (`import harness_lock as hlock`, CLI `hlock`) so the user surface stays as specified
> here. The Rust core stays the orchestrator/aggregator; ecosystem- and
> GPU-heavy work (real LLM calls, pandas/polars trace loading, synthetic data,
> grading, prompt search, LoRA) runs in the Python worker
> (`harness_lock/tasks/worker.py`), which the core drives out-of-process over a
> versioned JSON contract and which in turn raises each body to a real Flyte
> task on `devbox`/`cluster` (Python `flyte` / `flyteplugins-rs`;
> `~/.flyte/config.yaml`). A [flyte-sdk-rs](https://github.com/flyteorg/flyte-sdk-rs)
> backend also exists behind the `flyte` cargo feature (provisional). A reference
> pure-Python implementation is preserved in `python-legacy/`.

---

## 1. Summary

`harness-lock` is a Python package (plus a family of open-weights models and datasets) that
treats an agent system the way a package manager treats dependencies:

| Package manager            | harness-lock                                                        |
|----------------------------|-------------------------------------------------------------------|
| `pyproject.toml`           | `harness.toml`: the **manifest**. What you allow and require.   |
| dependency resolver        | **resolver**: search over LLMs × decision models × harnesses × policies, scored against domain tasks |
| `uv.lock` / `Cargo.lock`   | `harness.lock`: the **lockfile**. Exactly what was chosen, pinned and hashed, plus the evidence for choosing it |
| `uv sync`                  | `hlock sync`: materialize the locked stack in an environment |
| `uv lock --upgrade-package`| `hlock update <component>`: re-resolve part of the stack and diff the evidence |
| CI `--locked` check        | `hlock check` / `hlock verify`: fail if the running stack drifts from the lock |
| `uv add` / `uv tree`       | `hlock add skill …` / `hlock tree`: add and inspect the stack's dependencies |

As in a package lock, the stack has **dependencies**, and they are resolved, pinned, and
hashed like packages. Models and the harness are dependencies, and so are the things the
harness plugs in: tools, MCP servers, skills, instruction files, subagents, hooks, commands,
plugins, and harness-native extensions such as Pydantic AI *capabilities* (§5.3, §7.6).

The tuning backend runs on **Flyte 2** for durability (retries, caching, resumability,
fan-out, lineage), with a single dial for how much infrastructure it uses:

```
local  ───────────►  devbox  ───────────►  cluster (self-hosted Flyte or Union)
pure Python,          local Flyte in        remote scale, GPUs, many
zero setup            Docker: UI, durable   concurrent trials, shared cache
                      state, resume
```

Users never see Flyte unless they deliberately go down to the lowest API layer (§9).

**Positioning in one line:** harness-lock is *AutoML for agent systems*, or more precisely a
*domain agent compiler*. Give it representative tasks, available models, tools, and
harnesses. It searches the space, evaluates the traces, trains lightweight decision
policies, and locks the best agent system for your domain.

**Open-source first, with an upgrade path.** The "anyone can try it" path is fully local
and needs no account. The "why scale matters" path (durability, caching, lineage, HITL,
retries, GPUs, team-shared results) is the same program on a Flyte or Union cluster.
*Run locally in 10 minutes. Scale up when it matters.*

---

## 2. Problem and motivation

### 2.1 The problem

Teams building agents today make a set of coupled decisions mostly by vibes:

- **Which LLM(s)?** Frontier API, open weights on vLLM, local on Ollama, or a mix.
- **Which harness?** OpenCode, Pi, Claude Agent SDK, Codex CLI, LangGraph, PydanticAI,
  OpenAI Agents SDK, Hermes, a homegrown loop.
- **Where do cheap decisions go?** Routing, retry, escalate-to-human, stop, tool choice.
  A small decision model can often replace a frontier LLM call here.
- **Which policies?** Retries, timeouts, sandboxing, approval gates, memory, concurrency.
- **Which steps should stop being agentic** and become deterministic workflows?

Even after a team settles on a good configuration, it is **not reproducible**:

- Model aliases move (`claude-sonnet-latest`, `gpt-5`), and providers retire snapshots.
- Harness versions, system prompts, tool schemas, and MCP server versions change quietly.
- No artifact records *why* a configuration was chosen or how well it scored.
- Nothing fails in CI when the running agent no longer matches what was evaluated.

The result is agents that degrade silently and can't be rolled back, diffed, or audited.

### 2.2 Why now

- **The question has shifted** from "which model is best?" to "which model + harness +
  tools actually completes *my* tasks reliably?" Evaluation is moving from static QA toward
  traces, harness changes, and replay loops. Nobody yet ships an artifact that captures the
  answer and keeps it true over time.
- **Customer signal (Union).** Recurring asks from Union customers that harness-lock addresses
  directly:
  - Flyte-native agent harnesses with tool calling, MCP, memory, and human-in-the-loop.
  - Hermes and other third-party harness integration with Flyte.
  - LangGraph agents whose node functions run as Flyte tasks.
  - Open-source Flyte MCP work.
  - Sandboxed code execution and "MLE bot" style demos.
  - Agent/actor throttling, mapped (fanned-out) agent tasks, native SDK integration.
  - Dataset snapshotting/versioning, model-factory workflows, eval and observability.
- **Why an open artifact rather than a fine-tuned model alone.** A model on its own is hard
  to make broadly useful unless it beats frontier models or owns a narrow niche. A tool
  that produces comparisons, leaderboards, and a sharable lockfile has far more surface
  area, and the narrow open models (§11) ride along as companion artifacts.

## 3. Goals and non-goals

### Goals

1. **One sharable artifact.** `harness.lock` fully describes a tuned agent stack and the
   evidence behind it. It is human-readable, diffable in a PR, and machine-enforceable.
2. **Domain-specific tuning.** Tune against *your* tasks (an eval pack), not a generic
   leaderboard.
3. **Harness-agnostic.** Pluggable adapters for existing harnesses; no new agent framework
   to learn.
4. **Runs anywhere.** `pip install harness-lock` and lock on a laptop with no services.
   The same code path scales to a devbox or a cluster by changing one flag.
5. **Layered devex.** Flyte powers the backend but stays invisible until a user needs to
   customize compute or durability.
6. **Open artifacts.** Open-weights decision/judge models, open eval packs, an open trace
   dataset, all under permissive licenses.
7. **Updateable.** Partial re-resolution (`update`), evidence diffs, and drift detection
   make it safe to move to new models and harness versions.

### Non-goals (v0–v1)

- Building a new general-purpose agent harness. The built-in `reference` harness exists only
  for tests and as a baseline.
- Hosting inference. harness-lock calls providers or the user's own endpoints.
- Training frontier-scale models. Fine-tuning is limited to small decision/judge models
  and, optionally, LoRA adapters on open-weights LLMs.
- Guaranteeing bit-exact determinism for closed API models. The lock pins what *can* be
  pinned and fingerprints the rest (§5.5).

---

## 4. Core concepts

| Term | Definition |
|---|---|
| **Stack** | One concrete agent configuration: LLM assignment(s) + decision model + harness + tools + prompts + policies + runtime. |
| **Component** | One axis of a stack (e.g. `llm.planner`, `decision.router`, `harness`, `policy.retries`). |
| **Manifest** (`harness.toml`) | User-authored constraints: candidate components, eval packs, objectives, budget, backend. |
| **Search space** | The set of stacks implied by the manifest's candidates and constraints. |
| **Eval pack** | Versioned set of domain tasks + fixtures + graders. The unit of evaluation. |
| **Trial** | One `(stack, task, seed)` execution. Produces a trace and grades. |
| **Trace** | Structured record of a trial: model calls, decisions, tool calls, observations, artifacts, costs, timings. |
| **Grader** | Scores a trial: deterministic checks, artifact checks, or an LLM/judge model. |
| **Decision model** | A small model (or rule set) that makes structured choices inside the harness loop: route, retry, escalate, stop, tool choice. |
| **Resolver** | The search + evaluation + selection engine that turns a manifest into a lock. |
| **Lock** (`harness.lock`) | The resolved stack, fully pinned, with hashes, provenance, and evaluation evidence. |
| **Adapter** | Plugin that teaches harness-lock how to configure, run, trace, and enforce a specific harness. |
| **Backend** | Where the resolver executes: `local`, `devbox`, or `cluster`. |
| **Peripheral** | Anything a harness plugs in besides the model: tools, MCP servers, skills, instruction files, subagents, hooks, commands, plugins, LSP servers, memory, retrieval indexes, etc. (§5.4). |
| **Component kind** | A typed category of lockable dependency (`skill`, `mcp`, `pydantic-ai.capability`, …) with its own schema, resolver, hasher, materializer, and checker (§5.3, §7.6). |
| **Portable kind** | A kind defined by harness-lock (or an open standard) that adapters can install into *any* supporting harness, e.g. Agent Skills, MCP servers, `AGENTS.md`. |
| **Native kind** | A kind that only exists in one harness, registered by its adapter under a namespace, e.g. `pydantic-ai.capability`, `opencode.plugin`, `pi.extension`, `claude.hook`. |
| **Bundle** | A component that expands into other components (Claude Code plugin, Pi package, a Pydantic AI capability that brings tools + instructions + hooks). |

### 4.1 The loop

```
 harness.toml ─┐
 eval packs ─────┤
                 ▼
          ┌─────────────┐   trials (fan-out)   ┌──────────────┐
          │   planner   │ ───────────────────► │ run + trace  │  ← adapters run each harness
          └─────────────┘                      └──────┬───────┘
                 ▲                                    ▼
                 │  next candidates            ┌──────────────┐
                 └──────────────────────────── │    grade     │  ← graders / judge model
                                               └──────┬───────┘
                                                      ▼
                         (optional) train decision model on traces
                                                      ▼
                                               ┌──────────────┐
                                               │ select + pin │ ──► harness.lock
                                               └──────────────┘      + traces/ + report
```

---

## 5. The `harness.lock` file

`harness.lock` is the central artifact. It has three jobs:

1. **Document.** What the agent is, and the evidence that it works.
2. **Enforce.** What the runtime must match. `check`, `sync`, and the runtime guard read it.
3. **Version.** Diffable in git, partially updateable, reproducible from the manifest.

### 5.1 Format

- **TOML**, generated, deterministic key ordering, stable formatting (so diffs are minimal).
- Begins with a `# This file is generated by harness-lock. Do not edit by hand.` header.
- Large content (prompts, tool schemas, traces, weights) is **referenced by content hash**,
  never inlined. Blobs live in `.harnesslock/store/` (local) or a configured object store,
  addressed as `sha256:<digest>`.
- Secrets are **never** written. Credentials are referenced by name (`env:ANTHROPIC_API_KEY`).
- `lock-version` is the schema version. Readers must reject unknown major versions.

### 5.2 Example

```toml
# This file is generated by harness-lock. Do not edit by hand.
lock-version = 1
generated-by = "harness-lock 0.3.0"
generated-at = "2026-10-16T14:02:11Z"
manifest-hash = "sha256:9f1c…"          # harness.toml that produced this lock

[stack]
name = "ml-eng-debugger"
selection = "pareto:min-cost@quality>=0.85"   # how the winner was chosen
harness = "opencode"

# ---------- LLMs ----------
[[llm]]
role = "planner"
provider = "anthropic"
model = "claude-sonnet-5-5"              # never an alias; aliases are resolved at lock time
resolved-from = "anthropic:claude-sonnet-latest"
params = { temperature = 0.2, max_tokens = 8192 }
fingerprint = "sha256:41aa…"             # behavioral fingerprint, see §5.5

[[llm]]
role = "executor"
provider = "vllm"
model = "Qwen/Qwen3-32B"
revision = "a1b2c3d4…"                   # HF commit
weights = "sha256:77e0…"                 # safetensors manifest digest
adapter = { lora = "hf://acme/qwen3-32b-mleng-lora", revision = "e5f6…", weights = "sha256:0c3d…" }
serving = { engine = "vllm", engine-version = "0.11.2", dtype = "bf16" }
params = { temperature = 0.0, max_tokens = 4096, seed = 7 }
fingerprint = "sha256:b902…"

# ---------- Decision model ----------
[decision]
kind = "model"                           # model | rules | llm
model = "hf://harness-lock/decider-1.5b"
revision = "3c9e…"
weights = "sha256:5d1a…"
adapter = { lora = "store://sha256:8e7f…" }   # tuned on this project's traces
decisions = ["route", "retry", "escalate", "stop"]
thresholds = { escalate = 0.72, stop = 0.90 }

# ---------- Harness ----------
[harness]
name = "opencode"
version = "1.4.2"
source = "npm:opencode-ai@1.4.2"
integrity = "sha512-…"
adapter = "hlock.adapters.opencode@0.3.0"
config = "sha256:2b77…"                  # rendered harness config blob
system-prompt = "sha256:c41e…"
pattern = "plan-act-reflect"

# ---------- Tools ----------
[[tool]]
name = "python"
kind = "builtin"
schema = "sha256:aa01…"

[[tool]]
name = "github"
kind = "mcp"
source = "npm:@modelcontextprotocol/server-github@2025.9.1"
integrity = "sha512-…"
schema = "sha256:f00d…"                  # hash of the tool list the server advertised at lock time
permissions = ["read:repo", "write:pull_request"]

# ---------- Skills (Agent Skills / SKILL.md), see §7.7 ----------
[[skill]]                                # third-party, OSS vendor catalog
name = "pdf"
origin = "third-party"
source = "git+https://github.com/anthropics/skills@9b1e2f0#path=skills/pdf"
registry = "skills.sh:anthropics/skills/pdf"   # how it was found; the git commit is what's pinned
digest = "sha256:51c2…"                  # tree digest: SKILL.md + scripts/ + references/ + assets/
spec = "agentskills/1"
license = "Proprietary (see LICENSE.txt)"
frontmatter = "sha256:0e9d…"             # name, description, compatibility, allowed-tools, metadata
allowed-tools = "Bash(python:*) Read"    # mapped onto [policy]; anything broader is a lock error
executes = ["scripts/fill_form.py", "scripts/extract.py"]
runtime-deps = "sha256:aa7c…"            # pinned pip/npm/system deps baked into the sandbox image
scan = { tool = "harness-lock-scan@0.2.0", result = "pass", report = "store://sha256:c3f1…" }
install = "native"                       # adapter decides the path (.opencode/skills/, .claude/skills/, …)
requires = ["tool:python"]               # edges in the dependency graph

[[skill]]                                # first-party, in this repo
name = "flyte-sdk"
origin = "first-party"
source = "path:./skills/flyte-sdk"
digest = "sha256:8f42…"
editable = true                          # like `pip -e`: edits mark evidence stale instead of failing check
evidence-digest = "sha256:8f42…"         # digest the evidence was measured with

[[skill]]                                # first-party, private org repo
name = "pytorch-debugging"
origin = "first-party"
source = "git+ssh://git@github.com/acme/agent-skills@4f2a9c1#path=skills/pytorch-debugging"
digest = "sha256:7c10…"
signature = { kind = "sigstore", identity = "https://github.com/acme/agent-skills/.github/workflows/release.yml@refs/tags/v1.3.0" }
requires = ["tool:python"]

[[skill]]                                # shipped inside the harness: not installed, but still locked
name = "skill-creator"
origin = "harness-builtin"
provided-by = "harness:codex-cli"
digest = "sha256:19de…"                  # read from the installed harness at lock time
enabled = false                          # built-ins are explicitly enabled or disabled, never implicit

[[skill]]                                # arrived inside a bundle (plugin / package)
name = "repo-conventions"
origin = "bundled"
provided-by = "native:opencode-safe-git"
digest = "sha256:6b0a…"

# ---------- Instructions ----------
[[instructions]]
name = "AGENTS.md"
digest = "sha256:3e5b…"
scope = "project"

# ---------- Context isolation ----------
[context]
isolation = "locked-only"                # ignore ~/.config/opencode, ~/.claude, ~/.codex, ~/.pi, etc.
compaction = { strategy = "harness-default", threshold = 0.85 }
tool-output-limit-tokens = 20000

# ---------- Subagents ----------
[[subagent]]
name = "test-runner"
llm = "executor"                         # reference a locked [[llm]] role
tools = ["python", "filesystem"]
prompt = "sha256:9a0e…"

# ---------- Harness-native components (namespaced by adapter) ----------
[[native]]
kind = "opencode.plugin"
name = "opencode-safe-git"
source = "npm:opencode-safe-git@0.4.1"
integrity = "sha512-…"

[[native]]
kind = "opencode.lsp"
name = "pyright"
source = "npm:pyright@1.1.405"
integrity = "sha512-…"

# ---------- Policies (enforced at runtime) ----------
[policy]
retries = 1
tool-timeout-seconds = 120
max-steps = 40
max-cost-usd-per-task = 1.50
sandbox = { enabled = true, image = "ghcr.io/harness-lock/sandbox-py@sha256:19ab…", network = "egress-allowlist" }
human-approval = "on_risky_action"
risky-actions = ["filesystem.write:/", "github.merge", "shell:rm"]
tool-allowlist = ["python", "github", "filesystem"]

# ---------- Evaluation evidence ----------
[[evidence]]
eval-pack = "harness-lock/ml-engineering"
eval-pack-version = "0.2.0"
eval-pack-hash = "sha256:6aa2…"
split = "holdout"
trials = 150                             # 50 tasks × 3 seeds
success-rate = { mean = 0.87, ci95 = [0.81, 0.92] }
cost-usd-per-task = { mean = 0.34, p95 = 0.81 }
latency-s-per-task = { p50 = 41.0, p95 = 112.0 }
human-intervention-rate = 0.06
traces = "store://sha256:e3d9…"          # trace dataset for this evidence
report = "store://sha256:01fe…"

[evidence.baseline]                      # what the winner beat, for context in diffs
stack = "anthropic:claude-sonnet-5-5 + opencode, no decision model"
success-rate = 0.89
cost-usd-per-task = 1.12

# ---------- Drift guards ----------
[verify]
canary-tasks = 12                                # subset re-run by `hlock verify`
min-success-rate = 0.80                  # fail verify below this
fingerprint-tolerance = 0.15             # max behavioral distance before warning

# ---------- Provenance ----------
[provenance]
backend = "cluster"
run = "flyte://my-org/harness-lock/development/r8x2k1"
search = { strategy = "successive-halving", candidates = 48, trials = 1312, spend-usd = 214.07 }
```

### 5.3 Component model: the stack as a dependency graph

Everything in a lock is a **component** of some **kind**, and every component shares one
envelope:

| Field | Meaning |
|---|---|
| `kind` | Core kind (`llm`, `decision`, `harness`, `tool`, `mcp`, `skill`, `instructions`, `subagent`, `hook`, `command`, …) or a namespaced native kind (`pydantic-ai.capability`). |
| `name` | Unique within its kind. `kind:name` is the component ID used in edges. |
| `source` | Where it came from: `npm:`, `pypi:`, `git+…@rev`, `hf://`, `oci://`, `path:`, `https://` (remote), `inline`. |
| `version` / `revision` | Resolved version or commit. Never a range or a moving ref. |
| `digest` / `integrity` | Content hash of the artifact (package integrity, directory digest, weights digest, image digest). |
| `config` | Hash of the rendered configuration for this component, if any. |
| `requires` | Edges to other component IDs (a skill requires `tool:python`; a subagent requires an `llm` role). |
| `provides` | For bundles: the component IDs it expands into. |
| `scope` | Where it applies: the whole stack, one LLM role, one subagent, or one harness. |
| `enforce` | `pin` (checked and guarded), `record` (provenance only), or `off`. |

Components form a DAG. `hlock tree` prints it. `update` follows the edges, so updating a
bundle re-resolves what it provides, and updating a skill re-checks what requires it.

**Pinning strategies**, chosen per kind:

| Strategy | Used when | Example |
|---|---|---|
| **artifact** | We can hash the bytes | skill directory, sandbox image, open weights, plugin tarball |
| **version + integrity** | A package manager can reproduce it | `npm:opencode-ai@1.4.2` + `sha512-…`, `pypi:pydantic-ai-harness==0.6.1` + wheel hash |
| **snapshot** | It's data that changes over time | retrieval index, memory seed, fixtures |
| **endpoint + fingerprint** | It's remote and we can't pin bytes | closed LLM, remote MCP server, A2A agent, provider-native tools |
| **literal** | Small and worth reviewing in a PR | policies, thresholds, params |

**Portable vs. native kinds.** Portable kinds are defined once and installed into any harness
whose adapter supports them. The adapter knows where an Agent Skill goes for OpenCode versus
Claude Agent SDK versus Codex, and how to register an MCP server in each config format. That
lets the resolver hold a skill or MCP server constant while it swaps harnesses. Native kinds only
exist for one harness and are registered by its adapter (§7.6). When the manifest searches
across harnesses, native components sit under the harness they belong to and only apply when
that harness is chosen.

**Bundles are locked twice.** A bundle (Claude Code plugin, Pi package, OpenCode plugin,
Pydantic AI capability) is pinned as itself *and* expanded into what it provides. `check`
catches a bundle whose version is unchanged but whose contents changed.

### 5.4 Peripherals worth locking

What a modern harness plugs in, based on the configuration surfaces of OpenCode, Pi,
Claude Agent SDK / Claude Code, Codex CLI, and Pydantic AI (+ Pydantic AI Harness), plus the
Agent Skills, MCP, and AGENTS.md standards. "Behavior" means changing it can change task
outcomes, so it's enforced. "Record" means provenance only.

| Peripheral | Examples across harnesses | Kind | Pinning | Lock role | Search axis? |
|---|---|---|---|---|---|
| **Built-in tools** | Pi's read/write/edit/bash; OpenCode `tools`; Claude Code built-ins | `tool` | harness version + tool schema hash | behavior | enable/disable sets |
| **Function tools** | Pydantic AI function tools / toolsets; LangGraph tools | `tool` | source digest of the module + schema hash | behavior | yes |
| **MCP servers (local)** | stdio servers in `opencode.json`, Codex `config.toml [mcp_servers]`, Claude `mcpServers` | `mcp` | version + integrity; advertised tools/resources/prompts schema hash | behavior | include/exclude, version |
| **MCP servers (remote)** | HTTP/SSE servers, vendor-hosted MCP | `mcp` | URL + OAuth scopes + advertised-schema fingerprint | behavior (warn on drift) | include/exclude |
| **Skills** | Agent Skills (`SKILL.md` + scripts + references), adopted by Claude, Codex, OpenCode, Pi, VS Code, Cursor, and others | `skill` | commit + tree digest; script deps pinned with it; origin, trust, scan (§7.7) | behavior | include/exclude, variants |
| **Instruction files** | `AGENTS.md`, `CLAUDE.md` + rules, Cursor rules, OpenCode rules | `instructions` | content digest per file, plus hierarchy order | behavior | variants |
| **System prompt / output style** | custom system prompts, Claude output styles, prompt append vs. replace | `prompt` | content digest | behavior | variants |
| **Subagents** | Claude/OpenCode agent definitions, Pydantic AI `Subagents`, LangGraph sub-graphs | `subagent` | prompt digest + model role + tool list | behavior | yes |
| **Hooks** | Claude Code hook events (PreToolUse, Stop, …), Pi extension events, Pydantic AI hooks | `hook` | command/source digest + event binding | behavior | rarely |
| **Commands / prompt templates** | Claude & OpenCode slash commands, Pi prompt templates | `command` | content digest | record (behavior if a task invokes them) | no |
| **Plugins / packages / extensions** | Claude Code plugins, OpenCode plugins, Pi packages & extensions | bundle (native) | version + integrity, expanded | behavior | include/exclude |
| **Harness-native capabilities** | Pydantic AI capabilities (Thinking, Planning, Memory, Compaction, Guardrails, …) | `pydantic-ai.capability` | import path + package version + args hash | behavior | yes (args, include/exclude) |
| **Agent modes / profiles** | OpenCode agents (build/plan), Codex profiles, Pi plan mode | native | config hash | behavior | yes |
| **Permissions / approval rules** | OpenCode `permission`, Claude permission rules, Codex approval & sandbox modes | `policy` | literal | behavior | yes |
| **LSP servers, formatters, linters** | OpenCode `lsp` and `formatter`; diagnostics fed back to the model | native / `toolchain` | binary version + integrity | behavior | sometimes |
| **Execution environment** | sandbox image, language runtimes, OS packages, non-secret env vars, workspace layout | `runtime` | image digest + lockfiles | behavior | rarely |
| **Context management** | compaction strategy and thresholds, tool-output limits, prompt caching, deferred tool loading / tool search | `context` | literal | behavior | yes |
| **Provider features** | reasoning/thinking effort, provider-native tools (web search, code exec), API version and beta headers, structured-output schemas | part of `llm` | literal + fingerprint | behavior | yes |
| **Memory** | memory files and stores, Pydantic AI `Memory`, conversation search | `memory` | backend + schema + *seed snapshot* (never live contents) | behavior | on/off, scope |
| **Retrieval / knowledge** | embedding model, chunking, index snapshot, reranker, vector DB version | `retrieval` | snapshot digest + model digests | behavior | yes |
| **Browser / computer use** | Playwright or Browser Use versions, viewport, browser build | `browser` | version + image digest | behavior | rarely |
| **Guardrails** | prompt-injection defenders, content filters, spend limits, trajectory judges | `guardrail` | source + config hash | behavior | yes |
| **Remote agents** | A2A agent cards, hosted subagents | `remote-agent` | URL + card digest + fingerprint | behavior (warn on drift) | include/exclude |
| **Gateway / routing** | LiteLLM or another gateway, fallback chains | `gateway` | version + config hash | behavior | fallback order |
| **Telemetry** | OTel exporters, Logfire, LangSmith | `telemetry` | version | record | no |
| **External APIs behind tools** | the GitHub API behind an MCP server, SaaS endpoints | (`requires` on the tool) | API version where one exists; otherwise recorded responses in fixtures | record | no |

Two cross-cutting rules come out of this survey:

1. **Context isolation is part of the lock.** Most harnesses silently merge user-level config
   (`~/.claude`, `~/.config/opencode`, `~/.codex`, `~/.pi`), which makes a "locked" agent
   behave differently on every machine. `[context].isolation = "locked-only"` (the default)
   makes adapters run the harness with an isolated config root, for example the Claude Agent
   SDK's explicit `settingSources` or a temporary `HOME`. `"merge-user"` is an explicit opt-out,
   and `check` warns about it.
2. **Live state is never locked; its seed is.** Memory stores, retrieval indexes, and
   workspaces change while the agent runs. The lock pins their schema, backend version, and
   starting snapshot. During tuning, each trial starts from that snapshot so memory can't
   leak between trials.

### 5.5 Behavioral fingerprints

Closed models can't be pinned by weights, and providers sometimes change behavior under a
fixed model ID. A **fingerprint** is a hash over the model's outputs on a fixed, small
probe set (fixed prompts, `temperature=0`, structured outputs), plus an embedding summary
used for a *distance* metric. `hlock verify` re-probes and reports the distance. Above
`fingerprint-tolerance` it warns; with `--strict` it fails.

Fingerprints are evidence, not a guarantee. The spec says so plainly in the CLI output.

### 5.6 Enforcement

Enforcement happens at three points:

1. **`hlock check`** (static, fast, offline-capable). Walks the component graph and
   runs each kind's checker. That verifies installed harness, MCP server, plugin, and LSP
   versions and integrity; skill directory and instruction-file digests; rendered config and
   prompt hashes; the sandbox image digest; context isolation; and that `harness.toml`
   still hashes to `manifest-hash`. It also flags *undeclared* peripherals: a skill, hook,
   or MCP server present in the harness's config roots that isn't in the lock. Exit codes
   are CI-friendly.
2. **`hlock verify`** (dynamic, costs money). Re-runs the canary tasks and fingerprint
   probes against live providers, and re-reads the advertised schemas of remote MCP
   servers and remote agents. Compares everything to the evidence and thresholds.
3. **Runtime guard** (in-process, via adapters). When the agent runs through
   `hlock run` or `hlock.runtime.guard(...)`:
   - model calls are checked against the locked provider/model/params; mismatch → error,
   - tool and MCP calls are checked against `tool-allowlist`, `permissions`, and the locked
     tool schemas (a server that starts advertising new tools is a violation),
   - only locked skills, hooks, and native components are loadable,
   - `max-steps`, `max-cost-usd-per-task`, and timeouts are enforced,
   - `risky-actions` trigger the configured approval hook,
   - every run emits a trace in the standard schema (§10), so production traces can feed
     the next tuning round.

Enforcement modes: `strict` (fail), `warn` (log + continue), `off`. Configured in the manifest,
overridable per command.

### 5.7 Updating

```bash
hlock update llm.executor                    # re-resolve one component, keep the rest pinned
hlock update harness --to 1.5.0              # try a specific new version
hlock update skill:pytorch-debugging         # pull the skill's new revision, re-check dependents
hlock update --all                           # full re-resolve
hlock diff                                   # component + evidence diff vs. the committed lock
```

`update` reuses cached trials for unchanged stacks (thanks to Flyte caching, §8.3), so a
partial update only pays for new trials. `diff` renders both a structural diff and an
**evidence diff** ("success-rate 0.87 → 0.89 (+0.02, CI overlaps); cost −31%"), suitable for
pasting into a PR description. `hlock diff --format md` produces exactly that.

### 5.8 Upgrading (`hlock upgrade`)

`update` (§5.7) stays within a component's declared range and is conservative. `upgrade` is the
deliberate, governed path for moving a component to a **newer major / breaking version** (or all
dependent components at once), with a safe undo and a compatibility review gate:

```bash
hlock upgrade                          # plan-only: list upgradeable components + what changes
hlock upgrade --all                    # upgrade every locked component that has a newer/breaking version
hlock upgrade harness --to 2.0.0       # upgrade one component to a specific target
hlock upgrade skill:pytorch-debugging mcp:github --dry-run
hlock upgrade --apply                  # perform the upgrade + re-resolve dependents + re-evaluate
hlock upgrade --rollback               # restore the previous harness.lock if the upgrade regressed
```

Guarantees and behavior:

- **Safe undo.** The previous `harness.lock` is retained (as `harness.lock.prev`, or on the working
  branch) so `--rollback` is one command and every change stays PR-reviewable via `hlock diff`.
- **Edge-aware re-resolution.** Re-resolves the component's `requires`/`provides` edges (§5.3):
  upgrading a bundle re-resolves what it provides; upgrading a skill re-checks its dependents.
- **Cost-controlled re-evaluation.** Runs only the affected components' trials on the
  search/holdout splits (everything else is cached, §8.3) and reports an evidence diff
  (`hlock diff --format md`) for the PR description.
- **Gate.** `upgrade` will not apply unless the new evidence meets `[objective]` constraints
  (`--force` overrides). `hlock check` after an upgrade enforces the new lock, and
  `hlock verify` re-probes fingerprints (§5.5).
- **Discovery.** `hlock outdated` lists the components `upgrade` (or `update`) can move.

| | `hlock update` (§5.7) | `hlock upgrade` (§5.8) |
|---|---|---|
| Scope | within a declared range | across majors / breaking |
| Default | re-resolve pinned components | plan / gate / apply-rollback |
| Safety | minimal, cached | undoable, gated, evidence-checked |

### 5.9 The tune lock (`harness.tune.lock`)

`hlock tune` (§8.5) writes a second, *derived* lockfile — `harness.tune.lock` (name is
`--out`-renamable) — that records **tuning output** rather than a search's selection.
It is a superset of the schema-v1 lock (so `hlock check` / `hlock verify` work on it) plus
tune-specific tables:

```toml
# This file is generated by harness-lock. Do not edit by hand.
lock-version = 1
generated-by = "harness-lock 0.4.0 (tune 1)"
base-lock = "sha256:9f1c…"          # the harness.lock it was derived from
kind = "tuned"

[stack]
name = "ml-eng-debugger"
selection = "pareto:min-cost@quality>=0.85"

# ---- Prompt tuning (depth >= prompt) ----
[[tune.prompt.applied]]
component = "harness.system-prompt"
before = "sha256:b1"                 # content-hash before
after = "sha256:b2"                  # content-hash after (never inlined)
gain = { "success-rate" = "+0.03", "cost-usd-per-task" = "-0.12" }

# ---- Hyperparameter search (depth >= hpo) ----
[tune.hpo]
axes = { retries = [0, 1, 3], temperature = [0.0, 0.2, 0.7], "tool-timeout-seconds" = [30, 120] }
trials = 214
best = { retries = 1, temperature = 0.2, "tool-timeout-seconds" = 120 }

# ---- Fine-tuning (depth >= ft) ----
[[tune.finetune]]
target = "decision"                  # decision model fine-tuned on traces
base = "hf://harness-lock/decider-1.5b"
data = "traces://…"                  # provenance of the fine-tune dataset
adapter = { lora = "store://sha256:8e7f…" }   # content-addressed LoRA
samples = 3142
loss = 0.11

# ---- data provenance ----
[tune.data]
source = "user:./runs"               # or "hub:harness-lock/traces" | "synthetic"
mode = "user-traces"            # or "synthetic"
redact = true
split = { train = 0.8, val = 0.1, test = 0.1 }

[tune.provenance]
run = "flyte://…"                    # the (durable) run that produced it
budget = { "spend-usd" = 18.4, "gpu-h" = 1.2 }
duration-s = 912

[evidence]                            # re-measured on holdout after tuning
# … same shape as harness.lock's evidence
```

`harness.tune.lock` is **not** a replacement for `harness.lock` — it is the researched,
higher-performing candidate. It is adopted into the enforceable lock with
`hlock resolve --from harness.tune.lock` / `hlock adopt harness.tune.lock` once the team decides to
ship it; at that point the tuned changes become pinned, enforced behavior with the same
`check`/`verify`/`run` guarantees as any other locked stack.

---

## 6. The manifest: `harness.toml`

User-authored. Declares candidates and constraints; the resolver resolves them into a lock.

```toml
[project]
name = "ml-eng-debugger"

[evals]
packs = ["harness-lock/ml-engineering@^0.2", "./evals/internal"]   # hub packs + local packs
splits = { search = 0.7, holdout = 0.3 }
seeds = 3

# ---- What the resolver may try ----
[candidates.llm.planner]
any-of = ["anthropic:claude-sonnet-latest", "openai:gpt-5", "vllm:Qwen/Qwen3-32B"]

[candidates.llm.executor]
any-of = ["vllm:Qwen/Qwen3-32B", "ollama:qwen3:8b", "same-as:planner"]

[candidates.decision]
any-of = ["none", "rules:default", "hf://harness-lock/decider-1.5b", "llm:openai:gpt-5-mini"]
train = "allow"                          # allow tuning a decision model on this project's traces

[candidates.harness]
any-of = ["opencode", "pi", "claude-agent-sdk", "langgraph:./graphs/debugger.py"]
pattern = ["single-agent", "plan-act-reflect", "dag+agent-loop"]   # where the harness supports it

[candidates.policy]
retries = [0, 1, 3]
human-approval = ["never", "on_risky_action", "always_before_write"]
tool-timeout-seconds = [30, 120]
memory = ["none", "session", "project"]
sandbox = [true]                         # fixed: not a search axis

# ---- Dependencies: portable peripherals (installed into whichever harness wins) ----
[tools]
python = {}
github = { mcp = "npm:@modelcontextprotocol/server-github", permissions = ["read:repo", "write:pull_request"] }

[skills]                                 # see §7.7 for all source types
pdf = { registry = "skills.sh", id = "anthropics/skills/pdf" }                        # OSS, via a registry
gh-actions = { git = "https://github.com/openai/skills", path = "skills/.curated/gh-fix-ci", rev = "main" }
pytorch-debugging = { git = "ssh://git@github.com/acme/agent-skills", path = "skills/pytorch-debugging", tag = "v1.3.0" }
flyte-sdk = { path = "./skills/flyte-sdk", editable = true }                         # first-party, in-repo
internal-sql = { oci = "ghcr.io/acme/skills/internal-sql:2" }                          # first-party, org registry

[skills.builtin]                         # harness-shipped skills must be explicitly on or off
codex-cli = { enable = ["openai-docs"], disable = ["skill-creator", "plugin-creator"] }

[skills.trust]
allow-sources = ["github.com/anthropics/*", "github.com/openai/skills", "github.com/acme/*", "path:./skills"]
require = ["pinned-commit", "license", "scan-pass"]
max-allowed-tools = "policy"             # a skill can't pre-approve tools beyond [policy]

[instructions]
files = ["AGENTS.md"]

[context]
isolation = "locked-only"

# Peripherals can also be search axes
[candidates.skills]
optional = ["pdf", "gh-actions"]         # resolver may try with and without
variants = { flyte-sdk = ["./skills/flyte-sdk", "./skills/flyte-sdk-terse"] }   # A/B two versions of a skill
[candidates.instructions]
any-of = [["AGENTS.md"], ["AGENTS.md", "docs/agent-style.md"]]
[candidates.mcp]
optional = ["github"]

# ---- Dependencies: harness-native peripherals (only apply when that harness is chosen) ----
[candidates.harness.opencode]
version = ">=1.4,<2"
plugins = ["npm:opencode-safe-git@^0.4"]
lsp = { pyright = "npm:pyright@^1.1" }

[candidates.harness.pi]
packages = ["npm:@acme/pi-mleng-pack@^1"]  # bundle: extensions + skills + prompt templates

# ---- What "best" means ----
[objective]
primary = "success-rate"
constraints = { success-rate = ">=0.85", cost-usd-per-task = "<=1.00", latency-s-p95 = "<=180" }
select = "min-cost"                      # among stacks meeting constraints

[budget]
max-spend-usd = 250
max-trials = 2000
strategy = "auto"                        # grid | random | successive-halving | bayesian | auto

[enforce]
mode = "strict"

[backend]
target = "local"                         # local | devbox | cluster
# profile = "gpu-training"              # see §9.3
```

Candidate syntax:

- `provider:model` for LLMs. Aliases are allowed in the manifest; the lock always pins them.
- `same-as:<role>` to tie roles together.
- `hf://org/repo[@rev]`, `store://sha256:…`, local paths for models/adapters.
- `langgraph:path/to/graph.py` and similar for harnesses that need user code.
- Any component accepts a constraint table instead of a list:
  `[candidates.harness.opencode] version = ">=1.4,<2"`.
- Peripheral sources: `npm:`, `pypi:`, `git = …` + `rev`/`path`, `hub = …` (skill registries),
  `path = …` (local), `url = …` (remote MCP / A2A). Ranges and moving refs (`main`) are fine
  in the manifest. The lock always resolves them to an exact version or commit plus a digest.
- `optional = [...]` makes a peripheral an include/exclude search axis; `any-of` picks
  among variants.
- `[candidates.harness.<name>.<kind>]` declares harness-native components. Each adapter
  documents its kinds (§7.6).

Peripherals can also be added from the CLI, which edits the manifest and re-locks only the
affected component:

```bash
hlock add skill git+https://github.com/acme/agent-skills#path=skills/pytorch-debugging
hlock add mcp npm:@modelcontextprotocol/server-github --permissions read:repo
hlock add pydantic-ai.capability pydantic_ai_harness.Planning
hlock remove skill pdf
```

---

## 7. Components

### 7.1 Eval packs

An eval pack is a directory (or a Hub reference) with a `pack.toml`:

```toml
[pack]
name = "harness-lock/ml-engineering"
version = "0.2.0"
license = "Apache-2.0"
domain = "ml-engineering"

[[task]]
id = "debug_failed_training_run_014"
prompt = "tasks/014/prompt.md"
fixtures = "tasks/014/workspace/"       # copied into the sandbox
tools = ["python", "filesystem", "logs"]
expected = { artifacts = ["patch.diff", "root_cause.md"] }
graders = ["tests:tasks/014/check.py", "judge:rubric/root_cause.md"]
tags = ["recovery", "code-edit"]
timeout-seconds = 600
```

Grader types:

- `tests:` a Python function `def grade(workspace, trace) -> Grade`. Deterministic, preferred.
- `artifact:` existence/schema/content checks on outputs.
- `judge:` a rubric scored by a judge model (default: the open `harness-lock/judge` model,
  §11; any LLM can be configured). Judge prompt and model are pinned in the evidence.
- `trace:` checks over the trace itself (e.g. "never called `shell` with `rm -rf`",
  "asked for approval before writing outside workspace").

Metrics computed per trial: success, score, cost, latency, steps, retries, tool-use
correctness, tool errors, recoveries, human interventions, safety violations. Aggregates
come with bootstrap CIs, plus **determinism** (outcome and trajectory variance across seeds)
and **replayability** (whether the trial reproduces from its recorded trace, §10.3).

**Bootstrapping a pack.** Domain coverage is the biggest adoption barrier, so there are
three ways in:

- *Built:* `hlock evals init` writes a complete hello-world pack (three tasks with
  fixtures) that runs as is, so a new user sees the whole loop before writing anything.
- *Built:* `hlock evals init -t coding|data|research|support|blank` writes a template: task
  slots marked `TODO(hlock)`, cycled over the template's task kinds (`--tasks N`, default
  10), plus an Agent Skill (`.agents/skills/hlock-eval-pack/SKILL.md`). The user's own
  agent follows the skill to fill the slots from local context (git history, merged PRs,
  closed issues, notebooks, tickets), proposing tasks before writing them and keeping
  secrets out of fixtures. `hlock evals check` (also run by `resolve`) reports unfilled
  slots, missing prompts or fixtures, duplicate ids, and tasks that can't fail.
- *Built:* `hlock evals init --from-traces [PATH]` drafts tasks from the sessions agent CLIs
  logged for the project (Claude Code, Codex, Pi), or from JSONL/CSV records with a prompt
  column: one task per distinct request, prompts redacted, and the files each session
  created as expected artifacts. An Agent Skill guides the user's agent through review.
- *Planned:* drafting from production traces in the trace schema, or imported from
  OTel/LangSmith/etc., with draft graders beyond artifact checks.

Initial official packs: `research-analyst`, `data-cleaning`, `ml-engineering`,
`code-migration`, `support-triage`. Each has 30–100 tasks and a hidden holdout split on the Hub.

The first pack, **`research-analyst`** (the "research/data agent gauntlet"), is deliberately
concrete. Its 25–50 tasks each exercise one or more of:

- reading a messy CSV,
- calling a search/retrieval tool,
- writing and running Python,
- recovering from a failing tool call,
- producing a cited answer,
- creating an artifact (report, plot),
- deciding when human approval is needed,
- not leaking a secret planted in the workspace,
- resuming from a failed step.

Every task is graded by deterministic checks first and a judge only where necessary.

Backlog of candidate packs (community or later official): `bi-analyst`,
`scientific-literature-review`, `drug-discovery-research`, `finance-forecasting`,
`workflow-debugging`.

### 7.2 LLM providers

A thin provider layer (built on LiteLLM or similar, behind our own interface) with:
alias resolution to dated IDs, price tables for cost accounting, fingerprint probing,
and serving adapters for `vllm`, `sglang`, and `ollama` that can *start* an open-weights
model from a pinned revision when the backend allows it (devbox/cluster with GPUs).

### 7.3 Decision models

The decision model makes cheap, structured choices so the LLM doesn't have to:

| Decision | Input | Output |
|---|---|---|
| `route` | task + state summary | which LLM role/size handles the next step |
| `retry` | failed step + error | retry / modify-and-retry / give up |
| `escalate` | proposed action + context | proceed / ask human |
| `stop` | trajectory so far | done / continue |
| `tool` | state + tool list | tool to call (optional, harness permitting) |

Implementations: `none`, `rules` (YAML rules, a strong baseline), `llm` (any LLM prompted
with a structured-output schema), and `model` (a small classifier/generator, §11).

**Tuning a decision model** is a tuning stage: after an initial sweep, successful and failed
trajectories are labeled (by outcome and by the judge), a LoRA is trained on top of
`harness-lock/decider`, and the stacks that use it are re-evaluated on the holdout split. The
resulting adapter is content-addressed and pinned in the lock.

Harnesses differ in where they let you intervene. Each adapter declares which decision
hooks it supports (§7.4); the resolver only proposes decision models for supported hooks.

### 7.4 Harness adapters

```python
class HarnessAdapter(Protocol):
    name: str
    supported_decisions: set[Decision]
    portable_kinds: dict[str, InstallTarget]   # e.g. {"skill": ".opencode/skills/", "mcp": "opencode.json#mcp"}
    native_kinds: list[ComponentKind]          # e.g. [OpencodePlugin, OpencodeLsp, OpencodeAgentMode] (§7.6)
    config_roots: list[ConfigRoot]             # user/project dirs to isolate or scan for undeclared peripherals

    def resolve(self, spec: HarnessSpec) -> ResolvedHarness:
        """Pin version/source/integrity. Called at lock time."""

    def materialize(self, stack: Stack, workdir: Path) -> HarnessConfig:
        """Render config, prompts, tool wiring, decision hooks into workdir."""

    async def run(self, task: Task, config: HarnessConfig, sandbox: Sandbox,
                  recorder: TraceRecorder) -> RunResult:
        """Execute one task. Emit trace events through `recorder`."""

    def check(self, locked: LockedHarness, env: Environment) -> list[Violation]:
        """Static verification for `hlock check`."""
```

How adapters capture traces and enforce the lock:

- **Proxy mode (default for CLI harnesses: OpenCode, Pi, Codex CLI, Claude Code).** The
  harness is pointed at a local OpenAI/Anthropic-compatible proxy run by harness-lock. The
  proxy records model calls, enforces model/params, injects decision-model hooks, and
  meters cost. MCP traffic goes through an MCP proxy that enforces the allowlist and
  permissions.
- **In-process mode (Python frameworks: LangGraph, PydanticAI, OpenAI Agents SDK,
  Claude Agent SDK).** Instrumentation via the framework's callback/hook APIs plus the proxy
  for model calls.

v0 adapters: `reference`, `opencode`, `pi`, `claude-agent-sdk`, `pydantic-ai`, `langgraph`.
v1 adds: `codex-cli`, `openai-agents`, `hermes`, `crewai`, `custom` (any command that
reads a config and talks to a configurable model endpoint).

The proxy is effectively a small local LLM gateway (routing, metering, policy). That was
considered as a standalone product (Appendix A); here it is an internal component.

### 7.5 Sandboxes

Trials run in a sandbox: `subprocess` (local, least isolation, opt-in), `docker` (default
when Docker is available), or the cluster's container runtime. The sandbox image is pinned
by digest in the lock. Network policy: `none`, `egress-allowlist`, `open`.

### 7.6 Extensible component kinds (Pydantic AI capabilities and friends)

Harnesses keep inventing new units of extension: Pydantic AI *capabilities*, OpenCode
plugins and agent modes, Pi packages and extensions, Claude Code plugins and output styles,
Codex profiles. harness-lock can't hard-code all of them, so **component kinds are a plugin
point**. Each kind is a small class registered by an adapter, or by a third-party package
through the `hlock.component_kinds` entry point:

```python
class ComponentKind(Protocol):
    name: str                                  # "pydantic-ai.capability"
    harness: str | None                        # "pydantic-ai"; None for portable kinds
    entry: type[BaseModel]                     # schema of a manifest/lock entry for this kind
    strategy: PinStrategy                      # artifact | version+integrity | snapshot | endpoint+fingerprint | literal
    stands_in_for: tuple[str, ...] = ()        # core kinds it can provide, e.g. ("tool", "hook", "instructions")

    def resolve(self, spec: BaseModel, ctx: ResolveContext) -> Resolved:
        """Pin version/revision and digest. Expand bundles into `provides`. Add `requires` edges."""

    def search_axes(self, spec: BaseModel) -> list[Axis]:
        """Optional: which fields the resolver may vary (include/exclude, args, variants)."""

    def materialize(self, resolved: Resolved, cfg: HarnessConfig) -> None:
        """Write this component into the harness's config or code."""

    def check(self, locked: Resolved, env: Environment) -> list[Violation]:
        """Static verification for `hlock check`."""

    def guard(self) -> RuntimeGuard | None:
        """Optional runtime enforcement hook for `hlock run`."""
```

Because the lock schema is generic (`[[native]] kind = "…"`, §5.2) and each kind brings its
own entry schema, adding a kind never changes the lockfile format.

#### Worked example: Pydantic AI capabilities

In Pydantic AI, a **capability** is a reusable, composable unit that bundles tools/toolsets,
lifecycle hooks, instructions, model settings, and even model selection. You pass it via
`Agent(..., capabilities=[...])`. Core `pydantic-ai` ships some built-ins (e.g. `Thinking`,
`WebSearch`, `MCP`), and the separate `pydantic-ai-harness` package ships many more
(file system, shell, sandboxes, `Planning`, `Subagents`, `Memory`, `Skills`, `Compaction`,
`Guardrails`, `Tool Approval`, `Spend Limits`, `Durable Execution`, …). Capabilities can be
deferred (`defer_loading=True` with an `id`) so the model loads them on demand. Agents can
also be declared in YAML/JSON *agent specs* (`Agent.from_file` / `Agent.from_spec`), which
list capabilities by name with arguments.

The `pydantic-ai` adapter registers `pydantic-ai.capability`:

- **Manifest.** Capabilities live under the harness and can be search axes:

  ```toml
  [candidates.harness.pydantic-ai]
  version = ">=2,<3"
  harness-lib = "pydantic-ai-harness>=0.6"

  [[candidates.harness.pydantic-ai.capability]]
  use = "Planning"                               # resolved against pydantic_ai / pydantic_ai_harness

  [[candidates.harness.pydantic-ai.capability]]
  use = "Thinking"
  args.effort = ["medium", "high"]               # search axis over constructor args

  [[candidates.harness.pydantic-ai.capability]]
  use = "Memory"
  optional = true                                # include/exclude axis
  defer-loading = [false, true]                  # on-demand vs. always-loaded is an axis too

  [[candidates.harness.pydantic-ai.capability]]
  use = "acme_agents.capabilities:RepoSearch"    # user-defined capability (import path)
  ```

- **Lock.** Each capability is pinned by import path, the providing package's version and
  wheel hash (or the source digest of the user module), a hash of its constructor args,
  `id`, and `defer_loading`:

  ```toml
  [[native]]
  kind = "pydantic-ai.capability"
  name = "Thinking"
  source = "pypi:pydantic-ai==2.3.1"
  integrity = "sha256:…"
  config = "sha256:…"                            # canonicalized args: { effort = "high" }
  args = { effort = "high" }                     # small args are inlined for PR review
  provides = ["llm:planner#params.thinking"]

  [[native]]
  kind = "pydantic-ai.capability"
  name = "Skills"
  source = "pypi:pydantic-ai-harness==0.6.1"
  integrity = "sha256:…"
  requires = ["skill:pytorch-debugging", "skill:flyte-sdk"]   # reuse the portable skill components
  ```

- **Interop with portable kinds.** Capabilities that wrap a portable peripheral (`MCP`,
  `Skills`) declare `requires` edges to the core `mcp` / `skill` components, so a skill is
  pinned once and shared whether the winning harness is Pydantic AI or OpenCode.
  Capabilities that provide tools or hooks list them in `provides`, so the runtime guard's
  tool allowlist and the trace schema see them like any other tool.
- **Materialization.** The adapter renders a Pydantic AI agent spec from the lock and loads
  it with `Agent.from_file`, so the locked agent is a real, inspectable Pydantic AI
  artifact. `export --format pydantic-ai` emits that spec. User-defined capabilities that a
  spec can't express are passed as keyword arguments to `Agent.from_spec`.
- **Enforcement.** `check` verifies package versions and hashes and the source digests of
  user capability modules. The guard rejects capabilities loaded at runtime that aren't in
  the lock, which includes self-extension capabilities such as capability creation unless
  they're explicitly allowed.

#### Other native kinds at launch

| Adapter | Native kinds |
|---|---|
| `opencode` | `opencode.plugin`, `opencode.agent` (modes like build/plan), `opencode.command`, `opencode.lsp`, `opencode.formatter` |
| `pi` | `pi.package` (bundle), `pi.extension`, `pi.prompt-template`, `pi.theme` (record-only) |
| `claude-agent-sdk` | `claude.plugin` (bundle), `claude.hook`, `claude.output-style`, `claude.command`, `claude.setting-sources` |
| `codex-cli` | `codex.profile`, `codex.approval-mode`, `codex.sandbox-mode` |
| `langgraph` | `langgraph.graph` (graph module digest), `langgraph.checkpointer` |

All of them also use the portable kinds (`tool`, `mcp`, `skill`, `instructions`, `subagent`)
where the harness supports them.

### 7.7 Skills

Skills get their own section because they're the fastest-growing kind of agent dependency.
They're also the one most likely to be copied around informally and to drift. An Agent
Skill is a directory with a `SKILL.md` (YAML frontmatter with a required `name` and
`description`, plus optional `license`, `compatibility`, `metadata`, and experimental
`allowed-tools`) and optional `scripts/`, `references/`, and `assets/`. Harnesses load skills
progressively: every skill's name and description at startup, the body on activation, and
resources on demand. The format is an open standard (agentskills.io) read by Claude, Codex,
OpenCode, Pi, VS Code, Cursor, Gemini CLI, Goose, and others, so it's harness-lock's main
**portable** peripheral.

#### 7.7.1 Where skills come from

| Origin | Examples | Typical source | What the lock pins |
|---|---|---|---|
| **OSS vendor catalogs** | `anthropics/skills`; `openai/skills` (`.system`, `.curated`, and experimental tiers); `google/skills`; SDK vendors shipping skills for their own frameworks (e.g. Flyte) | public git | commit + tree digest |
| **Community registries and indexes** | `skills.sh` (`npx skills add …`), other skill directories, GitHub topic repos. A July 2026 crawl counted ~57k skills across ~1.1k repos | registry ID → git | the registry ID for provenance, but the **git commit + digest** is what's pinned; registries are discovery, not trust |
| **Plugin / package bundles** | Claude Code and Codex plugins (marketplaces), Pi packages, OpenCode plugins | npm / git / marketplace | bundle version + integrity, and each contained skill's digest (`origin = "bundled"`) |
| **Harness built-ins** | Codex `.system` skills (skill-installer, skill-creator, …); skills baked into a harness release | the harness itself | harness version + each built-in's digest + explicit enabled/disabled |
| **First-party, in-repo** | `./skills/…`, `.agents/skills/`, `.claude/skills/` committed with the project | `path:` | tree digest; optionally `editable` |
| **First-party, org-shared** | private monorepo of company skills, internal registry, OCI artifacts | `git+ssh`, `oci://`, private registry | commit/tag or image digest + tree digest + optional signature |
| **Personal (user-level)** | `~/.claude/skills`, `~/.agents/skills`, `~/.codex/skills` | user's machine | nothing: excluded by `isolation = "locked-only"`. `hlock skills import --from-user` copies chosen ones into the manifest as first-party skills |
| **Generated** | skills written by `skill-creator`-style tools or proposed by the resolver (open question 9) | `store://` | digest + the run that produced it |

#### 7.7.2 Source syntax

```toml
[skills]
a = { registry = "skills.sh", id = "owner/repo/skill" }        # resolved to git + commit
b = { git = "https://github.com/owner/repo", path = "skills/b", rev = "main" | tag = "v1" | commit = "…" }
c = { gh = "owner/repo/skills/c" }                              # shorthand for public GitHub
d = { path = "./skills/d", editable = true }
e = { oci = "ghcr.io/acme/skills/e:2" }
f = { plugin = "claude:acme-marketplace/acme-tools", skill = "f" }  # one skill out of a bundle
g = { as = "pdf-acme", git = "…", path = "skills/pdf" }          # alias on name collision
```

Ranges and moving refs are allowed in the manifest. The lock always pins a commit (or image
digest) plus a **tree digest**: a Merkle hash over normalized paths, contents, and executable
bits, excluding VCS metadata. That makes a vendored copy and a git checkout of the same
skill hash identically, so `check` can verify either one.

#### 7.7.3 What the lock records per skill

On top of the common envelope (§5.3), a `[[skill]]` entry carries:

- `origin`: `third-party` | `first-party` | `bundled` | `harness-builtin` | `generated`.
- `registry`: where it was discovered, if anywhere (provenance only).
- `frontmatter`: hash of the parsed frontmatter, with `license`, `compatibility`, and
  `allowed-tools` inlined for review. `hlock resolve` validates frontmatter against the spec (the
  same rules as the `skills-ref validate` reference validator): name format, the name matching
  its directory, and length limits.
- `allowed-tools`: inlined and checked against `[policy]`. A skill can never widen what the
  stack's policy allows.
- `executes` and `runtime-deps`: scripts the skill can run and their dependencies, which are
  pinned and baked into the sandbox image so a skill's script runs the same everywhere.
- `scan`: result of the skill security scan (below).
- `signature`: optional publisher signature, e.g. Sigstore keyless with the release
  workflow's identity. `[skills.trust]` can require it for given sources.
- `editable` and `evidence-digest`: first-party development mode (below).
- `provided-by`: for bundled and harness-built-in skills, the bundle or harness that
  supplies them.

#### 7.7.4 First-party skills: the development loop

Teams iterate on their own skills constantly, so a strict "any edit fails `check`" rule would
be miserable. First-party `path:` skills can be marked `editable = true`, like `pip install -e`:

- `hlock check` passes but reports **stale evidence** when the skill's current digest differs
  from `evidence-digest`. With `--strict` (for release branches), it fails.
- `hlock resolve --refresh skill:flyte-sdk` re-measures only the trials affected by that skill.
  Thanks to trial caching (§8.3), everything else is reused.
- `hlock diff` shows the skill's content diff next to the evidence diff, so a PR that edits a
  skill shows whether it helped.
- Non-editable first-party skills (e.g. released from an org repo by tag) behave like
  third-party ones.

#### 7.7.5 Harness built-ins and bundles

Harness-shipped skills are the easiest to miss. They change when the harness is upgraded,
and nobody declared them. At lock time the adapter enumerates every built-in skill the
pinned harness version ships, records each one's digest, and requires the manifest to
enable or disable it (`[skills.builtin]`; the default is *disabled*, so behavior never
depends on something undeclared). Skills inside plugins or packages are expanded and locked
individually (`origin = "bundled"`), so a plugin update that quietly changes one skill shows
up in `hlock diff`.

#### 7.7.6 Interop with existing skill tooling

- **`npx skills` (skills.sh CLI).** It already writes a project `skills-lock.json` and a
  user-level `~/.agents/.skill-lock.json`. `hlock skills import skills-lock.json` turns an
  existing setup into manifest entries. `hlock sync` can also write a compatible
  `skills-lock.json`, so teammates using `npx skills` see the same set. `harness.lock` stays
  the source of truth because it also carries digests, trust, and evidence.
- **Harness installers** (Codex's `$skill-installer`, Claude plugin marketplaces). Skills
  installed this way land in config roots that `check` scans, and they're reported as
  undeclared until added with `hlock add skill` or `hlock skills adopt`.
- **Install targets.** Each adapter maps the portable skill onto its harness's directory
  convention (`.claude/skills/`, `.opencode/…`, `.agents/skills/`, `.codex/skills/`, …). The
  same locked skill installs into any harness the resolver picks.

#### 7.7.7 Skills in tuning and traces

- **Activation is traced.** Adapters emit `skill.load` (metadata), `skill.activate` (body
  read), and `skill.resource` (script run or reference read) events (§10.1). Evidence
  therefore shows which skills were actually used, and `hlock skills unused` lists skills that
  never activated on the holdout split, which are candidates for removal.
- **Skill count is a cost.** Every installed skill spends startup context on its metadata
  and competes for selection. The lock records total metadata tokens, and `optional` skills
  let the resolver find the smallest set that keeps quality.
- **Variants.** `variants` A/B-tests two versions of a skill (a new upstream commit vs. the
  pinned one, or a terse vs. verbose rewrite) against the eval pack.

#### 7.7.8 Skill supply-chain security

Skills are instructions *and* executable code, from an ecosystem of tens of thousands of
mostly unreviewed repos. Published research has already documented malicious skills in
public registries, and cross-skill composition attacks that evade per-skill scanners.
harness-lock therefore:

- pins by commit + tree digest, never by name alone, and treats registries as discovery
  rather than trust;
- scans every skill at lock time (instructions for injection patterns, scripts for
  network, credential, and filesystem access, and `allowed-tools` breadth) and records the
  result;
- **scans the locked skill set together**, not just each skill, because composition is where
  per-skill scanners fail;
- enforces `[skills.trust]` (allowed sources, required license, pinned commits, scan pass,
  signatures);
- runs skill scripts only inside the sandbox, under the stack's `[policy]`;
- includes safety graders in official eval packs that try to trigger skill-borne injection,
  so a stack whose skills make it unsafe can't win.

The scanner is pluggable (`hlock skills scan --scanner …`). The built-in one is a baseline, not
a guarantee.

### 7.8 Harness-aware workflow: profiles, pin, sync, launch

Most people already have a harness they like (Claude Code, Codex, Pi, OpenCode, Hermes,
Cursor, …) with a model, MCP servers, skills and plugins configured by hand. This section
makes hlock aware of *that* setup: it can target a harness, pin its current state into
`harness.lock`, rehydrate it on another machine, and launch an isolated session that matches
the lock. The loop is `hlock pin` → commit → `hlock sync` → `hlock launch <harness>`.

#### 7.8.1 Harness and runtime profiles

A **profile** is a declarative TOML description of one harness or one local inference
runtime. Profiles replace per-harness code: adding a harness means writing a file, not a
release. A harness profile says:

| Field | Purpose |
|---|---|
| `id`, `aliases`, `kind` | Identity. `kind` is `cli` (an agent CLI), `framework` (an agent library), or `custom` (a project's own agent). |
| `binaries`, `version.args` | How to find it on `PATH` and read its version. |
| `active-env` | Variables the harness sets in processes it spawns. Their presence means "hlock is running inside this harness". |
| `config-root` | Where user config lives and the variable that relocates it (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `HERMES_HOME`, `CURSOR_CONFIG_DIR`, `XDG_CONFIG_HOME` + `opencode`). |
| `install` | Install methods (`npm`, `brew`, `brew-cask`, `pip`, `uv-tool`, `script`, `app`, `manual`) with path hints, so hlock can tell how the copy on this machine was installed and reproduce it. |
| `files` | Named config files with a format (`json`, `jsonc`, `toml`, `yaml`) and a restore policy: `whole`, `pointers` (only some subtrees, e.g. `/mcpServers` of `~/.claude.json`), or `record` (digest only, e.g. project files already in git). |
| `model`, `providers` | Ordered sources for the default and small model, provider, and base URL, written as `env:VAR` or `<file>#<json-pointer>`. |
| `mcp`, `skills`, `bundles`, `instructions` | Where MCP servers, skill directories, plugins/packages, and instruction files live. |
| `launch`, `run` | How to start a session (CLI) or run the agent (code harness). |

Runtime profiles (Ollama, LM Studio, llama.cpp, vLLM, SGLang, MLX LM, LocalAI, Docker Model
Runner, Jan) add default ports and provider names (to recognize a harness's base URL), the
model store layout, and pull/serve command templates. Unsloth isn't a runtime; its models are
Hugging Face repos (`hf.co/unsloth/…`) that a runtime serves, so they pin as model sources.

Built-in profiles ship inside `harness-lock-core`. Users add or override them by `id` in
`~/.config/hlock/profiles/{harnesses,runtimes}/*.toml`, `$HLOCK_PROFILE_PATH`, and
`./.hlock/profiles/` (later wins). Profiles marked `support = "full"` are covered by tests
for pin, sync and launch; `basic` ones cover detection, version, MCP and skills.

#### 7.8.2 Targeting a harness

The target harness is part of the manifest, because it changes what the lock means:

```toml
[target]
harness = "claude-code"        # id or alias
```

Commands take `--harness <name>` as a one-off override. (Not `--target`, which `[backend]`
already uses for the execution backend.) Resolution order: `--harness`, `HLOCK_HARNESS`,
`[target] harness`, the harness hlock is running inside, then the only installed harness.
`hlock init` writes `[target]` when the choice is unambiguous.

For `resolve`, the target narrows `[candidates] harness` (it becomes the sole candidate, or
must be one of them) and the lock's `[harness]` pins the version and install source of the
installed copy. Trials still run on the backend's adapter until native adapters exist, and
`[harness] adapter` records which one ran, so evidence is never mislabeled. `check` fails when
the lock's harness differs from `[target]` or when the installed version differs from the pin.

#### 7.8.3 Code harnesses: frameworks and custom agents

Not every agent is a CLI. Pydantic AI agents, LangChain Deep Agents, LangGraph graphs, OpenAI
Agents SDK apps, and hand-written loops are declared in the manifest:

```toml
[harnesses.triage]
framework = "pydantic-ai"            # inherit the framework's pinned packages
entrypoint = "triage.agent:run"      # python module:callable, called as fn(prompt, ctx)

[harnesses.loop]
command = ["./bin/agent", "--model", "{model}", "{prompt}"]   # any executable
packages = ["pypi:httpx"]
```

A project harness is a `custom` profile, so targeting, pin, sync and launch treat it like any
other. Framework profiles (`pydantic-ai`, `deepagents`, `langgraph`, `openai-agents`,
`claude-agent-sdk`, `google-adk`, `smolagents`, `crewai`) contribute the packages to pin.
Because a code harness has an entrypoint, hlock can execute it for real in `resolve` and
`run` through a small contract:

| Input | |
|---|---|
| `HLOCK_PROMPT`, `HLOCK_MODEL` (`provider:model`), `HLOCK_LLM_BASE_URL` | the task and the locked model |
| `HLOCK_WORKSPACE` | working directory for artifacts |
| `HLOCK_MCP_CONFIG` | a generated `mcp.json` (`mcpServers` shape) for the locked MCP servers |
| `HLOCK_SKILLS_DIR`, `HLOCK_LOCK` | locked skills and the lockfile itself |
| `HLOCK_OUTPUT` | optional path for a JSON result: `{success, cost_usd, steps, trace}` |

Exit code `0` means success when no result file is written. A Python `entrypoint` runs
through a small shim, passed with `python -c` so the agent's interpreter needs nothing from
hlock installed, that builds `ctx` from the same variables; there's one execution path.

#### 7.8.4 `hlock pin`: lock the setup you have

`hlock pin [--harness X]` captures the live state of one harness into `harness.lock`, with no
manifest or tuning needed:

- `[harness]`: id, version, install source (`brew-cask:claude-code@2.1.285`,
  `npm:@earendil-works/pi-coding-agent@1.0.0`, …).
- `[[llm]]`: the default model (`role = "default"`) and the small/fast model
  (`role = "small"`) when configured, with provider, base URL, and the runtime serving it.
- `[[runtime]]` (new): local inference runtimes the configured providers point at (matched
  by provider name, then port), their versions, and the models the harness references, with
  digests where the store has them (Ollama manifests, GGUF files, HF revisions). Remote
  endpoints are recorded as endpoints only.
- `[[tool]] kind = "mcp"`: each MCP server, normalized across config shapes (`command` as a
  string or a list, `env`/`environment`, `url`, `type`/`transport`, enabled flags), with the
  scope it came from.
- `[[skill]]`: each skill directory with a tree digest (§7.7.2) and an origin: a git
  checkout records remote + commit, a plugin's skills are `bundled`, harness-shipped skills
  are `harness-builtin`, and anything else is `local`.
- `[[native]]`: plugins, packages and extensions (Claude Code plugins + marketplaces, Pi
  packages with their npm lock versions, OpenCode plugins).
- `[[instructions]]`: instruction files with digests.
- `[[config]]` (new): each profile file with its digest and restore policy.
- A top-level `kind = "pin"` and `[provenance] backend = "pin"`; pinned locks carry no evidence.

Content the target machine can't fetch (hand-written user skills, user-level instruction
files, restorable config files) is **vendored** into `.hlock/vendor/` next to the lock, so the
lock plus that directory is self-contained. **Secrets are never written.** Values under keys
that look secret (`*key*`, `*token*`, `*secret*`, `*password*`, `authorization`, `headers`,
`env` values), and auth files, are replaced with a placeholder naming an environment
variable, e.g. `${HLOCK_SECRET_FLYTE_MCP_AUTHORIZATION}`. `sync` and `launch` fill them in.

Re-pinning shows a diff (`~ model: sonnet → opus`, `+ mcp github`). `pin` refuses to overwrite
a resolved lock (one with evidence) unless given `--force` or `-o other.lock`. On a pinned
lock, `hlock check` re-captures and reports drift, so CI can guard an agent setup the same way
it guards a manifest.

#### 7.8.5 `hlock sync`: rehydrate it somewhere else

Installing everything automatically across package managers, OSes and GUI apps is
unrealistic, so `sync` is built around a **plan**, not a script. Each step has an action and a
status:

| Status | Meaning |
|---|---|
| `satisfied` | already true here (version or digest matches) |
| `auto` | hlock does it: write or merge a config file, copy a vendored skill, run the harness's own CLI (`claude plugin install`, `pi install npm:…@x`), pull a model through an installed runtime |
| `manual` | needs the user: install a runtime or app (`brew install ollama`, the LM Studio app), sign in, provide a secret. hlock prints the exact command or link and why it's needed |

`hlock sync` runs the `auto` steps (after confirming, or with `--yes`), then ends with the
remaining manual steps and exits `3` while any are left (`2` is taken by usage errors); it's
safe to re-run until it's clean.
Options:

- `--install` and `--pull`: opt in to package-manager installs and model downloads, which
  are otherwise manual steps with the exact command.
- `--plan` (and `--json`): print the plan without changing anything, for people, scripts and
  agents.
- `--skill [DIR]`: write an Agent Skill (`hlock-rehydrate/SKILL.md`) generated from the lock
  and plan: what to install, in what order, how to verify each step (`claude --version`,
  `ollama list`), and how to finish with `hlock sync` and `hlock check`. A vanilla agent on a
  fresh machine can then do the manual steps under the user's eye.
- `--prefix DIR`: materialize into an isolated config root instead of the user's real one (a
  "virtual environment" for the harness; see launch).
- `--docker [--build]`: emit a `Dockerfile` that installs the harness, copies the
  materialized config root into the image's home, and pre-installs MCP launchers. Model
  weights stay on the host by default; runtimes are reached through `host.docker.internal`.
- `--exact`: also remove MCP servers, skills and plugins that aren't in the lock (off by
  default, because it edits a personal setup).

Before replacing an existing file, sync writes `<file>.hlock-bak`.

#### 7.8.6 `hlock launch`: start a session that matches the lock

`hlock launch [<harness>] [-- harness args]` is how a locked setup gets *used*, the way
`uv run` uses a venv:

1. Verify the installed harness version matches `[harness]` (offer `hlock sync` if not).
2. Materialize the locked config into `.hlock/env/<harness>/` (the same code path as
   `sync --prefix`), filling secret placeholders from the environment.
3. `exec` the harness with its config-root variable pointing there, plus `--model` from the
   lock, so the session sees exactly the locked model, MCP servers, skills and plugins, and
   none of the user's unlocked config (`[context] isolation = "locked-only"`, §5.4).
   Harnesses without a relocation variable fall back to a temporary `HOME`.

`launch` into a different harness than the one pinned translates the **portable** components
(MCP servers, skills, instruction files, and the model when the target can reach its provider)
through the target profile's writers, and lists what doesn't carry over (plugins and
harness-native settings). Code harnesses launch through their entrypoint with the §7.8.3
contract. Sign-in state isn't copied; the first launch of an isolated root may ask the user
to log in once.

#### 7.8.7 `hlock localize`: a local equivalent of any lock

`hlock localize [harness.lock] -o harness.local.lock` takes any lock (pinned, resolved, or
tuned) and produces the best equivalent that uses only local components. The output name
is the user's choice; `.local` is a convention.

What gets replaced:

- **The harness**, if it's proprietary or can't reach a local endpoint. Profiles gain two
  flags: `open-source` and `local-models` (whether the harness can use an OpenAI- or
  Anthropic-compatible local endpoint). Candidates are `--harness <name>` if given, else the
  source harness when it qualifies, else the default set (`opencode`, `pi`, `codex` with its
  OSS provider, `hermes`, `goose`), filtered to what's installed or installable.
- **LLM roles and the decision model**, if they use API providers. They move to a local
  runtime: `--runtime ollama|lmstudio|llama-cpp|vllm|…` if given, else the installed runtimes.
  Runtime profiles gain a small model catalog (`[[catalog]]`: model id, family, parameter
  count, quantization, minimum memory, tool-calling support, and a capability tier per role).
  The default search set is filtered to what fits this machine's RAM/VRAM, and weights already
  on disk come first. `--models a,b,c` overrides the catalog.

What carries over unchanged: the portable components (MCP servers, skills, instructions),
translated as in `launch`. Remote MCP servers are kept but flagged, since they're also network
dependencies (`--strict` drops them).

How the choice is made: with eval packs available (a `harness.toml` with `[evals]`, or
`--evals`), `localize` builds an in-memory search space (local harnesses × local models ×
decision models) and runs it through the `tune`/resolver pipeline with an all-local
constraint. A localized code harness runs for real through the §7.8.3 contract; agent CLIs use
the backend's adapter until native adapters exist. The evidence is compared with the source
lock's (`[evidence.baseline]`). Without tasks, it ranks candidates heuristically (memory fit,
on-disk weights, catalog tier) and marks the lock `evaluated = false`.

The output is an ordinary lock (`kind = "localized"`, `base-lock` naming the source) with
`[[runtime]]` entries whose models are `present = false` until pulled, so the rest of the flow
applies unchanged: `hlock sync --pull` fetches weights, and `hlock launch` starts the local
harness.

#### 7.8.8 Delivery

| Stage | Scope |
|---|---|
| 1 | Profiles (built-in + user + project), detection and probing, `hlock harness list/show`, `hlock runtime list`, `[target]`, `--harness` on `resolve`, target checks in `check` |
| 2 | `hlock pin`, secret redaction, vendoring, pinned-lock rendering in `show`/`tree`, drift in `check` |
| 3 | `hlock sync`: plan, auto/manual steps, `--plan`, `--json`, `--skill`, `--prefix`, `--docker` |
| 4 | `hlock launch`, cross-harness translation of portable components, and the code-harness execution contract for `run`/`resolve` |
| 5 | `hlock localize`: profile `open-source`/`local-models` flags, runtime model catalogs, memory-aware candidate search, and the all-local tune pass |

Open: multi-harness pins in one lock (open question 2), and whether sync should ever run
system package managers by default instead of printing commands.

---

## 8. Tuning backend (Flyte 2)

### 8.1 Why Flyte, and the one-code-path rule

Every backend runs **the same Flyte 2 program**. Local mode is Flyte's local execution,
not a separate code path. That gives us one implementation of retries, caching,
checkpointing, and fan-out, and makes "move up the dial" a config change, not a port.

`flyte` is a hard dependency of `harness-lock`. It's a pure-Python install and needs no
services for local mode.

### 8.2 The dial

| | `local` | `devbox` | `cluster` (Flyte or Union) |
|---|---|---|---|
| Setup | none | Docker; `hlock backend up` wraps `flyte start devbox` | existing cluster + `flyte create config` / config file |
| Where trials run | this Python process (async, bounded concurrency) | containers on the local Flyte devbox | pods on the cluster |
| Durability | local SQLite cache; resume after crash re-uses cached trials | durable run state, object store, survives restarts | durable, shared across users |
| Observability | terminal UI (`--tui`), local reports, `.harnesslock/runs/` | Flyte UI at localhost | Flyte/Union UI, shared |
| Parallelism | ~4–16 concurrent trials | bounded by the laptop | hundreds–thousands of trials |
| GPUs | whatever the machine has | `flyte start devbox --gpu` on a CUDA host | GPU node pools: serve open-weights LLMs, train decision models |
| Cache sharing | per machine | per machine | per org/project: teammates reuse each other's trials |
| Good for | first lock in 10 minutes, small packs, API-only models | long runs on one box, overnight sweeps | big search spaces, open-weights serving, fine-tuning, team use |

Switching:

```bash
hlock resolve                                  # local (default)
hlock backend up                            # starts a devbox (flyte start devbox) and writes config
hlock resolve --backend devbox
hlock resolve --backend cluster                # uses ~/.flyte/config.yaml or [backend] in manifest
```

Under the hood, `--backend local` maps to Flyte local run mode; `devbox` and `cluster` call
`flyte.init(...)` / `flyte.init_from_config(...)` with the corresponding endpoint and then
`flyte.run(...)` the tuning pipeline. harness-lock owns image building for the trial
environment (`flyte.Image`), so users don't write Dockerfiles.

### 8.3 How durability maps onto the tuning pipeline

The tuning pipeline is a Flyte program:

```
lock_pipeline(manifest)                         @env.task
├── resolve_components(manifest)               @env.task  cache  (alias → pinned IDs, versions)
├── build_trial_images(resolved)               flyte.Image builds (devbox/cluster only)
├── for each round (strategy-driven):
│   ├── plan_round(state)                      @env.task  (strategy proposes stacks)
│   ├── run_trials(stacks × tasks × seeds)     fan-out via asyncio.gather / flyte.map
│   │   └── run_trial(stack, task, seed)       @trial_env.task  cache, retries
│   │       ├── model call                     @flyte.trace  (checkpointed; replay on retry)
│   │       ├── tool call                      @flyte.trace
│   │       └── decision call                  @flyte.trace
│   ├── grade_trials(results)                  @env.task  cache
│   └── update_state(grades)                   @env.task
├── train_decision_model(traces)  [optional]   @gpu_env.task  cache
├── evaluate_holdout(finalists)                fan-out of run_trial
├── fingerprint(finalist LLMs)                 @env.task
└── write_resolve(winner, evidence)               @env.task → harness.lock + store blobs
```

Durability properties this gives us:

- **Trial-level caching.** `run_trial` is cached on `(stack hash, task hash, seed,
  adapter version)`. Re-running `lock` after a crash, or `update`-ing one component, only
  executes trials that don't exist yet. On a cluster, the cache is shared by the team.
- **Step-level checkpointing.** Model/tool calls inside a trial are `@flyte.trace`d, so a
  retried trial replays completed calls instead of paying for them again.
- **Retries** at the trial level for infra failures (provider 5xx, preemption), kept
  separate from agent-level retries, which are a *policy under test*.
- **Budget enforcement** in `plan_round` and as a hard stop in `run_trial` (spend is
  tracked per trial output; the planner refuses rounds that would exceed `max-spend-usd`).
- **Rate limiting** per provider via bounded concurrency in the fan-out, plus reusable
  containers (`flyte.ReusePolicy`) for trial workers on devbox/cluster to avoid cold starts.
- **Human-in-the-loop** for `human-approval` policies during tuning: by default a
  *simulated* approver (a scripted policy defined by the eval pack) so tuning is unattended;
  optionally real approvals through Flyte's human-in-the-loop mechanism on devbox/cluster.
- **Lineage.** Every lock's `provenance.run` points at the Flyte run that produced it,
  and every evidence blob points at the trial outputs.

### 8.4 Search strategies

- `grid`, `random`: small spaces, baselines.
- `successive-halving` / Hyperband: run all candidates on a few tasks, keep the top
  fraction, add tasks. Default for most spaces; this is where cost savings come from.
- `bayesian`: model-based (Optuna-compatible sampler) over mixed categorical/numeric axes.
- `auto`: picks based on space size and budget.
- **Multi-objective.** Every strategy maintains a Pareto front over (quality, cost,
  latency); `[objective]` picks the winner from the front.
- **Statistical guardrails.** Final comparisons use paired bootstrap on the holdout split.
  If the winner isn't distinguishable from the runner-up, the lock records that and
  `select` tie-breaks by cost.

Strategies are pure Python behind a `Strategy` protocol (§9.2), so they're usable without
knowing Flyte.

### 8.5 Tuning pipeline (`hlock tune`)

`tune` is the compute-heavy step: instead of *selecting* among given components (the resolver,
§4.1), it *produces* improvements to the harness — prompt tuning, hyperparameter optimization,
and fine-tuning of small LLMs / decision models — and writes them to `harness.tune.lock` (§5.9).
It runs on the same Flyte 2 pipeline as `lock` (§8.1–8.3), inserting an **tune stage**
between `grade_trials` and `select`, so all durability properties (caching, resume, retries,
lineage, fan-out) apply. On `local` it is the same in-process path.

```
hlock tune [flags]  →  harness.tune.lock  (+ report + store blobs)
```

**Depth of tuning** (`--depth`, defaults to `auto`). The user chooses how deep to go;
each level is a superset of the previous:

```bash
hlock tune --depth probe            # cheap prompt/policy probes (minutes, no retrain)
hlock tune --depth prompt           # + system-prompt & instructions search (applied as new digests)
hlock tune --depth hpo              # + hyperparameter optimization (retries, temps, timeouts, context…)
hlock tune --depth ft               # + fine-tune small decision/judge/LLMs (LoRA) on the chosen data
hlock tune --depth full             # + workflow compilation + end-to-end holdout re-validation
hlock tune --depth auto             # pick the deepest level the budget allows
```

**Data source** (`--data`). Tuning is driven by real behavior where possible and
synthetic data where not:

- **User traces** — `--data user` (default when harness trace stores are found). `tune`
  finds traces using each harness adapter's knowledge of where that harness records runs
  (`config_roots`, §7.4; trace store, §10):
  - proxy-instrumented CLI harnesses: `~/.config/opencode/…`, `~/.pi/…`, `~/.claude/…`, `~/.codex/…`
  - Flyte-backed harnesses: runs under `flyte://org/project/domain`
  - explicit: `--data user:./runs,harnesslock-traces.jsonl` or `hlock traces export` output
  - a hub pack: `--data hub:harness-lock/traces`
- **Synthetic** — `--data synthetic[:<n>]`: generates tasks from the eval pack (failure
  seeding, tool-error recovery, prompt-injection attempts) plus optional LLM-authored
  variants to cover the space real traces under-sample.

**Flags & subcommands** (options are composable):

```bash
hlock tune --harness opencode          # restrict to one harness
hlock tune --prompt-only               # only pitch prompt/instructions changes (no training)
hlock tune --no-finetune               # skip GPU fine-tuning
hlock tune --axes retries,temperature,tool-timeout-seconds
hlock tune --max-steps 60 --max-tokens 2000
hlock tune --out harness.tune.lock     # default filename — user-renamable
hlock tune --budget spend-usd=50,gpu-h=1,max-trials=1000
hlock tune --strict                    # fail unless the tune lock beats the base on [objective]
hlock tune --apply                     # also adopt the result into harness.lock
hlock tune diff                        # tuned changes vs: base (evidence + content)
hlock tune reuse-traces --dir …        # point at an exact trace directory
hlock tune dry-run                     # print plan + cost estimate, run nothing
hlock tune --detach                    # run as a background tune job
hlock tune list | show [job] | fetch [job] [--wait] [--apply] | cancel <job>
```

**Background jobs.** Tunes run for minutes to days, so `--detach` starts a *tune job*:
a detached orchestrator process with its own directory under `.harnesslock/tune/jobs/<id>/`
(settings, status, log, trial artifacts, and the resulting tune lock). A job never writes
the project's locks; `fetch` copies its result to `harness.tune.lock` (and `--apply` adopts
it), warning when `harness.lock` changed since the job started. Today the orchestrator runs
on the submitting machine on every backend, with only the heavy stages on Flyte for
`cluster`. Planned: on `cluster`, submit the orchestrator itself as a Flyte run so a job
survives the laptop sleeping, with the job record holding the run URL (`hlock.attach(url)`,
§9.2) and `list`/`show`/`fetch` reading status and outputs from the cluster.

**Evaluation discipline.** `tune` re-runs the tuned stack against the **holdout** split
(at `--depth full`, or on request) and only records a gain when it is distinguishable with
bootstrap CIs (§8.4). `--strict` refuses to emit `harness.tune.lock` if it does not beat the base
on `[objective]`. Everything tuning produces — applied prompts, chosen hyperparameters,
fine-tune LoRA weights, the data split, the budget — is content-addressed in the store and
recorded in `harness.tune.lock` (§5.9), so tuning is itself reproducible from its
provenance run and diffable in a PR.

---

## 9. Layered API

The rule: **each layer down exposes more control and requires more knowledge. Flyte appears
only at Layer 3.** Layers 0–2 import nothing from `flyte` in user code.

```
Layer 0  CLI + harness.toml             "I have tasks and API keys"
Layer 1  Python API (hlock)          "I want to script and embed this"
Layer 2  Extension points + profiles      "I need custom graders/strategies/adapters/resources"
Layer 3  Flyte-native (hlock.flyte)  "I'm composing this into my own Flyte pipelines/infra"
```

### 9.1 Layer 0: CLI

The package is `harness-lock`, and its command is **`hlock`**, matching the Python import
convention `import harness_lock as hlock`. A long-form `harness-lock` alias is installed too, for
scripts and for environments where `hlock` is already taken.

```bash
pip install harness-lock
hlock init                               # interactive: pick a pack, providers, harness → harness.toml
hlock resolve                               # search → harness.lock (+ .harnesslock/report.html)
hlock show                               # pretty-print the lock + evidence
hlock tree                               # component dependency graph (models, harness, skills, MCP, capabilities, …)
hlock add|remove <kind> <src>            # add/remove a peripheral (skill, mcp, tool, native kind) and re-lock it
hlock sync                               # install/pin harness, peripherals, sandbox image, adapters
hlock run "fix the failing test in ./repo"             # run the locked agent with runtime guard
hlock check                              # static lock verification (CI)
hlock verify                             # canary + fingerprint re-check (scheduled CI)
hlock update <component>|--all           # partial/full re-resolve (within range, §5.7)
hlock upgrade [component…|--all] [--dry-run] [--apply|--rollback] [--to <version>]   # major/breaking, undoable (§5.8)
hlock diff [--format md]                 # structural + evidence diff vs. committed lock
hlock tune [--depth probe|prompt|hpo|ft|full|auto] [--data user|synthetic[:n]|hub:…|user:<paths>] [--out NAME] [--strict] (see §8.5)
                                       # tune the harness -> harness.tune.lock
hlock export --format docker|flyte|langgraph           # emit a deployable bundle for the locked stack
hlock leaderboard [--pack <name>]                      # ranked stacks from the last search (Pareto front)
hlock outdated                           # locked peripherals (skills, MCP, plugins, harness) with newer upstream versions
hlock skills import|adopt|scan|unused    # import skills-lock.json or user skills, adopt undeclared, scan, find unused
hlock evals init|check|templates|new|pull|push
hlock traces ls|show|export              # browse traces; export as HF dataset
hlock traces replay <trial>              # re-run a trial from its recorded trace (§10.3)
hlock traces explain <trial>             # failure triage via harness-lock/triage (§11)
hlock backend up|down|status             # devbox lifecycle (wraps flyte start/stop/get devbox)
```

`export` turns the lock into something deployable outside harness-lock, so the result is
an installable domain agent system and not only a leaderboard:

```
dist/
  harness.lock            # copied verbatim; the source of truth
  agent.yaml            # harness-native config rendered from the lock
  policy.yaml           # runtime policies for the guard
  eval_report.md        # evidence, leaderboard, and baseline comparison
  traces/               # holdout traces backing the evidence
  docker-compose.yaml   # --format docker: harness + proxy + sandbox + (optional) vLLM
  flyte_workflow.py     # --format flyte: the agent as a durable Flyte task
  graph.py              # --format langgraph: LangGraph graph whose nodes call the locked models
```

### 9.2 Layer 1: Python API

```python
import harness_lock as hlock

manifest = hlock.Manifest.load("harness.toml")
lock = harness.lock(manifest, backend="local")       # returns hlock.Lock; writes harness.lock
print(lock.evidence.summary())

# Programmatic manifest
space = hlock.SearchSpace(
    llm={"planner": ["anthropic:claude-sonnet-latest", "vllm:Qwen/Qwen3-32B"]},
    decision=["none", "hf://harness-lock/decider-1.5b"],
    harness=["opencode", "pi"],
    policy={"retries": [0, 1]},
)
lock = harness.lock(
    space,
    evals=hlock.EvalPack.pull("harness-lock/data-cleaning"),
    objective=hlock.Objective(primary="success-rate", select="min-cost",
                           constraints={"success-rate": ">=0.8"}),
    budget=hlock.Budget(max_spend_usd=50),
    backend="devbox",
)

# Use the lock at runtime
agent = hlock.load("harness.lock")
result = await agent.run("Clean ./data/orders.csv and report anomalies")
```

`harness.lock(...)` returns a handle that can be awaited, watched (`.stream()` yields trial
events), or detached on devbox/cluster (`.url`, `hlock.attach(url)`).

### 9.3 Layer 2: Extension points and profiles

Still no Flyte imports. Users plug in Python objects that harness-lock schedules for them.

```python
@hlock.grader("revenue_matches")
def revenue_matches(workspace: hlock.Workspace, trace: hlock.Trace) -> hlock.Grade:
    got = float(workspace.read("answer.txt"))
    return hlock.Grade(success=abs(got - 1_204_331.17) < 0.01)

class MyStrategy(hlock.Strategy):
    def propose(self, history: hlock.History, budget: hlock.BudgetState) -> list[hlock.Stack]: ...

@hlock.adapter("my-harness")
class MyHarness(hlock.HarnessAdapter): ...

@hlock.component_kind("my-harness.middleware")       # new lockable peripheral kind (§7.6)
class Middleware(hlock.ComponentKind): ...
```

**Profiles** describe compute and durability in harness-lock terms, mapped to Flyte by the
backend:

```toml
[backend.profiles.default]
trial = { cpu = 2, memory = "4Gi", concurrency = 32, retries = 2, reuse = { replicas = 8 } }

[backend.profiles.gpu-training]
train = { gpu = "A100:1", memory = "64Gi", timeout = "4h" }
serve = { gpu = "H100:2", engine = "vllm" }      # for open-weights candidates
```

```python
harness.lock(manifest, backend="cluster",
        profile=hlock.Profile(trial=hlock.Resources(cpu=2, memory="4Gi", concurrency=64)))
```

Profiles are ignored where they don't apply (e.g. resource requests in local mode), matching
Flyte's own behavior.

### 9.4 Layer 3: Flyte-native

For users who want to compose tuning into their own Flyte workflows, run it on a schedule,
use custom images or secrets, or change durability behavior. Here, Flyte knowledge is
expected.

```python
import flyte
from hlock.flyte import envs, tasks

# Reuse and customize harness-lock's TaskEnvironments
trial_env = envs.trial.clone_with(
    name="acme_trials",
    image=envs.trial.image.with_pip_packages("acme-internal-tools"),
    secrets=[flyte.Secret(key="ACME_API_KEY")],
    resources=flyte.Resources(cpu=4, memory="8Gi"),
    reusable=flyte.ReusePolicy(replicas=16, idle_ttl=600),
)

env = flyte.TaskEnvironment(name="acme_agent_ops", depends_on=[trial_env, envs.core])

@env.task
async def nightly_relock(manifest_path: str) -> str:
    manifest = await tasks.load_manifest(manifest_path)
    resolved = await tasks.resolve_components(manifest)
    results = await tasks.run_search(resolved, trial_env=trial_env)   # same pipeline, custom env
    lock = await tasks.write_resolve(results)
    await open_pr_with_resolve(lock)                                       # user's own task
    return lock.hash

if __name__ == "__main__":
    flyte.init_from_config()
    run = flyte.run(nightly_relock, manifest_path="s3://acme/agents/harness.toml")
    print(run.url)
```

Layer 3 surface (`hlock.flyte`):

- `envs`: `core`, `trial`, `grade`, `train`, `serve`, as `flyte.TaskEnvironment`s.
- `tasks`: every pipeline stage in §8.3, individually importable and composable.
- `images`: the base images, as `flyte.Image` builders.
- `run_trial` as a building block for users who want their own search loop.

**Stability contract.** Layers 0–1 follow semver strictly. Layer 2 is stable from v0.3.
Layer 3 is marked *provisional* until v1.0 and versioned alongside the minimum supported
Flyte 2 version.

---

## 10. Traces and datasets

### 10.1 Trace schema

Built on the OpenTelemetry GenAI semantic conventions, with harness-lock extensions, so traces
can be exported to any OTel backend and imported from OTel-instrumented agents.

```json
{
  "trace_id": "…",
  "trial": { "stack": "sha256:…", "task": "ml-engineering/debug_failed_training_run_014", "seed": 1 },
  "events": [
    { "type": "llm.call", "role": "planner", "model": "claude-sonnet-5-5",
      "input_ref": "sha256:…", "output_ref": "sha256:…",
      "usage": { "input_tokens": 4120, "output_tokens": 611 }, "cost_usd": 0.021, "latency_ms": 2310 },
    { "type": "decision", "kind": "retry", "model": "harness-lock/decider-1.5b",
      "choice": "modify_and_retry", "confidence": 0.81 },
    { "type": "tool.call", "tool": "python", "input_ref": "sha256:…", "output_ref": "sha256:…",
      "status": "error", "error_class": "ModuleNotFoundError", "latency_ms": 840 },
    { "type": "skill.activate", "skill": "pytorch-debugging", "digest": "sha256:7c10…" },
    { "type": "approval", "action": "filesystem.write:/etc/hosts", "outcome": "denied", "by": "simulated" }
  ],
  "result": { "artifacts": ["patch.diff"], "final_ref": "sha256:…" },
  "grades": [ { "grader": "tests:check.py", "success": true, "score": 1.0 } ],
  "metrics": { "steps": 17, "cost_usd": 0.31, "latency_s": 48.2, "retries": 1, "human_interventions": 0 }
}
```

Payloads are content-addressed and stored separately so traces stay small and shareable,
and payloads can be redacted without breaking structure (`hlock traces export --redact`).

The trace layer is designed to stand on its own ("OpenTelemetry for agent traces"): the
schema, recorder, and replay are usable without the resolver, so teams can adopt tracing first
and tuning later.

### 10.2 Public datasets (Hugging Face)

- `harness-lock/traces`: traces from tuning runs over the official packs, across many
  stacks. Successes and failures, with grades. The training corpus for §11.
- `harness-lock/failures`: failed trajectories labeled with failure class and the recovery
  step that worked in a sibling trial.
- `harness-lock/evals-*`: the official eval packs (holdout splits withheld).

Users opt in to contributing traces (`hlock traces push --public`) with a mandatory
redaction pass. Nothing is uploaded by default.

The trace corpus is likely the most defensible long-term asset: it trains §11's models and
seeds new eval packs.

### 10.3 Replay

`hlock traces replay <trial>` re-executes a trial using its recorded trace:

- `--mode exact` serves every model and tool response from the trace. This checks
  harness/adapter determinism and is free.
- `--mode from-step N` replays up to step N, then runs live. This is for debugging a
  failure point, or for testing a new decision model or policy against a real prefix.
- `--mode live` re-runs from scratch with the locked stack and compares outcomes.

On devbox and cluster, replay rides on Flyte's `@flyte.trace` checkpoints (§8.3). Locally,
it reads the trace store.

---

## 11. Open-weights models

Small, narrow, permissively licensed (Apache-2.0), released on Hugging Face under
`harness-lock/`, fine-tuned from a permissively licensed small open base model.

| Model | Size (target) | Job | Used by |
|---|---|---|---|
| `harness-lock/decider` | ~0.5B and ~1.5B | Structured decisions: route / retry / escalate / stop / tool | `[decision]` in locks; base for per-project LoRA tuning |
| `harness-lock/judge` | ~7–8B | Rubric grading of trajectories and artifacts, calibrated to human labels | `judge:` graders (default) |
| `harness-lock/triage` | ~1.5B | Classify a failed trace: model vs. harness vs. tool vs. task-spec failure; suggest the fix | `hlock traces explain`, resolver analysis |

Design requirements:

- Each runs on CPU (decider) or one consumer GPU (judge), so `local` mode can use them.
- Each ships with a model card, eval results on held-out packs, and the exact training
  recipe as an harness-lock/Flyte pipeline (`hlock train decider …`), so anyone can
  reproduce or retrain on their own traces.
- The judge is calibrated against human-labeled grades. Calibration error is reported, and
  the lock records which judge produced the evidence.

Marketing hooks: *"A 1.5B decision model that cuts agent cost by X% at equal success on
Y eval packs."* and *"We trained a small model to debug failed agents from their traces."*
(triage). Only claim numbers we can publish with the reproduction pipeline.

These models are deliberately narrow companion artifacts. "We fine-tuned a general
assistant" is neither defensible nor useful here.

---

## 12. Security and safety

- **Secrets** are never written to the lock, traces, or store. Provider keys are resolved
  by name from env/`.env` (local) or the backend's secret store (devbox/cluster).
- **Sandboxing is on by default** for anything that executes code or touches the filesystem.
  `subprocess` sandboxing requires explicit opt-in.
- **Tool permissions** in the lock are least-privilege by default: the resolver only grants
  permissions the manifest lists, and the runtime guard enforces them.
- **Supply chain.** Harnesses, MCP servers, plugins, and packages are pinned by version and
  integrity hash; skills by directory digest; sandbox images by digest; models by weights
  digest. `check` fails on mismatch.
- **Skills and plugins are code.** Skills can ship scripts, and plugins, hooks, and
  extensions run arbitrary code inside the harness. `lock` and `add` show what a new
  peripheral can execute (scripts, hook commands, requested permissions). Such peripherals
  only run inside the sandbox, and undeclared ones are refused under `isolation = "locked-only"`.
- **Remote peripherals** (remote MCP servers, A2A agents, provider-native tools) can't be
  pinned by bytes. Their advertised schemas are fingerprinted, OAuth scopes are locked, and
  any change in what they advertise is a `verify` failure.
- **Trace hygiene.** Redaction hooks run before any export. Public trace contributions
  require a redaction pass and a license acknowledgment.
- **Safety graders.** Every official pack includes trace graders for exfiltration,
  destructive actions, and approval-bypass. A stack with safety violations can't be selected
  unless the manifest explicitly allows it.

---

## 13. Package layout

```
hlock/
  __init__.py            # Layer 1: lock(), load(), Manifest, SearchSpace, Objective, Budget, …
  cli/                   # Layer 0
  manifest/              # harness.toml parsing, validation, hashing
  lockfile/              # harness.lock schema, (de)serialization, diff, check
  store/                 # content-addressed blob store (local dir, s3/gcs/azure via fsspec)
  tasks/                 # eval packs, graders, splits, Hub client
  providers/             # LLM providers, alias resolution, pricing, fingerprints, serving
  decision/              # rules, llm, model decision backends; training recipes
  components/            # component model: ComponentKind registry, pin strategies, graph, portable kinds
                         #   (tool, mcp, skill, instructions, subagent, hook, memory, retrieval, …)
  adapters/              # harness adapters + their native kinds: reference, opencode, pi,
                         #   claude_agent_sdk, pydantic_ai, langgraph, …
  proxy/                 # model proxy + MCP proxy (tracing, enforcement, metering)
  sandbox/               # subprocess, docker, cluster runtimes
  traces/                # schema, recorder, OTel import/export, redaction
  search/                # Strategy protocol + grid/random/halving/bayesian/auto, Pareto, stats
  runtime/               # load(), guard(), run()
  backends/              # Layer 2: Profile/Resources → Flyte mapping; local/devbox/cluster
  flyte/                 # Layer 3: envs, tasks, images (the tuning pipeline)
examples/
  research-agent/        # harness.toml + committed harness.lock for each example
  data-analysis-agent/
  code-execution-agent/
```

Install extras: `harness-lock[vllm]`, `harness-lock[train]`, `harness-lock[docker]`,
`harness-lock[all]`. Python ≥ 3.10. Works on macOS, Linux, and Windows (WSL for docker sandbox).

---

## 14. Roadmap

**v0.1: "first lock in 10 minutes"** (local only)
- `init`, `lock`, `show`, `check`, `run` · `harness.lock` schema v1
- Adapters: `reference`, `opencode`, `claude-agent-sdk`
- Component model with portable kinds `tool`, `mcp` (local), `skill`, `instructions`;
  context isolation; `tree`, `add`, `remove`
- Skills: git, path (editable), and registry sources; tree digests; frontmatter validation;
  harness built-in enumeration; `skills import` from `skills-lock.json`; baseline scanner
- API providers only; grid/random/successive-halving; tests + judge graders (judge = any LLM)
- One official pack: `research-analyst`
- Flyte local execution with trial caching and resume

**v0.2: "durable and updateable"**
- `devbox` backend, `backend up/down`, Flyte UI links in output
- `update`, `diff --format md`, `verify` with fingerprints
- Adapters: `pi`, `pydantic-ai` (with `pydantic-ai.capability`), `langgraph` · packs:
  `data-cleaning`, `ml-engineering`
- Portable kinds `subagent`, `hook`, `command`, remote `mcp` · native kinds for each v0 adapter
  · peripherals as search axes (`optional`, `any-of`, capability args)
- Docker sandbox default · trace export to OTel and HF

**v0.3: "decision models"**
- `harness-lock/decider` + `harness-lock/judge` released · decision-model tuning stage
- Layer 2 extension points stable, including the public `ComponentKind` entry point · profiles
- Portable kinds `memory`, `retrieval`, `guardrail`, `browser`
- `evals init --from-traces` from OTel/LangSmith traces

**v0.4: "scale"**
- `cluster` backend (self-hosted Flyte and Union) · shared team cache
- Open-weights candidates served on demand (vLLM/SGLang) · LoRA tuning of executor LLMs
- `harness-lock/triage` · public `harness-lock/traces` dataset · leaderboard site per pack
- **`upgrade`** (§5.8) with plan/apply/rollback · **`tune`** (§8.5) with `probe|prompt|hpo|ft`
  depths fuelled by user traces (auto-discovered per harness) or synthetic data → `harness.tune.lock`

**v1.0**
- Lock schema v1 frozen · Layer 3 stable · `export` targets · 10+ adapters · 8+ packs

---

## 15. Launch, positioning, and success metrics

### 15.1 Taglines

- *Lock your agent into place.*
- *Don't pick an agent framework. Benchmark your domain and lock the stack.*
- *Stop vibe-checking agents. Replay, evaluate, and lock them.*
- *AutoML for agent harnesses, tool-use policies, and workflow architectures.*

### 15.2 The demo

**"We tuned an AI data analyst for your company in 20 minutes."**

1. Start from 20–50 domain tasks (an official pack, or `evals new --from-traces`).
2. harness-lock tries stacks such as frontier LLM + LangGraph, Claude + OpenCode, Qwen +
   custom workflow, small decider + big-model fallback, and local model on a cluster backend.
3. `hlock leaderboard` shows the Pareto front.
4. The findings are the story. Illustrative examples of what we expect to show:
   - the frontier-only stack is best but expensive,
   - an open model + decision model gets ~90% of the quality at a fraction of the cost,
   - OpenCode beats generic tool-calling on code-edit tasks,
   - a deterministic workflow beats an agent loop for repeatable data prep.
5. `hlock resolve` writes `harness.lock`. `export` ships it. The lock lands in a PR.

The illustrative numbers must be replaced with real, reproducible results before launch.

### 15.3 Shareable comparisons

Per-pack leaderboards produce charts and debate:

- "Claude vs GPT vs Qwen on real agent workflows."
- "LangGraph vs OpenCode vs Pi on the same tasks."
- "Which agent stack survives tool failures?"
- "Your agent isn't production-ready until it has an `harness.lock`."

### 15.4 Launch artifacts

GitHub repo · PyPI package · HF models (§11) · HF datasets (§10.2) · official eval packs ·
per-pack leaderboard · three example agents (research, data analysis, code execution) ·
Flyte/Union cluster backend docs.

### 15.5 Success metrics

- Time from `pip install` to first `harness.lock` on the local backend: **< 10 minutes**.
- `harness.lock` files in public GitHub repos (search `filename:harness.lock lock-version`).
- Community eval packs and adapters published.
- Downloads of `harness-lock/decider` and `harness-lock/judge`.
- Share of locks produced on devbox/cluster backends (the dial is working).
- Published case studies where a locked stack matched frontier-only quality at materially
  lower cost.

---

## 16. Open questions

1. **Name collision.** **Resolved:** the manifest is `harness.toml` and the lockfile is `harness.lock`.
   Still worth checking `harness.lock` against other
tools' lockfiles before v1.0.
2. **Lock scope.** One lock per agent, or a multi-harness lock (`[[agent]]` tables) for systems
   with several cooperating agents?
3. **Prompt optimization — folded into `hlock tune`.** System-prompt / instruction search now
   lives in the `prompt` and `hpo` depths of `hlock tune` (§8.5), where candidate prompts are
   tuned and locked as new content digests in `harness.tune.lock` (§5.9) rather than floating
   search axes in the manifest. Open question: how much of this *authorship* (prompt/instruction
   edits) should also surface as first-class manifest search axes in v1, vs. staying an
   tune-only concern?
4. **Workflow compilation.** Compile repeated successful trajectories into deterministic
   Flyte workflows, so steps that don't need to be agentic stop being agentic, and
   generate a workflow from a task plan. That's a strong differentiator (and a natural use
   of the triage model and trace corpus) but a large scope. Treat it as a post-v1
   `hlock compile` command, with the compiled workflow becoming a lockable
   `harness = "workflow:…"` candidate?
5. **Closed-model drift policy.** When a provider retires a pinned model ID, should
   `verify` auto-propose an `update`, or only fail?
6. **Judge trust.** How much of the evidence may come from judge graders before the lock
   is flagged "low-confidence"?
7. **Hub.** Host eval packs and locks on Hugging Face only, or run a small registry with
   a leaderboard per pack?
8. **Harness licensing.** Some harnesses have restrictive terms for automated or benchmark
   use. We need a per-adapter compliance note before publishing leaderboards.
9. **Tuning peripheral contents.** Beyond include/exclude, should the resolver *edit* skills and
   instruction files (prompt-optimization style) and lock the improved versions as new
   digests? That's powerful, but it means harness-lock authors content, not just selects it.
10. **Skill registries and publisher trust.** Registries (skills.sh and others) are treated
    as discovery only, and pins are always git commits or OCI digests (§7.7). Should
    harness-lock ship a default `[skills.trust]` allowlist of vendor catalogs? Should it push
    for signed skill releases (Sigstore) as an ecosystem norm, and contribute a digest/signature
    field upstream to the Agent Skills spec?
11. **Isolation default.** `locked-only` isolation is the correct default for reproducibility
    but will surprise people who rely on personal `~/.claude` / `~/.config/opencode` setups.
    Should `hlock init` offer to import those into the manifest?
12. **Capability drift in fast-moving libraries.** `pydantic-ai-harness` is on 0.x
    versioning with frequent API changes. Should native kinds declare a supported version
    window that the resolver enforces?

---

## Appendix A. Concept history and alternatives considered

harness-lock came out of an exploration of which customer problems could be fully or mostly
solved by an open artifact (model, dataset, harness, or installable system) that anyone
could try, not just Union customers.

**Evolution of the concept:**

1. **TraceForge / HarnessBench**: an open benchmark and runtime kit. A lightweight local
   harness, a public agent-trace dataset, and a benchmark measuring completion, tool-use
   correctness, recovery, cost, latency, determinism, replayability, and human-intervention
   rate, with a Flyte/Union backend for durable runs. *Kept as:* the trace layer (§10), the
   eval packs and graders (§7.1), and leaderboards (§15.3).
2. **HarnessForge**: reframed from "evaluate and replay agents" to "produce the best
   domain-specialized agent system". It tunes LLM + decision model + harness + tools +
   policy against domain eval packs and exports the winner. TraceForge became its data
   layer. *Kept as:* the resolver (§8), the manifest search space (§6), decision models (§7.3),
   and `export` (§9.1).
3. **harness-lock**: adds the missing piece, a sharable, enforceable, version-controlled
   result. The lockfile turns a one-time optimization into something you can commit,
   review, verify in CI, and update incrementally. It also adds the local → devbox →
   cluster dial and the layered API, so the tool is useful without any infrastructure.

**Alternatives considered:**

| Option | Artifact | Verdict |
|---|---|---|
| Agent benchmark + trace dataset | repo + dataset + benchmark | High reach; folded in as §7.1, §10, §15.3 |
| Private model-factory template | repo + workflows | Strong Union fit, but a template rather than a product; candidate future example |
| Local LLM gateway proxy | installable system | Crowded market; folded in as an internal component (§7.4 proxy) |
| Fine-tuned workflow-repair model | model + dataset | Best as a companion artifact; became `harness-lock/triage` (§11) |
| Standalone research-agent app | app/harness | Good demo but less differentiated; became an example agent + the first eval pack |

## Appendix B. References for the peripheral and skills survey (§5.4, §7.6, §7.7)

- Pydantic AI capabilities: https://pydantic.dev/docs/ai/capabilities/overview/
- Pydantic AI agent specs (`Agent.from_file` / `from_spec`): https://pydantic.dev/docs/ai/core-concepts/agent-spec/
- Pydantic AI Harness (capability library): https://pydantic.dev/docs/ai/harness/
- On-demand capabilities: https://pydantic.dev/articles/pydantic-ai-capabilities
- Agent Skills open standard: https://agentskills.io/ · specification: https://agentskills.io/specification
- `anthropics/skills`: https://github.com/anthropics/skills
- OpenAI skills catalog (system / curated / experimental): https://codex.danielvaughan.com/2026/05/14/openai-skills-catalogue-codex-cli-official-curated-experimental-skill-installer/
- Google skills repo: https://pinggy.io/blog/google_official_agent_skills_repository/
- skills.sh CLI lock files (`skills-lock.json`, `~/.agents/.skill-lock.json`): https://www.mintlify.com/vercel-labs/skills/advanced/lock-files
- Skill ecosystem study (registry → repository, ~57k skills): https://arxiv.org/pdf/2607.00911
- Skill-ecosystem security scanning: https://arxiv.org/pdf/2609.17274 · cross-skill composition evading scanners: https://arxiv.org/pdf/2608.09732
- OpenCode config (agents, commands, plugins, LSP, formatters, permissions): https://opencode.ai/v2/docs/config/
- Pi coding agent (extensions, skills, prompt templates, packages): https://app.unpkg.com/@mariozechner/pi-coding-agent@0.73.1/files/README.md
- Claude Agent SDK filesystem features (settingSources, CLAUDE.md, skills, hooks, plugins): https://code.claude.com/docs/en/agent-sdk/claude-code-features
- Claude Agent SDK system prompts and output styles: https://platform.claude.com/docs/en/agent-sdk/modifying-system-prompts
- Codex CLI MCP / config.toml: https://codex.danielvaughan.com/2026/03/26/codex-cli-mcp-integration/
- Cross-harness config comparison: https://notes.systemoverlord.com/Computers/AI/Agent-Configs
