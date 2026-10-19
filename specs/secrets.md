# Secrets: where values come from, and where they land

Status: implemented on `feat/secret-sources` (§1–§4). §5 lists what's left open.

## Problem

`hlock pin` never writes a secret into a lock: it replaces API keys, tokens and
credential-looking values with `${HLOCK_SECRET_<NAME>}` placeholders and lists them as
`[[secret]]` entries. That part is sound. The weak points were downstream:

1. **Values had one source.** `sync` and `launch` read a placeholder's value only from the
   environment variable of the same name. Users had to keep tokens in shell profiles or
   `.envrc` files, which are plaintext and easy to commit or leak to every process.
2. **Values were written to disk in plaintext.** `sync` filled placeholders into config
   files (`.claude.json`, `opencode.json`, …) with the file's default mode, usually
   world-readable (`0644`).
3. **`launch` did the same**, although it controls the harness's process and could hand it
   the values directly.
4. **Docker** took values only from `docker run -e`, which shows them in `docker inspect`
   and shell history, and ignored Docker/Compose secrets mounted at `/run/secrets`.

Other tools settled on a common pattern: store a *reference*, resolve the value as late as
possible, prefer the OS keychain for storage, and hand the value to the process instead of
writing it to disk. Examples: `gh` and git-credential-manager (keychain), Docker's
`credsStore` (credential helper commands), 1Password's `op run` / `op read` (references),
Kubernetes and Compose (secret files mounted at runtime).

## Goals

- No new place where secret values are stored by hlock in plaintext.
- Work with the stores people already use: OS keychain, password-manager CLIs, env vars.
- Keep the lock format unchanged: placeholders stay the contract.
- Never print values: `--json`, plans, generated skills and prompts carry names only.

Non-goals: encrypting the lock, team secret distribution (sops, Vault policies), and
rotating secrets.

## 1. Owner-only files

Any file hlock writes that may contain a secret value is restricted to its owner
(`0600`), along with its `.hlock-bak` backup:

- `sync` `WriteConfig` steps whose vendored file contains placeholders (in `.hlock/env` or
  `--in-place`);
- config written by `launch` when translating into a harness without environment
  references (§3);
- files the Docker entrypoint fills (§4).

Files without placeholders keep their mode. On non-Unix platforms this is a no-op.

## 2. Secret sources

`secrets::lookup(name)` resolves a placeholder variable. First hit wins:

1. **Environment**: the variable itself (`HLOCK_SECRET_GITHUB_TOKEN`). Unchanged
   behavior, and the way to override anything below for one command.
2. **Secrets file**: a *reference* in `~/.config/hlock/secrets.toml` (or
   `$HLOCK_SECRETS_FILE`):

   ```toml
   [secrets]
   GITHUB_GITHUB_TOKEN = { command = "op read op://Work/GitHub/token" }
   ANTHROPIC_KEY = { env = "ANTHROPIC_API_KEY" }   # reuse a variable you already set
   SLACK_BOT = { file = "~/.tokens/slack" }
   ```

   Keys may omit the `HLOCK_SECRET_` prefix. Each entry needs exactly one of `command`
   (run with `sh -c`; the value is its trimmed stdout; non-zero exit or empty output means
   no value), `env`, or `file` (trimmed contents; `~` expands). A literal string value is
   an error that points at `hlock secret set`: the file holds references, so it's safe to
   keep in a dotfiles repo.

   A command runs at most once per hlock process (cached by command text), with stdin
   closed and stderr passed through so password-manager prompts and errors show. It
   times out after 120 seconds.
