---
title: hlock
description: Your agent harness made portable. Pin your harness, model, MCP servers, skills, and config in a harness.lock, version it, and rehydrate it anywhere.
---

<p align="center">
  <img src="assets/logo-animated.svg" alt="hlock logo" width="120">
</p>

<h1 class="hl-tagline">Your agent harness<br><span class="hl-grad">made portable.</span></h1>

Your agent is more than a model. It's a harness, MCP servers, skills, and config,
hand-tuned on one machine. `hlock` is the missing harness packaging layer:
`harness.lock` tracks all of its dependencies, so you can commit it to git and restore
it anywhere.
{ .hl-lead }

<div class="hl-actions" markdown>

[Get started :lucide-arrow-right:](getting-started/quickstart.md){ .md-button .hl-btn .hl-btn--primary }
[:fontawesome-brands-github: GitHub](https://github.com/unionai-oss/hlock){ .md-button .hl-btn .hl-btn--ghost }

</div>

<div class="hl-install" markdown>

=== "pip"

    ```bash
    pip install hlock
    ```

=== "uv"

    ```bash
    uv add hlock            # add to a project
    uv tool install hlock   # or install the CLI globally
    ```

=== "uvx (no install)"

    ```bash
    uvx hlock --help
    ```

=== "pipx"

    ```bash
    pipx install hlock
    ```

</div>

<div class="hl-demo">
  <div class="hl-term" id="hl-term" role="img" aria-label="An hlock session: pin a Claude Code setup into harness.lock and push it to git, then pull and sync it on another machine">
    <div class="hl-term__bar">
      <span class="hl-term__dots"><i></i><i></i><i></i></span>
      <span class="hl-term__title">~/projects/my-app</span>
    </div>
    <pre class="hl-term__body" aria-hidden="true"></pre>
    <div class="hl-term__caption" aria-hidden="true"></div>
  </div>
</div>

## Lock the harness you already have

If you've used `uv`, `cargo`, or `npm`, you already know the shape of `hlock`:

| You know this from package managers | In `hlock` |
|---|---|
| `uv.lock` / `Cargo.lock` | **`harness.lock`**, the [lockfile](reference/lockfile.md): every dependency of your harness, pinned and hashed |
| `pip freeze` | **[`hlock pin`](guides/pin.md)**: record the setup you already have, and re-pin as it grows |
| `--locked` check | **[`hlock check`](guides/pin.md#keep-it-honest-with-check)**: show what changed since the pin |
| `uv sync` / `npm ci` | **[`hlock sync`](guides/sync.md)**: rehydrate the locked setup on any machine |
| `uv run` | **[`hlock launch`](guides/launch.md)** and **[`hlock run`](reference/cli.md#run)**: start sessions and run prompts on the locked setup |

<div class="grid cards" markdown>

-   :lucide-lock:{ .lg .middle } **Pin everything**

    ---

    The harness version, models, MCP servers, skills, plugins, instruction files, and
    config, each with a digest. [Claude Code, Codex, Pi, OpenCode, autonomous agents like
    OpenClaw, and more](reference/harnesses.md).

-   :lucide-key-round:{ .lg .middle } **Secrets stay out**

    ---

    API keys and tokens become `HLOCK_SECRET_*` placeholders, so the lock is safe to
    commit. You supply the values when you sync.

-   :lucide-laptop:{ .lg .middle } **Rehydrate anywhere**

    ---

    `hlock sync` restores the setup on a new machine, in an isolated directory, or in a
    Docker image, and tells you exactly what it can't do for you.

-   :lucide-terminal:{ .lg .middle } **Launch from the lock**

    ---

    `hlock launch` starts your harness in an isolated environment that matches the lock,
    with none of the unlocked config on your machine.

</div>

## Manage your team's agents

Contributors to one repository rarely share an agent setup: one uses Claude Code with
hand-written skills, another uses Codex with a different model. Each person can commit their own
lock to `.hlock/locks/harness.<handle>.lock`, next to the code it was used on. Anyone with a clone
can then try a teammate's setup in isolation with `hlock launch --lock`, or
[`hlock pull`](guides/pull.md) it from outside the repository. Add the
[hlock badge](project/badge.md) to your README to point readers at the locks. See
[Team repositories](guides/teams.md) for the layout and the commands.

```bash
hlock pin -o .hlock/locks/harness.alice.lock         # commit your setup with the code
hlock launch --lock .hlock/locks/harness.bob.lock    # try a teammate's, isolated from yours
```

## Go local

[`hlock localize`](guides/localize.md) turns any lock into an equivalent on an open-source
harness and models served by a local runtime like Ollama or LM Studio. It's like
cross-compiling: your MCP servers, skills, and instructions come along. Give it an
[eval pack](guides/eval-packs.md), and it measures the local candidates against your tasks.

## Let an agent do the busywork

Some jobs need judgment, not a script. [`hlock meta`](guides/meta.md) hands them to an
agent, the **meta harness**, and checks every result:

- **`install`**: installs what the sync plan leaves to you, then runs `hlock sync --yes`.
- **`bootstrap-evals`**: fills your eval packs with real tasks from your repo.
- **`update`**: moves skills, MCP servers, and plugins to their latest releases. `--pin`
  records it in the lock.

Use the agent built into hlock, or a harness you already have (Pi, Codex, Goose, OpenCode,
Hermes), on your API key or, with `--local`, open-weight models. It asks before every
command, or runs `--headless` against an allowlist.

## Then make it better

A locked harness is a baseline. Once your setup is reproducible, you can measure it: every
lock `hlock resolve` writes records its success rate, cost, and latency on your own tasks.
The tools will look familiar:

| You know this from | In `hlock` |
|---|---|
| a test suite | **[`hlock evals init --from-traces`](guides/eval-packs.md#draft-a-pack-from-your-traces)**: draft an eval pack from the requests you've made to your agent |
| `pyproject.toml` | **`harness.toml`**, the [manifest](reference/manifest.md): the models, decision models, and harnesses to try, and what "best" means |
| `uv lock` | **[`hlock resolve`](guides/resolve.md)**: measure every combination on your evals, and lock the winner with its evidence |
| `uv lock --upgrade` | **[`hlock upgrade`](guides/upgrade.md)**: try newer models, and keep them only if they hold up |
| profile-guided optimization | **[`hlock tune`](guides/tune.md)**: tune prompts, hyperparameters, and small models on your evals |

<div class="grid cards" markdown>

-   :lucide-flask-conical:{ .lg .middle } **Find the best stack for your tasks**

    ---

    Draft an [eval pack](guides/eval-packs.md) from the sessions your agent already
    logged, list the models and harnesses to try in `harness.toml`, and
    [`hlock resolve`](guides/resolve.md) locks the winner with its evidence.

-   :lucide-sliders-horizontal:{ .lg .middle } **Tune what's inside**

    ---

    [`hlock tune`](guides/tune.md) goes past model choice into prompts, hyperparameters,
    and fine-tuned small models, on your evals. Run it locally, on a devbox, or lift
    trials to a Flyte cluster for GPUs with the [`cluster` backend](guides/backends.md).

</div>

## Where to next

- **New here?** Start with [Installation](getting-started/installation.md) and the
  [Quickstart](getting-started/quickstart.md).
- **Want the mental model?** Read [How it works](concepts/index.md).
- **Sharing a repository?** See [Team repositories](guides/teams.md).
- **Want an agent to do the setup?** See [Delegating to the meta harness](guides/meta.md).
- **Looking something up?** See the [CLI](reference/cli.md),
  [Python API](reference/python-api.md), or [`harness.toml`](reference/manifest.md) reference.
