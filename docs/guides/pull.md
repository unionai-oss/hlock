# Pulling someone else's lock

`hlock pull` gets a pinned lock from someone else's git repository, so you can try their
agent setup with [`hlock launch`](launch.md) or adopt it with [`hlock sync`](sync.md). Say a
teammate pinned their Claude Code setup with [`hlock pin --user`](pin.md#project-and-user-scope) and pushed
`harness.lock` and `.hlock/` to `github.com/alice/agent-setup`:

```console
$ hlock pull github:alice/agent-setup
✓ pulled github:alice/agent-setup @ b259f53 → alice
╭─ 📌 alice · pinned ────────────────────────────────────────────────────╮
│                                                                        │
│  harness        claude-code 2.1.285                                    │
│  scope          project + user                                         │
│  llm · default  anthropic:opus                                         │
│  mcp            github (stdio)                                         │
│  skills         1  (1 local)                                           │
│  config         {root}/settings.json, ~/.claude.json /mcpServers       │
│  secrets        1 redacted  (supplied as HLOCK_SECRET_* when syncing)  │
│  pulled         github:alice/agent-setup @ b259f53                     │
│                                                                        │
╰────────────────────────────────────────────────────────────────────────╯
! starts MCP servers with: npx  · review before syncing into your own config
also in the repo: harness.fast.lock  → hlock pull github:alice/agent-setup/harness.fast.lock
next → hlock launch --lock alice  ·  hlock sync alice --in-place --plan
```

The lock and the files it vendors go into hlock's store, `~/.hlock/pulls/alice/`, laid out
like the repository they came from. Nothing is written to your project, so you can use
`alice` from any directory. The name is the repository owner by default (`alice`, or
`alice-fast` for a remote `harness.fast.lock`); pass `--as NAME` to choose another.

Pulling runs nothing from the lock. The summary shows what's in it, and the warning lists
the programs its MCP servers start, because those run as soon as the harness does. Before it
writes anything, `pull` scans the files the lock vendors for
[characters that hide text from a reviewer](audit.md#hidden-characters), and checks the lock
against your project's [policy](audit.md#policy). When you sync or launch it, each MCP server,
plugin, hook and skill script needs your [approval on this machine](audit.md#executables)
first.

## Try it, or adopt it

Anywhere hlock takes a lock, a pulled lock's name works too. To try the setup without
touching your own config, launch it. `launch` restores the lock into an isolated config root,
`~/.hlock/env/alice/`, and starts the harness there in your current directory:

```bash
hlock launch --lock alice
```

`hlock sync alice` restores into that same root without starting anything, as `sync` does
for any lock (it's [isolated by default](sync.md)). `sync` and `launch` also take a source
directly and pull it first:

```console
$ hlock sync github:alice/agent-setup --plan
✓ pulled github:alice/agent-setup @ b259f53 → alice
! starts MCP servers with: npx
Sync plan for Claude Code 2.1.285 → ~/.hlock/env/alice/claude-code/config
✓  harness  Claude Code 2.1.285
✓  mcp      `npx` for MCP servers: github
●  config   Write ~/.hlock/env/alice/claude-code/config/settings.json
●  config   Write ~/.hlock/env/alice/claude-code/config/.claude.json /mcpServers
●  skill    Copy skill review → ~/.hlock/env/alice/claude-code/config/skills/review
○  secret   HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)  not set
```

To make it your own setup, sync it into your config with `--in-place`. Look at the plan
first:

```console
$ hlock sync alice --in-place --plan
Sync plan for Claude Code 2.1.285 → your config ~/.claude
✓  harness  Claude Code 2.1.285
✓  mcp      `npx` for MCP servers: github
●  config   Write ~/.claude/settings.json
●  config   Write ~/.claude.json /mcpServers
●  skill    Copy skill review → ~/.claude/skills/review
○  secret   HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)  not set
```

Secrets were never in the lock; supply your own values, from the environment, your keychain
(`hlock secret set`), or a password manager. See [Secrets](secrets.md).

## Manage what you've pulled

```console
$ hlock list
name   harness              source                    commit   pulled
alice  claude-code 2.1.285  github:alice/agent-setup  b259f53  0s ago
next → hlock launch --lock alice  ·  hlock pull (refresh all)
```

`hlock pull` with no source refreshes every pulled lock from where it came from;
`hlock pull alice` refreshes one. A refresh prints the commit change and what changed in the
setup, or says the lock is up to date:

```console
$ hlock pull
✓ alice is up to date with github:alice/agent-setup @ b259f53
```

A lock pulled at a tag or commit (`@v2`, `@b259f53…`) stays there; pull it again with another
ref to move it. Pulling replaces `alice` only if it came from the same repository; otherwise
`pull` stops and suggests `--as` (or `--force` to replace it).

`hlock rm alice` removes it, along with the isolated roots `launch` made for it.

## Into a project

To commit someone's lock alongside your project, for a team that wants to share it, write it
into the project with `-o`:

```console
$ hlock pull github:alice/agent-setup -o harness.alice.lock
✓ pulled github:alice/agent-setup @ b259f53 → harness.alice.lock
╭─ 📌 harness.alice.lock · pinned ───────────────────────────────────────╮
│                                                                        │
│  harness        claude-code 2.1.285                                    │
│  scope          project + user                                         │
│  llm · default  anthropic:opus                                         │
│  mcp            github (stdio)                                         │
│  skills         1  (1 local)                                           │
│  config         {root}/settings.json, ~/.claude.json /mcpServers       │
│  secrets        1 redacted  (supplied as HLOCK_SECRET_* when syncing)  │
│  pulled         github:alice/agent-setup @ b259f53                     │
│                                                                        │
╰────────────────────────────────────────────────────────────────────────╯
copied 3 vendored entries into .hlock/vendor/alice/claude-code/
! starts MCP servers with: npx  · review before syncing into your own config
also in the repo: harness.fast.lock  → hlock pull github:alice/agent-setup/harness.fast.lock
next → hlock launch --lock harness.alice.lock  ·  hlock sync harness.alice.lock --in-place --plan
```

A named lock file gets its own vendor directory, so it never mixes with your
`harness.lock`. `hlock pull` with no source refreshes these too, and
`hlock pull harness.alice.lock` refreshes one. For a repository where several contributors
keep their locks, see [Team repositories](teams.md).

## Sources

| Source | Example |
|---|---|
| `github:OWNER/REPO[/PATH][@REF]` | `github:alice/agent-setup/harness.fast.lock@v2` |
| `gitlab:GROUP[/SUBGROUP…]/REPO[//PATH][@REF]` | `gitlab:acme/platform/agents//team@main` |
| `git:URL[//PATH][@REF]` | `git:git@github.com:alice/private-setup.git` |
| A URL from the browser | `https://github.com/alice/agent-setup/blob/main/harness.fast.lock` |

- `PATH` is a lock file or a directory to look in. On GitLab, where groups nest, it follows
  `//`.
- `REF` is a branch, tag, or full commit SHA; the default is the repository's default
  branch. A URL to a file (`…/blob/<ref>/<path>`) sets both.
- If the directory has `harness.lock`, that's the one pulled, and other `harness.*.lock`
  files are listed with the command that pulls each. With only named locks and more than
  one, `pull` lists them and stops.

hlock fetches with `git` (a shallow, sparse fetch of the lock and the files it vendors), so
private repositories work with whatever credentials `git` already uses: a credential helper,
an SSH key, or a `url.<base>.insteadOf` rewrite. For GitHub Enterprise or a self-hosted
GitLab, set `HLOCK_GITHUB_URL` or `HLOCK_GITLAB_URL` to its base URL. `HLOCK_HOME` moves the
store from `~/.hlock`.

A token in the URL (`https://user:TOKEN@host/…` or `https://TOKEN@host/…`) is used for the
fetch and nowhere else: hlock leaves it out of `[pulled]`, its messages, and `git`'s errors,
so a refresh (`hlock pull NAME`) fetches with your `git` credentials. An SSH URL keeps its
user name and loses a password.

## What hlock checks

Everything in a pulled lock came from someone else, so `pull` checks it before it writes
anything:

- The source: a `REF` or URL that starts with `-`, a URL with a transport helper
  (`ext::…`), and a `PATH` with `..` are refused. `git` runs with only the `file`, `git`,
  `http`, `https` and `ssh` transports.
- The lock: names and paths that would reach outside the vendor directory, and versions or
  model ids that sync would put in a command, are refused (see
  [checks on a lock](../reference/lockfile.md#checks)).
- Vendored files: a symlink in the repository is skipped with a warning, never followed,
  and every file is written inside the lock's vendor directory.
- A refresh checks each lock's `[pulled] from` the same way before fetching anything.

The vendored files are copied into a new directory that replaces the old one only once the
copy is complete, so a failed pull leaves the previous files in place.

## Where it came from

A pulled lock records its origin in a `[pulled]` table, and `hlock --lock alice show` prints
it in the summary:

```toml
[pulled]
at = "2026-10-18T21:08:32Z"
commit = "b259f53b541fc06018152866f5482da6f9002f67"
from = "github:alice/agent-setup"
path = "harness.lock"
url = "https://github.com/alice/agent-setup.git"
```

## Reference

```bash
hlock pull SOURCE [--as NAME | -o FILE] [--force] [--json]
hlock pull [NAME | LOCK ...] [--json]
hlock list [--json]
hlock rm NAME...
```

See the [CLI reference](../reference/cli.md#pull) for every option.
