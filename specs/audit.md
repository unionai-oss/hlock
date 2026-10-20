# Audit and trust: content scanning, executable approval, and policy

Status: implemented (see [the guide](../docs/guides/audit.md)) · Owner: Niels Bantilan · Last updated: 2026-10-20 · Builds on [v0](v0.md) §7.7.8, [pull](pull.md), [secrets](secrets.md), [dependencies](dependencies.md)

> A lock you pulled from someone else is a program you're about to run. hlock should look
> at it before you do, ask before it installs anything that executes, and let a team say
> what it will allow.

---

## 1. Summary

`hlock pull` made it one command to get someone else's setup, and the
[dependencies](dependencies.md) spec makes it one command to add a skill from any git
repository. Both move text and code that the agent will act on from strangers onto your
machine. Today hlock checks digests (did the bytes change?) but not content (are these bytes
safe?), and `sync --yes` installs plugins and writes MCP servers that start processes,
with no step that singles those out.

[Microsoft APM](https://github.com/microsoft/apm) treats this as a first-class problem
("agent context is executable in effect — a prompt is a program for an LLM"), and has
shipped three pieces worth adopting:

1. **A hidden-Unicode gate** on every install. Tag characters, bidi overrides and
   variation selectors can hide instructions a reviewer never sees; APM blocks them and
   warns on zero-width characters. It's cheap, deterministic, and catches a real attack
   class (the "Glassworm" vector).
2. **An executable trust gate.** Hooks, binaries and self-defined MCP servers need an
   explicit `apm approve`; transitive MCP servers are blocked unless re-declared.
3. **An org policy file** (`apm-policy.yml`) that allows or denies sources, MCP servers and
   transports, inherits tighten-only from enterprise to repo, and is enforced in CI by
   `apm audit --ci`, with SARIF output.

This spec adopts all three, fits them onto hlock's existing steps (`pin`, `pull`, `sync`,
`launch`, `check`), and extends policy to what only hlock records: models, providers,
harness versions, and evidence.

```bash
hlock audit                       # scan the lock's files, verify digests, check policy
hlock audit --ci --format sarif   # for code scanning
hlock trust                       # list executables a lock would run; approve or deny
hlock export --format cyclonedx   # SBOM, including models
```

## 2. Goals and non-goals

Goals:

- Nothing that hides text from a reviewer reaches a harness through hlock without a
  visible decision.
- Before hlock installs or starts something that executes, you've seen it and approved
  that exact digest once.
- A team can write down which sources, MCP servers, providers and models are allowed, and
  CI fails when a lock breaks the rule.
- Output that existing tooling reads: SARIF for code scanning, CycloneDX for SBOMs.

Non-goals:

- Detecting prompt injection in plain text. No scanner does this reliably; APM says the
  same about its own. The pluggable scanner from v0 §7.7.8 stays the place for that.
- Signing locks or dependencies. Trust still comes from the repository, as in `pull`.
- Sandboxing the harness at runtime. That's the harness's job and the `[policy]` section's.

## 3. Content scanning

### 3.1 What's scanned

Every file hlock is about to place where a harness reads it: vendored skills, instruction
files and config (on `pin`, `pull`, `sync`, `launch`), fetched dependencies (on `add`), and
the prompts of eval packs and graders (on `evals check`, since a judge prompt is also
an agent's input).

### 3.2 What's found

The same tiers as APM, so a file that passes one tool passes the other:

| Severity | Characters | Effect |
|---|---|---|
| critical | Unicode tag characters (U+E0000–E007F), bidi overrides (U+202A–202E, U+2066–2069), variation selectors 17–256 | blocks the step |
| warning | zero-width characters, a BOM after the first byte, bidi marks, invisible operators | printed, recorded |
| info | non-breaking spaces and similar | recorded with `--verbose` |

A critical finding stops `pull`, `add` and `sync` before anything is written, naming the file,
line, column and code point. `--allow-hidden FILE` passes one file, and the lock records
the exception. `hlock audit --strip` removes the characters from vendored files and
re-digests them.

### 3.3 What the lock records

Each `[[skill]]`, `[[instructions]]` and `[[config]]` entry gets a `scan` key:
`"clean"`, `"warnings:3"`, or `"allowed:critical"`. A lock whose entries were never
scanned (written by an older hlock) is scanned on the next `check`.

## 4. Executable trust

### 4.1 What counts as executable

A lock can make your machine run code in five ways:

- stdio MCP servers (`command` + `args`);
- plugins and packages installed through the harness's CLI (`claude plugin install`,
  `pi install`), which may carry hooks;
- hooks in harness settings (`hooks` in Claude Code's `settings.json`, and the equivalents
  other profiles name);
- skill scripts (`scripts/` in a skill);
- the harness itself, installed at a pinned version from a pinned source.

