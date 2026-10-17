//! harness-lock: an alias for [hlock](https://crates.io/crates/hlock).
//!
//! Re-exports [`harness_lock_core`], so `cargo add harness-lock` gives you the same
//! manifest, lockfile, store, and tuning APIs as `hlock`, under `harness_lock::…`.
//! Prefer `cargo add hlock` in new projects.

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
