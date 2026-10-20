# Pinning your setup

`hlock pin` locks the agent setup you already have. Say you've configured your harness by
hand, with a model, a few MCP servers, some skills, and a couple of plugins. `pin` writes all
of it into `harness.lock`, with no manifest and no tuning, so you can commit it, check it in
CI, and later rehydrate it somewhere else.

```console
$ hlock pin
✓ pinned claude-code 2.1.285 → harness.lock  (the only installed harness)
╭─ 📌 harness.lock · pinned ─────────────────────────────────────────────╮
│                                                                        │
│  harness        claude-code 2.1.285                                    │
│  llm · default  anthropic:opus                                         │
│  mcp            github (stdio)                                         │
│  skills         1  (1 local)                                           │
│  config         {root}/settings.json, ~/.claude.json /mcpServers       │
│  secrets        1 redacted  (supplied as HLOCK_SECRET_* when syncing)  │
│                                                                        │
╰────────────────────────────────────────────────────────────────────────╯
vendored 3 files into .hlock/vendor/claude-code/  · review before committing
next → hlock check  ·  hlock sync
```

`pin` picks the harness the same way other commands do (`--harness`, `HLOCK_HARNESS`,
`[target] harness`, the harness you're running inside, then the only installed one; see
[Harnesses](harnesses.md#target-a-harness)). Pass `--harness pi`, say, to pin a different one.

## What gets pinned

| Section | What `pin` records |
|---|---|
| `[harness]` | Version and install source, e.g. `brew-cask:claude-code@2.1.285` or `npm:@earendil-works/pi-coding-agent@1.0.0` |
| `[[llm]]` | The default model (`role = "default"`) and the small/fast model (`role = "small"`) if set, with the provider, the base URL, the local runtime serving it, and the config key it came from (`resolved-from`) |
| `[[runtime]]` | Inference runtimes the configured providers point at: Ollama, LM Studio, llama.cpp, vLLM, and others, matched by provider name or port. `location = "local"` runtimes get a version and a lookup in the local model store (digest and size for Ollama, repo and size for LM Studio, revision for the Hugging Face cache). Self-hosted servers on a private network (`10.x`, `192.168.x`, Tailscale `100.64/10`, `*.local`) are `location = "network"`. Public API endpoints aren't runtimes. |
| `[[tool]]` with `kind = "mcp"` | Each MCP server, normalized across config formats: `transport`, `command`/`args` or `url`, `env`, `headers`, `scope` (`user` or `project`), and the package it launches (`npm:…`, `pypi:…`, `oci:…`) |
| `[[skill]]` | Each skill with a tree digest and an origin: `harness-builtin` (shipped with the harness), `bundled` (from a plugin), `third-party` (a git checkout, pinned to its commit), `first-party` (in the project), or `local` (hand-written; vendored) |
| `[[native]]` | Plugins, packages and marketplaces: Claude Code plugins (version and commit) and their marketplaces, Pi packages (version and npm integrity), OpenCode plugins, and the Python packages of a code harness |
| `[[instructions]]` | Instruction files the harness reads, like `CLAUDE.md` and `AGENTS.md`, with digests |
| `[[config]]` | The harness's config files with digests, and what `sync` restores from each: the whole file, only some subtrees (`pointers`, e.g. `/mcpServers` of `~/.claude.json`), or nothing (`record`, for project files that are already in git) |

Paths are written portably: `{root}` is the harness's config root, `~` is the home
directory, and relative paths are relative to the project.

## Secrets stay out

`pin` never writes a secret. Values under keys like `apiKey`, `token`, `password`,
`Authorization` and `credentials`, credential-looking values (`sk-…`, `ghp_…`, long random
tokens), and secret query parameters in URLs are replaced with placeholders:

```toml
[[tool]]
name = "github"
kind = "mcp"
source = "npm:@modelcontextprotocol/server-github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
scope = "user"

[tool.env]
GITHUB_TOKEN = "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
```

Values that are already environment references (`${GITHUB_TOKEN}`, `{env:KEY}`) are kept as
they are. The lock's `[[secret]]` entries list every placeholder and what uses it. When
you restore the setup, each value comes from the environment, your OS keychain
(`hlock secret set`), or a password-manager reference. See [Secrets](secrets.md).

## Vendoring

Some things another machine can't fetch: hand-written skills, user-level instruction files,
and your config files. `pin` copies them, with secrets already redacted, into
`.hlock/vendor/<harness>/` next to the lock, so the lock plus that directory is
self-contained. A lock named `harness.<name>.lock` or `<name>.lock` vendors into
`.hlock/vendor/<name>/<harness>/` instead, so two locks never share vendored files. Commit both. Review the vendored files first, because a skill you wrote
yourself is copied as it is. Pass `--no-vendor` to record digests only.

`pin` builds the new vendor directory beside the old one and swaps it in, so a pin that
fails leaves the previous files as they were. Symlinks inside a skill aren't followed: they
aren't vendored or digested, and `pin` warns about each one. A file it can't read fails that
skill's vendoring, with a warning, rather than being copied as an empty file. A skill whose
`name:` isn't a plain directory name (it has a `/` or `..`) is locked under its directory's
name. Two vendored files with the same name (two `AGENTS.md`) get `AGENTS.md` and
`AGENTS-2.md`.

Source URLs carry no credentials: a skill's git remote or a marketplace URL is recorded
without its user name and password or token (`https://ghp_…@github.com/a/b` is locked as
`https://github.com/a/b`); an ssh user name (`git@github.com:a/b`) stays.

## Keep it honest with `check`

On a pinned lock, `hlock check` re-captures the harness and fails if anything changed:

```console
$ hlock check
✗ drift: your claude-code setup changed since harness.lock was pinned
  ~ llm default: anthropic:opus → anthropic:sonnet
  ~ config {root}/settings.json: 016233bf39 → d5aaa91f9e
  hint: re-pin with hlock pin, or restore it with hlock sync --in-place
```

A config file's digest covers what `sync` restores from it, parsed and with secrets
redacted, so reformatting the file or using a different value for a secret isn't drift.

Running `hlock pin` again overwrites the pin and prints the same kind of diff under
"changes since the last pin".

## Pinned and resolved locks

A pinned lock has `kind = "pin"` and no evidence. A resolved lock, written by `hlock resolve`,
measures a search space against your tasks. `pin` won't overwrite a resolved lock: pass
`-o pinned.lock` to keep both, or `--force` to replace it.

`hlock show` and `hlock tree` render either kind.

## Next: rehydrate it

[`hlock sync`](sync.md) restores a pinned setup on another machine: into an isolated
directory (`.hlock/env/`, the default), into your own config (`--in-place`), or into a
Docker image.
