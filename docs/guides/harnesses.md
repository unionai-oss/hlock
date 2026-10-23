# Harnesses

hlock knows about the agent harnesses people actually use, and lets you say which one a lock
is for. Each harness is described by a [profile](harness-profiles.md), so hlock knows where
it keeps its config, model, MCP servers, skills and plugins, and how it was installed.

## What's on this machine

```console
$ hlock harness list
Agent CLIs
   harness       installed           source                              support
●  claude-code   2.1.285             brew-cask:claude-code@2.1.285       full
   codex         —                                                       full
   pi            1.0.0               npm:@earendil-works/pi-coding-agen  full
                                     t@1.0.0
   opencode      1.18.18             brew:opencode@1.18.18               full
   cursor-agent  2026.05.28-a70ca7c  script@2026.05.28-a70ca7c           full
   gemini-cli    —                                                       basic
   copilot-cli   —                                                       basic
   goose         —                                                       basic
   amp           —                                                       basic

Autonomous agents
  harness   installed  source         support
  hermes    0.20.0     script@0.20.0  full
  openclaw  —                         basic
  nanobot   —                         basic
  picoclaw  —                         basic
  zeroclaw  —                         basic
  ironclaw  —                         basic

Frameworks
  harness           installed  source
  pydantic-ai       —
  deepagents        —
  …

● you're running inside Claude Code
```

Autonomous agents, such as OpenClaw, are agent CLIs that run as long-lived gateways you
message from chat apps. They're listed apart, and work with the same commands. The `●`
marks the harness hlock is running inside, detected from the variables each harness
sets in the processes it spawns. `source` is how the installed copy was installed, which is
what a lock pins. `full` profiles are covered by tests for every harness-aware command;
`basic` ones cover detection, version, MCP servers and skills.

[Supported harnesses](../reference/harnesses.md) lists every built-in harness, framework
and runtime, and what hlock captures from each.

`hlock harness show <name>` prints a profile and the exact paths it resolves to here:

```console
$ hlock harness show pi
╭─ Pi ─────────────────────────────────────────────────────────────────────╮
│  id                       pi  pi-coding-agent, pi-agent                  │
│  kind                     cli                                            │
│  support                  full                                           │
│  installed                1.0.0  /Users/you/.nvm/versions/node/v22…/pi   │
│  source                   npm:@earendil-works/pi-coding-agent@1.0.0      │
│  config root              /Users/you/.pi/agent  ($PI_CODING_AGENT_DIR)   │
│  file · mcp               /Users/you/.pi/agent/mcp.json                  │
│  file · models            /Users/you/.pi/agent/models.json               │
│  file · settings          /Users/you/.pi/agent/settings.json             │
│  skills                   /Users/you/.pi/agent/skills                    │
│  …                                                                       │
╰──────────────────────────────────────────────────────────────────────────╯
```

`hlock runtime list` does the same for local inference runtimes (Ollama, LM Studio,
llama.cpp, vLLM, SGLang, MLX LM, LocalAI, Docker Model Runner, Jan).

## Target a harness

Put the harness a lock is for in the manifest:

```toml
[target]
harness = "claude-code"        # an id or alias: claude, codex, pi, opencode, …
```

It belongs in `harness.toml` rather than only on the command line because it changes what
the lock means, and the manifest hash covers it. `hlock init` writes it for you when you run
it inside a harness, or when you pass `--harness`:

```console
$ hlock init --pack ./evals --harness claude
✓ wrote harness.toml  (project 'demo', eval pack ./evals)
target claude-code  (--harness)
next → hlock resolve
```

Commands that act on a harness pick it in this order:

1. `--harness <name>`
2. the `HLOCK_HARNESS` environment variable
3. `[target] harness` in `harness.toml`
4. the harness hlock is running inside
5. the only harness installed on `PATH`

The flag is `--harness`, not `--target`, because `[backend] target` already names the
execution backend.

## Resolve for a target

With a target, `resolve` narrows `[candidates] harness` to it. With no harness candidates,
or only the default `reference`, the target becomes the only candidate; otherwise it has to
be one of them. The lock's `[harness]` pins the version and install source of the copy on
this machine:

The candidate models must be ones the harness runs: `resolve` refuses an agent CLI paired
with the offline `reference:` models. `hlock init --harness claude-code` takes them from a
`harness.lock` that pins Claude Code, and otherwise leaves `[candidates.llm.planner]` for you
to fill in.

```console
$ hlock init --force --harness claude-code
✓ wrote harness.toml  (project 'demo', eval pack ./evals)
target claude-code  (--harness)
next → hlock resolve
$ hlock resolve
…
╭─ 🔒 harness.lock ──────────────────────────────╮
│                                                │
│  Stack                                         │
│  harness        claude-code 2.1.285            │
│  llm · planner  anthropic:opus                 │
│  …                                             │
╰────────────────────────────────────────────────╯
! claude-code itself didn't run these trials: they ran on hlock's offline reference harness, so the evidence is a stand-in. hlock evals run tries the real agent on tasks.
```

