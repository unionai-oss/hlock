---
title: hlock
description: Make your agent harness pinnable, portable, and tunable. Pin your harness, model, MCP servers, skills, and config in a harness.lock, version it, and rehydrate it anywhere.
---

<p align="center">
  <img src="assets/logo-animated.svg" alt="hlock logo" width="120">
</p>

# hlock { style="text-align: center" }

<p class="hl-tagline">Make your agent harness pinnable, portable, and tunable.</p>

Your agent is more than a model: it's a harness version, MCP servers, skills, plugins,
instruction files, and config, set up by hand on one machine. `hlock` pins all of it in a
`harness.lock` you commit to git, catches drift, and brings the same setup back on
another laptop, in CI, or in a container.
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

<div class="hl-demo" markdown>
![An hlock session: pin a Claude Code setup, check it, and sync it onto another machine](assets/quickstart.svg)
</div>

## Lock the harness you already have

If you've used `uv`, `cargo`, or `npm`, you already know the shape of `hlock`:

| You know this from package managers | In `hlock` |
|---|---|
| `uv.lock` / `Cargo.lock` | **`harness.lock`**, the [lockfile](reference/lockfile.md): every dependency of your harness, pinned and hashed |
| `pip freeze` | **[`hlock pin`](guides/pin.md)**: record the setup you already have, and re-pin as it grows |
| `--locked` check | **[`hlock check`](guides/pin.md#keep-it-honest-with-check)**: show what changed since the pin |
| `uv sync` / `npm ci` | **[`hlock sync`](guides/sync.md)**: rehydrate the locked setup on any machine |
| `uv run` | **[`hlock launch`](guides/launch.md)**: start sessions and run prompts on the locked setup |

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

Once your harness is locked, you have a baseline to improve on.

<div class="grid cards" markdown>

-   :lucide-flask-conical:{ .lg .middle } **Optimize your harness**

    ---

    Start an [eval pack](guides/eval-packs.md) with `hlock evals init`, and have your agent
    fill it in with real work from your project.
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

</div>

## Where to next

- **New here?** Start with [Installation](getting-started/installation.md) and the
  [Quickstart](getting-started/quickstart.md).
- **Want the mental model?** Read [How it works](concepts/index.md).
- **Looking something up?** See the [CLI](reference/cli.md),
  [Python API](reference/python-api.md), or [`harness.toml`](reference/manifest.md) reference.
