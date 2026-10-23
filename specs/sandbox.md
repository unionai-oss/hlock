# Sandboxes: running a locked agent inside an isolation runtime

Status: accepted, phase 1 in progress · Owner: Niels Bantilan · Last updated: 2026-10-22 · Builds on [v0 §7.5](v0.md#75-sandboxes), [secrets](secrets.md)

> hlock pins *which* agent setup runs. It says nothing about *how* that setup runs: the
> agent, its MCP servers and the eval graders all run as the user, with the user's files,
> network and credentials. Sandbox runtimes (Docker, macOS `sandbox-exec`, bubblewrap,
> Docker's `sbx` microVMs, NVIDIA OpenShell) solve that half. This spec lets a lock name
> the sandbox it runs in, runs the agent and the code that grades it through that
> sandbox, keeps model-provider key *values* out of a sandboxed agent's reach, and records
> what sandbox the evidence was measured under.

---

## 1. Problem

1. **No process isolation.** `hlock launch`, `hlock run`, `hlock evals run`, a code
   harness's trials in `resolve`/`tune`, and every `tests` grader run with the user's
   permissions. A grader "often runs code the agent wrote" (eval-packs guide). The isolated
   config root (`.hlock/env/`) keeps *config* out; it doesn't keep the agent in.
2. **Secrets are readable by the agent.** Provider keys and `HLOCK_SECRET_*` values reach
   the harness through its environment, so anything the agent runs can print them
   ([secrets §3](secrets.md), "exposure trade-off").
3. **The runtime isn't pinned.** `[policy] sandbox` exists in the lock schema, but nothing
   writes or reads it.
4. **Evidence doesn't say what it was measured under.**
5. **Nothing to plug a runtime into.** `launch` `exec`s the harness directly.

## 2. Non-goals (phase 1)

- **Building a sandbox.** hlock wraps runtimes that exist and doesn't verify a third-party
  runtime's claims.
- **Per-host egress allowlists.** Network is `open` or `none`. A runtime with its own
  egress policy (OpenShell's YAML, a seatbelt profile) takes it through `profile` (§4.3);
  hlock pins the file, it doesn't interpret it.
- **Proxying every secret.** Phase 1 proxies model-provider API keys, on `docker` only
  (§6). MCP tokens and other lock `[[secret]]`s travel by environment, and the run says so.
- **Meta jobs and translated launches** (`hlock meta run`, `hlock launch pi` on a Claude
  lock) refuse a sandbox. They run with `--no-sandbox`, which a policy with `require`
  refuses, so under `require` they don't run at all.
- **Agent-CLI trials in `resolve`/`tune`.** They're simulated in process
  (`resolver.rs` sends non-code harnesses to the reference adapter): nothing runs. Code-
  harness trials and graders run for real, and those are sandboxed.
- **Turnkey `sbx` / OpenShell support.** Both own their VM's image, mounts and credential
  injection. They can be command runners (§4.3), but the proxy can't reach a VM, and
  their exact CLIs aren't tested here. Phase 2.

## 3. Model

A **sandbox** is a runner plus a policy:

| Part | Meaning |
|---|---|
| **runner** | `none`, `docker`, or a named command runner |
| **network** | `open` or `none` |
| **secrets** | `env` (today's behavior) or `proxy` (§6, `docker` only) |
| **profile** | Optional runtime-native policy file, pinned by digest |
| **applies-to** | Which of hlock's process kinds run inside: `agent`, `trials`, `graders` |

| Kind | What runs | Started by |
|---|---|---|
| `agent` | An agent CLI: `launch`, `run`, `evals run` | Python, from `LaunchPrep` |
| `trials` | A code harness: its `resolve`/`tune` trials, and `run`/`evals run` of a code harness | Rust, `contract::run` |
| `graders` | A `tests` grader: `evals run`, `evals grade`, `resolve`, `tune` | Rust, `graders::tests` |

hlock's own work stays outside: installers, `git`, secret commands, probes, the worker,
the proxy.

On `flyte` or a command backend, trials and grading run where the backend runs them, and
hlock records that (§7.1).

## 4. Configuration

### 4.1 `harness.toml`

```toml
[sandbox]
runner = "docker"                         # none (default) | docker | a runner name
image = "ghcr.io/acme/agent@sha256:9f2c…"  # docker: required
network = "open"                          # open (default) | none
secrets = "env"                           # env (default) | proxy
applies-to = ["agent", "trials", "graders"]   # default: all three
profile = "sandbox/agent.sb"              # optional; inside the project
grader-image = "ghcr.io/acme/ci@sha256:…" # optional: a toolchain image for graders
```

### 4.2 Turning it off

- `--no-sandbox` on `launch`, `run`, `evals run`, `evals grade`, `resolve` and `tune`
  runs without the lock's sandbox, and prints `sandbox off (--no-sandbox)` on the run line.
- `HLOCK_NO_SANDBOX=1` does the same from the environment, for CI. Inside an hlock
  sandbox hlock sets `HLOCK_IN_SANDBOX=1`, and there `HLOCK_NO_SANDBOX` is ignored, so an
  agent's nested `hlock run` can't turn it off. `HLOCK_IN_SANDBOX` never relaxes a check;
  when it's set outside a sandbox, hlock notes that it's ignoring `HLOCK_NO_SANDBOX`.
- A project policy with `[sandbox] require = true` refuses both.

### 4.3 Command runners

A runner is an argv template hlock puts in front of the command. macOS's `sandbox-exec`:

```toml
# ~/.config/hlock/sandbox.toml (user), or harness.toml [sandbox.runners.<name>] (project)
[runners.seatbelt]
command = ["sandbox-exec", "-f", "{profile}", "-D", "CWD={cwd}", "-D", "CONFIG={config}", "-D", "HOME={home}"]
networks = ["open", "none"]   # what the profile can enforce; default ["open"]
```

or bubblewrap on Linux:

```toml
[runners.bwrap]
command = ["bwrap", "--die-with-parent", "--unshare-all", "{network-args}",
           "--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc", "--symlink", "usr/lib", "/lib",
           "--symlink", "usr/lib64", "/lib64", "--symlink", "usr/bin", "/bin", "--tmpfs", "/tmp",
           "--proc", "/proc", "--dev", "/dev", "{mounts}", "--chdir", "{cwd}", "--"]
mount-args = ["--bind", "{src}", "{path}"]
mount-ro-args = ["--ro-bind", "{src}", "{path}"]
network-args = { none = [], open = ["--share-net"] }
```

- **Placeholders.** `{cwd}`, `{config}` (the config root), `{home}` (the sandbox's
  `HOME`), `{profile}` (hlock's copy of the profile file, §5), `{image}`, `{id}` (a per-run
  id), `{network}`. An argument that's exactly `{mounts}` or `{network-args}` expands to
  several arguments, or none: `{mounts}` repeats `mount-args`/`mount-ro-args` for each
  path in §4.5, with `{src}` (the host path) and `{path}` (the path inside), which differ
  only for a hidden file (hlock's empty file over it). Read-only paths use
  `mount-ro-args`, falling back to `mount-args` when the runner gives none, with a note; a
  template without `{src}` can't hide files, and gets a note too. Values never go into argv.
- **`networks`** (or `network-args`'s keys) are the modes the runner claims to enforce.
  That's the definer's claim; hlock refuses only a mode the runner doesn't list.
- **The environment.** The runner process gets the sandbox environment of §4.6 and
  nothing else. Runners that pass their environment through (bwrap, `sandbox-exec`) hand
  it on as is. hlock never puts a value in argv.
- **Seatbelt doesn't nest.** A harness that applies its own seatbelt profile (Claude Code's
  bash sandbox) fails inside a `sandbox-exec` runner; turn the harness's own sandbox off
  there.
- **Where runners come from.** The user file first, then the project's
  `[sandbox.runners]`. A project can't define `docker` or `none`, or shadow a user-level
  runner. A lock names its runner, and the template never goes into the lock, so a lock
  stays portable. A lock naming a runner this machine doesn't define is an error.

### 4.4 The built-in `docker` runner

```
docker run --rm -i [-t] --name hlock-{id} --init
  --network {none|bridge} --cap-drop=ALL --security-opt no-new-privileges --pids-limit 4096
  --user <uid>:<gid> -w {cwd} --entrypoint <program name> -e HOME={home}
  --mount type=bind,src=P,dst=P[,readonly] …
  -e NAME …  [--add-host host.docker.internal:host-gateway]
  {image} <args…>
```

- **Same-path mounts.** `src` and `dst` are the canonical host path (symlinks resolved;
  `/var` → `/private/var` on macOS), so absolute paths in the restored config mean the
  same thing inside. A path containing `,` or starting with `-` is refused.
- **`--entrypoint`** is the program's bare name. It bypasses an `hlock sync --docker`
  image's own entrypoint, which writes secret values into config files.
- **The user's home and the Docker socket** are never mounted.
- **The image** must contain the harness on a system path (`--user` can't read
  `/root/...`), at the lock's version, plus what its stdio MCP servers run (`npx`/`uvx`
  need `network = "open"`) and what the agent needs to work on the project (compilers,
  test runners, `git`). §4.7 covers building one.
- **The version check.** When the image is present locally and labeled
  `ai.hlock.harness` and `ai.hlock.harness-version` (§4.7), `launch` compares the labels
  with the lock instead of probing the host. The labels are the image builder's claim.
  Without them, or when the image hasn't been pulled yet, `launch` notes that the version
  isn't checked. The harness doesn't need to be installed on the host either way.
- **Plugins** are installed on the host into the sandbox config root, as today, since
  that's the root the container sees (an image's own `/root` isn't readable by
  `--user`). When the harness isn't installed on the host, its plugin installs are skipped
  with a note.
- **Image references** must match Docker's reference grammar. A tag without `@sha256:`
  works, with a note that the image isn't pinned.
- **Timeouts** also run `docker kill hlock-{id}`, since killing the `docker` client leaves
  the container running. For the same reason a sandboxed `launch` doesn't `exec`: hlock
  stays as the parent, waits, and kills the container if it's interrupted.
- **Credentials.** A macOS keychain login can't reach a Linux container. Use an API key
  (`ANTHROPIC_API_KEY`), or for a Claude subscription a long-lived token from
  `claude setup-token` (`CLAUDE_CODE_OAUTH_TOKEN`) with `secrets = "env"`.

Docs note: with `network = "open"` the container can reach the host's services through
`host.docker.internal`, and on cloud CI the metadata endpoint (`169.254.169.254`).

### 4.5 What the sandbox sees

| Kind | Read-write | Read-only |
|---|---|---|
| `agent` (launch, run) | the project; the sandbox config root; `HOME` | control paths, hidden files |
| `agent` (evals run) | the task workspace; the sandbox config root; `HOME` | — |
| `trials` | the trial workspace; `HOME` | the project; hidden files |
| `graders` | the workspace; the grade's scratch directory; `HOME` | the pack directory |

- **A separate config root.** A sandboxed run restores the lock into
  `.hlock/sandbox-env/<harness>/` instead of `.hlock/env/<harness>/`, with `HOME` at
  `.hlock/sandbox-env/<harness>/home/` (kept between runs, so package caches survive). An
  unsandboxed launch never loads anything a sandboxed session wrote, and pinned files are
  restored from the lock on every start, as today.
- **Control paths** decide what hlock runs next on the host, so they're mounted read-only
  over the read-write project: `harness.toml`, `harness*.lock`, `hlock-policy.toml`,
  `.envrc`, `.hlock/` (except `.hlock/sandbox-env/`), and when `.git/` exists,
  `.git/hooks/` and `.git/config`. A control file that doesn't exist can't be mounted
  (Docker would create it on the host), so hlock snapshots the control files before the
  run and, after it, reports any the session created or changed, by name, as a warning to
  review before running hlock or direnv again. (The agent can still read `.git/config`,
  including any token in a remote URL; the docs say so.)
- **Hidden files** are `.env` and `.env.*` anywhere in the project (skipping `.git`,
  `node_modules`, `target`, `.venv`; a project with more than 100,000 entries to search is
  an error, not a partial search), each covered by an empty read-only file. The
  unsandboxed roots in `.hlock/env/` (filled-in secrets, logins) and `.hlock/docker/` are
  covered by an empty directory. The sandbox writes only its own config root and `HOME`;
  the rest of `.hlock/sandbox-env/` (profiles, graders' and trials' homes) is read-only to it.
  Variables that steer the runner (`DOCKER_*`, `LD_*`, `DYLD_*`, `PATH`) never come from the
  lock or the project.

These guarantees hold for `docker`, and for a command runner whose template uses
`{mounts}`. A template that ignores `{mounts}` (like the seatbelt one, whose profile
decides) gets what its profile allows. That's why runners and profiles need trust (§5).

`--in-place` (your own config) can't be sandboxed: hlock refuses the combination.

### 4.6 The environment

Each spawn point passes the whole host environment today (`{**os.environ, …}`).
Sandboxed, the run gets only:

- what the harness needs: `LaunchPrep.env` (config-root variables, paths rewritten to the
  sandbox root), the lock's secret values (`secrets = "env"`), the proxy's variables
  (`proxy`), and the provider credentials the harness profile names
  (`ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, …, under `env` only);
- `HOME`, `HLOCK_IN_SANDBOX=1`, and `TERM`, `COLORTERM`, `LANG`, `LC_*`, `TZ` when set;
- for the runner process itself (outside the sandbox): `PATH`, `USER`, `LOGNAME`, `TMPDIR`,
  `XDG_RUNTIME_DIR`, `DOCKER_*` (docker gets these, and passes only `-e NAME` for the
  rest).

Names must match `^[A-Za-z_][A-Za-z0-9_]*$`, or the run stops.

### 4.7 Building an image

`hlock sync --docker` gains:

- `--base IMAGE`: build on your own image (a devcontainer, your CI image) instead of
  `node:22-bookworm`. It must be Debian-based with `apt-get`. Installs that need Node or
  Python add them when they're missing.
- Binaries an installer put under `/root/.local/bin` are copied to `/usr/local/bin`, so
  `--user` can run them.
- `LABEL ai.hlock.harness=<id>` and, when the lock pins a version,
  `ai.hlock.harness-version=<version>`. When the harness install is left out (untrusted, or
  no scriptable Linux install), the labels are too.
- The image is tagged `hlock-<harness>:<first 12 of the lock's hash>` (the name `sync --docker`
  already used, with the lock's hash as its tag), and `sync --docker` prints the
  `[sandbox]` lines to paste, with a reminder that a local tag isn't a digest: push it to a
  registry to share it, and pin `@sha256:`.

## 5. Checks and trust

Before anything starts, hlock checks and stops with a fix:

| Condition | Error |
|---|---|
| `docker` without `image`, or a bad image reference | name a valid image |
| The runner isn't defined here, or its program isn't on `PATH` | define or install it, or `--no-sandbox` |
| `network` isn't one the runner lists | the runner can't enforce it |
| `profile` is missing, outside the project, or its digest differs from the lock's | it changed since the lock |
| The template uses `{profile}` and none is set | set `profile` |
| `secrets = "proxy"` with a runner other than `docker` | phase 1 proxies for docker only |
| `secrets = "proxy"` with `network = "none"` | the agent couldn't reach the proxy |
| `secrets = "proxy"` and no API key to proxy (only a subscription token) | use an API key, or `secrets = "env"` |
| `--in-place`, `meta run`, or a translated launch with a sandbox | §2 |
| `harness.toml` has a `[sandbox]` the lock doesn't match | re-pin (or re-resolve) |

After the profile's check, hlock copies it into a directory it owns and points
`{profile}` at the copy, so it can't change between the check and the use.

**Trust.** The sandbox definition is something hlock executes, or hands the agent's
environment to, so it's a trust kind: `sandbox`, one entry per lock, with the digest of
the runner's template (for a project-level runner), the image and grader image
references, and the profile's digest. `hlock trust` lists it. `launch` asks once, like it
does for MCP servers, and asks again when any of those changes. A user-level runner with
the built-in `docker` and images the user typed into their own `~/.config` doesn't need
approval; anything from the project does.

**Warnings**, only on commands that run the kind they're about:

- `evals run`, `resolve`, `tune`: when `applies-to` leaves out `graders`, "graders run code
  the agent wrote, on this machine".
- `resolve`, `tune`, code-harness `run`/`evals run`: under `proxy`, "code-harness trials
  get keys by environment".
- `launch`, `run`, `evals run` under `proxy`: one line naming the lock secrets that still
  go by environment ("2 MCP secrets reach the sandbox by environment").

### 5.1 Project policy

`hlock-policy.toml` gets `[sandbox] require = true`: a lock without a sandbox (or a run
with `--no-sandbox`/`HLOCK_NO_SANDBOX`) violates the policy. It's enforced like the rest
(`enforcement = block | warn | off`) at `pin`, `launch`, `run`, `evals run`, `evals grade`,
`resolve` and `tune`. `PolicyFile` denies unknown fields, so an older hlock refuses a policy with
`[sandbox]` (fails closed). An older hlock reading a new *lock* ignores
`[policy.sandbox]` (a free-form map) and runs unsandboxed. The docs say so; teams that
rely on the sandbox set `require`.

## 6. Secrets: the provider-key proxy (`docker`)

With `secrets = "proxy"`, hlock starts a credential proxy for each sandboxed agent run.

| Route | Upstream | Key header | Paths allowed |
|---|---|---|---|
| `/anthropic` | `https://api.anthropic.com` | `x-api-key` | `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models` |
| `/openai` | `https://api.openai.com` | `Authorization: Bearer` | `/v1/chat/completions`, `/v1/responses`, `/v1/models`, `/v1/embeddings` |

A route exists when hlock finds its key (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) in the
environment. With neither set, the run stops: a subscription login can't be proxied.

**The sandbox gets** `ANTHROPIC_BASE_URL=http://host.docker.internal:<port>/anthropic` and
`ANTHROPIC_API_KEY=<session token>` (and the same for OpenAI). Every other provider
credential is left out and listed in the notes: `ANTHROPIC_AUTH_TOKEN`,
`CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_*`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`,
`OPENROUTER_API_KEY`, `AWS_*`, `GOOGLE_APPLICATION_CREDENTIALS`. A lock secret whose value
equals a proxied key is left out too.

**How it forwards.**

- The upstream is fixed per route. The `Host` header and absolute-URI targets are
  ignored, and `CONNECT` is refused.
- Paths with `..`, `//` or an encoded `/`, or outside the route's list, are refused.
- Incoming `x-api-key`, `authorization`, and any header ending `-key` or `-token` are
  dropped, as are hop-by-hop headers. The session token is checked in constant time, then
  the real key is added and the request goes out over HTTPS with certificate checks.
- Redirects aren't followed. Responses stream through as they arrive (SSE works).
- The log has method, route, path and status, and never headers or bodies. It's in a
  private directory hlock makes for the session, outside anything a sandbox mounts, and is
  opened without following links.
- The token is 32 random bytes, hex-encoded.

**Where it listens.** On macOS and Windows, `127.0.0.1`, which Docker Desktop's
`host.docker.internal` reaches. On Linux, the `bridge` network's gateway address (from
`docker network inspect bridge`), which every container on that bridge can reach; the
token is what stops them. If hlock can't bind that address, it stops. It never falls back
to `0.0.0.0`.

**Its lifetime.** The proxy runs in hlock's own process, which stays as the parent of the
sandboxed run (§4.4) and stops the proxy when the run ends. The container's processes end
with the container (`--rm --init`), and an interrupted run gets `docker kill`. The keys
never reach the sandbox's argv or environment, and the hlock process isn't visible from
inside the container.

**Claude Code** asks whether to trust a custom API key in an interactive session. Under
`proxy`, `launch` records the session token as approved in the sandbox root's
`.claude.json` (`customApiKeyResponses.approved`, the last 20 characters) so it doesn't ask
every time. The file is `0600`, and the sandbox root isn't checked for drift.

**What this protects.** The agent can't read the key, print it, or send it anywhere. It
*can* use it through the proxy while the session lasts, and with open network it could
relay that use outward for the session's length. The allowed paths include ones that take
file and vector-store references, so the docs recommend a dedicated low-scope key or
workspace. A harness that ignores the base-URL variable sends the token to the real
provider, which rejects it: loud, and nothing leaks.

## 7. The lock

`[policy] sandbox`, in schema v1 (a free-form map), records the sandbox:

```toml
[policy.sandbox]
runner = "docker"
image = "ghcr.io/acme/agent@sha256:9f2c…"
network = "open"
secrets = "env"
applies-to = ["agent", "trials", "graders"]
profile = { path = "sandbox/agent.sb", digest = "sha256:…" }
grader-image = "ghcr.io/acme/ci@sha256:…"
```

- **Writers.** `pin`, `resolve` and `tune` (which rebuilds `[policy]`; it carries this
  over) copy `harness.toml`'s `[sandbox]` in, with the profile's digest. Since it comes
  from the project's own `harness.toml`, each also approves the `sandbox` executable on
  this machine, as `pin` approves what it captures.
- **Readers.** `launch`, `run`, `evals run` and `evals grade` read the **lock**, so a
  pulled lock brings its sandbox. `resolve` and `tune` read the manifest.
- **Drift.** For a pinned lock, `hlock check`'s re-capture reports the change like any
  other (`~ sandbox: …`, with `weakened`). For a resolved lock, `hlock check` compares `harness.toml`'s `[sandbox]` with the lock's directly.
  The manifest hash isn't the signal: a pinned lock's hash covers only dependencies. On a
  resolved lock, editing `[sandbox]` changes the manifest hash, so `check` asks for a
  re-resolve, which is right, since the evidence was measured under the old sandbox.
  `check` also reports profile-digest drift.
- **`hlock diff`** prints sandbox changes on their own lines. A change that weakens the
  sandbox is marked `weakened`: runner to `none`, network to `open`, a kind dropped from
  `applies-to`, `secrets` to `env`, an image going from digest to tag, the profile
  removed or changed.

### 7.1 Evidence

`[evidence] sandbox` (new, optional) records what actually ran sandboxed during the
resolve or tune, with `--no-sandbox` applied:

```toml
[evidence.sandbox]
runner = "docker"
network = "open"
applies-to = ["trials", "graders"]   # kinds that ran, and ran sandboxed
note = ""                            # "agent-CLI trials are simulated" | "backend flyte"
```

- Written by the resolver (`resolve`) and by tune's lock writer.
- `resolve` and `tune` always write it; an unsandboxed run records `runner = "none"`.
  **Absent** means the lock predates sandboxes, and nothing compares.
- For a code-harness lock, when `[policy.sandbox]` covers `trials` and the evidence didn't
  run them sandboxed, or ran them under another runner or network, `launch`, `run` and
  `hlock check` note it once: "the evidence was measured without this sandbox". For an
  agent-CLI lock there's nothing to compare: its trials were simulated.

## 8. CLI surface

- `hlock sandbox`: the lock's sandbox, the runner and whether it's available and trusted,
  the image's labels, the lock secrets that go by environment, warnings, and the wrapped
  argv `launch` would run. `--json`.
- The run line gains the sandbox:
  `▶ Claude Code in docker (network open, secrets proxy)  …`.
- `launch --dry-run --json` adds a `sandbox` object.
- `--no-sandbox` on the six commands. `hlock trust` lists the `sandbox` kind.
- `hlock sync --docker --base IMAGE`, and `--build` printing the `[sandbox]` lines.

## 9. Implementation

- **Rust, `sandbox.rs`.** The types (manifest, lock, evidence), parsing and validation,
  runner lookup (user file, then project), the trust executable (like
  `trust::delegation`, since the runner isn't in the lock), the checks, the mount list
  (control paths, placeholders, hidden files, canonical paths), the profile copy, and
  `wrap(…) -> Wrapped { argv, env, run_id, kill }`. Every spawn point uses it.
- **Spawn points.** `launch::prepare` takes the sandbox (and `--no-sandbox`) in
  `LaunchOptions`, uses the sandbox root, skips the host probe under docker, and returns
  `sandbox` in `LaunchPrep`. `contract::run` gets the sandbox as a new parameter, and
  `graders::tests` through a new `GradeOptions.sandbox` that `grade` passes down; both wrap
  and kill on timeout.
- **Python, `hlock/sandbox.py`.** The proxy (stdlib `http.server`/`http.client`, streaming,
  in a thread of the waiting hlock process), the control-file snapshot, and image-label reading (`docker image inspect`). The CLI runs the wrapped
  argv with the sandbox environment in place of `{**os.environ, …}`.
- **Manifest, policy, lock, diff.** `sandbox` in `TOP_KEYS` with its own key report;
  `PolicyFile.sandbox`; typed reads of `policy.sandbox`; `Evidence.sandbox`; sandbox lines
  in `diff_locks` and `lockdiff`.
- **Docker image.** `LABEL`s and `--base` in `sync::dockerfile`; the tag and paste lines
  in the CLI.

## 10. Testing

- Rust: template expansion (multi-argument placeholders, read-only fallback, missing
  `{profile}`), the docker argv (mounts, read-only, placeholders, hidden files,
  `--entrypoint`, `--name`), every check, name validation, canonical paths, runner
  precedence, the trust digest changing with each input, lock round trip, diff marking
  `weakened`, evidence comparison.
- Python: a fake runner script that records its argv and its environment's names, then
  runs the rest. `launch --dry-run`, `run`, `evals run`, a `tests` grader and a code-harness
  trial go through it. Assert that the sandbox environment holds no host variable outside
  §4.6.
- The proxy, against a local fake upstream (an `http` upstream injectable for tests only):
  token swap, refusal without the token, `..`, absolute-URI targets, `CONNECT`, an unlisted
  path, credential headers stripped, streaming, stopped when the run ends.
- `sandbox-exec` on macOS CI runs a real command-runner test. Docker tests run only when
  the daemon is up.

## 11. Order of work

Each step ships on its own:

1. **Record.** Types, the manifest key, `pin`/`resolve`/`tune` writing `[policy.sandbox]`,
   round trip, `check` drift, `diff` with `weakened`, `--no-sandbox`, `hlock sandbox`
   (showing, not running).
2. **Docker for `agent`** with `secrets = "env"`: the sandbox root, mounts, the
   environment, labels, `sync --docker --base` and labels, trust. This answers "does
   hlock work with a sandbox?"
3. **Graders, then trials**, through `wrap` in Rust, with timeout kill. `evidence.sandbox`.
4. **Command runners**, with the `sandbox-exec` test.
5. **Policy `require`.**
6. **The proxy** for docker.

Phase 2: per-host allowlists, the proxy for command runners and code-harness trials,
per-MCP-server token proxying, sandboxed `meta` jobs, tested `sbx` and `openshell`
presets, and remembering per machine that a project ran sandboxed, so dropping the sandbox
asks first.
