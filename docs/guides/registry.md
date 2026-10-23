# Sharing through a registry

A registry is a server that keeps published agent setups, so others can find one with
`hlock search` and pull it by name, without knowing which repository it's in. hlock speaks an
open protocol, the [hlock registry HTTP API](https://github.com/unionai-oss/hlock/blob/main/specs/registry.md);
any server that implements it works, and hlock has no default registry. Pulling from a git
repository ([`hlock pull github:…`](pull.md)) needs no registry at all.

A published setup is whichever of these files your project has:

| File | |
|---|---|
| `harness.toml` | What the project declares |
| `harness.lock`, and the files it vendors under `.hlock/vendor/` | The pinned setup |
| `apm.yml` (or `apm.yaml`) | [APM](../getting-started/apm.md)'s manifest |
| `apm.lock.yaml` | APM's lock |
| `README.md` | Shown on the registry's page for it |

A project that uses only APM can publish its `apm.yml` and `apm.lock.yaml` the same way. (A
reusable APM *package*, with an `.apm/` directory, is published with `apm publish` to a
registry that speaks APM's protocol, not with hlock.)

## Log in

```console
$ hlock login https://registry.example.com
Your code: WXYZ-1234
Confirm it at https://registry.example.com/device?code=WXYZ-1234
✓ logged in to https://registry.example.com as alice  (token in the credentials file)
next → hlock push  ·  hlock search QUERY
```

`login` opens the registry's page in your browser and waits while you confirm the code. The
token goes in your OS keychain (macOS `security`, or Linux `secret-tool`), else in
`~/.hlock/credentials.toml`, readable only by you. The URL becomes your default registry, in
`~/.hlock/config.toml`. Pass `--no-browser` to print the link without opening it, or
`--token TOKEN` (`--token -` reads it from stdin) to use a token you made in the registry's
settings.

```console
$ hlock whoami
alice (Alice) on https://registry.example.com
  namespaces  alice
  scopes      read, publish
  token       from the credentials file
```

`hlock logout` forgets the token. In CI, set `HLOCK_REGISTRY` to the registry's URL and
`HLOCK_REGISTRY_TOKEN` to a token instead of logging in; the environment wins over what
`login` saved. Every command also takes `--registry URL`.

## Publish

Look at what would be published first:

```console
$ hlock push --dry-run
would push alice/review-bot@1.0.0 to https://registry.example.com (5 files, 404 B, sha256:24f99c95f3c0…)
file                                              size   digest
.hlock/vendor/claude-code/skills/review/SKILL.md  17 B   ec9934ffb1e9
apm.lock.yaml                                     39 B   13fe02d9ad1f
apm.yml                                           56 B   3dbfab129de8
harness.lock                                      220 B  d880772b7e3c
harness.toml                                      30 B   a49d0989b2a7
next → hlock push
$ hlock push
✓ pushed alice/review-bot@1.0.0 (5 files, 404 B, sha256:24f99c95f3c0…)
  https://registry.example.com/alice/review-bot
next → hlock pull alice/review-bot@1.0.0
```

- **The name** is `NAMESPACE/NAME`: by default your namespace (your handle) and the project's
  name (`[project] name` in `harness.toml`, else `name` in `apm.yml`, else the directory's).
  Pass another, such as an organization's, as an argument: `hlock push acme/review-bot`.
- **The version** is `--version`, else `apm.yml`'s `version`, else `sha-` and the first 12
  hex digits of the archive's digest. A version can't change once it's published.
- **The archive** is a `tar.gz` with sorted entries and no timestamps or owners, so the same
  files always give the same digest. Pushing a version that's already published with the
  same files says so and succeeds; with other files, it fails and asks for a new version.
- **Checks before anything is sent:** `push` refuses a value that looks like a credential
  rather than a `${HLOCK_SECRET_…}` placeholder (re-pin with [`hlock pin`](pin.md) to redact
  it), and [characters that hide text from a reviewer](audit.md#hidden-characters). Files a
  lock vendors from outside `.hlock/vendor/`, and symlinks, are left out with a warning.
- An archive is at most 4 MiB and 500 files.

## Find a setup

```console
$ hlock search review
package           source  files         harness  description
alice/review-bot  hosted  harness, apm  pi       a setup
next → hlock pull alice/review-bot
```

`--kind harness` or `--kind apm` keeps setups with hlock's or APM's files; `--source hosted`,
`github` or `gitlab` keeps those pushed to the registry, or those a registry found in a
repository; `--type apm-package` shows APM packages. `--json` prints the registry's answer.

## Pull by name

A `NAMESPACE/NAME` source pulls from your registry, the way a `github:` source pulls from
git. Everything in [Pulling someone else's lock](pull.md) applies: the lock goes in the store
under its name, nothing from it runs, and its vendored files are scanned first.

```console
$ hlock pull alice/review-bot
✓ pulled alice/review-bot @ 1.0.0 → review-bot
╭─ 🔒 review-bot ─────────────────╮
│                                 │
│  Stack                          │
│  harness   claude-code 2.1.285  │
│  decision                       │
│  backend                        │
│                                 │
│  Evidence                       │
│  success rate  —                │
│  cost / task   —                │
│  latency       p50 —  ·  p95 —  │
│  trials        0                │
│                                 │
│  lock-version 1 · manifest —    │
│                                 │
╰─────────────────────────────────╯
next → hlock launch --lock review-bot  ·  hlock sync review-bot --in-place --plan
```

`@VERSION` pulls one version and stays there; without one, `hlock pull` (or
`hlock pull review-bot`) later moves to the newest. The download is checked against the
version's digest before anything is unpacked, and an archive with anything but the files
listed above, a path outside it, or a link is refused. The lock records where it came from:

```toml
[pulled]
at = "2026-10-22T21:08:32Z"
digest = "sha256:24f99c95f3c0…"
from = "registry:https://registry.example.com/alice/review-bot"
path = "harness.lock"
url = "https://registry.example.com/alice/review-bot"
version = "1.0.0"
```

A registry can also list setups that live in GitHub or GitLab repositories. Pulling one of
those fetches it from its repository with git, at the commit the registry gives.

## Into a project

`--into DIR` writes the whole setup into a directory, rather than the lock alone into the
store: `harness.toml`, `harness.lock` and the files it vendors, `apm.yml` and
`apm.lock.yaml`. It's how you start a project from someone's setup:

```console
$ hlock pull alice/review-bot --into .
✓ pulled alice/review-bot @ 1.0.0 → .  (5 files)
  apm.lock.yaml
  apm.yml
  harness.lock
  harness.toml
  .hlock/vendor/ 1 vendored file
next → hlock sync --plan  ·  apm install --frozen
```

It refuses to replace a file that's already there unless you pass `--force`. `--into` works
with git sources too (`hlock pull github:alice/agent-setup --into .`).

## Reference

```bash
hlock login [URL] [--token TOKEN | --no-browser]
hlock logout [--registry URL]
hlock whoami [--registry URL] [--json]
hlock push [NAMESPACE/NAME] [--version V] [-C DIR] [--dry-run] [--registry URL] [--json]
hlock search [QUERY...] [--type T] [--kind K] [--source S] [--limit N] [--registry URL] [--json]
hlock pull NAMESPACE/NAME[@VERSION] [--as NAME | -o FILE | --into DIR] [--force] [--registry URL] [--json]
```

See the [CLI reference](../reference/cli.md#push) for every option.
