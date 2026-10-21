# Memory: lock the memory system, never its contents

Status: proposed · Owner: Niels Bantilan · Last updated: 2026-10-21 · Builds on [v0](v0.md) §5.4

> A lock records *which* memory an agent uses (the harness's own memory, on or off; a
> memory MCP server and its endpoint) and *where* it lives, but never what's in it.
> `launch` keeps your memory instead of silently forking it, trials always start from a
> known seed, and `audit` scans memory for secrets and hidden text.

---

## 1. Problem

Every major harness now writes its own memory as it runs, and hlock has no model for it.
v0 already set the rule ("live state is never locked; its seed is", §5.4), and the
autonomous-agent profiles leave `MEMORY.md` and `memory/` out of `instructions` so a
conversation isn't drift. Nothing else follows from the rule yet, and the gaps show up
in everyday commands.

### 1.1 What people use (October 2026)

Generated memory is a local, unreviewed cache next to committed instruction files:

| Harness | Generated memory | Turned on/off by |
|---|---|---|
| Claude Code | `{root}/projects/<cwd-slug>/memory/`: a `MEMORY.md` index plus topic files with frontmatter. The index head loads each session. On by default | `settings#/autoMemoryEnabled`, `CLAUDE_CODE_DISABLE_AUTO_MEMORY`; moved with `settings#/autoMemoryDirectory` |
| Codex CLI | `{root}/memories/` (`MEMORY.md`, summaries, rollouts), built by a background consolidation pass. Preview | `config#/features/memories` |
| Gemini CLI | `save_memory` appends to a `## Gemini Added Memories` section *inside* `{root}/GEMINI.md` | the tool's availability |
| VS Code / Copilot | local `/memories/{,repo/,session/}`; Copilot Memory is GitHub-hosted, per repo, expires after 28 days | product settings |
| Windsurf | `~/.codeium/windsurf/memories/`, per workspace | product settings |
| Cursor | removed in 2.1; users told to export memories to `.mdc` rules | — |
| Autonomous agents | `MEMORY.md`, `memory/`, daily notes in the workspace | always on |

Third-party memory comes in through plugins and MCP servers that hlock already pins:
claude-mem (Claude Code hooks, SQLite + Chroma), Mem0/OpenMemory, Supermemory, Zep/Graphiti,
Basic Memory (local markdown), Byterover Cipher, and the reference MCP `memory`
knowledge-graph server. Their main selling point is portability across harnesses and
sharing within a team.

What practitioners say, and why it matters for hlock:

