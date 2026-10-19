//! Core library for **harness-lock** (`hlock`).
//!
//! Implements the manifest, the lockfile, the content-addressed store, task
//! packs, the deterministic reference provider/harness, the search layer, and
//! the resolver — the same v0.1 surface as the (now reference) Python
//! implementation, in Rust. The [`flyte`] backend and the PyO3 Python bindings
//! live in sibling crates.
//!
//! The Python-facing API mirrors [`resolve`](resolver::Resolver::resolve) and the lockfile
//! model so the user surface stays stable across the language boundary.

pub mod adapters;
pub mod agent;
pub mod capture;
pub mod contract;
pub mod launch;
pub mod localize;
pub mod error;
pub mod hash;
pub mod lockfile;
pub mod manifest;
pub mod meta;
pub mod ops;
pub mod profiles;
pub mod providers;
pub mod pull;
pub mod resolver;
pub mod search;
pub mod store;
pub mod sync;
pub mod tasks;
pub mod worker;

#[cfg(feature = "flyte")]
pub mod flyte;

pub use adapters::HarnessAdapter;
pub use error::{Error, Result};
pub use hash::{content_hash, sha256_hex, sha256_text};
pub use lockfile::Lockfile;
pub use manifest::{find_manifest, Manifest};
pub use profiles::Registry;
pub use resolver::Resolver;
pub use store::Store;
pub use worker::Backend;

/// Schema version of the `harness.lock` lockfile.
pub const LOCK_VERSION: i64 = 1;
/// Default output filename for the lockfile.
pub const LOCKFILE_PATH: &str = "harness.lock";
/// Default manifest filename.
pub const MANIFEST_PATH: &str = "harness.toml";
