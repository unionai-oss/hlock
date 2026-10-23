# Desktop apps: pin, check, sync and migrate GUI agent harnesses

Status: phase 1 implemented, more apps added (§10) · Owner: Niels Bantilan · Last updated: 2026-10-23 · Builds on [v0](v0.md) §7.8, [migrate](migrate.md), [meta harness](meta-harness.md), [dependencies](dependencies.md)

> More and more agent setup lives in desktop apps: Claude Desktop, VS Code with Copilot,
> Cursor, Zed. Their MCP servers, instructions and skills are plain files, often the same
> files their CLI siblings read. A new profile kind, `kind = "app"`, lets hlock find these
> apps, read their versions, and pin, check, sync and migrate their setup like any other
> harness. An app reads only its own config, so `sync` writes it only when asked
> (`--apps`, or `--in-place`), and says when to quit or restart the app. hlock can't
> start a GUI app against an isolated config, so `launch`, `run`, `resolve` and `tune`
> refuse apps. A missing app is installed with `sync --install` where Homebrew can, and
> otherwise handed to the meta harness: `hlock meta run install`.

---

## 1. Problem

hlock's harness profiles describe agent CLIs and code frameworks. A growing share of agent
setup lives in desktop apps instead:

| App | Where its agent setup lives | What hlock covers today |
|---|---|---|
| Claude Desktop | Chat MCP servers in `claude_desktop_config.json` under `/mcpServers` (stdio only). The Code tab reads `~/.claude`, the same files as Claude Code. Connectors, chat skills, projects, Desktop Extensions and the model live in the app or the claude.ai account. | The Code tab, through `claude-code` |
| VS Code + Copilot | `mcp.json` in the user settings directory (key `servers`), `.vscode/mcp.json`, `.github/copilot-instructions.md`, `.github/instructions/*.instructions.md`, `AGENTS.md`, skills in `.github/skills`, `.agents/skills`, `~/.copilot/skills`. | No (`copilot-cli` covers the Copilot CLI only) |
| Cursor (the IDE) | `~/.cursor/mcp.json` and `.cursor/mcp.json`, shared with the Cursor Agent CLI; `.cursor/rules/*.mdc`. User Rules live in the app. | The shared files, through `cursor-agent` |
| Zed | `context_servers` in Zed's `settings.json` (JSON with comments) and `.zed/settings.json`; the first of `.rules`, `AGENTS.md`, `CLAUDE.md`, … | No |

For an app, hlock can't today:

1. **Find it or read its version.** Apps have no `--version` flag on `PATH`; on macOS the
   version is `CFBundleShortVersionString` in the bundle's `Contents/Info.plist`.
2. **Write its MCP config.** VS Code keys servers under `servers` with `type: "stdio"` /
   `"http"`; Zed under `context_servers`. Neither is a shape hlock writes.
3. **Find its files on every OS.** Apps keep config in the OS's application-support
   directory (`~/Library/Application Support/Code/User` on macOS, `~/.config/Code/User`
   on Linux, `%APPDATA%\Code\User` on Windows), and a profile path is one template.
4. **Say what it can't restore.** Connectors, chat projects and app-managed rules live in a
   vendor account or app database. A user who syncs a Claude Desktop lock should be told,
   not left to assume the setup is complete.
