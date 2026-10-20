# `hlock-policy.toml`

A project's policy: what its locks may use. hlock reads `hlock-policy.toml` next to the lock
(else in the project directory), plus whatever it names with `extends`. See
[Audit and trust](../guides/audit.md#policy) for how it's enforced.

| Key | Description |
|---|---|
| `extends` | A parent policy, pinned by hash: a path (relative to this file) or a git file (`github:OWNER/REPO/PATH`, at the default branch's head), followed by `#sha256:HEX` of its bytes. Up to 5 levels. A git parent is fetched once and kept in `~/.hlock/cache/policy/` by its hash; it can extend another git policy, not a path |
| `enforcement` | `block` (the default: violations stop `pin`, `add`, `pull`, `sync` and `launch`), `warn` (they're printed), or `off`. The strictest level wins, so `warn` can't relax a parent that blocks |

## `[sources]`

| Key | Description |
|---|---|
| `allow` | Patterns a dependency's source must match. Unset: anything not denied |
| `deny` | Patterns no source may match |
| `trusted` | Patterns whose executables need no `hlock trust` approval, once you've approved this list on your machine (`hlock trust` lists it as kind `policy`; a changed list asks again) |

Sources are matched without their ref or range, as `github:OWNER/REPO[/PATH]`,
`git:HOST/PATH`, `npm:PACKAGE`, `pypi:PACKAGE`, `plugin:MARKETPLACE[/NAME]`, `path:…`, or a
remote server's URL. A pattern also matches everything inside what it names, so
`github:acme/*` matches `github:acme/skills/skills/sql`. The policy checks declared
dependencies, pinned skills and instruction files with a source, MCP servers, plugins and
APM packages.

An MCP server is matched by what it runs, not only by what its entry says: a remote server by
its URL, and a stdio server by the package its command runs, when that's the source its entry
names (`npx -y PKG` for `npm:PKG`, `uvx PKG` for `pypi:PKG`). A stdio server that runs anything
else (`node ./server.js`, `sh -c …`) has no source `allow` can check, so it breaks an `allow`
list; a `deny` matches either. `trusted` follows the same rule, and covers a skill's scripts
only when the skill is fetched from its source rather than copied from a vendored copy.

## `[mcp]`

| Key | Description |
|---|---|
| `transports` | The transports MCP servers may use: `stdio`, `http`, `sse`. A server with no `transport` is `http` when it has a URL and `stdio` when it has a command |
| `deny` | Patterns no MCP server's source may match |

## `[models]`

| Key | Description |
|---|---|
| `providers` | The providers models and decision models may come from (`anthropic`, `ollama`, …) |
| `deny` | `provider:model` patterns (`*:*-preview`) |

## `[harness]`

| Key | Description |
|---|---|
| `allow` | The harnesses a lock may be for |
| `min-version` | The oldest version of each harness allowed: `{ claude-code = "2.1" }` |

## `[evidence]`

For resolved locks (those with evidence); a pinned lock has none to check.

| Key | Description |
|---|---|
| `min-success-rate` | The lowest mean success rate allowed |
| `max-cost-usd-per-task` | The highest mean cost per task allowed |
| `max-age-days` | How old the lock's measurement may be, by its `generated-at` |

## Patterns

Patterns are anchored at both ends. `*` matches any run of characters except `/`, and `**`
any run including `/`. There are no character classes or braces. This is APM's pattern
grammar.

## Inheritance

Every level's rules apply: a source must satisfy each level's `allow` and no level's `deny`,
a model each level's `providers`, and so on. So a child policy can deny more and require more,
but can't allow what its parent denies. A source is trusted only when every level that lists
`trusted` sources lists one it matches, and none denies it. An unknown key is an error, so a typo can't leave a
rule unenforced.
