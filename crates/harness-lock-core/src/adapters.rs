//! Harness adapters — a baseline (reference) adapter for the local loop.

use std::fs;
use std::path::Path;

use serde_json::json;

use crate::error::Result;
use crate::fsutil::copy_tree;
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
            let src = pack_dir.join(pack_rel("fixtures", fixtures)?);
            if src.exists() {
                copy_tree(&src, workspace)?;
            }
        }
        let solution = match (&task.solution, &task.pack_dir) {
            (Some(s), Some(d)) => Some(d.join(pack_rel("solution", s)?)).filter(|p| p.is_dir()),
            _ => None,
        };
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
            for attempt in 0..max_attempts {
                let r = provider.complete_for(&model, &task.id, step as u64, attempt as u64);
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
                if let Some(sol) = &solution {
                    copy_tree(sol, workspace)?;
                }
                for art in expected.iter().filter(|_| solution.is_none()) {
                    let out = workspace.join(pack_rel("expected artifact", Path::new(art))?);
                    if let Some(parent) = out.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    crate::fsutil::clear_symlink(&out)?;
                    fs::write(&out, format!("artifact for {}", task.id))?;
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

/// A path a task names in the pack or the workspace, checked to stay inside it.
fn pack_rel<'a>(what: &str, p: &'a Path) -> Result<&'a Path> {
    crate::fsutil::check_safe_rel(what, &p.to_string_lossy())?;
    Ok(p)
}
