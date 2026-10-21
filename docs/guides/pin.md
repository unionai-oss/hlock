# Pinning your setup

`hlock pin` locks the agent setup you already have: the harness and its version, the model,
and the MCP servers, skills, instruction files and config the harness reads. It writes them
into `harness.lock`, with no manifest and no tuning, so you can commit it, check it in CI,
and later rehydrate it somewhere else.

```console
$ hlock pin
✓ pinned claude-code 2.1.0 → harness.lock  (the only installed harness)
╭─ 📌 harness.lock · pinned ───────────╮
│                                      │
│  harness        claude-code 2.1.0    │
│  scope          project              │
│  llm · default  anthropic:opus       │
│  mcp            linear (http)        │
│  skills         1  (1 first-party)   │
│  instructions   CLAUDE.md            │
│  config         .mcp.json            │
│                                      │
╰──────────────────────────────────────╯
left out user-level: 1 skill, 1 MCP server, 1 instruction file, 2 config files
  to pin it too: hlock pin --user, or pick with -i GLOB
next → hlock check  ·  hlock sync
```

`pin` picks the harness the same way other commands do (`--harness`, `HLOCK_HARNESS`,
`[target] harness`, the harness you're running inside, then the only installed one; see
[Harnesses](harnesses.md#target-a-harness)). Pass `--harness pi`, say, to pin a different one.

## Project and user scope

A harness reads its setup from two places: the **project** (files in the repository, like
`.claude/skills/`, `.mcp.json`, `CLAUDE.md` and `.claude/settings.json`) and your **user**
config root in your home directory (`~/.claude`, `~/.pi/agent`, `~/.codex`, …), which holds
your personal skills, MCP servers, plugins, instructions and settings.

By default, `pin` records only the project's. Your personal setup stays out of the
repository, and the summary says what was left out. To pin your user-level setup too, for
example to carry it to a new laptop or to share it, pass `--user`:

```console
$ hlock pin --user
✓ pinned claude-code 2.1.0 → harness.lock  (the only installed harness)
╭─ 📌 harness.lock · pinned ───────────────────────────────────────────────────╮
│                                                                              │
│  harness        claude-code 2.1.0                                            │
│  scope          project + user                                               │
│  llm · default  anthropic:opus                                               │
│  mcp            github (stdio), linear (http)                                │
│  skills         2  (1 local, 1 first-party)                                  │
│  instructions   CLAUDE.md, {root}/CLAUDE.md                                  │
│  config         .mcp.json, {root}/settings.json, ~/.claude.json /mcpServers  │
│  secrets        1 redacted  (supplied as HLOCK_SECRET_* when syncing)        │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯
vendored 4 files into .hlock/vendor/claude-code/  · review before committing
changes since the last pin
  + mcp github
  + skill review-pr (user)
  + instructions {root}/CLAUDE.md
  + config {root}/settings.json
  + config ~/.claude.json
next → hlock check  ·  hlock sync
```

Some things are pinned in either scope, because they say what the stack *is* wherever
they're set: the harness version and install source, the default and small models with the
runtimes serving them, decision models, and the skills built into the harness. Everything
else follows the scope:

| | `hlock pin` | `hlock pin --user` |
|---|---|---|
| Harness, models, runtimes, deciders | ✓ | ✓ |
| Skills | in the project | also in your user config root |
| MCP servers | in project files (`.mcp.json`) | also user-level ones, and Claude Code's per-project "local" servers in `~/.claude.json` |
| Instruction files | in the project (`CLAUDE.md`, `AGENTS.md`) | also user-level ones (`~/.claude/CLAUDE.md`) |
| Config files | project files, recorded by digest | also your user config, vendored |
| Plugins and packages | in project config | also user-installed ones |
| Other model providers | in project config | also the rest of your user-level provider list |

### Pick parts of your user setup

To pin only some of your user-level setup, pass `-i` (`--include-user-scope`) with a glob,
as many times as you need. Relative patterns are relative to the harness's config root, so
`skills/review-*` means `~/.claude/skills/review-*` for Claude Code and
`~/.pi/agent/skills/review-*` for Pi:

```console
$ hlock pin -i 'skills/review-*' -i CLAUDE.md
✓ pinned claude-code 2.1.0 → harness.lock  (the only installed harness)
╭─ 📌 harness.lock · pinned ────────────────────────────────────╮
│                                                               │
│  harness        claude-code 2.1.0                             │
│  scope          project + user  (skills/review-*, CLAUDE.md)  │
│  llm · default  anthropic:opus                                │
│  mcp            linear (http)                                 │
│  skills         2  (1 local, 1 first-party)                   │
│  instructions   CLAUDE.md, {root}/CLAUDE.md                   │
│  config         .mcp.json                                     │
│                                                               │
╰───────────────────────────────────────────────────────────────╯
vendored 2 files into .hlock/vendor/claude-code/  · review before committing
left out user-level: 1 skill, 1 MCP server, 2 config files
  to pin it too: hlock pin --user, or pick with -i GLOB
next → hlock check  ·  hlock sync
```

- `*` and `?` match within one path segment; `**` matches across directories.
- A pattern that matches a directory takes everything under it: `-i skills` pins every
  user-level skill.
- `~/…` and absolute paths work too, for files outside the config root, like
  `~/.agents/skills/*`.
- MCP servers, plugins and model providers are picked by the file that defines them:
  `-i '~/.claude.json'` pins Claude Code's user-level MCP servers, along with the
  `/mcpServers` part of that file.
- A pattern that matches nothing is reported as a warning.
- `-i` and `--user` don't combine, since `--user` already pins everything.

Quote patterns, so your shell doesn't expand them first.

### What check and re-pinning do with the scope

The lock records the scope in `[provenance] scopes`, and any `-i` patterns in
`[provenance] user-include`. `hlock check` re-captures the same selection: a project-only
lock doesn't drift when you add a personal skill, and neither does a lock pinned with
`-i 'skills/review-*'` when you edit a skill it didn't pick. The drift hint repeats the
flags the lock was pinned with.

Re-pinning doesn't remember `--user` or `-i`: `hlock pin` always means the project only.
If the previous pin's user scope differs, `pin` warns with the flags that would keep it,
and the diff lists what was dropped. Locks pinned before scopes existed captured
everything, and `check` treats them that way.

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

With `--user`, some of what's pinned can't be fetched by another machine: hand-written
skills, user-level instruction files, and your config files. `pin` copies them, with
secrets already redacted, into
`.hlock/vendor/<harness>/` next to the lock, so the lock plus that directory is
self-contained. A lock named `harness.<name>.lock` or `<name>.lock` vendors into
`.hlock/vendor/<name>/<harness>/` instead, so two locks never share vendored files. Commit both. Review the vendored files first, because a skill you wrote
yourself is copied as it is. Pass `--no-vendor` to record digests only. A project-only pin
vendors nothing: project files are already in the repository, so the lock records their
digests.

`pin` builds the new vendor directory beside the old one and swaps it in, so a pin that
fails leaves the previous files as they were. Symlinks inside a skill aren't followed: they
aren't vendored or digested, and `pin` warns about each one. A file it can't read fails that
skill's vendoring, with a warning, rather than being copied as an empty file. A skill whose
`name:` isn't a plain directory name (it has a `/` or `..`) is locked under its directory's
name. Two vendored files with the same name (two `AGENTS.md`) get `AGENTS.md` and
`AGENTS-2.md`.

A [declared dependency](dependencies.md) is never vendored: it has a source and a commit,
so `sync` fetches it. Re-pinning keeps what's declared as declared. When the installed copy
differs from its declared pin, `pin` keeps the pin and warns (`hlock sync` restores it;
`hlock add` accepts the new one). When the project has an `apm.lock.yaml`, the skills
`apm install` deployed are recorded as dependencies of their package rather than as the
project's own files.

Source URLs carry no credentials: a skill's git remote or a marketplace URL is recorded
without its user name and password or token (`https://ghp_…@github.com/a/b` is locked as
`https://github.com/a/b`); an ssh user name (`git@github.com:a/b`) stays.

## Keep it honest with `check`

On a pinned lock, `hlock check` re-captures the harness, in the scope it was pinned with,
and fails if anything changed. Here, on a lock pinned with `--user`:

```console
$ hlock check
✗ drift: your claude-code setup changed since harness.lock was pinned
  ~ llm default: anthropic:opus → anthropic:sonnet
  ~ config {root}/settings.json: 016233bf39 → d5aaa91f9e
  hint: re-pin with hlock pin --user, or restore it with hlock sync --in-place
```

A config file's digest covers what `sync` restores from it, parsed and with secrets
redacted, so reformatting the file or using a different value for a secret isn't drift.

Running `hlock pin` again overwrites the pin and prints the same kind of diff under
"changes since the last pin". When a skill, instruction file or config file changed, it adds
`see the content: hlock diff`, which shows the text behind those digests (see
[Review lock changes](diff.md)).

## Pinned and resolved locks

A pinned lock has `kind = "pin"` and no evidence. A resolved lock, written by `hlock resolve`,
measures a search space against your tasks. `pin` won't overwrite a resolved lock: pass
`-o pinned.lock` to keep both, or `--force` to replace it.

`hlock show` and `hlock tree` render either kind.

## Next: rehydrate it

[`hlock sync`](sync.md) restores a pinned setup on another machine: into an isolated
directory (`.hlock/env/`, the default), into your own config (`--in-place`), or into a
Docker image.
