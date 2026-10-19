# Installation

`hlock` ships as a Python package with a compiled Rust core. Prebuilt wheels cover:

| Platform | Architectures |
|---|---|
| Linux (manylinux) | x86_64, aarch64 |
| macOS | arm64 (Apple silicon), x86_64 (Intel) |
| Windows | x64 |

One wheel per platform covers **CPython 3.9 and newer** (abi3). On other platforms,
pip builds from the source distribution, which needs a [Rust toolchain](https://rustup.rs).

## Install the CLI and Python package

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

Check that it worked:

```console
$ hlock --version
hlock, version 0.1.2
```

hlock works with Claude Code, Codex, Pi, OpenCode, Cursor, Gemini CLI and more; see
[Supported harnesses](../reference/harnesses.md).

!!! info "`harness-lock` is an alias for `hlock`"

    [`harness-lock`](https://pypi.org/project/harness-lock/) is published as an alias:
    each release pins the identical `hlock` release, so installing either name gives you
    the same `hlock` (and `harness-lock`) command and the same API. `import hlock` and
    `import harness_lock` are interchangeable. These docs use `hlock` everywhere.

!!! warning "Upgrading from 0.1.2 or earlier with pip"

    Through 0.1.2 the roles were reversed: `harness-lock` was the package and `hlock` the
    alias. Both the old `harness-lock` and the new `hlock` contain the `harness_lock`
    module, so `pip install -U harness-lock` in an existing 0.1.2 environment can leave
    `harness_lock` half removed (`import hlock` then fails with an `ImportError`).
    Reinstall cleanly instead:

    ```bash
    pip uninstall -y harness-lock hlock && pip install hlock
    ```

    `uv` upgrades in place without this problem.

## Optional extras

| Extra | Install | Adds |
|---|---|---|
| `flyte` | `pip install "hlock[flyte]"` | `flyteplugins-rs`, so the [`cluster` backend](../guides/backends.md) can lift work to Flyte/Union tasks |

The worker that runs heavy tuning stages (`devbox` and `cluster` backends) imports
`pandas`/`polars`, `torch`/`peft`/`trl`, and `flyte` lazily, and only when a stage needs
them. Install those in the worker's environment when you use the stages that need them;
see [Backends](../guides/backends.md).

## Use it from Rust

The core library is published on crates.io as `hlock`:

```bash
cargo add hlock
```

`hlock` (and its alias crate, `harness-lock`) re-export
[`harness-lock-core`](https://crates.io/crates/harness-lock-core).
See the [Rust API](../reference/rust-api.md) reference for an example and the key types.

## Build from source

See [Development](../project/development.md).
