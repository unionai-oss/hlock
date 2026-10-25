# The hlock registry HTTP API v1

Status: implemented (client) · Last updated: 2026-10-24 · Builds on [pull](pull.md)

> A registry keeps published agent setups and finds them by name. This is the protocol
> between `hlock` and a registry. It's open: any server that implements it works with
> `hlock login`, `push`, `search`, `pull NAMESPACE/NAME` and `apm push`/`pull`, and hlock has
> no default one.

---

## 1. Summary

```bash
hlock login https://registry.example.com      # device login (RFC 8628); token in the keychain
hlock push                                    # the repository you're in, from its root: → <handle>/<repo>@<version>
hlock push --root agents/review               # one project of a monorepo
hlock search "code review" --kind apm
hlock pull alice/hlock[@1.0.0]                # its harness.lock, into ~/.hlock/pulls/hlock/
hlock pull alice/hlock//.hlock/locks/harness.pi.lock   # another of its locks
hlock pull alice/hlock --root .               # the whole package, eval packs too, rooted at . (--into is an alias)
hlock apm push                                # an APM package (apm.yml + .apm/), as `apm publish` does
hlock apm pull alice/review-skills[@1.0.0]    # into ./review-skills/
```

A registry package is **a project**: everything hlock knows about in one repository,
versioned together. A version has a **setup archive** (§5.1): whichever of `harness.toml`,
`harness.lock` (with the files it vendors under `.hlock/vendor/`), the harness files elsewhere
under `.hlock/` (such as `.hlock/locks/harness.pi.lock`, with the files each vendors under its
own directory's `.hlock/vendor/`), `apm.yml`, `apm.lock.yaml` and `README.md` the project has.
It may also have an **evals archive** (§5.2): the eval packs the root `harness.toml` names,
which the setup archive commits to by digest. The locks in a package are addressed by path
(§6, Pull).

A registry may also list APM packages (`type = "apm-package"`, published with `apm publish`
over APM's own [Registry HTTP API](https://microsoft.github.io/apm/reference/registry-http-api/)),
and setups that live in git repositories (`source = "github"` or `"gitlab"`), which hlock pulls
with git.

The API is shaped like APM's registry API (`versions`, `versions/{v}/download`, `PUT
versions/{v}`, RFC 7807 errors, Bearer tokens), so one server can serve both.

## 2. Goals and non-goals

Goals:

- Publish and pull a setup by name, with immutable, content-addressed versions: a version's
  files, eval packs included, are fixed when it's first published.
- Nothing hlock-specific about the server: a small, documented protocol a third party can
  implement, with no default registry baked into hlock.
- A pulled setup is checked exactly like a git pull: digest, unsafe archive entries, hidden
  characters, project policy, and nothing runs until `sync` or `launch`.
- A client that predates eval packs can still pull every version.
- Credentials never in a project file: the OS keychain, an owner-only file, or the
  environment for CI.

Non-goals (for now): private packages, tags other than `latest`, yanking from the CLI,
signatures, a short lock selector (`alice/hlock:pi`), and a registry server
(implementations live elsewhere).

## 3. Conventions

- **Base URL**: the registry URL, e.g. `https://registry.example.com` (it may have a path,
  without `//` in it). Every endpoint is under `{base}/api/v1`. Plain `http` is accepted only
  for `localhost`, `127.0.0.1` and `[::1]`.
- **JSON** is `snake_case`. **Errors** are RFC 7807 `application/problem+json` with `title`,
  `status`, and optionally `detail` and `extensions.errors` (a list of strings).
- **Auth**: `Authorization: Bearer <token>`. Reads of public packages may be anonymous.
- **Identity**: `namespace` is `[a-z0-9-]{1,39}` (not starting or ending with `-`); `name` is
  `[a-z0-9._-]{1,100}`, starting with a letter or digit; `version` is `[A-Za-z0-9._+-]{1,64}`,
  starting with a letter or digit. `latest` names the newest version that isn't yanked and
  doesn't wait on its evals archive.

## 4. Endpoints

| Method + path | Request | Response |
|---|---|---|
| `POST /auth/device` | `{client_name}` | `200 {device_code, user_code, verification_uri, verification_uri_complete?, expires_in, interval}` |
| `POST /auth/device/token` | `{device_code}` | `200 {access_token, token_type, scopes, user}`, or `400 {error}` with `error` one of `authorization_pending`, `slow_down`, `expired_token`, `access_denied` (RFC 8628 §3.5) |
| `GET /whoami` | | `200 {user: {handle, name}, namespaces: [string], scopes: [string]}`; `401` without a valid token |
| `GET /search?q=&type=&kind=&source=&harness=&page=&per_page=` | | `200 {total, page, per_page, results: [Result]}` |
| `GET /packages/{ns}/{name}` | | `200 Package`; `404` if there's no such package |
| `GET /packages/{ns}/{name}/versions` | | `200 {package, versions: [Version]}` (APM's version list) |
| `GET /packages/{ns}/{name}/versions/{version}` | | `200 Version` |
| `GET /packages/{ns}/{name}/versions/{version}/download` | | `200` the setup archive, `Content-Type: application/gzip`, `X-Hlock-Digest: sha256:<hex>` (and optionally `Digest: sha-256=<base64>`) |
| `PUT /packages/{ns}/{name}/versions/{version}` | the setup archive, `Content-Type: application/gzip` (a setup) or `application/zip` (an APM package); `X-Hlock-Evals-Digest: sha256:<hex>` when the version will have an evals archive | `201 {package, version, digest, published_at, size_bytes, files, url, evals}`; `409` if the version exists; `413` if too large; `422` (with `extensions.errors`) if invalid; `401` / `403` |
| `PUT /packages/{ns}/{name}/versions/{version}/evals` | the evals archive, `Content-Type: application/gzip` | `201 {digest, size_bytes}`; `409` if the version declared no evals digest or another one, or already has its evals archive; `404` if there's no such version; `413`, `422` as for setups |
| `GET /packages/{ns}/{name}/versions/{version}/evals` | | `200` the evals archive, `X-Hlock-Digest: sha256:<hex>` |

```text
Result  = {ref, type, source, kinds, description, latest_version, harness, models, mcp,
           skills, apm_dependencies, pull, updated_at, pulls, stars}
Package = {package, type, source, description, kinds, pull?, latest?, versions: [Version]}
Version = {version, digest, published_at, size_bytes, yanked, files: [{path, digest, size}],
           pull?, evals}
Evals   = {digest, uploaded} | null
```

- `type` is `setup` or `apm-package`; `source` is `hosted`, `github` or `gitlab`; `kinds` lists
  `harness` (the setup has `harness.toml` or `harness.lock`), `apm` (`apm.yml` or
  `apm.lock.yaml`) and `evals` (the version has an evals archive).
- `pull` is what to pass to `hlock pull`: `ns/name` for a hosted setup, or a git source
  (`github:owner/repo@<commit>`) for a remote one. A remote package's `Version.pull` pins its
  commit.
- `digest` is `sha256:` and the hex SHA-256 of the archive bytes.
- `evals` is the evals archive the version committed to (`X-Hlock-Evals-Digest` on its first
  `PUT`), and whether it's been uploaded; `null` when it committed to none. A server that
  supports eval packs always includes the field (in the `201` too); a server that doesn't
  leaves it out, and that's how a client tells them apart (§6, Push).

## 5. The archives

### 5.1 The setup archive

A gzip'd tar. A server must refuse (`422`), and the client refuses before writing anything:

- an entry that isn't a regular file or a directory (links, devices);
- an absolute path, or one with a `..` segment;
- a path other than:
  - `harness.toml`, `harness.lock`, `apm.yml`, `apm.yaml` (written as `apm.yml`),
    `apm.lock.yaml`, `README.md`;
  - a **nested harness file**: `harness.toml`, `harness.lock` or `harness.NAME.lock` under
    `.hlock/`, such as `.hlock/locks/harness.pi.lock`, but not under any `.hlock/vendor/` or
    under one of the directories hlock keeps its own state in (`.hlock/vendor`, `env`, `store`,
    `tune`, `tuned`, `trials`, `runs`, `result`, `report`, `bak`, `meta`, `cache`, `docker`,
    `sandbox-env`, `pulls`, `approve`, `trust`);
  - a **vendored file**: `D/.hlock/vendor/REST`, where `D` is the root (`.hlock/vendor/REST`)
    or a directory under `.hlock/` outside any `.hlock/vendor/`. A lock's `vendored` paths are
    relative to the lock's own directory, so `.hlock/locks/harness.pi.lock` vendors into
    `.hlock/locks/.hlock/vendor/`;
- an archive with none of `harness.toml`, `harness.lock`, `apm.yml`, `apm.lock.yaml`, or a
  nested harness file;
- an entry whose size isn't its header's (a PAX `size` record that differs from the ustar
  size: the limits count the ustar sizes, and readers read the PAX one);
- more than 500 entries, more than 4 MiB compressed, or more than 64 MiB unpacked.

A server should also check that `harness.lock` and `harness.toml` parse as TOML and the APM
files as YAML.

Compatibility: hlock 0.6.0 allowed only the root files and `.hlock/vendor/`, so it refuses an
archive with nested harness files ("isn't a file a setup may contain"); a newer hlock pulls it.

`hlock push` writes entries sorted by path, as ustar regular files with mode `0644`, mtime 0,
uid and gid 0 and no user or group names, gzip'd with mtime 0 and OS 255, so the same files
give the same bytes, and the same digest.

### 5.2 The evals archive

The project's eval packs, so that someone pulling a resolved setup can rerun the evidence its
lock records (`[evidence] eval-pack-digest`). It's a separate archive so that the setup
archive's rules, limits and clients don't change, and pulling a lock into the store never
downloads the packs.

**A pack path** is relative, with `/`-separated components that each match
`[A-Za-z0-9_-][A-Za-z0-9._-]*` and don't end in `.`: no `.` or `..`, no leading dots (so no
`.hlock/`, `.claude/`, `.github/` …), nothing outside ASCII, no `\` or `:`.

**A pack directory** in the archive is a directory holding `pack.toml` whose path is a pack
path and none of whose ancestors is a pack directory.

A gzip'd tar, deterministic like the setup archive, but each entry's mode is `0644` or
`0755` (a file that's executable on disk; graders run scripts, and the packs' digest records
the executable bit). A server must refuse (`422`), and the client refuses before writing
anything:

- an entry that isn't a regular file or a directory, or a mode other than `0644` and `0755`;
- an absolute path, or one with a `..` segment;
- a path that isn't inside a pack directory;
- a path with a component that's empty, `.`, `..`, has characters other than `A-Za-z0-9._+@=,-`,
  ends in `.`, is a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
  `LPT1`–`LPT9`, with or without an extension, in any case), or is `.git` or `.hlock` (in any
  case);
- two paths that are equal when lowercased;
- an entry whose size isn't its header's, as for the setup archive;
- more than 5,000 entries, more than 32 MiB compressed, or more than 256 MiB unpacked.

A server needs no TOML for this. The client also refuses, on pull, a pack directory that the
setup archive's root `harness.toml` doesn't list in `[evals] packs`.

**Which packs `push` packs.** The string entries of the root `harness.toml`'s `[evals] packs`
(or the old `[tasks] packs`); a leading `./` and a trailing `/` are dropped. An entry with a
`:` (a remote pack) is left out with a note. Every other entry must be a pack path that isn't
equal to, inside, or around another one, isn't a setup file's name, and names a directory with
`pack.toml` inside the project, reached through no symlink; otherwise `push` stops with an
error naming it (so the pushing and pulling sides never compute the packs' digest from
different files). Each pack is packed as exactly the files `tree_digest` hashes: everything
but `.git`, `.DS_Store`, `__pycache__`, `node_modules` and symlinks. `push` stops, listing every
problem, on a file that breaks the path rules above, one it can't read or that's larger than
20 MiB (it can't be scanned), one with critical hidden characters, and one that looks like a
credential:

- a private key block (`-----BEGIN … PRIVATE KEY-----`) in any file;
- a credential-shaped value in a TOML, JSON or YAML file;
- on a line of a text file, a token with a known credential prefix (`sk-`, `ghp_`, `AKIA`,
  `xoxb-`, `npm_`, …), or a `key = value` / `key: value` whose key names a credential
  (`password`, `api_key`, `token`, …) and whose value is a literal of at least 8 characters
  that isn't a placeholder (`${…}`, `HLOCK_SECRET_…`). A value that's an expression
  (`os.environ["KEY"]`) isn't a literal. The general long-mixed-case-token rule the setup
  archive uses isn't applied to free text, where fixtures are full of hashes;
- a file named `.env` or `.env.*` (except `.env.example`), `.netrc`, `.npmrc`, `.pypirc`,
  `credentials` (in any directory), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`,
  `id_ed25519*` or `id_ecdsa*`.

`push --no-evals` publishes without the eval packs.

## 6. Client behavior

**Configuration.** The registry is `--registry`, else `HLOCK_REGISTRY`, else `[registry] url`
in `~/.hlock/config.toml` (under `HLOCK_HOME`), which `login` writes. The token is
`HLOCK_REGISTRY_TOKEN`, else the OS keychain (service `hlock`, account
`HLOCK_SECRET_REGISTRY_<HOST>`, the host upper-cased with other characters as `_`, since
keychain names are plain variable names), else `~/.hlock/credentials.toml`:

```toml
[registries."https://registry.example.com"]
token = "…"
```

created with mode `0600` before the token is written to it. `HLOCK_KEYCHAIN=0` skips the
keychain.

**Push.** The project pushed is the **root**: `--root DIR` (`-C` and `--dir` are aliases), else
the root of the git work tree the working directory is in (`git rev-parse --show-toplevel`,
without the caller's `GIT_DIR` or `GIT_WORK_TREE`), else the working directory. Only git's
"not a git repository (or any …)" means "not in one"; when git fails otherwise (it isn't
installed, `safe.directory`, a broken `.git`), the root is the working directory and push warns
with git's first line of error. The result says which (`root`, and `root_from`: `flag`,
`repository`, `current-directory`, `current-directory-git-failed`). Pushing the repository's
root from a subdirectory that has a setup of its own warns that that setup isn't the one
pushed. Two names for `--root` with different values are a usage error. When the
root is a repository's and has no setup, push fails and names the `--root` of the nearest
directory from the working directory up that has one. Push packs the setup archive (§5.1),
refusing values that look like credentials (by key name, or by shape: `sk-…`, `ghp_…`, long
mixed-case tokens) and critical hidden characters, and the evals archive (§5.2), before
anything is sent.

The name is `NAMESPACE/NAME`, else `whoami`'s `user.handle` and the first of:

1. the repository name of the project's git remote (`origin`, else the only remote), the last
   segment of its URL without `.git`, when the project directory is the root of its git work
   tree (a project in a subdirectory of a repository skips this, so sibling projects don't
   collide);
2. `harness.toml`'s `[project] name`;
3. `apm.yml`'s `name`;
4. the directory's name;

slugified. The output says where the name came from, never the remote's URL. When the name
came from the git remote and the registry doesn't have `NS/<repo>`, push checks each name it
would otherwise have defaulted to, in order (`[project] name`, unless it's `demo`, which
`hlock init` writes and unrelated projects share; `apm.yml`'s `name`; the directory's). If the
registry has `NS/<that>`, push stops ("a package named NS/X exists") and asks for
`NAMESPACE/NAME`, so people pulling the old name don't silently stop getting updates.

The version is `--version`, else `apm.yml`'s `version`, else `sha-` and the first 12 hex of
the SHA-256 of `<setup digest>\n<evals digest>` when there's an evals archive (so changing
only a pack gives a new version), of the setup archive otherwise.

