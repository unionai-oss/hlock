# hlock pull: use someone else's lock

Status: implemented · Owner: Niels Bantilan · Last updated: 2026-10-18 · Builds on [v0](v0.md)

> `hlock pin` turns your setup into a file. `hlock pull` gets that file from someone else's
> repository, so you can try their setup with `hlock launch` or adopt it with `hlock sync`.

---

## 1. Summary

People already share agent setups by pasting dotfiles, `settings.json` snippets and lists
of MCP servers into READMEs. A pinned `harness.lock` plus its `.hlock/vendor/` directory is a
better unit to share: it's complete, it has no secrets, and `sync` knows how to rehydrate it.
What's missing is the step that gets it from a repository onto your machine.

```bash
hlock pull github:alice/agent-setup          # → ~/.hlock/pulls/alice/
hlock launch --lock alice                    # try it in an isolated config root, from any project
hlock sync alice                             # or adopt it into your own config
hlock list                                   # what you've pulled
hlock pull                                   # later: refresh every pulled lock
hlock rm alice

hlock sync github:alice/agent-setup          # pull and sync in one step
hlock pull github:alice/agent-setup -o harness.alice.lock   # into a project, to commit it
```

## 2. Goals and non-goals

Goals:

- One command from "I saw a setup I like" to a lock that `sync` and `launch` can use.
- Never overwrite your own `harness.lock`, never mix the pulled lock's vendored files with
  yours, and by default don't write into your project at all.
- Pulling runs nothing from the lock. You see what the lock would install and run before
  any `sync` changes your machine.
- A pulled lock records where it came from and the exact commit, so it's reproducible and
  can be refreshed.
- Work with public and private repositories, GitHub, GitLab (including subgroups and
  self-hosted instances), and any other git remote.

Non-goals (for now):

- A registry or search ("hub") of setups. `pull` takes a location, not a name.
- Signing or verifying who wrote a lock. Trust comes from the repository you pull from.
- Merging someone else's lock into your own. `hlock localize` and `hlock pin` already cover
  adapting a lock; `pull` only fetches.

## 3. Design iterations

### 3.1 Iteration 1: download a file

```bash
hlock pull github:alice/agent-setup     # writes ./harness.lock
```

The obvious version fetches `harness.lock` over HTTPS (`raw.githubusercontent.com`) into the
working directory. Trying it against real pinned locks shows four problems:

1. **It clobbers your lock.** Most projects that use hlock already have a `harness.lock`.
2. **The lock isn't self-contained without its vendor directory.** A pinned lock's
   hand-written skills, user-level instruction files and redacted config live in
   `.hlock/vendor/<harness>/` next to it, referenced by relative paths in the lock
   (`vendored = ".hlock/vendor/claude-code/skills/my-skill"`). Without them `sync` can
   restore almost nothing. A raw-file download can't fetch a directory.
3. **Private repos and GitLab** need per-host APIs and tokens.
4. **No provenance.** Once downloaded, nothing says which commit the lock came from, so you
   can't refresh it or tell whether upstream changed.

### 3.2 Iteration 2: fetch with git, write a named lock

Fixes, each reusing something hlock already has:

- **Fetch with `git`**: a shallow, sparse fetch of only `harness*.lock` and the paths the lock
  vendors. Git already handles private repos (credential helpers, SSH keys, `insteadOf`
  rewrites), any host, branches, tags and commits, and reports the resolved commit. Every
  hlock user has git.
- **Write a named lock.** hlock already gives `harness.<name>.lock` its own vendor namespace
  (`.hlock/vendor/<name>/<harness>/`), so two locks in one directory never share vendored
  files. `pull` writes `harness.<name>.lock` and relocates the vendored paths into that
  namespace. Your `harness.lock` and `.hlock/vendor/<harness>/` are never touched.
- **Record provenance in the lock** as a `[pulled]` table: the source as typed, the git URL,
  the requested ref, the resolved commit, the path in the repo, and when it was pulled.
  Keeping it in the lock (not a sidecar file) means the lock describes itself wherever it's
  copied, and `hlock show` can print it.

The default name is what makes the next command readable. Candidates:

