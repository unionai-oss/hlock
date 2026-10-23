# The hlock registry HTTP API v1

Status: implemented (client) · Last updated: 2026-10-22 · Builds on [pull](pull.md)

> A registry keeps published agent setups and finds them by name. This is the protocol
> between `hlock` and a registry. It's open: any server that implements it works with
> `hlock login`, `push`, `search` and `pull NAMESPACE/NAME`, and hlock has no default one.

---

## 1. Summary

```bash
hlock login https://registry.example.com      # device login (RFC 8628); token in the keychain
hlock push                                    # → <handle>/<project>@<version>
hlock search "code review" --kind apm
hlock pull alice/review-bot[@1.0.0]           # into ~/.hlock/pulls/review-bot/
hlock pull alice/review-bot --into .          # the whole setup into a project
```

A **setup** is whichever of `harness.toml`, `harness.lock` (with the files it vendors under
`.hlock/vendor/`), `apm.yml`, `apm.lock.yaml` and `README.md` a project has. A registry may
also list APM packages (`type = "apm-package"`, published with `apm publish` over APM's own
[Registry HTTP API](https://microsoft.github.io/apm/reference/registry-http-api/)), and
setups that live in git repositories (`source = "github"` or `"gitlab"`), which hlock pulls
with git.

The API is shaped like APM's registry API (`versions`, `versions/{v}/download`, `PUT
versions/{v}`, RFC 7807 errors, Bearer tokens), so one server can serve both.

## 2. Goals and non-goals

Goals:

- Publish and pull a setup by name, with immutable, content-addressed versions.
- Nothing hlock-specific about the server: a small, documented protocol a third party can
  implement, with no default registry baked into hlock.
- A pulled setup is checked exactly like a git pull: digest, unsafe archive entries, hidden
  characters, project policy, and nothing runs until `sync` or `launch`.
- Credentials never in a project file: the OS keychain, an owner-only file, or the
  environment for CI.

Non-goals (for now): private packages, tags other than `latest`, yanking from the CLI,
signatures, and a registry server (implementations live elsewhere).

## 3. Conventions

- **Base URL**: the registry URL, e.g. `https://registry.example.com` (it may have a path).
  Every endpoint is under `{base}/api/v1`. Plain `http` is accepted only for `localhost`,
  `127.0.0.1` and `[::1]`.
- **JSON** is `snake_case`. **Errors** are RFC 7807 `application/problem+json` with `title`,
  `status`, and optionally `detail` and `extensions.errors` (a list of strings).
- **Auth**: `Authorization: Bearer <token>`. Reads of public packages may be anonymous.
- **Identity**: `namespace` is `[a-z0-9-]{1,39}` (not starting or ending with `-`); `name` is
  `[a-z0-9._-]{1,100}`, starting with a letter or digit; `version` is `[A-Za-z0-9._+-]{1,64}`,
  starting with a letter or digit. `latest` names the newest version that isn't yanked.

## 4. Endpoints

| Method + path | Request | Response |
|---|---|---|
| `POST /auth/device` | `{client_name}` | `200 {device_code, user_code, verification_uri, verification_uri_complete?, expires_in, interval}` |
| `POST /auth/device/token` | `{device_code}` | `200 {access_token, token_type, scopes, user}`, or `400 {error}` with `error` one of `authorization_pending`, `slow_down`, `expired_token`, `access_denied` (RFC 8628 §3.5) |
| `GET /whoami` | | `200 {user: {handle, name}, namespaces: [string], scopes: [string]}`; `401` without a valid token |
| `GET /search?q=&type=&kind=&source=&harness=&page=&per_page=` | | `200 {total, page, per_page, results: [Result]}` |
| `GET /packages/{ns}/{name}` | | `200 Package` |
| `GET /packages/{ns}/{name}/versions/{version}` | | `200 Version` |
| `GET /packages/{ns}/{name}/versions/{version}/download` | | `200` the archive bytes, `Content-Type: application/gzip`, `X-Hlock-Digest: sha256:<hex>` (and optionally `Digest: sha-256=<base64>`) |
| `PUT /packages/{ns}/{name}/versions/{version}` | the archive bytes, `Content-Type: application/gzip` | `201 {package, version, digest, published_at, size_bytes, files, url}`; `409` if the version exists; `413` if too large; `422` (with `extensions.errors`) if invalid; `401` / `403` |

```text
Result  = {ref, type, source, kinds, description, latest_version, harness, models, mcp,
           skills, apm_dependencies, pull, updated_at, pulls, stars}
Package = {package, type, source, description, kinds, pull?, latest?, versions: [Version]}
Version = {version, digest, published_at, size_bytes, yanked, files: [{path, digest, size}],
           pull?}
```

- `type` is `setup` or `apm-package`; `source` is `hosted`, `github` or `gitlab`; `kinds` lists
  `harness` (the setup has `harness.toml` or `harness.lock`) and `apm` (`apm.yml` or
  `apm.lock.yaml`).
- `pull` is what to pass to `hlock pull`: `ns/name` for a hosted setup, or a git source
  (`github:owner/repo@<commit>`) for a remote one. A remote package's `Version.pull` pins its
  commit.
- `digest` is `sha256:` and the hex SHA-256 of the archive bytes.

## 5. The archive

A gzip'd tar. A server must refuse (`422`), and the client refuses before writing anything:

- an entry that isn't a regular file or a directory (links, devices);
- an absolute path, or one with a `..` segment;
- a path other than `harness.toml`, `harness.lock`, `apm.yml`, `apm.yaml` (written as
  `apm.yml`), `apm.lock.yaml`, `README.md`, or one under `.hlock/vendor/`;
- an archive with none of `harness.toml`, `harness.lock`, `apm.yml`, `apm.lock.yaml`;
- more than 500 entries, more than 4 MiB compressed, or more than 64 MiB unpacked.

A server should also check that `harness.lock` and `harness.toml` parse as TOML and the APM
files as YAML.

`hlock push` writes entries sorted by path, as ustar regular files with mode `0644`, mtime 0,
uid and gid 0 and no user or group names, gzip'd with mtime 0 and OS 255, so the same files
give the same bytes, and the same digest.

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

**Push.** Packs the setup (§5), refusing values that look like credentials (by key name, or
by shape: `sk-…`, `ghp_…`, long mixed-case tokens) and critical hidden characters. The name
defaults to `whoami`'s `user.handle` and the project's name; the version to `--version`, else
`apm.yml`'s `version`, else `sha-<first 12 hex of the digest>`. On `409`, the client reads the
version: the same digest is success ("already published"), another is an error.

**Pull.** A bare `NAMESPACE/NAME[@VERSION]` that isn't an existing file is a registry
reference; `github:`, `gitlab:`, `git:` and URLs stay git sources. The client reads the
package; an `apm-package` is refused with a pointer to `apm install`; a remote one is pulled
from its `pull` git source. A hosted one: read the version, download, check the bytes against
the version's `digest` and `X-Hlock-Digest`, unpack into a scratch directory (§5), then place
it as a git pull does, with this `[pulled]` table:

```toml
[pulled]
from = "registry:https://registry.example.com/alice/review-bot"   # "@1.0.0" when pinned
url = "https://registry.example.com/alice/review-bot"
path = "harness.lock"
version = "1.0.0"
digest = "sha256:…"
ref = "1.0.0"                                                     # when pinned
at = "2026-10-22T21:08:32Z"
```

`hlock pull` with no source refreshes it from `from`. `--into DIR` writes every file of the
setup into DIR, refusing to replace existing files without `--force`.

## 7. Design notes

- **Why not `hlock apm push`?** APM packages already have `apm publish` and APM's registry
  API; an hlock command for them would duplicate it. A project's `apm.yml` + `apm.lock.yaml`
  isn't an APM package (APM publishes only packages with `.apm/`), and it usually sits next to
  `harness.lock`, which hlock reconciles with it, so one `push` carries all four files.
- **Why tar.gz and not OCI?** One request to publish and one to fetch, digests anyone can
  compute with `sha256sum`, and the same shape as APM's archives. An OCI mapping can come
  later without changing the client's checks.
- **Why no default registry?** hlock works offline and with git alone; a registry is a
  choice, made once with `hlock login URL`.
