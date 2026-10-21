//! Backends: where `resolve` and `tune` run their work (see `specs/backends.md`).
//!
//! The Rust core stays the orchestrator and aggregator — manifests, search
//! planning, trial bookkeeping, evidence, and lock construction. The
//! ecosystem- and compute-heavy *work items* (real LLM invocation, user-trace
//! loading, synthetic-data generation, prompt search, LoRA fine-tuning) go to a
//! backend over a versioned JSON contract.
//!
//! `offline` runs the deterministic in-process reference harness (no backend
//! process). Every other backend is an [`Executor`]: the built-in `local` and
//! `flyte` backends and any Python backend run in hlock's Python worker
//! (`python -m hlock.tasks.worker <request.json> <response.json>`), and a
//! command backend (`[backends.<name>] command`, or `hlock-backend-<name>` on
//! `PATH`) is any executable that speaks the same contract. [`serve`] turns an
//! `Executor` written in Rust into such an executable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::adapters::{HarnessAdapter, RunOutcome};
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::process::{self, Limits};
use crate::search::Stack;
use crate::tasks::Task;

/// The built-in backends. `offline` runs in process; `local` and `flyte` run in hlock's
/// Python worker.
pub const BUILTIN: &[&str] = &["offline", "local", "flyte"];

/// Old backend names, still accepted for a release: `(old, new)`.
pub const RENAMED: &[(&str, &str)] = &[("devbox", "local"), ("cluster", "flyte")];

/// The work items hlock's stock handlers run. A backend lists the ones it runs in its
/// `backend.describe` answer.
pub const KINDS: &[&str] = &["llm.run_trial", "data.load", "data.synthetic", "grade", "prompt.search", "ft.lora"];

/// The work item that asks a backend what it runs.
pub const DESCRIBE: &str = "backend.describe";

/// The keys a `[backends.<name>]` table may have.
pub const BACKEND_DEF_KEYS: &[&str] = &["command", "python", "options"];

/// How a backend's work runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    /// The deterministic reference harness, in process: no backend process.
    Offline,
    /// hlock's Python worker, running a Python backend: built in or found by its
    /// `hlock.backends` entry point (`object: None`), or a `module:attr` object.
    Python { object: Option<String> },
    /// An executable that speaks the contract, given the request and response paths
    /// after its own arguments.
    Command(Vec<String>),
    /// An [`Executor`] a Rust program handed the resolver in process
    /// ([`crate::Resolver::with_executor`]).
    Embedded,
}

/// Where the resolver executes: a backend, resolved from its name.
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    pub name: String,
    /// The old name it was selected by (`devbox`, `cluster`).
    pub renamed_from: Option<String>,
    pub runner: Runner,
    /// `[backends.<name>] options`, sent with every work item.
    pub options: serde_json::Value,
}

impl Backend {
    /// The in-process reference harness.
    pub fn offline() -> Backend {
        Backend { name: "offline".into(), renamed_from: None, runner: Runner::Offline, options: json!({}) }
    }

