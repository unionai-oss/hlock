# Rust API

The core of `hlock` is a Rust library, published on crates.io as `hlock`. Two other
crates expose the same API:

| Crate | Use it when |
|---|---|
| [`hlock`](https://crates.io/crates/hlock) | The default: `hlock::Manifest`, `hlock::Tuner`, … |
| [`harness-lock`](https://crates.io/crates/harness-lock) | An alias for `hlock`, under `harness_lock::…` |
| [`harness-lock-core`](https://crates.io/crates/harness-lock-core) | You depend on the implementation crate directly |

```bash
cargo add hlock
```

The full API documentation, generated from the source, is on docs.rs:
**[docs.rs/hlock](https://docs.rs/hlock)** (which re-exports
**[docs.rs/harness-lock-core](https://docs.rs/harness-lock-core)**).

## Example

Resolve a lock, check it against the manifest, and read the pinned stack and its
evidence. This is the same flow as `hlock resolve`, `hlock check`, and `hlock show`.

```rust title="src/main.rs"
use std::path::Path;

use hlock::{Lockfile, Manifest, Tuner};

fn main() -> hlock::Result<()> {
    // Resolve: load the manifest and tune. `tune()` also writes ./harness.lock.
    let manifest = Manifest::load(Path::new("harness.toml"))?;
    let live_hash = manifest.manifest_hash();
    let lock = Tuner::new(manifest, ".harnesslock").tune()?;
    println!("{}", lock.summary());

    // Check: an empty list means the lock matches the manifest.
    let lock = Lockfile::load(Path::new("harness.lock"))?;
    let drift = lock.check_manifest(&live_hash);
    assert!(drift.is_empty(), "drift: {drift:?}");

    // Inspect the pinned stack and its evidence.
    for llm in &lock.llm {
        println!("{} -> {}:{}", llm.role, llm.provider, llm.model);
    }
    let success = lock.evidence.success_rate.get("mean");
    println!("success rate {success:?} over {} trials", lock.evidence.trials);
    Ok(())
}
```

```console
$ cargo run
stack="demo" harness=reference llm=qwen-7b decision=none success-rate=1.00 trials=2
planner -> reference:qwen-7b
success rate Some(1.0) over 2 trials
```

## Key types

| Item | What it is |
|---|---|
| [`Manifest`](https://docs.rs/harness-lock-core/latest/harness_lock_core/manifest/struct.Manifest.html) | A parsed `harness.toml`: `load`, `from_str`, `manifest_hash`, `eval_packs`, `backend_target` |
| [`Tuner`](https://docs.rs/harness-lock-core/latest/harness_lock_core/tuner/struct.Tuner.html) | The resolver: `new`, `with_backend`, `enumerate_stacks`, `run_trial`, `tune` |
| [`Lockfile`](https://docs.rs/harness-lock-core/latest/harness_lock_core/lockfile/struct.Lockfile.html) | The `harness.lock` model: `load`, `from_str`, `save`, `check_manifest`, `summary`, and public fields for every section |
| [`Backend`](https://docs.rs/harness-lock-core/latest/harness_lock_core/worker/enum.Backend.html) | `Local`, `Devbox`, or `Cluster`; `Backend::parse("devbox")` |
| [`Store`](https://docs.rs/harness-lock-core/latest/harness_lock_core/store/struct.Store.html) | The content-addressed artifact store under `.harnesslock/` |
| [`HarnessAdapter`](https://docs.rs/harness-lock-core/latest/harness_lock_core/adapters/trait.HarnessAdapter.html) | The trait a harness implements to run trials |
| [`Error`](https://docs.rs/harness-lock-core/latest/harness_lock_core/error/enum.Error.html), [`Result`](https://docs.rs/harness-lock-core/latest/harness_lock_core/error/type.Result.html) | The error type, with one variant per kind (`Manifest`, `Lockfile`, `EvalPack`, `Tuning`, …) |
| `LOCK_VERSION`, `LOCKFILE_PATH`, `MANIFEST_PATH` | Schema version `1`, `"harness.lock"`, `"harness.toml"` |

## Modules

| Module | Contents |
|---|---|
| [`manifest`](https://docs.rs/harness-lock-core/latest/harness_lock_core/manifest/) | Manifest schema: candidates, objective, budget |
| [`lockfile`](https://docs.rs/harness-lock-core/latest/harness_lock_core/lockfile/) | Lockfile schema: stack, policy, evidence, provenance |
| [`tuner`](https://docs.rs/harness-lock-core/latest/harness_lock_core/tuner/) | The resolver: trial execution and lock construction |
| [`search`](https://docs.rs/harness-lock-core/latest/harness_lock_core/search/) | Stacks, trial results, Pareto fronts, winner selection |
| [`tasks`](https://docs.rs/harness-lock-core/latest/harness_lock_core/tasks/) | Eval packs, the search/holdout split, grading |
| [`ops`](https://docs.rs/harness-lock-core/latest/harness_lock_core/ops/) | `upgrade` and `optimize`, the engines behind those commands |
| [`worker`](https://docs.rs/harness-lock-core/latest/harness_lock_core/worker/) | Backends and the Python worker bridge (versioned JSON contract) |
| [`providers`](https://docs.rs/harness-lock-core/latest/harness_lock_core/providers/) | The deterministic reference LLM provider |
| [`store`](https://docs.rs/harness-lock-core/latest/harness_lock_core/store/), [`hash`](https://docs.rs/harness-lock-core/latest/harness_lock_core/hash/) | Content addressing and hashing |

The `flyte` module is behind the `flyte` cargo feature and is provisional.

!!! note "Python users"

    The [Python API](python-api.md) is built on this crate through PyO3, so the two
    behave the same. Most users only need the Python package and the CLI.
