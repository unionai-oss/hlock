# The union provider: hlock push/pull into a Union Artifact store

Status: implemented (client) · Builds on [registry](registry.md) and [pull](pull.md) ·
Mirrors `unionai-oss/hlock-cloud`'s `web/lib/union.ts`

> A registry is an HTTP server hlock talks to over the [registry API](registry.md). A **union**
> provider is the other way to give a setup a home: the archive goes straight into a Union
> (Flyte 2) **Artifact** — the versioned, write-once object store a Union tenant exposes through
> `flyteidl2.artifact.ArtifactService`. `hlock push union:NS/NAME` and
> `hlock pull union:NS/NAME[@VERSION]` treat a Union Artifact as a content-addressed version.

```bash
hlock push union:acme/review-bot            # → artifact setup.acme.review-bot@sha-<digest>
hlock pull union:acme/review-bot@1.2.0      # its setup, like a registry pull (VERSION required)
hlock push union:acme/review-bot --flyte-config ./.flyte/config.yaml
```

## 1. Why a union store

The registry API is open and self-hostable, but it needs a server someone runs. Union hosts
Artifacts as part of its platform — a publisher on Union can point hlock at their tenant and
publish without standing up a registry. Each package version is one immutable Artifact whose
value is the exact setup archive bytes, so reading it back is one call and content never
changes once written.

## 2. Finding the store: a flyte config

Like the `flyte` CLI, hlock reads where the tenant is from a `config.yaml`: `admin.endpoint`
(who to reach), `task.org`, `task.project` and `task.domain` (which artifact store inside it).

| Where hlock looks (in order) | |
|---|---|
| `--flyte-config PATH` | any path |
| `HLOCK_UNION_CONFIG` | a path in the environment |
| `./.flyte/config.yaml` | the project's flyte config |
| `~/.flyte/config.yaml` | the user's flyte config |

`admin.endpoint` may be `dns:///host`, `https://host` or a bare host; hlock strips the prefix
and always talks TLS (plain `http` is accepted only for loopback, so a local store works).

## 3. Authenticating: a Union API key

The config says *where* the store is; the credential is a **Union API key** in `FLYTE_API_KEY`,
the same key the hlock-cloud web app reads — `base64("endpoint:client_id:client_secret:org")`.
hlock:

1. `GET {base}/.well-known/oauth-authorization-server` for the token endpoint (RFC 8414),
2. `POST {base}/flyteidl.service.AuthMetadataService/GetPublicClientConfig` for the scopes and
   the auth header name (`authorizationMetadataKey`),
3. `POST` the token endpoint with `grant_type=client_credentials` (Basic auth over the key's
   `client_id:client_secret`) → a bearer token, cached until just before it expires.

The artifact scope's `org` is the config's `task.org`, falling back to the key's org.

## 4. The artifact: a setup archive as a binary literal

The write and read are the `flyteidl2.artifact.ArtifactService` over Connect-JSON (the default
`application/json` encoding), exactly as hlock-cloud's `union.ts` does.

`POST /flyteidl2.artifact.ArtifactService/CreateArtifact`:

```json
{
  "artifactId": { "name": { "org": "demo", "project": "hlock-cloud", "domain": "development", "name": "setup.acme.review-bot" }, "version": "sha-abc123" },
  "spec": {
    "value": { "scalar": { "binary": { "value": "<base64 of the tar.gz>", "tag": "sha256:…" } } },
    "type": { "simple": "BINARY" },
    "info": { "description": "hlock setup acme/review-bot", "userMetadata": { "package": "acme/review-bot", "digest": "sha256:…" } }
  }
}
```

`POST /flyteidl2.artifact.ArtifactService/GetArtifact` `{ "name": {...same name...}, "version" }`
returns the artifact with the bytes back in `spec.value.scalar.binary.value`. CreateArtifact is
write-once: a version that exists fails with an "already exists" error, which hlock turns into
the same "already published" path a registry's `409` gives (the existing version is read back
and its digest checked, so a version can never silently change).

### Naming

Artifact names are `kind.<segment>…`, lowercased, with unsafe characters slugged to `-` and a
`.h<sha256>⁸` suffix when cleaning changed a segment (so `acme/my.repo` and `acme/my-repo`
never collide). hlock uses `setup` as the kind, so `union:acme/review-bot` →
`setup.acme.review-bot`. Eval packs (with `--root`, unless `--no-evals`) are a sibling artifact
`setup.acme.review-bot.evals` at the same version. Versions are sanitized to
`[A-Za-z0-9._+-]`; a missing `@VERSION` on pull is an error, because without a registry tracking
versions a Union store has no "latest" to resolve to.

## 5. Pull

A union pull fetches the artifact, verifies its SHA-256 against the recorded digest, unpacks
and places it exactly like a [registry pull](registry.md): the same archive checks, the same
`[pulled]` table with `from = "union:NS/NAME@VERSION"`, and the same `sync`/`launch` next steps.
Refreshing a pulled union lock re-reads the flyte config and refetches the same version.
