# Decision models: typed judgments across hlock

Status: draft, not built. Extends `v0.md` §7.1 (graders), §7.3 (decision models) and §11
(open models), and `meta-harness.md`. Supersedes the `[decision]` table in `v0.md` §5.

## 1. Summary

A *decision model* answers a typed question about some state: is this true, which of these
options, where on this scale. It doesn't generate text. It returns calibrated
probabilities in one forward pass, at a small fraction of an LLM's cost and latency.
TypeSafe's Jev (September 2026) was the first with a public API. OpenAI, Cloudflare,
Perplexity and Respan followed, and Kev is an open-weight family that serves Jev's API
from Ollama or its own server.

hlock makes many judgments of this shape. Today it either asks an LLM to write JSON or
doesn't make them:

| Where | Judgment | Today |
|---|---|---|
| `judge:` graders | Does the output meet each criterion? Where does it sit on the rubric? | An LLM writes `{"score": …}` |
| `trace:` graders | Did the agent ask before a destructive action? | Not built |
| The stack's decisions | Approve this tool call? Is the task done? Which model next? | `[decision]` is a label; nothing runs |
| `evals init --from-traces` | Is this session a reusable task? What kind? | Every distinct request becomes a draft |
| `meta verify bootstrap-evals` | Does each prompt stand alone? Does the solution do the task? | Static checks only |

This spec makes decision models a first-class kind of locked component, called a
**decider**:

- one naming scheme and two wire protocols (§4);
- `[deciders.<use>]` in `harness.toml` and `[[decider]]` in `harness.lock`, keyed by
  *what the decider decides* (§6);
- per-harness support, from native settings to hooks hlock writes (§5, §7);
- deciders as the default judge for eval packs (§8), and in `hlock meta` (§9).

## 2. Goals and non-goals

### Goals

- Lock a decider like any other component: exact model version, endpoint, protocol, the
  questions hlock asks it, and where in the harness it's wired, with drift detection.
- Configure it once per use, in one place, whether it's fixed or searched by `resolve`.
- Reach every harness hlock supports, through the best mechanism each one offers.
- Make judges cheap enough to run on every trial and in `hlock evals check --run`.
- Keep a local, open-weight path (Kev on Ollama) for every use, so `localize` and offline
  runs work.
- Never let a decider widen what an agent may do unless the user says so in the manifest,
  where it shows up in review.

### Non-goals

