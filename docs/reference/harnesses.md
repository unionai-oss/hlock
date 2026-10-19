---
description: The agent harnesses, agent frameworks, and local inference runtimes hlock supports, what it captures from each, and how to add one.
---

# Supported harnesses

hlock supports agent CLIs, autonomous agents, agent frameworks, and local inference
runtimes. Each one is
described by a [profile](../guides/harness-profiles.md), a TOML file that tells hlock where
the harness keeps its config, model, MCP servers, skills and plugins, and how it was
installed. These tables are generated from the built-in profiles, so they match the
version of hlock these docs are for. `hlock harness list` shows the same harnesses, and which
of them are installed on your machine.

!!! note "Trials don't drive agent CLIs yet"

    `pin`, `sync`, `launch`, `run` and `localize` work with the agent CLIs below. During
    `hlock resolve`, though, trials for an agent CLI run on the backend's reference adapter,
    so the lock's evidence is a stand-in. Your own agents, built on a
    [framework](#agent-frameworks) or written from scratch, run for real in trials. See
    [Harnesses](../guides/harnesses.md#resolve-for-a-target).

## Agent CLIs

<!-- generated:clis -->
| Harness | Vendor | Name in hlock | Support | Open source | Installs from |
|---|---|---|---|---|---|
| [Claude Code](https://code.claude.com) | Anthropic | `claude-code` (`claude`) | full | — | `brew-cask`, `npm`, `script` |
| [Codex CLI](https://developers.openai.com/codex/cli) | OpenAI | `codex` (`codex-cli`, `openai-codex`) | full | ✓ | `npm`, `brew-cask` |
| [Pi](https://pi.dev) | — | `pi` (`pi-coding-agent`, `pi-agent`) | full | ✓ | `npm` |
| [OpenCode](https://opencode.ai) | — | `opencode` (`open-code`, `sst-opencode`) | full | ✓ | `brew`, `npm`, `script` |
| [Cursor Agent](https://cursor.com/cli) | Anysphere | `cursor-agent` (`cursor`, `cursor-cli`) | full | — | `script` |
| [Gemini CLI](https://github.com/google-gemini/gemini-cli) | Google | `gemini-cli` (`gemini`) | basic | ✓ | `npm`, `brew` |
| [GitHub Copilot CLI](https://github.com/github/copilot-cli) | GitHub | `copilot-cli` (`copilot`, `github-copilot`) | basic | — | `npm`, `brew` |
| [Goose](https://block.github.io/goose) | Block | `goose` (`block-goose`) | basic | ✓ | `brew`, `script` |
| [Amp](https://ampcode.com) | Sourcegraph | `amp` (`ampcode`, `sourcegraph-amp`) | basic | — | `npm`, `script` |
<!-- /generated:clis -->

**Name in hlock** is what you pass to `--harness` or put in `[target] harness`. Any alias
in parentheses works too. **Installs from** lists the install methods hlock recognizes, most
preferred first. A lock records which one the installed copy came from, and
`hlock sync --install` uses it.

**Support** says how complete and well-tested the profile is:

- `full`: everything `pin`, `sync` and `launch` read is described and maintained.
- `basic`: detection, version, MCP servers and skills are covered. The rest of the profile
  may be partial or less tested, so reports and fixes are welcome.

### What hlock captures

<!-- generated:capture -->
| Harness | Model | Providers | MCP servers | Skills | Plugins | Instructions | `localize` |
|---|---|---|---|---|---|---|---|
| Claude Code | ✓ | — | ✓ | ✓ | plugins | `CLAUDE.md`, `.claude/CLAUDE.md` | with `--harness` |
| Codex CLI | ✓ | ✓ | ✓ | ✓ | — | `AGENTS.md`, `AGENTS.override.md` | ✓ |
| Pi | ✓ | ✓ | ✓ | ✓ | packages | `AGENTS.md`, `CLAUDE.md` | ✓ |
| OpenCode | ✓ | ✓ | ✓ | ✓ | plugins | `AGENTS.md`, `CLAUDE.md` | ✓ |
| Cursor Agent | ✓ | — | ✓ | ✓ | — | `AGENTS.md`, `CLAUDE.md`, `.cursorrules` | — |
| Gemini CLI | ✓ | — | ✓ | ✓ | — | `GEMINI.md`, `AGENTS.md` | — |
| GitHub Copilot CLI | ✓ | — | ✓ | ✓ | — | `AGENTS.md`, `.github/copilot-instructions.md` | — |
| Goose | ✓ | — | ✓ | ✓ | — | `.goosehints`, `AGENTS.md` | ✓ |
| Amp | — | — | ✓ | ✓ | — | `AGENTS.md` | — |
<!-- /generated:capture -->

- **Model**: the configured model, which `pin` records and `launch` passes to the harness.
- **Providers**: custom model providers and their base URLs, from the harness's config.
- **MCP servers** and **Skills**: pinned, synced, and written into the harness's config by
  `launch`.
- **Plugins**: plugins or packages installed into the harness, pinned with their versions.
- **Instructions**: the project instruction files the harness reads. Its config root
  usually holds a global one as well.
- **`localize`**: whether [`hlock localize`](../guides/localize.md) can move a lock onto
  this harness with local models. ✓ means it's open source and can use a local endpoint.
  *with `--harness`* means it can use a local model but isn't open source, so `localize`
  uses it only when you ask for it. *by hand* means `localize` can move a lock onto it, but
  doesn't know the harness's settings for a local endpoint, so it tells you what to set.

Every agent CLI also runs one prompt non-interactively with
[`hlock run`](cli.md#run).

## Autonomous agents

Autonomous agents run on their own instead of in a session you drive. Each one is a
long-running gateway that you message from chat apps (Telegram, Slack, WhatsApp, …). It wakes
itself on heartbeats and schedules, and keeps a workspace of persona files, skills, and
memory it writes as it goes. To hlock they're agent CLIs, so `pin`, `check`, `sync`,
`launch`, `run` and `localize` work the same way. `hlock harness list` lists them in their
own group.

<!-- generated:autonomous -->
| Harness | Vendor | Name in hlock | Support | Open source | Installs from |
|---|---|---|---|---|---|
| [Hermes Agent](https://github.com/NousResearch/hermes-agent) | Nous Research | `hermes` (`hermes-agent`, `nous-hermes`) | full | ✓ | `script` |
| [OpenClaw](https://openclaw.ai) | — | `openclaw` (`clawdbot`, `moltbot`) | basic | ✓ | `npm`, `script` |
| [nanobot](https://github.com/HKUDS/nanobot) | HKUDS | `nanobot` (`nanobot-ai`, `hkuds-nanobot`) | basic | ✓ | `uv-tool`, `pip`, `script` |
| [PicoClaw](https://github.com/sipeed/picoclaw) | Sipeed | `picoclaw` (`pico-claw`, `sipeed-picoclaw`) | basic | ✓ | `manual` |
| [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) | ZeroClaw Labs | `zeroclaw` (`zero-claw`) | basic | ✓ | `script`, `brew` |
| [IronClaw](https://github.com/nearai/ironclaw) | NEAR AI | `ironclaw` (`iron-claw`, `nearai-ironclaw`) | basic | ✓ | `manual` |
<!-- /generated:autonomous -->

<!-- generated:autonomous-capture -->
| Harness | Model | Providers | MCP servers | Skills | Plugins | Instructions | `localize` |
|---|---|---|---|---|---|---|---|
| Hermes Agent | ✓ | — | ✓ | ✓ | — | `AGENTS.md`, `.hermes.md`; `~/.hermes/`: `SOUL.md` | ✓ |
| OpenClaw | ✓ | ✓ | ✓ | ✓ | — | `~/.openclaw/workspace/`: `AGENTS.md`, `SOUL.md`, `IDENTITY.md`, `USER.md`, `TOOLS.md` | ✓ |
| nanobot | ✓ | ✓ | ✓ | ✓ | — | `~/.nanobot/workspace/`: `AGENTS.md`, `SOUL.md`, `USER.md` | ✓ |
| PicoClaw | ✓ | — | ✓ | ✓ | — | `~/.picoclaw/workspace/`: `AGENT.md`, `AGENTS.md`, `SOUL.md`, `USER.md`, `IDENTITY.md` | ✓ |
| ZeroClaw | — | — | ✓ | ✓ | — | `~/.zeroclaw/agents/assistant/workspace/`: `AGENTS.md`, `SOUL.md`, `IDENTITY.md`, `USER.md`, `TOOLS.md` | by hand |
| IronClaw | ✓ | — | — | ✓ | — | — | ✓ |
<!-- /generated:autonomous-capture -->

**Instructions** here are the persona and operating files in the agent's workspace, under
its config root. hlock pins the files you write: who the agent is (`SOUL.md`,
`IDENTITY.md`), how it works (`AGENTS.md`, `TOOLS.md`) and who it works for (`USER.md`). It
leaves out what the agent writes as it runs, such as `MEMORY.md`, `memory/`, `HEARTBEAT.md`,
sessions and cron state. A conversation therefore isn't drift, but a changed persona is, and
`hlock sync` restores the pinned one.

What hlock doesn't do for these agents:

- **Run the gateway.** `hlock launch` and `hlock run` start the agent's CLI. `run` sends one
  message and waits for the reply, for example `openclaw agent exec`, `nanobot agent -m`, or
  `picoclaw agent -m`. Keeping a gateway running, and the channel accounts it logs into,
  stay with the agent's own service commands.
- **Pin channel credentials.** Bot tokens in the agent's config are secrets. hlock handles
  them like any other [secret](../guides/secrets.md) and never writes them into a lock.
- **Evaluate them in trials.** As with the other agent CLIs, `hlock resolve` runs trials on
  the reference adapter.

Notes on each agent:

- **OpenClaw** keeps its config in JSON5. hlock reads JSON5 and writes plain JSON, which
  OpenClaw also reads, so comments in the file don't survive a `sync`. hlock pins the
  default workspace, `~/.openclaw/workspace`. If `agents.defaults.workspace` points
  somewhere else, copy the profile and change the paths. `--model` is a flag of
  OpenClaw's subcommands, so when you `hlock launch openclaw` with another harness's lock,
  OpenClaw keeps its own model and `launch` says so. On Ollama, `localize` uses
  OpenClaw's native `ollama` API, because OpenClaw calls tools unreliably through Ollama's
  `/v1` endpoint.
- **nanobot** moves its home with `nanobot --home`, not with an environment variable, so an
  isolated `launch` relocates `HOME` instead. A `modelPreset` in its config takes precedence
  over `agents.defaults.model`, which is the model `pin` records. `localize` therefore sets
  a preset.
- **PicoClaw**'s model is a `model_name` that refers to an entry in `model_list`, and `pin`
  records that name. hlock doesn't read API keys kept in `.security.yml`. MCP servers need
  `tools.mcp.enabled` set to `true`.
- **ZeroClaw** chooses a model per agent, so `pin` records the whole `config.toml` instead
  of a model. hlock reads instruction files and skills from the workspace of the
  `assistant` agent, the name used in ZeroClaw's quickstart examples. `hlock run` messages
  that agent too. ZeroClaw has no `localize` settings yet, so `localize` tells you what to
  set by hand.
- **IronClaw** manages MCP servers as extensions and keeps its identity files in its memory
  store, so hlock pins neither. It pins the model, the provider and `config.toml`.

Hermes Agent is listed here because it also runs as a gateway. It's as fully supported as
the agent CLIs above. NanoClaw isn't supported: it's a git checkout that you customize by editing its
code, and its settings live in a SQLite database rather than in a config file a profile can
describe.

## Agent frameworks

Framework profiles are building blocks for your own agent. Declare a
[project harness](../guides/harnesses.md#your-own-agent-frameworks-and-custom-scripts)
with `framework = "<name>"` and an `entrypoint` or `command`, and hlock pins the framework's
packages along with your agent.

<!-- generated:frameworks -->
| Framework | Vendor | Name in hlock | Packages pinned | Instructions | Skills |
|---|---|---|---|---|---|
| [Pydantic AI](https://ai.pydantic.dev) | Pydantic | `pydantic-ai` (`pydantic_ai`, `pydanticai`) | `pydantic-ai-slim`, `pydantic-ai` | `AGENTS.md` | — |
| [Deep Agents](https://github.com/langchain-ai/deepagents) | LangChain | `deepagents` (`deep-agents`, `langchain-deepagents`) | `deepagents`, `langchain`, `langgraph` | `AGENTS.md` | `skills`, `.deepagents/skills` |
| [LangGraph](https://langchain-ai.github.io/langgraph/) | LangChain | `langgraph` | `langgraph`, `langchain-core` | — | — |
| [OpenAI Agents SDK](https://openai.github.io/openai-agents-python/) | OpenAI | `openai-agents` (`openai-agents-sdk`, `agents-sdk`) | `openai-agents`, `openai` | — | — |
| [Claude Agent SDK](https://docs.claude.com/en/docs/agent-sdk/overview) | Anthropic | `claude-agent-sdk` (`claude-sdk`) | `claude-agent-sdk` | `CLAUDE.md` | `.claude/skills` |
| [Google ADK](https://google.github.io/adk-docs/) | Google | `google-adk` (`adk`) | `google-adk` | — | — |
| [smolagents](https://huggingface.co/docs/smolagents) | Hugging Face | `smolagents` | `smolagents` | — | — |
| [CrewAI](https://docs.crewai.com) | — | `crewai` | `crewai` | — | — |
<!-- /generated:frameworks -->

An agent written from scratch, with no framework, works too: give the project harness a
`command` or an `entrypoint` and list its `packages`.

## Local inference runtimes

[`hlock localize`](../guides/localize.md) serves local models with these runtimes, and
[`hlock pin`](../guides/pin.md) recognizes a model provider whose base URL points at one of
their ports. `hlock runtime list` shows which are installed. Ollaya serves only
[decision models](../guides/deciders.md); Ollama serves those (Kev) as well as LLMs.

<!-- generated:runtimes -->
| Runtime | Name in hlock | Default port |
|---|---|---|
| Ollama | `ollama` | `11434` |
| LM Studio | `lmstudio` (`lm-studio`, `lms`) | `1234` |
| llama.cpp | `llama-cpp` (`llamacpp`, `llama.cpp`, `llama-server`) | `8080` |
| vLLM | `vllm` | `8000` |
| SGLang | `sglang` | `30000` |
| MLX LM | `mlx-lm` (`mlx`) | `8080` |
| LocalAI | `localai` (`local-ai`) | `8080` |
| Docker Model Runner | `docker-model-runner` (`docker-model`, `dmr`) | `12434` |
| Jan | `jan` | `1337` |
| Ollaya | `ollaya` | `11435` |
<!-- /generated:runtimes -->

## Add a harness

If your harness isn't listed, you can add it without changing hlock's code:

1.  **Write a profile.** Copy the built-in profile closest to your harness from
    [`crates/harness-lock-core/profiles/harnesses`](https://github.com/unionai-oss/hlock/tree/main/crates/harness-lock-core/profiles/harnesses)
    and edit it. [Harness profiles](../guides/harness-profiles.md) describes every field.
2.  **Try it locally.** Put the file in `.hlock/profiles/harnesses/` in your project, or in
    `~/.config/hlock/profiles/harnesses/` to use it everywhere. Then check that hlock
    reads it the way you expect:

    ```console
    $ hlock harness show my-agent
    ╭─ My Agent ───────────────────────────────────────────────────────────────────────╮
    │                                                                                  │
    │  id               my-agent                                                       │
    │  kind             cli                                                            │
    │  support          basic                                                          │
    │  installed        no                                                             │
    │  config root      /Users/you/.my-agent  ($MY_AGENT_HOME)                         │
    │  file · settings  /Users/you/.my-agent/settings.json  (missing)                  │
    │  skills           /Users/you/.my-agent/skills  (missing)                         │
    │  profile          /Users/you/my-project/.hlock/profiles/harnesses/my-agent.toml  │
    │                                                                                  │
    ╰──────────────────────────────────────────────────────────────────────────────────╯
    ```

    `hlock harness show` prints the paths the profile resolves to on your machine, and
    whether each one exists. Then pin a setup with `hlock pin --harness my-agent` and look
    at what lands in `harness.lock`.

3.  **Contribute it.** Open a pull request that adds the profile to
    `crates/harness-lock-core/profiles/harnesses/` with `support = "basic"`, and regenerate
    this page with `uv run python scripts/harness_table.py`. To mark it `full`, show in the
    pull request that `pin`, `sync` and `launch` work with it, and add tests. See
    [Contributing](../project/contributing.md).

The same steps fix a built-in profile that has a path wrong for your setup: a profile
with the same `id` replaces the built-in one.
