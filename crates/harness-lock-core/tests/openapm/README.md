# OpenAPM v0.1 conformance fixtures

Copied unchanged from [microsoft/apm](https://github.com/microsoft/apm) at commit
`897496177ba389c4708b110efa1ee5345ccefb79` (2026-10-20), from
`tests/fixtures/spec-conformance/`. They're MIT-licensed, Copyright (c) Microsoft
Corporation; see `LICENSE` in this directory.

`tests/openapm.rs` runs them against hlock's dependency resolver (`src/deps.rs`) and its
`apm.yml` / `apm.lock.yaml` readers (`src/apm.rs`):

| Fixture | Requirement | What's checked |
|---|---|---|
| `resolution/semver-dialect.json` | req-rs-007, req-rs-014 | every range picks the expected tag, including `||`, comma-AND, pre-release opt-in and the build-metadata tie-break |
| `lockfile/*.yml` | req-lk-001, req-lk-005, req-lk-011 | each reads, with its commits, refs and depths |
| `manifest/valid-*.yml`, `manifest/x-extension-roundtrip.yml` | req-mf-001…004, req-mf-006 | each reads |
| `manifest/invalid-*.yml` | req-mf-001, req-mf-002, req-mf-012, req-mf-020 | each is refused |

To upgrade to a new OpenAPM version, copy the new fixtures over these, update the commit
above and `deps::OPENAPM_VERSION`, and fix what the tests report.