- Training deciders (`v0.md` §7.3's tuning stage; §12 says how it builds on this).
- Replacing LLM judges for free-form rubrics, long reasoning, or images on text-only
  models. They become the fallback.
- Wrapping community plugins (jev-guard, pi-verdict, …). `hlock pin` already records them
  as plugins and config; they're opaque to hlock, which is fine.

## 3. Research: who supports decision models (October 2026)

Checked 2026-10-18 against official docs, changelogs, release notes and package source
(**V**), or third-party coverage only (**R**). Keys and versions marked R must be confirmed
before they go into a profile.

### 3.1 Agent CLIs (the harnesses hlock profiles)

| Harness | Native decider support | Extension points a decider can use |
|---|---|---|
| Claude Code | None. `CLAUDE_CODE_AUTO_MODE_MODEL` (2.1.302) picks the Claude model for the auto-mode classifier, not a decision model (V). | Hooks with `command` and `http` handlers. `PreToolUse` returns `permissionDecision`: `allow`, `deny`, `ask` or `defer`. `PermissionRequest` returns `decision.behavior`: `allow` or `deny`. `Stop` returns `decision: "block"` to keep going. Registered in `settings.json` `hooks` (V, confirmed first-hand). |
| Codex CLI | None user-facing. A Decisions transport for its Guardian risk classifier is test-only (PR #50066) (V). | `hooks.json` or `[hooks]` in `config.toml`; `command` and `mcp_tool` handlers run. `PreToolUse`, `PermissionRequest` (same output shape as Claude Code), `Stop` (V). |
| OpenCode | None in the agent. OpenCode Zen *serves* Jev at a `/v1/systemone` endpoint (V). | Plugins (`.opencode/plugins/`): `tool.execute.before` can block; `permission.asked` events (V); a permission `evaluate` hook in 2.0.12+ (R). |
| Pi | **Native model type** (V). Classifier models since 0.99.0 (`ModelRuntime.classify()`, built-in `jev-latest`); OpenAI `gpt-6-luna` in 1.1.0; local Kev and other `/v1/systemone` servers in 1.3.0 (API type `typesafe-system-one`). They're registered in `models.json` with the other providers, but no built-in harness decision uses them. | Extensions (`~/.pi/agent/extensions/`, project dirs): `tool_call` returns `{block, reason}`; `ctx.modelRegistry.classify()` reaches the registered classifiers (V). |
| Cursor CLI | None. Auto-review uses an LLM classifier subagent (V). | `hooks.json`: `beforeShellExecution` returns `permission` `allow`, `deny` or `ask`; the CLI reportedly fires only the shell hooks (R). |
| Copilot CLI | None (V). | `.github/hooks/*.json`: `preToolUse` can deny with a reason (V). |
| Gemini CLI | None; its Flash-or-Pro router is an LLM (V). | `settings.json` hooks: `BeforeTool` and `AfterAgent` return `decision` `allow` or `deny`; `BeforeToolSelection` narrows tools (V). |
| Amp | **Internal** (V). Uses Jev for diff ordering and thread labels; plugins get `amp.ai.noul`, `choice`, `score` with the model named per call (`typesafe/jev`, `openai/gpt-6-luna`). No settings key picks the model. | `amp.permissions` with a `delegate` action that runs a program (exit 0 allows, 1 asks, 2+ rejects) (V/R). |
| Goose | **Partial** (V). v1.52.0 added a `DecisionProvider` crate (Jev, OpenRouter) with nothing wired to it and no config key. `smart_approve` still uses the LLM. | MCP extensions; `GOOSE_MODE` (R). |
| Hermes | **A native slot** (V). `auxiliary.approval` in `config.yaml` (with `approvals.mode: smart`) names the model that reviews approvals; the community `jev-approvals` plugin points it at Jev. | Plugins: `pre_tool_call` and other blocking hooks (V). |

So: two harnesses have settings a decider lives in (Pi's model registry, Hermes's approval
slot), one uses deciders internally with no setting (Amp), and the rest reach them only
through hooks. Every harness except Goose has a permission or tool-call hook that can
block, and Claude Code and Codex share the same `PermissionRequest` output.

### 3.2 Frameworks (code harnesses)

| Framework | Native support | How it's configured |
|---|---|---|
| PydanticAI | **Yes** (V, 2.57.0) | Model ids `system-one:<model>` (confirmed first-hand), `typesafe:<model>`, `openai-decisions:gpt-6-luna`; `SYSTEM_ONE_BASE_URL`, `SYSTEM_ONE_API_KEY`. `bool`/`Literal`/int output fields become noul/choice/score; falls back to an LLM when unsure. |
| LangChain (Py, JS) | **Yes** (V) | `langchain-typesafe` / `@langchain/typesafe` (`TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL`), with model-router and auto-mode middleware; `langchain-openai` ships `OpenAIDecisions`. |
| Vercel AI SDK | **Experimental** (V) | `experimental_decide({ model: openai.decisionModel('gpt-6-luna'), … })`; `@ai-sdk/typesafe-ai`. |
| DSPy | **Experimental** (V, 3.4.1) | `dspy.experimental.TypeSafe(model=…)`; `TYPESAFE_*` variables. |
| Google ADK | On `main`, unreleased (V) | `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL`. |
| LangGraph, deepagents, Claude Agent SDK, OpenAI Agents SDK, CrewAI, smolagents, LlamaIndex | None (V, package source searched) | Call one from a node, tool, guardrail or hook. |

Two env-var conventions cover every framework that has support: `SYSTEM_ONE_*` and
`TYPESAFE_*`, both meaning "a `/v1/systemone` endpoint and its key".

### 3.3 Serving and gateways

| Where | Protocol | Notes |
|---|---|---|
| TypeSafe | System One, `POST /v1/systemone` | `jev-1.13.0`, `jev-1.14.0`; aliases `jev-latest`, `jev-preview`. $0.042/1M input tokens, output free. `TYPESAFE_API_KEY`. The response names the exact model. |
| OpenAI | Decisions, `POST /v1/decisions` | `gpt-6-luna`, beta. Questions are an *array* (`predicate`, `choice`, `score`), input is `input` (text and up to 128 images), answers may be `refusal`. Not System One-compatible. |
| Ollama ≥ 0.40.4 | System One at `localhost:11434/v1/systemone` | `ollama pull kev`: 0.8B, 4B, 9B, 27B (confirmed first-hand). Also serves Clef. |
| Ollaya 0.13 | System One and Decisions at `localhost:11435` | A runtime built only for decision models, Ollama-style (§3.4). |
| Kev's own server | System One | `python -m kev.serve --run jaredpalmer/kev-4b --port 8009`; packs many questions into one forward pass. |
| SGLang 0.5.22, vLLM `main` | System One and Decisions | Serve Clef natively; vLLM reads other models through logprobs. |
| Cloudflare Workers AI | System One body | `@cf/cloudflare/clef`, `clef-flash`; the REST API wraps the response in `result`. |
| OpenRouter | Both: `/api/v1/systemone` passthrough and `/api/alpha/decisions` | Slugs such as `typesafe/jev-1.14`, `jaredpalmer/kev-4b`, `cloudflare/clef`, `respan/span-02`, `perplexity/pplx-decider-v1.2`. |
| LiteLLM proxy, Portkey | System One–style `/v1/systemone` or `/v1/decisions` | Route to the providers above. |

Two protocols cover everything: **System One** (TypeSafe's shape) and **Decisions**
(OpenAI's). Everything else speaks one of them, usually System One.

### 3.4 Ollaya

[Ollaya](https://ollaya.dev) ([source](https://github.com/ollaya-dev/ollaya), Apache-2.0)
runs open decision models locally the way Ollama runs LLMs (V, from its site, README and
releases):

- **Interface:** one binary with a daemon on `127.0.0.1:11435` and Ollama's command set
  (`pull`, `run`, `list`, `ps`, `show`, `rm`, `cp`, `stop`, `create`).
- **APIs:** `POST /v1/systemone`, `/v1/decisions` and `GET /v1/models`, plus a native
  `/api/*`. Existing Jev clients work by changing the base URL.
- **Models:** more than 20 families, pulled from their authors' Hugging Face repos: Kev,
  Clef, Laya, `decider`, `winnow`, `decima`, `arbiter`, `qwen3guard` and others. Some are
  vision models.
- **Modelfiles:** these can bundle a question set (`QUESTIONS ./triage.json`).
- **Configuration:** `OLLAYA_HOST`, `OLLAYA_MODELS`, `OLLAYA_API_KEY`. Pulls are
  digest-based since 0.13.0.
- **Platforms:** macOS (MLX), Linux and Windows (CUDA, Vulkan), and Docker.
- **Speed:** one third-party benchmark measured 27–28 ms for a warm, five-question request.

It's young: first released in September 2026, with releases every one to three days, a
single maintainer, and features that need a minimum version (0.12.0, 0.13.0).

**Verdict: support it as a runtime, not as the default.** It earns a place because:

- it serves model families Ollama doesn't;
- it speaks both protocols, so it can stand in for OpenAI's Decisions API locally;
- its runtime shape (binary, port, pull command, model directory) fits hlock's runtime
  profiles exactly.

Ollama stays first in the default order (§8.2). It's more established, and many users
already run it.

### 3.5 Eval tools

LangSmith, Langfuse, promptfoo and DeepEval offer decision models as judges (V). Inspect AI,
OpenAI Evals graders, autoevals and openevals don't (V). That's precedent for §8: eval
tools already treat "a decision model as the judge" as a normal configuration.

## 4. Naming and protocols

### 4.1 Model references

A decider is `provider:model`, like an LLM, with providers that name the protocol. The
prefixes follow PydanticAI's where they overlap, so the same string works in a pack and in
a PydanticAI agent:

| Reference | Protocol | Endpoint | Key |
|---|---|---|---|
| `typesafe:jev-1.13.0` | System One | `https://api.typesafe.ai` | `TYPESAFE_API_KEY` |
| `openai-decisions:gpt-6-luna` | Decisions | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `ollama:kev:4b` | System One | the Ollama runtime (`localhost:11434`) | none |
| `ollaya:kev:4b`, `ollaya:winnow:e4b` | System One (Decisions also served) | the Ollaya runtime (`localhost:11435`, `OLLAYA_HOST`) | `OLLAYA_API_KEY`, if set |
| `openrouter:typesafe/jev-1.14` | System One (passthrough) | `https://openrouter.ai/api` | `OPENROUTER_API_KEY` |
| `cloudflare:clef` | System One (unwrapped from `result`) | needs `base-url` with the account | `CLOUDFLARE_API_TOKEN` |
| `system-one:<model>` | System One | `base-url` (required): Kev's server, SGLang, vLLM, LiteLLM, Portkey | `key-env`, if any |
| `rules:<set>` | none; hlock's rule sets | | |

`openai:` stays the LLM provider. A table that takes only deciders (§6) still requires
`openai-decisions:`, because judge settings accept both kinds and the same string must
mean the same thing everywhere.

Aliases (`jev-latest`) are accepted in manifests. Locks, traces and grades record the
model the response names (`jev-1.13.0`). `hlock evals check` warns about an alias in a
pack.

### 4.2 The client

A Rust module, `decide.rs`, beside `agent.rs`, with one interface over both protocols:

```rust
pub enum Kind { Noul, Choice, Score }
pub struct Question { id: String, kind: Kind, instructions: Value, criteria: Value }
pub struct Answer { id: String, value: Value, probabilities: BTreeMap<String, f64>, confidence: Option<f64>, refused: bool }
pub struct Decided { model: String, answers: Vec<Answer>, input_tokens: u64, cost_usd: f64 }

pub fn decide(cfg: &DeciderConfig, state: &Value, questions: &[Question]) -> Result<Decided>;
```

It maps `noul` to Decisions' `predicate` and keyed questions to its array. It retries 429
and 529, splits batches over a provider's limits (TypeSafe: 32k tokens a question; Clef:
64 questions), unwraps Cloudflare's `result`, and prices input tokens from a table. Keys
resolve through the secret sources (`secrets.md` §2).

## 5. What a decider decides: uses

A decider is locked for a **use**, the decision it makes. Uses that run inside the agent:

| Use | Question hlock asks | Type | Answers |
|---|---|---|---|
| `approve` | Given the project's policy, should this tool call run, need the user, or be refused? | `choice` | the options in `may` (§6.2) |
| `stop` | Has the task in the first message been done? | `noul` | continue when P < threshold |
| `route` | Which model role should handle the next step? | `choice` | the lock's LLM roles |
| `tool` | Which tools fit the next step? | `noul` per tool | tools to offer |

Uses hlock itself runs, never wired into a harness:

| Use | Used by |
|---|---|
| `default` | Anything below that isn't set |
| `judge` | Judge and trace graders (§8). `[evals.judge]` and `[pack.judge]` override it and may name an LLM. |
| `draft` | `evals init --from-traces` (§8.6) |

The questions are hlock's, versioned with hlock, and their digest is locked, so changing
a question shows up as drift. A project adds its own rules to `approve` and `stop` with a
`policy` file (§6.2), which goes into the question's instructions.

## 6. Configuration

### 6.1 Design iterations

**1. A `decision` role in `[[llm]]`.** One model list, and `localize`, `endpoint` and
`runtime` come for free. Rejected: pinning writes `[[llm]]` roles back into the harness's
model settings, which would hand a decision model to Claude Code as its chat model; the
protocols differ; and an LLM role doesn't say where in the harness the model is wired.

**2. Grow the `[decision]` table.** It's where the spec put the decision model, with
`decisions = ["retry", "stop"]`. Rejected: it holds one model, but uses need different
ones (a 0.8B Kev on every tool call, a 9B judge), and different hooks, policies and
thresholds.

**3. One entry per use (chosen).** `[deciders.<use>]` in the manifest and `[[decider]]`
entries in the lock, each saying which model, how it's reached, where it's wired, and what
it may decide. A use is fixed with `model` or searched with `any-of`, in the same table,
so there's one place to look. `[decision]` and `[candidates] decision` keep loading
(§6.5).

### 6.2 `harness.toml`

```toml
[deciders.default]                      # hlock's own judgments: judges, drafting
model = "typesafe:jev-1.13.0"

[deciders.approve]                      # the agent's permission prompts
any-of = ["none", "ollama:kev:4b", "typesafe:jev-1.13.0"]   # resolve picks one
may = ["deny", "ask"]                   # what it may decide; default: deny and ask only
policy = ".hlock/approve.md"            # the project's rules, in its own words
min-probability = { deny = 0.8, ask = 0.5 }

[deciders.stop]
model = "ollama:kev:0.8b"
min-probability = 0.9                   # P(done) needed to let the agent stop
```

| Key | Description |
|---|---|
| `model` | A reference (§4.1), `none`, or `rules:<set>` |
| `any-of` | Candidates for `resolve` instead of `model` |
| `base-url`, `key-env`, `protocol` | Override the provider's defaults |
| `may` | `approve` only: which of `allow`, `ask`, `deny` it may answer. Default `["deny", "ask"]`. Adding `allow` lets it approve calls for the user, and `hlock check` prints that as a notice. |
| `policy` | A file of the project's rules, added to the question; its digest is locked |
| `min-probability` | How sure it must be to act, overall or per answer. Below it, the decider defers, and the harness does what it would have done. |
| `timeout-ms` | How long a hook waits (default 2000) before deferring |
| `hooks` | Override where it's wired, e.g. `["PreToolUse"]` instead of the profile's default |

Defaults keep a decider safe to add: it can only make the agent more careful, it defers
whenever it's unsure, slow or unreachable, and it never approves unless `may` says so.

### 6.3 `harness.lock`

```toml
[[decider]]
use = "approve"
provider = "ollama"
model = "kev:4b"
protocol = "systemone"
runtime = "ollama"
digest = "sha256:9a1f…"                 # the weights, as the runtime reports them
resolved-from = "harness.toml#/deciders/approve/any-of/1"
via = "hook"                            # native | hook | extension | env
wiring = [{ file = "settings", event = "PermissionRequest" }, { file = "settings", event = "PreToolUse" }]
may = ["deny", "ask"]
min-probability = { deny = 0.8, ask = 0.5 }
questions = "sha256:41c0…"              # hlock's question set for this use
policy = { path = ".hlock/approve.md", digest = "sha256:77e2…" }

[[decider]]
use = "stop"
provider = "typesafe"
model = "jev-1.13.0"                    # what answered; the manifest may say jev-latest
protocol = "systemone"
endpoint = "https://api.typesafe.ai"
via = "hook"
wiring = [{ file = "settings", event = "Stop" }]
min-probability = 0.9
questions = "sha256:0d93…"

[[secret]]
name = "TYPESAFE_API_KEY"               # listed like any other secret the lock needs
```

A judge isn't part of the stack the harness runs. It's how the evidence was produced, so
it goes in the evidence:

```toml
[evidence.judge]
provider = "typesafe"
model = "jev-1.13.0"
protocol = "systemone"
questions = "sha256:…"                  # the packs' criteria and levels, as asked
uncertain-grades = 0.04                 # share of judge grades near the line (§8.3)
```

`[evidence] judge = "…"`, the string `resolve` writes today, still loads.

### 6.4 Pinned locks

`hlock pin` records the deciders a harness already has, through the profile (§7):

| Found | Lock entry |
|---|---|
| Hermes `auxiliary.approval` naming a model | `use = "approve"`, `via = "native"`, `resolved-from = "config#/auxiliary/approval"` |
| Pi classifier models in `models.json` | `use = "any"` (available to extensions and tools), `via = "native"` |
| Hooks hlock wrote (`hlock decide hook …` commands) | The entry they were written from, unchanged |
| A community plugin that calls Jev | Nothing new: it's pinned as a plugin and config already |

`sync` restores them; `check` reports a changed model, policy, question set or missing
hook as drift.

### 6.5 Compatibility

- `[candidates] decision = ["none", "rules:default"]` loads as `[deciders.default]
  any-of`.
- A lock's `[decision]` table loads as one `[[decider]]` per entry in its `decisions`,
  with `via = "simulated"` for the reference harness. New locks write `[[decider]]` only.
- Locks without deciders are unchanged.

## 7. Wiring deciders into harnesses

### 7.1 Profiles

Harness profiles gain a `[deciders]` section saying how each use is reached:

```toml title="profiles/harnesses/claude-code.toml"
[deciders.approve]
hook = { file = "settings", events = ["PermissionRequest", "PreToolUse"], format = "claude-code" }
[deciders.stop]
hook = { file = "settings", events = ["Stop"], format = "claude-code" }
```

```toml title="profiles/harnesses/hermes.toml"
[deciders.approve]
native = { file = "config", pointer = "/auxiliary/approval", keys = { provider = "provider", model = "model", base-url = "base_url" } }
```

```toml title="profiles/harnesses/pydantic-ai.toml"
[deciders]
env = { systemone = ["SYSTEM_ONE_BASE_URL", "SYSTEM_ONE_API_KEY"], model = "HLOCK_DECIDER_{USE}" }
```

### 7.2 The hook command

For `via = "hook"`, `sync` and `launch` write a hook whose command is
`hlock decide hook <use> --format <format>`. It reads the harness's event from stdin, asks
the locked decider, and prints the harness's own answer format. It defers (prints nothing
or the format's no-decision value) on error, timeout, or an answer below
`min-probability`. hlock already writes into each harness's config files at `sync`, with
backups and in the isolated root, so a hook is one more pointer it owns.

### 7.3 Support matrix

| Harness | `approve` | `stop` | `route` | Mechanism |
|---|---|---|---|---|
| Claude Code | yes | yes | no | hook: `PermissionRequest` (allow, deny), `PreToolUse` (ask, deny, defer), `Stop` |
| Codex | yes | yes | no | hook, same output as Claude Code |
| Gemini CLI | deny only | yes | tool subset | hook: `BeforeTool`, `AfterAgent`, `BeforeToolSelection` |
| Cursor CLI | shell commands only | no | no | hook: `beforeShellExecution` |
| Copilot CLI | deny only | no | no | hook: `preToolUse` |
| OpenCode | yes | no | no | a plugin file hlock writes (`tool.execute.before`; the permission hook once confirmed) |
| Pi | yes | no | no | `via = "native"` for registered classifiers; an extension hlock writes for `approve` (`tool_call`) |
| Amp | yes | no | no | `amp.permissions` `delegate` to the hook command (exit codes) |
| Hermes | yes | no | no | native: `auxiliary.approval` |
| Goose | no | no | no | none until its decision provider is wired; `resolve` won't propose deciders |
| Code harnesses | any | any | any | env: `HLOCK_DECIDER_<USE>` plus the framework's own variables (§7.4) |
| Reference | simulated | simulated | simulated | the offline harness simulates each use |

`resolve` proposes candidates only for uses the target harness supports, as `v0.md` §7.3
already requires. Profiles add a use as each harness grows a mechanism, so the matrix is
data, not code.

### 7.4 Code harnesses

The contract (`contract.rs`) passes each locked decider as `HLOCK_DECIDER_<USE>` (the
reference), `HLOCK_DECIDER_<USE>_BASE_URL` and `_PROTOCOL`. For a System One decider, it
also sets `SYSTEM_ONE_BASE_URL`/`SYSTEM_ONE_API_KEY` and `TYPESAFE_BASE_URL`/
`TYPESAFE_API_KEY` when the profile lists them, so PydanticAI, LangChain, DSPy and ADK
pick it up with no code. The Python shim adds `hlock.decide(use, state)` for frameworks
without support.

### 7.5 Why not a proxy

hlock could serve one local endpoint and route each use behind it. Rejected for now: it
adds a long-running process to `run` and `launch`, and every harness above can reach a
decider directly or through a short-lived hook command. Revisit if hook latency matters.

## 8. Eval packs

### 8.1 Judges ask decider-shaped questions

Judge graders take `criteria` (a list) or `rubric` (a file) today. This adds `levels`:

| Grader | A decider is asked | Score | Passes when |
|---|---|---|---|
| `criteria = [...]` | One `noul` per criterion, in one call | Mean P(true) | score ≥ `pass` (0.7) |
| `levels = [...]` (2–10, worst first) | One `score` | `score / (n − 1)` | score ≥ `pass` |
| `rubric = "file.md"` | A bullet list in the file becomes `criteria`; a `## Levels` list becomes `levels` | as above | as above |

```toml
graders = [
  { type = "judge", criteria = [
      "States Q3 revenue and its change from Q2",
      { text = "Cites the source", true = "Names the document each claim came from", false = "Makes claims with no source" },
  ] },
  { type = "judge", artifacts = ["reply.md"], levels = [
      "Doesn't address the customer's problem",
      "Addresses it, with wrong or unsafe steps",
      "Correct steps, but no link to the help article",
      "Correct steps and the right help article",
  ], pass = 0.66 },
]
```

The state is `{"task": <prompt>, "files": {<path>: <text>}}`. A free-form rubric with no
list or levels needs an LLM judge, and `hlock evals check` says so when the judge is a
decider.

A checklist is what deciders do best: narrow yes/no questions, answered in parallel, with
calibrated probabilities, at about $0.0001 a task. That makes judges affordable on every
trial of every seed, and in `check --run`.

### 8.2 Which judge

The first that's set: `[evals.judge]`, `[pack.judge]`, `[deciders.judge]`,
`[deciders.default]`. When none is, hlock uses the first that's available and records it
in the evidence:

1. `ollama:kev:4b`, if Ollama is running with Kev pulled;
2. `ollaya:kev:4b`, if Ollaya is running with Kev pulled;
3. `typesafe:jev-1.13.0`, if `TYPESAFE_API_KEY` resolves;
4. none: judge graders fail with "no judge model", as today, naming both options.

Templates write `[pack.judge] model = "typesafe:jev-1.13.0"` (a version, not an alias)
instead of `anthropic:claude-sonnet-5-5`, and their `rubric.md` stubs become criteria
lists, which either kind of judge reads.

### 8.3 Uncertainty and fallback

```toml
[evals.judge]
model = "typesafe:jev-1.13.0"
fallback = "anthropic:claude-sonnet-5-5"  # re-judges uncertain grades
uncertain = 0.15                           # within 0.15 of `pass`, or confidence below 0.5
```

With a `fallback`, an uncertain grade is re-judged by the LLM, whose verdict counts; both
are recorded. Without one, it stands, marked `uncertain`, and `[evidence.judge]
uncertain-grades` records the share.

### 8.4 Checking and calibrating judges

`hlock evals check --run` runs decider judges on the fixtures alone and on the solution,
with the deterministic graders. LLM judges need `--judges`. `hlock evals grade` prints
each criterion's probability:

```console
$ hlock evals grade summarize_q3
summarize_q3  its solution · score 0.94
  ✓ artifact:summary.md  1.00
  ✓ judge:3 criteria (typesafe:jev-1.13.0)  0.94
      0.99  States Q3 revenue and its change from Q2
      0.97  Names the report it summarizes
      0.86  Is under 200 words
```

`hlock evals calibrate [--against MODEL]` judges each solution and fixtures-only run with
the decider and an LLM, and lists the criteria they disagree on: the ones to reword or give
`true`/`false` descriptions.

### 8.5 Trace graders

`trace:` graders (planned in `v0.md` §7.1) ask a decider about the trial's tool calls:

```toml
graders = [{ type = "trace", criteria = ["Asked the user before deleting files", "Never ran sudo"], pass = 1.0 }]
```

Exact checks (a regex over commands) belong in a `tests` grader reading `$HLOCK_TRACE`.

### 8.6 Drafting from traces

With a `draft` (or `default`) decider, `evals init --from-traces` asks about each request:
does it stand alone (`noul`), is the result checkable (`noul`), which kind of work is it
(`choice` over the template kinds, picking `check.py` or `rubric.md`), and does it contain
a secret or personal data (`noul`). The output names the model that read the prompts, since
a hosted one means they left the machine.

## 9. `hlock meta`

`harness.meta.toml` takes `[deciders.default]` like `harness.toml`; `meta init` offers it
when Ollama has Kev or `TYPESAFE_API_KEY` is set, and `meta pin` locks it.

| Job | The decider | Effect |
|---|---|---|
| `bootstrap-evals` (verify) | Per task: does the prompt stand alone; do the graders check what the prompt asks; does the solution do the task | Warnings, never errors |
| `bootstrap-evals` (run) | Drafts from traces as in §8.6 | Fewer, better drafts |
| `install` | Classifies each failed step: missing tool, network, permissions, needs the user | Says whose move it is |
| `update` | Classifies changelog entries: breaking, feature, fix | Marks risky updates before `--pin` |

The built-in agent gets `stop` (one more turn, at most twice, when P(done) is low) and a
risk note on interactive approval prompts. A decider never allows a command for the
built-in agent: only `[agent] allow` and the user do.

## 10. Local runtimes

Deciders served locally use hlock's runtime profiles (`profiles/runtimes/`), like local
LLMs:

- **Ollama:** its profile gains catalog entries for Kev and Clef with `role = "decider"`,
  so `localize` and §8.2 can pick a size.
- **Ollaya:** a new profile with binary `ollaya`, port `11435`, `pull = "ollaya pull
  {model}"`, `serve = "ollaya serve"`, models under `OLLAYA_MODELS`, install by script or
  Docker, and its own decider catalog. The weights digest comes from `/api/tags` (to
  confirm), as it does for Ollama.
- **Pulling:** `hlock sync --pull` pulls the deciders a lock names through their runtime,
  and `hlock runtime list` shows both.
- **Versions:** a lock records the runtime version alongside the model digest. Ollaya
  models can need a minimum Ollaya version, and `sync` checks it.

## 11. Commands

| Command | Change |
|---|---|
| `hlock decide STATE --noul "…" [--choice …] [--score …]` | New: ask a decider one-off questions, to try criteria or policies. `--model`, `--use`, `--json`. |
| `hlock decide hook USE --format F` | New: the hook entry point (§7.2) |
| `hlock evals check --run`, `evals grade`, `evals calibrate` | §8.4 |
| `hlock pin`, `sync`, `launch`, `check` | Capture, wire, and check deciders (§6.4, §7.2) |
| `hlock resolve`, `show` | Search `any-of`; show each decider, its cost, and uncertain grades |
| `hlock localize` | Map hosted System One deciders to `ollama:kev:<size>`, with a note that scores may move |
| `hlock upgrade` | `jev-1.13.0 → jev-1.14.0` as an upgrade with evidence |

Python: `hl.decide(state, questions, model=None) -> str` (JSON).

## 12. Rollout

1. `decide.rs` (System One), judges on deciders (§8.1–8.2), templates, `check --run` and
   `grade`, `[evidence.judge]`. Most of the value; no harness changes.
2. `[deciders.*]` and `[[decider]]` with compatibility (§6), `hlock decide`, the Decisions
   protocol.
3. Uncertainty and fallback, `evals calibrate`.
4. Hooks: `hlock decide hook`, Claude Code and Codex `approve` and `stop` first (one output
   format covers both), then Gemini, Cursor, Copilot, Amp, OpenCode, Pi.
5. Native capture: Hermes `auxiliary.approval`, Pi classifiers; code-harness env (§7.4).
6. `resolve` over `any-of`; `localize` and `upgrade`.
7. Meta (§9), trace graders, drafting.
8. Tuning (`v0.md` §7.3): LoRA on Kev, whose training code is open, labeled by outcomes and
   by fallback verdicts.

## 13. Tests

- `decide.rs` against fake System One and Decisions servers: mapping, batching and
  splitting, retries, refusals, Cloudflare's wrapper, the answered model id, pricing.
- Judges: criteria, levels, rubric parsing, thresholds, `uncertain`, fallback to a fake LLM;
  the judge order in §8.2.
- Manifest and lock: `[deciders.*]` with `model` and `any-of`; `[[decider]]` round trip;
  old `[decision]` and `[candidates] decision` loading; drift on model, policy, questions,
  and a removed hook.
- `hlock decide hook`: each format's input and output from recorded events; defer on
  timeout, error, and low probability; never `allow` unless `may` has it.
- Pin: Hermes and Pi fixtures produce `via = "native"` entries.

## 14. Open questions

1. **Approval as a choice or two nouls.** One `choice` over `may` reads naturally; two
   nouls ("needs the user?", "harmful?") may calibrate better. Measure on recorded
   permission prompts.
2. **Hook latency.** A hosted decider adds 70–500 ms to each gated tool call. Should
   `approve` default to `PermissionRequest` only (prompts the user would see anyway), not
   `PreToolUse`?
3. **Score semantics.** `score / (n − 1)` assumes evenly spaced levels. Allow
   `levels = [{text, value}]`?
4. **Images.** Jev is text-only; Decisions, Clef and Span take images. Support them per
   provider for chart tasks?
5. **Local-only packs.** Should a pack be able to require a local judge
   (`[pack.judge] local = true`), so outputs never leave the machine?
6. **Ollaya's churn.** At releases every few days, should its profile pin a minimum
   version, and should `check` warn when the installed Ollaya is older than the one a lock
   was resolved with?
7. **Modelfile question presets.** Ollaya can bundle a question set into a named model.
   Should hlock ever write one (e.g. a project's `approve` policy), or keep the questions
   in hlock where their digest is locked?
8. **OpenCode's permission hook.** Confirm the `evaluate` hook (reported in 2.0.12+) before
   relying on it instead of `tool.execute.before`.
