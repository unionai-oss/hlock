# hlock workflows

Each section is one thing a user asks for: the commands in order, the flags that matter,
and what to watch for. Run `hlock <command> --help` for every flag.

## Lock the setup you have

```bash
hlock pin                    # the project's setup: harness, model, project skills, MCP, config
hlock pin --user             # also ~/.claude, ~/.pi/agent, …: hand-written files are vendored
hlock pin -i 'skills/review-*' -i CLAUDE.md   # only some of the user-level setup
hlock pin --harness pi       # when more than one harness is installed
```

- `pin` prints a summary and a `left out user-level:` line naming what it skipped. Tell the
  user what was left out rather than adding `--user` on your own: it copies personal files
  into the repository.
- With `--user`, files another machine can't download go into `.hlock/vendor/`. Ask the user
  to review them before committing.
- Secrets become `HLOCK_SECRET_*` placeholders. The lock never holds a value.
- Re-pinning doesn't remember `--user` or `-i`. `pin` warns when the scope changes.
- `pin` won't overwrite a resolved lock (one with evidence). Use `-o other.lock`, or `--force`
  only if the user agrees.

Guide: https://docs.hlock.ai/guides/pin/

## Check for drift

```bash
hlock check
```

On a pinned lock, `check` re-captures the setup and lists what changed (`~ llm default:
anthropic:opus → anthropic:sonnet`). It exits `1` on drift. The hint offers two fixes:
re-pin to accept the change, or `hlock sync --in-place` to undo it. Ask the user which one
they want. On a resolved lock, `check` fails when `harness.toml` changed after the lock was
written.

## Add, remove, and update dependencies

```bash
hlock add skill github:OWNER/REPO/path/to/skill@^1.2 --dry-run
hlock add skill github:OWNER/REPO/path/to/skill@^1.2
hlock add mcp npm:@modelcontextprotocol/server-github@^1
hlock add mcp https://mcp.example.com/mcp
hlock add instructions github:OWNER/rules/style.md
hlock add plugin plugin:MARKETPLACE/NAME
hlock add skill ./skills/local-skill
hlock remove NAME
hlock outdated
hlock upgrade dep:NAME          # plan only
hlock upgrade dep:NAME --apply
```

- `add` declares the dependency in `harness.toml`, pins its commit or version and digest in
  `harness.lock`, and installs it into the project (`.claude/skills/` and the like). Use
  `--scope user` to install it for the user only.
- After `@`, a branch, tag, or commit is a ref, and anything that reads as a range (`^1.2`,
  `~1.4.0`, `>=1 <2`) matches tags named `v{version}`.
- `add` never overwrites something it didn't put there. If it stops, pick another name with
  `--as NAME`.
- `add --eval` runs the eval pack with and without the dependency before keeping it.
- `add --adopt NAME` declares something `pin` already recorded, as it is.

Guide: https://docs.hlock.ai/guides/dependencies/

## Reproduce a lock on this machine

```bash
hlock sync --plan            # always look first
hlock sync                   # asks before running the automatic steps
hlock sync --install         # also lets hlock run npm, brew, uv tool install, …
hlock sync --pull            # also lets hlock download model weights
hlock sync --in-place        # into the user's own config instead of .hlock/env/
hlock sync --docker --build  # a Docker image instead
hlock sync --skill           # writes an Agent Skill listing the manual steps for an agent
```

- Plan steps are `✓` satisfied, `●` automatic, or `○` manual. A manual step prints the exact
  command to run. Show those commands to the user and run them only with their agreement.
- `sync` exits `3` while manual steps remain and `0` once everything matches. Re-running it is
  safe.
- `⚑` marks an executable (an MCP server, plugin, hook, or skill script) that isn't trusted on
  this machine yet. `--yes` doesn't approve it. The user approves it with `hlock trust` (see
  below).
- By default `sync` writes an isolated root under `.hlock/env/<harness>/` and leaves the
  user's config alone. It prints how to use it, such as
  `CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config claude`.

Guide: https://docs.hlock.ai/guides/sync/

## Start a session on the lock

```bash
hlock launch                    # restores into .hlock/env/ and starts the harness there
hlock launch --dry-run          # prints the command instead
hlock launch claude --resume    # arguments after the harness name go to the harness
hlock launch --lock NAME        # a pulled or teammate's lock
hlock run "PROMPT"              # one non-interactive run on the locked agent
```

`launch` stops if the installed harness isn't the pinned version (`hlock sync --install`
fixes that). `launch` replaces the current process with an interactive session, so as an
agent prefer `--dry-run` or `hlock run`.

Guide: https://docs.hlock.ai/guides/launch/

## Use someone else's lock

```bash
hlock pull github:OWNER/REPO               # stored in ~/.hlock/pulls/OWNER/
hlock pull github:OWNER/REPO/harness.fast.lock
hlock launch --lock OWNER                  # try it, isolated
hlock sync OWNER --in-place --plan         # adopt it into the user's config: plan first
hlock list
hlock rm OWNER
```

Pulling runs nothing. The summary warns about the programs the lock's MCP servers start.
A pulled lock is someone else's code: run `hlock audit OWNER` and let the user
decide on `hlock trust` before syncing it.

Guide: https://docs.hlock.ai/guides/pull/

## Per-person locks in a shared repo

Each contributor keeps `.hlock/locks/harness.HANDLE.lock`:

```bash
hlock pin --harness claude-code -o .hlock/locks/harness.alice.lock
hlock --lock .hlock/locks/harness.alice.lock check
hlock --lock .hlock/locks/harness.bob.lock sync --plan    # try a teammate's
```

