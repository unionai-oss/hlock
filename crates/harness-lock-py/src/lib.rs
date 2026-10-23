//! PyO3 Python bindings for **harness-lock**.
//!
//! Exposes the same user-facing API as the (reference) pure-Python `hlock`
//! package, backed by the Rust core (`harness-lock-core`): `Manifest`,
//! `SearchSpace`, `Objective`, `Budget`, `lock()`, `load()`, and the `Lockfile`
//! model.
//!
//! Dict-shaped data is exchanged as canonical JSON strings to keep the binding
//! surface small and robust.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use harness_lock_core as core;

// Errors from the core, by kind. All are ValueErrors, so `except ValueError` still
// catches them; I/O errors are raised as the matching OSError (FileNotFoundError, …).
create_exception!(hlock, HarnessLockError, PyValueError, "An error from hlock. A ValueError.");
create_exception!(hlock, ManifestError, HarnessLockError, "harness.toml is missing or invalid.");
create_exception!(hlock, LockfileError, HarnessLockError, "A lock is missing, invalid, or refused.");
create_exception!(hlock, EvalPackError, HarnessLockError, "An eval pack is missing or invalid.");
create_exception!(hlock, StoreError, HarnessLockError, "The content-addressed store is missing a blob.");
create_exception!(hlock, TuningError, HarnessLockError, "Tuning found no viable stack, or failed.");
create_exception!(hlock, ProfileError, HarnessLockError, "A harness or runtime profile, a selection, or a step failed.");
create_exception!(hlock, WorkerError, HarnessLockError, "The Python worker failed.");
create_exception!(hlock, PullError, HarnessLockError, "`hlock pull`: a source can't be read, or git failed.");
create_exception!(hlock, DependencyError, HarnessLockError, "A declared dependency can't be read, resolved, fetched or installed.");
create_exception!(hlock, AuditError, HarnessLockError, "Hidden characters, an executable not yet trusted, or a policy violation stopped a step.");
create_exception!(hlock, RegistryError, HarnessLockError, "A registry isn't configured or reachable, or it refused the request.");

fn py_err(e: core::Error) -> PyErr {
    use core::Error as E;
    let msg = e.to_string();
    match e {
        E::Io(io) => PyErr::from(io),
        E::Manifest(_) => ManifestError::new_err(msg),
        E::Lockfile(_) => LockfileError::new_err(msg),
        E::EvalPack(_) => EvalPackError::new_err(msg),
        E::Store(_) => StoreError::new_err(msg),
        E::Tuning(_) => TuningError::new_err(msg),
        E::Profile(_) => ProfileError::new_err(msg),
        E::Worker(_) => WorkerError::new_err(msg),
        E::Pull(_) => PullError::new_err(msg),
        E::Dependency(_) => DependencyError::new_err(msg),
        E::Audit(_) => AuditError::new_err(msg),
        E::Registry(_) => RegistryError::new_err(msg),
        E::Toml(_) | E::Json(_) => HarnessLockError::new_err(msg),
    }
}

/// A JSON argument that doesn't parse.
fn json_arg_err(what: &str, e: serde_json::Error) -> PyErr {
    HarnessLockError::new_err(format!("{what} isn't valid JSON: {e}"))
}

/// How often a long call checks for Ctrl-C.
const SIGNAL_POLL: Duration = Duration::from_millis(100);

/// Wait up to [`SIGNAL_POLL`] for a result, without the GIL. (A `Receiver` isn't `Sync`,
/// so it moves through `allow_threads` and back.)
fn wait<T: Send>(py: Python<'_>, rx: mpsc::Receiver<T>) -> (Result<T, mpsc::RecvTimeoutError>, mpsc::Receiver<T>) {
    py.allow_threads(move || (rx.recv_timeout(SIGNAL_POLL), rx))
}

/// Run `work` on its own thread, without the GIL, and wait for it while checking for
/// Ctrl-C. On KeyboardInterrupt (or another signal handler's exception), stop every
/// child process the core is waiting on, so the work ends promptly, and raise it.
fn interruptible<T: Send + 'static>(py: Python<'_>, work: impl FnOnce() -> PyResult<T> + Send + 'static) -> PyResult<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut rx = rx;
    loop {
        let got;
        (got, rx) = wait(py, rx);
        match got {
            Ok(r) => return r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(e) = py.check_signals() {
                    core::process::stop_all();
                    return Err(e);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessLockError::new_err("hlock's core stopped unexpectedly (a panic); please report it")),
        }
    }
}

// --------------------------------------------------------------------------- //
// Manifest
// --------------------------------------------------------------------------- //
/// A parsed `harness.toml`.
#[pyclass(module = "hlock", name = "Manifest")]
#[derive(Clone)]
pub struct PyManifest {
    inner: core::Manifest,
}

#[pymethods]
impl PyManifest {
    #[staticmethod]
    fn load(path: &str) -> PyResult<PyManifest> {
        core::Manifest::load(Path::new(path)).map(|inner| PyManifest { inner }).map_err(py_err)
    }

    #[staticmethod]
    fn from_str(source: &str) -> PyResult<PyManifest> {
        core::Manifest::from_str(source).map(|inner| PyManifest { inner }).map_err(py_err)
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name()
    }

    #[getter]
    fn backend(&self) -> String {
        self.inner.backend_target()
    }

    /// `[backends]`, as JSON: each backend the project defines or configures.
    fn backends(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner.backends).map_err(|e| py_err(e.into()))
    }

    fn optimizers(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner.optimizers).map_err(|e| py_err(e.into()))
    }

    fn manifest_hash(&self) -> String {
        self.inner.manifest_hash()
    }

    fn eval_packs(&self) -> Vec<String> {
        self.inner.eval_packs()
    }

    /// The old name for `eval_packs`.
    fn task_packs(&self) -> Vec<String> {
        self.inner.eval_packs()
    }

    /// `[target] harness`, if set.
    #[getter]
    fn target_harness(&self) -> Option<String> {
        self.inner.target_harness()
    }

    /// The model `[deciders.<use>]` names, else `[deciders.default]`'s (also when the
    /// use has only `any-of` candidates); raises on an invalid `[deciders]` table.
    fn decider(&self, use_: &str) -> PyResult<Option<String>> {
        let specs = self.inner.deciders().map_err(py_err)?;
        Ok(spec_with_model(&specs, Some(use_)).and_then(|s| s.model.clone()))
    }

    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner.evals).map_err(|e| py_err(e.into()))
    }
}

// --------------------------------------------------------------------------- //
// Objective / Budget
// --------------------------------------------------------------------------- //
#[pyclass(module = "hlock", name = "Objective")]
#[derive(Clone)]
pub struct PyObjective {
    primary: String,
    constraints: std::collections::BTreeMap<String, String>,
    select: String,
}

#[pymethods]
impl PyObjective {
    #[new]
    #[pyo3(signature = (primary="success-rate".to_string(), select="min-cost".to_string(), constraints=None))]
    fn new(
        primary: String,
        select: String,
        constraints: Option<std::collections::BTreeMap<String, String>>,
    ) -> Self {
        PyObjective {
            primary,
            constraints: constraints.unwrap_or_default(),
            select,
        }
    }

    #[getter]
    fn primary(&self) -> String {
        self.primary.clone()
    }
    #[getter]
    fn select(&self) -> String {
        self.select.clone()
    }
    #[getter]
    fn constraints(&self) -> PyResult<String> {
        serde_json::to_string(&self.constraints).map_err(|e| py_err(e.into()))
    }
}

#[pyclass(module = "hlock", name = "Budget")]
#[derive(Clone)]
pub struct PyBudget {
    max_spend_usd: Option<f64>,
    max_trials: Option<usize>,
    strategy: String,
    random_seed: u64,
}

#[pymethods]
impl PyBudget {
    #[new]
    #[pyo3(signature = (max_spend_usd=None, max_trials=None, strategy="grid".to_string(), random_seed=0))]
    fn new(max_spend_usd: Option<f64>, max_trials: Option<usize>, strategy: String, random_seed: u64) -> Self {
        PyBudget {
            max_spend_usd,
            max_trials,
            strategy,
            random_seed,
        }
    }
}

impl PyBudget {
    pub(crate) fn to_core(&self) -> core::manifest::Budget {
        core::manifest::Budget {
            max_spend_usd: self.max_spend_usd,
            max_trials: self.max_trials,
            strategy: self.strategy.clone(),
            random_seed: self.random_seed,
        }
    }
}

// --------------------------------------------------------------------------- //
// SearchSpace
// --------------------------------------------------------------------------- //
#[pyclass(module = "hlock", name = "SearchSpace")]
#[derive(Clone)]
pub struct PySearchSpace {
    llm: std::collections::BTreeMap<String, Vec<String>>,
    decision: Vec<String>,
    harness: Vec<String>,
    policy: serde_json::Value,
    tools: serde_json::Value,
    skills: serde_json::Value,
    evals: serde_json::Value,
    objective: Option<PyObjective>,
    budget: Option<PyBudget>,
}

fn default_evals() -> serde_json::Value {
    serde_json::json!({"seeds": 2})
}

#[pymethods]
impl PySearchSpace {
    #[new]
    #[pyo3(signature = (llm=None, decision=None, harness=None, policy=None, tools=None, skills=None, objective=None, budget=None, evals=None, tasks=None))]
    fn new(
        llm: Option<std::collections::BTreeMap<String, Vec<String>>>,
        decision: Option<Vec<String>>,
        harness: Option<Vec<String>>,
        policy: Option<String>,
        tools: Option<String>,
        skills: Option<String>,
        objective: Option<PyObjective>,
        budget: Option<PyBudget>,
        evals: Option<String>,
        tasks: Option<String>,
    ) -> PyResult<Self> {
        // each is a JSON object; anything else is an error, not a silent default
        let parse = |what: &str, s: Option<String>, fallback: serde_json::Value| -> PyResult<serde_json::Value> {
            let Some(txt) = s else { return Ok(fallback) };
            match serde_json::from_str(&txt).map_err(|e| json_arg_err(what, e))? {
                v @ serde_json::Value::Object(_) => Ok(v),
                other => Err(HarnessLockError::new_err(format!("{what} must be a JSON object, not {other}"))),
            }
        };
        Ok(PySearchSpace {
            llm: llm.unwrap_or_default(),
            decision: decision.unwrap_or_default(),
            harness: harness.unwrap_or_default(),
            policy: parse("policy", policy, serde_json::json!({}))?,
            tools: parse("tools", tools, serde_json::json!({}))?,
            skills: parse("skills", skills, serde_json::json!({}))?,
            // `tasks` is the old name for `evals`
            evals: parse("evals", evals.or(tasks), default_evals())?,
            objective,
            budget,
        })
    }

    fn to_manifest(&self, name: &str) -> PyManifest {
        let m = to_core_manifest(name, self);
        PyManifest { inner: m }
    }
}

fn to_core_manifest(name: &str, sp: &PySearchSpace) -> core::Manifest {
    let mut candidates = core::manifest::Candidates::default();
    candidates.llm = sp
        .llm
        .iter()
        .map(|(k, v)| (k.clone(), core::manifest::LLMCandidate { any_of: v.clone() }))
        .collect();
    candidates.decision = sp.decision.clone();
    candidates.harness = sp
        .harness
        .iter()
        .map(|h| core::manifest::HarnessCandidate {
            name: h.clone(),
            pattern: vec![],
            config: Default::default(),
        })
        .collect();
    candidates.policy = match &sp.policy {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let list = match v {
                    serde_json::Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                (k.clone(), list)
            })
            .collect(),
        _ => Default::default(),
    };

    let objective = match &sp.objective {
        Some(o) => core::manifest::Objective {
            primary: o.primary.clone(),
            constraints: o.constraints.clone(),
            select: o.select.clone(),
        },
        None => core::manifest::Objective::default(),
    };
    let budget = sp
        .budget
        .as_ref()
        .map(|b| b.to_core())
        .unwrap_or_else(core::manifest::Budget::default);

    let mut project = std::collections::BTreeMap::new();
    project.insert("name".into(), serde_json::json!(name));
    core::Manifest {
        project,
        evals: object_of(&sp.evals),
        candidates,
        tools: object_of(&sp.tools),
        skills: object_of(&sp.skills),
        objective,
        budget,
        ..Default::default()
    }
}

fn object_of(v: &serde_json::Value) -> std::collections::BTreeMap<String, serde_json::Value> {
    match v {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        _ => Default::default(),
    }
}

// --------------------------------------------------------------------------- //
// Lockfile
// --------------------------------------------------------------------------- //
#[pyclass(module = "hlock", name = "Lockfile")]
pub struct PyLockfile {
    inner: core::Lockfile,
}

#[pymethods]
impl PyLockfile {
    #[staticmethod]
    fn load(path: &str) -> PyResult<PyLockfile> {
        core::Lockfile::load(Path::new(path)).map(|inner| PyLockfile { inner }).map_err(py_err)
    }

