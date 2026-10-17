//! hlock: make your agent harness pinnable, portable, and tunable.
//!
//! This crate is the published Rust entry point for hlock. It re-exports
//! [`harness_lock_core`], so `cargo add hlock` gives you the manifest, lockfile, store,
//! and tuning APIs under `hlock::…`. The same API is also published under the alias
//! [`harness-lock`](https://crates.io/crates/harness-lock).

pub use harness_lock_core::*;

#[cfg(test)]
mod tests {
    // The facade must expose the same modules and top-level items as the core crate.
    #[test]
    fn reexports_core_api() {
        let _ = std::any::type_name::<crate::Lockfile>();
        let _ = std::any::type_name::<crate::manifest::Manifest>();
        let _ = std::any::type_name::<crate::Resolver>();
        let _ = crate::sha256_text;
    }
}
