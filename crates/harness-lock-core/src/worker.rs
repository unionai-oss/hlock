//! Bridge from the Rust core to the Python worker (`hlock.tasks.worker`).
//!
//! The Rust core stays the orchestrator and aggregator — manifests, search
//! planning, trial bookkeeping, evidence, and lock construction. The
//! ecosystem- and compute-heavy *work items* of `hlock tune` that must run
//! where the Python toolchain and GPUs live (real LLM invocation, user-trace
//! loading with pandas/polars, synthetic-data generation, model grading, prompt
//! search, LoRA fine-tuning) are handled by a small Python worker that the core
//! shells out to over a versioned JSON contract.
//!
//! On `local` the resolver uses the deterministic in-process reference harness (no
//! Python). On `devbox`/`cluster` the [`PythonAdapter`] takes over and calls
//! `python -m hlock.tasks.worker <request.json> <response.json>`; that
//! worker may in turn lift each body to a real Flyte task via the Python `flyte`
//! / `flyteplugins-rs` packages, but returns the identical response shape either
//! way — so the Rust core never changes based on where the work actually ran.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::adapters::{HarnessAdapter, RunOutcome};
use crate::error::{Error, Result};
use crate::search::Stack;
use crate::tasks::Task;

/// Where the resolver executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Local,
    Devbox,
    Cluster,
}

impl Backend {
    pub fn parse(s: &str) -> Backend {
        match s.trim().to_ascii_lowercase().as_str() {
            "devbox" => Backend::Devbox,
            "cluster" => Backend::Cluster,
            _ => Backend::Local,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Local => "local",
            Backend::Devbox => "devbox",
            Backend::Cluster => "cluster",
        }
    }

    /// Whether this backend routes work through the Python worker.
    pub fn needs_worker(&self) -> bool {
        matches!(self, Backend::Devbox | Backend::Cluster)
    }
}

/// Current version of the Rust <-> Python task contract.
pub const CONTRACT_VERSION: u64 = 1;

/// One unit of work sent to the Python worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub version: u64,
    /// Handler id: `llm.run_trial | data.load | data.synthetic | grade |
    /// prompt.search | ft.lora`.
    pub kind: String,
    /// Backend the worker should run the body under (devbox/cluster -> Flyte lift).
    pub backend: String,
    pub stack: serde_json::Value,
    pub task: serde_json::Value,
    pub seed: u64,
    pub workdir: String,
    pub plan: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub gpu_h: f64,
    #[serde(default)]
    pub latency_s: f64,
    #[serde(default)]
    pub steps: u64,
    #[serde(default)]
    pub trace: serde_json::Value,
    #[serde(default)]
    pub artifact_digests: Vec<String>,
    #[serde(default)]
    pub model_digests: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskResponse {
    pub version: u64,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub result: Option<TaskResult>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A `HarnessAdapter` that delegates each trial to the Python worker.
///
/// Thin wrapper over [`PythonWorker`] for the trial path (kind `llm.run_trial`).
pub struct PythonAdapter {
    worker: PythonWorker,
}

impl PythonAdapter {
    pub fn new() -> Self {
        PythonAdapter {
            worker: PythonWorker::new(),
        }
    }
}

impl Default for PythonAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessAdapter for PythonAdapter {
    fn name(&self) -> &str {
        "python-worker"
    }

    fn run(&self, stack: &Stack, task: &Task, workspace: &Path, seed: u64) -> Result<RunOutcome> {
        let r = self
            .worker
            .call("llm.run_trial", "cluster", Some(stack), Some(task), seed, workspace, &json!({}))?;
        Ok(RunOutcome {
            success: r.success,
            cost_usd: r.cost_usd,
            latency_s: r.latency_s,
            steps: r.steps,
            trace: r.trace,
        })
    }
}

/// Shells any work item out to the Python worker over the versioned JSON contract.
///
/// Used by the resolver (as `PythonAdapter`) for trials and directly by `ops::tune`
/// for the data / prompt / fine-tune stages of the pipeline.
///
/// The interpreter is taken from `HL_PYTHON` (falling back to `python3`) so it can
/// be pinned to the interpreter that actually has `hlock` installed (e.g. a
/// project venv), independent of the interpreter the Rust wheel itself runs inside.
pub struct PythonWorker {
    interp: String,
    module: String,
}

impl PythonWorker {
    pub fn new() -> Self {
        let interp = std::env::var("HL_PYTHON").unwrap_or_else(|_| "python3".to_string());
        PythonWorker {
            interp,
            module: "hlock.tasks.worker".to_string(),
        }
    }

    /// Invoke the worker for `kind`, writing a request into `workspace`, and return
    /// the parsed result. `plan` carries the depth/data/axes so the worker is
    /// depth-aware on devbox/cluster.
    pub fn call(
        &self,
        kind: &str,
        backend: &str,
        stack: Option<&Stack>,
        task: Option<&Task>,
        seed: u64,
        workspace: &Path,
        plan: &serde_json::Value,
    ) -> Result<TaskResult> {
        let req_path = workspace.join("_al_request.json");
        let out_path = workspace.join("_al_response.json");
        if let Some(parent) = req_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let request = TaskRequest {
            version: CONTRACT_VERSION,
            kind: kind.to_string(),
            backend: backend.to_string(),
            stack: stack.map(|s| s.to_value()).unwrap_or(json!({})),
            task: task.map(|t| {
                json!({
                    "id": t.id,
                    "prompt_text": t.prompt_text,
                    "expected": t.expected_artifacts(),
                    "fixtures": t.fixtures.as_ref().map(|p| p.to_string_lossy().to_string()),
                })
            })
            .unwrap_or(json!({})),
            seed,
            workdir: workspace.to_string_lossy().to_string(),
            plan: plan.clone(),
        };
        std::fs::write(&req_path, serde_json::to_vec(&request)?)?;

        let out = std::process::Command::new(&self.interp)
            .arg("-m")
            .arg(&self.module)
            .arg(&req_path)
            .arg(&out_path)
            .output()
            .map_err(|e| {
                Error::Worker(format!(
                    "could not start Python worker ({}) — is harness-lock installed for `{}`? ({e})",
                    self.module, self.interp
                ))
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Worker(format!(
                "python worker failed ({}): {}",
                out.status,
                stderr.trim()
            )));
        }
        let text = std::fs::read_to_string(&out_path).map_err(|e| {
            Error::Worker(format!("worker did not write a response file {}: {e}", out_path.display()))
        })?;
        let resp: TaskResponse = serde_json::from_str(&text)?;
        if !resp.ok {
            return Err(Error::Worker(
                resp.error.unwrap_or_else(|| "worker reported failure".into()),
            ));
        }
        resp.result.ok_or_else(|| Error::Worker("worker returned no result".into()))
    }
}

impl Default for PythonWorker {
    fn default() -> Self {
        Self::new()
    }
}
