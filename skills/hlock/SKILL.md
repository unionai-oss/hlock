---
name: hlock
description: Use hlock to pin, check, sync, launch, audit, and tune an agent setup (harness, models, MCP servers, skills, plugins, instructions, config) recorded in harness.lock, and answer questions about how hlock works. Use when the user mentions hlock, harness-lock, harness.lock, harness.toml, eval packs, or wants to lock, share, reproduce, audit, or optimize their agent setup.
---

# hlock

hlock is a lockfile for agent setups. `harness.lock` pins a harness at a version, its models,
MCP servers, skills, plugins, instruction files, and config, with a digest for each, so the
same agent comes back on another machine. There are two ways to get a lock:

- **Pin** the setup you already have: `hlock pin`. No manifest needed.
- **Resolve** a better one: declare candidates in `harness.toml`, point it at an eval pack,
  and `hlock resolve` measures each combination and locks the winner.

The full docs are at https://docs.hlock.ai. For a command's exact flags, run
`hlock <command> --help`. It reflects the version installed here, so trust it over this
skill when they disagree.

## Ground rules

- **Don't hand-edit `harness.lock`, `harness.meta.lock`, `harness.tune.lock`, or anything
  under `.hlock/vendor/`.** They're generated. Change the setup, then re-run the command
  that writes them (`pin`, `add`, `remove`, `resolve`, `tune fetch`). An edited vendored
  file fails its digest check.
- **Edit `harness.toml` freely.** It's the part the user writes. After editing it, run
  `hlock check`.
- **Look before you change anything.** Most commands that write have a dry run: `sync --plan`,
  `add --dry-run`, `launch --dry-run`, `upgrade` (it only plans unless you pass `--apply`).
  Show the user the plan first.
- **Leave trust decisions to the user.** `--trust`, `hlock trust --yes`, and `--allow-hidden`
  approve code or hidden characters to run on the user's machine. Don't pass them unless the
  user asked you to after seeing what they approve (`hlock trust` lists it, and `hlock audit`
  explains each finding).
- **Never put secret values in files.** Locks hold `HLOCK_SECRET_*` placeholders. Values go
  in the keychain (`hlock secret set NAME`) or the environment.
- **`sync` doesn't touch the user's own config by default.** It writes an isolated root
  under `.hlock/env/`. `--in-place` writes into `~/.claude`, `~/.pi`, and the like, so ask
  before using it.

## What to run

| The user wants to | Run | Guide |
|---|---|---|
| Lock their current setup | `hlock pin` (project only), `hlock pin --user` (also `~/.claude` and the like) | [pin](https://docs.hlock.ai/guides/pin/) |
| Know if their setup changed since the pin | `hlock check` | [pin](https://docs.hlock.ai/guides/pin/#keep-it-honest-with-check) |
| Add a skill, MCP server, plugin, or instruction file | `hlock add KIND SOURCE` (`--dry-run` first) | [dependencies](https://docs.hlock.ai/guides/dependencies/) |
| Remove or update one | `hlock remove NAME`, `hlock outdated`, `hlock upgrade dep:NAME` | [dependencies](https://docs.hlock.ai/guides/dependencies/) |
| Reproduce a lock on this machine | `hlock sync --plan`, then `hlock sync` | [sync](https://docs.hlock.ai/guides/sync/) |
| Start a session on the locked setup | `hlock launch` (`--dry-run` to see the command) | [launch](https://docs.hlock.ai/guides/launch/) |
| Try someone else's setup | `hlock pull github:OWNER/REPO`, then `hlock launch --lock …` | [pull](https://docs.hlock.ai/guides/pull/) |
| Keep per-person locks in a repo | `hlock pin --harness H -o .hlock/locks/harness.HANDLE.lock` | [teams](https://docs.hlock.ai/guides/teams/) |
| Check a lock is safe to use | `hlock audit`, `hlock trust` | [audit](https://docs.hlock.ai/guides/audit/) |
| Store a secret a lock needs | `hlock secret list`, `hlock secret set NAME` | [secrets](https://docs.hlock.ai/guides/secrets/) |
| Write tasks to measure agents on | `hlock evals init`, `hlock evals check`, `hlock evals grade` | [eval packs](https://docs.hlock.ai/guides/eval-packs/) |
| Find the best model and harness | `hlock init`, edit `harness.toml`, `hlock resolve` | [resolve](https://docs.hlock.ai/guides/resolve/) |
| Run everything on local models | `hlock localize` | [localize](https://docs.hlock.ai/guides/localize/) |
| See what's locked | `hlock show`, `hlock tree` | [CLI](https://docs.hlock.ai/reference/cli/) |
| See which harnesses hlock supports | `hlock harness list`, `hlock harness show NAME` | [harnesses](https://docs.hlock.ai/reference/harnesses/) |

For the details of each workflow, the flags that matter, and what the output means, read
[references/workflows.md](references/workflows.md). For the files hlock reads and writes,
read [references/files.md](references/files.md).

## Which lock

Commands use `harness.lock` next to the nearest `harness.toml` unless told otherwise. For
another lock, pass the global option before the command: `hlock --lock PATH check`. You can
also set `HLOCK_LOCKFILE` to an absolute path. When a project has several harnesses
installed, pass `--harness NAME` to `pin`.

## Reading results

- Exit codes: `0` success; `1` failure (drift, a failed step, an invalid file); `2` bad
  usage; `3` from `sync` or `meta run` means the automatic steps finished but some steps need
  the user.
- `sync` plans mark each step `✓` satisfied, `●` automatic (hlock does it), or `○` manual
  (hlock prints the exact command). Re-running `sync` is safe.
- Most commands end with a `next →` line suggesting the next command. Follow it unless the
  user wants something else.
- Errors print one line with a `hint:`. Read the hint before trying something else.

## Answering questions

Answer from the docs, not from memory: hlock changes quickly. If this repository is the
hlock source (it has `crates/harness-lock-core/`), the docs are in `docs/` and the design
notes for features that aren't built yet are in `specs/`. Otherwise use https://docs.hlock.ai.
If something is only in `specs/`, say it's planned, not available.