    fn summary(&self) -> String {
        self.inner.summary()
    }

    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner).map_err(|e| py_err(e.into()))
    }

    #[getter]
    fn manifest_hash(&self) -> Option<String> {
        self.inner.manifest_hash.clone()
    }

    #[getter]
    fn harness(&self) -> String {
        self.inner.harness.name.clone()
    }

    /// `"pin"` for locks written by `hlock pin`, `"tuned"` for tune locks, else `None`.
    #[getter]
    fn kind(&self) -> Option<String> {
        self.inner.kind.clone()
    }

    #[getter]
    fn harness_version(&self) -> Option<String> {
        self.inner.harness.version.clone()
    }

    #[getter]
    fn decision(&self) -> String {
        self.inner.decision.kind.clone()
    }

    #[getter]
    fn llm(&self) -> String {
        let pairs: Vec<(String, String)> = self
            .inner
            .llm
            .iter()
            .map(|l| (l.role.clone(), format!("{}:{}", l.provider, l.model)))
            .collect();
        serde_json::to_string(&pairs).unwrap_or_else(|_| "[]".into())
    }

    #[getter]
    fn evidence(&self) -> String {
        let mut m = serde_json::Map::new();
        m.insert("trials".into(), serde_json::json!(self.inner.evidence.trials));
        m.insert("eval_pack".into(), serde_json::json!(self.inner.evidence.eval_pack));
        m.insert("eval_pack_version".into(), serde_json::json!(self.inner.evidence.eval_pack_version));
        m.insert("eval_pack_digest".into(), serde_json::json!(self.inner.evidence.eval_pack_digest));
        m.insert("split".into(), serde_json::json!(self.inner.evidence.split));
        m.insert("simulated".into(), serde_json::json!(self.inner.evidence.simulated));
        m.insert(
            "success_rate".into(),
            serde_json::json!(self.inner.evidence.success_rate),
        );
        m.insert(
            "cost_usd_per_task".into(),
            serde_json::json!(self.inner.evidence.cost_usd_per_task),
        );
        m.insert(
            "latency_s_per_task".into(),
            serde_json::json!(self.inner.evidence.latency_s_per_task),
        );
        serde_json::to_string(&serde_json::Value::Object(m)).unwrap_or_else(|_| "{}".into())
    }

    #[getter]
    fn provenance(&self) -> String {
        self.inner.provenance.backend.clone()
    }
}

// --------------------------------------------------------------------------- //
// LockedAgent
// --------------------------------------------------------------------------- //
#[pyclass(module = "hlock", name = "LockedAgent")]
pub struct PyLockedAgent {
    lock: core::Lockfile,
}

#[pymethods]
impl PyLockedAgent {
    fn summary(&self) -> String {
        self.lock.summary()
    }

    /// Run the locked agent on a prompt using the Rust reference harness.
    /// Returns a JSON object with `success` / `model` / `cost_usd`.
    fn run(&self, prompt: &str) -> String {
        let model = self
            .lock
            .llm
            .first()
            .map(|l| l.model.clone())
            .unwrap_or_else(|| "qwen-7b".into());
        let provider = core::providers::ReferenceProvider::default();
        let r = provider.complete(&model, prompt.len() as u64);
        serde_json::json!({
            "success": r.content.starts_with("ANSWER_OK"),
            "model": model,
            "cost_usd": r.cost_usd,
        })
        .to_string()
    }
}

// --------------------------------------------------------------------------- //
// module functions
// --------------------------------------------------------------------------- //
/// Resolve the manifest and write the winning lock. `on_event`, if given, is called
/// with each progress event as a JSON string: `plan`, `trial`, `note`, and `done` (which
/// holds every stack's scores and the holdout check). `no_cache` re-runs every trial.
#[pyfunction]
#[pyo3(signature = (manifest=None, backend=None, workdir=None, harness=None, lock=None, on_event=None, no_cache=false))]
#[allow(clippy::too_many_arguments)]
fn resolve(
    py: Python<'_>,
    manifest: Option<PyObject>,
    backend: Option<String>,
    workdir: Option<String>,
    harness: Option<String>,
    lock: Option<String>,
    on_event: Option<PyObject>,
    no_cache: bool,
) -> PyResult<PyLockfile> {
    let core_manifest = resolve_manifest(py, manifest, backend.as_deref())?;
    let root = core_manifest.root();
    let wd = workdir.map(PathBuf::from).unwrap_or_else(|| root.join(".hlock"));
    let lock = lock.map(PathBuf::from).unwrap_or_else(|| root.join(core::LOCKFILE_PATH));
    let backend = core::Backend::resolve(&core_manifest, None).map_err(py_err)?;
    let lf = interruptible(py, move || {
        let mut resolver = core::Resolver::with_backend(core_manifest, &wd, backend);
        resolver.harness_override = harness;
        resolver.use_cache = !no_cache;
        if let Some(f) = on_event {
            if let Ok(mut slot) = resolver.on_event.lock() {
                *slot = Some(Box::new(move |e: serde_json::Value| {
                    Python::with_gil(|py| {
                        if let Err(err) = f.call1(py, (e.to_string(),)) {
                            err.print(py);
                        }
                    })
                }));
            }
        }
        let lf = resolver.resolve_lock().map_err(py_err)?;
        lf.save(&lock).map_err(py_err)?;
        Ok(lf)
    })?;
    Ok(PyLockfile { inner: lf })
}

/// What `resolve` would run, as JSON, without running anything: the stacks, the tasks in
/// each split, and the trial counts (`hlock resolve --dry-run`).
#[pyfunction]
#[pyo3(signature = (manifest=None, backend=None, workdir=None, harness=None))]
fn resolve_plan(py: Python<'_>, manifest: Option<PyObject>, backend: Option<String>, workdir: Option<String>, harness: Option<String>) -> PyResult<String> {
    let core_manifest = resolve_manifest(py, manifest, backend.as_deref())?;
    let wd = workdir.map(PathBuf::from).unwrap_or_else(|| core_manifest.root().join(".hlock"));
    let backend = core::Backend::resolve(&core_manifest, None).map_err(py_err)?;
    let mut resolver = core::Resolver::with_backend(core_manifest, &wd, backend);
    resolver.harness_override = harness;
    let notes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = notes.clone();
    if let Ok(mut slot) = resolver.on_event.lock() {
        *slot = Some(Box::new(move |e: serde_json::Value| {
            if let (Some(t), Ok(mut n)) = (e["text"].as_str(), sink.lock()) {
                n.push(t.to_string());
            }
        }));
    }
    let mut plan = resolver.plan().map_err(py_err)?;
    plan["notes"] = serde_json::json!(notes.lock().map(|n| n.clone()).unwrap_or_default());
    Ok(json_str(plan))
}

/// Adopt the tune lock at `path` as the lock at `lock`, keeping the old one as
/// `<lock>.prev` (`hlock tune apply`). Returns the `.prev` path, or None.
#[pyfunction]
#[pyo3(signature = (path="harness.tune.lock".to_string(), lock="harness.lock".to_string(), manifest=None))]
fn tune_apply(path: String, lock: String, manifest: Option<String>) -> PyResult<Option<String>> {
    let m = match manifest.as_deref().map(Path::new).filter(|p| p.is_file()) {
        Some(p) => Some(core::Manifest::load(p).map_err(py_err)?),
        None => None,
    };
    let prev = core::ops::tune_apply(Path::new(&path), Path::new(&lock), m.as_ref()).map_err(py_err)?;
    Ok(prev.map(|p| p.to_string_lossy().into_owned()))
}

/// Run a tune candidate on a split of a tune session (`hlock tune eval`): its trials,
/// graded, as JSON. Counts against the session's budget; never runs the test split.
#[pyfunction]
#[pyo3(signature = (session, candidate, split="train".to_string(), tasks=None, trials=None))]
fn tune_eval(py: Python<'_>, session: String, candidate: String, split: String, tasks: Option<Vec<String>>, trials: Option<u32>) -> PyResult<String> {
    interruptible(py, move || {
        let v = core::optimize::evaluate(Path::new(&session), Path::new(&candidate), &split, tasks.as_deref(), trials).map_err(py_err)?;
        Ok(json_str(v))
    })
}

/// The content behind the changed digests between two locks (`hlock diff`), as JSON.
/// `old` and `new` are lock files or git revisions; without them, `<lock>.prev` (or
/// `HEAD`'s lock) and `lock`.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), old=None, new=None, fetch=false))]
fn lock_diff(lock: String, old: Option<String>, new: Option<String>, fetch: bool) -> PyResult<String> {
    let lock = Path::new(&lock);
    let (o, n) = core::lockdiff::sides(lock, old.as_deref(), new.as_deref()).map_err(py_err)?;
    let project = core::fsutil::absolute(lock.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")));
    let ctx = core::profiles::Ctx::from_process();
    Ok(json_str(core::lockdiff::diff(&o, &n, &project, &core::lockdiff::DiffOptions { fetch }, &ctx)))
}

#[pyfunction]
fn load(path: &str) -> PyResult<PyLockedAgent> {
    core::Lockfile::load(Path::new(path))
        .map(|lock| PyLockedAgent { lock })
        .map_err(py_err)
}

fn resolve_manifest(py: Python<'_>, manifest: Option<PyObject>, backend: Option<&str>) -> PyResult<core::Manifest> {
    let mut m: core::Manifest = match manifest {
        Some(obj) => {
            if let Ok(pm) = obj.extract::<PyRef<PyManifest>>(py) {
                pm.inner.clone()
            } else if let Ok(sp) = obj.extract::<PyRef<PySearchSpace>>(py) {
                to_core_manifest("agent", &sp)
            } else if let Ok(s) = obj.extract::<String>(py) {
                // Accept either a path or raw TOML text. A single line with no `=` is a path,
                // so a missing harness.toml reads as "cannot read", not a TOML parse error.
                let p = Path::new(&s);
                if p.exists() || (!s.contains('\n') && !s.contains('=')) {
                    core::Manifest::load(p).map_err(py_err)?
                } else {
                    core::Manifest::from_str(&s).map_err(py_err)?
                }
            } else {
                return Err(ManifestError::new_err(
                    "manifest must be a hlock.Manifest, hlock.SearchSpace, a path, or TOML text",
                ));
            }
        }
        None => {
            // the nearest harness.toml at or above the working directory
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let path = core::find_manifest(&cwd).unwrap_or_else(|| cwd.join(core::MANIFEST_PATH));
            core::Manifest::load(&path).map_err(py_err)?
        }
    };
    if let Some(b) = backend {
        m.backend.insert("target".to_string(), serde_json::json!(b));
    }
    Ok(m)
}

#[pyfunction]
#[pyo3(signature = (manifest=None, to=None, all=false, apply=false, rollback=false, force=false, workdir=None, lock=None))]
fn upgrade(
    py: Python<'_>,
    manifest: Option<PyObject>,
    to: Option<String>,
    all: bool,
    apply: bool,
    rollback: bool,
    force: bool,
    workdir: Option<String>,
    lock: Option<String>,
) -> PyResult<String> {
    let m = resolve_manifest(py, manifest, None)?;
    let root = m.root();
    let spec = core::ops::UpgradeSpec {
        to,
        all,
        apply,
        rollback,
        force,
        workdir: workdir.map(PathBuf::from).unwrap_or_else(|| root.join(".hlock")),
        lock: Some(lock.map(PathBuf::from).unwrap_or_else(|| root.join(core::LOCKFILE_PATH))),
    };
    interruptible(py, move || {
        let o = core::ops::upgrade(m, &spec).map_err(py_err)?;
        Ok(serde_json::json!({
            "plan": o.plan,
            "applied": o.applied,
            "rolled_back": o.rolled_back,
            "before": o.before,
            "after": o.after,
            "lock_path": o.lock_path.to_string_lossy(),
            "message": o.message,
        })
        .to_string())
    })
}

#[pyfunction]
#[pyo3(signature = (manifest=None, depth="auto".to_string(), data="user".to_string(), harness=None, out=None, strict=false, apply=false, max_trials=None, backend=None, workdir=None, lock=None, optimizer=None, edit=None, budget_rollouts=None, budget_usd=None))]
fn tune(
    py: Python<'_>,
    manifest: Option<PyObject>,
    depth: String,
    data: String,
    harness: Option<String>,
    out: Option<String>,
    strict: bool,
    apply: bool,
    max_trials: Option<usize>,
    backend: Option<String>,
    workdir: Option<String>,
    lock: Option<String>,
    optimizer: Option<String>,
    edit: Option<Vec<String>>,
    budget_rollouts: Option<u64>,
    budget_usd: Option<f64>,
) -> PyResult<String> {
    let m = resolve_manifest(py, manifest, backend.as_deref())?;
    let root = m.root();
    let spec = core::ops::TuneSpec {
        optimizer,
        edit,
        budget_rollouts,
        budget_usd,
        depth,
        data,
        harness,
        out: out.map(PathBuf::from).unwrap_or_else(|| root.join("harness.tune.lock")),
        strict,
        apply,
        max_trials,
        backend,
        workdir: workdir.map(PathBuf::from).unwrap_or_else(|| root.join(".hlock")),
        lock: Some(lock.map(PathBuf::from).unwrap_or_else(|| root.join(core::LOCKFILE_PATH))),
    };
    interruptible(py, move || {
        let o = core::ops::tune(m, &spec).map_err(py_err)?;
        Ok(serde_json::json!({
            "path": o.path.to_string_lossy(),
            "report": o.report,
            "summary": o.lock.summary(),
        })
        .to_string())
    })
}

// --------------------------------------------------------------------------- //
// harness & runtime profiles
// --------------------------------------------------------------------------- //
/// The project: `manifest` (a path, existing or not) or the nearest `harness.toml`
/// at or above the working directory, else `fallback_root` (a lock's directory),
/// else the working directory. Returns the manifest if one exists, and a context
/// rooted at the project directory so project-relative paths resolve there.
fn project(manifest: Option<&str>, fallback_root: Option<&Path>) -> PyResult<(Option<core::Manifest>, core::profiles::Ctx)> {
    let proc = core::profiles::Ctx::from_process();
    let path = match manifest {
        Some(m) => Some(if Path::new(m).is_absolute() { PathBuf::from(m) } else { proc.cwd.join(m) }),
        None => core::find_manifest(&proc.cwd),
    };
    match path {
        Some(p) => {
            let root = p.parent().map(Path::to_path_buf).unwrap_or_else(|| proc.cwd.clone());
            let m = if p.is_file() { Some(core::Manifest::load(&p).map_err(py_err)?) } else { None };
            Ok((m, core::profiles::Ctx::at(&root)))
        }
        None => Ok((None, fallback_root.map(core::profiles::Ctx::at).unwrap_or(proc))),
    }
}

/// The directory a lock path lives in (absolute).
fn lock_dir(path: &str) -> PathBuf {
    let p = Path::new(path);
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    abs.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// The profile registry for a project: built-ins, profile dirs, and the manifest's
/// `[harnesses.<id>]` tables.
fn registry(manifest: Option<&str>, fallback_root: Option<&Path>) -> PyResult<(core::Registry, core::profiles::Ctx, Option<core::Manifest>)> {
    let (m, ctx) = project(manifest, fallback_root)?;
    let reg = core::Registry::load_for(&ctx, m.as_ref()).map_err(py_err)?;
    Ok((reg, ctx, m))
}

fn json_str(v: serde_json::Value) -> String {
    v.to_string()
}

/// Every known harness profile with what's installed here, as a JSON list.
/// Pass `probe=False` to skip running each harness's `--version`.
#[pyfunction]
#[pyo3(signature = (probe=true, manifest=None))]
fn harnesses(py: Python<'_>, probe: bool, manifest: Option<String>) -> PyResult<String> {
    // probing runs each harness's `--version`
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
        let rows: Vec<serde_json::Value> = reg
            .harnesses
            .iter()
            .map(|h| {
                let p = if probe {
                    core::profiles::probe_harness(h, &ctx)
                } else {
                    core::profiles::Probe { id: h.id.clone(), active: h.is_active(&ctx), ..Default::default() }
                };
                serde_json::json!({
                    "id": h.id, "name": h.name, "kind": h.kind, "autonomous": h.autonomous, "aliases": h.aliases, "support": h.support,
                    "origin": h.origin, "probe": p,
                })
            })
            .collect();
        Ok(json_str(serde_json::json!(rows)))
    })
}