3. **OS keychain**: service `hlock`, account `HLOCK_SECRET_<NAME>`.
   - macOS: `/usr/bin/security`. Reads use `find-generic-password -w`. Writes use
     `security -i` with the command on stdin, so the value never appears in a process's
     arguments (where `ps` would show it).
   - Linux: `secret-tool` (libsecret / GNOME Keyring / KWallet via the Secret Service).
     `store` reads the value from stdin.
   - `HLOCK_KEYCHAIN=0` turns the keychain off (CI, or machines without a session bus).

   The backends are the platforms' own CLIs, not a linked library, so the wheels gain no
   native dependencies (the `keyring` crate's Linux backend needs libdbus at build time).

The **CLI**:

```bash
hlock secret set NAME     # store in the keychain; prompts without echo, or reads stdin
hlock secret rm NAME      # remove from the keychain
hlock secret list         # the lock's secrets and where each would come from (no values)
```

`secret list` reports a command reference as `secrets file (command)` without running it.

**Plans.** `sync` resolves secrets while planning (it compares the config it would write
with what's there), so a command reference runs during `--plan` too, once. A satisfied
secret step says where its value came from (`from the keychain`). A missing one suggests
`hlock secret set <NAME>`. Generated skills, meta prompts and `--json` output carry
names, never values.

**Fallback** is unchanged: when no source has a value, `sync` keeps a real value already
in the target config, and in prefix mode, one in the user's own config.

Every place that fills placeholders uses `lookup`: `sync` (config files), `launch`
(including translation into another harness), code harnesses (`contract.rs`, tool
environments), and the meta harness (through `launch`).

## 3. Environment references instead of values (`launch`)

`launch` starts the harness itself, so it can keep values off disk entirely: it writes the
harness's own *environment reference* where the secret goes and puts the value in the
harness's environment.

A harness profile declares, per config file, how the harness reads an environment
variable there:

```toml
[files.state]                 # claude-code: ~/.claude.json
pointers = ["/mcpServers"]
env-ref = "${{name}}"         # → ${HLOCK_SECRET_GITHUB_TOKEN}

[files.config]                # opencode: opencode.json
env-ref = "{env:{name}}"      # → {env:HLOCK_SECRET_GITHUB_TOKEN}
```

`{name}` is replaced with the placeholder variable. A profile sets `env-ref` only when
the harness expands the reference everywhere hlock writes secrets in that file:

- **Claude Code** expands `${VAR}` in MCP server `command`, `args`, `env`, `url` and
  `headers`, for servers in `.mcp.json` and in `~/.claude.json` (user and local scope)
  ([docs](https://code.claude.com/docs/en/mcp)). hlock restores only `/mcpServers` from
  that file, so the whole restored part is covered. `settings.json` gets no `env-ref`
  (its `env` block isn't documented to expand), so secrets there are written as values
  with `0600`.
- **OpenCode** substitutes `{env:VAR}` across the whole config text before parsing it
  (`ConfigVariable.substitute` in `packages/core/src/config/variable.ts`).
- Other harnesses: no `env-ref` until their expansion is confirmed. Their secrets are
  written as values with `0600`.

Mechanics:

- `SyncOptions::inject` makes a plan write references for files with an `env-ref`. The
  "keep the existing value" fallback ignores the target, which holds references.
- `sync::secret_env` returns the values behind those references: from `lookup`, else from
  the same place in the user's own config (the machine that pinned the lock). A value is
  recovered from a string with one placeholder (`Bearer ${X}` against `Bearer abc` gives
  `abc`).
- When translating into another harness, the target's `mcp.write` file's `env-ref`
  applies the same way.
- `LaunchPrep.secret_env` carries the values to the Python side, which adds them to the
  environment it `exec`s with and removes them before printing (`launch --dry-run --json`,
  `meta run --dry-run --json`). The isolation summary printed before a session skips
  `HLOCK_*` variables.

`sync` doesn't inject: it prepares a root you start yourself
(`CLAUDE_CONFIG_DIR=… claude`), so there's no process to pass values to. It writes values,
owner-only.

**Exposure trade-off.** A value in the harness's environment is inherited by everything
the harness starts, including the shell commands an agent runs, so an agent could
`echo $HLOCK_SECRET_…`. A value in a config file is just as readable by that agent
(`cat ~/.claude.json`). Injection removes the at-rest copy on disk, which is the larger
risk (backups, sync clients, other users, accidental commits). It doesn't sandbox the
agent from its own credentials, and nothing short of a credential proxy would.

## 4. Docker secrets

The generated entrypoint fills `${HLOCK_SECRET_*}` placeholders from, in order:

1. the environment (`docker run -e HLOCK_SECRET_…`);
2. `/run/secrets/HLOCK_SECRET_<NAME>`, then `/run/secrets/<NAME>`: Docker swarm secrets,
   Compose `secrets:`, and Kubernetes secret volumes mounted there.
   `HLOCK_SECRETS_DIR` moves the directory.

Files it fills become `0600`. Example:

```yaml
services:
  agent:
    build: .hlock/docker/claude-code
    secrets: [GITHUB_GITHUB_TOKEN]
secrets:
  GITHUB_GITHUB_TOKEN:
    environment: GITHUB_TOKEN    # or file: ./github-token
```

## 5. Open questions

- **Windows**: no keychain backend. Credential Manager has no CLI that reads a secret
  back. The options are a small PowerShell helper or the `keyring` crate behind a
  Windows-only feature.
- **Shorthands** for common managers (`"op://…"` meaning `op read`) instead of spelling
  out `command`.
- **`sync --references`**: write `env-ref`s from `sync` too, and print an env file or an
  `hlock exec` wrapper that supplies values, for people who start the harness themselves.
- **Per-project references**: a project-level secrets file (committed, references only)
  so a team shares *where* secrets come from without sharing values.
- **More `env-ref` profiles**: confirm expansion for Codex, Pi, Goose, Hermes, Cursor and
  Gemini CLI configs.
