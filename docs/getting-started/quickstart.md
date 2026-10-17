# Quickstart

You've set up your agent by hand: a harness, a model, a few MCP servers, some skills, an
instructions file. It works on your laptop. This walkthrough pins all of it into a
`harness.lock`, versions it in git, keeps it current as you add to your setup, brings it
back on another machine, and starts sessions from it. It takes about five minutes.

The examples use Claude Code. The same commands work with Codex, Pi, OpenCode, Cursor,
Gemini CLI, and the [other supported harnesses](../guides/harnesses.md).

## 1. Pin your setup

Run `hlock pin` in the repository where you want to keep the lock:

```console
$ hlock pin
✓ pinned claude-code 2.1.0 → harness.lock  (the only installed harness)
╭─ 📌 harness.lock · pinned ─────────────────────────────────────────────╮
│                                                                        │
│  harness        claude-code 2.1.0                                      │
│  llm · default  anthropic:opus                                         │
│  mcp            github (stdio)                                         │
│  skills         1  (1 local)                                           │
│  instructions   {root}/CLAUDE.md                                       │
│  config         {root}/settings.json, ~/.claude.json /mcpServers       │
│  secrets        1 redacted  (supplied as HLOCK_SECRET_* when syncing)  │
│                                                                        │
╰────────────────────────────────────────────────────────────────────────╯
vendored 4 files into .hlock/vendor/claude-code/  · review before committing
next → hlock check  ·  hlock sync
```

`pin` found the installed harness and recorded its version and install source, the model,
every MCP server and the package it launches, skills, plugins, instruction files, and the
config files they came from, each with a digest.

Secrets never reach the lock. The GitHub token in the MCP server's config became a
placeholder:

```toml title="harness.lock"
[tool.env]
GITHUB_TOKEN = "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
```

Things another machine can't download, like a skill you wrote yourself, your
`CLAUDE.md`, and your config files, are copied (with secrets redacted) into
`.hlock/vendor/`. See [Pinning your setup](../guides/pin.md) for everything `pin` records.

## 2. Commit it

```bash
git add harness.lock .hlock/
git commit -m "Pin my agent setup"
```

Like `uv.lock` or `Cargo.lock`, `harness.lock` belongs in version control.

## 3. Add to your setup, and lock what's new

Setups grow by hand: you install a skill, connect another MCP server, edit a setting.
`hlock check` re-captures your setup and shows what changed since the pin:

```console
$ hlock check
✗ drift: your claude-code setup changed since harness.lock was pinned
  + mcp linear
  + skill write-tests (user)
  ~ config ~/.claude.json: ad825f266f → b33482f12e
  hint: re-pin with hlock pin, or restore it with hlock sync
```

Run `hlock pin` again to lock the new pieces. It prints the same diff, and the change
becomes a reviewable diff of `harness.lock` in git:

```console
$ hlock pin
✓ pinned claude-code 2.1.0 → harness.lock  (the only installed harness)
╭─ 📌 harness.lock · pinned ─────────────────────────────────────────────╮
│                                                                        │
│  harness        claude-code 2.1.0                                      │
│  llm · default  anthropic:opus                                         │
│  mcp            github (stdio), linear (http)                          │
│  skills         2  (2 local)                                           │
│  instructions   {root}/CLAUDE.md                                       │
│  config         {root}/settings.json, ~/.claude.json /mcpServers       │
│  secrets        1 redacted  (supplied as HLOCK_SECRET_* when syncing)  │
│                                                                        │
╰────────────────────────────────────────────────────────────────────────╯
vendored 5 files into .hlock/vendor/claude-code/  · review before committing
changes since the last pin
  + mcp linear
  + skill write-tests (user)
  ~ config ~/.claude.json: ad825f266f → b33482f12e
next → hlock check  ·  hlock sync
```

If the change wasn't intended, `hlock sync` restores the locked setup instead.

## 4. Rehydrate it elsewhere

On a new laptop, a VM, or a teammate's machine, clone the repository, set the secrets the
lock asks for, and run `hlock sync`:

```console
$ export HLOCK_SECRET_GITHUB_GITHUB_TOKEN=ghp_…
$ hlock sync --yes
Sync plan for Claude Code 2.1.0 → your config
✓  harness       Claude Code 2.1.0
✓  mcp           `npx` for MCP servers: github
✓  secret        HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)
●  config        Write ~/.claude/settings.json
●  config        Write ~/.claude.json /mcpServers
●  skill         Copy skill review-pr → ~/.claude/skills/review-pr
●  skill         Copy skill write-tests → ~/.claude/skills/write-tests
●  instructions  Write ~/.claude/CLAUDE.md
✓ Write ~/.claude/settings.json
✓ Write ~/.claude.json /mcpServers
✓ Copy skill review-pr → ~/.claude/skills/review-pr
✓ Copy skill write-tests → ~/.claude/skills/write-tests
✓ Write ~/.claude/CLAUDE.md
✓ in sync with harness.lock

$ hlock check
✓ your claude-code setup matches harness.lock
```

`sync` builds a plan: `✓` steps are already true, `●` steps hlock does for you, and `○`
steps need you, with the exact command to run. It keeps a backup of any file it replaces.
Pass `--install` to let it install missing harnesses and packages, and `--pull` to
download local model weights. Without `--yes`, it shows the plan and asks first.
`hlock sync --docker` writes a Docker image with the same setup instead.

## 5. Launch sessions and run prompts

`hlock launch` starts the harness on the locked setup, in an isolated config directory
under `.hlock/env/`, the way `uv run` uses a virtual environment. Your own `~/.claude`
isn't touched, and nothing you haven't locked leaks in:

```console
$ hlock launch
✓ Write .hlock/env/claude-code/config/settings.json
✓ Write .hlock/env/claude-code/config/.claude.json /mcpServers
✓ Copy skill review-pr → .hlock/env/claude-code/config/skills/review-pr
✓ Copy skill write-tests → .hlock/env/claude-code/config/skills/write-tests
✓ Write .hlock/env/claude-code/config/CLAUDE.md
▶ Claude Code  CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config …/bin/claude
```

Arguments after the harness name go to the harness, so a one-off prompt is the harness's
own non-interactive mode. The environment is only rebuilt when the lock changes:

```console
$ hlock launch claude -p "summarize README.md"
▶ Claude Code  CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config …/bin/claude -p summarize README.md
```

You can also launch the lock in a different harness: `hlock launch opencode` carries the
MCP servers, skills, and instructions over. See [Launching a locked harness](../guides/launch.md).

## Where to next

Your setup is pinned, versioned, and portable. From here, two paths:

**Optimize your harness.** Measure your setup against your own tasks and let hlock find a
better one:

1. Start an [eval pack](../guides/eval-packs.md) with `hlock evals init -t <template>`.
   It writes an Agent Skill, so the agent you already use can fill the pack in with real
   work from your project: git history, merged pull requests, closed issues, tickets.
2. List the models, decision models, and harnesses to try in a `harness.toml`, and
   [resolve](../guides/resolve.md) the best combination.
3. Go below the stack with [`hlock tune`](../guides/tune.md): prompts,
   hyperparameters, and fine-tuned small models.

**Go local.** [`hlock localize`](../guides/localize.md) turns your lock into an equivalent
that runs entirely on your machine, with an open-source harness and local models. Give it
an eval pack, and it measures the local candidates against your tasks and records the
evidence next to your original lock's.
