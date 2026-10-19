# Launching a locked harness

`hlock launch` starts a session in your harness that matches the lock: the pinned version,
model, MCP servers, skills, plugins and config, and none of the unlocked config on your
machine. It's how a lock gets *used*, the way `uv run` uses a virtual environment.

```console
$ hlock launch
✓ Write .hlock/env/claude-code/config/settings.json
✓ Write .hlock/env/claude-code/config/.claude.json /mcpServers
✓ Copy skill my-skill → .hlock/env/claude-code/config/skills/my-skill
▶ Claude Code  CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config /opt/homebrew/bin/claude
```

`launch`:

1. checks that the installed harness is the version the lock pins, and stops if it isn't
   (`hlock sync --install` fixes that; `--allow-version-mismatch` overrides);
2. restores the locked setup into an isolated config root under `.hlock/env/<harness>/`,
   the same way [`hlock sync`](sync.md#the-isolated-directory) does, including
   plugin installs; this only does work the first time, or after the lock changes;
3. starts the harness with its config-root variable pointing there (`CLAUDE_CONFIG_DIR`,
   `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `HERMES_HOME`, `CURSOR_CONFIG_DIR`,
   `XDG_CONFIG_HOME`), replacing the `hlock` process.

Secrets come from the environment, your secrets file, or your keychain (see
[Secrets](secrets.md)). On the machine where you pinned, they also come from your live
config, so MCP servers and providers work without setting anything up. `launch` warns about
any that are missing. For Claude Code and OpenCode, `launch` writes a reference such as
`${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}` into the config and passes the value in the
harness's environment, so it never reaches the disk. Sign-in state isn't copied, so the first
session in a new environment may ask you to log in.

Arguments after the harness name go to the harness:

```bash
hlock launch claude --resume          # claude --resume, in the locked environment
hlock launch -- --dry-run             # after --, even flags hlock would take
hlock launch --in-place               # your own config, no isolation (version still checked)
hlock launch --dry-run                # prepare everything and print the command
```

## Launch a different harness

Name another harness to run the lock's **portable** parts in it:

```console
$ hlock launch opencode --dry-run
✓ wrote 1 MCP server to .hlock/env/opencode/xdg/opencode/opencode.json
✓ copied 1 skill to .hlock/env/opencode/xdg/opencode/skills
! translating a Claude Code lock into OpenCode
! Claude Code-specific settings (permissions, hooks, theme, providers) don't carry over
▶ OpenCode  XDG_CONFIG_HOME=.hlock/env/opencode/xdg /opt/homebrew/bin/opencode --model anthropic/opus
```

| Carries over | How |
|---|---|
| MCP servers | Rewritten in the target's config shape (`mcpServers` for most, OpenCode's `mcp` with `local`/`remote`, Codex's `mcp_servers` in TOML, Goose's `extensions`). Secrets come from the environment, else from the source harness's live config. |
| Skills | Copied into the target's skills directory |
| User instructions | The source's user-level file (e.g. `~/.claude/CLAUDE.md`) becomes the target's (e.g. OpenCode's `AGENTS.md`) |
| Model | Passed with the target's model flag when it can reach the provider (`provider/model` for OpenCode and Pi); otherwise `launch` says so and the target uses its default |

Plugins, built-in skills, and harness-specific settings (permissions, hooks, providers,
themes) don't carry over, and `launch` lists them.

## Run one prompt

`hlock run "<prompt>"` prepares the same environment and runs the harness's
non-interactive mode on the prompt (`claude -p`, `codex exec`, `opencode run`, `pi -p`,
`hermes -z`, …, from the profile's `[launch] prompt`). It exits with the harness's result,
and `--json` captures the output for scripts. See the [CLI reference](../reference/cli.md#run).

## Code harnesses

For a [project harness](harnesses.md#your-own-agent-frameworks-and-custom-scripts),
`hlock launch` runs its entrypoint or command through the
[code-harness contract](harnesses.md#how-hlock-runs-a-code-harness), with the
arguments joined into the prompt. `hlock run "<prompt>"` does the same and reports the
result.
