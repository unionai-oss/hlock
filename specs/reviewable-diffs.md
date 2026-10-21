# Reviewable lock diffs: show the content behind every digest

Status: proposed · Owner: Niels Bantilan · Last updated: 2026-10-21 · Builds on [v0](v0.md) §5.4, [audit](audit.md) §6

> A lock pins content by digest, so `git diff harness.lock` shows that a prompt, skill
> or config changed but not how. `hlock diff` resolves each changed digest to the text
> behind it and prints a review: which components changed, the text diff of each, and
> how the evidence moved. Tuned artifacts (a tuned system prompt today, other tuned
> components later) are written as committed files, so there is always text to diff.

---

## 1. Problem

The lock is meant to be reviewed in a PR ([concepts](../docs/concepts/index.md): "Reviewing
a lock diff in a PR tells you what changed in the agent"). For models, harness versions,
MCP commands and evidence that's true: the values are in the lock. For anything with a
body (instructions, skills, config, tuned prompts) the lock holds a digest, and a reviewer
sees this:

```diff
 [[skill]]
 name = "review"
-digest = "sha256:a6cec631260e2d64e678c9554233646c7e2118bad1485955c48db83cf4e5acde"
+digest = "sha256:0b91f2e7c4d8a3365b1e2f8c7d9a0e4b1c6f3a2d8e5b7c9f0a1d2e3b4c5d6e7f"
```

Where the text lives today, by case:

| What changed | Where the text is | What a reviewer has to do |
|---|---|---|
| Project instruction file or skill | In the repo | Find the file in the same PR and match it to the digest by hand |
| `--user` skill, instruction or config | Vendored under `.hlock/vendor/` | Same, in another directory |
| Declared dependency (`github:acme/skills`) | Not in the repo; `source` and commit change | Open the upstream repo and compare commits |
| Tuned system prompt | **Nowhere.** `tune` measures with it (`ops.rs`, `tuned_prompt`) but the tune lock records only `before`/`after` digests | Can't review it, and `tune apply` can't reproduce it |
| Evidence | In the lock | Read the numbers (fine today) |

`pin`, `check` and `upgrade` already print a component summary (`capture::diff_locks`),
but it stops at the same short digests (`~ skill review (project): a6cec63126 → 0b91f2e7c4`).

The last row of the second table is the sharpest: a tuned prompt is the one artifact hlock
itself produces, and it's the one a reviewer can't see. As tuning grows past the system
prompt (§1.1), every new tuned component would have the same problem.

### 1.1 Context: Harness² and what gets tuned

[Harness²](https://hyper.ai/en/papers/2610.14224) (Google Cloud AI Research and
Northwestern, October 2026) tunes an agent's harness with the model frozen. It treats eight
components as editable: system prompt, guardrails, memory, tool guidance, scripts,
subagents, skills, and middleware plugins. How hlock covers each today:

| Harness² component | hlock pins it as | hlock tunes it |
|---|---|---|
| System prompt | `[[instructions]]` | yes (`tune --depth prompt`) |
| Guardrails | `[policy]`, `[[decider]]` | deciders by fine-tune only |
| Memory | not yet ([memory](memory.md)) | no |
| Tool guidance | `[[tool]]` (MCP config); guidance text only if in instructions | no |
| Scripts | `[[skill]] scripts` | no |
| Subagents | not yet (v0 §5.4 lists `subagent`; no capture) | no |
| Skills | `[[skill]]` | no |
| Plugins | `[[native]]` | no |

A Harness² tune backend (through the [backend protocol](backends.md)) would emit edits to
several of these at once. That only works if each edit lands as text a reviewer can read,
which is what this spec adds. Subagent capture is a separate gap (§6).

## 2. Design

### 2.1 Tuned artifacts are committed files

`tune` writes each tuned component's content next to the tune lock, content-addressed:

```
.hlock/tuned/<digest-prefix>/system-prompt.md
```

and the tune lock points to it:

```toml
[[tune.prompt.applied]]
component = "harness.system-prompt"
before = "sha256:43635a1f…"
after = "sha256:9c2e8b41…"
path = ".hlock/tuned/9c2e8b41/system-prompt.md"
gain = { success-rate = "+0.08" }
```

`tune apply` adds the file to the applied lock as an `[[instructions]]` entry with
`source = "tune"`, so `sync`, `launch` and `migrate` carry it like any other instruction
file. Before this, an applied tune changed the lock's evidence but not what the agent
reads, so the lock described a stack nobody could rebuild.

The same layout covers future tuned components (`tool-guidance.md`, `skills/<name>/SKILL.md`,
`subagents/<name>.md`): one directory per tuned artifact, the component name as the file name.

The text goes through `pin`'s redaction and hidden-Unicode scan before it's written.
A tuned prompt is model output and can carry anything.

### 2.2 `hlock diff`

```console
$ hlock diff
harness.lock.prev → harness.lock

~ llm default: anthropic:sonnet → anthropic:opus
~ skill review (project): a6cec63126 → 0b91f2e7c4
    skills/review/SKILL.md
    @@ -12,7 +12,8 @@
     ## Steps
    -1. Read the diff.
    +1. Read the diff and the linked issue.
    +2. Run the tests before commenting.
+ instructions harness.system-prompt (tune)
    .hlock/tuned/9c2e8b41/system-prompt.md  (new, 14 lines)
~ evidence success-rate: 0.71 → 0.79  (n=40 → 40)
    cost-usd-per-task: 0.042 → 0.051

1 skill, 1 instruction, 1 model changed · evidence +0.08 success, +$0.009/task
```

Arguments:

| Form | Compares |
|---|---|
| `hlock diff` | `<lock>.prev` and `<lock>` if `.prev` exists, else `HEAD:<lock>` and the working tree |
| `hlock diff REV` | `REV:<lock>` and the working tree (`hlock diff main`) |
| `hlock diff A B` | Two lock files, or two git revs |
| `--lock FILE` | Which lock (default `harness.lock`) |

The summary lines are `diff_locks`, unchanged. For each `~` or `+` component with a
digest, `diff` resolves the old and new content and prints a unified diff under it.
Resolution order:

1. A vendored or tuned file the lock entry points to (`vendored`, `path`), read at the
   matching side's rev.
2. The project file at its recorded path, at the matching rev.
3. The local store (`.hlock/store`) by digest, for declared dependencies already fetched.
4. With `--fetch`, the declared dependency's source at its locked commit.

Each resolved file is checked against the lock's digest before it's shown. If a side can't
be resolved or doesn't match, `diff` says so on that component (`old content unavailable:
fetch with --fetch`, `content doesn't match the locked digest`) and exits 1 under `--ci`.
It never shows a diff of text the lock didn't pin.

Skills are directories: `diff` shows each changed file, and lists added or removed files
by name. Binary files and files over 64 KiB are listed with their size, not diffed.

Config files are diffed from their redacted vendored copies, so secrets never appear.

Options:

| Option | Effect |
|---|---|
| `--stat` | Summary lines and per-component line counts only |
| `--format markdown` | For a PR comment: summary table, each text diff in a `<details>` block |
| `--json` | Components, resolved paths, and hunks |
| `--fetch` | Fetch declared dependencies that aren't in the store |
| `--ci` | Exit 1 if any changed component's content can't be resolved |

### 2.3 Hidden text in the diff

A reviewer reading a diff must see what the agent will read. `diff` runs the
[audit](audit.md) §3 content scan on every hunk it prints and renders flagged characters
visibly (`⟦U+202E⟧`, `⟦ZWSP⟧`), with a warning line on the component. This is the
reviewer-facing half of audit §6.

### 2.4 `git diff` integration (phase 2)

`hlock diff --setup-git` adds to `.gitattributes`:

```
harness.lock          diff=hlock
.hlock/locks/*.lock   diff=hlock
```

and to the repo's `.git/config`:

```
[diff "hlock"]
    command = hlock diff --git-external
```

so `git diff`, `git log -p` and `git show` render lock changes with `hlock diff`'s output.
It's opt-in, writes only the local git config, and `--setup-git --remove` undoes it. GitHub's
web diff doesn't use external drivers, which is why §2.5 exists.

### 2.5 A PR comment in CI (phase 2)

The [CI guide](../docs/guides/ci.md) gets a step that runs
`hlock diff origin/$BASE --format markdown --ci` and posts or updates one comment on the
PR. The comment leads with the evidence change, since that's the decision a reviewer makes:
did the success rate go up enough to justify what changed.

### 2.6 `pin`, `check`, `upgrade` and `tune apply`

They keep printing the summary they print today and add one hint line when any component
with a body changed: `see the content: hlock diff`. They don't print text diffs by default;
a re-pin that touches twenty skills shouldn't scroll the terminal.

## 3. Non-goals

- **Inlining content into the lock.** The lock stays small and digest-based; content lives
  in files the lock points to. A lock with every skill inlined would be unreadable as a diff
  in its own right.
- **Semantic diffs** ("this edit makes the agent more cautious"). That's judgment;
  `hlock meta` could do it later on top of `--json`.
- **Diffing memory contents.** The lock never holds them ([memory](memory.md)).

## 4. Phasing

1. §2.1 (tuned files, `tune apply` writes `[[instructions]]`) and §2.2 to §2.3, §2.6.
   §2.1 fixes a reproducibility bug and should land first, on its own if needed.
2. §2.4 `--setup-git`, §2.5 CI comment.

## 5. Open questions

- Should `.hlock/tuned/` keep artifacts from tunes that weren't applied? Proposal: `tune`
  writes them; `tune apply` leaves only the applied one plus whatever `.prev` references,
  and `hlock gc` (not yet specced) removes the rest.
- `hlock diff REV` on a lock that vendors with a namespace (`harness.<name>.lock`) needs the
  namespace from the old side's file name, which may have been renamed. Follow git's rename
  detection, or require `A B` in that case?

## 6. Follow-ups

- **Subagent capture.** v0 §5.4 lists `subagent` as a portable kind (`.claude/agents/`,
  OpenCode agents) with a prompt digest, model role and tool list, but `pin` doesn't capture
  it. It's one of the components Harness² tunes and should be pinned before it's tuned.
- **Tool guidance as its own component**, separate from instruction files, so a tuner can
  edit it without rewriting the system prompt.
- **Pluggable optimizers** (Harness², GEPA, RRSI and others) that write one tuned artifact
  per component they edit, in the §2.1 layout: see [tune optimizers](tune-optimizers.md).