/// Every known inference-runtime profile with what's installed here, as JSON.
#[pyfunction]
#[pyo3(signature = (probe=true, manifest=None))]
fn runtimes(py: Python<'_>, probe: bool, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
        let rows: Vec<serde_json::Value> = reg
            .runtimes
            .iter()
            .map(|r| {
                let p = if probe {
                    core::profiles::probe_runtime(r, &ctx)
                } else {
                    core::profiles::Probe { id: r.id.clone(), ..Default::default() }
                };
                serde_json::json!({
                    "id": r.id, "name": r.name, "aliases": r.aliases, "ports": r.ports,
                    "origin": r.origin, "models_dir": r.models_dir(&ctx), "probe": p,
                })
            })
            .collect();
        Ok(json_str(serde_json::json!(rows)))
    })
}

/// One harness profile (by id or alias), its probe, and the paths it resolves to here.
#[pyfunction]
#[pyo3(signature = (name, manifest=None))]
fn harness_profile(py: Python<'_>, name: String, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
        let h = reg.require_harness(&name).map_err(py_err)?;
        let root = h.config_root(&ctx);
        let files: serde_json::Map<String, serde_json::Value> = h
            .files
            .keys()
            .map(|id| {
                let p = h.file_path(id, &ctx).unwrap_or_default();
                (id.clone(), serde_json::json!({"path": p, "exists": p.exists()}))
            })
            .collect();
        let skills: Vec<serde_json::Value> = h
            .skills
            .dirs
            .iter()
            .map(|d| {
                let p = ctx.expand(d, Some(&root));
                serde_json::json!({"path": p, "exists": p.is_dir()})
            })
            .collect();
        Ok(json_str(serde_json::json!({
            "profile": h,
            "probe": core::profiles::probe_harness(h, &ctx),
            "paths": {"config_root": root, "files": files, "skills": skills},
            "isolation_env": h.config_root.env,
        })))
    })
}

/// Pick the target harness: `explicit`, then `HLOCK_HARNESS`, then
/// `manifest_target`, then the harness running this process, then the only
/// installed one. Returns JSON `{"id", "reason"}`.
#[pyfunction]
#[pyo3(signature = (explicit=None, manifest_target=None, manifest=None))]
fn select_harness(py: Python<'_>, explicit: Option<String>, manifest_target: Option<String>, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
        let sel = reg.select(explicit.as_deref(), manifest_target.as_deref(), &ctx).map_err(py_err)?;
        Ok(json_str(serde_json::json!(sel)))
    })
}

/// Capture the target harness's live setup into a pinned lock at `out`: the project's
/// skills, MCP servers, instructions and config, plus the user-level ones with `user`, or
/// only those whose path matches one of the `include` globs.
/// Returns JSON with the lock, the chosen harness, the diff from a previous pin,
/// vendored files, warnings, and the user-level items the pin left out.
#[pyfunction]
#[pyo3(signature = (harness=None, out="harness.lock".to_string(), vendor=true, force=false, manifest=None, user=false, include=None))]
#[allow(clippy::too_many_arguments)]
fn pin(py: Python<'_>, harness: Option<String>, out: String, vendor: bool, force: bool, manifest: Option<String>, user: bool, include: Option<Vec<String>>) -> PyResult<String> {
    let include = include.unwrap_or_default();
    let scope = match (user, include.is_empty()) {
        (true, false) => return Err(HarnessLockError::new_err("pass user or include, not both: user already includes everything")),
        (true, true) => core::capture::UserScope::All,
        (false, false) => core::capture::UserScope::Matching(include),
        (false, true) => core::capture::UserScope::None,
    };
    interruptible(py, move || {
        let (reg, ctx, manifest) = registry(manifest.as_deref(), Some(&lock_dir(&out)))?;
        let out = if Path::new(&out).is_absolute() { out } else { std::env::current_dir().unwrap_or_default().join(&out).to_string_lossy().into_owned() };
        let target = manifest.as_ref().and_then(|m| m.target_harness());
        let project = manifest.as_ref().and_then(|m| m.project.get("name").and_then(|v| v.as_str()).map(str::to_string));
        let o = core::capture::pin(&reg, &ctx, harness.as_deref(), target.as_deref(), project.as_deref(), Path::new(&out), vendor, force, scope)
            .map_err(py_err)?;
        Ok(json_str(serde_json::json!({
            "lock": o.lock,
            "path": o.path,
            "selection": o.selection,
            "diff": o.diff,
            "previous": o.previous,
            "vendored": o.vendored,
            "warnings": o.warnings,
            "skipped": o.skipped,
        })))
    })
}

/// Re-capture the harness a pinned lock is for; returns a JSON list of drift lines.
/// With `prefix`, re-capture the isolated root `hlock sync --prefix` writes instead.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), prefix=None, manifest=None))]
fn check_pin(py: Python<'_>, path: String, prefix: Option<String>, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
        let drift = core::capture::check_pin_at(&lock, &reg, &ctx, prefix.as_deref().map(Path::new)).map_err(py_err)?;
        Ok(json_str(serde_json::json!(drift)))
    })
}

/// Pull the lock at `source` (`github:…`, `gitlab:…`, `git:…`, or a repository URL) into
/// the store as `~/.hlock/pulls/<name>/harness.lock`, or into the project file `out`.
/// Returns JSON with the lock, its path and name, the source, the commit, the diff from a
/// lock it replaced, vendored files, other locks in the repo, and warnings.
#[pyfunction]
#[pyo3(signature = (source, out=None, name=None, force=false, allow_hidden=None))]
fn pull(py: Python<'_>, source: String, out: Option<String>, name: Option<String>, force: bool, allow_hidden: Option<Vec<String>>) -> PyResult<String> {
    let target = match out {
        Some(o) => core::pull::Target::File(std::env::current_dir().unwrap_or_default().join(o)),
        None => core::pull::Target::Store(name),
    };
    // fetching waits on the network
    interruptible(py, move || {
        let o = core::pull::pull_allowing(&source, &target, force, &allow_hidden.unwrap_or_default()).map_err(py_err)?;
        Ok(json_str(serde_json::to_value(&o).map_err(|e| py_err(e.into()))?))
    })
}

/// Parse a pull source (`github:…`, `gitlab:…`, `git:…`, a repository URL) the way `pull`
/// does, and return it as JSON (`spec`, `url`, `ref`, `path`, `owner`; credentials left
/// out). Raises `PullError` for a source `pull` would refuse.
/// The backend `backend` names (else the manifest's target), how it was found, and what
/// it says it runs, as JSON (`hlock backend status`). A backend that can't describe
/// itself has an `error` instead of `describe`.
#[pyfunction]
#[pyo3(signature = (manifest=None, backend=None, workdir=None))]
fn backend_describe(py: Python<'_>, manifest: Option<PyObject>, backend: Option<String>, workdir: Option<String>) -> PyResult<String> {
    // without a manifest, the defaults (the backend is still named by `backend`)
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let m = match manifest {
        None if core::find_manifest(&cwd).is_none() => core::Manifest::from_str("").map_err(py_err)?,
        manifest => resolve_manifest(py, manifest, None)?,
    };
    let b = core::Backend::resolve(&m, backend.as_deref()).map_err(py_err)?;
    let wd = workdir.map(PathBuf::from).unwrap_or_else(|| m.root().join(".hlock"));
    let mut out = serde_json::json!({
        "name": b.name,
        "renamed_from": b.renamed_from,
        "source": b.source(),
        "options": b.options,
    });
    if let Some(w) = core::worker::Worker::for_backend(&b, wd.join("backend")) {
        match py.allow_threads(|| w.describe().cloned()) {
            Ok(d) => out["describe"] = serde_json::to_value(d).map_err(|e| py_err(e.into()))?,
            Err(e) => out["error"] = serde_json::json!(match e {
                core::Error::Worker(m) => m,
                e => e.to_string(),
            }),
        }
    }
    to_json(&out)
}

/// The optimizer `optimizer` names (else the manifest's `[tune] optimizer`, else `hlock`)
/// and how it was found, as JSON (`hlock optimizer status`): `name`, `source`, `options`,
/// and the `object` or `command` that runs it.
#[pyfunction]
#[pyo3(signature = (manifest=None, optimizer=None))]
fn optimizer_describe(py: Python<'_>, manifest: Option<PyObject>, optimizer: Option<String>) -> PyResult<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let m = match manifest {
        None if core::find_manifest(&cwd).is_none() => core::Manifest::from_str("").map_err(py_err)?,
        manifest => resolve_manifest(py, manifest, None)?,
    };
    let name = optimizer.or_else(|| m.tune.get("optimizer").and_then(|v| v.as_str()).map(str::to_string)).unwrap_or_else(|| "hlock".into());
    let o = core::optimize::Optimizer::resolve(&m, &name).map_err(py_err)?;
    let mut out = o.to_value();
    out["source"] = serde_json::json!(o.source());
    to_json(&out)
}

#[pyfunction]
fn pull_parse(source: &str) -> PyResult<String> {
    let s = core::pull::parse(source).map_err(py_err)?;
    Ok(json_str(serde_json::to_value(&s).map_err(|e| py_err(e.into()))?))
}

/// The lock path of the stored pull `name`, or None.
#[pyfunction]
fn pull_find(name: &str) -> Option<String> {
    core::pull::find(name).map(|p| p.to_string_lossy().into_owned())
}

/// Every stored pull as a JSON list (`name`, `path`, `harness`, `version`, `pulled`, `error`).
#[pyfunction]
fn pull_list() -> PyResult<String> {
    Ok(json_str(serde_json::to_value(core::pull::list()).map_err(|e| py_err(e.into()))?))
}

/// Delete the stored pull `name` and its launch environments; returns the removed directory.
#[pyfunction]
fn pull_remove(name: &str) -> PyResult<String> {
    Ok(core::pull::remove(name).map_err(py_err)?.to_string_lossy().into_owned())
}

/// Where `launch` keeps isolated config roots for the stored pull `name`.
#[pyfunction]
fn pull_env(name: &str) -> String {
    core::pull::env_dir(name).to_string_lossy().into_owned()
}

// --------------------------------------------------------------------------- //
// The registry client (`hlock login`, `push`, `search`, `pull NS/NAME`)
// --------------------------------------------------------------------------- //

/// The registry to use (`registry`, else `HLOCK_REGISTRY`, else the one `login` saved),
/// normalized. Raises `RegistryError` when none is configured.
#[pyfunction]
#[pyo3(signature = (registry=None))]
fn registry_url(registry: Option<String>) -> PyResult<String> {
    core::registry::resolve_url(registry.as_deref()).map_err(py_err)
}

/// Where the registry's token comes from (`environment`, `keychain`, `credentials file`), or None.
#[pyfunction]
fn registry_token_source(url: &str) -> Option<String> {
    core::registry::token_for(url).map(|(_, s)| s.to_string())
}

/// `hlock login URL --token T`: check the token with the registry, then keep it and the URL.
#[pyfunction]
fn registry_login_token(py: Python<'_>, url: String, token: String) -> PyResult<String> {
    interruptible(py, move || to_json(&core::registry::login_with_token(&url, &token).map_err(py_err)?))
}

/// Start a device login; returns the registry's response as JSON.
#[pyfunction]
fn registry_device_start(py: Python<'_>, url: String) -> PyResult<String> {
    interruptible(py, move || {
        let url = core::registry::normalize_url(&url).map_err(py_err)?;
        to_json(&core::registry::Client::new(&url, None).device_start().map_err(py_err)?)
    })
}

