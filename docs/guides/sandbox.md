# Sandboxes

A lock pins *which* agent setup runs. A sandbox decides *how* it runs: with what files,
what network, and which credentials. Without one, `hlock launch`, `hlock run`, `hlock evals
run`, a code harness's trials and every `tests` grader run with your permissions, on your
files, with your network. The [isolated config root](launch.md) keeps your *config* out of
a session; a sandbox keeps the *agent* in.

hlock doesn't build sandboxes. It runs the agent through a runtime you already have:
Docker, built in, or any command that wraps another command, like macOS's `sandbox-exec` or
Linux's bubblewrap. The lock records which sandbox it runs in, so a teammate who pulls the
lock gets the same one.

## Run the lock in Docker

Name the runner and an image in `harness.toml`, then re-pin (or re-resolve):

```toml title="harness.toml"
[sandbox]
runner = "docker"
image = "ghcr.io/acme/agent@sha256:4f1c9e0b7d2a"
network = "open"
```

```console
$ hlock pin
$ hlock sandbox
runner      docker  installed
image       ghcr.io/acme/agent@sha256:4f1c9e0b7d2a
network     open
secrets     env
applies to  agent, trials, graders
trust       trusted
next → hlock launch --dry-run
```

The lock now has a `[policy.sandbox]` table, and `launch`, `run` and `evals run` use it:

```console
$ hlock launch --dry-run
▶ Claude Code in docker (network open, secrets env)  CLAUDE_CONFIG_DIR=.hlock/sandbox-env/claude-code/config claude
  runs docker run --rm -i --name=hlock-58fc8e3c97b6 --init --network=bridge --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=4096 --user=502:20 --workdir=… --entrypoint=claude --mount … ghcr.io/acme/agent@sha256:4f1c9e0b7d2a
```

