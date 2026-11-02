# Publishing to a Union Artifact store

`hlock push` and `hlock pull` normally talk to a [registry](registry.md) — an HTTP server that
speaks hlock's open protocol. With `union:`, the archive goes straight into a
[Union](https://union.ai) (Flyte 2) **Artifact** instead: a versioned, write-once object store
on the Union tenant you point hlock at. No registry to run; one command to push, one to pull.

```console
$ hlock push union:acme/review-bot
✓ pushed acme/review-bot@sha-0f3a9c11e2  (3 files, 7.3 kB, sha256:12eaa750e9e…)
next → hlock pull union:acme/review-bot@sha-0f3a9c11e2
```

## Pointing hlock at the store: a flyte config

`union:` needs a flyte `config.yaml` to know where the store is — the same file
[the `flyte` CLI](https://docs.union.ai) reads. hlock looks in order for:

1. `--flyte-config PATH`
2. `HLOCK_UNION_CONFIG`
3. `./.flyte/config.yaml`
4. `~/.flyte/config.yaml`

```yaml
# ./.flyte/config.yaml
admin:
  endpoint: dns:///demo.hosted.unionai.cloud
task:
  org: demo
  project: hlock-cloud
  domain: development
```

`admin.endpoint` names the host (with or without `dns:///` or `https://`); `task.project` and
`task.domain` pick the artifact store inside it; `task.org` is the store's org (when it's empty,
the one in your API key is used).

## Logging in: the Union API key

The config says *where*; the credential that says *who* is a **Union API key** in `FLYTE_API_KEY`
— `base64` of `endpoint:client_id:client_secret:org`, the same key the
[hlock-cloud](https://github.com/unionai-oss/hlock-cloud) web app uses. Make one in the Union
console (an "API key"), then:

```console
$ export FLYTE_API_KEY=$(printf 'demo.hosted.unionai.cloud:CLIENT:SECRET:demo' | base64)
```

The endpoint in the key and in the config should name the same host. hlock authenticates with
it (OAuth `client_credentials`) every time it writes or reads.

## Publish

```console
$ hlock push union:acme/review-bot
```

Packs what exists of the setup — `harness.toml`, `harness.lock` (with the files it vendors),
`apm.yml`, `apm.lock.yaml`, `README.md` and the harness files under `.hlock/` — checks it for
secrets and hidden characters (as every push does), and stores it as a Union Artifact named
`setup.acme.review-bot`. Pushing the same content twice is safe: a version is written once, so
a second push of the same version reports it's already published (the stored bytes are checked
rather than assumed identical).

```console
$ hlock push union:acme/review-bot --dry-run --json   # list files and digest, push nothing
$ hlock push union:acme/review-bot --version 1.2.0    # an explicit version
$ hlock push --root agents/review union:agents/review  # one project of a monorepo
```

Eval packs (`harness.toml`'s `[evals] packs`) go in a sibling artifact
`setup.acme.review-bot.evals` at the same version; `--no-evals` leaves them out.

## Pull

```console
$ hlock pull union:acme/review-bot@1.2.0
```

Fetches the artifact, verifies its digest, and places the lock and its vendored files like any
other pull — into `~/.hlock/pulls/review-bot/` so `hlock launch --lock review-bot` and
`hlock sync` can use it. A source needs an explicit `@VERSION`: a Union store has no "latest"
the way a registry (which tracks versions separately) does.

```console
$ hlock pull union:acme/review-bot@1.2.0 --root .            # the whole setup into a project
$ hlock pull union:acme/review-bot@1.2.0 -o harness.acme.lock  # one lock into a file
```

Once pulled, the lock records `from = "union:acme/review-bot@1.2.0"`, and a bare `hlock pull`
refreshes it from the same store.
