# Harness profiles

A profile is a TOML file that describes one harness or one local inference runtime. hlock's
harness-aware commands read profiles instead of hard-coding each harness, so supporting a
new harness, or fixing a path for an existing one, means editing a file.

!!! note "What reads which fields"

    Profiles drive detection, versions and install sources, `hlock harness show`, targeting,
    [`hlock pin`](pin.md) (`model`, `providers`, `mcp`, `skills`, `bundles`, `instructions`,
    `files`), [`hlock sync`](sync.md) (`install`, `files`, `skills.write`, `config-root`), and
    [`hlock launch`](launch.md) (`launch`, `mcp.write`, `mcp.shape`, `instructions`, and `run`
    for code harnesses), [`hlock run`](../reference/cli.md#run) (`launch.prompt`), and
    [`hlock localize`](localize.md) (`open-source`, `local-models`,
    `local-provider`, and runtime `catalog`s).

## Where profiles come from

Built-in profiles ship with hlock (source:
[`crates/harness-lock-core/profiles`](https://github.com/unionai-oss/hlock/tree/main/crates/harness-lock-core/profiles)).
hlock then reads these directories, lowest precedence first:

1. `~/.config/hlock/profiles/`
2. each directory in `HLOCK_PROFILE_PATH` (`:`-separated)
3. `./.hlock/profiles/`

Each directory holds `harnesses/*.toml` and `runtimes/*.toml`. A profile with the same `id`
as an earlier one replaces it, so you can override a built-in. `hlock harness show <name>`
prints where the active profile came from.

`./.hlock/profiles/` comes with the repository you're in, so hlock trusts it less, since
`hlock check` runs each profile's binaries to read their versions. A profile there:

- can add a new `id`, but can't replace a built-in or one from the directories above it
  (hlock skips it; override a built-in in `~/.config/hlock/profiles/` instead);
- names its `binaries` and `run.python` as programs on `PATH`; an entry with a `/` or `\`
  is ignored;
- probes its version only with a version flag (`--version`, `-version`, `-V`, `-v` or
  `version`); other `version.args` are ignored.

## A harness profile

```toml
id = "my-agent"
name = "My Agent"
kind = "cli"                       # cli | framework | custom
aliases = ["myagent"]
binaries = ["my-agent"]
version = { args = ["--version"] }
active-env = ["MY_AGENT_SESSION"]  # set by the harness in the processes it spawns
config-root = { env = "MY_AGENT_HOME", default = "~/.my-agent" }
instructions = ["AGENTS.md", "{root}/AGENTS.md"]

[[install]]
method = "npm"                     # npm | brew | brew-cask | pip | uv-tool | script | app | manual
package = "my-agent"
[[install]]
method = "script"
command = "curl -fsSL https://example.com/install.sh | bash"
path-hints = ["/.my-agent/bin/"]   # identifies this method from the binary's real path

[files.settings]
path = "{root}/settings.json"
format = "json"                    # json | jsonc | json5 | toml | yaml
restore = "whole"                  # whole | pointers | record

[model]
default = ["env:MY_AGENT_MODEL", "settings#/model"]
provider = ["settings#/provider"]

[mcp]
sources = ["settings#/mcpServers"]
write = "settings#/mcpServers"

[skills]
dirs = ["{root}/skills", ".my-agent/skills"]
write = "{root}/skills"

[launch]
model-flag = "--model"
prompt = ["run", "{prompt}"]       # one prompt, non-interactively: `hlock run`
```

### Paths

`~` is your home directory and `{root}` is the config root. A relative path is relative to
the project (the current directory). `config-root.env` is the variable that relocates the
root; when it names a parent directory (like `XDG_CONFIG_HOME`), add
`suffix = "<subdir>"`. A file that lives outside the root by default but inside it when the
root is relocated (Claude Code's `~/.claude.json`) sets `isolated = "{root}/…"`.

### Sources

`model`, `mcp` and `providers` take ordered lists of sources, and the first one that has a
value wins:

- `env:VAR` reads an environment variable.
- `<file>#<pointer>` reads a value from a named file in `[files]`, where `<pointer>` is a
  [JSON pointer](https://datatracker.ietf.org/doc/html/rfc6901), applied to TOML and YAML
  files as if they were JSON. `{cwd}` inside a pointer is replaced with the escaped project
  path (Claude Code keeps per-project MCP servers under `/projects/{cwd}/mcpServers`).

Set `model.format = "provider/model"` when the model value carries its provider (OpenCode's
`anthropic/claude-sonnet-4-5`), or `model.default-provider` when the harness only talks to one.

An MCP source is a map of servers by name, or a list of servers that each have a `name`
key (ZeroClaw's `[[mcp.servers]]`). `launch` writes servers as a map, so a harness that
keeps a list leaves out `mcp.write`.

### Config formats

`format` is `json`, `jsonc` (JSON with comments and trailing commas), `json5` (also
unquoted keys, single-quoted strings and hex numbers, as in OpenClaw's `openclaw.json`),
`toml` or `yaml`. hlock writes `jsonc` and `json5` files back as plain JSON, which both
formats accept, so comments don't survive a `sync` or `launch`.

### Autonomous agents

Set `autonomous = true` on a `cli` profile for an agent that runs on its own, as a gateway
with chat channels, schedules and memory, rather than in a session a person drives. Every
command treats it as an agent CLI. Only `hlock harness list` and the
[Supported harnesses](../reference/harnesses.md#autonomous-agents) page group it apart, and
`hlock harness show` labels it.

List only the workspace files you author in `instructions`, such as `SOUL.md` and
`AGENTS.md`. If the profile lists files the agent writes as it runs, such as `MEMORY.md` or
daily notes, `hlock check` reports drift after every conversation, and `hlock sync`
overwrites the agent's memory.

### Restore policies

`restore` says what a file contributes to a locked setup: `whole` (the whole file),
`pointers` (only the subtrees listed in `pointers`, for files that also hold unrelated state),
or `record` (a digest only, for project files that are already in git).

### Environment references

`env-ref` says how the harness reads an environment variable inside a file, with `{name}`
standing for the variable: `"${{name}}"` for Claude Code's `~/.claude.json`, and
`"{env:{name}}"` for OpenCode's `opencode.json`. With it, [`hlock launch`](launch.md) writes a
secret as `${HLOCK_SECRET_…}` (or `{env:HLOCK_SECRET_…}`) and passes the value in the
harness's environment, instead of writing the value into the file. Set it only when the
harness expands references everywhere hlock writes secrets in that file. See
[Secrets](secrets.md#where-values-go).

### Field reference

| Field | Meaning |
|---|---|
| `id`, `name`, `aliases` | Identity; any alias works wherever a harness name does |
| `kind` | `cli`, `framework`, or `custom` (default: `cli`, or `custom` with `run`) |
| `autonomous` | `true` for a `cli` harness that runs as a long-lived gateway (OpenClaw, nanobot, …); see [Autonomous agents](#autonomous-agents) |
| `support` | `full` or `basic` (default `basic`) |
| `vendor`, `homepage` | Informational |
| `binaries` | Names to look up on `PATH`, in order |
| `version.args` | Arguments that print the version; the first dotted number is used |
| `active-env` | Variables whose presence means hlock runs inside this harness |
| `config-root` | `env`, `suffix`, `default` |
| `install` | Install methods, most preferred first: `method`, `package`, `command`, `path-hints` |
| `files.<id>` | `path`, `isolated`, `format`, `restore`, `pointers`, `env-ref` |
| `model` | `default`, `small`, `provider`, `base-url` sources; `format`; `default-provider` |
| `providers` | `source`, plus pointers into each provider for `base-url` and `models` |
| `mcp` | `sources`, `write`, `shape` (`standard`, `opencode`, `codex`, `goose`, `openclaw`, `nanobot`): how `launch` writes servers |
| `skills` | `dirs`, `write`, `builtin-dirs`, `builtin-manifest` |
| `bundles` | Plugins/packages: `kind`, `extractor` (`claude-plugins`, `pi-packages`, `npm-list`), `source`, `lockfile` |
| `instructions` | Instruction files the harness reads |
| `launch` | `args`, `model-flag`; `prompt`: the arguments that run one prompt non-interactively, with `{prompt}` replaced (`hlock run`, and [`hlock meta run --headless`](meta.md)); `interactive`: the arguments that start a session with `{prompt}` as its first message (`hlock meta run`) |
| `api-keys` | API providers the harness talks to, and the variable each reads its key from: `[{ provider = "anthropic", env = "ANTHROPIC_API_KEY" }]` |
| `api-provider` | How [`hlock meta`](meta.md) sets an API provider and model: a list of `{ file, set }` like `local-provider`, with `{provider}` and `{model}` substituted |
| `open-source`, `local-models` | Used by [`hlock localize`](localize.md) to choose local-capable harnesses |
| `local-provider` | How `localize` points the harness at a local endpoint: a list of `{ file, set, runtimes }`, where `set` maps JSON pointers to values; `{provider}`, `{model}`, `{base_url}` and `{host}` are substituted. An entry with `runtimes` applies only to those runtimes; entries apply in order, so one for `["ollama"]` after a generic one overrides it (OpenClaw uses Ollama's native API this way) |
| `packages` | Packages to pin for code harnesses, e.g. `pypi:pydantic-ai` |
| `framework` | For `custom` harnesses: a framework profile to inherit packages from |
| `run` | For code harnesses: `entrypoint`, `command`, `python`, `env` |
| `deciders.<use>` | How a [decision model](deciders.md) for `approve`, `stop`, `route` or `tool` reaches the harness. `hook = { file, events, format, pointer }`: hooks merged into the profile file `file`, under `pointer` (default `/hooks`). `hook = { path, events, format }`: a file at `path` instead, merged into (`cursor`) or written whole and owned by hlock (`copilot`, and the plugin formats `amp`, `opencode`, `pi`). Formats: `claude-code`, `codex`, `gemini`, `cursor`, `copilot`, `amp`, `opencode`, `pi`. `native = { file, pointer, provider-key, model-key, base-url-key }`: a setting of the harness's own that `pin` records |
| `decider-env` | Frameworks: pairs of variables their own decision-model support reads a System One endpoint and key from, e.g. `[["SYSTEM_ONE_BASE_URL", "SYSTEM_ONE_API_KEY"]]` |

## A runtime profile

```toml
id = "my-runtime"
name = "My Runtime"
binaries = ["myrt"]
version = { args = ["--version"] }
ports = [9000]                          # recognizes a provider base URL like http://localhost:9000/v1
provider-names = ["myrt"]               # provider names harness configs use for it
pull = "myrt pull {model}"
serve = "myrt serve"

[models]
layout = "hf-cache"                     # ollama | publisher-repo | hf-cache
dirs = ["env:MYRT_MODELS", "~/.myrt/models"]

[[install]]
method = "brew"
package = "myrt"

[[catalog]]                             # models `hlock localize` may pick
id = "my-coder:30b"
params-b = 30
min-memory-gb = 24                      # weights plus a working context
tier = 3                                # 3 strong agentic coder, 2 capable, 1 small/fast
tools = true                            # reliable tool calling
roles = []                              # e.g. ["small", "decision"]; empty means any
                                        # "decider": a decision model, never picked as a chat model
```

## Project harnesses

Agents you write yourself don't need a profile file: declare them under
`[harnesses.<id>]` in `harness.toml` (see [Harnesses](harnesses.md#your-own-agent-frameworks-and-custom-scripts)).
They accept every harness-profile field, plus `entrypoint`, `command`, `python` and `env` at
the top level of the table.

If you write a profile for a harness others use, please contribute it; see
[Add a harness](../reference/harnesses.md#add-a-harness).
