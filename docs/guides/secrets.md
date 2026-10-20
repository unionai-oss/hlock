# Secrets

A lock never holds a secret. [`hlock pin`](pin.md#secrets-stay-out) replaces API keys,
tokens and other credentials with placeholders like `${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}`,
and lists each one as a `[[secret]]` entry. This page covers where hlock finds the values
when it restores a setup, and where it puts them.

## Where values come from

For each placeholder, hlock uses the first of these that has a value:

1. **The environment variable** of the same name (`HLOCK_SECRET_GITHUB_GITHUB_TOKEN`).
2. **A reference in your secrets file**, `~/.config/hlock/secrets.toml`: a command to
   run, another variable, or a file.
3. **Your OS keychain**: the macOS Keychain, or the Secret Service on Linux (GNOME Keyring,
   KWallet) through `secret-tool`.

`hlock secret list` shows what the lock needs and where each value would come from. It
never prints a value:

```console
$ hlock secret list
secret                            from     used by
HLOCK_SECRET_GITHUB_GITHUB_TOKEN  not set  mcp github GITHUB_TOKEN
secrets file ~/.config/hlock/secrets.toml  · keychain security
next → hlock secret set NAME
```

## Store a secret in the keychain

```console
$ hlock secret set GITHUB_GITHUB_TOKEN
value for GITHUB_GITHUB_TOKEN:
✓ stored HLOCK_SECRET_GITHUB_GITHUB_TOKEN in the keychain

$ hlock secret list
secret                            from      used by
HLOCK_SECRET_GITHUB_GITHUB_TOKEN  keychain  mcp github GITHUB_TOKEN
secrets file ~/.config/hlock/secrets.toml  · keychain security
```

The name works with or without the `HLOCK_SECRET_` prefix, and is letters, digits and
`_`. The value is read without
echo, or from stdin when stdin isn't a terminal, so you can pipe it from a password
manager:

```bash
op read op://Work/GitHub/token | hlock secret set GITHUB_GITHUB_TOKEN
```

hlock stores it under the service `hlock`, with the account set to the full variable name.
On macOS the value goes to `security` over stdin, never as a command-line argument, so
other processes can't see it in `ps`. `hlock secret rm NAME` removes it.

## Reference a password manager

To keep a secret in a password manager instead, reference it in
`~/.config/hlock/secrets.toml` (or the file `HLOCK_SECRETS_FILE` names):

```toml
[secrets]
GITHUB_GITHUB_TOKEN = { command = "op read op://Work/GitHub/token" }
ANTHROPIC_KEY = { env = "ANTHROPIC_API_KEY" }   # reuse a variable you already set
SLACK_BOT = { file = "~/.tokens/slack" }
```

Each entry has exactly one of:

| Key | Value comes from |
|---|---|
| `command` | The trimmed output of the command, run with `sh -c`. A failing command or empty output means no value. |
| `env` | Another environment variable |
| `file` | A file's trimmed contents (`~` expands) |

```console
$ hlock secret list
secret                            from                    used by
HLOCK_SECRET_GITHUB_GITHUB_TOKEN  secrets file (command)  mcp github GITHUB_TOKEN
```

The file holds references, not values, so you can keep it in your dotfiles. hlock rejects
a literal value there and points you to `hlock secret set`. A command runs at most once
per hlock command, is stopped after 2 minutes, and what it prints on stderr shows in your
terminal when it finishes (a Touch ID prompt for `op` appears as usual). `secret list`
doesn't run commands. `sync` does, even with `--plan`, because it
compares the config it would write with what's already there.

## Where values go

| Command | What hlock writes |
|---|---|
| [`hlock launch`](launch.md) | For harnesses that read environment variables in their config, a **reference** instead of the value: `${HLOCK_SECRET_…}` in Claude Code's MCP servers, `{env:HLOCK_SECRET_…}` in OpenCode's config. The value goes into the environment of the harness process hlock starts, so it's never written to disk. Elsewhere, the value, in a file only you can read. |
| [`hlock sync`](sync.md) | The value, in a file only you can read (`0600`), along with its `.hlock-bak` backup. You start the harness yourself afterwards, so hlock has no process to pass the value to. |
| [`hlock sync --docker`](sync.md#into-a-docker-image) | Placeholders. The container's entrypoint fills them when it starts (see below). |

`--json` output, sync plans, generated skills and meta prompts name secrets but never
include their values.

If no source has a value, `sync` keeps a real value that's already in the target config.
In an isolated root, it takes one from your own config.

A value in the harness's environment is visible to everything the harness starts,
including the commands an agent runs. That is also true of a value in a config file the
agent can read. What `launch` removes is the copy on disk.

## In Docker

The generated entrypoint fills each placeholder from the environment
(`docker run -e HLOCK_SECRET_…`), else from a mounted secret file:
`/run/secrets/HLOCK_SECRET_<NAME>`, or `/run/secrets/<NAME>`. That covers Docker and
Compose secrets and Kubernetes secret volumes. Files it fills are made readable by their
owner only.

```yaml
services:
  agent:
    build: .hlock/docker/claude-code
    secrets: [GITHUB_GITHUB_TOKEN]
secrets:
  GITHUB_GITHUB_TOKEN:
    environment: GITHUB_TOKEN    # or file: ./github-token
```

## In CI

Set the `HLOCK_SECRET_*` variables from your CI's secret store. On machines without a
desktop session, set `HLOCK_KEYCHAIN=0` so hlock doesn't look for a keychain.