/// Poll a device login once: `{"status": "pending" | "slow_down"}`, or, when it's done,
/// `{"status": "ok", "stored", "whoami"}` with the token kept and the URL saved.
#[pyfunction]
fn registry_device_poll(py: Python<'_>, url: String, device_code: String) -> PyResult<String> {
    interruptible(py, move || {
        let url = core::registry::normalize_url(&url).map_err(py_err)?;
        match core::registry::Client::new(&url, None).device_poll(&device_code).map_err(py_err)? {
            core::registry::Poll::Pending => to_json(&serde_json::json!({"status": "pending"})),
            core::registry::Poll::SlowDown => to_json(&serde_json::json!({"status": "slow_down"})),
            core::registry::Poll::Token(v) => {
                let token = v["access_token"].as_str().unwrap_or_default().to_string();
                let mut out = core::registry::login_with_token(&url, &token).map_err(py_err)?;
                out["status"] = serde_json::json!("ok");
                to_json(&out)
            }
        }
    })
}

/// Forget the registry's token. True if one was removed.
#[pyfunction]
fn registry_logout(url: &str) -> PyResult<bool> {
    core::registry::forget(url).map_err(py_err)
}

/// The registry's `whoami` for the saved token, as JSON.
#[pyfunction]
fn registry_whoami(py: Python<'_>, url: String) -> PyResult<String> {
    interruptible(py, move || to_json(&core::registry::Client::for_url(&url).whoami().map_err(py_err)?))
}

/// Search the registry; returns its JSON response.
#[pyfunction]
#[pyo3(signature = (url, query, r#type=None, kind=None, source=None, page=None, per_page=None))]
fn registry_search(py: Python<'_>, url: String, query: String, r#type: Option<String>, kind: Option<String>, source: Option<String>, page: Option<u32>, per_page: Option<u32>) -> PyResult<String> {
    interruptible(py, move || {
        let filters = [
            ("type", r#type.unwrap_or_default()),
            ("kind", kind.unwrap_or_default()),
            ("source", source.unwrap_or_default()),
            ("page", page.map(|p| p.to_string()).unwrap_or_default()),
            ("per_page", per_page.map(|p| p.to_string()).unwrap_or_default()),
        ];
        to_json(&core::registry::Client::for_url(&url).search(&query, &filters).map_err(py_err)?)
    })
}

/// `hlock push`: pack the setup in `dir` and publish it (or, with `dry_run`, say what
/// would be published). Returns JSON.
#[pyfunction]
#[pyo3(signature = (url, dir=".".to_string(), name=None, version=None, dry_run=false))]
fn registry_push(py: Python<'_>, url: String, dir: String, name: Option<String>, version: Option<String>, dry_run: bool) -> PyResult<String> {
    interruptible(py, move || to_json(&core::registry::push(&url, Path::new(&dir), name.as_deref(), version.as_deref(), dry_run).map_err(py_err)?))
}

/// `hlock pull NS/NAME[@VERSION]` (or a `registry:` spec): into the store as `name`, into
/// the project lock file `out`, or every file of the setup into the directory `into`.
#[pyfunction]
#[pyo3(signature = (spec, registry=None, out=None, name=None, into=None, force=false, allow_hidden=None))]
fn registry_pull(py: Python<'_>, spec: String, registry: Option<String>, out: Option<String>, name: Option<String>, into: Option<String>, force: bool, allow_hidden: Option<Vec<String>>) -> PyResult<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    interruptible(py, move || {
        let allow = allow_hidden.unwrap_or_default();
        let v = match into {
            Some(d) => {
                let d = cwd.join(d);
                core::registry::pull(registry.as_deref(), &spec, core::registry::Dest::Into(&d), force, &allow)
            }
            None => {
                let target = match out {
                    Some(o) => core::pull::Target::File(cwd.join(o)),
                    None => core::pull::Target::Store(name),
                };
                core::registry::pull(registry.as_deref(), &spec, core::registry::Dest::Target(&target), force, &allow)
            }
        }
        .map_err(py_err)?;
        to_json(&v)
    })
}

/// Pull the setup at a git source and write its files into the directory `into`.
#[pyfunction]
#[pyo3(signature = (source, into, force=false, allow_hidden=None))]
fn pull_into(py: Python<'_>, source: String, into: String, force: bool, allow_hidden: Option<Vec<String>>) -> PyResult<String> {
    let into = std::env::current_dir().unwrap_or_default().join(into);
    interruptible(py, move || {
        let o = core::pull::pull_into(&source, &into, force, &allow_hidden.unwrap_or_default()).map_err(py_err)?;
        Ok(json_str(serde_json::to_value(&o).map_err(|e| py_err(e.into()))?))
    })
}

/// True if `arg` is a registry reference (`NAMESPACE/NAME[@VERSION]`) and not a file.
#[pyfunction]
fn is_registry_ref(arg: &str) -> bool {
    core::registry::is_ref(arg)
}

/// True if `arg` names a remote source (`github:`, `gitlab:`, `git:`, a URL) rather than a lock path.
#[pyfunction]
fn is_pull_source(arg: &str) -> bool {
    core::pull::is_source(arg)
}

fn sync_options(prefix: Option<String>, in_place: bool, docker: Option<String>, install: bool, pull: bool, all_models: bool, exact: bool) -> core::sync::SyncOptions {
    core::sync::SyncOptions {
        prefix: prefix.map(PathBuf::from),
        in_place,
        docker: docker.map(PathBuf::from),
        install,
        pull,
        all_models,
        exact,
        inject: false,
        also: Vec::new(),
        allow_hidden: Vec::new(),
        trust: false,
        apps: false,
        default_prefix: false,
    }
}

/// `[target] also` from the project's manifest, when the lock is the project's (next to it).
fn also_harnesses(manifest: Option<&str>, lock: &str) -> Vec<String> {
    let Ok(Some(m)) = load_manifest(manifest) else { return Vec::new() };
    let same_dir = m.source_path.as_ref().and_then(|p| p.parent()).map(|d| core::fsutil::absolute(d)) == Some(lock_dir(lock));
    if !same_dir {
        return Vec::new();
    }
    m.target.get("also").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// The project's manifest when it exists (for the dependency functions).
fn project_manifest(path: &str) -> PyResult<Option<core::Manifest>> {
    let p = Path::new(path);
    if p.is_file() {
        Ok(Some(core::Manifest::load(p).map_err(py_err)?))
    } else {
        Ok(None)
    }
}

fn abs(path: &str) -> PathBuf {
    core::fsutil::absolute(Path::new(path))
}

fn to_json<T: serde::Serialize>(v: &T) -> PyResult<String> {
    serde_json::to_string(v).map_err(|e| py_err(e.into()))
}

/// Resolve and fetch a dependency for `hlock add` without changing the project: returns
/// the prepared dependency as JSON (what `--dry-run` shows, and what `deps_commit` takes).
#[pyfunction]
#[pyo3(signature = (source, kind=None, name=None, scope=None, harness=None, manifest="harness.toml".to_string(), lock="harness.lock".to_string(), allow_hidden=None))]
#[allow(clippy::too_many_arguments)]
fn deps_prepare(py: Python<'_>, source: String, kind: Option<String>, name: Option<String>, scope: Option<String>, harness: Option<String>, manifest: String, lock: String, allow_hidden: Option<Vec<String>>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        let req = core::deps::AddRequest { kind, source, name, scope, harness, allow_hidden: allow_hidden.unwrap_or_default() };
        let p = core::deps::prepare(&req, &abs(&manifest), &abs(&lock), &reg, &ctx).map_err(py_err)?;
        to_json(&p)
    })
}

/// Declare, pin and (with `install`) install a prepared dependency. `evidence` is the JSON
/// of what `deps_measure` measured, when the dependency was kept after `--eval`.
#[pyfunction]
#[pyo3(signature = (prepared, install=true, measurement=None))]
fn deps_commit(py: Python<'_>, prepared: String, install: bool, measurement: Option<String>) -> PyResult<String> {
    let p: core::deps::Prepared = serde_json::from_str(&prepared).map_err(|e| json_arg_err("prepared", e))?;
    let m: Option<core::deps::Measurement> = measurement.map(|m| serde_json::from_str(&m)).transpose().map_err(|e| json_arg_err("measurement", e))?;
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&p.manifest.to_string_lossy()), Some(&lock_dir(&p.lock.to_string_lossy())))?;
        let digest = p.resolved.first().and_then(|r| r.entry.digest().map(str::to_string));
        let o = core::deps::commit(&p, &reg, &ctx, install, m.map(|m| m.evidence(digest))).map_err(py_err)?;
        to_json(&o)
    })
}

/// `hlock add --eval`: run the eval pack on the locked stack with and without the
/// prepared dependency; returns the measurement as JSON. Nothing in the project changes.
#[pyfunction]
fn deps_measure(py: Python<'_>, prepared: String) -> PyResult<String> {
    let p: core::deps::Prepared = serde_json::from_str(&prepared).map_err(|e| json_arg_err("prepared", e))?;
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&p.manifest.to_string_lossy()), Some(&lock_dir(&p.lock.to_string_lossy())))?;
        to_json(&core::deps::measure(&p, &reg, &ctx).map_err(py_err)?)
    })
}

/// `hlock add --adopt NAME`: declare something the pinned lock already records.
#[pyfunction]
#[pyo3(signature = (name, manifest="harness.toml".to_string(), lock="harness.lock".to_string()))]
fn deps_adopt(py: Python<'_>, name: String, manifest: String, lock: String) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        to_json(&core::deps::adopt(&name, &abs(&manifest), &abs(&lock), &reg, &ctx).map_err(py_err)?)
    })
}

/// `hlock remove KIND NAME`: undeclare a dependency, unpin it, and (with `uninstall`)
/// take it out of the harness.
#[pyfunction]
#[pyo3(signature = (kind, name, manifest="harness.toml".to_string(), lock="harness.lock".to_string(), uninstall=true))]
fn deps_remove(py: Python<'_>, kind: String, name: String, manifest: String, lock: String, uninstall: bool) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        let kind = core::deps::Kind::parse(&kind).map_err(py_err)?;
        to_json(&core::deps::remove(kind, &name, &abs(&manifest), &abs(&lock), &reg, &ctx, uninstall).map_err(py_err)?)
    })
}

/// How the declared dependencies differ from the lock and the harness: a JSON list of
/// issues (`problem`, `kind`, `name`, `detail`, `fix`, `fails`).
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), manifest="harness.toml".to_string()))]
fn deps_check(py: Python<'_>, lock: String, manifest: String) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        let m = project_manifest(&manifest)?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        to_json(&core::deps::check(m.as_ref(), &l, &reg, &ctx).map_err(py_err)?)
    })
}

/// Bring the lock's declared dependencies in line with the manifest (and `apm.yml`):
/// returns `{"changes": […]}`. With `write`, saves the lock; with `frozen`, raises
/// instead of changing it.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), manifest="harness.toml".to_string(), frozen=false, write=true))]
fn deps_refresh(py: Python<'_>, lock: String, manifest: String, frozen: bool, write: bool) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        let m = project_manifest(&manifest)?;
        let mut l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let changes = core::deps::refresh(m.as_ref(), &mut l, &reg, &ctx, frozen).map_err(py_err)?;
        if write && !changes.is_empty() {
            l.save(Path::new(&lock)).map_err(py_err)?;
        }
        Ok(json_str(serde_json::json!({"changes": changes})))
    })
}

/// After `sync` wrote declared MCP servers into project config files (`files`, profile file
/// ids), re-record them in the lock. Returns whether the lock changed.
#[pyfunction]
#[pyo3(signature = (files, lock="harness.lock".to_string(), manifest="harness.toml".to_string()))]
fn deps_record_configs(py: Python<'_>, files: Vec<String>, lock: String, manifest: String) -> PyResult<bool> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        core::deps::record_configs(Path::new(&lock), &reg, &ctx, &files.into_iter().collect()).map_err(py_err)
    })
}

/// What a lock would run on this machine, each with its trust status, as JSON.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), manifest=None))]
fn trust_list(py: Python<'_>, lock: String, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&lock)))?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let dir = lock_dir(&lock);
        let store = core::trust::TrustStore::load(&ctx).map_err(py_err)?;
        let policy = core::policy::Policy::for_lock(Path::new(&lock), &ctx).map_err(py_err)?;
        let es = core::trust::assess(&l, &dir, reg.harness(&l.harness.name), &ctx, &policy, &store);
        Ok(json_str(serde_json::json!({"executables": es, "store": core::trust::store_path(&ctx)})))
    })
}

/// Approve (or, with `deny`, deny) a lock's executables on this machine: those whose
/// digest or name is in `only`, else every one not yet trusted. Returns how many changed.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), only=None, deny=false, manifest=None))]
fn trust_approve(py: Python<'_>, lock: String, only: Option<Vec<String>>, deny: bool, manifest: Option<String>) -> PyResult<usize> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&lock)))?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let mut store = core::trust::TrustStore::load(&ctx).map_err(py_err)?;
        let policy = core::policy::Policy::for_lock(Path::new(&lock), &ctx).map_err(py_err)?;
        let es = core::trust::assess(&l, &lock_dir(&lock), reg.harness(&l.harness.name), &ctx, &policy, &store);
        // a name, a full digest, or at least 8 characters of one (`--only ""` names nothing)
        let named = |n: &str, e: &core::trust::Executable| !n.is_empty() && (n == e.digest || n == e.name || (n.len() >= 8 && e.digest.ends_with(n)) || (n.len() >= 8 && e.digest.trim_start_matches("sha256:").starts_with(n)));
        let pick: Vec<&core::trust::Executable> = es
            .iter()
            .filter(|e| match &only {
                Some(names) => names.iter().any(|n| named(n, e)),
                None => e.status == "untrusted" || (deny && e.status == "trusted"),
            })
            .collect();
        if let Some(names) = &only {
            if let Some(n) = names.iter().find(|n| !pick.iter().any(|e| named(n, e))) {
                return Err(py_err(core::Error::Audit(format!("{lock} runs nothing named or digested {n:?}; see hlock trust"))));
            }
        }
        let n = if deny { store.deny(pick) } else { store.approve(pick) };
        store.save(&ctx).map_err(py_err)?;
        Ok(n)
    })
}