| Default for `github:alice/agent-setup` | Next command | Problem |
|---|---|---|
| `harness.agent-setup.lock` (repo) | `hlock sync harness.agent-setup.lock` | Setup repos have generic names (`dotfiles`, `agents`, `agent-config`), so two people's locks collide |
| `harness.alice-agent-setup.lock` | `hlock sync harness.alice-agent-setup.lock` | Unique but long to type |
| `harness.alice.lock` (owner) | `hlock sync harness.alice.lock` | Collides only when you pull two repos from one owner |

Owner wins: the question a pulled lock answers is "whose setup is this", and a collision is
detected, not silent (§4.4). When the repo's lock is itself named (`harness.fast.lock`), the
default is `alice-fast`. `--as NAME` overrides it.

### 3.3 Iteration 3: make the common paths one command

Walking through the flows people actually want surfaced three more changes.

**"I just want to try it."** The safest way to try a setup is `hlock launch`, which restores
the lock into an isolated config root (`.hlock/env/`) and starts the harness there, without
touching your own config. So after a pull, the first suggested next step is `launch`, and
`sync` second.

**"Pull, then sync" is always two commands.** `sync` and `launch` accept a source wherever
they accept a lock path, and pull it first:

```bash
hlock sync github:alice/agent-setup --prefix .hlock/env
hlock launch --lock github:alice/agent-setup
```

This is sugar over `pull`: the lock still lands in `harness.alice.lock`, so the next run
works offline and `hlock pull` refreshes it.

**Copying a URL from the browser.** People find setups on the web, so `pull` also takes the
URLs a browser shows: `https://github.com/alice/agent-setup`, or the URL of the lock file
itself (`…/blob/main/harness.fast.lock`), which selects both the ref and the file.

**Repos with several locks.** If the repo has `harness.lock`, that's the one pulled, and
`pull` lists the others with the exact command to pull each. If it has only named locks and
more than one, `pull` stops and lists them. No interactive picker: the output is a command
you can copy, and the behavior is the same in CI.

**Refreshing.** `hlock pull` with no source refreshes every pulled lock in the project from
its recorded source; `hlock pull harness.alice.lock` refreshes one. A refresh prints the
commit change and the same component diff `hlock pin` prints when you re-pin.

### 3.4 Iteration 4: a global store

Iterations 2 and 3 wrote pulled locks into the project, because `sync` and `launch` work
against the project. Using it showed that this was the wrong home:

- **Trying a setup isn't about a project.** Once you've pulled alice's setup you want to
  launch it from whichever repo you're working in, not re-pull it per project.
- **It dirties the repo.** `harness.alice.lock` and `.hlock/vendor/alice/` showed up in
  `git status`, one `git add -A` away from being committed for the whole team.
- **There was nothing to list.** Pulled locks were scattered across projects, so "what have
  I pulled?" and "remove that one" had no natural answer.

This is the `docker pull` / `uv tool install` model: a pulled setup is a personal artifact in
a per-user store, used by name. So pulls go to `~/.hlock/pulls/<name>/` by default:

```
~/.hlock/pulls/alice/harness.lock
~/.hlock/pulls/alice/.hlock/vendor/claude-code/…
~/.hlock/env/alice/…                 # launch's isolated roots for it
```

Each entry is laid out like the repository it came from (a `harness.lock` with its vendor
directory next to it), so `sync` and `launch` read it with no special casing, and a remote
`harness.fast.lock`'s vendored paths are normalized to the plain layout.

- **Names resolve everywhere a lock does.** `sync NAME`, `launch --lock NAME`, and the
  global `--lock NAME` (`hlock --lock alice show`) look in the store when no file by that
  name exists. An existing path always wins, so nothing that worked before changes meaning.
  `launch` keeps `--lock NAME` rather than `launch NAME`, because its positional argument is
  already a harness (`hlock launch pi`) and a pulled lock named `pi` would be ambiguous.
- **Launch environments live with the lock.** `launch --lock alice` defaults its prefix to
  `~/.hlock/env/alice/`, so the isolated root is reused across projects and nothing lands in
  the project's `.hlock/env/`. The session still starts in the current directory.
