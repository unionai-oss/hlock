# Team repositories

A `harness.lock` pins one setup. In a shared repository, contributors often have their own:
one uses Claude Code with a few hand-written skills, another uses Codex with a different
model. This page describes a layout for committing all of them to the repository, so each
contributor's setup is versioned with the code and anyone can try anyone else's.

## Layout

Keep one lock per contributor in `.hlock/locks/`, named `harness.<handle>.lock`:

```text
your-repo/
├── harness.lock                    # optional: the project's shared setup
├── .hlock/
│   ├── vendor/                     # files harness.lock vendors
│   ├── locks/
│   │   ├── harness.alice.lock      # alice's setup
│   │   ├── harness.bob.lock        # bob's setup
│   │   └── .hlock/vendor/          # files those locks vendor
│   │       ├── alice/claude-code/
│   │       └── bob/codex/
│   └── env/                        # isolated roots from sync and launch (not committed)
└── .gitignore
```

The conventions:

- **One lock per contributor, named by handle.** Use your GitHub or GitLab username in
  lowercase, so `harness.alice.lock` is plainly alice's. For a second setup of your own, add
  a suffix: `harness.alice-local.lock`.
- **Keep the `harness.` prefix.** `hlock pull` only finds locks named `harness.lock` or
  `harness.<name>.lock`, and the name after `harness.` is the lock's vendor namespace, so
  two contributors' vendored files never collide.
- **The root `harness.lock` is optional.** Use it for the setup the project recommends,
  such as one the maintainers [resolved](resolve.md) against the repository's
  [eval pack](eval-packs.md). Contributors' own locks don't replace it.
- **Commit each lock with its vendored files.** A plain `hlock pin` records only the
  repository's setup and vendors nothing. With `--user`, it copies your hand-written skills
  and config into `.hlock/locks/.hlock/vendor/<handle>/`, next to the lock. Review them
  before committing, as you would for any [pin](pin.md#vendoring).
- **Don't commit `.hlock/env/`.** It's where `sync` and `launch` restore locks in
  isolation. Add it to `.gitignore`:

    ```gitignore title=".gitignore"
    .hlock/env/
    ```

- **Only re-pin your own lock.** A lock records the setup on the machine that pinned it, so
  only its owner can re-pin or [check](pin.md) it.

On GitHub, a `CODEOWNERS` file can make each contributor the reviewer for their own lock:

```text title=".github/CODEOWNERS"
/.hlock/locks/harness.alice.lock    @alice
/.hlock/locks/.hlock/vendor/alice/  @alice
/.hlock/locks/harness.bob.lock      @bob
/.hlock/locks/.hlock/vendor/bob/    @bob
```

## Pin your setup

From the repository root, pin into your lock with `-o`. If you have more than one harness
installed, pass `--harness`:

```console
$ hlock pin --harness claude-code -o .hlock/locks/harness.alice.lock
✓ pinned claude-code 2.1.0 → .hlock/locks/harness.alice.lock  (--harness)
╭─ 📌 .hlock/locks/harness.alice.lock · pinned ─╮
│                                               │
│  harness        claude-code 2.1.0             │
│  scope          project                       │
│  llm · default  anthropic:opus                │
│  mcp            linear (http)                 │
│  skills         1  (1 first-party)            │
│  instructions   CLAUDE.md                     │
│  config         .mcp.json                     │
│                                               │
╰───────────────────────────────────────────────╯
left out user-level: 1 skill, 1 MCP server, 1 instruction file, 2 config files
  to pin it too: hlock pin --user, or pick with -i GLOB
next → hlock check  ·  hlock sync
```

By default your lock records your harness and model, and the skills, MCP servers and
config the repository itself provides. Your personal setup in your home directory stays
out of the repository. To share it too, add `--user`, or pick parts of it with
`-i 'skills/review-*'`: hand-written skills and config are then copied into
`.hlock/locks/.hlock/vendor/alice/`, so review them before committing (see
[Project and user scope](pin.md#project-and-user-scope)).

Later commands take your lock with `--lock`. To check for drift, then re-pin:

```console
$ hlock --lock .hlock/locks/harness.alice.lock check
✗ drift: your claude-code setup changed since .hlock/locks/harness.alice.lock was pinned
  ~ llm default: anthropic:opus → anthropic:sonnet
  hint: re-pin with hlock pin, or restore it with hlock sync --in-place
$ hlock --lock .hlock/locks/harness.alice.lock pin --harness claude-code
```

To skip `--lock` on every command, set `HLOCK_LOCKFILE` in your shell. Relative paths are
resolved from the working directory, so give an absolute path:

```bash
export HLOCK_LOCKFILE="$PWD/.hlock/locks/harness.alice.lock"
```

## Try a teammate's setup

In a clone of the repository, any lock in `.hlock/locks/` works with `sync` and `launch`.
Both restore it into an isolated root under `.hlock/env/`, so your own config isn't touched:

```console
$ hlock --lock .hlock/locks/harness.alice.lock sync --plan
Sync plan for Claude Code 2.1.0 → .hlock/env/harness.alice/claude-code/config
✓  harness  Claude Code 2.1.0
●  config   Write .hlock/env/harness.alice/claude-code/config/settings.json
●  skill    Copy skill review-pr →
            .hlock/env/harness.alice/claude-code/config/skills/review-pr
```

```bash
hlock launch --lock .hlock/locks/harness.bob.lock
```

From outside the repository, [pull](pull.md) a lock by its path. Pulling the directory lists
the locks in it, with the command to pull each:

```bash
hlock pull github:acme/app/.hlock/locks                       # lists the locks to choose from
hlock pull github:acme/app/.hlock/locks/harness.bob.lock --as bob
hlock launch --lock bob
```

Without `--as`, a pulled lock is named after the repository owner and the lock, such as
`acme-bob`.

## Show it in your README

Add the [hlock badge](../project/badge.md) and link it to `.hlock/locks/`, so readers can
find every contributor's setup.