Keep the `harness.` prefix: `pull` only finds locks named `harness.lock` or
`harness.NAME.lock`. Only a lock's owner can re-pin it, since it records their machine.
Don't commit `.hlock/env/`.

Guide: https://docs.hlock.ai/guides/teams/

## Audit and trust

```bash
hlock audit                  # integrity, hidden characters, executables, policy, secrets
hlock audit --ci             # also fail on warnings
hlock audit --format sarif -o audit.sarif
hlock trust                  # list what the lock would run here, with digests
hlock trust --only NAME      # approve one (the user's call)
hlock trust --deny NAME
hlock export --format cyclonedx   # an SBOM (or spdx; apm-lock for apm.lock.yaml)
```

- Hidden characters (bidi overrides, zero-width spaces) in a skill or instruction file block
  `sync`, `launch`, and `pull`. The error names the file. `--allow-hidden FILE` lets it
  through, and `hlock audit --strip` removes them from vendored files. Both are the user's
  call after they look at the file.
- Approvals are stored per machine, by digest, in `~/.hlock/trust.toml`. A changed digest
  asks again.
- `hlock-policy.toml` in the project says which sources, MCP servers, providers, models, and
  harnesses are allowed.

Guide: https://docs.hlock.ai/guides/audit/

## Secrets

```bash
hlock secret list            # each secret the lock needs and where its value would come from
hlock secret set NAME        # reads the value without echo; stores it in the OS keychain
```

Values come from `HLOCK_SECRET_NAME` in the environment, then `~/.config/hlock/secrets.toml`
(which can reference a password manager), then the keychain. Never echo a value or write it
to a file.

Guide: https://docs.hlock.ai/guides/secrets/

## Measure and optimize

```bash
hlock evals init                       # a runnable hello-world pack in ./evals
hlock evals init --from-traces         # draft tasks from past agent sessions in this project
hlock evals templates                  # templates for evals init --template
hlock evals check                      # exits 1 while the pack isn't ready
hlock evals run [TASK]...              # run the locked agent on tasks and grade each run (costs a session each)
hlock evals grade TASK [WORKSPACE]     # grade a directory as a run of TASK would leave it
hlock init --pack ./evals              # a starter harness.toml
hlock resolve --dry-run                # the stacks and trial counts, before spending anything
hlock resolve                          # run the candidates on the pack, lock the winner
hlock show                             # the stack and its evidence
hlock upgrade                          # on a resolved lock: plan newer models, --apply to switch
hlock upgrade --rollback               # restore harness.lock.prev
hlock tune                             # tune the lock's retries/temperature into harness.tune.lock
hlock tune apply                       # adopt harness.tune.lock (keeps harness.lock.prev)
hlock migrate --to codex               # the same setup on another harness, in harness.codex.lock
hlock localize                         # an all-local equivalent in harness.local.lock
```

- `migrate` moves skills, MCP servers, instructions and deciders to where the new harness
  reads them (`CLAUDE.md` → `AGENTS.md`, `.claude/skills` → `.agents/skills` for Codex), and
  models to a provider it supports (`--model provider:model` picks one). Plugins and settings
  don't carry; it lists them. Nothing changes until `hlock sync harness.codex.lock`
  (`--in-place` to switch your own config), so show the user the report first.

- `evals init` with `--template` or `--from-traces` also writes an Agent Skill that guides
  an agent through filling in or reviewing the pack. Use it when you're asked to write
  tasks.
- A useful pack has at least 10 tasks, each with a grader that checks the result (a
  `check.py` or a `rubric.md`) and a known-good `solution/`.
- `resolve` can cost money: a code harness runs for real, and every backend but `offline`
  (`local`, `flyte`, or a custom one) calls real models. Run `hlock resolve --dry-run` first and confirm the stacks, the trial count, and
  `[budget] max-trials` / `max-spend-usd` with the user. `hlock backend status` shows where
  trials will run.
- Agent CLIs (Claude Code, Codex, …) are measured on a stand-in in `resolve` and `tune`; the
  lock says `simulated = true`. To see the real agent on tasks, use `hlock evals run`.
- `hlock init` writes the offline reference demo unless given `--harness`. For an agent
  CLI, it takes candidate models from a `harness.lock` that pins that harness.

Guides: https://docs.hlock.ai/guides/eval-packs/, https://docs.hlock.ai/guides/resolve/,
https://docs.hlock.ai/guides/tune/, https://docs.hlock.ai/guides/migrate/,
https://docs.hlock.ai/guides/localize/

## The meta harness

`hlock meta` is an agent hlock uses for its own work, pinned in `harness.meta.lock`:

```bash
hlock meta init              # pick a model from the API keys or local runtimes found
hlock meta jobs              # install, bootstrap-evals, update, and custom jobs
hlock meta run install       # do sync's manual steps, then check the machine matches
hlock meta verify JOB        # run only the job's check
```

If you're already the user's agent, you can do these jobs yourself with the commands above.
Mention `hlock meta` when the user wants it to happen unattended or in CI.

Guide: https://docs.hlock.ai/guides/meta/

## Harnesses

```bash
hlock harness list           # every harness hlock knows, and which are installed here
hlock harness show NAME      # its profile and the paths it reads on this machine
hlock runtime list           # local inference runtimes (Ollama, LM Studio, vLLM, …)
```

hlock picks the harness from `--harness`, then `HLOCK_HARNESS`, then `[target] harness` in
`harness.toml`, then the harness it's running inside, then the only one installed.

Guide: https://docs.hlock.ai/guides/harnesses/
