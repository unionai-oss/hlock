//! A backend written in Rust: it answers each trial by writing the task's prompt to the
//! task's first expected artifact. Build it, put it on `PATH` as `hlock-backend-echo`, and
//! `[backend] target = "echo"` runs trials on it (see docs/guides/backends.md).
//!
//!     cargo build -p hlock --example echo_backend
//!     cp target/debug/examples/echo_backend ~/.local/bin/hlock-backend-echo

use std::path::Path;

use hlock::worker::{serve, Describe, Executor, TaskRequest, TaskResult};
use hlock::{Error, Result};

struct Echo;

impl Executor for Echo {
    fn describe(&self, _req: &TaskRequest) -> Result<Describe> {
        Ok(Describe::new("echo", ["llm.run_trial"]).about("echo example backend (Rust)"))
    }

    fn call(&self, req: &TaskRequest) -> Result<TaskResult> {
        if req.kind != "llm.run_trial" {
            return Err(Error::Worker(format!("echo doesn't run {}", req.kind)));
        }
        let artifact = req.task["expected"][0].as_str().unwrap_or("answer.md");
        let prompt = req.task["prompt_text"].as_str().unwrap_or_default();
        std::fs::write(Path::new(&req.workdir).join(artifact), prompt)?;
        Ok(TaskResult { success: true, steps: 1, cost_usd: Some(0.0), ..Default::default() })
    }
}

fn main() {
    std::process::exit(serve(&Echo));
}
