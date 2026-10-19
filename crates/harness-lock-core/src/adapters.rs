//! Harness adapters — a baseline (reference) adapter for the local loop.

use std::fs;
use std::path::Path;

use serde_json::json;

use crate::error::Result;
use crate::providers::ReferenceProvider;
use crate::search::Stack;
use crate::tasks::Task;

/// The resolved outcome of running one task through a harness.
pub struct RunOutcome {
    pub success: bool,
    pub cost_usd: f64,
    pub latency_s: f64,
    pub steps: u64,
    /// Trace events (a serde array) for content-addressed storage.
    pub trace: serde_json::Value,
}

/// A harness adapter runs one `(stack, task, seed)` and produces a `RunOutcome`.
pub trait HarnessAdapter {
    fn name(&self) -> &str;
    fn run(&self, stack: &Stack, task: &Task, workspace: &Path, seed: u64) -> Result<RunOutcome>;
}

/// Baseline/offline harness that drives the deterministic reference provider.
pub struct ReferenceAdapter {
    provider: ReferenceProvider,
}

impl Default for ReferenceAdapter {
    fn default() -> Self {
        ReferenceAdapter {
            provider: ReferenceProvider::default(),
        }
    }
}

impl HarnessAdapter for ReferenceAdapter {
    fn name(&self) -> &str {
        "reference"
    }

    fn run(&self, stack: &Stack, task: &Task, workspace: &Path, seed: u64) -> Result<RunOutcome> {
        // copy fixtures into the workspace
        if let (Some(fixtures), Some(pack_dir)) = (&task.fixtures, &task.pack_dir) {
            let src = pack_dir.join(fixtures);
            if src.exists() {
                copy_dir(&src, workspace);
            }
        }
        let model_spec = stack
            .llm
            .get("planner")
            .cloned()
            .unwrap_or_else(|| "reference:qwen-7b".to_string());
        let model = self.provider.resolve_alias(&model_spec).1;
        let expected = task.expected_artifacts();

        let mut provider = self.provider.clone();
        provider.seed = seed;
        let mut cost = 0.0;
        let mut latency = 0.0;
        let mut success = false;
        let mut steps = 0u64;
        let mut events = Vec::new();

        for step in 0..stack.max_steps {
            // agent-level retries: attempt this step up to 1+retries times
            let max_attempts = (stack.retries + 1) as usize;
            let mut any_ok = false;
            for _attempt in 0..max_attempts {
                let r = provider.complete(&model, 1);
                cost += r.cost_usd;
                latency += r.latency_ms / 1000.0;
                events.push(json!({
                    "type": "llm.call",
                    "role": "planner",
                    "model": model.clone(),
                    "usage": {"input_tokens": r.usage.input_tokens, "output_tokens": r.usage.output_tokens},
                    "cost_usd": r.cost_usd,
                    "latency_ms": r.latency_ms,
                }));
                if r.content.starts_with("ANSWER_OK") {
                    any_ok = true;
                    break;
                }
            }
            if any_ok {
                // a simulated success leaves the task's solution, when it has one
                let solution = task.solution.as_ref().zip(task.pack_dir.as_ref()).map(|(s, d)| d.join(s)).filter(|p| p.is_dir());
                if let Some(sol) = &solution {
                    copy_dir(sol, workspace);
                }
                for art in expected.iter().filter(|_| solution.is_none()) {
                    let out = workspace.join(art);
                    if let Some(parent) = out.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(&out, format!("artifact for {}", task.id));
                }
                success = true;
                steps = step as u64 + 1;
                break;
            }
            if let Some(max_cost) = stack.max_cost_usd_per_task {
                if cost > max_cost {
                    steps = step as u64 + 1;
                    break;
                }
            }
            steps = step as u64 + 1;
        }

        Ok(RunOutcome {
            success,
            cost_usd: (cost * 1e6).round() / 1e6,
            latency_s: (latency * 1e3).round() / 1e3,
            steps,
            trace: json!(events),
        })
    }
}

pub(crate) fn copy_dir(src: &Path, dst: &Path) {
    let _ = fs::create_dir_all(dst);
    if let Ok(rd) = fs::read_dir(src) {
        for entry in rd.flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir(&from, &to);
            } else if let Ok(data) = fs::read(&from) {
                let _ = fs::write(&to, data);
            }
        }
    }
}