/// `hlock audit`: integrity, hidden characters, executables, policy and secrets, as JSON
/// (`format="sarif"` for SARIF 2.1.0). With `strip`, cleans vendored files first.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), ci=false, strip=false, verbose=false, format="json".to_string(), manifest=None))]
fn audit(py: Python<'_>, lock: String, ci: bool, strip: bool, verbose: bool, format: String, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&lock)))?;
        let r = core::audit::run(Path::new(&lock), &reg, &ctx, ci, strip, verbose).map_err(py_err)?;
        if format == "sarif" {
            return Ok(json_str(core::audit::sarif(&r)));
        }
        to_json(&r)
    })
}

/// The hidden-Unicode findings in what a lock places (critical and warning), and whether
/// each entry's critical findings were allowed, as JSON.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), manifest=None))]
fn scan_lock(py: Python<'_>, lock: String, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&lock)))?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let contents = core::audit::contents(&l, &lock_dir(&lock), reg.harness(&l.harness.name), &ctx);
        let out: Vec<serde_json::Value> = core::audit::scan_contents(&contents)
            .into_iter()
            .flat_map(|(c, fs)| {
                let allowed = core::audit::allowed(c.scan.as_deref());
                fs.into_iter().filter(|f| f.severity > core::scan::Severity::Info).map(move |f| serde_json::json!({"finding": f, "allowed": allowed, "entry": c.name, "kind": c.kind}))
            })
            .collect();
        Ok(json_str(serde_json::json!(out)))
    })
}

/// The project policy's verdict on a lock: `{"enforcement", "files", "violations"}`.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), manifest=None))]
fn policy_check(py: Python<'_>, lock: String, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (_, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&lock)))?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let p = core::policy::Policy::for_lock(Path::new(&lock), &ctx).map_err(py_err)?;
        let v = if p.enforcement == "off" { Vec::new() } else { p.check_lock(&l) };
        Ok(json_str(serde_json::json!({"enforcement": p.enforcement, "files": p.files, "violations": v})))
    })
}

/// A lock as an SBOM: `format` is `cyclonedx` (CycloneDX 1.6 JSON) or `spdx` (SPDX 2.3 JSON).
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), format="cyclonedx".to_string()))]
fn export_sbom(lock: String, format: String) -> PyResult<String> {
    let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
    let doc = match format.as_str() {
        "cyclonedx" => core::sbom::cyclonedx(&l),
        "spdx" => core::sbom::spdx(&l),
        other => return Err(HarnessLockError::new_err(format!("{other:?} isn't an SBOM format: cyclonedx or spdx"))),
    };
    serde_json::to_string_pretty(&doc).map(|t| t + "\n").map_err(|e| py_err(e.into()))
}

/// Declared dependencies with a newer upstream, as JSON (`current`, `wanted`, `latest`).
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), manifest="harness.toml".to_string(), kind=None))]
fn deps_outdated(py: Python<'_>, lock: String, manifest: String, kind: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        let m = project_manifest(&manifest)?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let only = kind.map(|k| core::deps::Kind::parse(&k)).transpose().map_err(py_err)?;
        to_json(&core::deps::outdated(m.as_ref(), &l, &reg, &ctx, only).map_err(py_err)?)
    })
}

/// `hlock upgrade dep:NAME`: the declared dependency resolved to the newest it allows,
/// prepared (JSON for `deps_commit`) but not applied.
#[pyfunction]
#[pyo3(signature = (name, manifest="harness.toml".to_string(), lock="harness.lock".to_string()))]
fn deps_upgrade(py: Python<'_>, name: String, manifest: String, lock: String) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        to_json(&core::deps::prepare_upgrade(&name, &abs(&manifest), &abs(&lock), &reg, &ctx).map_err(py_err)?)
    })
}

/// `hlock export --format apm-lock`: the lock as `apm.lock.yaml`, written to `out` (or,
/// with `out=None`, only returned). Returns JSON with the text and a summary.
#[pyfunction]
#[pyo3(signature = (lock="harness.lock".to_string(), out=None, force=false, manifest="harness.toml".to_string()))]
fn export_apm_lock(py: Python<'_>, lock: String, out: Option<String>, force: bool, manifest: String) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(Some(&manifest), Some(&lock_dir(&lock)))?;
        let l = core::Lockfile::load(Path::new(&lock)).map_err(py_err)?;
        let profile = reg.require_harness(&l.harness.name).map_err(py_err)?;
        let env = core::deps::Env::new(&ctx, profile);
        let out = out.map(|o| abs(&o));
        to_json(&core::apm::export(&l, &env, out.as_deref(), force).map_err(py_err)?)
    })
}

/// Build the `hlock sync` plan for a lock as JSON. Nothing is changed. The plan writes
/// into an isolated root (`prefix`, default `.hlock/env`) unless `in_place` or `docker`.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), prefix=None, in_place=false, docker=None, install=false, pull=false, all_models=false, exact=false, manifest=None, refresh=false, allow_hidden=None, trust=false, apps=false, default_prefix=None))]
#[allow(clippy::too_many_arguments)]
fn sync_plan(py: Python<'_>, path: String, prefix: Option<String>, in_place: bool, docker: Option<String>, install: bool, pull: bool, all_models: bool, exact: bool, manifest: Option<String>, refresh: bool, allow_hidden: Option<Vec<String>>, trust: bool, apps: bool, default_prefix: Option<String>) -> PyResult<String> {
    // no GIL while it waits on the network (deciders), so other Python threads keep running
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let mut lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
        if refresh {
            // the plan a sync would make: declarations it would lock first, in memory only
            let m = manifest.as_deref().map(project_manifest).transpose()?.flatten();
            core::deps::refresh(m.as_ref(), &mut lock, &reg, &ctx, false).map_err(py_err)?;
        }
        // a default prefix (a pulled lock's environment) is dropped for a desktop app
        let defaulted = prefix.is_none() && default_prefix.is_some();
        let mut opts = sync_options(prefix.or(default_prefix), in_place, docker, install, pull, all_models, exact);
        opts.default_prefix = defaulted;
        opts.also = also_harnesses(manifest.as_deref(), &path);
        opts.allow_hidden = allow_hidden.unwrap_or_default();
        opts.trust = trust;
        opts.apps = apps;
        let plan = core::sync::plan(&lock, Path::new(&path), &reg, &ctx, &opts).map_err(py_err)?;
        Ok(json_str(serde_json::json!(plan)))
    })
}

/// Perform one `auto` step from a plan (its JSON); returns a message.
#[pyfunction]
fn sync_run_step(py: Python<'_>, step: &str) -> PyResult<String> {
    let step: core::sync::Step = serde_json::from_str(step).map_err(|e| json_arg_err("step", e))?;
    if let Some(core::sync::Action::Run { argv, .. }) = &step.action {
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(ProfileError::new_err(format!("step {:?} has no command to run", step.id)));
        }
    }
    // installers and downloads run here; Ctrl-C stops them
    interruptible(py, move || core::sync::run_step(&step, &core::profiles::Ctx::from_process()).map_err(py_err))
}

/// Mark a prefix-mode plan's isolated environment as not for version control
/// (a `.gitignore` of `*`). Does nothing for other plans.
#[pyfunction]
fn sync_ignore_env(plan: &str) -> PyResult<()> {
    let plan: core::sync::Plan = serde_json::from_str(plan).map_err(|e| json_arg_err("plan", e))?;
    match &plan.env {
        Some(dir) => core::sync::ignore_env(dir).map_err(py_err),
        None => Ok(()),
    }
}

/// The secrets a lock's placeholders need, and where each would come from (no values):
/// JSON `{"file": …, "keychain": …, "secrets": [{name, used_by, source}]}`.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), manifest=None))]
fn secret_status(py: Python<'_>, path: String, manifest: Option<String>) -> PyResult<String> {
    // the keychain lookups run a command each
    interruptible(py, move || {
        let (_, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
        core::secrets::references(&ctx).map_err(py_err)?; // surface a malformed secrets file
        let rows: Vec<_> = lock
            .secret
            .iter()
            .map(|s| serde_json::json!({"name": s.name, "used_by": s.used_by, "source": core::secrets::locate(&s.name, &ctx)}))
            .collect();
        Ok(json_str(serde_json::json!({
            "file": core::secrets::secrets_file(&ctx),
            "keychain": core::secrets::keychain(&ctx).map(|(k, _)| k),
            "secrets": rows,
        })))
    })
}

/// Store a secret in the OS keychain; returns its full `HLOCK_SECRET_…` name.
#[pyfunction]
fn secret_set(py: Python<'_>, name: String, value: String) -> PyResult<String> {
    interruptible(py, move || core::secrets::keychain_set(&name, &value, &core::profiles::Ctx::from_process()).map_err(py_err))
}

/// Remove a secret from the OS keychain; false when it had none.
#[pyfunction]
fn secret_delete(py: Python<'_>, name: String) -> PyResult<bool> {
    interruptible(py, move || core::secrets::keychain_delete(&name, &core::profiles::Ctx::from_process()).map_err(py_err))
}

/// The rehydrate Agent Skill (SKILL.md text) for a plan.
#[pyfunction]
#[pyo3(signature = (plan, lock_name="harness.lock".to_string()))]
fn sync_skill(plan: &str, lock_name: String) -> PyResult<String> {
    let plan: core::sync::Plan = serde_json::from_str(plan).map_err(|e| json_arg_err("plan", e))?;
    Ok(core::sync::rehydrate_skill(&plan, &lock_name))
}

/// The Dockerfile and entrypoint for a docker-mode plan, as JSON.
#[pyfunction]
#[pyo3(signature = (plan, path="harness.lock".to_string(), manifest=None, base=None))]
fn sync_dockerfile(plan: &str, path: String, manifest: Option<String>, base: Option<String>) -> PyResult<String> {
    let plan: core::sync::Plan = serde_json::from_str(plan).map_err(|e| json_arg_err("plan", e))?;
    let (reg, _, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let profile = reg.require_harness(&plan.harness).map_err(py_err)?;
    let (dockerfile, entrypoint) = core::sync::dockerfile_on(&plan, &lock, profile, base.as_deref());
    Ok(json_str(serde_json::json!({"dockerfile": dockerfile, "entrypoint": entrypoint})))
}

/// Prepare `hlock launch`: verify, materialize the isolated root, and return
/// JSON with the argv and environment to exec, plus what was done and notes.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), harness=None, prefix=None, allow_version_mismatch=false, in_place=false, args=Vec::new(), manifest=None, prompt=None, trust=false, allow_hidden=None, project=None, no_sandbox=false, proxy_env=None, tty=false))]
#[allow(clippy::too_many_arguments)]
fn launch_prepare(
    py: Python<'_>,
    path: String,
    harness: Option<String>,
    prefix: Option<String>,
    allow_version_mismatch: bool,
    in_place: bool,
    args: Vec<String>,
    manifest: Option<String>,
    prompt: Option<String>,
    trust: bool,
    allow_hidden: Option<Vec<String>>,
    project: Option<String>,
    no_sandbox: bool,
    proxy_env: Option<std::collections::BTreeMap<String, String>>,
    tty: bool,
) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
        // with a stand-in project, a relative lock path still means the real one
        let path = if project.is_some() { lock_dir(&path).join(Path::new(&path).file_name().unwrap_or_default()).to_string_lossy().into_owned() } else { path };
        let opts = core::launch::LaunchOptions {
            project: project.map(PathBuf::from),
            prompt,
            target: harness,
            prefix: prefix.map(PathBuf::from),
            allow_version_mismatch,
            in_place,
            workspace: std::env::current_dir().ok(),
            trust,
            allow_hidden: allow_hidden.unwrap_or_default(),
            no_sandbox,
            proxy_env: proxy_env.unwrap_or_default(),
            tty,
            ..Default::default()
        };
        let prep = core::launch::prepare(&lock, Path::new(&path), &reg, &ctx, &opts, &args).map_err(py_err)?;
        Ok(json_str(serde_json::json!(prep)))
    })
}