(The `--mount` arguments are cut here; there's one per path in
[what the sandbox sees](#what-the-sandbox-sees).)

The container runs as you (`--user`), with no capabilities, and sees each path at the same
place it has on your machine, so the paths in the restored config still work. Your home
directory and the Docker socket are never mounted. A sandboxed `launch` keeps hlock running
as the parent: it stops the container if you interrupt it, and checks what the session
changed when it ends.

### An image with the harness in it

The image runs the harness, so it needs the harness, at the lock's version, and whatever
your MCP servers and your project need (`git`, compilers, test runners). The harness
doesn't have to be installed on your machine. `hlock sync --docker` writes a Dockerfile for
the lock:

```console
$ hlock sync --docker
Sync plan for Claude Code 2.1.308 → .hlock/docker/claude-code
✓  config  Check .claude/settings.json
✓ wrote .hlock/docker/claude-code/Dockerfile
next →
  docker build -t hlock-claude-code:fe8fe2dd18e4 .hlock/docker/claude-code
  docker run -it --rm -v "$PWD:/workspace" hlock-claude-code:fe8fe2dd18e4
  or run the lock in it as a sandbox: add to harness.toml, then re-pin
    [sandbox]
    runner = "docker"
    image = "hlock-claude-code:fe8fe2dd18e4"  # push it, and pin @sha256:, to share it
```

`--base IMAGE` builds on your own image (a devcontainer, your CI image) instead of
`node:22-bookworm`; it must be Debian-based. `--build` runs `docker build` too. The image is
labeled with the harness and its version (`ai.hlock.harness`, `ai.hlock.harness-version`),
and `launch` checks the labels against the lock before it starts. A local tag isn't a
digest: push the image to a registry to share it, and pin `@sha256:` in `image`.

### Credentials in a container

A macOS keychain login can't reach a Linux container. Use an API key
(`ANTHROPIC_API_KEY`), or for a Claude subscription a long-lived token from
`claude setup-token` (`CLAUDE_CODE_OAUTH_TOKEN`), exported where you run hlock.

With `network = "open"`, the container can reach services on your machine through
`host.docker.internal` and, on a cloud CI runner, its metadata endpoint. Use
`network = "none"` where that matters (the agent then can't reach a model provider either,
unless it's local).

## Keep provider keys out of the agent's reach

By default (`secrets = "env"`), the sandbox gets your provider keys as environment
variables, and anything the agent runs can print them. With `secrets = "proxy"` (Docker
only), it gets a session token instead:

```toml title="harness.toml"
[sandbox]
runner = "docker"
image = "ghcr.io/acme/agent@sha256:4f1c9e0b7d2a"
secrets = "proxy"
```

hlock starts a small proxy for the session. The agent's `ANTHROPIC_BASE_URL` (and
`OPENAI_BASE_URL`) point at it, and its `ANTHROPIC_API_KEY` is the session token. The proxy
swaps the token for your real key and forwards to the provider, streaming the response. It
only forwards to the provider's own URL, only on the model endpoints (`/v1/messages`,
`/v1/chat/completions`, `/v1/responses`, `/v1/models`, …), and stops when the session ends.
Every other provider credential (`CLAUDE_CODE_OAUTH_TOKEN`, `AWS_*`, …) is left out.

The agent can *use* the key while the session runs, but it can't read it, print it, or send
it anywhere. A harness that ignores the base URL sends the token to the provider, which
rejects it. The key must be an API key in your environment; a subscription login can't be
proxied. Lock secrets (MCP servers' tokens) still reach the sandbox as environment
variables, and `launch` names them.

## Define your own runner

A runner is a command hlock puts in front of the agent's. Define one in
`~/.config/hlock/sandbox.toml`. Here is macOS's `sandbox-exec`, with a profile that lets the
agent write only in the project, its `HOME` and temporary directories:

```toml title="~/.config/hlock/sandbox.toml"
[runners.seatbelt]
command = ["sandbox-exec", "-f", "{profile}", "-D", "CWD={cwd}", "-D", "HOME={home}"]
networks = ["open", "none"]
```

```scheme title="sandbox/agent.sb"
(version 1)
(allow default)
; writes only in the project, the sandbox's HOME, and temporary directories
(deny file-write*)
(allow file-write*
  (subpath (param "CWD"))
  (subpath (param "HOME"))
  (subpath "/private/tmp")
  (subpath "/private/var/folders")
  (literal "/dev/null"))
```

```toml title="harness.toml"
[sandbox]
runner = "seatbelt"
profile = "sandbox/agent.sb"
```

```console
$ hlock pin
$ hlock sandbox
runner      seatbelt  user  installed
network     open
secrets     env
applies to  agent, trials, graders
profile     sandbox/agent.sb  sha256:616281cd…
trust       trusted
next → hlock launch --dry-run
$ hlock launch --dry-run
▶ Claude Code in seatbelt (network open, secrets env)  CLAUDE_CONFIG_DIR=.hlock/sandbox-env/claude-code/config /opt/homebrew/bin/claude
  runs sandbox-exec -f ~/my-project/.hlock/sandbox-env/profiles/616281cda75e-agent.sb -D CWD=~/my-project -D HOME=~/my-project/.hlock/sandbox-env/claude-code/home /opt/homebrew/bin/claude
```

The profile is pinned by digest. A run whose profile changed since the lock stops, and
hlock runs a copy of it that it owns, so it can't change between the check and the run.
Seatbelt profiles don't nest: turn off a harness's own seatbelt sandbox (Claude Code's
bash sandbox) under this runner.

A runner that bind-mounts paths (bubblewrap, firejail) gets hlock's list of what the
sandbox should see through `{mounts}`:

```toml title="~/.config/hlock/sandbox.toml"
[runners.bwrap]
command = ["bwrap", "--die-with-parent", "--unshare-all", "{network-args}",
           "--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc", "--symlink", "usr/lib", "/lib",
           "--symlink", "usr/lib64", "/lib64", "--symlink", "usr/bin", "/bin", "--tmpfs", "/tmp",
           "--proc", "/proc", "--dev", "/dev", "{mounts}", "--chdir", "{cwd}", "--"]
mount-args = ["--bind", "{src}", "{path}"]
mount-ro-args = ["--ro-bind", "{src}", "{path}"]
network-args = { none = [], open = ["--share-net"] }
```

| Placeholder | Becomes |
|---|---|
| `{cwd}` | Where the command runs |
| `{home}` | The sandbox's `HOME` |
| `{config}` | The harness's config root (the `HOME` when there's none) |
| `{profile}` | hlock's copy of `[sandbox] profile` |
| `{image}`, `{id}`, `{network}` | `[sandbox] image`, a per-run id, the network mode |
| `{mounts}` | For each path: `mount-args`, or `mount-ro-args` for a read-only one, with `{src}` (the path on your machine) and `{path}` (the path inside) |
| `{network-args}` | `network-args` for the network mode |

`networks` (or the keys of `network-args`) lists the network modes the runner enforces.
That's your claim; hlock only refuses a mode the runner doesn't list. The runner process
gets the sandbox's environment, plus what it needs to start from yours (`PATH`, `USER`,
`TMPDIR`, `DOCKER_*`), and hlock never puts a value in its arguments.

