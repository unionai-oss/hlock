# Development

## Prerequisites

- [uv](https://docs.astral.sh/uv/getting-started/installation/)
- A [Rust toolchain](https://rustup.rs) (stable)

uv takes care of Python itself: it installs a matching interpreter if you don't have
one.

## Set up

```bash
git clone https://github.com/unionai-oss/hlock
cd hlock
uv sync
```

`uv sync` creates `.venv`, builds the Rust core and the PyO3 bindings with
[maturin](https://www.maturin.rs), installs the package in editable mode, and installs
the `dev` dependency group (maturin, pytest, ruff) at the versions in `uv.lock`.

Python changes take effect immediately. When you change Rust code, the next `uv run`
or `uv sync` rebuilds the extension: `pyproject.toml` lists the Rust sources as uv
`cache-keys`, so uv knows when the build is stale.

To work on a specific Python version, pass it once: `uv sync --python 3.9`.

## Test and lint

```bash
uv run pytest                                   # bindings, CLI, worker
uv run ruff check harness_lock hlock tests
cargo test -p harness-lock-core                 # Rust core + integration tests
```

Try the CLI against your working tree with `uv run hlock …`.

CI runs the same checks on every PR, on Python 3.9, 3.12, and 3.13. It also builds
the wheel, the sdist, and the `harness-lock` alias, installs them, and dry-runs
`cargo publish` for all three crates.

## Dependencies

| Where | What |
|---|---|
| `[project] dependencies` | Runtime dependencies, shipped in the wheel |
| `[project.optional-dependencies]` | User-facing extras, e.g. `hlock[flyte]` |
| `[dependency-groups] dev` | Contributor tools, installed by `uv sync` |
| `[dependency-groups] docs` | The docs toolchain (Zensical, Python 3.11+) |

Add a dependency with `uv add <pkg>` (runtime) or `uv add --group dev <pkg>`, and
commit the updated `uv.lock`.

## Docs

The site uses [Zensical](https://zensical.org), from the team behind Material for
MkDocs. Pages are Markdown in `docs/`, and the configuration is `zensical.toml`.

```bash
uv run --group docs zensical serve              # http://localhost:8000, with live reload
uv run --group docs zensical build --clean      # static site in site/
```

Every push to `main` publishes the site to GitHub Pages
(`.github/workflows/docs.yml`). PRs that touch the docs build the site, so a broken
page fails review before it ships.

The terminal screenshot on the home page and in the README is rendered from a real CLI
session. Regenerate it after changing CLI output:

```bash
uv run python scripts/cli_screenshot.py         # writes docs/assets/quickstart.svg
```

## Where things live

See [Architecture](../concepts/architecture.md#repository-layout) for the repository
layout, and the [design specs](https://github.com/unionai-oss/hlock/tree/main/specs) for
the full design.
