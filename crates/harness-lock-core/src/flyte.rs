//! Flyte backend (provisional) built on flyte-sdk-rs.
//!
//! Enabled with `cargo build --features flyte`. Uses
//! https://github.com/flyteorg/flyte-sdk-rs for durable, traced trial execution.
//!
//! The SDK's model: a task container binary declares `#[flyte::task]` /
//! `#[flyte::trace]` steps, `flyte::run` executes bodies locally, and a Python
//! launcher (`flyteplugins-rs`) registers the task with a Flyte backend
//! (reading `~/.flyte/config.yaml`) and builds the worker image.
//!
//! This module is intentionally minimal and **not validated against a live
//! backend yet** — it records the pointer so the resolver's `run_trial` can be
//! wrapped as a traced step (replay-on-retry, human-in-the-loop via
//! `flyte::condition`). Full fan-out and image bundling is a follow-up against
//! flyte-sdk-rs (the SDK is experimental: `flyte = "0.1"`).

#![cfg(feature = "flyte")]

use flyte as _flyte_crate; // pins the dependency so `--features flyte` compiles it

/// A single trial step, recorded as a child action on a backend and replayed on
/// retry (in-process checkpointing). Returns the resolved trial key.
#[flyte::trace]
pub async fn run_trial_step(stack_hash: String, task_id: String, seed: u64) -> Result<String, flyte::Error> {
    Ok(format!("{stack_hash}:{task_id}:{seed}"))
}