### 4.2 The gate

`sync` and `launch` already build a plan. Executable steps get their own marker, and they
need an approval that `--yes` doesn't give:

```console
$ hlock sync alice
…
⚑  mcp      github      npx -y @modelcontextprotocol/server-github     not yet trusted
⚑  hook     PreToolUse  ~/.hlock/env/alice/…/hooks/guard.sh  sha256:3f1a…  not yet trusted
⚑  plugin   acme-tools  acme-marketplace@1.4.2                          not yet trusted

3 executables need your approval: hlock trust alice   (or re-run with --trust)
```

`hlock trust [LOCK]` lists each executable with its full command and digest and records
approvals in `~/.hlock/trust.toml`, keyed by digest. A changed digest needs approval again.

### 4.3 Trust follows digests, not locks

An earlier draft exempted "locks you pinned yourself". That can't be enforced: a
`harness.lock` is a file in git, and nothing in it proves who wrote it. A project lock you
pinned last month may have been edited since in a teammate's PR, or in a malicious one,
to add a hook or change an MCP server's `command`. That's the same attack as a
`postinstall` script in a `package.json` change, and `hlock sync --yes` after `git pull` is
exactly where it would land.

So the gate never asks where a lock came from, only whether you've approved each
executable's digest on this machine:

- **`hlock pin` approves what it captures.** Those executables already run on this
  machine, so pinning them adds their digests to `trust.toml`. Pinning and then syncing on
  the same machine never prompts.
- **After `git pull`, only what's new prompts.** If a teammate's change adds an MCP server
  or edits a hook, `sync` shows that one entry, not the whole lock.
- **A new machine prompts once.** The first `sync` of your project there lists every
  executable in one batch, and you approve them all with one answer. After that it's silent
  until something changes.
- **CI and scripts** pass `--trust` to approve whatever the lock contains, which is an
  explicit statement that the lock was reviewed some other way (for example, code review on
  the PR that changed it).
- **`[sources] trusted` in the policy** (§5) skips the prompt for executables from listed
  sources, such as the organization's own repositories.

The result is the same in the common case (no prompts for your own setup on your own
machine), and it catches the case that matters: an executable you've never seen arriving
through a file you didn't write.

### 4.4 Transitive MCP servers

When an `apm:` dependency or a plugin brings its own MCP servers, they're shown as
`⚑ mcp (via acme-tools)` and need approval like any other. This mirrors APM's
`--trust-transitive-mcp` without a separate flag.

## 5. Policy

### 5.1 The file

A project or organization writes `hlock-policy.toml`:

```toml
extends = "github:acme/agent-policy/hlock-policy.toml#sha256:8c1e…"   # tighten-only
enforcement = "block"                     # block | warn | off

[sources]
allow = ["github:acme/*", "github:anthropics/skills", "npm:@modelcontextprotocol/*"]
deny = ["github:*/awesome-*"]
trusted = ["github:acme/*"]               # skip the executable gate (§4)

[mcp]
transports = ["stdio", "http"]           # no sse
deny = ["npm:*-filesystem-*"]

[models]                                  # only hlock can enforce these
providers = ["anthropic", "openai", "ollama"]
deny = ["*:*-preview"]

[harness]
allow = ["claude-code", "codex", "opencode"]
min-version = { claude-code = "2.1" }

[evidence]                                # resolved locks only
min-success-rate = 0.8
max-cost-usd-per-task = 0.50
max-age-days = 90                        # re-measure after models change under you
```

`extends` pins the parent policy by hash, so a change upstream doesn't silently change what
a repo allows. Inheritance only tightens: a child can deny more and require more, never
allow something its parent denies.

### 5.2 Where it's enforced

- `pin`, `add`, `pull`, `sync`: a denied source, server, provider or harness blocks the
  step under `enforcement = "block"`.
- `check` and `audit`: report every violation; `audit --ci` exits non-zero and can't be
  relaxed by flags, only by changing the policy file (APM makes the same distinction between
  install-time bypass and CI).
- `resolve`: candidates the policy denies are dropped before the search, and the summary
  says which.

### 5.3 What APM's policy doesn't cover

APM governs what's installed. hlock's lock also says which model runs, through which
provider, on which harness version, and how well that combination did on your tasks. So a
team can write "only these providers", "no preview models", and "a lock must show at least
80% success on our pack, measured within 90 days" in one file, and CI checks all of it.

## 6. `hlock audit`

One command for CI and for a reviewer looking at a lock diff:

1. **Integrity:** every vendored, fetched and installed file matches its digest.
2. **Content:** the scan of §3, over the same files.
3. **Executables:** lists them, with trust status.
4. **Policy:** §5.
5. **Secrets:** no `[[config]]` or vendored file contains a value that `pin`'s redaction
   rules would have replaced (a regression guard for hand-edited vendor files).