/// The lock's sandbox and how it stands on this machine: the spec, `harness.toml`'s (and
/// whether they differ), whether it's turned off, its runner and whether that's
/// installed, whether it's trusted, and notes. Returns JSON; never raises for a sandbox
/// that can't run (it's reported in `error`).
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), manifest=None, no_sandbox=false))]
fn sandbox_info(path: String, manifest: Option<String>, no_sandbox: bool) -> PyResult<String> {
    let (_, ctx, m) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let spec = core::sandbox::Spec::from_lock(&lock).map_err(py_err)?;
    let declared = match &m {
        Some(m) => m.sandbox_spec().map_err(py_err)?,
        None => None,
    };
    let (off, note) = core::sandbox::disabled(no_sandbox, &ctx);
    let ldir = lock_dir(&path);
    let table = core::sandbox::project_table(&ldir);
    let runners = core::sandbox::runners(&ctx, table.as_ref()).map_err(py_err)?;
    let runner = spec.as_ref().and_then(|s| runners.get(&s.runner)).map(|r| {
        let program = if r.builtin() { r.name.clone() } else { r.command[0].clone() };
        serde_json::json!({"name": r.name, "origin": r.origin, "program": program, "installed": ctx.which(&program).is_some(), "command": r.command})
    });
    let trusted = core::trust::sandbox_executable(&lock, &ldir, &ctx).map(|e| {
        let store = core::trust::TrustStore::load(&ctx).ok();
        store.and_then(|s| s.status(&e.digest).map(str::to_string)).unwrap_or_else(|| "untrusted".into())
    });
    let error = spec.as_ref().filter(|_| !off).and_then(|s| core::sandbox::Sandbox::new(s, &ctx.cwd, table.as_ref(), &ctx).err()).map(|e| e.to_string());
    let policy = core::policy::Policy::for_lock(Path::new(&path), &ctx).map_err(py_err)?;
    let violations: Vec<String> = policy.check_sandbox(spec.is_some(), off).into_iter().map(|v| v.message).collect();
    Ok(json_str(serde_json::json!({
        "spec": spec,
        "summary": spec.as_ref().map(|s| s.summary()),
        "declared": declared,
        "drift": m.is_some() && declared != spec,
        "off": off,
        "note": note,
        "runner": runner,
        "runners": runners.keys().collect::<Vec<_>>(),
        "trusted": trusted,
        "error": error,
        "policy": violations,
        "evidence": lock.evidence.sandbox,
        "evidence_note": core::sandbox::evidence_note(&lock),
        "weakened": core::sandbox::weakened(declared.as_ref(), spec.as_ref()),
    })))
}

/// A digest of each control file in `project` (harness.toml, the locks, the policy,
/// `.envrc`, git hooks), or null where there's none: compared before and after a
/// sandboxed session, to report what it created or changed. Returns JSON.
#[pyfunction]
fn sandbox_control_snapshot(project: String) -> String {
    json_str(serde_json::json!(core::sandbox::control_snapshot(Path::new(&project))))
}

/// Begin an OpenShell session (`sandbox.session` from a prepared launch): create the
/// sandbox, upload its places, attach providers. `env` is the runner's environment
/// (`sandbox.env`), `command_env` the command's (`sandbox.session_env`); `timeout`, in seconds,
/// is when the caller gives up on the command. Returns JSON `{argv, env, deadline, session}`:
/// run `argv` in `env`, then pass `session` to [`sandbox_session_end`]. Not interruptible:
/// what it creates is removed if it fails.
#[pyfunction]
#[pyo3(signature = (session, env, command_env, timeout=None))]
fn sandbox_session_begin(py: Python<'_>, session: String, env: std::collections::BTreeMap<String, String>, command_env: std::collections::BTreeMap<String, String>, timeout: Option<u64>) -> PyResult<String> {
    let s: core::openshell::Session = serde_json::from_str(&session).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let started = py.allow_threads(move || core::openshell::begin(s, &env, &command_env, timeout)).map_err(py_err)?;
    Ok(json_str(serde_json::json!(started)))
}

/// End an OpenShell session: copy back what the command changed, delete the sandbox and its
/// providers. `stopped`: how the caller stopped the command early (`"timeout"`,
/// `"interrupt"`), when it did. Returns JSON `{notes}`: what the run should say.
#[pyfunction]
#[pyo3(signature = (session, env, stopped=None))]
fn sandbox_session_end(py: Python<'_>, session: String, env: std::collections::BTreeMap<String, String>, stopped: Option<String>) -> PyResult<String> {
    let s: core::openshell::Session = serde_json::from_str(&session).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let notes = py.allow_threads(move || core::openshell::end(&s, &env, stopped.as_deref())).map_err(py_err)?;
    Ok(json_str(serde_json::json!({"notes": notes})))
}

/// Run a locked code harness (`[harnesses.<id>]`) on a prompt through the
/// execution contract. Returns JSON with success, output, cost, latency and log.
#[pyfunction]
#[pyo3(signature = (prompt, path="harness.lock".to_string(), workspace=None, timeout=600, manifest=None, trust=false, no_sandbox=false))]
#[allow(clippy::too_many_arguments)]
fn run_code_harness(py: Python<'_>, prompt: String, path: String, workspace: Option<String>, timeout: u64, manifest: Option<String>, trust: bool, no_sandbox: bool) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
        let profile = reg.require_harness(&lock.harness.name).map_err(py_err)?;
        if profile.kind != "custom" {
            return Err(ProfileError::new_err(format!("{} isn't a code harness", profile.id)));
        }
        core::launch::require_trusted(&lock, Path::new(&path), &reg, &ctx, trust).map_err(py_err)?;
        // the agent works where the user is; the project root stays on PYTHONPATH
        let ws = workspace.map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| ctx.cwd.clone());
        let inv = core::contract::invocation(profile, Some(&lock), Some(Path::new(&path)), None, &prompt, &ws, 0, &ctx).map_err(py_err)?;
        let mut notes = Vec::new();
        let sandbox = core::sandbox::for_lock(&lock, Path::new(&path), &ctx.cwd, &ctx, no_sandbox, trust, &mut notes).map_err(py_err)?;
        if sandbox.as_ref().is_some_and(|s| !s.spec.applies("trials")) {
            notes.push("the lock's sandbox doesn't cover trials (applies-to), so the code harness runs on this machine".to_string());
        }
        let sandbox = sandbox.filter(|s| s.spec.applies("trials"));
        let r = core::contract::run_in(&inv, &ctx, std::time::Duration::from_secs(timeout), sandbox.as_ref()).map_err(py_err)?;
        let mut v = serde_json::json!(r);
        v["notes"] = serde_json::json!(notes);
        v["sandbox"] = serde_json::json!(sandbox.map(|s| s.spec.summary()));
        Ok(json_str(v))
    })
}

/// Write a localized (all-local) copy of a lock to `out`. Evaluates candidates on
/// `evals` (an eval pack path; `tasks` is its old name) or `./harness.toml`'s eval
/// packs when available.
/// Returns JSON with the lock and the localize report.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), out="harness.local.lock".to_string(), harness=None, runtime=None, models=Vec::new(), memory_gb=None, evals=None, tasks=None, strict=false, allow_network=false, workdir=None, manifest=None))]
fn localize(
    py: Python<'_>,
    path: String,
    out: String,
    harness: Option<String>,
    runtime: Option<String>,
    models: Vec<String>,
    memory_gb: Option<f64>,
    evals: Option<String>,
    tasks: Option<String>,
    strict: bool,
    allow_network: bool,
    workdir: Option<String>,
    manifest: Option<String>,
) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, found) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let manifest = match evals.or(tasks) {
            Some(t) => Some(
                core::Manifest::from_str(&format!(
                    "[evals]\npacks = [{t:?}]\nseeds = 2\n[objective]\nprimary = \"success-rate\"\nselect = \"max-quality\"\n"
                ))
                .map_err(py_err)?,
            ),
            None => found,
        };
        let opts = core::localize::LocalizeOptions {
            harness,
            runtime,
            models,
            memory_gb,
            manifest,
            strict,
            allow_network,
            workdir: workdir.map(PathBuf::from).unwrap_or_else(|| ctx.cwd.join(".hlock/localize")),
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        let abs = |p: &str| if Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) };
        let o = core::localize::localize_to(&abs(&path), &abs(&out), &reg, &ctx, &opts).map_err(py_err)?;
        Ok(json_str(serde_json::json!({"lock": o.lock, "report": o.report, "path": out})))
    })
}

/// Write a copy of a lock moved onto the harness `to` to `out` (default:
/// `harness.<to>.lock` beside it). `model` (`provider:model`) sets the default role.
/// Returns JSON with the lock, the migrate report, and the path written.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), to=String::new(), out=None, model=None, manifest=None))]
fn migrate(py: Python<'_>, path: String, to: String, out: Option<String>, model: Option<String>, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let target = reg.require_harness(&to).map_err(py_err)?.id.clone();
        let out = out.unwrap_or_else(|| core::migrate::default_out(Path::new(&path), &target).to_string_lossy().into_owned());
        let cwd = std::env::current_dir().unwrap_or_default();
        let abs = |p: &str| if Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) };
        let opts = core::migrate::MigrateOptions { to, model };
        let o = core::migrate::migrate_to(&abs(&path), &abs(&out), &reg, &ctx, &opts).map_err(py_err)?;
        Ok(json_str(serde_json::json!({"lock": o.lock, "report": o.report, "path": out})))
    })
}

/// Free text with credential-looking words replaced by `[redacted]`.
#[pyfunction]
fn redact_text(text: String) -> String {
    core::capture::redact_text(&text)
}

/// The manifest at `path`, when one is passed; a path that isn't a file is an error.
fn load_manifest(path: Option<&str>) -> PyResult<Option<core::Manifest>> {
    match path {
        Some(p) => Ok(Some(core::Manifest::load(Path::new(p)).map_err(py_err)?)),
        None => Ok(None),
    }
}

/// Grading options from the manifest at `path`: `[evals.judge]`, `[deciders.judge]` or
/// `[deciders.default]`, and, with `auto`, a judge hlock finds. Invalid tables raise.
fn grade_options(manifest: Option<&str>, auto: bool) -> PyResult<core::tasks::GradeOptions> {
    let m = load_manifest(manifest)?;
    let (judge, default_judge) = match &m {
        Some(m) => (m.judge_checked().map_err(py_err)?, m.default_judge_checked().map_err(py_err)?),
        None => (None, None),
    };
    Ok(core::tasks::GradeOptions { judge, default_judge, auto, ..Default::default() })
}

/// Load the eval pack at `path` and check it (see `EvalPack::check`). With `run`, also
/// try its graders on each task's fixtures and solution (`EvalPack::check_graders`):
/// tests and decision-model judges, and LLM judges too with `judges`. `manifest` is a
/// `harness.toml` whose judge settings apply. Returns JSON with the pack's header, its
/// task ids, the judge, and the issues found.
#[pyfunction]
#[pyo3(signature = (path, run=false, manifest=None, judges=false))]
fn check_eval_pack(py: Python<'_>, path: String, run: bool, manifest: Option<String>, judges: bool) -> PyResult<String> {
    // no GIL while it waits on the network, so other Python threads keep running
    interruptible(py, move || {
        let pack = core::tasks::EvalPack::load(&path, None).map_err(py_err)?;
        let mut opts = grade_options(manifest.as_deref(), run)?;
        opts.judges = if judges { core::tasks::Judges::All } else { core::tasks::Judges::Deciders };
        let judge = pack.judge_for(&opts);
        let mut issues = pack.check_with(judge.as_ref());
        if run {
            if issues.iter().any(|i| i.level == "error") {
                issues.push(core::tasks::PackIssue { level: "warn".into(), task: None, message: "the graders weren't run; fix the errors first".into() });
            } else {
                issues.extend(pack.check_graders(&opts));
            }
        }
        Ok(json_str(serde_json::json!({
            "name": pack.name,
            "version": pack.version,
            "domain": pack.domain,
            "path": pack.path,
            "hash": pack.hash,
            "tasks": pack.tasks.iter().map(|t| &t.id).collect::<Vec<_>>(),
            "judge": judge.map(|j| serde_json::json!({"model": j.model, "decider": j.is_decider()})),
            "issues": issues,
        })))
    })
}

/// Make `dest` a fresh workspace for task `task` of the eval pack at `path`, holding
/// its fixtures (see `EvalPack::prepare_workspace`). Returns JSON with the task's
/// `prompt` text and `timeout-seconds`, for `hlock evals run`.
#[pyfunction]
fn prepare_eval_workspace(path: String, task: String, dest: String) -> PyResult<String> {
    let pack = core::tasks::EvalPack::load(&path, None).map_err(py_err)?;
    let t = pack.prepare_workspace(&task, Path::new(&dest)).map_err(py_err)?;
    Ok(json_str(serde_json::json!({
        "task": t.id,
        "prompt": t.prompt_text,
        "timeout-seconds": t.timeout_seconds,
        "workspace": dest,
    })))
}

/// The content digest of the eval packs the manifest at `manifest` names (see
/// `tasks::packs_digest`), as the lock's `[evidence] eval-pack-digest` records it.
#[pyfunction]
fn eval_packs_digest(manifest: String) -> PyResult<String> {
    let m = core::Manifest::load(Path::new(&manifest)).map_err(py_err)?;
    Ok(core::tasks::packs_digest(&m))
}

/// Grade a directory as the output of task `task` in the eval pack at `path` (see
/// `EvalPack::grade_dir`); with no `workspace`, the task's solution. `manifest` is a
/// `harness.toml` whose judge settings apply. Returns JSON with the grades and the
/// overall `success` and `score`.
#[pyfunction]
#[pyo3(signature = (path, task, workspace=None, manifest=None, lock=None, no_sandbox=false, trust=false))]
#[allow(clippy::too_many_arguments)]
fn grade_eval_task(py: Python<'_>, path: String, task: String, workspace: Option<String>, manifest: Option<String>, lock: Option<String>, no_sandbox: bool, trust: bool) -> PyResult<String> {
    // no GIL while it waits on the network, so other Python threads keep running
    interruptible(py, move || {
        let pack = core::tasks::EvalPack::load(&path, None).map_err(py_err)?;
        let mut opts = grade_options(manifest.as_deref(), true)?;
        // `tests` graders run in the lock's sandbox, when it covers them
        let mut notes = Vec::new();
        if let Some(lp) = lock.filter(|l| Path::new(l).is_file()) {
            let (_, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&lp)))?;
            let lf = core::Lockfile::load(Path::new(&lp)).map_err(py_err)?;
            opts.sandbox = core::sandbox::for_lock(&lf, Path::new(&lp), &ctx.cwd, &ctx, no_sandbox, trust, &mut notes).map_err(py_err)?.filter(|s| s.spec.applies("graders"));
        }
        let grades = pack.grade_dir(&task, workspace.as_deref().map(Path::new), &opts).map_err(py_err)?;
        let score = grades.iter().map(|g| g.score).sum::<f64>() / grades.len().max(1) as f64;
        Ok(json_str(serde_json::json!({
            "task": task,
            "workspace": workspace,
            "judge": pack.judge_for(&opts).map(|j| j.model),
            "success": grades.iter().all(|g| g.success),
            "score": score,
            "grades": grades,
            "sandbox": opts.sandbox.as_ref().map(|s| s.spec.summary()),
            "notes": notes,
        })))
    })
}

