<div align="center">

<img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/logo-animated.svg" alt="hlock logo" width="120">

# hlock

### Make your agent harness pinnable, portable, and tunable.

**Tune your agent stack against your own tasks, then pin the winner in a `harness.lock`.**

[![PyPI](https://img.shields.io/pypi/v/hlock?label=pypi&color=3775A9&logo=pypi&logoColor=white)](https://pypi.org/project/hlock/)
[![crates.io](https://img.shields.io/crates/v/hlock?color=E6532D&logo=rust&logoColor=white)](https://crates.io/crates/hlock)
[![CI](https://img.shields.io/github/actions/workflow/status/unionai-oss/hlock/ci.yml?branch=main&label=ci&logo=github)](https://github.com/unionai-oss/hlock/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-docs.hlock.ai-7C3AED?logo=readthedocs&logoColor=white)](https://docs.hlock.ai/)
[![Python](https://img.shields.io/pypi/pyversions/hlock?logo=python&logoColor=white)](https://pypi.org/project/hlock/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/unionai-oss/hlock/blob/main/LICENSE)

[**Documentation**](https://docs.hlock.ai/) ·
[Quickstart](https://docs.hlock.ai/getting-started/quickstart/) ·
[CLI reference](https://docs.hlock.ai/reference/cli/) ·
[Design spec](https://github.com/unionai-oss/hlock/blob/main/SPEC.md)

</div>

---

`hlock` treats an agent system the way a package manager treats dependencies. You
bring your domain tasks, the models you're allowed to use, and the harness you like.
`hlock` searches the stack, measures every candidate against your tasks, and writes a
`harness.lock`: a pinned, version-controlled, enforceable description of the agent that
actually works for your domain, along with the evidence for choosing it.

| You know this from package managers | In `hlock` |
|---|---|
| 📄 `pyproject.toml` | **`harness.toml`**, the manifest: what the resolver may try, and what "best" means |
| 🧮 dependency resolver | **the resolver**: search over LLMs × decision models × harnesses × policies |
| 🔒 `uv.lock` / `Cargo.lock` | **`harness.lock`**, the lockfile: what was chosen, pinned and hashed, plus the evidence |
| ✅ CI `--locked` check | **`hlock check`**: fail when the manifest drifts from the lock |

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

No accounts or API keys needed. The built-in `reference` provider and harness are
deterministic and offline, so the whole loop runs on a laptop.

```bash
hlock evals init                          # write a hello-world eval pack to ./evals
hlock init --name demo                    # write a starter harness.toml
hlock resolve                             # search the space -> harness.lock
hlock check                               # fail if harness.toml drifted from the lock
hlock run "count the words in words.txt"  # run the locked agent
```

For your own work, `hlock evals init -t coding` (or `data`, `research`, `support`) writes
an eval pack template plus an Agent Skill, so the agent you already use can fill it in
with real tasks from your project. See [Eval packs](https://docs.hlock.ai/guides/eval-packs/).

<p align="center">
  <img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/quickstart.svg" alt="An hlock session: evals init, init, resolve, check, and run" width="640">
</p>

From there: `hlock show` prints the locked stack and its evidence, `hlock tree` prints the
component graph, `hlock upgrade` plans model upgrades, and `hlock tune` tunes prompts,
hyperparameters, and small models into a `harness.tune.lock`.

### Already have a harness set up?

Pin it. `hlock pin` captures your Claude Code, Codex, Pi, OpenCode, Hermes or Cursor setup
into `harness.lock`: the harness version, the models and local runtimes it uses, MCP
servers, skills, plugins, and config, with secrets replaced by placeholders.

```bash
hlock harness list      # the harnesses on this machine, and the one you're running inside
hlock pin               # lock your current setup into harness.lock
hlock check             # fail if your setup drifted from the pin
hlock sync              # on another machine: rehydrate it (also --prefix DIR, --docker)
hlock launch            # start a session that matches the lock (or: hlock launch opencode)
hlock localize          # an all-local equivalent: local harness + Ollama/LM Studio models
```

See [Harnesses](https://docs.hlock.ai/guides/harnesses/) and
[Pinning your setup](https://docs.hlock.ai/guides/pin/),
[Rehydrating with sync](https://docs.hlock.ai/guides/sync/), and
[Launching a locked harness](https://docs.hlock.ai/guides/launch/).

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
  <img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/how-it-works.svg" alt="How hlock works: harness.toml and eval packs feed hlock resolve (search, evaluate, select), which writes harness.lock for hlock check and hlock run" width="860">
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

`hlock` is at **v0.1**. What works today: manifest and lockfile schema v1, a
content-addressed store, eval packs with graders, the reference provider and harness,
grid search with Pareto selection, `upgrade` (plan/apply/rollback), and `tune` across
the `local`, `devbox`, and `cluster` backends. [SPEC.md](https://github.com/unionai-oss/hlock/blob/main/SPEC.md) describes the full design,
much of which is still on the roadmap.

## License

[Apache-2.0](https://github.com/unionai-oss/hlock/blob/main/LICENSE)
