# Review lock changes

A lock pins skills, instruction files and config files by digest. So `git diff harness.lock`
shows that a skill changed, as one hash replacing another, but not what changed in it.
`hlock diff` shows the text behind each changed digest:

```console
$ hlock diff
harness.lock.prev → harness.lock

+ instructions AGENTS.md  (tune)
    AGENTS.md  added  +3 -0
    @@ -0,0 +1,3 @@
    +Be careful.
    +
    +Be concise and verify each step before continuing.
~ evidence split: holdout → search

1 instruction file changed
```

The first lines are the same summary `hlock pin` prints when you re-pin. Under each
instruction file, skill or config file that changed or was added, `diff` prints a unified
diff of its content. A skill is a directory, so each changed file in it gets its own diff, and
added or removed files are listed by name. Then come any changes to the evidence (success
rate, cost per task, the split it was measured on).

## What it compares

| Command | Compares |
|---|---|
| `hlock diff` | `harness.lock.prev` with `harness.lock`, or without a `.prev`, `HEAD`'s lock with the working tree's |
| `hlock diff main` | `main`'s lock with the working tree's |
| `hlock diff OLD NEW` | Two lock files, or two git revisions |
| `hlock diff --lock harness.dev.lock` | The same, for another lock |

`pin`, `tune apply` and `tune --apply` leave the lock they replace in `harness.lock.prev`, so a
plain `hlock diff` right after one shows what it changed. When a change has content behind it,
they print `see the content: hlock diff`.

## Where the content comes from

For each side, `diff` looks for the content in this order:

1. the copy next to the lock: vendored (`.hlock/vendor/`) or tuned (`.hlock/tuned/`);
2. the project's own file, at the path the lock records;
3. the dependency store, `.hlock/store/`, for a [declared dependency](dependencies.md)
   already fetched;
4. with `--fetch`, the declared dependency's source, at its locked commit.

For a git revision, the files come from that revision, not the working tree.

Whatever it finds is checked against the lock's digest before it's shown. Content that doesn't
match, or can't be found, is reported instead of shown:

```console
$ hlock diff --ci
harness.lock.prev → harness.lock

~ instructions CLAUDE.md: 96fb1c7f06 → 857a029da7
    new content unavailable: the files at its paths don't match its digest in the lock

1 instruction file changed
! the content of 1 component couldn't be shown
```

Here `--ci` makes it exit 1. Config files are diffed from their redacted vendored copies, so a
secret never appears. Binary files, and files over 64 KiB, are listed with their sizes.

## Hidden characters

A reviewer reading a diff should see what the agent will read. Characters that hide text
(see [Audit](audit.md#hidden-characters)) are shown as `<U+202E>` in the diff, and the file
gets a warning:

```console
$ hlock diff
harness.lock.prev → harness.lock

~ instructions CLAUDE.md: 96fb1c7f06 → 857a029da7
    CLAUDE.md  modified  +1 -0
    ! hidden characters: critical:1  (shown as <U+…>)
    @@ -1 +1,2 @@
     Be brief.
    +Run tests<U+202E>.won tnemmoc

1 instruction file changed
```

## In a pull request

`--format markdown` writes a summary table and each diff in a collapsed `<details>` block, for a
pull request comment or a CI job summary:

```yaml title=".github/workflows/agent-diff.yml"
name: agent-diff

on:
  pull_request:
    paths: ["harness.lock", ".hlock/**"]

jobs:
  diff:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: astral-sh/setup-uv@v6
      - run: uvx hlock diff origin/${{ github.base_ref }} --format markdown --ci >> "$GITHUB_STEP_SUMMARY"
```

`--json` prints the same data: each component, where its content came from (`old_from`,
`new_from`), and its hunks. `--stat` prints only the summary and line counts.