- **`hlock list` and `hlock rm`.** `list` shows name, harness, source, commit and age; `rm`
  deletes the entry and its launch environments. A bare `list` is generic next to
  `harness list` and `runtime list`, but "my pulled setups" is the obvious reading.
- **Project-local stays, as a flag.** A team that wants a shared lock committed in the repo
  uses `-o harness.alice.lock`, which behaves as in iteration 2. `hlock pull` with no
  arguments refreshes both kinds.
- **Location.** `~/.hlock`, matching the project's `.hlock/`, with `HLOCK_HOME` to move it.
  XDG directories are more correct on Linux, but `~/.hlock` is easier to find when you want
  to look inside or delete it.

### 3.5 Rejected alternatives

- **An interactive picker for multi-lock repos.** Hard to script and hides the command. A
  listed command is just as fast to use.
- **Project-local by default.** See iteration 4.
- **A sidecar file (`.hlock/sources.toml`) for provenance.** Gets separated from the lock
  when the lock is copied, and is a second file to keep in sync.
- **HTTP APIs per host.** More code, a token story per host, and no directory fetch. Git does
  all of it.
- **`hlock add` / `hlock use`.** `add` suggests merging into your lock. `use` suggests
  switching your config, which is what `sync` does. `pull` matches the mental model of
  `git pull` and `docker pull`: get an artifact someone else published. The existing
  `hlock sync --pull` flag (download model weights) is a different, narrower meaning, but it
  only appears as a flag of `sync`.
- **Running `sync` automatically after `pull`.** Pulling should be safe to do on a whim.
  Writing to your config is a separate, visible step (or an explicit `hlock sync <source>`).

## 4. Specification

### 4.1 Commands

```bash
hlock pull SOURCE [--as NAME | -o FILE] [--force] [--json]
hlock pull [NAME | LOCK ...] [--json]     # refresh pulled locks
hlock list [--json]
hlock rm NAME...
hlock sync SOURCE|NAME [sync options]     # pull if needed, then sync
hlock launch --lock SOURCE|NAME [...]     # pull if needed, then launch
```

An argument is a source if it starts with `github:`, `gitlab:`, `git:`, `https://` or
`http://`. A lock argument resolves in order: an existing file, a source (pulled into the
store first), a name in the store.

### 4.2 Sources

| Form | Example |
|---|---|
| `github:OWNER/REPO[/PATH][@REF]` | `github:alice/agent-setup`, `github:alice/agent-setup/harness.fast.lock@v2` |
| `gitlab:GROUP[/SUBGROUP…]/REPO[//PATH][@REF]` | `gitlab:acme/platform/agents//team@main` |
| `git:URL[//PATH][@REF]` | `git:git@github.com:alice/private-setup.git`, `git:https://git.example.com/a/b.git@v1` |
| A GitHub URL | `https://github.com/alice/agent-setup/blob/main/harness.fast.lock` |
| A GitLab URL | `https://gitlab.com/acme/platform/agents/-/tree/main/team` |

- `PATH` is a lock file (ends in `.lock`) or a directory to look in. On GitHub the repo is
  always `OWNER/REPO`, so anything after it is the path; on GitLab, where groups nest, the
  path follows `//`.
- `REF` is a branch, tag, or full commit SHA. The default is the remote's default branch.
  For `git:` sources the ref follows the last `@` after the last `/` and `:`, so SSH remotes
  (`git@host:…`) parse as expected.
- `HLOCK_GITHUB_URL` and `HLOCK_GITLAB_URL` set the base URL for GitHub Enterprise and
  self-hosted GitLab (defaults `https://github.com` and `https://gitlab.com`). URLs on those
  hosts are recognized too. Other `https://` URLs are treated as git remotes.

### 4.3 Fetching

In a temporary directory:

```bash
git init -q
git remote add origin <url>
git fetch -q --depth 1 --filter=blob:none origin <ref or HEAD>
git sparse-checkout set --no-cone /<path>/harness.lock /<path>/harness.*.lock
git checkout -q FETCH_HEAD
# read the lock, then:
git sparse-checkout add /<path>/<each vendored path>
```

Only the lock files and the paths the chosen lock vendors are downloaded. Git runs with
`GIT_TERMINAL_PROMPT=0`, so a private repo without credentials fails fast with a hint
instead of hanging on a password prompt.