A project can define runners too, in `harness.toml`'s `[sandbox.runners.<name>]`. It can't
redefine `docker` or one of yours, and since hlock runs it, it's
[approved](audit.md#executables) like the lock's other executables.

## What the sandbox sees

| Run | Read-write | Read-only |
|---|---|---|
| `launch`, `run` | the project; the session's config root and `HOME` | the control files below |
| `evals run` | the task's workspace; the session's config root and `HOME` | |
| a code harness's trials | the trial's workspace; `HOME` | the project |
| `tests` graders | the workspace; the grade's scratch directory; `HOME` | the project; the pack |

Wherever the project is mounted, its `.env` files and the unsandboxed config roots in
`.hlock/env/` (which hold filled-in secrets and logins) are covered, and so is
`.hlock/docker/`.

- **Its own config root.** A sandboxed session restores the lock into
  `.hlock/sandbox-env/<harness>/`, not `.hlock/env/`, so nothing it writes there reaches an
  unsandboxed launch. Its `HOME` is `.hlock/sandbox-env/<harness>/home/`, kept between
  runs.
- **Control files** decide what runs next on your machine, so they're read-only:
  `harness.toml`, `harness*.lock`, `hlock-policy.toml`, `.envrc`, `.hlock/`, and
  `.git/hooks` and `.git/config`. One that doesn't exist yet can't be mounted, so hlock
  checks after the session and warns about any it created or changed:
  `! the sandboxed session created .envrc: review it before you run hlock (or direnv) again`.
  A control file that's a link is protected where it points; replacing the link itself is
  reported the same way. The agent can still read `.git/config`, including any token in a
  remote URL.
- **`.env` and `.env.*` files** anywhere in the project are covered by an empty file. A
  project with more than 100,000 files to search (outside `.git`, `node_modules`, `target`
  and virtualenvs) can't be sandboxed this way, and says so.
- **The environment** is only what the harness needs: its config-root variables, the lock's
  secrets, the provider credentials it reads (or the proxy's), `TERM`, `LANG`, `TZ` and
  `HOME`. Not your `PATH`, cloud credentials or anything else. Variables that would steer the
  runner itself (`DOCKER_*`, `LD_*`, `DYLD_*`) are left out, even when the lock sets them.

`--in-place` (your own config) can't be sandboxed, and neither can `hlock meta run` or a
launch that translates the lock into another harness yet; run those with `--no-sandbox`.

## Choose what runs inside

`applies-to` lists what runs sandboxed: `agent` (an agent CLI's `launch`, `run` and
`evals run`), `trials` (a code harness: its trials in `resolve` and `tune`, and its
`launch`, `run` and `evals run`) and `graders` (`tests` graders). All three by default. `grader-image` gives graders their own
image, with your toolchain:

```toml title="harness.toml"
[sandbox]
runner = "docker"
image = "ghcr.io/acme/agent@sha256:4f1c9e0b7d2a"
grader-image = "ghcr.io/acme/ci@sha256:9d03…"
applies-to = ["agent", "graders"]
```

`resolve` and `tune` read `[sandbox]` from `harness.toml` and run code-harness trials and
graders in it. An agent CLI's trials are simulated, so nothing of it runs there. The lock's
evidence records what ran sandboxed:

```toml title="harness.lock"
[evidence.sandbox]
runner = "docker"
network = "open"
applies-to = ["trials", "graders"]
```

When a code-harness lock runs in a sandbox its evidence wasn't measured in, `hlock check`
and `hlock sandbox` say so.

## Turn it off

`--no-sandbox` on `launch`, `run`, `evals run`, `evals grade`, `resolve` and `tune` runs
without it, and says so. `HLOCK_NO_SANDBOX=1` does the same from the environment. Inside an
hlock sandbox, `HLOCK_NO_SANDBOX` is ignored, so an agent's own `hlock run` can't turn it off.

A team that relies on the sandbox can require it in [`hlock-policy.toml`](../reference/policy.md#sandbox):

```toml title="hlock-policy.toml"
[sandbox]
require = true
```

```console
$ hlock run --no-sandbox "hi"
✗ error: harness.lock breaks the project's policy:
  the policy requires a sandbox, and the sandbox is turned off (--no-sandbox or HLOCK_NO_SANDBOX) (hlock-policy.toml)
change the lock, or the policy (enforcement = "warn" to only warn)
```

An older hlock ignores a lock's `[policy.sandbox]` and runs without it, but refuses a policy
with a `[sandbox]` table, so `require` keeps working.

## Review a change to the sandbox

`hlock check` fails when `harness.toml`'s `[sandbox]` (or its profile file) no longer
matches the lock, and `hlock diff` shows the change. A change that protects less is marked
`weakened`:

```console
$ hlock check
✗ drift: your claude-code setup changed since harness.lock was pinned
  ~ sandbox: docker (network none, secrets env) ghcr.io/acme/agent@sha256:4f1c9e0b7d2a → docker (network open, secrets env) ghcr.io/acme/agent@sha256:4f1c9e0b7d2a  weakened: network none → open
  hint: re-pin with hlock pin, or restore it with hlock sync --in-place
```

The sandbox is approved like an executable: a lock whose runner template (when the project
defines it), image or profile changed asks again before it runs. `hlock trust` lists it as
kind `sandbox`. What you pin or resolve from your own `harness.toml` is approved on your
machine.

## What a sandbox doesn't do

- hlock doesn't check a runtime's claims. A command runner's network modes, and what its
  profile allows, are what you wrote.
- Network is `open` or `none`. Per-host allowlists belong in a runtime that has them
  (OpenShell's policy, a seatbelt or firewall profile, passed as `profile`).
- The proxy covers model-provider keys for `docker`. A code harness's trials get keys by
  environment, and so do MCP servers' tokens.
- Docker's `sbx` and NVIDIA OpenShell run agents in microVMs with their own images and
  credential handling. You can wire them as command runners, but hlock doesn't ship tested
  presets for them yet.