The setup `PUT` carries `X-Hlock-Evals-Digest` when there's an evals archive. Then:

- the `201`, or on a `409` the existing `Version` (whose `digest` must be the setup archive's,
  or push fails: versions can't change), has no `evals` field: the registry doesn't take eval
  packs. The client warns that it published without them; a rerun is "already published";
- `evals` is `null` or has another digest: push fails (the version is published with no, or
  other, eval packs; push another `--version`);
- `evals.uploaded` is `true`: done;
- otherwise the client `PUT`s the evals archive. A `409` there is success when the version now
  has these eval packs uploaded. So a rerun after a failed upload completes it.

Push warns, for each lock it packs whose `[evidence] eval-pack-digest` isn't the digest of the
eval packs being pushed, that its evidence was measured on other packs.

**Pull.** A registry reference is `NAMESPACE/NAME[//PATH][@VERSION]`, the version last as in
git sources (`github:o/r//path@ref`): `@VERSION` is what follows the last `@` after the last
`/`, and the rest is split at its first `//` into the package and PATH (relative, inside the
package, without `@`). `NS/NAME@VERSION//PATH` is refused with the corrected reference. A bare
reference that isn't an existing file is a registry reference; `github:`, `gitlab:`, `git:` and
URLs stay git sources.

The client reads the package; an `apm-package` is refused with a pointer to `apm install`; a
remote one is pulled from its `pull` git source, with PATH joined to that source's own path
(a package at `github:o/mono//agents/review@<sha>` pulls `//.hlock/locks/harness.pi.lock` as
`agents/review/.hlock/locks/harness.pi.lock`). A hosted one: read the version, download, check the bytes against the version's `digest`
and `X-Hlock-Digest`, unpack into a scratch directory (§5.1), choose the lock, then place it as
a git pull does.

The **locks in a package** are the root `harness.lock` and `harness.*.lock`, and its nested
harness files that end in `.lock`. Into the store or a project lock file (`-o FILE`):

- a PATH ending in `.lock` takes that lock, which must be one of the package's;
- a PATH that's a directory takes, in it, `harness.lock`, else the only `harness.*.lock`, else
  fails listing them;
- no PATH takes the root `harness.lock`, else the package's only lock, else fails listing them.
  With a root lock and others, the output lists the others and the reference that pulls each
  (`hlock pull alice/hlock//.hlock/locks/harness.pi.lock`).

The lock's vendored files are copied from its own directory's `.hlock/vendor/`. A lock goes in
the store as `NAME` for the root lock and `NAME-X` for `harness.X.lock`; a stored lock of that
name from another source, or from another path in the same package, isn't replaced without
`--force` (`--as NAME` keeps both). The `[pulled]` table:

```toml
[pulled]
from = "registry:https://registry.example.com/alice/hlock//.hlock/locks/harness.pi.lock"   # "@1.0.0" when pinned
url = "https://registry.example.com/alice/hlock"
path = ".hlock/locks/harness.pi.lock"
version = "1.0.0"
digest = "sha256:…"
ref = "1.0.0"                                                     # when pinned
at = "2026-10-22T21:08:32Z"
```

`from` has `//PATH` whenever the lock isn't the root `harness.lock`, so a refresh (`hlock
pull` with no source) pulls the same lock, and fails if the package no longer has it. To read
`from` back, strip `registry:` and the URL's `scheme://`, then split at the first `//`. An hlock
from before paths misreads a `from` with `//PATH` (it takes part of the path for the package)
and fails with a not-found error rather than refresh another lock.

`--root DIR` (alias `--into DIR`) writes every file of the setup archive with DIR as its root, refusing to replace existing
files without `--force`, and takes no PATH. When the version has an uploaded evals archive (and
no `--no-evals`), it downloads it too (at most 32 MiB), checks it against `Version.evals.digest`
and `X-Hlock-Digest`, unpacks it (§5.2, restoring `0755` explicitly), scans it for hidden
characters like the rest (honouring `--allow-hidden`), and writes the packs. A pack
directory that's already in DIR is a conflict like an existing file; with `--force` the whole
directory is replaced, never merged into, so no task of another version survives and the
packs' digest matches the lock's. A version whose evals archive is committed to but not
uploaded is refused.
The lock's `[pulled]` gains `evals-digest`, and the output warns that eval packs run commands
when graded and their fixtures are the agent's workspace. Store pulls and `-o FILE` never
fetch the evals archive.

**APM packages.** These go over APM's own endpoints, which an hlock registry serves under the
same `/api/v1` prefix, so its APM base URL (for `apm.yml`'s `registries:`) is `{url}/api`.
`hlock apm push` needs `apm.yml` with `name` and `version` and a non-empty `.apm/`, and packs
the flat zip `apm publish` makes: `apm.yml` (from `apm.yaml` if that's the file), `.apm/`,
and the root's `README.md`, `CHANGELOG.md` and `LICENSE`/`LICENCE` (case-insensitive), without
symlinks, `.DS_Store` or `._*`. Entries are sorted, deflated, dated 1980-01-01 and mode
`0644`. It runs `push`'s credential and hidden-character checks, then `PUT`s the zip as
`apm.yml`'s version (at most 50 MiB). On `409`, the version list decides between "already
published" and an error. `hlock apm pull` reads the version list, takes `@VERSION` or the
newest that isn't yanked (the highest semver, else the last `published_at`), downloads it,
checks its digest, and unpacks a zip or a tar.gz into a scratch directory, refusing links,
absolute paths, `..`, more than 5000 entries and more than 256 MiB. It refuses an archive with
no `apm.yml` (pointing at `hlock pull` when it's a setup) and critical hidden characters
outside `--allow-hidden`, then writes the files into `--into` (default `./NAME`), replacing
existing ones only with `--force`.

## 7. Design notes

- **Why is a package a project, not a lock?** The files change together: a lock's evidence is
  only meaningful with the eval packs at the digest it records, and `harness.toml` and its locks
  are reconciled with each other. One version per push keeps a consistent snapshot; a version
  per lock would lose which pack went with which lock. It mirrors git pulls too, where the
  repository is the source and a lock is chosen inside it, and gives one search result per
  project.
- **Why name it after the repository?** It's what someone pushing thinks they're publishing.
  `[project] name` is rarely chosen (`hlock init` writes `demo`, `hlock add` the directory's
  name), so it's the fallback. Only at the work tree's root, so a monorepo's projects don't
  collide; never after the git owner, so a team publishes under its org and a fork doesn't
  collide with its upstream.
- **Why a second archive for eval packs?** A pack's paths are arbitrary, so allowing them in the
  setup archive would break every older client's pull. It also keeps their larger limits, and
  their download, away from store pulls. The setup `PUT` commits to the evals archive's digest,
  so a version can't gain or change eval packs after anyone has pulled it.
- **Why such strict pack paths?** The setup's own `harness.toml` names the packs, so a
  publisher could otherwise make `pull --root` write `.claude/settings.json` hooks, CI workflows
  or editor config into someone's project. No leading dots in a pack path, ASCII-only
  components and no case collisions keep that out on every OS.
- **Why `hlock apm push` as well as `push`?** A project's `apm.yml` + `apm.lock.yaml` isn't an
  APM package (APM publishes only packages with `.apm/`), and it usually sits next to
  `harness.lock`, which hlock reconciles with it, so one `push` carries all four files. An APM
  package is something else, which `apm publish` already handles; `hlock apm push` and `pull`
  are a convenience that reuse `hlock login`'s token and registry and `push`'s checks, and
  publish the same archive `apm publish` would, so APM installs it unchanged.
- **Why tar.gz and not OCI?** One request to publish and one to fetch, digests anyone can
  compute with `sha256sum`, and the same shape as APM's archives. An OCI mapping can come
  later without changing the client's checks.
- **Why no default registry?** hlock works offline and with git alone; a registry is a
  choice, made once with `hlock login URL`.