hlock can't drive agent CLIs inside trials yet, so trials run on the backend's adapter.
`[harness] adapter` in the lock records which one, and `[evidence] simulated = true` marks
evidence from a stand-in, which `hlock show` and `hlock check` repeat. To watch the real
agent on your tasks, use [`hlock evals run`](eval-packs.md#try-tasks-on-your-agent).
`hlock resolve --harness <name>` overrides the target for one run and reminds you to
record it in `harness.toml`.

`hlock check` then also fails when:

- the lock is for a different harness than `[target]`, or
- the installed harness version differs from the version the lock pins.

A harness that isn't installed (on a CI runner, say) skips the version check.

## Desktop apps

Desktop apps keep their MCP servers, skills and instruction files in plain files, so hlock
pins, checks, syncs and migrates them like an agent CLI. Their profiles have `kind = "app"`:

| App | Profile | Shares its files with |
|---|---|---|
| Claude Desktop | `claude-desktop` | (its Code tab: `claude-code`) |
| VS Code with Copilot | `vscode` | |
| Cursor | `cursor-ide` | `cursor-agent`: `~/.cursor/mcp.json`, rules, skills |
| ChatGPT (formerly the Codex app) | `chatgpt` (alias `codex-app`) | `codex`: `~/.codex/config.toml`, `AGENTS.md`, skills |
| Devin Desktop (formerly Windsurf) | `devin-desktop` (alias `windsurf`) | the Devin CLI: `~/.config/devin` |
| Kiro | `kiro` | |
| Antigravity (the app and the IDE) | `antigravity` | |
| TRAE | `trae` | |

hlock never starts an app, so `launch`, `run`, `resolve` and `tune` refuse one: you open
the app yourself. An app that shares its files with a CLI is restored by either profile's
lock; pin with the app's profile to record the app's version.

Devin Desktop's older Cascade agent keeps its setup under `~/.codeium/windsurf`, and the
profile restores only Devin Local's. Zed isn't here yet: its MCP servers live in a
`settings.json` full of comments, which a write would drop.

An app's setup is in your user config, so `pin` takes the user scope by default:

```console
$ hlock pin --harness claude-desktop
✓ pinned claude-desktop 3.1623.1 → harness.lock  (--harness)
╭─ 📌 harness.lock · pinned ─────────────────────────────────────╮
│                                                                │
│  harness        claude-desktop 3.1623.1  app@3.1623.1          │
│  scope          project + user                                 │
│  llm · default  unknown:harness-default                        │
│  mcp            filesystem (stdio)                             │
│  config         {root}/claude_desktop_config.json /mcpServers  │
│                                                                │
╰────────────────────────────────────────────────────────────────╯
vendored 1 files into .hlock/vendor/claude-desktop/  · review before committing
next → hlock check  ·  hlock sync
```

hlock reads an app's version from its bundle on macOS (`/Applications/Claude.app`), or from
its CLI shim (`code --version`). Apps update themselves, so a newer installed version isn't
drift. Claude Desktop pins only the `mcpServers` in `claude_desktop_config.json`: the rest
of that file is state the app writes. Its Code tab reads `~/.claude`, which is
[Claude Code's](../reference/harnesses.md#agent-clis) profile.

An app reads only its own config, not an isolated root, so `sync` writes it only with
`--apps` (or `--in-place`). Without it, the steps that would write it wait for you, and an
interactive `sync` asks first:

```console
$ hlock sync
Sync plan for Claude Desktop 3.1623.1 → your config ~/Library/Application Support/Claude
(needs --apps)
✓  harness  Claude Desktop 3.1623.1
✓  mcp      `npx` for MCP servers: filesystem
○  config   Write ~/Library/Application Support/Claude/claude_desktop_config.json
            /mcpServers  Claude Desktop reads only your own config; hlock sync --apps
            writes it
not restored: 3 parts of Claude Desktop that live outside files hlock restores (hlock
harness show claude-desktop)

1 step needs you:
  1. Write ~/Library/Application Support/Claude/claude_desktop_config.json /mcpServers
Claude Desktop reads only your own config; hlock sync --apps writes it
next → hlock sync --apps
$ hlock sync --apps --yes
…
✓ Write ~/Library/Application Support/Claude/claude_desktop_config.json /mcpServers
✓ in sync with harness.lock
quit and reopen Claude Desktop to load the changes
```

- **A running app isn't written.** Claude Desktop can overwrite its config while it runs,
  so while it's open the step says to quit it first.
- **What isn't in a file isn't restored.** Connectors (remote MCP servers), chat projects
  and Desktop Extensions live in your account or the app. `hlock harness show
  claude-desktop` lists them, with links. A remote MCP server can't go in Claude Desktop's
  file, so hlock leaves it to you as a connector.
- **Installing the app.** `sync --install` runs `brew install --cask` on macOS. Without
  Homebrew, the app is a download, which the [meta harness](meta.md#install) can do for you:
  `hlock meta run install --apps`.

To keep using Claude Code and put the same MCP servers in Claude Desktop, name the app in
`[target] also` (see [Mixed-harness teams](dependencies.md#mixed-harness-teams)). VS Code
can also import Claude Desktop's servers itself (`chat.mcp.discovery.enabled`), so with both
you may see a server listed twice.

## Your own agent: frameworks and custom scripts

Agents built on a framework, or written from scratch, are declared in the manifest as
**project harnesses**:

```toml
[target]
harness = "triage"

[harnesses.triage]
framework = "pydantic-ai"            # pins the framework's packages
entrypoint = "triage.agent:run"      # a Python module:callable

[harnesses.loop]
command = ["./bin/agent", "--model", "{model}", "{prompt}"]   # or any executable
packages = ["pypi:httpx"]
```

A project harness needs an `entrypoint` or a `command`. `framework` names a framework
profile, whose packages are added to `packages`; `hlock harness show triage` lists each
package with the version installed in the Python that `HL_PYTHON` (or `python3`) points at.
Project harnesses appear in `hlock harness list` and can be targeted like any other.

Unlike agent CLIs, a code harness runs for real during `hlock resolve`: each trial calls its
entrypoint, so the evidence in the lock measures your agent. `hlock run "<prompt>"` and
`hlock launch` run it on a locked setup.

### How hlock runs a code harness

hlock passes everything through environment variables and runs the harness in the trial's
workspace:

| Variable | Value |
|---|---|
| `HLOCK_PROMPT` | The task prompt |
| `HLOCK_MODEL` | The model as `provider:model` |
| `HLOCK_LLM_BASE_URL` | The model's base URL, when the lock or `HL_LLM_BASE_URL` sets one |
| `HLOCK_WORKSPACE` | The working directory for artifacts (also the current directory) |
| `HLOCK_MCP_CONFIG` | A `{"mcpServers": …}` file for the lock's MCP servers, readable only by you. Its `${HLOCK_SECRET_…}` placeholders are filled from the environment and your [secret sources](secrets.md); other `${…}` are left as written. |
| `HLOCK_SKILLS_DIR` | A directory holding the lock's skills |
| `HLOCK_INSTRUCTIONS_DIR` | A directory holding the lock's instruction files that have a copy next to it, such as a tune candidate's |
| `HLOCK_SUBAGENTS_DIR`, `HLOCK_SCRIPTS_DIR`, `HLOCK_MIDDLEWARE_DIR` | Directories holding the lock's subagents, scripts and middleware files that have a copy next to it, such as a tune candidate's |
| `HLOCK_LOCK` | The lockfile |
| `HLOCK_OUTPUT` | Where the harness may write a JSON result |
| `HLOCK_SEED`, `HLOCK_HARNESS` | The trial seed and the harness id |
| `HLOCK_DECIDER_<USE>`, `HLOCK_DECIDE_LOCK`, `HLOCK_BIN` | The locked [decision models](deciders.md#code-harnesses), and how to ask them |

A `command` gets `{prompt}`, `{model}` and `{workspace}` substituted in its arguments; a
relative executable path is relative to the project. The result is the JSON in
`HLOCK_OUTPUT`, `{"success": true, "cost_usd": 0.01, "steps": 3, "trace": […]}`, or, without
one, the exit code (`0` is success).

An `entrypoint` is called as `fn(prompt, ctx)`, and may be `async`:

```python title="triage.py"
def run(prompt, ctx):
    # ctx.model ("provider:model"), ctx.provider, ctx.model_name, ctx.base_url,
    # ctx.workspace, ctx.mcp_config, ctx.skills_dir, ctx.lock, ctx.seed,
    # ctx.decide("route", options=[…], state={…}) for a locked decision model
    (ctx.workspace / "summary.md").write_text(answer(prompt, ctx.model))
    return {"output": "done", "cost_usd": 0.002}
```

Return a `dict` (merged into the result, `success` defaults to true), a `bool`, or anything
else (recorded as `output`). An exception is a failed run. hlock runs it with `python -c`
and a small built-in shim, so the interpreter (`python = "…"` in the table, else
`HL_PYTHON`, else `python3`) needs only your agent's dependencies, not hlock. The project
directory is on `PYTHONPATH`.

## Add a harness hlock doesn't know

Write a profile. See [Harness profiles](harness-profiles.md), and
[Add a harness](../reference/harnesses.md#add-a-harness) for how to try it and contribute it.