/// Compare two judges on the pack at `path` (see `EvalPack::calibrate`): `against`
/// is the second judge's model; the first is the pack's judge (or `model`). Returns
/// JSON rows and the agreement rate.
#[pyfunction]
#[pyo3(signature = (path, against, model=None, manifest=None, against_base_url=None))]
fn calibrate_eval_pack(py: Python<'_>, path: String, against: String, model: Option<String>, manifest: Option<String>, against_base_url: Option<String>) -> PyResult<String> {
    // no GIL while it waits on the network, so other Python threads keep running
    interruptible(py, move || {
        let pack = core::tasks::EvalPack::load(&path, None).map_err(py_err)?;
        let opts = grade_options(manifest.as_deref(), true)?;
        let a = match model {
            Some(m) => core::tasks::JudgeConfig::new(&m),
            None => pack.judge_for(&opts).ok_or_else(|| py_err(core::Error::EvalPack("no judge to calibrate: set [pack.judge], [evals.judge], or pass --model".into())))?,
        };
        let b = core::tasks::JudgeConfig { base_url: against_base_url, ..core::tasks::JudgeConfig::new(&against) };
        let rows = pack.calibrate(&a, &b);
        let agree = rows.iter().filter(|r| r.agree).count();
        Ok(json_str(serde_json::json!({
            "a": a.model,
            "b": b.model,
            "rows": rows,
            "agreement": if rows.is_empty() { serde_json::Value::Null } else { serde_json::json!(agree as f64 / rows.len() as f64) },
        })))
    })
}

/// The `[deciders.<use_>]` spec that names a model, else `[deciders.default]`'s (a use
/// with only `any-of` candidates has no model to call).
fn spec_with_model<'a>(specs: &'a std::collections::BTreeMap<String, core::decide::DeciderSpec>, use_: Option<&str>) -> Option<&'a core::decide::DeciderSpec> {
    let named = |u: &str| specs.get(u).filter(|s| s.model.is_some());
    use_.and_then(named).or_else(|| named("default"))
}

/// What `decide` calls: an explicit `model`, with the manifest spec's settings (base URL,
/// key, protocol) only when that spec's model is from the same provider; else the spec's
/// model; else a decider hlock finds.
fn decider_config(specs: &std::collections::BTreeMap<String, core::decide::DeciderSpec>, use_: Option<&str>, model: Option<String>) -> Option<core::decide::DeciderConfig> {
    let spec = spec_with_model(specs, use_);
    let provider = |m: &str| core::decide::split_ref(core::decide::split_threshold(m).0).map(|(p, _)| p.to_string());
    match model {
        Some(mdl) => {
            let same = spec.filter(|s| provider(&mdl).is_some() && s.model.as_deref().and_then(provider) == provider(&mdl));
            Some(match same {
                Some(s) => s.config(&mdl),
                None => core::decide::DeciderConfig::from_ref(core::decide::split_threshold(&mdl).0),
            })
        }
        None => match spec.and_then(|s| s.model.as_deref().map(|m| s.config(m))) {
            Some(c) => Some(c),
            None => core::graders::auto_judge().map(|j| core::decide::DeciderConfig { model: j.model, ..Default::default() }),
        },
    }
}

/// Ask a decision model `questions` (JSON: a list of `{id, type, instructions,
/// criteria}`) about `state` (JSON). `model` is `provider:model`; without one, the
/// manifest's `[deciders.<use_>]`, then `[deciders.default]`, then one hlock finds.
#[pyfunction]
#[pyo3(signature = (state, questions, model=None, use_=None, manifest=None))]
fn decide(py: Python<'_>, state: String, questions: String, model: Option<String>, use_: Option<String>, manifest: Option<String>) -> PyResult<String> {
    // no GIL while it waits on the network, so other Python threads keep running
    interruptible(py, move || {
        // plain text is a state too, but text that starts like JSON must be JSON
        let state: serde_json::Value = match serde_json::from_str(&state) {
            Ok(v) => v,
            Err(e) if state.trim_start().starts_with(['{', '[']) => return Err(json_arg_err("state", e)),
            Err(_) => serde_json::Value::String(state),
        };
        let questions: Vec<core::decide::Question> = serde_json::from_str(&questions).map_err(|e| json_arg_err("questions", e))?;
        let m = load_manifest(manifest.as_deref())?;
        let specs = m.as_ref().map(|m| m.deciders()).transpose().map_err(py_err)?.unwrap_or_default();
        let cfg = decider_config(&specs, use_.as_deref(), model).ok_or_else(|| {
            py_err(core::Error::Manifest("no decision model: pass --model, set [deciders.default] in harness.toml, run Kev on Ollama or Ollaya, or set TYPESAFE_API_KEY".into()))
        })?;
        let var = |k: &str| std::env::var(k).ok();
        let ep = cfg.endpoint(&var).map_err(|e| py_err(core::Error::Profile(e)))?;
        let decided = core::decide::decide(&ep, &state, &questions).map_err(|e| py_err(core::Error::Profile(e)))?;
        Ok(json_str(serde_json::json!({"provider": ep.provider, "decided": decided})))
    })
}

/// Answer one harness hook event (JSON on the hook's stdin) with the locked decider
/// for `use_` in the lock at `lock`. Returns JSON `{stdout, exit, note}`; it never
/// raises, since a hook that fails must defer.
#[pyfunction]
#[pyo3(signature = (use_, format, input, lock, event=None))]
fn decide_hook(py: Python<'_>, use_: String, format: String, input: String, lock: String, event: Option<String>) -> String {
    // no GIL while it waits on the network, so other Python threads keep running
    py.allow_threads(move || {
        let lock_path = Path::new(&lock);
        let out = match core::Lockfile::load(lock_path) {
            Ok(l) => match serde_json::from_str::<serde_json::Value>(&input) {
                Ok(input) => {
                    let root = lock_path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
                    let var = |k: &str| std::env::var(k).ok();
                    core::decide::hook(&use_, &format, event.as_deref(), &input, &l, &root, &var)
                }
                // defer, and say why
                Err(e) => core::decide::HookOutput { stdout: String::new(), exit: 0, note: format!("the hook's input wasn't JSON ({e}); deferring") },
            },
            Err(e) => core::decide::HookOutput { stdout: String::new(), exit: 0, note: format!("can't read the lock: {e}") },
        };
        json_str(serde_json::json!(out))
    })
}

/// What changed in the lock's deciders that the lock alone can tell (see
/// `decide::drift`): hlock's questions, and policy files.
#[pyfunction]
fn decider_drift(lock: String) -> PyResult<Vec<String>> {
    let lock_path = Path::new(&lock);
    let l = core::Lockfile::load(lock_path).map_err(py_err)?;
    let root = lock_path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    Ok(core::decide::drift(&l, &root))
}

// --------------------------------------------------------------------------- //
// hlock meta
// --------------------------------------------------------------------------- //

fn meta_manifest(path: Option<&str>) -> PyResult<Option<core::meta::MetaManifest>> {
    match path {
        Some(p) if Path::new(p).is_file() => Ok(Some(core::meta::MetaManifest::load(Path::new(p)).map_err(py_err)?)),
        _ => Ok(None),
    }
}

/// The project's state a meta job reads, plus the registry and context to read it with.
#[allow(clippy::too_many_arguments)]
fn meta_input(
    lock: &str,
    meta_lock: &str,
    manifest: Option<&str>,
    out: Option<String>,
    count: usize,
    instructions: String,
    guide: Option<String>,
    apps: bool,
) -> PyResult<(core::meta::JobInput, core::Registry, core::profiles::Ctx)> {
    let project = lock_dir(lock);
    let (reg, ctx, m) = registry(manifest, Some(&project))?;
    let abs = |p: &str| if Path::new(p).is_absolute() { PathBuf::from(p) } else { std::env::current_dir().unwrap_or_default().join(p) };
    let input = core::meta::JobInput {
        project,
        lock_path: abs(lock),
        manifest_path: m.as_ref().and_then(|m| m.source_path.clone()),
        manifest: m,
        meta_lock_path: abs(meta_lock),
        out: out.map(|o| abs(&o)),
        count,
        guide: guide.map(|g| abs(&g)),
        instructions,
        apps,
    };
    Ok((input, reg, ctx))
}

/// `harness.meta.toml` as JSON: the meta harness target, the default run mode, and
/// every job (built-in and custom). A missing file gives the defaults.
#[pyfunction]
#[pyo3(signature = (meta_manifest=None))]
fn meta_config(meta_manifest: Option<String>) -> PyResult<String> {
    let m = self::meta_manifest(meta_manifest.as_deref())?;
    Ok(json_str(serde_json::json!({
        "exists": m.is_some(),
        "target": m.as_ref().and_then(|m| m.target_harness()),
        "model": m.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
        "mode": if m.as_ref().is_some_and(|m| m.headless()) { "headless" } else { "interactive" },
        "jobs": core::meta::jobs(m.as_ref()),
    })))
}

/// Choose the meta harness and its model for `hlock meta init`: from the options,
/// the API keys in the environment, and installed local runtimes. Returns JSON with
/// the choice and the `harness.meta.toml` text to write.
#[pyfunction]
#[pyo3(signature = (harness=None, provider=None, model=None, runtime=None, memory_gb=None, base_url=None, manifest=None))]
#[allow(clippy::too_many_arguments)]
fn meta_init(py: Python<'_>, harness: Option<String>, provider: Option<String>, model: Option<String>, runtime: Option<String>, memory_gb: Option<f64>, base_url: Option<String>, manifest: Option<String>) -> PyResult<String> {
    // probes installed harnesses and runtimes
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
        let opts = core::meta::InitOptions { harness, provider, model, runtime, memory_gb, base_url };
        let c = core::meta::choose(&reg, &ctx, &opts).map_err(py_err)?;
        Ok(json_str(serde_json::json!({"choice": c, "manifest": core::meta::manifest_text(&c)})))
    })
}

/// Pin the meta harness into `out`: write `harness.meta.toml`'s `[model]` into the
/// meta harness's own config root, then capture that config.
#[pyfunction]
#[pyo3(signature = (out="harness.meta.lock".to_string(), meta_manifest="harness.meta.toml".to_string(), vendor=true, force=false, manifest=None))]
fn meta_pin(py: Python<'_>, out: String, meta_manifest: String, vendor: bool, force: bool, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, manifest) = registry(manifest.as_deref(), Some(&lock_dir(&out)))?;
        let out = if Path::new(&out).is_absolute() { out } else { std::env::current_dir().unwrap_or_default().join(&out).to_string_lossy().into_owned() };
        let meta = self::meta_manifest(Some(&meta_manifest))?
            .ok_or_else(|| ManifestError::new_err(format!("cannot read {meta_manifest}: no such file; create one with `hlock meta init`")))?;
        let project = manifest.as_ref().and_then(|m| m.project.get("name").and_then(|v| v.as_str()).map(str::to_string));
        let o = core::meta::pin(&reg, &ctx, &meta, Path::new(&out), project.as_deref(), vendor, force).map_err(py_err)?;
        Ok(json_str(serde_json::json!({
            "lock": o.lock,
            "path": o.path,
            "diff": o.diff,
            "previous": o.previous,
            "vendored": o.vendored,
            "warnings": o.warnings,
        })))
    })
}

/// Drift between `harness.meta.toml`, the meta lock, and the meta harness's own
/// config, as a JSON list of lines (empty when they match).
#[pyfunction]
#[pyo3(signature = (path="harness.meta.lock".to_string(), meta_manifest=None, manifest=None))]
fn meta_check(py: Python<'_>, path: String, meta_manifest: Option<String>, manifest: Option<String>) -> PyResult<String> {
    interruptible(py, move || {
        let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
        let meta = self::meta_manifest(meta_manifest.as_deref())?;
        let abs = if Path::new(&path).is_absolute() { PathBuf::from(&path) } else { std::env::current_dir().unwrap_or_default().join(&path) };
        let lock = core::Lockfile::load(&abs).map_err(py_err)?;
        let drift = core::meta::check(&reg, &ctx, meta.as_ref(), &lock, &abs).map_err(py_err)?;
        Ok(json_str(serde_json::json!(drift)))
    })
}