The lock is parsed with the normal loader, so an invalid lock or an unsupported
`lock-version` fails the pull before anything is written.

### 4.4 Writing

By default the lock goes into the store as `$HLOCK_HOME/pulls/<name>/harness.lock`
(`HLOCK_HOME` defaults to `~/.hlock`). `NAME` is `--as`, else the owner (GitHub owner,
GitLab top-level group, or the repo name for `git:` sources) plus `-<x>` when the remote lock
is `harness.<x>.lock`, lowercased and limited to `[a-z0-9._-]`. With `-o FILE` it goes into
the project as `FILE` instead.

- Each `vendored` path in the lock (skills, instructions, config) is copied from the
  checkout and rewritten in the lock: to `.hlock/vendor/<harness>/…` next to a stored lock,
  or into `FILE`'s vendor namespace (`.hlock/vendor/<name>/<harness>/…` for
  `harness.<name>.lock`). The previous contents of that vendor directory are removed first.
  A vendored path that is absolute or contains `..` fails the pull. A vendored path missing
  from the repo (the author didn't commit `.hlock/`) is dropped from the lock with a warning;
  `sync` then marks that item manual.
- The lock gets a `[pulled]` table:

  ```toml
  [pulled]
  at = "2026-10-18T21:08:32Z"
  commit = "b259f53b541fc06018152866f5482da6f9002f67"
  from = "github:alice/agent-setup"
  path = "harness.lock"
  url = "https://github.com/alice/agent-setup.git"
  ```

  plus `ref` when one was given.
- If the target already exists, `pull` replaces it only if its `[pulled]` URL is the same
  repository (an update), or with `--force`. Otherwise it fails and suggests `--as` (or
  another `-o FILE`).

Nothing else is written, and nothing from the lock is run. `launch` with a stored lock
defaults its prefix to `$HLOCK_HOME/env/<name>/`; `rm` deletes `pulls/<name>/` and
`env/<name>/`.

### 4.5 Output

```console
$ hlock pull github:alice/agent-setup
✓ pulled github:alice/agent-setup @ b259f53 → alice
╭─ 📌 alice · pinned ─────────────────────────────────╮
│  harness   claude-code 2.1.285                      │
│  …                                                  │
│  pulled    github:alice/agent-setup @ b259f53       │
╰─────────────────────────────────────────────────────╯
! starts MCP servers with: npx  · review before syncing into your own config
also in the repo: harness.fast.lock  → hlock pull github:alice/agent-setup/harness.fast.lock
next → hlock launch --lock alice  ·  hlock sync alice --plan

$ hlock list
name   harness              source                    commit   pulled
alice  claude-code 2.1.285  github:alice/agent-setup  b259f53  2h ago
```

The commands warning lists the distinct executables the lock's stdio MCP servers start,
because those run as soon as the harness starts. On a refresh, `pull` prints
`abc1234 → b259f53` and the component diff, or "up to date". With `-o`, it also says where
it copied the vendored files.

`--json` prints `{lock, path, name, stored, source, commit, previous_commit, vendored,
others, warnings, diff}`, where each of `others` is `{path, source}`.

### 4.6 Python API

```python
harness_lock.pull(source, out=None, name=None, force=False) -> str  # JSON, as --json
harness_lock.pull_find(name) -> str | None    # a stored lock's path
harness_lock.pull_list() -> str                # JSON, as hlock list --json
harness_lock.pull_remove(name) -> str
harness_lock.pull_env(name) -> str             # launch's prefix for a stored lock
harness_lock.is_pull_source(arg) -> bool
```

## 5. Tests

- Source parsing: every form in §4.2, refs with slashes, SSH remotes, `//` paths, bad input.
- End to end against a local git repository (`HLOCK_GITHUB_URL=file://…`, `HLOCK_HOME` in a
  temp directory): a pinned lock with vendored skills and config is pulled into the store and
  syncs by name from a project that stays untouched; `-o` into a project; `launch` keeps its
  environment in the store; `list` and `rm`; a repo with several locks; name collisions and
  `--force`; refresh after an upstream commit; `..` in a vendored path; `hlock sync <source>`.
