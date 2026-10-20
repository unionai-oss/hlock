# AGENTS.md

Instructions for coding agents working in this repository. Humans: see the
[contributing guide](https://docs.hlock.ai/project/contributing/).

## What this is

hlock (published as `hlock`, with `harness-lock` as an alias) tunes an agent stack against a task
pack and pins the winner in `harness.lock`. A Rust core (`crates/harness-lock-core`)
does the deterministic work. A Python package (`hlock/`) provides PyO3
bindings, the `hlock` CLI, and a worker for heavy stages. See
`docs/concepts/architecture.md`.

## Setup and checks

```bash
uv sync                                              # build the extension, install dev tools
uv run pytest                                        # Python bindings, CLI, worker
uv run ruff check harness_lock hlock tests
uv run prek run --all-files                          # commit hooks in prek.toml
cargo test -p harness-lock-core                      # Rust core
uv run --group docs zensical build --clean           # docs, if you touched docs/
```

`uv run` rebuilds the Rust extension when Rust sources change. Run all checks before
you report a change as done, and report failures as they are.

## Layout

| Path | Contents |
|---|---|
| `crates/harness-lock-core/` | Rust core: manifest, lockfile, store, eval packs, search, resolver |
| `crates/harness-lock-py/` | PyO3 bindings (ships only in the wheel) |
| `crates/hlock/`, `crates/harness-lock/` | Re-export crates (`harness-lock` is the alias) |
| `harness_lock/` | Shim re-exporting `hlock`, shipped in the `hlock` wheel |
| `packaging/harness-lock/` | The code-free `harness-lock` PyPI alias (pins `hlock`) |
| `hlock/cli.py` | The `hlock` CLI (rich + rich-click) |
| `hlock/tasks/worker.py` | Worker for the `devbox` and `cluster` backends (versioned JSON contract) |
| `tests/` | pytest suites |
| `docs/`, `zensical.toml` | Documentation site |
| `evals/` | Demo eval pack |
| `specs/` | Design specs, including unbuilt features (`v0.md` is the core design) |

## Rules

- Keep changes focused on the task. Mention unrelated problems instead of fixing them.
- Match the surrounding code's style, naming, and comment density. The Rust code
  isn't rustfmt-formatted; don't run `cargo fmt` over files you aren't changing.
- Add or update tests with every behavior change.
- Update `docs/` when user-visible behavior changes. Docs describe what the code does
  today; planned features belong in `specs/`.
- Run any command or code example you add to the docs, and use its real output.
- After changing CLI output, run `uv run python scripts/cli_screenshot.py`.
- After changing the home page terminal (`docs/javascripts/terminal.js`), build the docs and
  run `uv run --with playwright python scripts/demo_video.py`.
- After changing a built-in profile, run `uv run python scripts/harness_table.py`.
- Don't hand-edit generated files: `harness.lock`, `uv.lock`,
  `docs/assets/quickstart.svg`, `docs/assets/demo.{webp,mp4}`, the tables in `docs/reference/harnesses.md`.
- Don't bump versions or edit release workflows unless asked. Versions are set with
  `scripts/set_version.py`.
- Keep the worker contract in sync: `CONTRACT_VERSION` in
  `hlock/tasks/worker.py` and `crates/harness-lock-core/src/worker.rs`.
- In pull requests, say that an agent made the change.
