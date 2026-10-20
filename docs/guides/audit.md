# Audit and trust

A lock you pulled from someone else is a program you're about to run. hlock looks at what a
lock would place before you do, asks before it installs anything that executes, and lets a
team write down what it allows:

- **Hidden characters.** Every file hlock is about to place where a harness reads it is
  scanned for Unicode that hides text from a reviewer, the way
  [APM](https://github.com/microsoft/apm) scans on install.
- **Executables.** MCP servers, plugins, hooks, settings that run a command, and skill
  scripts need an approval on this machine, by digest, that `--yes` doesn't give.
- **Policy.** An `hlock-policy.toml` says which sources, MCP servers, providers, models and
  harnesses a project allows, and how good the evidence must be.
- **`hlock audit`** checks all of it, plus integrity and secrets, for a reviewer or for CI
  (with SARIF for code scanning), and `hlock export` writes an SBOM.

## Hidden characters

Bidi overrides, tag characters and some variation selectors make text read one way in an
editor and another way to the model. A skill can say "Review the diff." on screen and carry
an instruction after it that only the agent sees. hlock scans the same tiers as APM, so a
file that passes one tool passes the other:

| Severity | Characters | Effect |
|---|---|---|
| critical | Unicode tag characters (U+E0001–E007F), bidi overrides and isolates (U+202A–202E, U+2066–2069), variation selectors 17–256 (U+E0100–E01EF), unpaired surrogates | stops `pull`, `add`, `sync` and `launch` |
| warning | zero-width characters, a BOM after the first byte, bidi marks, invisible math operators, variation selectors 1–15, soft hyphens, line and paragraph separators, Hangul fillers, the combining grapheme joiner | printed and recorded |
| info | non-breaking and unusual spaces, a leading BOM, a zero-width joiner inside an emoji | `hlock audit -v` |

A file isn't skipped because it has a stray NUL byte or isn't valid UTF-8: what decodes is
scanned. Only files named as binary (`.png`, `.zip`, `.woff`, …) are left out, and a file over
20 MB counts as critical, since it can't be scanned. In JSON, TOML and YAML files the parsed
strings are scanned too, so an escaped `"\u202e"` (plain ASCII in the file) is found as
the character the harness reads.

A critical character stops `pull` before it writes anything, naming the file, line,
column and code point:

```console
$ hlock pull github:alice/agent-setup
✗ error: github:alice/agent-setup has characters that hide text from a reviewer:
  .hlock/vendor/claude-code/skills/review/SKILL.md:1:17: U+202E right-to-left override (bidi-override)
nothing was written. Look at the file in an editor that shows them; if they're meant to be there, pass --allow-hidden .hlock/vendor/claude-code/skills/review/SKILL.md
```

`hlock add` does the same for a dependency it fetched, before anything is declared or
installed. `sync` and `launch` scan what they're about to place again, because a lock can
change between a pull and a sync: a step with a critical character becomes a manual step
that says why, and a fetched dependency is scanned once it arrives. `hlock evals check`
scans an eval pack's prompts and judge rubrics, which an agent reads too. `hlock pin`
records what's on your machine, so it warns rather than refuses.

`--allow-hidden FILE` (a file, or a directory of them) lets one through on purpose, and the
lock records the exception. FILE is the path the error names; a path of two or more parts
(`skills/review/SKILL.md`) also matches the end of a longer one, but a bare file name doesn't
match every file of that name. Each `[[skill]]`, `[[instructions]]` and `[[config]]` entry has
a `scan` key: `clean`, `warnings:N`, `critical:N`, or `allowed:critical`. `hlock check` scans
entries an older hlock never scanned, and fails on a critical character the lock didn't
allow.

An exception in a lock isn't enough on its own, since whoever wrote the lock wrote the
exception too. On another machine, `allowed:critical` content is listed by `hlock trust`
(kind `hidden`) and isn't placed until it's approved there, by the content's digest. The
`--allow-hidden` you pass to `pull` or `add` approves it on your machine.

`hlock audit --strip` takes the hidden characters out of vendored files (keeping the info
ones, and zero-width non-joiners and bidi marks, which are part of ordinary Persian, Hebrew
and Indic text) and updates their digests and scans in the lock. It refuses when a vendored
file doesn't match its digest: re-digesting it would record the edit as what's pinned.

## Executables

A lock can make your machine run code in several ways: stdio MCP servers (a command and its
arguments), plugins installed through the harness's CLI, hooks in harness settings, other
settings that run a command or load code (a status line command, an API key helper,
`NODE_OPTIONS` in `env`, enabled plugins and their marketplaces, Codex's `notify`, Gemini's tool
discovery command, OpenCode's `plugin`), skill scripts (a skill's `scripts/`), and the harness
itself from its install source. A remote MCP server a package brings counts too: it gets what
the agent sends it. `sync` marks each one it would install, and it needs an approval that
`--yes` doesn't give:

```console
$ hlock sync --yes
Sync plan for Claude Code 2.1.3 → .hlock/env/claude-code/config
✓  harness  Claude Code 2.1.3
✓  mcp      `npx` for MCP servers: github
●  skill    Copy skill review → .hlock/env/claude-code/config/skills/review
○  config   Write .hlock/env/claude-code/config/settings.json  ⚑ runs hook PreToolUse Bash, not yet
            trusted: approve with hlock trust, or sync --trust
○  config   Write .hlock/env/claude-code/config/.claude.json /mcpServers  ⚑ runs mcp github, not yet
            trusted: approve with hlock trust, or sync --trust
○  secret   HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)  not set
⚑  mcp      github  npx -y @modelcontextprotocol/server-github  sha256:bd687d33…  not yet trusted
⚑  hook     PreToolUse Bash  ~/.claude/hooks/guard.sh  sha256:791b1ae2…  not yet trusted

2 executables need your approval: hlock trust   (or re-run with --trust)
```

On a terminal, `sync` asks once for all of them. `hlock trust` lists each executable with
what it runs and its digest, and records your approvals in `~/.hlock/trust.toml`:

```console
$ hlock trust --yes
   kind  name             runs                                        digest        status
⚑  mcp   github           npx -y @modelcontextprotocol/server-github  bd687d330ce4  not yet trusted
⚑  hook  PreToolUse Bash  ~/.claude/hooks/guard.sh                    791b1ae250ca  not yet trusted
✓ approved 2  (recorded in ~/.hlock/trust.toml)
next → hlock sync
```

`--only NAME` approves some of them (a name, or at least 8 characters of a digest), and `--deny
NAME` denies one: `sync` won't install it, and won't ask again. `launch` and `run` ask the same
question before they start the harness, and fail when there's no terminal to ask on unless you
pass `--trust`. Launching into another harness leaves out what isn't trusted, and a code
harness, which gets the lock's MCP servers and skills as they are, doesn't start until all of
them are. Commands and names are shown with hidden characters made visible (`<U+202E>`), so
what you approve is what runs.

### Trust follows digests, not locks

A `harness.lock` is a file in git, and nothing in it says who wrote it. A lock you pinned
last month may have been edited since, in a teammate's PR or a malicious one, to add a hook or
change an MCP server's command. So hlock never asks where a lock came from, only whether
you've approved each executable's digest on this machine:

- An MCP server's covers its command, its arguments, the names of its environment variables,
  and the values that aren't secrets (`NODE_OPTIONS=--require ./x.js` changes what runs; a
  token doesn't).
- A hook's covers its event, its matcher and its command; a setting's, its value.
- A plugin's covers its version and source, and where its marketplace is and the revision it's
  pinned at.
- A script a command names (`~/.claude/hooks/guard.sh`, `$CLAUDE_PROJECT_DIR/hooks/a.py`,
  `node ./server.js`) is part of the digest when it's on this machine, so editing the script
  asks again.
- A skill's scripts are approved as the lock pins the skill, by its digest, which `sync` checks
  what it fetches or copies against. The lock lists each skill's `scripts`, so a declared
  skill's upgrade that brings new scripts asks again before it's fetched, and a skill that
  carries scripts its entry doesn't list isn't placed.

- **`hlock pin` approves what it captures.** Those executables already run here, so pinning and
  then syncing on the same machine never asks.
- **`hlock add` approves what you add**, since you asked for it. An MCP server a package brings
  still waits for `hlock trust`.
- **After `git pull`, only what's new asks.** If a teammate's change adds an MCP server or edits
  a hook, `sync` shows that one entry, not the whole lock.
- **A new machine asks once**, for everything, and you answer once.
- **CI and scripts** pass `--trust` to approve whatever the lock holds: a statement that the lock
  was reviewed some other way, such as code review on the PR that changed it.
- **`[sources] trusted`** in the [policy](#policy) skips the question for executables from the
  sources it lists, such as the organization's own repositories, once you've approved the list
  itself. The policy is in the repository with the lock, so whoever can edit one can edit the
  other: `hlock trust` lists the trusted list (kind `policy`) like an executable, and a changed
  list asks again. A source counts only where it's what runs: an MCP server whose command runs
  the package its entry names (`npx -y PKG` for `npm:PKG`), a skill fetched from its source,
  or a remote server's URL. An entry that claims `source = "npm:@acme/x"` but runs `sh` isn't
  trusted by it.

Approvals stay on the machine. `~/.hlock/trust.toml` isn't synced: a synced trust store would
be one more place an attacker could write approvals to.

## Policy

A project writes `hlock-policy.toml` next to its lock. Every key is optional:

```toml title="hlock-policy.toml"
extends = "github:acme/agent-policy/hlock-policy.toml#sha256:8c1e…"   # tighten-only
enforcement = "block"                     # block | warn | off

[sources]
allow = ["github:acme/*", "github:anthropics/skills", "npm:@modelcontextprotocol/*"]
deny = ["github:*/awesome-*"]
trusted = ["github:acme/*"]               # executables from these need no approval

[mcp]
transports = ["stdio", "http"]            # no sse
deny = ["npm:*-filesystem-*"]

[models]
providers = ["anthropic", "openai", "ollama"]
deny = ["*:*-preview"]

[harness]
allow = ["claude-code", "codex", "opencode"]
min-version = { claude-code = "2.1" }

[evidence]                                # resolved locks only
min-success-rate = 0.8
max-cost-usd-per-task = 0.50
max-age-days = 90
```

A denied source, MCP server, provider, model or harness stops `pin`, `add`, `pull` and
`sync` under `enforcement = "block"`, before anything is written:

```console
$ hlock sync --plan
✗ error: harness.lock breaks the project's policy:
  harness claude-code isn't in [harness] allow (hlock-policy.toml)
change the lock, or the policy (enforcement = "warn" to only warn)
```

`hlock check` and `hlock audit` report every violation, and `hlock resolve` drops the
candidate stacks the policy denies before it measures anything (the lock lists them under
`[provenance.search] dropped-by-policy`). `enforcement = "warn"` prints violations without
stopping, and `off` turns the policy off.

The policy comes from the project only: the file next to the lock, plus whatever it names with
`extends`. There's no user-level default, so a lock is judged the same way in CI and on every
laptop, and an organization's policy reaches a repository only by being referenced from it.
`extends` pins the parent by hash, so a change upstream doesn't silently change what a
repository allows. Inheritance only tightens: every level's rules apply, so a child can deny
more and require more, but can't allow what its parent denies. See the
[policy reference](../reference/policy.md) for every key and the pattern syntax.

## `hlock audit`

One command for CI, and for a reviewer looking at a lock diff:

```console
$ hlock audit
✓ 1. Integrity
✓ 2. Content  3 files scanned
✓ 3. Executables  2 found, 2 trusted here
   mcp      github  npx -y @modelcontextprotocol/server-github  trusted
   hook     PreToolUse Bash  ~/.claude/hooks/guard.sh  trusted
✓ 4. Policy
✓ 5. Secrets
✓ harness.lock passed  (0 warnings)
```

1. **Integrity:** every vendored and fetched file matches its digest in the lock.
2. **Content:** the hidden-character scan, over the same files.
3. **Executables:** what the lock would run, with its trust status here.
4. **Policy:** the project's `hlock-policy.toml`.
5. **Secrets:** no `[[config]]` or vendored file holds a value `pin`'s redaction would have
   replaced (a guard against hand-edited vendored files).

When a vendored skill was edited after it was pinned and hides text:

```console
$ hlock audit
✗ 1. Integrity
   error  skill review: doesn't match its digest in the lock: it was edited after it was pinned
✗ 2. Content  3 files scanned
   critical  .hlock/vendor/claude-code/skills/review/SKILL.md:5:17  U+202E right-to-left override
✓ 3. Executables  2 found, 2 trusted here
   mcp      github  npx -y @modelcontextprotocol/server-github  trusted
   hook     PreToolUse Bash  ~/.claude/hooks/guard.sh  trusted
✓ 4. Policy
✓ 5. Secrets
✗ audit failed: 2 errors, 0 warnings
  hint: a vendored file was edited after it was pinned: restore it (git checkout), or re-pin with
hlock pin
```

A declared dependency that isn't fetched on the machine the audit runs on can't be checked
there: it's a warning, and `sync` checks its digest as it fetches it.

`audit` exits `1` on errors. `--ci` also fails on warnings about the lock's own content (hidden
characters, words that look like credentials), and can't be relaxed by flags: only by changing
the policy file. An installed copy that drifted from the lock, or a policy in `warn` mode,
doesn't fail it. Executables that aren't trusted on the machine the audit runs on are listed,
but don't fail it (a CI runner has no approvals). A skill pinned with a zero-width space fails
`--ci` until `--strip` cleans it:

```console
$ hlock audit --ci
✓ 1. Integrity
✓ 2. Content  3 files scanned
   warning  .hlock/vendor/claude-code/skills/review/SKILL.md:5:7  U+200B zero-width space
✓ 3. Executables  2 found, 2 trusted here
   mcp      github  npx -y @modelcontextprotocol/server-github  trusted
   hook     PreToolUse Bash  ~/.claude/hooks/guard.sh  trusted
✓ 4. Policy
✓ 5. Secrets
✗ audit failed: 0 errors, 1 warning  (--ci counts warnings about the lock's content)
  hint: hlock audit --strip removes hidden characters from vendored files
$ hlock audit --strip --ci
✓ stripped hidden characters from .hlock/vendor/claude-code/skills/review/SKILL.md
✓ 1. Integrity
✓ 2. Content  3 files scanned
✓ 3. Executables  2 found, 2 trusted here
   mcp      github  npx -y @modelcontextprotocol/server-github  trusted
   hook     PreToolUse Bash  ~/.claude/hooks/guard.sh  trusted
✓ 4. Policy
✓ 5. Secrets
✓ harness.lock passed  (0 warnings)
```

`--format json` prints the report, and `--format sarif` prints SARIF 2.1.0 for GitHub code
scanning, with APM's rule ids for hidden characters (`apm/hidden-unicode/<category>`) so the
two tools' alerts line up. See [Using hlock in CI](ci.md#audit-the-lock).

## SBOM

`hlock export --format cyclonedx` writes a CycloneDX 1.6 SBOM of the lock, and
`--format spdx` an SPDX 2.3 one. Beyond packages, commits and hashes, it includes the harness
and its install source, inference runtimes, MCP servers, skills, plugins, APM packages, and
each model as a CycloneDX `machine-learning-model` component with its provider, revision and
digest where the lock has them:

```console
$ hlock export --format cyclonedx
✓ wrote sbom.cdx.json  (cyclonedx, 4 components, 1 model)
```

`-o FILE` writes elsewhere, and `-o -` writes to stdout.
