# Contributing

Contributions are welcome, from people and from coding agents working on someone's
behalf. This page covers how to propose a change and what a mergeable pull request
looks like. For setting up a working copy, see [Development](development.md).

## Ways to contribute

- **Report a bug or ask for a feature:** [open an issue](https://github.com/unionai-oss/hlock/issues/new).
  For bugs, include the command you ran, the output, `hlock --version`, and your OS
  and Python version. A minimal `harness.toml` and eval pack that reproduce the
  problem help most.
- **Improve the docs:** every page has an edit button that opens the source file
  on GitHub. Small fixes can go straight to a pull request.
- **Write code:** fix a bug, add a test, or pick up an open issue. For anything
  larger than a small fix, such as a new command, a manifest field, or a lockfile
  schema change, open an issue first so the design can be agreed before you build it.
- **Share eval packs:** packs for real domains make the resolver's evidence
  meaningful. See [Eval packs](../guides/eval-packs.md).

## Making a change

1.  Fork the repository and create a branch from `main`.
2.  Set up with `uv sync` (see [Development](development.md)).
3.  Make your change, with tests:

    | You changed | Add or update |
    |---|---|
    | Rust core (`crates/harness-lock-core`) | Rust tests (`cargo test -p harness-lock-core`) |
    | Python bindings, CLI, or worker | `tests/test_rust.py`, `tests/test_cli.py`, or `tests/test_worker.py` |
    | Behavior users can see | The matching page in `docs/` |
    | CLI output | Also run `uv run python scripts/cli_screenshot.py` |
    | A built-in harness or runtime profile | Also run `uv run python scripts/harness_table.py` |

4.  Run the same checks CI runs:

    ```bash
    uv run pytest
    uv run ruff check harness_lock hlock tests
    cargo test -p harness-lock-core
    uv run --group docs zensical build --clean   # if you touched docs/
    ```

5.  Open a pull request against `main`. Describe what changed and why, and how you
    tested it. Link the issue it addresses, if there is one.

CI must pass before a pull request is merged. A maintainer will review it, and may
ask for changes.

## Conventions

- **Keep pull requests focused.** One change per pull request is easier to review
  and to revert. Separate refactors from behavior changes.
- **Match the surrounding code.** Follow the existing style, naming, and comment
  density. Ruff enforces Python style. The Rust code isn't run through rustfmt, so
  don't reformat code you aren't otherwise changing.
- **Commit messages** start with a short summary line in the imperative mood ("Add
  `--json` to `hlock show`"), followed by a body explaining why, when it isn't
  obvious.
- **Don't bump versions** in a pull request. Releases are cut by maintainers; see
  [Releasing](releasing.md).
- **Don't hand-edit generated files**, such as `harness.lock`, `uv.lock`,
  `docs/assets/quickstart.svg`, or the tables in `docs/reference/harnesses.md`.
  Regenerate them with the tool that owns them.
- **Docs describe what the code does today.** If a feature is planned but not built,
  say so, or leave it to the [design specs](https://github.com/unionai-oss/hlock/tree/main/specs).

## For coding agents

If you're an AI coding agent contributing to this repository, follow everything
above, plus:

- **Read [`AGENTS.md`](https://github.com/unionai-oss/hlock/blob/main/AGENTS.md)**
  at the repository root first. It lists the commands and the rules for working in
  this codebase.
- **Run the checks before you say you're done**, and report the real results. If a
  test fails or a step was skipped, say so in the pull request.
- **Verify what you document.** Run the commands and code examples you add to the
  docs, and paste real output, not invented output.
- **Stay in scope.** Make the change that was asked for. Note other problems you
  find in the pull request description instead of fixing them on the side.
- **Say that the change was made with an agent** in the pull request description,
  and name the person responsible for it.

## License

hlock is licensed under [Apache-2.0](https://github.com/unionai-oss/hlock/blob/main/LICENSE).
By submitting a contribution, you agree that it's licensed under the same terms.