/// Prepare a meta job: render its prompt and build the command that runs it in the
/// meta harness pinned in `meta_lock`. Returns JSON (see `meta::Prepared`).
#[pyfunction]
#[pyo3(signature = (job, lock="harness.lock".to_string(), meta_lock="harness.meta.lock".to_string(), meta_manifest=None, manifest=None, out=None, count=10, instructions=String::new(), guide=None, headless=None, allow_version_mismatch=false, dry_run=false, apps=false))]
#[allow(clippy::too_many_arguments)]
fn meta_prepare(
    py: Python<'_>,
    job: String,
    lock: String,
    meta_lock: String,
    meta_manifest: Option<String>,
    manifest: Option<String>,
    out: Option<String>,
    count: usize,
    instructions: String,
    guide: Option<String>,
    headless: Option<bool>,
    allow_version_mismatch: bool,
    dry_run: bool,
    apps: bool,
) -> PyResult<String> {
    let meta = self::meta_manifest(meta_manifest.as_deref())?;
    if !Path::new(&meta_lock).is_file() {
        return Err(LockfileError::new_err(format!("no meta harness pinned: {meta_lock} doesn't exist; pin one with `hlock meta pin`")));
    }
    interruptible(py, move || {
        let (input, reg, ctx) = meta_input(&lock, &meta_lock, manifest.as_deref(), out, count, instructions, guide, apps)?;
        let ml = core::Lockfile::load(&input.meta_lock_path).map_err(py_err)?;
        let opts = core::meta::RunOptions { headless, allow_version_mismatch, dry_run };
        let prep = core::meta::prepare(&job, &input, meta.as_ref(), &ml, &reg, &ctx, &opts).map_err(py_err)?;
        Ok(json_str(serde_json::json!(prep)))
    })
}

/// hlock's own check of a meta job's result. Returns JSON (see `meta::Verdict`).
#[pyfunction]
#[pyo3(signature = (job, lock="harness.lock".to_string(), meta_lock="harness.meta.lock".to_string(), meta_manifest=None, manifest=None, out=None, count=0, apps=false))]
#[allow(clippy::too_many_arguments)]
fn meta_verify(py: Python<'_>, job: String, lock: String, meta_lock: String, meta_manifest: Option<String>, manifest: Option<String>, out: Option<String>, count: usize, apps: bool) -> PyResult<String> {
    // no GIL while it waits on the network (deciders), so other Python threads keep running
    interruptible(py, move || {
        let meta = self::meta_manifest(meta_manifest.as_deref())?;
        let (input, reg, ctx) = meta_input(&lock, &meta_lock, manifest.as_deref(), out, count, String::new(), None, apps)?;
        let v = core::meta::verify(&job, &input, meta.as_ref(), &reg, &ctx).map_err(py_err)?;
        Ok(json_str(serde_json::json!(v)))
    })
}

/// Run the built-in agent pinned in `meta_lock` on `prompt`, in `project` (default: the
/// lock's directory). `say(kind, text)` gets its progress and returns whether to keep
/// going; `ask(command)` returns `y`, `n` or `a` for a command that isn't allowed
/// (interactive runs only). Returns JSON (see `agent::Outcome`).
#[pyfunction]
#[pyo3(signature = (prompt, meta_lock="harness.meta.lock".to_string(), interactive=false, say=None, ask=None, project=None))]
fn agent_run(
    py: Python<'_>,
    prompt: String,
    meta_lock: String,
    interactive: bool,
    say: Option<PyObject>,
    ask: Option<PyObject>,
    project: Option<String>,
) -> PyResult<String> {
    let lock = core::Lockfile::load(Path::new(&meta_lock)).map_err(py_err)?;
    let project = project.map(PathBuf::from).unwrap_or_else(|| lock_dir(&meta_lock));
    let ctx = core::profiles::Ctx::from_process();
    let mut cfg = core::agent::config_from_lock(&lock, &project, interactive, &ctx.env).map_err(py_err)?;
    // harness.meta.toml's [deciders.default], next to the meta lock (or HLOCK_META_MANIFEST)
    let manifest = std::env::var("HLOCK_META_MANIFEST").map(PathBuf::from).unwrap_or_else(|_| lock_dir(&meta_lock).join(core::meta::META_MANIFEST_PATH));
    if manifest.is_file() {
        cfg.decider = core::meta::MetaManifest::load(&manifest).map_err(py_err)?.default_decider().map_err(py_err)?;
    }
    // The agent runs on its own thread without the GIL, taking it only to call the
    // callbacks. The first exception (a callback's, or Ctrl-C here) stops the run and is
    // raised once it has stopped.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    let failed: Arc<Mutex<Option<PyErr>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let fail = |cell: &Mutex<Option<PyErr>>, stop: &AtomicBool, e: PyErr| {
        if let Ok(mut first) = cell.lock() {
            first.get_or_insert(e);
        }
        stop.store(true, Ordering::SeqCst);
    };
    let (tx, rx) = mpsc::channel();
    {
        let (failed, stop) = (failed.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut say_fn = |kind: &str, text: &str| -> bool {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                let Some(f) = &say else { return true };
                Python::with_gil(|py| match f.call1(py, (kind, text)).and_then(|r| r.bind(py).is_truthy()) {
                    Ok(keep_going) => keep_going,
                    Err(e) => {
                        fail(&failed, &stop, e);
                        false
                    }
                })
            };
            let mut ask_fn = |command: &str| -> String {
                let Some(f) = ask.as_ref().filter(|_| !stop.load(Ordering::SeqCst)) else { return "n".into() };
                Python::with_gil(|py| match f.call1(py, (command,)).and_then(|r| r.extract::<String>(py)) {
                    Ok(answer) => answer,
                    Err(e) => {
                        fail(&failed, &stop, e);
                        "n".into()
                    }
                })
            };
            let _ = tx.send(core::agent::run(&cfg, &prompt, &mut say_fn, &mut ask_fn));
        });
    }
    // after Ctrl-C, give the run a moment to stop on its own (its commands are killed)
    let mut interrupted: Option<std::time::Instant> = None;
    let mut rx = rx;
    let out = loop {
        let got;
        (got, rx) = wait(py, rx);
        match got {
            Ok(out) => break Some(out),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if interrupted.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
                    break None;
                }
                if let Err(e) = py.check_signals() {
                    fail(&failed, &stop, e);
                    core::process::stop_all();
                    interrupted.get_or_insert_with(std::time::Instant::now);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessLockError::new_err("the built-in agent stopped unexpectedly (a panic); please report it")),
        }
    };
    if let Some(e) = failed.lock().ok().and_then(|mut first| first.take()) {
        return Err(e);
    }
    Ok(json_str(serde_json::json!(out)))
}

#[pymodule]
fn _hlock(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyManifest>()?;
    m.add_class::<PyObjective>()?;
    m.add_class::<PyBudget>()?;
    m.add_class::<PySearchSpace>()?;
    m.add_class::<PyLockfile>()?;
    m.add_class::<PyLockedAgent>()?;
    m.add_function(wrap_pyfunction!(resolve, m)?)?;
    m.add_function(wrap_pyfunction!(load, m)?)?;
    m.add_function(wrap_pyfunction!(lock_diff, m)?)?;
    m.add_function(wrap_pyfunction!(tune_eval, m)?)?;
    m.add_function(wrap_pyfunction!(upgrade, m)?)?;
    m.add_function(wrap_pyfunction!(tune, m)?)?;
    m.add_function(wrap_pyfunction!(harnesses, m)?)?;
    m.add_function(wrap_pyfunction!(runtimes, m)?)?;
    m.add_function(wrap_pyfunction!(harness_profile, m)?)?;
    m.add_function(wrap_pyfunction!(select_harness, m)?)?;
    m.add_function(wrap_pyfunction!(pin, m)?)?;
    m.add_function(wrap_pyfunction!(pull, m)?)?;
    m.add_function(wrap_pyfunction!(pull_find, m)?)?;
    m.add_function(wrap_pyfunction!(pull_parse, m)?)?;
    m.add_function(wrap_pyfunction!(backend_describe, m)?)?;
    m.add_function(wrap_pyfunction!(optimizer_describe, m)?)?;
    m.add_function(wrap_pyfunction!(pull_list, m)?)?;
    m.add_function(wrap_pyfunction!(pull_remove, m)?)?;
    m.add_function(wrap_pyfunction!(pull_env, m)?)?;
    m.add_function(wrap_pyfunction!(is_pull_source, m)?)?;
    m.add_function(wrap_pyfunction!(is_registry_ref, m)?)?;
    m.add_function(wrap_pyfunction!(pull_into, m)?)?;
    m.add_function(wrap_pyfunction!(registry_url, m)?)?;
    m.add_function(wrap_pyfunction!(registry_token_source, m)?)?;
    m.add_function(wrap_pyfunction!(registry_login_token, m)?)?;
    m.add_function(wrap_pyfunction!(registry_device_start, m)?)?;
    m.add_function(wrap_pyfunction!(registry_device_poll, m)?)?;
    m.add_function(wrap_pyfunction!(registry_logout, m)?)?;
    m.add_function(wrap_pyfunction!(registry_whoami, m)?)?;
    m.add_function(wrap_pyfunction!(registry_search, m)?)?;
    m.add_function(wrap_pyfunction!(registry_push, m)?)?;
    m.add_function(wrap_pyfunction!(registry_pull, m)?)?;
    m.add_function(wrap_pyfunction!(check_pin, m)?)?;
    m.add_function(wrap_pyfunction!(deps_prepare, m)?)?;
    m.add_function(wrap_pyfunction!(deps_commit, m)?)?;
    m.add_function(wrap_pyfunction!(deps_measure, m)?)?;
    m.add_function(wrap_pyfunction!(deps_adopt, m)?)?;
    m.add_function(wrap_pyfunction!(deps_remove, m)?)?;
    m.add_function(wrap_pyfunction!(deps_check, m)?)?;
    m.add_function(wrap_pyfunction!(deps_refresh, m)?)?;
    m.add_function(wrap_pyfunction!(deps_outdated, m)?)?;
    m.add_function(wrap_pyfunction!(deps_record_configs, m)?)?;
    m.add_function(wrap_pyfunction!(trust_list, m)?)?;
    m.add_function(wrap_pyfunction!(trust_approve, m)?)?;
    m.add_function(wrap_pyfunction!(audit, m)?)?;
    m.add_function(wrap_pyfunction!(scan_lock, m)?)?;
    m.add_function(wrap_pyfunction!(policy_check, m)?)?;
    m.add_function(wrap_pyfunction!(export_sbom, m)?)?;
    m.add_function(wrap_pyfunction!(deps_upgrade, m)?)?;
    m.add_function(wrap_pyfunction!(export_apm_lock, m)?)?;
    m.add_function(wrap_pyfunction!(sync_plan, m)?)?;
    m.add_function(wrap_pyfunction!(sync_run_step, m)?)?;
    m.add_function(wrap_pyfunction!(sync_skill, m)?)?;
    m.add_function(wrap_pyfunction!(sync_ignore_env, m)?)?;
    m.add_function(wrap_pyfunction!(secret_status, m)?)?;
    m.add_function(wrap_pyfunction!(secret_set, m)?)?;
    m.add_function(wrap_pyfunction!(secret_delete, m)?)?;
    m.add_function(wrap_pyfunction!(sync_dockerfile, m)?)?;
    m.add_function(wrap_pyfunction!(launch_prepare, m)?)?;
    m.add_function(wrap_pyfunction!(run_code_harness, m)?)?;
    m.add_function(wrap_pyfunction!(localize, m)?)?;
    m.add_function(wrap_pyfunction!(migrate, m)?)?;
    m.add_function(wrap_pyfunction!(check_eval_pack, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_plan, m)?)?;
    m.add_function(wrap_pyfunction!(tune_apply, m)?)?;
    m.add_function(wrap_pyfunction!(prepare_eval_workspace, m)?)?;
    m.add_function(wrap_pyfunction!(eval_packs_digest, m)?)?;
    m.add_function(wrap_pyfunction!(grade_eval_task, m)?)?;
    m.add_function(wrap_pyfunction!(sandbox_info, m)?)?;
    m.add_function(wrap_pyfunction!(sandbox_control_snapshot, m)?)?;
    m.add_function(wrap_pyfunction!(sandbox_session_begin, m)?)?;
    m.add_function(wrap_pyfunction!(sandbox_session_end, m)?)?;
    m.add_function(wrap_pyfunction!(calibrate_eval_pack, m)?)?;
    m.add_function(wrap_pyfunction!(decide, m)?)?;
    m.add_function(wrap_pyfunction!(decide_hook, m)?)?;
    m.add_function(wrap_pyfunction!(decider_drift, m)?)?;
    m.add_function(wrap_pyfunction!(redact_text, m)?)?;
    m.add_function(wrap_pyfunction!(agent_run, m)?)?;
    m.add_function(wrap_pyfunction!(meta_config, m)?)?;
    m.add_function(wrap_pyfunction!(meta_init, m)?)?;
    m.add_function(wrap_pyfunction!(meta_pin, m)?)?;
    m.add_function(wrap_pyfunction!(meta_check, m)?)?;
    m.add_function(wrap_pyfunction!(meta_prepare, m)?)?;
    m.add_function(wrap_pyfunction!(meta_verify, m)?)?;
    let py = m.py();
    m.add("HarnessLockError", py.get_type::<HarnessLockError>())?;
    m.add("ManifestError", py.get_type::<ManifestError>())?;
    m.add("LockfileError", py.get_type::<LockfileError>())?;
    m.add("EvalPackError", py.get_type::<EvalPackError>())?;
    m.add("StoreError", py.get_type::<StoreError>())?;
    m.add("TuningError", py.get_type::<TuningError>())?;
    m.add("ProfileError", py.get_type::<ProfileError>())?;
    m.add("WorkerError", py.get_type::<WorkerError>())?;
    m.add("PullError", py.get_type::<PullError>())?;
    m.add("RegistryError", py.get_type::<RegistryError>())?;
    m.add("DependencyError", py.get_type::<DependencyError>())?;
    m.add("AuditError", py.get_type::<AuditError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("LOCK_VERSION", core::LOCK_VERSION)?;
    Ok(())
}
