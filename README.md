<div align="center">

<img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/logo-animated.svg" alt="hlock logo" width="120">

# hlock

### Make your agent harness pinnable, portable, and tunable.

**Pin your harness, models, MCP servers, skills, and config in a `harness.lock`, version it, and rehydrate it anywhere.**

[![PyPI](https://img.shields.io/pypi/v/hlock?label=pypi&color=3775A9&logo=pypi&logoColor=white)](https://pypi.org/project/hlock/)
[![crates.io](https://img.shields.io/crates/v/hlock?color=E6532D&logo=rust&logoColor=white)](https://crates.io/crates/hlock)
[![CI](https://img.shields.io/github/actions/workflow/status/unionai-oss/hlock/ci.yml?branch=main&label=ci&logo=github)](https://github.com/unionai-oss/hlock/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-docs.hlock.ai-7C3AED?logo=readthedocs&logoColor=white)](https://docs.hlock.ai/)
[![Python](https://img.shields.io/pypi/pyversions/hlock?logo=python&logoColor=white)](https://pypi.org/project/hlock/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/unionai-oss/hlock/blob/main/LICENSE)

[**Documentation**](https://docs.hlock.ai/) ·
[Quickstart](https://docs.hlock.ai/getting-started/quickstart/) ·
[CLI reference](https://docs.hlock.ai/reference/cli/) ·
[Design specs](https://github.com/unionai-oss/hlock/tree/main/specs)

</div>

---

Your agent is more than a model: it's a harness version, MCP servers, skills, plugins,
instruction files, and config, set up by hand on one machine. `hlock` pins all of it in a
`harness.lock` you commit to git, catches drift, and brings the same setup back on
another laptop, in CI, or in a container. When you're ready, it measures your stack
against your own tasks and finds a better one.

| You know this from package managers | In `hlock` |
|---|---|
| 🔒 `uv.lock` / `Cargo.lock` | **`harness.lock`**: every dependency of your harness, pinned and hashed |
| 📌 `pip freeze` | **`hlock pin`**: record the setup you already have, and re-pin as it grows |
| ✅ `--locked` check | **`hlock check`**: show what changed since the pin |
| 📦 `uv sync` / `npm ci` | **`hlock sync`**: rehydrate the locked setup on any machine |
| ▶️ `uv run` | **`hlock launch`** and **`hlock run`**: start sessions and run prompts on the locked setup |

## Install

```bash
pip install hlock        # or: uv add hlock  ·  uvx hlock --help
```

Prebuilt wheels cover Linux (x86_64, aarch64), macOS (arm64, x86_64), and Windows (x64)
for CPython 3.9+. For Rust, use `cargo add hlock`.

> [!NOTE]
> [`harness-lock`](https://pypi.org/project/harness-lock/) is an alias for `hlock`.
> Both names install the same package at the same version, so `import hlock` and
> `import harness_lock` are the same API.

## Quickstart

Pin the harness you already use (Claude Code, Codex, Pi, OpenCode, Cursor, Gemini CLI,
and [more](https://docs.hlock.ai/guides/harnesses/)), then bring it back anywhere:

```bash
hlock pin                       # lock your current setup into harness.lock
git add harness.lock .hlock/    # version it (secrets are placeholders, never values)
hlock check                     # see what changed since the pin; re-pin to lock it
hlock sync                      # on another machine: rehydrate it into .hlock/env (or --in-place, --docker)
hlock launch                    # start a session on the locked setup, isolated from your config
hlock run "summarize README.md"  # or run one prompt through it (claude -p, codex exec, …)
```

For your own work, `hlock evals init -t coding` (or `data`, `research`, `support`) writes
an eval pack template plus an Agent Skill, so the agent you already use can fill it in
with real tasks from your project. See [Eval packs](https://docs.hlock.ai/guides/eval-packs/).

<p align="center">
  <img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/quickstart.svg" alt="An hlock session: pin a Claude Code setup, check it, and sync it onto another machine" width="640">
</p>

See the [Quickstart](https://docs.hlock.ai/getting-started/quickstart/),
[Pinning your setup](https://docs.hlock.ai/guides/pin/),
[Rehydrating with sync](https://docs.hlock.ai/guides/sync/), and
[Launching a locked harness](https://docs.hlock.ai/guides/launch/).

### Then make it better

**Optimize your harness.** Draft an eval pack from the requests you've already made to your
agent with `hlock evals init --from-traces` (it reads the sessions Claude Code, Codex, and
Pi logged for the project), and review it with your agent using the Agent Skill it writes. List the models, decision models and harnesses to try in a
`harness.toml`, and let `hlock resolve` lock the best combination with its success rate,
cost, and latency. `hlock tune` goes further, into prompts, hyperparameters, and
fine-tuned small models. The built-in `reference` provider is offline, so you can try the
loop with no API keys:

```bash
hlock evals init                          # write a hello-world eval pack to ./evals
hlock init --name demo                    # write a starter harness.toml
hlock resolve                             # measure every combination -> harness.lock
hlock run "count the words in words.txt"  # run the locked agent
```

**Go local.** `hlock localize` turns a lock into an equivalent on an open-source harness
and models served by Ollama or LM Studio, keeping your MCP servers, skills, and
instructions.

**Let an agent do the busywork.** `hlock meta` hands work that needs judgment (installing
what a lock needs on a new machine, filling in an eval pack, updating skills and MCP
servers) to a pinned agent and checks the result itself. The default is an agent built
into hlock, so there's nothing to install: give it an API key or a local model.

```bash
hlock meta init                 # pick a model for the built-in agent, and pin it
hlock meta run install          # install what harness.lock still needs, then verify
```

See [Finding the best stack](https://docs.hlock.ai/guides/resolve/),
[Eval packs](https://docs.hlock.ai/guides/eval-packs/),
[Tuning](https://docs.hlock.ai/guides/tune/), and
[Going local](https://docs.hlock.ai/guides/localize/), and
[The meta harness](https://docs.hlock.ai/guides/meta/).

<details>
<summary><b>Python API</b></summary>

```python
import json
import hlock
from hlock import SearchSpace, Objective, Budget

space = SearchSpace(
    llm={"planner": ["reference:qwen-7b", "reference:qwen-32b"]},
    decision=["none", "rules:default"],
    harness=["reference"],
    objective=Objective(select="min-cost", constraints={"success-rate": ">=0.5"}),
    budget=Budget(max_trials=50),
    evals=json.dumps({"packs": ["./evals"], "seeds": 2}),
)
lock = hlock.resolve(space, backend="local")   # writes ./harness.lock
print(lock.summary())

agent = hlock.load("harness.lock")
print(agent.run("clean ./data/orders.csv"))   # {"success": true, "model": "qwen-7b", ...}
```

</details>

## How it works

<p align="center">
  <img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/how-it-works.svg" alt="How hlock works: hlock pin captures your setup, or hlock resolve searches harness.toml and eval packs; either writes harness.lock for hlock check, sync, and launch" width="860">
</p>

## Architecture

A Rust core does all the deterministic work. Anything that needs the Python ML
ecosystem or a GPU runs in a Python worker, which can lift each piece of work to a
Flyte task on a cluster.

<p align="center">
  <img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/architecture.svg" alt="hlock architecture: Python CLI and API over a Rust core via PyO3; local runs in process, devbox and cluster hand work to a Python worker that can lift stages to Flyte or Union tasks" width="860">
</p>

| Path | What lives there |
|---|---|
| [`crates/harness-lock-core`](https://github.com/unionai-oss/hlock/tree/main/crates/harness-lock-core) | The core library (pure Rust): manifests, lockfiles, search planning, trial bookkeeping, evidence, lock construction. It orchestrates and aggregates, and never does heavy compute itself. |
| [`crates/harness-lock-py`](https://github.com/unionai-oss/hlock/tree/main/crates/harness-lock-py) | PyO3 bindings, compiled into `harness_lock._harness_lock` and shipped only inside the wheel. |
| [`crates/hlock`](https://github.com/unionai-oss/hlock/tree/main/crates/hlock), [`crates/harness-lock`](https://github.com/unionai-oss/hlock/tree/main/crates/harness-lock) | Thin crates that re-export the core (`harness-lock` is the alias). |
| [`harness_lock/`](https://github.com/unionai-oss/hlock/tree/main/harness_lock) | The Python package: the `hlock` CLI and the heavy-work worker ([`tasks/worker.py`](https://github.com/unionai-oss/hlock/blob/main/harness_lock/tasks/worker.py)). |
| [`hlock/`](https://github.com/unionai-oss/hlock/tree/main/hlock) | A shim that re-exports `harness_lock`, so `import hlock` works. |
| [`packaging/harness-lock`](https://github.com/unionai-oss/hlock/tree/main/packaging/harness-lock) | The `harness-lock` PyPI alias, which pins the matching `hlock` release. |
| [`docs/`](https://github.com/unionai-oss/hlock/tree/main/docs) | The documentation site ([Zensical](https://zensical.org)), published to GitHub Pages. |

## Backends

| Backend | Where trials run | Use it for |
|---|---|---|
| `local` *(default)* | in-process Rust reference harness | trying it out, CI, deterministic tests |
| `devbox` | Python worker on this machine | real LLM calls, your own traces, `flyte local` |
| `cluster` | Python worker that lifts each stage to Flyte/Union | GPUs, fan-out, durable caching, shared results |

```bash
hlock tune --backend devbox --depth probe --max-trials 10
# real LLM calls: export HL_LLM_BASE_URL=http://… HL_LLM_MODEL=qwen-32b [HL_LLM_API_KEY=…]
hlock tune --backend cluster --depth ft --data user:./runs   # LoRA on a GPU worker
```

See [Backends](https://docs.hlock.ai/guides/backends/) for setup and
the `HL_*` environment variables.

## Development

Development uses [uv](https://docs.astral.sh/uv/) and a Rust toolchain.

```bash
uv sync                            # create .venv, build the Rust extension, install dev tools
uv run pytest                      # Python bindings, CLI, worker
uv run ruff check harness_lock hlock tests
cargo test -p harness-lock-core    # Rust core + integration tests

uv run --group docs zensical serve # docs at http://localhost:8000
```

`uv run` rebuilds the extension automatically when Rust sources change.

Want to help? See the [contributing guide](https://docs.hlock.ai/project/contributing/). Releases are cut from tags. See [RELEASING.md](https://github.com/unionai-oss/hlock/blob/main/RELEASING.md).

## Status

What works today: `pin`, `check`, `sync`, and `launch` for the
[supported harnesses](https://docs.hlock.ai/guides/harnesses/), `localize`, manifest and
lockfile schema v1, a content-addressed store, eval packs (`hlock evals`) with graders, the
reference provider and harness, grid search with Pareto selection, `upgrade`
(plan/apply/rollback), `tune` across the `local`, `devbox`, and `cluster` backends, and the
`meta` harness. The [design specs](https://github.com/unionai-oss/hlock/tree/main/specs) describe the full design,
much of which is still on the roadmap.

## License

[Apache-2.0](https://github.com/unionai-oss/hlock/blob/main/LICENSE)
