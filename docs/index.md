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
    config, each with a digest. [Claude Code, Codex, Pi, OpenCode, and more](guides/harnesses.md).

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

## Then make it better

Once your harness is locked, you have a baseline to improve on, with tools you'll also
recognize:

| You know this from | In `hlock` |
|---|---|
| a test suite | **[`hlock evals init --from-traces`](guides/eval-packs.md#draft-a-pack-from-your-traces)**: draft an eval pack from the requests you've made to your agent |
| `pyproject.toml` | **`harness.toml`**, the [manifest](reference/manifest.md): the models, decision models, and harnesses to try, and what "best" means |
| `uv lock` | **[`hlock resolve`](guides/resolve.md)**: measure every combination on your evals, and lock the winner with its evidence |
| `uv lock --upgrade` | **[`hlock upgrade`](guides/upgrade.md)**: try newer models, and keep them only if they hold up |
| profile-guided optimization | **[`hlock tune`](guides/tune.md)**: tune prompts, hyperparameters, and small models on your evals |
| cross-compiling | **[`hlock localize`](guides/localize.md)**: the same harness setup, on an open-source harness and local models |

<div class="grid cards" markdown>

-   :lucide-flask-conical:{ .lg .middle } **Optimize your harness**

    ---

    Draft an [eval pack](guides/eval-packs.md) from your agent's past sessions, and review
    it with your agent.
    [`hlock resolve`](guides/resolve.md) finds the best combination of models, decision
    models, and harnesses for it, and [`hlock tune`](guides/tune.md) tunes
    prompts, hyperparameters, and small models. Every lock records its success rate,
    cost, and latency.

-   :lucide-house:{ .lg .middle } **Go local**

    ---

    [`hlock localize`](guides/localize.md) moves your lock off proprietary harnesses and
    API models onto an open-source harness and models that run on your machine, keeping
    your MCP servers, skills, and instructions. Give it an eval pack, and it measures the
    local candidates against your tasks.

-   :lucide-bot:{ .lg .middle } **Delegate the busywork**

    ---

    [`hlock meta`](guides/meta.md) hands work that needs judgment to an agent built into
    hlock: installing what a lock needs on a new machine, filling in an eval pack,
    updating skills and MCP servers. It runs on your API key or a local model, and hlock
    checks every result.

</div>

## Where to next

- **New here?** Start with [Installation](getting-started/installation.md) and the
  [Quickstart](getting-started/quickstart.md).
- **Want the mental model?** Read [How it works](concepts/index.md).
- **Want an agent to do the setup?** See [Delegating to the meta harness](guides/meta.md).
- **Looking something up?** See the [CLI](reference/cli.md),
  [Python API](reference/python-api.md), or [`harness.toml`](reference/manifest.md) reference.
