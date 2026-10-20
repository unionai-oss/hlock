---
name: union-run-locally
description: 'Set up the flyte CLI against a Union.ai organization and run a first workflow on the local machine as a tracked run, so it appears under Tracked Runs in the Union.ai UI without any cluster. Use for Union.ai onboarding, "run my first workflow", "flyte create config", "--tracked", "tracked run", or verifying an org endpoint and login before provisioning AWS.'
---

# Run your first workflow locally (tracked)

A **tracked run** executes on the user's own machine and reports its progress to Union.ai as
it goes. No cluster, no AWS spend, about two minutes. It is the cheapest possible proof that
the organization, the endpoint and the login all work — so run it *before*
`union-provision-aws`, and a later failure is then unambiguously about the cluster rather
than about the account.

Docs: <https://www.union.ai/docs/v2/union/deployment/self-serve/run-locally/>

## What you need first

- A Union.ai organization (see `union-self-serve` steps 1–2).
- Its endpoint, `<your-org>.hosted.unionai.cloud` — the hostname in the browser's address
  bar when signed in to the UI. **Bare hostname, no `https://`, no trailing slash.**
- Python 3.10 or newer, and a virtual environment. Do not `pip install` into the system
  Python; on a mac with Homebrew Python that fails outright with
  `externally-managed-environment`.
- A browser on the same machine. The first command that contacts the organization opens a
  browser window for sign-in.

Nothing here costs money and nothing here touches AWS. **No approval gates in this skill** —
everything is local, reversible, and confined to the working directory and `~/.flyte`.

## Step 1 — Install the SDK

The `flyte` package provides both the SDK and the `flyte` command. Union.ai uses the same
SDK and CLI as open-source Flyte; there is no separate `union` CLI in v2.

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install --upgrade flyte
flyte --version
```

`uv` works too and is faster — `uv venv && uv pip install flyte` — but then every later
command needs `uv run flyte ...` or an activated venv. Pick one and stay with it, because
a half-activated venv is the most common cause of "command not found: flyte" three steps
later.

Check the version is 2.x. If `flyte --version` reports a 1.x version or the command is
missing entirely while `pyflyte` exists, the environment has Flyte 1 (`flytekit`) installed.
That is the deprecated v1 SDK and none of this applies to it; create a clean venv.

## Step 2 — Write the config

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}" 2>/dev/null || true
: "${UNION_ENDPOINT:?set UNION_ENDPOINT to <your-org>.hosted.unionai.cloud}"

flyte create config \
    --endpoint "${UNION_ENDPOINT}" \
    --domain development \
    --project default \
    --builder remote
```

This writes `.flyte/config.yaml` **in the current directory**, not in `$HOME`. That matters
more than it looks:

- `flyte run` picks up the config from the current directory (or an ancestor). Run it from
  somewhere else and you get a confusing auth or endpoint error rather than a missing-config
  error.
- So create the config in the directory the workflow will live in, and stay there.
- Existing config? `flyte create config` will overwrite it. Check first if the directory is
  an existing project:

```bash
# 🟢 READ-ONLY
cat .flyte/config.yaml 2>/dev/null || echo "no config in $PWD"
```

The flags:

| Flag | Why this value |
|---|---|
| `--endpoint` | The organization hostname. Bare — `flyte` adds the scheme. |
| `--domain development` | Union.ai gives every project three domains: `development`, `staging`, `production`. Start in `development`. |
| `--project default` | The project every new organization ships with. |
| `--builder remote` | Container images are built by Union.ai's remote builder, so no local Docker daemon is needed. Irrelevant for a tracked run — which does not build an image at all — but it is the right default for the very next thing the user does. |

## Step 3 — Sign in

The first command that contacts the organization opens a browser for sign-in and caches the
credential; after that the CLI remembers you. Trigger it deliberately rather than letting it
surprise you in the middle of a run:

```bash
# 🟢 READ-ONLY — opens a browser on first use
flyte get project
```

This should list the `default` project. If the agent is driving a shell the human cannot
see, **say out loud that a browser window is about to open and that they need to complete
the sign-in** — otherwise the command just appears to hang.

Headless machine (a VM, a container, SSH with no display)? A device-code or manual flow may
not be available in every build. The reliable answer is to run this step on a laptop that
has a browser; the tracked run has to execute somewhere anyway, and a laptop is fine.

## Step 4 — Write and run a workflow

`hello.py` — this is the example from the docs, and the fan-out is deliberate: it produces
one parent action and five children, so the run page has something to show.

```python
import flyte

env = flyte.TaskEnvironment(name="hello_env")


@env.task
def fn(x: int) -> int:
    slope, intercept = 2, 5
    return slope * x + intercept


@env.task
def main(x_list: list[int] = [1, 2, 3, 4, 5]) -> float:
    y_list = list(flyte.map(fn, x_list))
    return sum(y_list) / len(y_list)
```

A Flyte 2 task is `@env.task` on a `flyte.TaskEnvironment`. There is no `@workflow`
decorator — `main` is just a task that calls other tasks. (Coming from Flyte 1? The
`flyte-migrate` skills cover the translation.)

Run it:

```bash
flyte run --tracked hello.py main
```

`--tracked` is the whole point: the workflow executes **on this machine** and streams its
progress to Union.ai. Drop the flag and `flyte` tries to run it on a cluster, which at this
stage does not exist yet — that failure is expected and is not a bug.

The command prints a URL. That is the run's page in the UI.

## Step 5 — Find it in the UI

Open the printed URL. Or navigate: organization home → **Projects** → **default** →
**Tracked Runs** → **main**.

**Tracked Runs is a separate section from Runs.** Runs that executed on a cluster appear
under **Runs**; runs that executed on the user's machine appear under **Tracked Runs**.
Looking in the wrong one and concluding nothing was recorded is the single most common
confusion at this step — so name the section explicitly when telling the user where to look.

The run page shows the parent action with its five `fn` children and their status and
timing, the `hello_env` environment, and the run's inputs and outputs under **Summary**.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `command not found: flyte` | The venv is not active in *this* shell. `source .venv/bin/activate`. Each agent Bash call is a fresh shell, so activate inside the same command as the `flyte` invocation. |
| Config not found / auth error on `flyte run` | Running from a different directory than the one holding `.flyte/config.yaml`. `cd` back, or re-run `flyte create config` here. |
| Browser never opens, command hangs | Headless host. Run this on a machine with a browser. |
| `externally-managed-environment` from pip | System Python. Create a venv. |
| The endpoint is rejected | A scheme or trailing slash crept into `--endpoint`. It wants `myorg.hosted.unionai.cloud` and nothing else. |
| Run succeeds locally, nothing appears in the UI | `--tracked` was omitted, or the UI is open on **Runs** instead of **Tracked Runs**, or on a different project/domain than `default`/`development`. |
| `ModuleNotFoundError` inside a task | A tracked run executes in *this* interpreter, so the task's imports must be installed in the active venv. Unlike a cluster run, there is no image to rebuild — just `pip install` it. |

Tracked runs report status, timing and I/O; they are not a full cluster execution. See
[Track local runs in the console](https://www.union.ai/docs/v2/union/user-guide/get-started/run-modes/running-locally.md#track-local-runs-in-the-console)
for exactly what is and is not reported.

## Next

- **Provision AWS** → `union-provision-aws`.
- **Already have AWS resources** → `union-connect-cluster`.
- After the cluster is connected, run the *same file* without `--tracked`:
  `flyte run hello.py main`. It then executes on the cluster and appears under **Runs**.
  That contrast is the clearest demonstration of what connecting a cluster bought.
