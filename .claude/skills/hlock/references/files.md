# Files hlock reads and writes

## In the project (commit these)

| Path | Written by | What it is |
|---|---|---|
| `harness.toml` | the user, `hlock init`, `hlock add` | The manifest: candidates to try, eval packs, objective, budget, `[target]`, and declared dependencies (`[skills]`, `[mcp]`, …). Edit it by hand. [Reference](https://docs.hlock.ai/reference/manifest/) |
| `harness.lock` | `pin`, `add`, `remove`, `resolve`, `upgrade` | The lock: the exact stack, with digests. `kind = "pin"` for a pinned setup; a resolved lock also has `[evidence]`. Generated, so don't edit it. [Reference](https://docs.hlock.ai/reference/lockfile/) |
| `harness.NAME.lock` | `pin -o`, `migrate`, `localize`, `tune fetch` | Other locks: `harness.codex.lock` from `migrate --to codex`, `harness.local.lock` from `localize`, `harness.tune.lock` from `tune`, or per-person locks. |
| `.hlock/locks/harness.HANDLE.lock` | `pin -o` | One contributor's lock in a shared repo. Its vendored files go in `.hlock/locks/.hlock/vendor/HANDLE/`. |
| `.hlock/vendor/` | `pin --user` | Copies of files another machine can't fetch (hand-written skills, user config), with secrets redacted. Checked by digest, so don't edit them. |
| `harness.meta.toml`, `harness.meta.lock` | `hlock meta init`, `hlock meta pin` | The meta harness's manifest and lock. [Reference](https://docs.hlock.ai/reference/meta-manifest/) |
| `hlock-policy.toml` | the user | What the project allows: sources, MCP servers, providers, models, harnesses, evidence. [Reference](https://docs.hlock.ai/reference/policy/) |
| `evals/` | `hlock evals init`, the user | An eval pack: `pack.toml` and one directory per task (`prompt.md`, `workspace/`, `solution/`, `check.py` or `rubric.md`). [Guide](https://docs.hlock.ai/guides/eval-packs/) |

## In the project (don't commit these)

| Path | What it is |
|---|---|
| `.hlock/env/` | Isolated config roots that `sync` and `launch` restore locks into |
| `.hlock/bak/` | What `add` and `sync` set aside before replacing it |
| `.hlock/store/`, `.hlock/runs/`, `.hlock/tune/`, `.hlock/localize/` | Trial artifacts and caches |
| `.hlock/meta/runs/` | Meta harness run records (they can quote anything the agent read) |
| `harness.lock.prev` | The lock before the last `upgrade --apply`, for `upgrade --rollback` |

## Per user

| Path | What it is |
|---|---|
| `~/.hlock/pulls/NAME/` | Locks fetched with `hlock pull` |
| `~/.hlock/env/NAME/` | Isolated roots for pulled locks |
| `~/.hlock/trust.toml` | Executables approved or denied on this machine, by digest |
| `~/.hlock/cache/git/` | Git cache for declared dependencies |
| `~/.config/hlock/secrets.toml` | Where secret values come from (password manager references) |

`HLOCK_HOME` moves `~/.hlock`. Other variables are listed in the
[environment reference](https://docs.hlock.ai/reference/environment/).

## Reading a lock

Ask hlock rather than parsing the TOML yourself: `hlock show` prints the stack and evidence,
`hlock show --json` prints the whole lock, and `hlock tree` prints the components as a tree.

The main sections:

- `[stack]`, `[harness]`: the harness, its version, and its install source
  (`npm:…@1.0.0`, `brew-cask:…`).
- `[[llm]]`: one per role (`default`, `planner`, …) with its provider and model.
- `[[mcp]]`, `[[skill]]`, `[[plugin]]`, `[[instructions]]`, `[[config]]`: what's installed,
  each with a digest. A skill's `origin` is `harness-builtin`, `bundled` (from a plugin),
  `third-party` (pinned to a git commit), `first-party` (in the project), or `local`
  (hand-written, so vendored). `declared = true` means it came from `harness.toml`.
- `[evidence]`: on a resolved lock, the success rate, cost, and latency it was measured
  with.
- `[provenance]`: how it was made (`backend = "pin"`, or the resolve backend) and the
  scopes pinned (`project`, `user`).
