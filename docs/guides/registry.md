# Sharing through a registry

A registry is a server that keeps published agent setups, so others can find one with
`hlock search` and pull it by name, without knowing which repository it's in. hlock speaks an
open protocol, the [hlock registry HTTP API](https://github.com/unionai-oss/hlock/blob/main/specs/registry.md);
any server that implements it works, and hlock has no default registry. Pulling from a git
repository ([`hlock pull github:…`](pull.md)) needs no registry at all.

A published package is a project: whichever of these files it has, versioned together.

| File | |
|---|---|
| `harness.toml` | What the project declares |
| `harness.lock`, and the files it vendors under `.hlock/vendor/` | The pinned setup |
| `harness.toml`, `harness.lock` and `harness.NAME.lock` anywhere else under `.hlock/`, with the files each lock vendors (under its own directory's `.hlock/vendor/`) | More setups, such as the per-harness locks in `.hlock/locks/` |
| `apm.yml` (or `apm.yaml`) | [APM](../getting-started/apm.md)'s manifest |
| `apm.lock.yaml` | APM's lock |
| `README.md` | Shown on the registry's page for it |
| The [eval packs](eval-packs.md) `harness.toml`'s `[evals] packs` names | What the locks' evidence was measured on, so whoever pulls it can rerun it |

The directories hlock keeps its own state in under `.hlock/` (`env/`, `store/`, `tune/`,
`runs/`, `bak/` and the like) aren't searched. The eval packs travel in a second archive of
the same version, so a registry or an hlock from before eval packs still takes and pulls the
rest.

A project that uses only APM can publish its `apm.yml` and `apm.lock.yaml` the same way. A
reusable APM *package*, with an `.apm/` directory, is published with
[`hlock apm push`](#apm-packages) instead.

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
would push alice/review-bot@1.0.0 to https://registry.example.com (11 files, 853 B, sha256:25c9c3830399…)  (name from the git remote)
file                                                  size   digest
.hlock/locks/.hlock/vendor/pi/skills/review/SKILL.md  17 B   ec9934ffb1e9
.hlock/locks/harness.pi.lock                          169 B  7d7e6b1c9c1c
.hlock/vendor/claude-code/skills/review/SKILL.md      17 B   ec9934ffb1e9
apm.lock.yaml                                         39 B   13fe02d9ad1f
apm.yml                                               56 B   3dbfab129de8
harness.lock                                          207 B  2c5584e8a5cf
harness.toml                                          53 B   9079dd5e1bed
evals/ eval pack, 4 files                             290 B
next → hlock push
$ hlock push
✓ pushed alice/review-bot@1.0.0 (11 files, 853 B, sha256:25c9c3830399…)  (name from the git remote)
  https://registry.example.com/alice/review-bot
  with eval packs: evals/
next → hlock pull alice/review-bot@1.0.0
```

- **The name** is `NAMESPACE/NAME`: by default your namespace (your handle) and the name of
  the repository the project is in, from its git remote (`origin`, else the only one). That
  holds when the project is the repository's root; a project in a subdirectory of a
  repository, or one that isn't in one, is named by `[project] name` in `harness.toml`, else
  `name` in `apm.yml`, else the directory's. `push` says where the name came from. Pass
  another, such as an organization's, as an argument: `hlock push acme/review-bot`. When the
  repository's name isn't published yet but one of the names `push` used to default to is
  (`[project] name`, unless it's `hlock init`'s `demo`, `apm.yml`'s, the directory's),
  `push` stops rather than start a second package, and asks you to pass the one you want.
- **The version** is `--version`, else `apm.yml`'s `version`, else `sha-` and the first 12
  hex digits of a digest of everything pushed, eval packs included. A version can't change
  once it's published, eval packs included.
- **The archive** is a `tar.gz` with sorted entries and no timestamps or owners, so the same
  files always give the same digest. Pushing a version that's already published with the
  same files says so and succeeds; with other files, it fails and asks for a new version.
- **Checks before anything is sent:** `push` refuses a value that looks like a credential
  rather than a `${HLOCK_SECRET_…}` placeholder (re-pin with [`hlock pin`](pin.md) to redact
  it), and [characters that hide text from a reviewer](audit.md#hidden-characters). Files a
  lock vendors from outside `.hlock/vendor/`, and symlinks, are left out with a warning.
- An archive is at most 4 MiB and 500 files.

### Eval packs

The eval packs in `harness.toml`'s `[evals] packs` are pushed with the setup, so someone who
pulls a resolved setup can check and rerun the evidence its lock records. A pack is pushed
as exactly the files hlock digests it by, with their executable bits, so its digest after a
pull matches the one in the lock's `[evidence]`. `push` warns when a lock's evidence was
measured on other packs than the ones you're pushing.

Because packs are written into the projects of people who pull them, `push` is strict about
what's in them, and stops (listing every problem) rather than leave a file out:

- a pack's path must be a plain relative directory in the project, like `evals` or
  `evals/review`, reached through no symlink: no leading dots, `..`, or characters outside
  letters, digits and `._-`. A remote pack (`github:…`) is left out with a note;
- file names must be safe on every OS: letters, digits and `._+@=,-` only, no Windows device
  names like `CON`, no two that differ only in case, and no `.hlock` directory;
- no credentials: no file named like one (`.env`, `.netrc`, `.npmrc`, `credentials`,
  `*.pem`, `*.key`, `*.p12`, `id_rsa`, …), no private key block, no credential-shaped value in
  TOML, JSON or YAML, and in a text file no token with a known prefix (`sk-`, `ghp_`, `AKIA`,
  `npm_`, …) and no `password = …`-style line with a literal value that isn't a
  `${…}` placeholder;
- no critical hidden characters, and no file over 20 MiB.

`push --no-evals` publishes without them. The eval packs are at most 32 MiB and 5,000 files.
A registry that doesn't take eval packs yet gets the rest, with a warning.

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
also in the package: .hlock/locks/harness.pi.lock  → hlock pull alice/review-bot//.hlock/locks/harness.pi.lock
next → hlock launch --lock review-bot  ·  hlock sync review-bot --in-place --plan
```

`@VERSION` pulls one version and stays there; without one, `hlock pull` (or
`hlock pull review-bot`) later moves to the newest. The download is checked against the
version's digest before anything is unpacked, and an archive with anything but the files
listed above, a path outside it, or a link is refused.

### One lock of a package

A package's root `harness.lock` is the one `NAMESPACE/NAME` pulls. Name another by its path
after `//`, the way a git source takes one, with the version last
(`NAMESPACE/NAME//PATH@VERSION`):

```console
$ hlock pull alice/review-bot//.hlock/locks/harness.pi.lock
✓ pulled alice/review-bot//.hlock/locks/harness.pi.lock @ 1.0.0 → review-bot-pi
```

A directory works too (`alice/review-bot//.hlock/locks` takes its `harness.lock`, else its
only lock). A package with no root `harness.lock` gives its only lock, or lists them to pick
from. The lock is stored as `NAME-X` for `harness.X.lock`, and records where it came from, so
`hlock pull review-bot-pi` refreshes that same lock:

```toml
[pulled]
at = "2026-10-25T03:47:36Z"
digest = "sha256:25c9c383039976144ec111366e21310095b42e9e0413b9254d74f18c63913952"
from = "registry:https://registry.example.com/alice/review-bot//.hlock/locks/harness.pi.lock"
path = ".hlock/locks/harness.pi.lock"
url = "https://registry.example.com/alice/review-bot"
version = "1.0.0"
```

A stored lock from another package, or another lock of the same package, isn't replaced
without `--force`; `--as NAME` keeps both.

A registry can also list setups that live in GitHub or GitLab repositories. Pulling one of
those fetches it from its repository with git, at the commit the registry gives.

## Into a project

`--into DIR` writes the whole package into a directory, rather than one lock into the store:
`harness.toml`, `harness.lock` and the files it vendors, the harness files under `.hlock/`
with the files they vendor, `apm.yml` and `apm.lock.yaml`, and the eval packs. It's how you
start a project from someone's setup:

```console
$ hlock pull alice/review-bot --into .
✓ pulled alice/review-bot @ 1.0.0 → .  (11 files)
  .hlock/locks/harness.pi.lock
  apm.lock.yaml
  apm.yml
  harness.lock
  harness.toml
  .hlock/vendor/ 2 vendored files
  evals/ eval pack, 4 files
! eval packs run commands when graded, and their fixtures are the agent's workspace: read them before hlock tune
next → hlock sync --plan  ·  apm install --frozen
```

It refuses to replace a file that's already there unless you pass `--force`. An eval pack
directory that's already there counts too, and `--force` replaces it whole, so no task from
another version is left in it. The eval packs are checked against their digest, and scanned
like everything else; nothing in them runs
until you evaluate with them. `--no-evals` leaves them out, and pulling a lock into the store
never downloads them. `--into` works with git sources too
(`hlock pull github:alice/agent-setup --into .`).

## APM packages

An [APM](../getting-started/apm.md) package is a directory with an `apm.yml` (with `name` and
`version`) and an `.apm/` directory of skills, prompts and instructions. `hlock apm push`
publishes one to your registry, the way `apm publish` does, using the token `hlock login`
saved:

```console
$ hlock apm push --dry-run
would push APM package alice/review-skills@1.0.0 to https://registry.example.com (4 files, 640 B, sha256:3f8140bf1a98…)
file                              size  digest
.apm/prompts/summarize.prompt.md  22 B  f6b68658adb4
.apm/skills/review/SKILL.md       65 B  00890eadd9f3
README.md                         16 B  fc63a00ec400
apm.yml                           67 B  a9f8e5ab051f
next → hlock apm push
$ hlock apm push
✓ pushed APM package alice/review-skills@1.0.0 (4 files, 640 B, sha256:3f8140bf1a98…)
  install with APM: alice/review-skills#1.0.0 from the registry https://registry.example.com/api
next → hlock apm pull alice/review-skills@1.0.0
```

- **The archive** is the one `apm publish` builds: a zip of `apm.yml`, everything under
  `.apm/`, and `README.md`, `CHANGELOG.md` and `LICENSE` when they're there. Symlinks and
  macOS `.DS_Store` and `._*` files are left out. Entries are sorted and have no timestamps,
  so the same files give the same digest.
- **The version** is `apm.yml`'s `version`. The name defaults to your namespace and
  `apm.yml`'s `name`; pass `NAMESPACE/NAME` for another.
- **The same checks as `push`** run first: no credential-looking values, no hidden characters.
  An archive is at most 50 MiB.

`hlock apm pull` downloads a package to read it or vendor it. It takes the newest version
unless you name one with `@VERSION`, and writes the package into `./NAME`, or into the
directory you pass with `--into DIR`:

```console
$ hlock apm pull alice/review-skills
✓ pulled APM package alice/review-skills @ 1.0.0 → review-skills  (4 files)
  README.md
  apm.yml
  .apm/ 2 files
  install with APM: alice/review-skills#1.0.0 from the registry https://registry.example.com/api
```

The download is checked against the version's digest, and refused if it has a path outside
it, a link, or [hidden characters](audit.md#hidden-characters) (`--allow-hidden FILE` lets
those through), before anything is written. A file that's already there is replaced only with
`--force`. Nothing in the package runs.

To install a package with APM, add the registry to `apm.yml` with the URL followed by `/api`,
where it serves APM's [Registry HTTP API](https://microsoft.github.io/apm/reference/registry-http-api/):

```yaml
registries:
  team:
    url: https://registry.example.com/api
  default: team
dependencies:
  apm:
    - alice/review-skills#1.0.0
```

## Reference

```bash
hlock login [URL] [--token TOKEN | --no-browser]
hlock logout [--registry URL]
hlock whoami [--registry URL] [--json]
hlock push [NAMESPACE/NAME] [--version V] [-C DIR] [--no-evals] [--dry-run] [--registry URL] [--json]
hlock search [QUERY...] [--type T] [--kind K] [--source S] [--limit N] [--registry URL] [--json]
hlock pull NAMESPACE/NAME[//PATH][@VERSION] [--as NAME | -o FILE | --into DIR [--no-evals]] [--force] [--registry URL] [--json]
hlock apm push [NAMESPACE/NAME] [-C DIR] [--dry-run] [--registry URL] [--json]
hlock apm pull NAMESPACE/NAME[@VERSION] [--into DIR] [--force] [--allow-hidden FILE] [--registry URL] [--json]
```

See the [CLI reference](../reference/cli.md#push) for every option.