- **Stale memory steers agents.** In the issue that led Claude Code to add a kill switch,
  users describe memory as "a parallel memory system outside the user's control" and give
  an outdated `MEMORY.md` note that misdirected a test plan as an example
  ([claude-code#23544](https://github.com/anthropics/claude-code/issues/23544)). Windsurf
  and VS Code tell users to promote anything durable into committed rules.
- **Memory changes measured behavior.** A Codex practitioner write-up cites OP-Bench:
  personalized agents scored 26–61% below memory-free baselines on some tasks. Its advice
  is to turn memory off in unfamiliar repos
  ([source](https://codex.danielvaughan.com/2026/06/12/codex-cli-memory-over-personalisation-sycophancy-risk-configuration-defence/)).
  Evals that run on an empty memory miss this drift, and evals that run on *your* memory
  aren't reproducible. We found almost no tooling for this, so hlock is well placed to fill
  the gap.
- **Memory is an attack and leak surface.** Memory poisoning, where one injected input
  writes a durable instruction, is on the OWASP Agentic Top 10 and was demonstrated against
  `MEMORY.md` files
  ([CSA, MemGhost](https://labs.cloudsecurityalliance.org/research/csa-research-note-memghost-agent-memory-poisoning-20260723-c/)).
  Agents also write API keys into plain-text memory
  ([NVIDIA](https://perspectives.nvidia.com/nvidia-nemoclaw/prevent-ai-agent-writing-api-keys-long-term-memory/)).
- **Formats are converging.** Claude Code, Codex, VS Code, the Anthropic API memory tool,
  Letta's MemFS and Cognition's "Agent Memory Repo" spec all use some form of a capped
  `MEMORY.md` index over markdown topic files. W3C and IETF efforts toward a portable
  memory format exist but have no harness adoption yet.

### 1.2 Gaps in hlock today

1. **`launch` forks memory silently.** `launch` points Claude Code at
   `CLAUDE_CONFIG_DIR=.hlock/env/claude-code/config`. Auto memory then lives under that
   root, so a launched session starts without anything the user's agent has learned about
   the project. What it learns stays in `.hlock/env/` and never reaches the user's normal
   sessions. Codex (`CODEX_HOME`) has the same problem. Nothing tells the user.
2. **Memory inside an instruction file is drift.** Gemini's `save_memory` writes into
   `~/.gemini/GEMINI.md`, which `pin --user` records as an instruction file. Every saved
   memory then makes `hlock check` fail, and `sync` erases the saved memories.
3. **Trials aren't isolated from memory.** A code harness's trial (`CommandAdapter`) runs
   with the caller's environment. A `claude -p` or `codex exec` inside it reads and writes
   the user's real memory, so one trial can affect the next, and the user's memory affects
   the evidence.
4. **The lock can't say whether memory was on.** A lock reviewed in a PR shows the skills
   and MCP servers but not whether auto memory is enabled, although that changes behavior
   as much as an instruction file. Memory MCP servers are pinned like any other server, so
   nothing marks their state as remote and impossible to reset.
5. **`audit` doesn't look at memory**, which is the one place an agent writes text it will
   later read back as instructions.

## 2. Design

Two principles from v0, applied:

- **System, not contents.** The lock records whether memory is on, which provider
  supplies it, and where it lives. Memory contents are never written into a lock,
  vendored, or digested for `check`.
- **Seeds, not live state, for anything measured.** A trial's memory starts empty or from
  a seed the eval pack ships, and nothing carries over between trials.

### 2.1 Profiles declare memory

A harness profile gets a `[memory]` table that lists where the harness writes memory and
how it's switched:

```toml
# claude-code.toml
[memory]
dirs = ["{root}/projects/{cwd-slug}/memory"]
enabled = ["env:CLAUDE_CODE_DISABLE_AUTO_MEMORY!", "settings#/autoMemoryEnabled"]   # ! = inverted
relocate = "settings#/autoMemoryDirectory"          # how hlock points memory elsewhere
off = { env = { CLAUDE_CODE_DISABLE_AUTO_MEMORY = "1" } }

# codex.toml
[memory]
dirs = ["{root}/memories"]
enabled = ["config#/features/memories"]
off = { set = { "config#/features/memories" = false } }

# gemini-cli.toml: memory shares a file with instructions
[memory]
sections = [{ file = "{root}/GEMINI.md", heading = "## Gemini Added Memories" }]

# openclaw.toml (and the other autonomous agents)
[memory]
files = ["{workspace}/MEMORY.md", "{workspace}/HEARTBEAT.md"]
dirs = ["{workspace}/memory"]
```

The exclusions that autonomous-agent profiles state today in comments move into this
table. Everything else depends on it:

- `pin` and `check` never capture a path in `dirs` or `files`. A `sections` entry is cut
  out of its instruction file before the file is digested, so `check` ignores saved
  memories, and `sync` writes the pinned instructions and keeps the section that's there
  (fixes §1.2.2).
- `harness show` lists the memory locations, and `docs/reference/harnesses.md` gets a
  memory column from the same data (`scripts/harness_table.py`).

### 2.2 The lock records the memory system

`pin` adds one `[memory]` entry per harness, plus one per memory provider it recognizes:

```toml
[memory]
native = { enabled = true, resolved-from = "settings#/autoMemoryEnabled", location = "default" }

[[memory.provider]]
name = "mem0"
via = "mcp:mem0"                     # the [[tool]] entry already pinned
store = "remote"                     # remote | local
endpoint = "https://api.mem0.ai"
```

- `native.enabled` follows `[memory] enabled` in the profile. A harness without a
  `[memory]` table records nothing. `location = "custom"` means `relocate` was set, and the
  path itself isn't recorded because it's per-machine.
- A provider is recognized from a small built-in catalog (MCP packages and plugins such as
  `@modelcontextprotocol/server-memory`, `mem0`, `supermemory`, `basic-memory`,
  `claude-mem`, `graphiti`), or tagged by hand with `hlock add mcp … --memory`. `store` is
  `local` when the server's state is a path on this machine, otherwise `remote`.
- `check` reports a change in `native.enabled` or in the provider list as drift. That's a
  change in behavior, and it's what a reviewer should see in the lock diff.

This replaces v0's planned `memory` component kind for the `pin` path. `[candidates.policy]
memory` (§2.4) is the resolve side.

### 2.3 `launch` and `sync` keep your memory

`launch` isolates *configuration*. Memory is what the agent has learned about the project,
so by default it should stay the user's. A new `--memory` option:

| Mode | Behavior |
|---|---|
| `shared` (default) | The harness reads and writes the user's own memory location. hlock uses `relocate` when the profile has one (Claude Code: `autoMemoryDirectory` set to the real `~/.claude/projects/<slug>/memory` in the isolated settings). Otherwise it symlinks each isolated `dirs` path to the real one |
| `isolated` | Today's behavior: memory lives in `.hlock/env/`, which is already gitignored |
| `off` | Memory disabled through the profile's `off` |

`launch` prints the mode on the line it already prints (`▶ Claude Code  memory: shared  …`),
so the behavior is never silent. `HLOCK_MEMORY` sets the default. `sync --prefix`
takes the same option. `run` defaults to `off`, because a scripted one-shot prompt shouldn't
read or write long-term memory unless asked.

`shared` across harnesses (`hlock launch opencode` on a Claude Code lock) has nothing to
share, since the formats differ (§2.7), so the mode falls back to `isolated` with a note.

### 2.4 Trials start from a seed

Every trial in `resolve`, `tune` and `evals run` runs with memory **off** unless the stack
says otherwise. For a code harness, `CommandAdapter` applies the profile's `off` (fixes
§1.2.3).

Memory becomes a search axis, as v0 §5.4 planned, with the values renamed to match what
can be measured:

```toml
[candidates.policy]
memory = ["off", "on"]
```

- `on` enables the harness's memory, starting from the task's **seed**: an optional
  `memory/` directory in the task's fixtures, in the common index-plus-topics shape. hlock
  writes it into the profile's memory location inside the trial's isolated root. Without a
  seed, `on` starts empty.
- Each trial gets a fresh copy, so memory never leaks between trials or seeds.
- With a seed, a task can test whether stale or poisoned memory misleads the agent: ship a
  `MEMORY.md` with a wrong fact and grade whether the agent checks it. An eval-pack guide
  section shows the pattern.
- A `remote` provider can't be reset, so a stack that includes one is refused in trials
  unless the manifest gives a per-trial namespace the server accepts
  (`[memory.providers.mem0] namespace = "hlock-{trial}"`). The refusal says why.
- The lock's evidence records the memory condition it was measured under
  (`[evidence] memory = "off"`), so a lock measured without memory doesn't claim to
  describe an agent that runs with it.

### 2.5 `audit` scans memory

`hlock audit` adds the profile's memory locations to what it scans (user scope only with
`--user`, the same as `pin`):

- **secrets**, with the detectors `pin` uses for placeholders: an API key the agent wrote
  into a memory file is reported with its file and line, and the value is never printed;
- **hidden characters**, with `scan.rs`, as for skills and instructions;
- **size**: an index longer than the harness loads (Claude Code: 200 lines or 25 KB)
  gets a note, because anything past that limit is silently ignored.

Detecting injected instructions in prose is out of scope (§3). Hidden characters and
secrets are the high-precision findings.

### 2.6 `migrate` and `localize` translate the memory entry (phase 2)

[`migrate`](migrate.md) and `localize` write a new lock for another harness, so they
translate `[memory]` the same way they translate skills and MCP servers. This ships with
§2.2, because without it a migrated lock would claim the target's memory is on just
because the source's was:

| Source entry | Target has a `[memory]` profile table | Target has none |
|---|---|---|
| `native.enabled = true/false` | Carried over. `sync` sets it through the target's own switch (`config#/features/memories` for Codex). If the target's memory can't be turned off, or is a preview, there's a note | Dropped, with a note ("OpenCode has no built-in memory; Claude Code's auto memory doesn't carry over") |
| A `local` provider | Carried over with its MCP server, as today | Same |
| A `remote` provider | Carried over with its MCP server, plus a note that its contents are shared and nothing moves | Same |

`localize` adds one rule. A lock that "runs entirely on your machine" can't send the
agent's memory to a hosted service, so a `remote` provider is flagged the way remote MCP
servers already are, and `--strict` drops it. The note names local alternatives from the
provider catalog (OpenMemory, Basic Memory).

Evidence follows the rules both commands already have: migrate clears it and keeps the
source's numbers as the baseline. `localize --evals` compares the local candidates with
that baseline, so it runs them under the baseline's memory condition (`[evidence]
memory`, §2.4). If the source lock doesn't record one, it assumes `off`, with a note.

Only the lock's entry changes. Memory *contents* stay where they are, so after a migrate
the target harness starts with empty memory. In `launch --memory shared` (§2.3) that's the
target's own, empty, location. `migrate` and `localize` say so in their notes and point to
`--memory` (§2.7). If `localize` keeps the source harness, nothing is lost: the memory
location doesn't change.

### 2.7 Moving memory contents between harnesses (phase 3)

```bash
hlock memory export [-o DIR]               # the harness's memory → index + topic files
hlock memory import DIR [--harness H]      # into H's memory location, after confirmation
hlock migrate --to codex --memory          # migrate, then copy memory across
hlock localize --memory                    # the same, when localize changes harness
```

`--memory` runs export and import after the lock is written. It copies into the target's
memory location for this machine, outside the lock, and only after confirmation (`-y` in
scripts). It never writes the contents into the new lock or `.hlock/`.

The export format is the shape the harnesses already share: a `MEMORY.md` index and one
markdown file per memory with `name`/`description`/`type` frontmatter. That's close to
Cognition's Agent Memory Repo, and we'd adopt that spec if it gets traction. Gemini's
section becomes one topic file per bullet. Import never overwrites existing memory: it
adds new files and reports conflicts. Third-party providers are out of scope here because
they already sell portability.

### 2.8 Promoting memory into the lock (phase 3)

The advice from vendors and practitioners is to promote durable memory into committed
instructions. The decision needs judgment, so it's a [meta harness](meta-harness.md) job:
`hlock meta run promote-memory` reads the memory, proposes edits to `AGENTS.md`/rules (or a
skill), and stops for review. hlock then re-pins and checks that the instruction files
changed and the lock still loads. Promoted knowledge becomes pinned, reviewed and
reproducible, and memory goes back to being a cache.

## 3. Non-goals

- **Storing or syncing memory contents.** Not in the lock, not vendored, not pushed to a
  team store. That's what the memory services are for, and a lock full of personal memory
  is the [pin-scope](pin-scope.md) leak problem again.
- **A memory provider of our own**, or an MCP server.
- **Judging memory quality** (staleness, contradictions) beyond what `audit` and seeded
  evals measure.
- **Prompt-injection detection** in memory prose.

## 4. Phasing

| Phase | Scope | Size |
|---|---|---|
| 1 | Profile `[memory]` (§2.1) for Claude Code, Codex, Gemini CLI and the autonomous agents; `pin`/`check` exclusion and Gemini section handling; trials run with memory `off` (§2.4, first bullet); `audit` scan (§2.5) | small: fixes the bugs in §1.2.2, §1.2.3 and §1.2.5 |
| 2 | Lock `[memory]` entry and provider catalog (§2.2); `launch --memory` (§2.3); `memory` search axis with seeds and evidence condition (§2.4); `migrate`/`localize` translate the entry (§2.6) | medium |
| 3 | `hlock memory export/import`, `migrate`/`localize --memory` (§2.7); `promote-memory` meta job (§2.8) | medium; wait for demand |

## 5. Open questions

1. **Default `launch` mode.** `shared` matches what users expect from their agent, but it
   means a launched session isn't fully defined by the lock. `isolated` is purer but
   surprises people. This proposal picks `shared` and states the mode on the launch line.
2. **Claude Code's slug.** `{cwd-slug}` follows Claude Code's own path mangling
   (`/a/b-c` → `-a-b-c`), and worktrees share one directory. We need to confirm the rule
   against the current version, or prefer `autoMemoryDirectory`, which avoids depending on
   it.
3. **Provider recognition.** Is a built-in catalog worth maintaining, or is
   `--memory` tagging plus tool-name heuristics (`add_memory`, `search_memories`) enough?
4. **Hosted memory** (Copilot Memory, Codex in the cloud) has no local path. Should it be
   recorded as `native.enabled` only, or as a `remote` provider?
