# Rehydrating with sync

`hlock sync` turns a lock back into a working setup: on a new laptop, in a fresh VM, in an
isolated directory, or in a Docker image. It works on any lock, and gets the most out of one
written by [`hlock pin`](pin.md).

By default `sync` writes into an isolated config root in `.hlock/env/`, so your own
config (`~/.claude`, `~/.pi`, …) is never changed unless you pass
[`--in-place`](#into-your-own-config).

Installing everything automatically across package managers, operating systems, GUI apps and
model stores isn't realistic, so `sync` builds a **plan** in which every step is one of:

| | Status | Meaning |
|---|---|---|
| ✓ | satisfied | Already true here: the version or digest matches |
| ● | automatic | hlock does it: restore a config file, copy a skill, install a plugin through the harness's own CLI (`claude plugin install`, `pi install`) |
| ○ | manual | It needs you, and hlock prints the exact command or link and why |

## On a new machine

Clone the repo that holds `harness.lock` and `.hlock/`, then:

```console
$ hlock sync --yes
Sync plan for Claude Code 2.1.0 → .hlock/env/claude-code/config
✓  harness       Claude Code 2.1.0
✓  mcp           `npx` for MCP servers: github
●  config        Write .hlock/env/claude-code/config/settings.json
●  config        Write .hlock/env/claude-code/config/.claude.json /mcpServers
●  skill         Copy skill review-pr → .hlock/env/claude-code/config/skills/review-pr
●  skill         Copy skill write-tests → .hlock/env/claude-code/config/skills/write-tests
●  instructions  Write .hlock/env/claude-code/config/CLAUDE.md
○  secret        HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)  not set
✓ Write .hlock/env/claude-code/config/settings.json
✓ Write .hlock/env/claude-code/config/.claude.json /mcpServers  placeholders for missing secrets
✓ Copy skill review-pr → .hlock/env/claude-code/config/skills/review-pr
✓ Copy skill write-tests → .hlock/env/claude-code/config/skills/write-tests
✓ Write .hlock/env/claude-code/config/CLAUDE.md

1 step needs you:
  1. Store 1 secret in your keychain, then re-run hlock sync (values stay out of the lock)
     hlock secret set GITHUB_GITHUB_TOKEN  # mcp github GITHUB_TOKEN
     or export each variable, or reference it in ~/.config/hlock/secrets.toml
next → hlock sync --skill (let your agent do it)
```

Without `--yes`, `sync` shows the plan and asks before it runs the automatic steps. Set the
secret (here from the environment; see [Secrets](secrets.md) for the keychain and
password managers) and run it again. Re-running is always safe: satisfied steps are skipped, and `sync`
exits `3` while manual steps remain and `0` once everything matches.

```console
$ export HLOCK_SECRET_GITHUB_GITHUB_TOKEN=ghp_…
$ hlock sync --yes
…
✓ in sync at .hlock/env/claude-code/config  your own config wasn't touched
use it: CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config claude
to restore into your own config instead: hlock sync --in-place
```

Start the harness with that command, or with [`hlock launch`](launch.md), which uses the
same directory. `hlock check --prefix .hlock/env` confirms the setup there matches the pin:

```console
$ hlock check --prefix .hlock/env
✓ the claude-code setup in .hlock/env matches harness.lock
```

Like a `.venv`, the isolated root isn't for version control: it's restored from the lock,
and it holds filled-in secrets and the harness's own state. hlock writes a `.gitignore` of
`*` into each harness's directory there (`.hlock/env/claude-code/.gitignore`), so `git add`
skips it. Commit `harness.lock` and `.hlock/vendor/` instead.

To start from someone else's setup instead, `hlock sync github:OWNER/REPO` pulls their lock
first, and `hlock sync NAME` syncs one you've already pulled; see
[Pulling someone else's lock](pull.md).

## What hlock does, and what it leaves to you

By default `sync` only does things that are local and reversible: it writes config files
into the isolated root (keeping the old one as `<file>.hlock-bak`), copies skills and
instruction files, fetches [declared dependencies](dependencies.md#sync-on-another-machine)
at their locked commits (checking each digest), and runs the harness's own plugin commands
against that root. These
steps need you to opt in:

| Flag | Lets hlock… | Otherwise the step is manual, with |
|---|---|---|
| `--in-place` | write into your own config root (`~/.claude`, …) instead of `.hlock/env/` | (no manual step: the setup goes into `.hlock/env/`) |
| `--install` | run package managers: `npm install -g`, `brew install`, `uv tool install`, install scripts | the exact command, e.g. `npm install -g @earendil-works/pi-coding-agent@1.0.0` |
| `--pull` | download model weights through an installed runtime (`ollama pull`, `lms get`, `hf download`) | the pull command and the download size |

Some steps are always manual: apps without a CLI installer (the LM Studio app), self-hosted
inference servers on another machine (`location = "network"` in the lock: hlock lists the
models the server should serve and a `curl` to check it), and secrets.

Secrets are filled from the `HLOCK_SECRET_*` environment variables, your secrets file, or
your OS keychain, in that order (see [Secrets](secrets.md)). If none has a value but the
target config already has a real value in that place (say, you're syncing on the machine
you pinned), `sync` keeps it and never replaces a real secret with a placeholder. Files
that receive secret values, and their backups, are readable only by you (`0600`).

Models a provider lists but no locked role uses are optional; `--all-models` includes them.
`--exact` also moves aside skills in the target skill directory that aren't in the lock.

### What a lock can't make hlock do

A lock may be someone else's ([`hlock pull`](pull.md)), and `launch` runs the automatic
steps without asking, so hlock checks what a lock asks for. A step that fails a check is
manual, with the reason ending in "refusing to write it automatically"; hlock never does it
for you:

- **Instruction files** go only where the harness reads them (`CLAUDE.md`,
  `{root}/CLAUDE.md`, … as its [profile](harness-profiles.md) lists). A lock entry named
  `~/.zshrc` or `{root}/settings.json` is refused.
- **Skills** go only under the harness's skill directories, and a skill's name must be a
  plain directory name (no `/` or `..`).
- **Vendored copies** must be relative paths inside the lock's directory.
- **Values that reach a command** (a version, package, model id, revision or git ref) must
  be plain: letters, digits and `._:/@+-`, not starting with `-`. They're quoted as well.
  A version like `1; curl x | sh` is refused.
- **Skills from git** come only from `https://`, `http://`, `ssh://`, `git://` or
  `user@host:path` URLs, at a hash or a plain ref; hlock checks a full hash after checkout.
- **A decider's model** is pulled only through a local runtime hlock knows (`ollama`,
  `ollaya`), with that runtime's own pull command.
- **Decider hooks** are written only for the uses and events `hlock decide hook` answers;
  see [Deciders](deciders.md#locks-from-someone-else).
- **Plugins, marketplaces and packages** from a pulled lock (one with a `[pulled]` table)
  install only with `--install`, because a plugin runs code. From your own lock they install
  automatically, as before.

Installers run with the same environment as the rest of `sync`, and stop after four hours.
Config files are written whole or not at all (through a temporary file and a rename).

## Let your agent do the manual steps

```console
$ hlock sync --skill
…
✓ wrote .agents/skills/hlock-rehydrate/SKILL.md
```

This writes an [Agent Skill](https://agentskills.io) generated from the plan: the remaining
steps in order, the command for each, a check for each, and rules (ask before installing
system software or large downloads, never print secrets, finish with `hlock sync` and
`hlock check`, with the same `--prefix` or `--in-place` as the plan). Open any agent in the directory, even a vanilla one, and ask it to rehydrate
the harness with the `hlock-rehydrate` skill. Use `--skill-dir` to write it somewhere else,
such as `.claude/skills/hlock-rehydrate`.

To have hlock run the agent and check its work, use the
[meta harness](meta.md#install) instead: `hlock meta run install`.

## The isolated directory

The isolated root works like a virtual environment for the harness. It lives at
`.hlock/env/<harness>/` (for a lock named `harness.<name>.lock`, `.hlock/env/harness.<name>/<harness>/`).
`--prefix DIR` puts it under `DIR` instead:

```console
$ hlock sync --prefix .hlock/env2 --yes
…
✓ in sync at .hlock/env2/claude-code/config  your own config wasn't touched
use it: CLAUDE_CONFIG_DIR=.hlock/env2/claude-code/config claude
```

hlock points the harness at the directory through its config-root variable
(`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `HERMES_HOME`,
`CURSOR_CONFIG_DIR`, `XDG_CONFIG_HOME`), and plugin installs run against it too. Files that
normally live outside the config root move inside it (Claude Code reads `.claude.json` from
`CLAUDE_CONFIG_DIR` when it's set). Skills that lived outside the config root (for example
`~/.agents/skills`) are placed in the harness's own skill directory inside the prefix. If a
secret isn't set in the environment, `sync` takes the value from your own config when it has
one, without changing your config. Sign-in state isn't copied, so the first session in a new
prefix may ask you to log in.

## Into your own config

To restore the setup into your real config root, so a plain `claude` (or `pi`, …) picks it
up, opt in with `--in-place`:

```console
$ hlock sync --in-place --yes
Sync plan for Claude Code 2.1.0 → your config ~/.claude
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

Each file it replaces is kept as `<file>.hlock-bak`. Only the parts the lock restores are
written: for `~/.claude.json`, only `/mcpServers`, so your sign-in and other state stay.
`--in-place` doesn't combine with `--prefix` or `--docker`.

## Into a Docker image

```console
$ hlock sync --docker
…
✓ wrote .hlock/docker/pi/Dockerfile
next →
  docker build -t hlock-pi .hlock/docker/pi
  docker run -it --rm -e HLOCK_SECRET_… -v "$PWD:/workspace" hlock-pi
```

`--docker` writes a build context to `.hlock/docker/<harness>/`. The restored config, skills
and instruction files go into `home/` and are copied to the image's home. The `Dockerfile`
installs the harness with a method that works on Linux (Homebrew casks fall back to npm or the
install script) and runs the plugin installs. Secrets are never baked into the image: the
entrypoint fills `${HLOCK_SECRET_*}` placeholders when the container starts, from `-e`
variables or from secret files mounted at `/run/secrets` (Docker and Compose secrets; see
[Secrets](secrets.md#in-docker)). Model weights stay on the host. A local runtime is reached at
`http://host.docker.internal:<port>`, which the Dockerfile notes. `--build` also runs
`docker build`.

## Reference

```bash
hlock sync [LOCK] [--plan | --json | --skill] [--yes] [--install] [--pull] [--all-models]
           [--prefix DIR | --in-place | --docker [--build]] [--exact] [--verbose]
```

See the [CLI reference](../reference/cli.md#sync) for every option.
