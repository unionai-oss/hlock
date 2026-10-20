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
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::adapters::{HarnessAdapter, RunOutcome};
use crate::error::{Error, Result};
use crate::process::{self, Limits};
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
    backend: Backend,
}

impl PythonAdapter {
    /// An adapter for the `cluster` backend. Prefer [`PythonAdapter::with_backend`].
    pub fn new() -> Self {
        Self::with_backend(Backend::Cluster)
    }

    /// An adapter that tells the worker which backend to run trials under.
    pub fn with_backend(backend: Backend) -> Self {
        PythonAdapter {
            worker: PythonWorker::new(),
            backend,
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
            .call("llm.run_trial", self.backend.as_str(), Some(stack), Some(task), seed, workspace, &json!({}))?;
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
    /// How long one call may take (`HLOCK_WORKER_TIMEOUT` seconds, default an hour).
    timeout: Duration,
}

/// The default for `HLOCK_WORKER_TIMEOUT`, in seconds.
pub const DEFAULT_WORKER_TIMEOUT: u64 = 3600;

impl PythonWorker {
    pub fn new() -> Self {
        let interp = std::env::var("HL_PYTHON").unwrap_or_else(|_| "python3".to_string());
        let secs = std::env::var("HLOCK_WORKER_TIMEOUT").ok().and_then(|s| s.trim().parse::<u64>().ok()).filter(|s| *s > 0).unwrap_or(DEFAULT_WORKER_TIMEOUT);
        PythonWorker {
            interp,
            module: "hlock.tasks.worker".to_string(),
            timeout: Duration::from_secs(secs),
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
        // a response left from an earlier call must not pass for this one's
        match std::fs::remove_file(&out_path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }

        let mut cmd = std::process::Command::new(&self.interp);
        cmd.arg("-m").arg(&self.module).arg(&req_path).arg(&out_path).stdin(std::process::Stdio::null());
        let out = process::run(cmd, &Limits::timeout(self.timeout)).map_err(|e| {
            Error::Worker(format!(
                "could not start Python worker ({}) — is harness-lock installed for `{}`? ({e})",
                self.module, self.interp
            ))
        })?;

        if out.timed_out {
            return Err(Error::Worker(format!(
                "python worker timed out after {}s (set HLOCK_WORKER_TIMEOUT to allow longer)",
                self.timeout.as_secs()
            )));
        }
        if !out.success {
            let status = out.code.map(|c| format!("exit status: {c}")).unwrap_or_else(|| "killed by a signal".into());
            return Err(Error::Worker(format!(
                "python worker failed ({status}): {}",
                out.stderr_text().trim()
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A worker whose "interpreter" is a shell script ignoring `-m module` and taking
    /// the request and response paths as `$3` and `$4`.
    fn fake(dir: &Path, body: &str, timeout: Duration) -> PythonWorker {
        let script = dir.join("fake-python");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        PythonWorker { interp: script.to_string_lossy().into_owned(), module: "m".into(), timeout }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_stale_response_is_not_reused() {
        let d = tmp("stale");
        std::fs::write(d.join("_al_response.json"), r#"{"version": 1, "ok": true, "result": {"success": true}}"#).unwrap();
        let w = fake(&d, "exit 0", Duration::from_secs(10));
        let err = w.call("grade", "devbox", None, None, 0, &d, &json!({})).unwrap_err().to_string();
        assert!(err.contains("did not write a response"), "{err}");
        // a real response is read, and the request carries the backend
        let w = fake(&d, r#"grep -q '"backend":"devbox"' "$3" && echo '{"version": 1, "ok": true, "result": {"success": true, "steps": 3}}' > "$4""#, Duration::from_secs(10));
        assert_eq!(w.call("grade", "devbox", None, None, 0, &d, &json!({})).unwrap().steps, 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_adapter_passes_its_backend() {
        assert_eq!(PythonAdapter::new().backend, Backend::Cluster);
        assert_eq!(PythonAdapter::with_backend(Backend::Devbox).backend, Backend::Devbox);
    }
}
