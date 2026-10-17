# Using hlock in CI

Commit `harness.toml` and `harness.lock` together, then have CI check that they still
match. If someone edits the manifest without re-locking, the build fails, just like
`uv sync --locked` or `cargo build --locked`.

## `hlock check`

```console
$ hlock check
✓ harness.lock matches harness.toml  e6471474cb15
```

When the manifest has drifted, it exits `1` and prints both hashes:

```console
$ hlock check
✗ drift: harness.toml changed since harness.lock was written
locked  sha256:e6471474cb158117c727fbf3371e2287935d191f16c71c24c14ea80ca54693e2
live    sha256:d4271d0bda41886908111b7a2346bc4856c4f3355a11856230054d717a4cd483
  hint: re-lock with hlock resolve, then commit both files
```

!!! note "The hash covers the whole file"

    The manifest hash is computed over the file as written, comments and whitespace
    included. Any edit to `harness.toml` means re-running `hlock resolve`.

## GitHub Actions

```yaml title=".github/workflows/agent-lock.yml"
name: agent-lock

on:
  pull_request:
    paths: ["harness.toml", "harness.lock", "tasks/**"]

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: astral-sh/setup-uv@v6
      - run: uvx hlock check
```

Output is plain text when CI isn't a terminal, and you can force that anywhere with
`NO_COLOR=1`.

## Smoke-test the locked agent

`hlock run` exits non-zero when the task fails, so a canary task can gate a deploy:

```yaml
      - run: uvx hlock run "clean ./data/orders.csv"
```

Use `--json` when a later step needs the outcome:

```bash
uvx hlock run --json "clean ./data/orders.csv" > outcome.json
```

## Scheduled re-tuning

`hlock tune --strict` exits non-zero unless it beats the current lock. A nightly
job can try, and open a PR only when there's a real improvement:

```yaml
on:
  schedule:
    - cron: "0 6 * * *"

jobs:
  tune:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: astral-sh/setup-uv@v6
      - run: uvx hlock tune --depth hpo --strict --apply
      # then open a PR with the updated harness.lock, e.g. with peter-evans/create-pull-request
```
