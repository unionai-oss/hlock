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

/// Current version of the Rust <-> Python task contract. 2: a trial's `cost_usd` may be
/// null (unknown), and `simulated` says the offline reference ran it.
pub const CONTRACT_VERSION: u64 = 2;

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
    /// `None` when the cost isn't known (the endpoint reported no price), never a
    /// made-up 0.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// The offline reference ran it, not a model: a simulated success.
    #[serde(default)]
    pub simulated: bool,
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

    /// The worker writes the trial's result into `workspace`, which starts from the task's
    /// fixtures, and the resolver grades it there. A simulated success (the offline
    /// reference) leaves the task's solution, as it does on `local`.
    fn run(&self, stack: &Stack, task: &Task, workspace: &Path, seed: u64) -> Result<RunOutcome> {
        preflight(self.backend, std::slice::from_ref(stack))?;
        if let Some(fixtures) = &task.fixtures {
            let src = pack_path(task, "fixtures", fixtures)?;
            if src.is_dir() {
                crate::fsutil::copy_tree(&src, workspace)?;
            }
        }
        let r = self
            .worker
            .call("llm.run_trial", self.backend.as_str(), Some(stack), Some(task), seed, workspace, &json!({}))?;
        if r.simulated && r.success {
            leave_solution(task, workspace)?;
        }
        Ok(RunOutcome {
            success: r.success,
            // an unknown cost counts as nothing in the totals; the trace keeps it null
            cost_usd: r.cost_usd.unwrap_or(0.0),
            latency_s: r.latency_s,
            steps: r.steps,
            trace: r.trace,
        })
    }
}

/// The message when a worker backend has nothing to call for `stack`: its planner is a
/// real model and `HL_LLM_BASE_URL` (`base_url`) isn't set. `reference` candidates run
/// on the offline reference without one.
fn missing_endpoint(backend: Backend, stack: &Stack, base_url: Option<&str>) -> Option<String> {
    if !backend.needs_worker() || base_url.is_some_and(|u| !u.trim().is_empty()) {
        return None;
    }
    let model = stack.llm.get("planner").cloned().unwrap_or_else(|| "reference:qwen-7b".into());
    let provider = model.split_once(':').map_or("reference", |(p, _)| p);
    (provider != "reference").then(|| format!("the {} backend needs a model endpoint for {model}: set HL_LLM_BASE_URL (see Backends)", backend.as_str()))
}

/// Fail before any trial when `backend` can't run one of `stacks`: on `devbox` and
/// `cluster`, a real candidate's model is called at `HL_LLM_BASE_URL`, so without it there
/// is nothing to call (and every trial would fail). Call it before the first trial;
/// [`PythonAdapter::run`] checks each trial too.
pub fn preflight(backend: Backend, stacks: &[Stack]) -> Result<()> {
    let base_url = std::env::var("HL_LLM_BASE_URL").ok();
    match stacks.iter().find_map(|s| missing_endpoint(backend, s, base_url.as_deref())) {
        Some(why) => Err(Error::Worker(why)),
        None => Ok(()),
    }
}

/// A path the task names in its pack: relative, and inside it.
fn pack_path(task: &Task, what: &str, rel: &Path) -> Result<std::path::PathBuf> {
    let dir = task.pack_dir.as_ref().ok_or_else(|| Error::EvalPack(format!("task {} has no pack directory", task.id)))?;
    if !crate::fsutil::safe_rel(&rel.to_string_lossy()) {
        return Err(Error::EvalPack(format!("task {}: {what} {} must be a relative path inside the pack", task.id, rel.display())));
    }
    Ok(dir.join(rel))
}

/// What a simulated success leaves: the task's solution, else a stand-in for each
/// expected artifact (as the reference adapter does on `local`).
fn leave_solution(task: &Task, workspace: &Path) -> Result<()> {
    if let Some(sol) = &task.solution {
        let src = pack_path(task, "solution", sol)?;
        if src.is_dir() {
            crate::fsutil::copy_tree(&src, workspace)?;
            return Ok(());
        }
    }
    for art in task.expected_artifacts() {
        if !crate::fsutil::safe_rel(&art) {
            continue;
        }
        let out = workspace.join(&art);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fsutil::clear_symlink(&out)?;
        std::fs::write(&out, format!("artifact for {}", task.id))?;
    }
    Ok(())
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

    fn stack(planner: &str) -> Stack {
        let mut s = Stack::default();
        s.llm.insert("planner".into(), planner.into());
        s
    }

    #[test]
    fn a_real_model_needs_an_endpoint_on_a_worker_backend() {
        let real = stack("anthropic:claude-sonnet-5-5");
        let why = missing_endpoint(Backend::Devbox, &real, None).unwrap();
        assert_eq!(why, "the devbox backend needs a model endpoint for anthropic:claude-sonnet-5-5: set HL_LLM_BASE_URL (see Backends)");
        assert!(missing_endpoint(Backend::Devbox, &real, Some("  ")).is_some());
        assert!(missing_endpoint(Backend::Devbox, &real, Some("http://localhost:8000/v1")).is_none());
        // the offline reference needs nothing, and `local` never calls the worker
        assert!(missing_endpoint(Backend::Devbox, &stack("reference:qwen-7b"), None).is_none());
        assert!(missing_endpoint(Backend::Local, &real, None).is_none());
    }

    /// A task in a pack at `dir`, with fixtures and a solution.
    fn task(dir: &Path) -> Task {
        std::fs::create_dir_all(dir.join("t/workspace")).unwrap();
        std::fs::create_dir_all(dir.join("t/solution")).unwrap();
        std::fs::write(dir.join("t/workspace/in.txt"), "input").unwrap();
        std::fs::write(dir.join("t/solution/out.txt"), "done").unwrap();
        Task {
            id: "t".into(),
            fixtures: Some("t/workspace".into()),
            solution: Some("t/solution".into()),
            expected: [("artifacts".to_string(), json!(["out.txt"]))].into(),
            pack_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn the_worker_starts_from_the_fixtures_and_a_simulated_success_leaves_the_solution() {
        let d = tmp("adapter");
        let t = task(&d.join("pack"));
        let ws = d.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // the worker sees the fixtures in its workspace before it answers
        let respond = |simulated: bool| format!(r#"test -f "$(dirname "$3")/in.txt" && echo '{{"version": 2, "ok": true, "result": {{"success": true, "simulated": {simulated}, "cost_usd": null}}}}' > "$4""#);
        let a = PythonAdapter { worker: fake(&d, &respond(true), Duration::from_secs(10)), backend: Backend::Devbox };
        let out = a.run(&stack("reference:qwen-7b"), &t, &ws, 0).unwrap();
        assert!(out.success && out.cost_usd == 0.0);
        assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "done");
        // a real model's run leaves only what it wrote
        let ws2 = d.join("ws2");
        std::fs::create_dir_all(&ws2).unwrap();
        let a = PythonAdapter { worker: fake(&d, &respond(false), Duration::from_secs(10)), backend: Backend::Devbox };
        assert!(a.run(&stack("reference:qwen-7b"), &t, &ws2, 0).unwrap().success);
        assert!(ws2.join("in.txt").is_file() && !ws2.join("out.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