    /// The backend `name` names, else the manifest's `[backend] target`: a built-in, a
    /// `[backends.<name>]` table, an `hlock-backend-<name>` executable on `PATH`, or else
    /// a Python backend the worker looks up by entry point (and reports missing when
    /// it's described).
    pub fn resolve(manifest: &Manifest, name: Option<&str>) -> Result<Backend> {
        let target = manifest.backend_target();
        let given = name.unwrap_or(&target).trim();
        let (name, renamed_from) = match RENAMED.iter().find(|(old, _)| old.eq_ignore_ascii_case(given)) {
            Some((old, new)) => (new.to_string(), Some(old.to_string())),
            None if BUILTIN.iter().any(|b| b.eq_ignore_ascii_case(given)) => (given.to_ascii_lowercase(), None),
            None => (given.to_string(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::Manifest(format!("backend {given:?} isn't a valid name: use letters, digits, '-' and '_'")));
        }
        let bad = |why: String| Error::Manifest(format!("[backends.{name}] {why}"));
        let def = match manifest.backends.get(&name) {
            Some(v) => Some(v.as_object().ok_or_else(|| bad("must be a table".into()))?),
            None => None,
        };
        let field = |k: &str| def.and_then(|d| d.get(k));
        let options = field("options").cloned().unwrap_or_else(|| json!({}));
        if !options.is_object() {
            return Err(bad("options must be a table".into()));
        }
        let command = match field("command") {
            None => None,
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(vec![s.clone()]),
            Some(serde_json::Value::Array(a)) if !a.is_empty() && a.iter().all(|v| v.as_str().is_some_and(|s| !s.is_empty())) => {
                Some(a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
            }
            Some(_) => return Err(bad("command must be a program, or a list of a program and its arguments".into())),
        };
        let object = match field("python") {
            None => None,
            Some(v) => match v.as_str().and_then(|s| s.split_once(':')).filter(|(m, a)| !m.is_empty() && !a.is_empty()) {
                Some(_) => v.as_str().map(String::from),
                None => return Err(bad("python must be \"module:attribute\"".into())),
            },
        };
        let runner = match name.as_str() {
            "offline" if def.is_some() => return Err(bad("can't be set: offline is built in and runs in process".into())),
            "offline" => Runner::Offline,
            "local" | "flyte" if command.is_some() || object.is_some() => return Err(bad(format!("can only set options: {name} is built in"))),
            "local" | "flyte" => Runner::Python { object: None },
            _ => match (command, object) {
                (Some(_), Some(_)) => return Err(bad("sets both command and python: keep one".into())),
                (Some(c), None) => Runner::Command(c),
                (None, Some(o)) => Runner::Python { object: Some(o) },
                (None, None) => match crate::profiles::Ctx::from_process().which(&format!("hlock-backend-{name}")) {
                    Some(p) => Runner::Command(vec![p.to_string_lossy().into_owned()]),
                    None => Runner::Python { object: None },
                },
            },
        };
        Ok(Backend { name, renamed_from, runner, options })
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// Whether this backend's work runs in a backend process (everything but `offline`).
    pub fn needs_worker(&self) -> bool {
        self.runner != Runner::Offline
    }

    /// The note for a backend selected by its old name.
    pub fn renamed_note(&self) -> Option<String> {
        self.renamed_from.as_ref().map(|old| format!("backend '{old}' is now '{}'; the old name will stop working in a future release", self.name))
    }

    /// How the backend was found, for `hlock backend status`.
    pub fn source(&self) -> String {
        match &self.runner {
            Runner::Offline => "built in, in process".into(),
            Runner::Python { object: None } if BUILTIN.contains(&self.name.as_str()) => "built in, Python worker".into(),
            Runner::Python { object: None } => format!("looked up as '{}' in the hlock.backends entry points", self.name),
            Runner::Python { object: Some(o) } => format!("Python object {o}"),
            Runner::Command(argv) if argv.len() == 1 && Path::new(&argv[0]).file_name().is_some_and(|f| f.to_string_lossy() == format!("hlock-backend-{}", self.name)) => {
                format!("hlock-backend-{} on PATH", self.name)
            }
            Runner::Command(argv) => format!("command {}", argv.join(" ")),
            Runner::Embedded => "in process (Rust)".into(),
        }
    }

    /// What, besides its name, decides the results this backend gives: its options.
    pub fn cache_identity(&self) -> String {
        format!("{}\0{}", self.name, self.options)
    }

    /// The process that runs this backend's work items, or `None` for `offline`.
    pub fn executor(&self) -> Option<ProcessExecutor> {
        match &self.runner {
            Runner::Offline | Runner::Embedded => None,
            Runner::Python { .. } => Some(ProcessExecutor::python()),
            Runner::Command(argv) => Some(ProcessExecutor::command(argv.clone())),
        }
    }
}

/// Current version of the backend contract. 2: a trial's `cost_usd` may be null
/// (unknown), and `simulated` says the offline reference ran it. 3: backends are named
/// (`backend_options`, `backend_object`) and answer `backend.describe`.
pub const CONTRACT_VERSION: u64 = 3;

/// One unit of work sent to a backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub version: u64,
    /// Handler id: `backend.describe | llm.run_trial | data.load | data.synthetic |
    /// grade | prompt.search | ft.lora`.
    pub kind: String,
    /// The backend's name.
    #[serde(default)]
    pub backend: String,
    /// `[backends.<name>] options`.
    #[serde(default)]
    pub backend_options: serde_json::Value,
    /// `[backends.<name>] python`: the `module:attr` the Python worker loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_object: Option<String>,
    #[serde(default)]
    pub stack: serde_json::Value,
    #[serde(default)]
    pub task: serde_json::Value,
    #[serde(default)]
    pub seed: u64,
    #[serde(default)]
    pub workdir: String,
    #[serde(default)]
    pub plan: serde_json::Value,
}

impl TaskRequest {
    /// A `kind` work item for `backend`, working in `workdir`.
    pub fn new(backend: &Backend, kind: &str, workdir: &Path) -> TaskRequest {
        TaskRequest {
            version: CONTRACT_VERSION,
            kind: kind.to_string(),
            backend: backend.name.clone(),
            backend_options: backend.options.clone(),
            backend_object: match &backend.runner {
                Runner::Python { object } => object.clone(),
                _ => None,
            },
            stack: json!({}),
            task: json!({}),
            seed: 0,
            workdir: workdir.to_string_lossy().to_string(),
            plan: json!({}),
        }
    }

    /// The trial's stack, task and seed.
    pub fn trial(mut self, stack: Option<&Stack>, task: Option<&Task>, seed: u64) -> TaskRequest {
        self.stack = stack.map(|s| s.to_value()).unwrap_or(json!({}));
        self.task = task
            .map(|t| {
                json!({
                    "id": t.id,
                    "prompt_text": t.prompt_text,
                    "expected": t.expected_artifacts(),
                    "fixtures": t.fixtures.as_ref().map(|p| p.to_string_lossy().to_string()),
                })
            })
            .unwrap_or(json!({}));
        self.seed = seed;
        self
    }

    /// The tune plan: depth, data, axes.
    pub fn plan(mut self, plan: &serde_json::Value) -> TaskRequest {
        self.plan = plan.clone();
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

/// A backend's answer to `backend.describe`: what it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Describe {
    #[serde(default)]
    pub name: String,
    /// The work items it runs ([`KINDS`]).
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Its trials call real models at `HL_LLM_BASE_URL`, so a non-`reference` candidate
    /// needs one set.
    #[serde(default)]
    pub model_endpoint: bool,
    /// One line for `hlock backend status`: its version, where it runs.
    #[serde(default)]
    pub about: String,
}

impl Describe {
    pub fn new<I, S>(name: &str, kinds: I) -> Describe
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Describe { name: name.to_string(), kinds: kinds.into_iter().map(Into::into).collect(), ..Default::default() }
    }

    pub fn about(mut self, about: impl Into<String>) -> Describe {
        self.about = about.into();
        self
    }

    pub fn model_endpoint(mut self, yes: bool) -> Describe {
        self.model_endpoint = yes;
        self
    }

    pub fn runs(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }
}

/// The response envelope. `result` is a [`TaskResult`], or a [`Describe`] for
/// `backend.describe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResponse {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Something that runs work items: a backend process ([`ProcessExecutor`]), or a Rust
/// backend in process. [`serve`] makes an executable out of one.
pub trait Executor: Send + Sync {
    /// What this backend runs (`backend.describe`).
    fn describe(&self, req: &TaskRequest) -> Result<Describe>;
    /// Run one work item.
    fn call(&self, req: &TaskRequest) -> Result<TaskResult>;
}

/// A backend, its executor, and what it said it runs (asked once).
pub struct Worker {
    pub backend: Backend,
    exec: Box<dyn Executor>,
    /// Where `backend.describe` works.
    scratch: PathBuf,
    described: OnceLock<std::result::Result<Describe, String>>,
}

/// The message of a worker error, without the variant's prefix.
fn message(e: Error) -> String {
    match e {
        Error::Worker(m) => m,
        e => e.to_string(),
    }
}

impl Worker {
    pub fn new(backend: Backend, exec: Box<dyn Executor>, scratch: impl Into<PathBuf>) -> Worker {
        Worker { backend, exec, scratch: scratch.into(), described: OnceLock::new() }
    }

    /// The worker for `backend`, or `None` for `offline`.
    pub fn for_backend(backend: &Backend, scratch: impl Into<PathBuf>) -> Option<Worker> {
        backend.executor().map(|e| Worker::new(backend.clone(), Box::new(e), scratch))
    }

    /// What the backend runs: asked once, the first time it's needed.
    pub fn describe(&self) -> Result<&Describe> {
        self.described
            .get_or_init(|| {
                std::fs::create_dir_all(&self.scratch).map_err(|e| e.to_string())?;
                self.exec.describe(&TaskRequest::new(&self.backend, DESCRIBE, &self.scratch)).map_err(message)
            })
            .as_ref()
            .map_err(|e| Error::Worker(e.clone()))
    }

    /// Whether the backend runs `kind`.
    pub fn runs(&self, kind: &str) -> Result<bool> {
        Ok(self.describe()?.runs(kind))
    }

    /// Why `kind` can't run here, when the backend doesn't run it.
    pub fn unsupported(&self, kind: &str) -> Result<Option<String>> {
        Ok((!self.runs(kind)?).then(|| format!("the {} backend doesn't run {kind}", self.backend.name)))
    }

    /// Run a `kind` work item in `workspace`.
    pub fn call(&self, kind: &str, stack: Option<&Stack>, task: Option<&Task>, seed: u64, workspace: &Path, plan: &serde_json::Value) -> Result<TaskResult> {
        self.exec.call(&TaskRequest::new(&self.backend, kind, workspace).trial(stack, task, seed).plan(plan))
    }

    /// Fail before any trial when the backend can't run one of `stacks`: it doesn't run
    /// trials, or its trials call `HL_LLM_BASE_URL` and that isn't set for a real model.
    pub fn preflight(&self, stacks: &[Stack]) -> Result<()> {
        if stacks.is_empty() {
            return Ok(());
        }
        if let Some(why) = self.unsupported("llm.run_trial")? {
            return Err(Error::Worker(why));
        }
        if !self.describe()?.model_endpoint {
            return Ok(());
        }
        let base_url = std::env::var("HL_LLM_BASE_URL").ok();
        match stacks.iter().find_map(|s| missing_endpoint(&self.backend.name, s, base_url.as_deref())) {
            Some(why) => Err(Error::Worker(why)),
            None => Ok(()),
        }
    }
}

/// A `HarnessAdapter` that delegates each trial to a backend (kind `llm.run_trial`).
pub struct WorkerAdapter {
    worker: Arc<Worker>,
}

impl WorkerAdapter {
    pub fn new(worker: Arc<Worker>) -> Self {
        WorkerAdapter { worker }
    }
}

impl HarnessAdapter for WorkerAdapter {
    fn name(&self) -> &str {
        "worker"
    }

    /// The backend writes the trial's result into `workspace`, which starts from the
    /// task's fixtures, and the resolver grades it there. A simulated success (the
    /// offline reference) leaves the task's solution, as it does on `offline`.
    fn run(&self, stack: &Stack, task: &Task, workspace: &Path, seed: u64) -> Result<RunOutcome> {
        self.worker.preflight(std::slice::from_ref(stack))?;
        if let Some(fixtures) = &task.fixtures {
            let src = pack_path(task, "fixtures", fixtures)?;
            if src.is_dir() {
                crate::fsutil::copy_tree(&src, workspace)?;
            }
        }
        let r = self.worker.call("llm.run_trial", Some(stack), Some(task), seed, workspace, &json!({}))?;
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

/// The message when a backend whose trials call `HL_LLM_BASE_URL` (`base_url`) has
/// nothing to call for `stack`: its planner is a real model and the endpoint isn't set.
/// `reference` candidates run on the offline reference without one.
fn missing_endpoint(backend: &str, stack: &Stack, base_url: Option<&str>) -> Option<String> {
    if base_url.is_some_and(|u| !u.trim().is_empty()) {
        return None;
    }
    let model = stack.llm.get("planner").cloned().unwrap_or_else(|| "reference:qwen-7b".into());
    let provider = model.split_once(':').map_or("reference", |(p, _)| p);
    (provider != "reference").then(|| format!("the {backend} backend needs a model endpoint for {model}: set HL_LLM_BASE_URL (see Backends)"))
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

/// Runs each work item in a process: hlock's Python worker, or a command backend. The
/// request and response are files in the item's `workdir`, named after the command.
pub struct ProcessExecutor {
    argv: Vec<String>,
    /// How errors name it: "python worker", "backend command `x`".
    label: String,
    /// How long one call may take (`HLOCK_WORKER_TIMEOUT` seconds, default an hour).
    timeout: Duration,
}

/// The default for `HLOCK_WORKER_TIMEOUT`, in seconds.
pub const DEFAULT_WORKER_TIMEOUT: u64 = 3600;

fn worker_timeout() -> Duration {
    let secs = std::env::var("HLOCK_WORKER_TIMEOUT").ok().and_then(|s| s.trim().parse::<u64>().ok()).filter(|s| *s > 0).unwrap_or(DEFAULT_WORKER_TIMEOUT);
    Duration::from_secs(secs)
}

impl ProcessExecutor {
    /// hlock's Python worker, in the interpreter `HL_PYTHON` names (else `python3`), so
    /// it can be pinned to the one that has `hlock` installed (e.g. a project venv).
    pub fn python() -> Self {
        let interp = std::env::var("HL_PYTHON").unwrap_or_else(|_| "python3".to_string());
        ProcessExecutor { argv: vec![interp, "-m".into(), "hlock.tasks.worker".into()], label: "python worker".into(), timeout: worker_timeout() }
    }

    /// A command backend: `argv`, then the request and response paths.
    pub fn command(argv: Vec<String>) -> Self {
        let label = format!("backend command `{}`", argv.first().map(String::as_str).unwrap_or_default());
        ProcessExecutor { argv, label, timeout: worker_timeout() }
    }

    /// Send `req` and return the response's `result`.
    fn exchange(&self, req: &TaskRequest) -> Result<serde_json::Value> {
        let workspace = Path::new(&req.workdir);
        let req_path = workspace.join("_al_request.json");
        let out_path = workspace.join("_al_response.json");
        std::fs::create_dir_all(workspace)?;
        std::fs::write(&req_path, serde_json::to_vec(req)?)?;
        // a response left from an earlier call must not pass for this one's
        match std::fs::remove_file(&out_path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }

        let (program, args) = self.argv.split_first().ok_or_else(|| Error::Worker("backend command is empty".into()))?;
        let mut cmd = std::process::Command::new(program);
        cmd.args(args).arg(&req_path).arg(&out_path).stdin(std::process::Stdio::null());
        let out = process::run(cmd, &Limits::timeout(self.timeout)).map_err(|e| {
            Error::Worker(if self.label == "python worker" {
                format!("could not start the Python worker (hlock.tasks.worker) — is hlock installed for `{program}`? ({e})")
            } else {
                format!("could not start {} ({e})", self.label)
            })
        })?;

        if out.timed_out {
            return Err(Error::Worker(format!("{} timed out after {}s (set HLOCK_WORKER_TIMEOUT to allow longer)", self.label, self.timeout.as_secs())));
        }
        // a failure the backend explained in its response is reported as it put it
        let resp = std::fs::read_to_string(&out_path).ok().and_then(|t| serde_json::from_str::<TaskResponse>(&t).ok());
        if let Some(TaskResponse { ok: false, error: Some(e), .. }) = &resp {
            return Err(Error::Worker(e.clone()));
        }
        if !out.success {
            let status = out.code.map(|c| format!("exit status: {c}")).unwrap_or_else(|| "killed by a signal".into());
            return Err(Error::Worker(format!("{} failed ({status}): {}", self.label, out.stderr_text().trim())));
        }
        let resp = match resp {
            Some(r) => r,
            None => {
                let text = std::fs::read_to_string(&out_path).map_err(|e| Error::Worker(format!("{} did not write a response file {}: {e}", self.label, out_path.display())))?;
                serde_json::from_str::<TaskResponse>(&text)?
            }
        };
        if !resp.ok {
            return Err(Error::Worker(resp.error.unwrap_or_else(|| format!("{} reported failure", self.label))));
        }
        resp.result.ok_or_else(|| Error::Worker(format!("{} returned no result", self.label)))
    }
}

impl Executor for ProcessExecutor {
    fn describe(&self, req: &TaskRequest) -> Result<Describe> {
        Ok(serde_json::from_value(self.exchange(req)?)?)
    }

    fn call(&self, req: &TaskRequest) -> Result<TaskResult> {
        Ok(serde_json::from_value(self.exchange(req)?)?)
    }
}

/// Run `exec` as a backend executable: read the request at the first argument, answer
/// it, write the response to the second, and return the exit status (0 when it
/// succeeded). A Rust backend's `main` is `std::process::exit(serve(&MyBackend))`.
pub fn serve(exec: &dyn Executor) -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [req, resp] => serve_files(exec, Path::new(req), Path::new(resp)),
        _ => {
            eprintln!("usage: {} <request.json> <response.json>", std::env::args().next().unwrap_or_default());
            2
        }
    }
}

/// [`serve`], given the request and response paths.
pub fn serve_files(exec: &dyn Executor, request: &Path, response: &Path) -> i32 {
    let fail = |error: String| TaskResponse { version: CONTRACT_VERSION, ok: false, result: None, error: Some(error) };
    let resp = match std::fs::read_to_string(request).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<TaskRequest>(&t).map_err(|e| e.to_string())) {
        Err(e) => fail(format!("bad request: {e}")),
        Ok(req) if req.version != CONTRACT_VERSION => fail(format!("unsupported contract version {}", req.version)),
        Ok(req) => {
            let result = if req.kind == DESCRIBE {
                exec.describe(&req).and_then(|d| Ok(serde_json::to_value(d)?))
            } else {
                exec.call(&req).and_then(|r| Ok(serde_json::to_value(r)?))
            };
            match result {
                Ok(v) => TaskResponse { version: CONTRACT_VERSION, ok: true, result: Some(v), error: None },
                Err(e) => fail(message(e)),
            }
        }
    };
    let ok = resp.ok;
    match serde_json::to_vec(&resp).map_err(std::io::Error::from).and_then(|b| std::fs::write(response, b)) {
        Ok(()) if ok => 0,
        Ok(()) => 1,
        Err(e) => {
            eprintln!("could not write the response {}: {e}", response.display());
            1
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A command backend: a shell script given the request and response paths as `$1`
    /// and `$2`. It describes itself as running `kinds`, and runs `body` for any other item.
    fn fake(dir: &Path, kinds: &str, body: &str, timeout: Duration) -> ProcessExecutor {
        let script = dir.join("fake-backend");
        let describe = format!(r#"{{"version": 3, "ok": true, "result": {{"name": "fake", "kinds": [{kinds}], "model_endpoint": true}}}}"#);
        std::fs::write(&script, format!("#!/bin/sh\nif grep -q '\"kind\":\"backend.describe\"' \"$1\"; then echo '{describe}' > \"$2\"; exit 0; fi\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut e = ProcessExecutor::command(vec![script.to_string_lossy().into_owned()]);
        e.timeout = timeout;
        e
    }

    const ALL: &str = r#""llm.run_trial", "data.load", "prompt.search""#;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn named(name: &str) -> Backend {
        Backend { name: name.into(), renamed_from: None, runner: Runner::Command(vec![]), options: json!({"k": 1}) }
    }

    fn worker(exec: ProcessExecutor, dir: &Path) -> Arc<Worker> {
        Arc::new(Worker::new(named("fake"), Box::new(exec), dir.join("describe")))
    }

    #[test]
    fn a_stale_response_is_not_reused() {
        let d = tmp("stale");
        std::fs::write(d.join("_al_response.json"), r#"{"version": 3, "ok": true, "result": {"success": true}}"#).unwrap();
        let w = worker(fake(&d, ALL, "exit 0", Duration::from_secs(10)), &d);
        let err = w.call("grade", None, None, 0, &d, &json!({})).unwrap_err().to_string();
        assert!(err.contains("did not write a response"), "{err}");
        // a real response is read, and the request carries the backend and its options
        let w = worker(fake(&d, ALL, r#"grep -q '"backend":"fake","backend_options":{"k":1}' "$1" && echo '{"version": 3, "ok": true, "result": {"success": true, "steps": 3}}' > "$2""#, Duration::from_secs(10)), &d);
        assert_eq!(w.call("grade", None, None, 0, &d, &json!({})).unwrap().steps, 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failure_is_reported_as_the_backend_put_it() {
        let d = tmp("fail");
        let w = worker(fake(&d, ALL, r#"echo '{"version": 3, "ok": false, "error": "ray is down"}' > "$2"; exit 1"#, Duration::from_secs(10)), &d);
        assert_eq!(w.call("grade", None, None, 0, &d, &json!({})).unwrap_err().to_string(), "backend error: ray is down");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn describe_is_asked_once_and_says_what_runs() {
        let d = tmp("describe");
        let w = worker(fake(&d, r#""llm.run_trial""#, "exit 1", Duration::from_secs(10)), &d);
        assert!(w.runs("llm.run_trial").unwrap());
        assert_eq!(w.unsupported("prompt.search").unwrap().unwrap(), "the fake backend doesn't run prompt.search");
        // the answer is kept: a broken backend afterwards doesn't change it
        std::fs::write(d.join("fake-backend"), "#!/bin/sh\nexit 1\n").unwrap();
        assert!(w.describe().unwrap().model_endpoint);
        // a backend that can't describe itself fails the same way every time
        let broken = worker(ProcessExecutor::command(vec![d.join("missing").to_string_lossy().into_owned()]), &d);
        let e1 = broken.describe().unwrap_err().to_string();
        assert!(e1.contains("could not start backend command"), "{e1}");
        assert_eq!(broken.describe().unwrap_err().to_string(), e1);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn stack(planner: &str) -> Stack {
        let mut s = Stack::default();
        s.llm.insert("planner".into(), planner.into());
        s
    }

    #[test]
    fn a_real_model_needs_an_endpoint_when_the_backend_calls_one() {
        let real = stack("anthropic:claude-sonnet-5-5");
        let why = missing_endpoint("local", &real, None).unwrap();
        assert_eq!(why, "the local backend needs a model endpoint for anthropic:claude-sonnet-5-5: set HL_LLM_BASE_URL (see Backends)");
        assert!(missing_endpoint("local", &real, Some("  ")).is_some());
        assert!(missing_endpoint("local", &real, Some("http://localhost:8000/v1")).is_none());
        // the offline reference needs nothing
        assert!(missing_endpoint("local", &stack("reference:qwen-7b"), None).is_none());
        // a backend that serves its own models isn't held to it, and one without trials fails
        let d = tmp("preflight");
        let own = d.join("own");
        std::fs::create_dir_all(&own).unwrap();
        let w = Arc::new(Worker::new(named("own"), Box::new(fake(&own, r#""llm.run_trial""#, "exit 1", Duration::from_secs(10))), own.join("describe")));
        std::fs::write(own.join("fake-backend"), "#!/bin/sh\necho '{\"version\": 3, \"ok\": true, \"result\": {\"kinds\": [\"llm.run_trial\"]}}' > \"$2\"\n").unwrap();
        assert!(w.preflight(&[real.clone()]).is_ok());
        let none = worker(fake(&d, r#""ft.lora""#, "exit 1", Duration::from_secs(10)), &d);
        assert_eq!(none.preflight(&[real]).unwrap_err().to_string(), "backend error: the fake backend doesn't run llm.run_trial");
        let _ = std::fs::remove_dir_all(&d);
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
        // the backend sees the fixtures in its workspace before it answers
        let respond = |simulated: bool| format!(r#"test -f "$(dirname "$1")/in.txt" && echo '{{"version": 3, "ok": true, "result": {{"success": true, "simulated": {simulated}, "cost_usd": null}}}}' > "$2""#);
        let a = WorkerAdapter::new(worker(fake(&d, ALL, &respond(true), Duration::from_secs(10)), &d));
        let out = a.run(&stack("reference:qwen-7b"), &t, &ws, 0).unwrap();
        assert!(out.success && out.cost_usd == 0.0);
        assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "done");
        // a real model's run leaves only what it wrote
        let ws2 = d.join("ws2");
        std::fs::create_dir_all(&ws2).unwrap();
        let a = WorkerAdapter::new(worker(fake(&d, ALL, &respond(false), Duration::from_secs(10)), &d));
        assert!(a.run(&stack("reference:qwen-7b"), &t, &ws2, 0).unwrap().success);
        assert!(ws2.join("in.txt").is_file() && !ws2.join("out.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn manifest(toml: &str) -> Manifest {
        Manifest::from_str(toml).unwrap()
    }

    #[test]
    fn backends_are_resolved_by_name() {
        let m = manifest("");
        assert_eq!(Backend::resolve(&m, None).unwrap(), Backend::offline());
        assert_eq!(Backend::resolve(&m, Some("local")).unwrap().runner, Runner::Python { object: None });
        // the old names still work, and say so
        let b = Backend::resolve(&m, Some("devbox")).unwrap();
        assert_eq!((b.name.as_str(), b.renamed_note().unwrap().as_str()), ("local", "backend 'devbox' is now 'local'; the old name will stop working in a future release"));
        assert_eq!(Backend::resolve(&m, Some("Cluster")).unwrap().name, "flyte");
        // a name nothing defines is left for the Python worker's entry points
        assert_eq!(Backend::resolve(&m, Some("ray")).unwrap().runner, Runner::Python { object: None });
        assert!(Backend::resolve(&m, Some("../x")).unwrap_err().to_string().contains("isn't a valid name"));
        let m = manifest(
            "[backend]\ntarget = \"ray\"\n[backends.ray]\ncommand = [\"ray-backend\", \"-v\"]\noptions = { address = \"auto\" }\n\
             [backends.modal]\npython = \"hlock_modal:ModalBackend\"\n[backends.flyte]\noptions = { project = \"p\" }\n",
        );
        let ray = Backend::resolve(&m, None).unwrap();
        assert_eq!((ray.runner.clone(), ray.options.clone()), (Runner::Command(vec!["ray-backend".into(), "-v".into()]), json!({"address": "auto"})));
        let modal = Backend::resolve(&m, Some("modal")).unwrap();
        assert_eq!(TaskRequest::new(&modal, "grade", Path::new("/w")).backend_object.as_deref(), Some("hlock_modal:ModalBackend"));
        assert_eq!(Backend::resolve(&m, Some("flyte")).unwrap().options, json!({"project": "p"}));
        for (toml, why) in [
            ("[backends.offline]\noptions = {}\n", "offline is built in"),
            ("[backends.local]\ncommand = \"x\"\n", "can only set options"),
            ("[backends.x]\ncommand = \"x\"\npython = \"a:b\"\n", "keep one"),
            ("[backends.x]\npython = \"nocolon\"\n", "module:attribute"),
            ("[backends.x]\ncommand = []\n", "command must be"),
            ("[backends.x]\noptions = 3\n", "options must be a table"),
        ] {
            let name = toml.split(['.', ']']).nth(1).unwrap();
            let e = Backend::resolve(&manifest(toml), Some(name)).unwrap_err().to_string();
            assert!(e.contains(why), "{toml}: {e}");
        }
    }

    #[test]
    fn an_executable_on_path_is_a_backend() {
        let d = tmp("path");
        let exe = d.join("hlock-backend-pathy");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![d.clone()];
        paths.extend(std::env::split_paths(&old));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let b = Backend::resolve(&manifest(""), Some("pathy"));
        std::env::set_var("PATH", old);
        assert_eq!(b.unwrap().runner, Runner::Command(vec![exe.to_string_lossy().into_owned()]));
        let _ = std::fs::remove_dir_all(&d);
    }

    struct Echo;

    impl Executor for Echo {
        fn describe(&self, req: &TaskRequest) -> Result<Describe> {
            Ok(Describe::new(&req.backend, ["grade"]).about("echo"))
        }

        fn call(&self, req: &TaskRequest) -> Result<TaskResult> {
            match req.kind.as_str() {
                "grade" => Ok(TaskResult { success: true, steps: req.seed, ..Default::default() }),
                k => Err(Error::Worker(format!("echo doesn't run {k}"))),
            }
        }
    }

    #[test]
    fn serve_answers_the_contract() {
        let d = tmp("serve");
        let (req, resp) = (d.join("req.json"), d.join("resp.json"));
        let ask = |r: &TaskRequest| {
            std::fs::write(&req, serde_json::to_vec(r).unwrap()).unwrap();
            let code = serve_files(&Echo, &req, &resp);
            (code, serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&resp).unwrap()).unwrap())
        };
        let b = named("echo");
        let (code, v) = ask(&TaskRequest::new(&b, DESCRIBE, &d));
        assert_eq!((code, v["result"]["kinds"].clone(), v["result"]["about"].clone()), (0, json!(["grade"]), json!("echo")));
        let (code, v) = ask(&TaskRequest::new(&b, "grade", &d).trial(None, None, 7));
        assert_eq!((code, v["ok"].clone(), v["result"]["steps"].clone()), (0, json!(true), json!(7)));
        let (code, v) = ask(&TaskRequest::new(&b, "ft.lora", &d));
        assert_eq!((code, v["error"].clone()), (1, json!("echo doesn't run ft.lora")));
        let mut old = TaskRequest::new(&b, "grade", &d);
        old.version = 2;
        let (code, v) = ask(&old);
        assert_eq!((code, v["error"].clone()), (1, json!("unsupported contract version 2")));
        std::fs::write(&req, "nope").unwrap();
        assert_eq!(serve_files(&Echo, &req, &resp), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