5. **Restore it safely.** Claude Desktop needs a full quit and relaunch to load MCP
   changes, and has been [reported](https://github.com/anthropics/claude-code/issues/32345)
   to rewrite its config file while it runs, dropping `mcpServers`.

And several commands would do the wrong thing if an app profile were added as
`kind = "cli"`: `launch` and `run` would exec it, `resolve` and `tune` would try to run it
headless, and `sync` would write into an isolated root under `.hlock/env/` that the app
never reads.

## 2. Goals and non-goals

### Goals

1. `pin`, `check`, `sync`, `diff`, `audit` and `migrate` work for desktop apps, driven by
   profiles. App-specific code is limited to detection, two MCP shapes, and the sync
   rules in §3.4.
2. `[target] also` can name an app, so a Claude Code lock also puts its MCP servers in
   Claude Desktop, without moving Claude Code out of its isolated root.
3. A lock records the app's version and install source; `sync --install` installs a
   missing app through Homebrew, and `hlock meta run install` can install it otherwise.
4. Setup that lives in an account or app database is named, with a link, so the result
   is honest about what it restored.
5. Commands that need to start the harness refuse apps, saying what to do instead.

### Non-goals

- **Starting apps.** No `launch`, `run`, `resolve`, `tune`, `evals run`, `localize`, or
  apps as meta harnesses.
- **Account-side or app-database settings.** hlock doesn't read or write connectors, chat
  projects, Desktop Extensions (`.mcpb`), Cursor User Rules or Zed's Rules Library.
- **App settings files** (`settings.json` of VS Code and Cursor). They hold flat dotted
  keys (`"chat.agent.enabled"`) that JSON pointers can't select as a group, and mostly
  editor preferences.
- **VS Code profiles other than the default** (`User/profiles/<id>/mcp.json`).
- **A GUI for hlock**, and **new CLI harnesses** from the same research (the DeepSeek
  harness `dsh` is an ordinary `cli` profile).

## 3. Design

### 3.1 `kind = "app"`

A harness profile's `kind` gains a fourth value, `app`: a desktop application a person
starts. hlock never executes it.

| Command | With an app |
|---|---|
| `hlock harness list`, `hlock harness show` | Listed under **Desktop apps**; `show` prints the app path, version, source, and what isn't restored (§3.8) |
| `hlock pin` | As for a CLI, but the user scope is pinned by default (§3.5). Auto-detection (`Registry::select`) still considers CLIs only, so `pin` names an app with `--harness` |
| `hlock check` | As for a CLI; a newer app version isn't drift (§3.7) |
| `hlock sync` | Writes the app's own config only with `--apps` or `--in-place` (§3.4); installs the app with `--install` (§3.7) |
| `[target] also` | Accepts apps (§3.6) |
| `hlock migrate --to` | Allowed to and from apps (§3.10) |
| `hlock diff`, `audit`, `trust`, `upgrade`, `show` | Unchanged: they work on the lock |
| `launch`, `run`, `resolve`, `tune`, `evals run --harness`, `localize`, `init --harness`, `meta init --harness` | Refused: "Claude Desktop is a desktop app, and hlock can't start it. `hlock sync --apps` puts the setup in place; then open the app." |

`init --harness` is refused because `init` writes a manifest for `resolve`; a manifest's
`[target] harness = "claude-desktop"` is refused where `resolve` and `tune` read it
(`resolver::apply_target`).

Where the code asks `kind == "cli"` meaning "an agent whose config hlock writes"
(`migrate`, `pin`'s not-installed warning, the sync harness step), apps are included.
Where it means "a program hlock can exec" (`launch`, `run`, resolve candidates, `meta`),
apps are refused before the existing `kind != "cli"` branch can treat them as a code
harness. §5 lists the call sites.

### 3.2 Finding an app and its version: `[app]`

```toml
id = "claude-desktop"
name = "Claude Desktop"
kind = "app"
binaries = []                         # CLI shims only; never the app's own executable

[app]
bundles = ["Claude.app"]              # macOS: looked for in /Applications and ~/Applications
paths.windows = ["~/AppData/Local/AnthropicClaude/claude.exe"]   # more install locations per OS
process = "Claude"                    # the running app's process name (§3.4)
quit-to-write = true                  # quit the app before hlock writes its config
reload = "restart"                    # restart | auto: how it picks up changes
```

`probe_harness` sends `kind = "app"` to a new `probe_app` (today any non-`cli` kind gets
the code-harness probe):

1. **macOS:** the first of `bundles` in the bundle search roots: `/Applications`, then
   `~/Applications`, or the `:`-separated `HLOCK_APP_DIRS` when set. The version is
   `CFBundleShortVersionString` from `Contents/Info.plist` (else `CFBundleVersion`). hlock
   parses an XML plist itself; a binary plist (it starts with `bplist00`) is converted by
   `/usr/bin/plutil -convert xml1 -o - <file>`, by absolute path, with the usual
   8-second timeout.
2. **Every OS:** the first `paths.<os>` entry that exists.
3. **Every OS:** the profile's `binaries` on `PATH`, versioned with `version.args`. These
   are CLI shims (`code`); the version is the first version on the first line that has
   one (`code --version` prints version, commit and architecture). A profile never lists
   the app's own executable here: probing would run it, and a GUI app would open.

The first hit sets `installed` and `binary` (the bundle or path); the version comes from
the first source that has one, so a bundle with an unreadable plist still counts as
installed, and a shim can supply the version. On Windows and Linux without a shim, an app
is reported installed with no version.

**Install method.** `detect_install` already maps a real path to a method: `/Caskroom/…/`,
and the existing `app` method's `path-hints`, as the Ollama and LM Studio runtime profiles
use. A bundle's path has no trailing slash, so an app's hint is `"/Claude.app"`, not
`"/Ollama.app/"`-style. An app bundle isn't under `Caskroom`, so `probe_app` also
checks `<prefix>/Caskroom/<package>` for the profile's `brew-cask` package, with
`<prefix>` each of `HOMEBREW_PREFIX`, `/opt/homebrew` and `/usr/local`. Otherwise the
method is `app`. The lock's `source` is `brew-cask:claude@1.0.3` or `app@1.0.3`, in the
existing `source_spec` format.

**OS name.** `Ctx::os()` returns `HLOCK_OS` from the context's environment when set
(tests use it), else `std::env::consts::OS` (`macos`, `linux`, `windows`).

### 3.3 Paths: `{app-config}`

A new path placeholder, `{app-config}`, is the OS's per-user application config directory:

| OS | `{app-config}` |
|---|---|
| macOS | `~/Library/Application Support` |
| Windows | `%APPDATA%`, else `~/AppData/Roaming` |
| Linux and others | `$XDG_CONFIG_HOME`, else `~/.config` |

It works wherever `~` does (`config-root.default`, `files.*.path`, `skills`,
`instructions`, `[app] paths`), and is expanded in `Ctx::expand`. VS Code's root is
`{app-config}/Code/User` on all three OSes, and Claude Desktop's `{app-config}/Claude`.

**Isolation.** An app has no config-root variable, so `isolation_env` returns `None` and
`sync --prefix` relocates `HOME` (`sync::target_ctx`). In that branch, and for docker,
`target_ctx` also removes `XDG_CONFIG_HOME` and `APPDATA` from the relocated context, so
`{app-config}` follows the relocated home. Without that, a "copy under a prefix" would
land in the user's real directory on Linux and Windows. The branch where a profile's own
config-root variable is `XDG_CONFIG_HOME` (OpenCode) is untouched.

Windows paths in phase 1 profiles come from vendor docs and are unverified on a real
install; the profiles say so in a comment. Claude Desktop installed as an MSIX package
(the Microsoft Store, winget) reads a virtualized copy of `%APPDATA%\Claude`; phase 2
handles it once tried on a real install.

### 3.4 Sync: an app reads only its own config

`sync`'s default, an isolated root under `.hlock/env/`, is a directory an app never
reads. Reporting "in sync" after writing there would be misleading, and writing into the
user's real config without being asked would break hlock's rule that `sync` doesn't
change your own config unless you opt in. So writing an app's own config is its own
opt-in, **`--apps`**, and `--in-place` implies it.

**An app lock** plans against the real paths (`mode = "home"`), unless `--prefix` or
`--docker` names another place:

- Core's `isolated()` doesn't add the default `.hlock/env` prefix for an app. A prefix
  the CLI adds by default for a pulled lock (`_pull_env`, cli.py) is passed as a default,
  not an explicit `--prefix` (`SyncOptions.default_prefix`), so it's dropped for an app
  too. A pulled Claude Desktop lock therefore plans against Claude Desktop's real config,
  which is what rehydrating it means.
- In `home` mode, every automatic step whose target is outside the project becomes
  **manual** without `--apps`: the reason is "Claude Desktop reads only your own config;
  `hlock sync --apps` writes it", the action is removed, and the step records
  `needs: "apps"`. Steps inside the project (`.vscode/mcp.json`, `.github/skills/`) stay
  automatic, as for CLIs. A harness install (`Run`) is gated by `--install` as before.
- "Outside the project" is `capture::is_project_path` (made `pub(crate)`), called with the
  app's config root and the real context, on the step's target path: `WriteConfig.path`,
  `WriteMcp.dest.path`, `CopyDir.to`, `CopyFile.to`, `GitSkill.to`, `FetchDep`'s
  destination, `WriteHooks.path`, `WriteFile.path`, `Remove.path`.
- With `--prefix DIR`, nothing is gated: it writes a copy under a relocated home, for
  inspection and tests, and the success message says the app won't read it. `--docker` is
  refused for an app lock ("Claude Desktop can't run in a container"). `--apps` writes your
  own config, so like `--in-place` it doesn't combine with `--prefix` or `--docker`.
- Precedence: an explicit `--prefix`, else (for an app) none, else the caller's default
  prefix, else `.hlock/env`. `plan()` notes whether the prefix was explicit before
  `isolated()` fills in the default.

**`[target] also` apps** (§3.6) are planned against their real paths in every mode except
docker and an explicit `--prefix`, where they're skipped with a note. Their `needs:
"apps"` steps are **optional**, so a plain `hlock sync` of a Claude Code lock with
`also = ["claude-desktop"]` still exits `0` once Claude Code is in place; they're listed
under the plan with the `--apps` hint. `hlock launch` never writes apps: it plans with an
empty `also`.

**Asking.** Interactively, without `--yes`, `--plan` or `--json`, when steps need `--apps`,
`sync` asks `Write Claude Desktop's config in ~/Library/Application Support/Claude? [y/N]`
and on yes plans again with `--apps`. Otherwise it doesn't ask.

**Running apps.** With `[app] quit-to-write = true` (Claude Desktop), steps that would
write the app's config are manual while the app runs, whatever the flags (a `--prefix`
copy isn't the app's config, so it isn't gated): "Claude Desktop
is running and may overwrite the file; quit it, then run `hlock sync --apps`". Detection
is `/usr/bin/pgrep -x <process>` on macOS and Linux, by absolute path; elsewhere, or when
`pgrep` is missing, the app is assumed not running. `HLOCK_APPS_RUNNING` (comma-separated
profile ids) overrides detection, for tests. Apps that reload their config live (VS Code)
don't set it.

**Reload.** After a sync that wrote an app's config, the last line says how the app picks
it up, from `[app] reload`: `restart` "quit and reopen Claude Desktop to load the
changes"; `auto` nothing.

The plan JSON gains `apps` (the profile ids that have `needs: "apps"` steps) and each
step's optional `needs`; the header shows `(needs --apps)`:

```
Sync plan for Claude Desktop 1.0.3 → your config ~/Library/Application Support/Claude (needs --apps)
```

Exit codes are unchanged: `3` while non-optional manual steps remain. So `hlock sync` of
an app lock without `--apps` exits `3`, with `hlock sync --apps` as the first `next →`
hint. `hlock check`'s hint to restore drift says `hlock sync --apps` for an app lock, and
`hlock sync --prefix P` with `--prefix`, as today.

### 3.5 Pin: apps are user-scope

`hlock pin` captures only the project scope unless `--user` or `-i GLOB` is given. Almost
everything an app reads is user-level (`{app-config}/…`), so for an app the default is
the whole user scope, as `--user`. Core resolves the default once the harness is chosen
(`capture::pin`); an explicit `--user` or `-i` is used as given. The lock records the
scope as today, so the re-pin hint is `hlock pin --harness claude-desktop --user`.

When the app isn't installed, `pin` warns, as for a CLI ("Claude Desktop isn't installed
here; pinning its config only"), and `[harness]` has no version or source. `sync` then
offers the profile's first install method that works here.

For a profile with no `model` sources (both phase 1 apps), `pin` records the `default`
role as `harness-default`, as it does for any harness whose model it can't read.

### 3.6 Apps that share a CLI's config, and `[target] also`

Claude Desktop's Code tab reads `~/.claude`, which `claude-code` covers. So
`claude-desktop` pins only `claude_desktop_config.json` `/mcpServers`. `/preferences` and
the other keys in that file are state the app writes (sidebar mode, scheduled tasks,
extension allow-lists), so they're never pinned.

The common case, "my Claude Code setup, and the same MCP servers in Claude Desktop", is
`[target] also`, which installs a lock's declared skills, MCP servers and instruction
files into more harnesses:

```toml
[target]
harness = "claude-code"
also = ["claude-desktop"]
```

Two changes in sync's also loop make it work for apps:

1. **Real paths.** An also-app is planned with `target_ctx(op, ctx, &SyncOptions {
   prefix: None, docker: None, .. })` and the gating in §3.4, not under the main
   harness's prefix.
2. **Scope fallback.** Declared dependencies default to `scope = "project"`, and Claude
   Desktop has no project MCP config, skill directory or instruction file. In the also
   loop only, an app's destination falls back to the user scope (`mcp_dest(op, "user",
   …)`, and the same for skills and instructions) when it has no project one, and the
   step title says so ("Claude Desktop: MCP server github (user config)"). When the app
   has no destination for a kind at all, that's one note, not one per dependency.
   `deps::mcp_dest` and friends are unchanged, so `hlock add` keeps its behavior:
   `hlock add mcp X --harness claude-desktop` asks for `--scope user`, and with it writes
   the file directly, as for a CLI, except that it refuses while a `quit-to-write` app is
   running.

VS Code's `chat.mcp.discovery.enabled` imports Claude Desktop's servers. With `also`
putting the same server in both, VS Code may list it twice; the docs say so.

### 3.7 Install, version, and the meta harness

**Install methods.** `brew-cask` and `app` already exist. `install_step` already falls
back to the first method whose tool is on `PATH`; for an app it skips `brew-cask`, as the
recorded source and as a fallback, when `Ctx::os()` isn't `macos` (Linux has `brew` but no
casks). `app` is a download: never
automatic; its `command` is the hint. (`winget` is phase 2, with the Windows paths.)

**Version.** Desktop apps update themselves, and neither casks nor download pages install
an old version. So for an app:

- a missing app is installed at the current version, and the step says so ("installs the
  current version; the lock pins 1.0.3");
- an installed app at another version is **satisfied**: "installed 1.0.4; the lock pins
  1.0.3, and Claude Desktop updates itself";
- `hlock check` doesn't report the version as drift: `check_pin_at` copies the lock's
  `[harness] version` and `source` into the re-capture before diffing, when the installed
  app's install method is the same. A changed method (`app` → `brew-cask`) is still
  reported. `hlock harness show` always prints the installed version.

**The meta harness.** A missing app with no package manager is a job for
[`hlock meta run install`](meta-harness.md): the download differs per OS and
architecture, and the steps (fetch the disk image, copy the app into `~/Applications`)
are routine for an agent but not something hlock should script. Three changes:

1. The install job's prompt, for an app harness step, adds: the app's name, the download
   hint, the bundle or path hlock looks for, and the check, `hlock harness show <id>`. It
   says to install into `~/Applications` rather than `/Applications` when that avoids
   `sudo`, and to ask before anything that needs admin rights. It doesn't ask the agent
   to open the app: hlock creates the config directory itself when it writes the file.
2. `hlock meta run install` and `hlock meta verify install` take `--apps`, passed to the
   plan they render and check. Without it, steps that need `--apps` are the user's, like
   secrets: a third bucket in `verify`, giving `needs-you` (exit `3`) and naming
   `hlock sync --apps`. With it, the agent finishes with `hlock sync --apps --yes`.
   (The flag isn't `--in-place`: [meta-harness.md](meta-harness.md) uses that wording for
   the meta harness's own config, which never runs in place.)
3. When an app install or `--apps` steps are what's left, `sync`'s `next →` hints start
   with `hlock sync --apps` (when steps need it), then, for the project's own lock with a
   `harness.meta.lock`, `hlock meta run install` (with `--apps` when steps need it), else
   `hlock sync --skill`. `meta run install` works on the project's lock only, so a pulled
   lock on a fresh machine goes through `--skill`: the generated rehydrate skill carries
   the same app guidance as (1) for the harness step, and tells the agent to finish with
   `hlock sync --apps` when steps need it.
4. With `--apps`, the install prompt says writing the app's own config was asked for, in
   place of "don't change the user's own config unless they ask".

The meta harness itself can't be an app (§3.1).

### 3.8 What isn't restored

A profile lists setup it knows lives outside any file hlock restores:

```toml
[[unmanaged]]
what = "Connectors (remote MCP servers)"
where = "your claude.ai account"
url = "https://claude.ai/settings/connectors"
[[unmanaged]]
what = "Desktop Extensions"
where = "the app (Settings → Extensions)"
```

`hlock harness show` lists them. `hlock sync` adds one plan note, "not restored: 3 more
live outside files hlock restores (`hlock harness show claude-desktop`)", and `-v` lists
them. They're notes, never steps, so they don't change the exit code, and the lock records
nothing for them: hlock can't read them, and a lock entry would claim knowledge it doesn't
have. When the plan itself loses something to the account (a remote server Claude Desktop
can't hold, §3.9), that step names the link.

### 3.9 MCP shapes and transports

Two shapes join `standard`, `opencode`, `codex`, `goose`, `openclaw` and `nanobot` in
`launch::mcp_entry`, which `sync`, `[target] also`, `add mcp` and `migrate` share:

| Shape | Local server | Remote server |
|---|---|---|
| `vscode` | `{"type": "stdio", "command", "args", "env"}` | `{"type": "http" \| "sse", "url", "headers"}` |
| `zed` | `{"command", "args", "env"}` | `{"url", "headers"}` |

Phase 1 ships no Zed profile (§7), but the shape is there for a user profile. Reading is
already shape-agnostic (`capture::normalize_mcp`); Zed's older form,
`"command": {"path", "args", "env"}` (with or without `"source": "custom"`), is added.

Claude Desktop uses `standard`. A profile says which transports its MCP config can hold
with `mcp.transports` (default: all); `claude-desktop` sets `["stdio"]`. A server whose
transport isn't listed gets a manual step in `sync` and `[target] also` ("Claude Desktop
adds remote servers as connectors: https://claude.ai/settings/connectors"), is refused by
`add mcp`, and gets a note in `migrate`, instead of an entry the app ignores.

`env-ref` isn't set for VS Code: its MCP docs don't confirm `${env:NAME}` in `mcp.json`,
and `env-ref` only matters to `launch`.

VS Code's `mcp.json` is JSON with comments. hlock writes JSONC back as plain JSON, so a
write drops its comments. That's existing behavior for Cursor Agent and OpenCode; the docs
say so.

### 3.10 Migrate

`migrate --to` accepts an app, and a lock pinned from an app can be migrated to a CLI or
another app (`every_pair`'s test includes apps). The translation is the existing one,
driven by the target profile:

- **MCP servers** are rewritten into the target's shape; a transport it can't hold is left
  out of its config with a note, and stays in `[[tool]]`.
- **Skills, instructions, rules** move by the target's directories. VS Code's rules
  directory is `.github/instructions` with `.instructions.md`.
- **Models.** For a target with no `model` sources, the note is "Claude Desktop sets its
  model in the app; the lock's models aren't configured", replacing the generic notes.
  `[[llm]]` is kept.
- **Deciders** are dropped with a note: no app profile has `[deciders]`.
- **Evidence** is cleared, as for any migration.

`migrate --local` to an app is refused, as `localize` is.

### 3.11 Security

- App detection reads files (`Info.plist`, `Caskroom` directory names) and runs only
  `/usr/bin/plutil` and `/usr/bin/pgrep`, by absolute path, with fixed arguments; the
  `process` name passed to `pgrep` must be a plain token.
- A project profile (`./.hlock/profiles/`) may declare `[app]`, within limits like those
  on its `binaries`: `bundles` must be plain names ending in `.app` (no `/` or `\`),
  `process` a plain token, and `paths` is ignored.
- Writes use the existing actions and checks: instruction files only where the profile
  says the app reads them, skills only under its skill directories, executables (MCP
  servers) only when trusted.
- `--install` runs `brew install --cask` with a plain-token package, through
  `install_command`.

## 4. Built-in profiles (phase 1)

| id | Aliases | Config root | Files (restore) | MCP | Skills, instructions | Install |
|---|---|---|---|---|---|---|
| `claude-desktop` | `claude-app` | `{app-config}/Claude` | `claude_desktop_config.json` (pointers `/mcpServers`) | `/mcpServers`, `standard`, stdio only | none (the Code tab is `claude-code`) | `brew-cask:claude`, download |
| `vscode` | `code`, `vs-code`, `visual-studio-code` | `{app-config}/Code/User` | `{root}/mcp.json` (whole), `.vscode/mcp.json` (record) | `/servers`, `vscode` | skills `.github/skills`, `.agents/skills`, `~/.copilot/skills`, `~/.agents/skills`; instructions `.github/copilot-instructions.md`, `AGENTS.md`; rules `.github/instructions` (`.instructions.md`) | `brew-cask:visual-studio-code`, download |

`claude-desktop`: `quit-to-write`, `reload = "restart"`, no shim. `vscode`: `reload =
"auto"`, shim `code`. Both `support = "basic"`.

## 5. Implementation

### Rust core

- `profiles.rs`: `AppSpec { bundles, paths, process, quit_to_write, reload }`;
  `McpSpec.transports`; `Unmanaged { what, where, url }`; `Ctx::os()`,
  `Ctx::app_config_dir()`, `{app-config}` in `Ctx::expand`; `probe_app` (bundle → paths →
  shim; plist reader; Caskroom check) and `app_running`; `confine` for `[app]`; two
  profile files in `BUILTIN_HARNESSES`.
- `launch.rs`: `vscode`, `zed` in `mcp_entry`; refuse apps at the top of `prepare`.
- `capture.rs`: the not-installed warning includes apps; Zed's object `command`;
  `is_project_path` `pub(crate)`; the user-scope default for apps; the app version kept in
  `check_pin_at`.
- `sync.rs`: `apps` and `default_prefix` in `SyncOptions`; `isolated()` skips apps;
  `target_ctx` drops `XDG_CONFIG_HOME`/`APPDATA` when relocating home; the app harness step
  (§3.7) with `verify = "hlock harness show <id>"`; the `--apps` gating and the
  running-app check; also-apps on real paths, with the scope fallback, optional gated
  steps, and skipped under docker or an explicit prefix; transports in `mcp_dep_step`;
  `brew-cask` skipped off macOS; `--docker` refused; `Plan.apps`, `Step.needs`;
  `unmanaged` notes.
- `deps.rs`: `install` refuses a transport the profile can't hold, and refuses while a
  `quit-to-write` app runs.
- `migrate.rs`: accept `cli | app` targets; transport and model notes.
- `resolver.rs` (`apply_target`), `meta.rs` (`ineligible`), `localize.rs`, `launch.rs`:
  refuse apps with the §3.1 message. `meta.rs`: `apps` in `JobInput`, the app guidance
  in `install_goal`, the third bucket in `verify`.

### Python and bindings

- `harness-lock-py`: `apps` and `default_prefix` on `sync_plan`; `apps` on the meta job
  calls.
- `cli.py`: `--apps` on `sync`, `meta run`, `meta verify`; the interactive confirm; header,
  hints, success and reload messages for app plans; `check`'s restore hint; explicit `app`
  branches in `run`, `init --harness`, `evals run`, `harness list` (a **Desktop apps**
  group) and `harness show` (unmanaged items).
- `scripts/harness_table.py`: a desktop-apps table in `docs/reference/harnesses.md`.
- `skills/hlock/SKILL.md`: a row for desktop apps (`pin --harness`, `sync --apps`), then
  the skill tests and installed copies, per AGENTS.md.

No lockfile schema change and no worker contract change: `[harness]` already has
`version` and `source`, and `Plan`/`Step` gain optional fields only.

### Docs

`docs/guides/harnesses.md` (a "Desktop apps" section), `harness-profiles.md` (`kind =
"app"`, `[app]`, `{app-config}`, `mcp.transports`, `[[unmanaged]]`, the `zed` and
`vscode` shapes), `sync.md` (`--apps`, running apps, reload), `pin.md` (the app default),
`meta.md` (`--apps`), `migrate.md`, `dependencies.md` (`also` with apps),
`reference/cli.md`, `reference/environment.md` (`HLOCK_OS`, `HLOCK_APP_DIRS`,
`HLOCK_APPS_RUNNING`), and the generated harness tables. Specs: `migrate.md`'s "agent
CLI" target rule now includes apps.

## 6. Tests

- Profiles: both parse; `{app-config}` per `HLOCK_OS`; a relocated home drops
  `XDG_CONFIG_HOME`/`APPDATA` and keeps an XDG profile's own variable; project-profile
  limits on `[app]`.
- Probe: a bundle under `HLOCK_APP_DIRS` with an XML `Info.plist`; a binary plist through
  `plutil` (macOS-only test); a Caskroom directory gives `brew-cask`; a shim on `PATH`
  supplies the version; an app's own executable is never run.
- `mcp_entry` for `vscode` and `zed`, both transports; Zed's object `command` parses;
  round-trip (`mcp_entry` → `normalize_mcp`) for every shape.
- Sync: an app lock plans in `home` mode, also with a default (pulled) prefix; without
  `--apps`, user-config steps are manual with `needs: "apps"` and project steps
  automatic; with `--apps` or `--in-place` they're automatic; `--prefix` gates nothing;
  `HLOCK_APPS_RUNNING` makes them manual even with `--apps`; `--docker` is refused; an
  installed app at another version is satisfied; a remote server for `claude-desktop` is
  manual; unmanaged notes don't change the exit code; `also = ["claude-desktop"]` from a
  prefixed Claude Code plan writes Claude Desktop's real `/mcpServers` only with `--apps`,
  through the user-scope fallback, and its gated steps are optional.
- Check: a newer app version isn't drift; a changed install method is.
- Pin: an app pins the user scope by default.
- Refusals: `launch`, `run`, `resolve`/`tune` with an app target, `init --harness`,
  `meta init --harness claude-desktop`, `migrate --local --to vscode`.
- Migrate: `claude-code` → `vscode` moves MCP servers to `servers` with `type`,
  `CLAUDE.md` to `AGENTS.md`, project skills to a directory VS Code reads; `claude-code` →
  `claude-desktop` leaves a remote server out of the config with a note.
- Meta: the install prompt for an app lock includes the download hint and no first-launch
  step; `verify install` without `--apps` is `needs-you` when only `--apps` steps remain.
- CLI: `hlock harness list` groups apps; `harness show claude-desktop` lists unmanaged
  items; `sync` on an app lock prints the `(needs --apps)` header and hint.

## 7. Phasing

1. **Phase 1 (this spec):** §3 and the two profiles in §4.
2. **Phase 2:**
   - **Zed**, once pointer writes into JSONC keep comments (its `settings.json` is
     usually heavily commented, and a write would drop every comment), with
     `instructions-first-match` (Zed reads only the first instruction file it finds).
   - **More apps** (done, §10): `cursor-ide`, `chatgpt`, `devin-desktop`, `kiro`,
     `antigravity`, `trae`. Still open: LM Studio's chat MCP and Bionic, JetBrains.
   - **Windows**: `winget`, the Claude Desktop MSIX path, running-app detection with
     `tasklist`, and versions from the registry (`DisplayVersion` under the app's
     `Uninstall` key).
   - Linux `.desktop`, Flatpak and Snap detection; `exact-version` for users who want app
     updates reported as drift; a key-prefix restore mode for app `settings.json` files;
     VS Code profiles.
3. **Later:** an org-wide inventory of MCP servers across apps and CLIs (a cloud feature,
   not in this repo).

## 8. As built (phase 1)

Phase 1 is §3 and §4 as written, with these details settled in the code:

- **Plan JSON.** `Plan.app` (the lock's harness is an app), `Plan.apps` (the *names* of the
  apps with `needs: "apps"` steps, which the CLI and the meta prompt print), and
  `Plan.reload` (app name → `restart`/`auto`, for the apps a plan will write). `Step.needs`
  is `"apps"` on gated steps. `Plan::commands()` is `hlock sync --apps` for an app lock in
  `home` mode.
- **`[target] also` apps.** Besides the `--apps`-gated steps, a server an app's config can't
  hold (a remote server for Claude Desktop) is optional too: the user adds it in the app or
  account, and it doesn't hold up the main harness.
- **Install wording.** A download-only app's step reads "a download, installed by hand;
  installs the current version; the lock pins …", and Homebrew's "may differ from the pin"
  note is dropped for apps (it's always the current version).
- **VS Code instructions.** `AGENTS.md` is listed before `.github/copilot-instructions.md`,
  so `migrate` renames `CLAUDE.md` to `AGENTS.md`, the file more harnesses share.
- **The `zed` shape** and Zed's older nested `command` form are in, for user profiles; the
  Zed profile is phase 2 (§7).
- **Tests.** Rust: `sync` (gating, `--apps`, running app, default and explicit prefixes,
  docker refusal, install step, also-apps, isolation of `{app-config}`), `profiles`
  (`{app-config}` per OS, bundle/shim/Caskroom probing, binary plists through `plutil` on
  macOS, project-profile limits), `migrate` (`every_pair` includes apps; Claude Code → VS
  Code and → Claude Desktop), `meta` (the install prompt and the `needs-you` bucket),
  `launch`/`resolver` refusals, `capture` (app version drift). Python: `tests/test_apps.py`
  (pin, sync with and without `--apps`, the interactive confirm, a running app, listing and
  refusals) and `tests/test_deps.py` (`also = ["claude-desktop"]`).

## 9. Open questions

1. **`--apps` versus a manifest setting.** Teams may want "always write Claude Desktop"
   without a flag. `[target] apps = true` in `harness.toml` could do it; left out until
   someone asks, since the flag is explicit about touching a user's own config.
2. **Claude Desktop rewriting its file.** The running-app check covers the reported bug
   for writes. If the app later drops `mcpServers` on its own, `check` reports it as drift;
   should the hint name the likely cause?

## 10. More apps (phase 2, profiles only)

Six more profiles, researched from vendor docs, changelogs and Homebrew casks in October
2026. None needed new sync rules; two needed MCP shapes.

| id | App | Config | MCP | Notes |
|---|---|---|---|---|
| `cursor-ide` | Cursor | `~/.cursor` | `mcp.json` `/mcpServers`, `.cursor/mcp.json`; `standard` | Shares files with `cursor-agent`; shim `cursor`; `reload = "restart"` (staff say only a full restart reloads every server) |
| `chatgpt` | ChatGPT (the Codex app until July 2026) | `~/.codex` (`CODEX_HOME`) | `config.toml` `/mcp_servers`; `codex`; stdio and HTTP | Shares files with `codex`; bundles `ChatGPT.app`, `Codex.app`; ChatGPT Classic has no file config |
| `devin-desktop` | Devin Desktop (Windsurf until June 2026) | `~/.config/devin` | `mcp_config.json` `/mcpServers`, `.devin/mcp_config.json`; new shape `devin` (`url` + `transport`) | Devin Local's files; Cascade's `~/.codeium/windsurf` is unmanaged; bundles `Devin.app`, `Windsurf.app` |
| `kiro` | Kiro | `~/.kiro` | `settings/mcp.json` `/mcpServers`, `.kiro/settings/mcp.json`; `standard` | Steering is the rules dir (`.kiro/steering`, `.md`); shim `kiro`; reloads live |
| `antigravity` | Antigravity app and IDE | `~/.gemini/config` | `mcp_config.json` `/mcpServers`, `.agents/mcp_config.json`; new shape `antigravity` (`serverUrl`); no SSE | One profile for both bundles, which read the same files |
| `trae` | TRAE | `{app-config}/Trae/User` | `mcp.json` `/mcpServers`, `.trae/mcp.json`; `standard` | The user MCP path is from MCP installers, not Trae's docs |

Not added: **Zed** (a write to its commented `settings.json` drops the comments, and Zed
reads only the first instruction file it finds), **LM Studio** (a runtime profile already;
its home moves with `~/.lmstudio-home-pointer`), **Goose Desktop** (shares
`~/.config/goose/config.yaml` with `goose`), and **JetBrains** AI Assistant (MCP set in the
IDE, not a documented file). Process names aren't documented for any of the six, so none
sets `quit-to-write`; Windows and Linux paths are listed only where a vendor documents them.
