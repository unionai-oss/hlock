# Rehydrating with sync

`hlock sync` turns a lock back into a working setup: on a new laptop, in a fresh VM, in an
isolated directory, or in a Docker image. It works on any lock, and gets the most out of one
written by [`hlock pin`](pin.md).

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
$ hlock sync
Sync plan for Claude Code 2.1.285 → your config
✓  harness  Claude Code 2.1.285
✓  mcp      `npx` for MCP servers: github
●  config   Write ~/.claude/settings.json
●  config   Write ~/.claude.json /mcpServers
●  skill    Copy skill my-skill → ~/.claude/skills/my-skill
○  secret   HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)  not set
Run 3 automatic steps? [Y/n]:
✓ Write ~/.claude/settings.json
✓ Write ~/.claude.json /mcpServers  placeholders for missing secrets
✓ Copy skill my-skill → ~/.claude/skills/my-skill

1 step needs you:
  1. Set 1 secret, then re-run hlock sync (values stay out of the lock)
     export HLOCK_SECRET_GITHUB_GITHUB_TOKEN=…  # mcp github GITHUB_TOKEN
next → hlock sync --skill (let your agent do it)
```

Set the secret and run it again. Re-running is always safe: satisfied steps are skipped, and
`sync` exits `3` while manual steps remain and `0` once everything matches.

```console
$ export HLOCK_SECRET_GITHUB_GITHUB_TOKEN=ghp_…
$ hlock sync --yes
…
✓ in sync with harness.lock
```

Then `hlock check` confirms the setup matches the pin.

To start from someone else's setup instead, `hlock sync github:OWNER/REPO` pulls their lock
first, and `hlock sync NAME` syncs one you've already pulled; see
[Pulling someone else's lock](pull.md).

## What hlock does, and what it leaves to you

By default `sync` only does things that are local and reversible: it writes config files
(keeping the old one as `<file>.hlock-bak`), copies skills and instruction files, and runs
the harness's own plugin commands. Two kinds of step need you to opt in:

| Flag | Lets hlock… | Otherwise the step is manual, with |
|---|---|---|
| `--install` | run package managers: `npm install -g`, `brew install`, `uv tool install`, install scripts | the exact command, e.g. `npm install -g @earendil-works/pi-coding-agent@1.0.0` |
| `--pull` | download model weights through an installed runtime (`ollama pull`, `lms get`, `hf download`) | the pull command and the download size |

Some steps are always manual: apps without a CLI installer (the LM Studio app), self-hosted
inference servers on another machine (`location = "network"` in the lock: hlock lists the
models the server should serve and a `curl` to check it), and secrets.

Secrets are filled from `HLOCK_SECRET_*` environment variables. If a variable isn't set but
the target config already has a real value in that place (say, you're syncing on the machine
you pinned), `sync` keeps it and never replaces a real secret with a placeholder.

Models a provider lists but no locked role uses are optional; `--all-models` includes them.
`--exact` also moves aside skills in the harness's skill directory that aren't in the lock.

## Let your agent do the manual steps

```console
$ hlock sync --skill
…
✓ wrote .agents/skills/hlock-rehydrate/SKILL.md
```

This writes an [Agent Skill](https://agentskills.io) generated from the plan: the remaining
steps in order, the command for each, a check for each, and rules (ask before installing
system software or large downloads, never print secrets, finish with `hlock sync` and
`hlock check`). Open any agent in the directory, even a vanilla one, and ask it to rehydrate
the harness with the `hlock-rehydrate` skill. Use `--skill-dir` to write it somewhere else,
such as `.claude/skills/hlock-rehydrate`.

To have hlock run the agent and check its work, use the
[meta harness](meta.md#install) instead: `hlock meta run install`.

## Into an isolated directory

`--prefix DIR` materializes the setup into its own config root instead of yours, like a
virtual environment for the harness:

```console
$ hlock sync --prefix .hlock/env --yes
…
✓ in sync at .hlock/env/claude-code/config
use it: CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config claude
```

hlock points the harness at the directory through its config-root variable
(`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `HERMES_HOME`,
`CURSOR_CONFIG_DIR`, `XDG_CONFIG_HOME`), and plugin installs run against it too. Your own
config isn't touched. Skills that lived outside the config root (for example
`~/.agents/skills`) are placed in the harness's own skill directory inside the prefix. Sign-in
state isn't copied, so the first session in a new prefix may ask you to log in.

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
entrypoint fills `${HLOCK_SECRET_*}` placeholders from `-e` variables when the container
starts. Model weights stay on the host. A local runtime is reached at
`http://host.docker.internal:<port>`, which the Dockerfile notes. `--build` also runs
`docker build`.

## Reference

```bash
hlock sync [LOCK] [--plan | --json | --skill] [--yes] [--install] [--pull] [--all-models]
           [--prefix DIR | --docker [--build]] [--exact] [--verbose]
```

See the [CLI reference](../reference/cli.md#sync) for every option.
