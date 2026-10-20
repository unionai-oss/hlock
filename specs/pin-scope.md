# Pin scope: project by default, user with `--user`

Status: implemented · Owner: Niels Bantilan · Last updated: 2026-10-20 · Builds on [v0](v0.md) §7.8.4

> `hlock pin` records the project's agent setup. `hlock pin --user` adds the setup in your
> home directory, and `hlock pin -i GLOB` adds only the parts of it you pick.

---

## 1. Problem

`hlock pin` captured everything the harness reads: the project's skills, MCP servers,
instructions and config, *and* everything in the user's config root (`~/.claude`,
`~/.pi/agent`, …). User-level content another machine can't fetch was vendored next to the
lock.

That's right for "carry my setup to a new laptop", and wrong for a lock committed to a
shared repository. Committing a contributor's lock to `.hlock/locks/` (the
[team layout](../docs/guides/teams.md)) vendored about 290 files of personal skills from one
person's `~/.pi` and `~/.claude` into the repository: unrelated to the project, noisy in
review, and a leak risk for anything a skill or instruction file says about its author.

## 2. Design

### 2.1 Two scopes

Every item a profile reads is classified by where it lives:

- **project**: under the working directory and outside the harness's config root
  (`.claude/skills/`, `.mcp.json`, `CLAUDE.md`, `.pi/settings.json`, …);
- **user**: everything else, which in practice means the config root in the home
  directory, plus files like `~/.claude.json` and `~/.agents/skills`.

`hlock pin` captures project scope. `hlock pin --user` captures both. `hlock pin -i GLOB`
(`--include-user-scope`, repeatable) captures the project plus the user-level items whose
path matches a pattern (§2.7). There's no user-only mode: a project's own setup is always
part of what an agent does there.

### 2.2 What ignores scope

Some entries describe what the stack *is*, not configuration someone wrote, so they're
captured in either scope, wherever they're set:

- the harness version and install source, and a code harness's packages;
- the default and small models, their endpoints, and the runtimes serving them;
- decision models (native settings and `[deciders.*]`);
- skills built into the harness, which come with its version.

Without these a project lock would say nothing about which model it runs, so the model
in `~/.claude/settings.json` is still recorded (`resolved-from = "settings#/model"`). The
file it came from is not.

### 2.3 What follows scope

Skills, MCP servers, plugins and packages, instruction files, config files, and the rest of
a user-level model-provider list. Claude Code's "local" MCP servers, stored in
`~/.claude.json` under `projects.<cwd>.mcpServers`, are user scope: they're private to the
user and not in the repository. Before this change they were labeled `project`.

A project-only capture vendors nothing: project files are already in the repository, so
the lock records their digests (`restore = "record"` for config).

### 2.4 Recording the scope

The lock records it as `[provenance] scopes = ["project"]` or `["project", "user"]`, plus
`[provenance] user-include = [...]` when `-i` picked the user-level items.
`hlock check` re-captures exactly that selection, so a project lock doesn't drift when the
user adds a personal skill, and a `--user` lock still notices. Its hint names the matching
re-pin command (`hlock pin --user`, or the lock's `-i` patterns, shell-quoted).

Locks pinned before this change have no `scopes`. They captured both, so `check` treats
a missing `scopes` as `["project", "user"]` and never reports false drift on an existing
lock.

### 2.5 Re-pinning

The scope doesn't carry over: `hlock pin` always means project scope, like any other CLI
flag. Re-pinning with a different user scope than the previous pin is allowed, but `pin`
warns with the flags that would keep it ("the previous pin included user-level setup; pass
-i 'skills/review-*' to keep it") and the diff lists every dropped item. `hlock meta run update --pin` re-pins with the scope the lock already has.

The meta harness always pins user scope, since its isolated config root is user scope by
construction.

### 2.6 Telling the user what was left out

A project-only pin counts what it skipped, by kind, and says so:

```console
left out user-level: 1 skill, 1 MCP server, 1 instruction file, 2 config files
  to pin it too: hlock pin --user
```

`--json` returns the same counts as `skipped`.

### 2.7 Picking user-level items with `-i`

`-i PATTERN` selects user-level items by path, so it works the same for every kind and
every harness:

- **Relative to the config root.** `skills/review-*` is `~/.claude/skills/review-*` for
  Claude Code and `~/.pi/agent/skills/review-*` for Pi, so the same flags work across
  harnesses. `{root}/…`, `~/…` and absolute patterns are accepted for files outside the
  root (`~/.agents/skills/*`, `~/.claude.json`).
- **Globs.** `*` and `?` stay within a path segment, `**` crosses segments, and `**/` also
  matches zero directories. The matcher is a few lines in the core rather than a new
  dependency.
- **Directories take their contents.** An item is picked when the pattern matches its path
  or any parent, so `-i skills` picks every skill.
- **Items inside a file are picked by the file.** MCP servers, plugins and providers live in
  config files (`~/.claude.json`, `installed_plugins.json`, `models.json`), so a pattern
  matching the file picks them, along with the file's config entry. Selecting single MCP
  servers by name would need a second pattern language; it can be added if people ask.
- **Feedback.** A pattern that matches nothing is a warning, and the summary still counts
  what was left out.
- **`--user` and `-i` don't combine** (a usage error): `--user` already includes every
  pattern.

Patterns are resolved against the config root of the context being captured, so
`hlock check --prefix` on an isolated root matches the same relative patterns there.
`~/…` and absolute patterns don't move with the root, which is a reason to prefer relative
ones.

## 3. Alternatives considered

- **Sticky scope** (re-pin keeps the previous lock's scope). Rejected in review. Convenient
  for a personal lock, but a plain `hlock pin` would then mean different things in different
  repositories, and every lock written before this change would keep vendoring user files,
  which is the behavior this fixes. Explicit flags plus a warning are easier to reason about.
- **`--scope project|user|all`**. More general, but user-only has no use (§2.1); the real
  choices are none, all, or some, which `--user` and `-i` cover.
- **Exclude patterns** (`--exclude-user-scope`). Picking what to include is the safer
  default for a lock that gets committed: a new personal skill never slips in unnoticed.
- **Dropping the model when it's set in user config.** A lock that doesn't know its model
  isn't useful, and the model name isn't personal content.
- **Detecting the right default** (user scope outside a git repository, project inside).
  Guessing makes the same command produce different locks; the summary line makes the
  choice visible instead.

## 4. Follow-ups

- Claude Code's profile reads the model only from user settings (`settings#/model`). A
  model set in `.claude/settings.json` should take precedence, as it does in Claude Code.
- `hlock sync` of a project-only lock restores only project-scoped content; the harness's
  login and personal settings in the isolated root are still the user's to provide.