Output: a table, `--json`, or `--format sarif` for GitHub code scanning. `--ci` turns
warnings about the lock's own content into failures.

## 7. SBOM export

`hlock export --format cyclonedx` (or `spdx`) writes an SBOM of the lock. Beyond what
APM's `apm lock export` covers (packages, commits, hashes, declared licenses), it includes
the harness and its install source, inference runtimes, and each model as a CycloneDX
`machine-learning-model` component, with provider, revision and digest where `pin`
recorded one. That's the piece an AI-BOM review asks for and a package-only SBOM can't
answer.

## 8. Phasing

1. Hidden-Unicode scan on `pull`, `sync`, `add` and `pin`; `scan` in the lock; `audit`
   integrity and content checks.
2. Executable gate and `hlock trust`.
3. `hlock-policy.toml` with `sources`, `mcp`, `harness`; `audit --ci --format sarif`.
4. `[models]` and `[evidence]` policy; `resolve` honoring the policy; SBOM export.

## 9. Decisions

- **Approvals are per machine and don't sync** (§4.3). `~/.hlock/trust.toml` stays local;
  a new machine gets one batched prompt. Syncing it (say, through `hlock secret`'s keychain
  backends) would save that one prompt, but a synced trust store is one more place an
  attacker could write approvals to. Revisit if users report the first-sync prompt as
  friction.
- **Policy comes from the project only.** hlock reads `hlock-policy.toml` next to the lock,
  plus whatever it names with `extends` (pinned by hash). There's no user-level or
  machine-level default file, so a lock is judged the same way in CI and on every laptop,
  and an organization's policy reaches a repo only by being referenced from it.
- **Both `pull` and `sync` scan.** `pull` scans before it writes anything into
  `~/.hlock/pulls/`, because people open pulled files in editors, and an editor that renders
  bidi overrides shows a reviewer different text from what the agent reads. `sync` scans
  again before placing files, since a pulled lock can be refreshed or edited in between.

## 10. Implementation notes

Choices the implementation made where this spec left room:

- **`scan` keys** are `clean`, `warnings:N`, `critical:N` and `allowed:critical`: `critical:N`
  records what `pin` found on your own machine (it warns rather than refuses).
- **`hlock add` approves what you add** (you saw it and asked for it); an MCP server a package
  brings still waits for `hlock trust`, as do executables arriving through `git pull`.
- **Digests**: an MCP server's covers its command, arguments, environment variable names and
  the values that aren't secret placeholders; a hook's its event, matcher and command; a
  setting's its value; a plugin's kind, name, version, source, revision, and its marketplace's
  source and revision; the harness's name, version and source. A script a command line names
  is hashed in when it exists here. A remote MCP server a package brings needs approval too.
- **Skill scripts** are approved by the skill's pinned digest, not the copy installed now (a
  declared skill's upgrade asks again). The lock lists each skill's `scripts`; sync refuses a
  skill that carries unlisted ones, and a lock older than the key asks about skills it can't
  see yet.
- **Settings that run things** beyond hooks and MCP servers come from a built-in list of JSON
  pointers looked up in every config file (`/statusLine/command`, `/apiKeyHelper`,
  `/env/NODE_OPTIONS`, `/enabledPlugins`, `/notify`, `/tools/discoveryCommand`, `/plugin`, …).
- **`allowed:critical`** in a lock is an exception its author wrote: it's approved per machine
  like an executable (kind `hidden`, by the content's digest). `--allow-hidden` on `pull` or
  `add` approves it.
- **`[sources] trusted`** counts on a machine only once the list itself is approved (kind
  `policy`), since the policy sits in the repository with the lock. A source counts only where
  it's verified: an MCP command that runs the named package, a skill fetched from its source,
  a remote server's URL.
- **`launch`** asks for approval on a terminal and fails without one unless `--trust`; it
  leaves out what isn't trusted or hides text, and says so, including when translating into
  another harness. A code harness (`launch` or `run`) needs everything trusted.
- **Scanning** decodes what it can of a file that isn't valid UTF-8, skips only files named as
  binary, treats a file over 20 MB as critical, and scans the parsed strings of JSON, TOML and
  YAML files. `--strip` keeps zero-width non-joiners and bidi marks, and refuses when a vendored
  file doesn't match its digest.
- **Policy subjects** are sources without their ref (`github:OWNER/REPO/PATH`, `npm:PKG`, …),
  and a pattern matching a repository matches the paths in it. Unknown policy keys are errors.
- **`audit`** doesn't fail on untrusted executables (a CI runner approves nothing); `--ci`
  fails on content warnings. SARIF uses APM's `apm/hidden-unicode/<category>` rule ids.
- **SBOMs** use deterministic serial numbers derived from the lock, so the same lock exports
  the same document.
