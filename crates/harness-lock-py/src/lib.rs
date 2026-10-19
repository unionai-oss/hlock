//! PyO3 Python bindings for **harness-lock**.
//!
//! Exposes the same user-facing API as the (reference) pure-Python `harness_lock`
//! package, backed by the Rust core (`harness-lock-core`): `Manifest`,
//! `SearchSpace`, `Objective`, `Budget`, `lock()`, `load()`, and the `Lockfile`
//! model.
//!
//! Dict-shaped data is exchanged as canonical JSON strings to keep the binding
//! surface small and robust.

use std::path::{Path, PathBuf};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use harness_lock_core as core;

fn py_err(e: core::Error) -> PyErr {
    PyValueError::new_err(e.to_string())
}

// --------------------------------------------------------------------------- //
// Manifest
// --------------------------------------------------------------------------- //
/// A parsed `harness.toml`.
#[pyclass(module = "harness_lock", name = "Manifest")]
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

    fn to_json(&self) -> String {
        serde_json::to_string(&self.inner.evals).unwrap_or_default()
    }
}

// --------------------------------------------------------------------------- //
// Objective / Budget
// --------------------------------------------------------------------------- //
#[pyclass(module = "harness_lock", name = "Objective")]
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
    fn constraints(&self) -> String {
        serde_json::to_string(&self.constraints).unwrap_or_default()
    }
}

#[pyclass(module = "harness_lock", name = "Budget")]
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
#[pyclass(module = "harness_lock", name = "SearchSpace")]
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
    ) -> Self {
        let parse = |s: Option<String>, fallback: serde_json::Value| -> serde_json::Value {
            match s {
                Some(txt) => serde_json::from_str(&txt).unwrap_or(fallback),
                None => fallback,
            }
        };
        PySearchSpace {
            llm: llm.unwrap_or_default(),
            decision: decision.unwrap_or_default(),
            harness: harness.unwrap_or_default(),
            policy: parse(policy, serde_json::json!({})),
            tools: parse(tools, serde_json::json!({})),
            skills: parse(skills, serde_json::json!({})),
            // `tasks` is the old name for `evals`
            evals: parse(evals.or(tasks), default_evals()),
            objective,
            budget,
        }
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
#[pyclass(module = "harness_lock", name = "Lockfile")]
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

    fn to_json(&self) -> String {
        serde_json::to_string(&self.inner).unwrap_or_default()
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
#[pyclass(module = "harness_lock", name = "LockedAgent")]
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
#[pyfunction]
#[pyo3(signature = (manifest=None, backend=None, workdir=None, harness=None, lock=None))]
fn resolve(
    py: Python<'_>,
    manifest: Option<PyObject>,
    backend: Option<String>,
    workdir: Option<String>,
    harness: Option<String>,
    lock: Option<String>,
) -> PyResult<PyLockfile> {
    let core_manifest = resolve_manifest(py, manifest, backend.as_deref())?;
    let root = core_manifest.root();
    let wd = workdir.map(PathBuf::from).unwrap_or_else(|| root.join(".harnesslock"));
    let lock = lock.map(PathBuf::from).unwrap_or_else(|| root.join(core::LOCKFILE_PATH));
    let backend = core::Backend::parse(core_manifest.backend_target().as_str());
    let mut resolver = core::Resolver::with_backend(core_manifest, &wd, backend);
    resolver.harness_override = harness;
    let lf = resolver.resolve_lock().map_err(py_err)?;
    lf.save(&lock).map_err(py_err)?;
    Ok(PyLockfile { inner: lf })
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
                return Err(PyValueError::new_err(
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
        workdir: workdir.map(PathBuf::from).unwrap_or_else(|| root.join(".harnesslock")),
        lock: Some(lock.map(PathBuf::from).unwrap_or_else(|| root.join(core::LOCKFILE_PATH))),
    };
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
}

#[pyfunction]
#[pyo3(signature = (manifest=None, depth="auto".to_string(), data="user".to_string(), harness=None, out=None, strict=false, apply=false, max_trials=None, backend=None, workdir=None, lock=None))]
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
) -> PyResult<String> {
    let m = resolve_manifest(py, manifest, backend.as_deref())?;
    let root = m.root();
    let spec = core::ops::TuneSpec {
        depth,
        data,
        harness,
        out: out.map(PathBuf::from).unwrap_or_else(|| root.join("harness.tune.lock")),
        strict,
        apply,
        max_trials,
        backend,
        workdir: workdir.map(PathBuf::from).unwrap_or_else(|| root.join(".harnesslock")),
        lock: Some(lock.map(PathBuf::from).unwrap_or_else(|| root.join(core::LOCKFILE_PATH))),
    };
    let o = core::ops::tune(m, &spec).map_err(py_err)?;
    Ok(serde_json::json!({
        "path": o.path.to_string_lossy(),
        "report": o.report,
        "summary": o.lock.summary(),
    })
    .to_string())
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
fn harnesses(probe: bool, manifest: Option<String>) -> PyResult<String> {
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
                "id": h.id, "name": h.name, "kind": h.kind, "aliases": h.aliases, "support": h.support,
                "origin": h.origin, "probe": p,
            })
        })
        .collect();
    Ok(json_str(serde_json::json!(rows)))
}

/// Every known inference-runtime profile with what's installed here, as JSON.
#[pyfunction]
#[pyo3(signature = (probe=true, manifest=None))]
fn runtimes(probe: bool, manifest: Option<String>) -> PyResult<String> {
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
}

/// One harness profile (by id or alias), its probe, and the paths it resolves to here.
#[pyfunction]
#[pyo3(signature = (name, manifest=None))]
fn harness_profile(name: &str, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
    let h = reg.require_harness(name).map_err(py_err)?;
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
}

/// Pick the target harness: `explicit`, then `HLOCK_HARNESS`, then
/// `manifest_target`, then the harness running this process, then the only
/// installed one. Returns JSON `{"id", "reason"}`.
#[pyfunction]
#[pyo3(signature = (explicit=None, manifest_target=None, manifest=None))]
fn select_harness(explicit: Option<String>, manifest_target: Option<String>, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
    let sel = reg.select(explicit.as_deref(), manifest_target.as_deref(), &ctx).map_err(py_err)?;
    Ok(json_str(serde_json::json!(sel)))
}

/// Capture the target harness's live setup into a pinned lock at `out`.
/// Returns JSON with the lock, the chosen harness, the diff from a previous pin,
/// vendored files, and warnings.
#[pyfunction]
#[pyo3(signature = (harness=None, out="harness.lock".to_string(), vendor=true, force=false, manifest=None))]
fn pin(harness: Option<String>, out: String, vendor: bool, force: bool, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, manifest) = registry(manifest.as_deref(), Some(&lock_dir(&out)))?;
    let out = if Path::new(&out).is_absolute() { out } else { std::env::current_dir().unwrap_or_default().join(&out).to_string_lossy().into_owned() };
    let target = manifest.as_ref().and_then(|m| m.target_harness());
    let project = manifest.as_ref().and_then(|m| m.project.get("name").and_then(|v| v.as_str()).map(str::to_string));
    let o = core::capture::pin(&reg, &ctx, harness.as_deref(), target.as_deref(), project.as_deref(), Path::new(&out), vendor, force)
        .map_err(py_err)?;
    Ok(json_str(serde_json::json!({
        "lock": o.lock,
        "path": o.path,
        "selection": o.selection,
        "diff": o.diff,
        "previous": o.previous,
        "vendored": o.vendored,
        "warnings": o.warnings,
    })))
}

/// Re-capture the harness a pinned lock is for; returns a JSON list of drift lines.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), manifest=None))]
fn check_pin(path: String, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let drift = core::capture::check_pin(&lock, &reg, &ctx).map_err(py_err)?;
    Ok(json_str(serde_json::json!(drift)))
}

/// Pull the lock at `source` (`github:…`, `gitlab:…`, `git:…`, or a repository URL) into
/// the store as `~/.hlock/pulls/<name>/harness.lock`, or into the project file `out`.
/// Returns JSON with the lock, its path and name, the source, the commit, the diff from a
/// lock it replaced, vendored files, other locks in the repo, and warnings.
#[pyfunction]
#[pyo3(signature = (source, out=None, name=None, force=false))]
fn pull(source: String, out: Option<String>, name: Option<String>, force: bool) -> PyResult<String> {
    let target = match out {
        Some(o) => core::pull::Target::File(std::env::current_dir().unwrap_or_default().join(o)),
        None => core::pull::Target::Store(name),
    };
    let o = core::pull::pull(&source, &target, force).map_err(py_err)?;
    Ok(json_str(serde_json::to_value(&o).map_err(|e| py_err(e.into()))?))
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

/// True if `arg` names a remote source (`github:`, `gitlab:`, `git:`, a URL) rather than a lock path.
#[pyfunction]
fn is_pull_source(arg: &str) -> bool {
    core::pull::is_source(arg)
}

fn sync_options(prefix: Option<String>, docker: Option<String>, install: bool, pull: bool, all_models: bool, exact: bool) -> core::sync::SyncOptions {
    core::sync::SyncOptions {
        prefix: prefix.map(PathBuf::from),
        docker: docker.map(PathBuf::from),
        install,
        pull,
        all_models,
        exact,
    }
}

/// Build the `hlock sync` plan for a lock as JSON. Nothing is changed.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), prefix=None, docker=None, install=false, pull=false, all_models=false, exact=false, manifest=None))]
fn sync_plan(path: String, prefix: Option<String>, docker: Option<String>, install: bool, pull: bool, all_models: bool, exact: bool, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let opts = sync_options(prefix, docker, install, pull, all_models, exact);
    let plan = core::sync::plan(&lock, Path::new(&path), &reg, &ctx, &opts).map_err(py_err)?;
    Ok(json_str(serde_json::json!(plan)))
}

/// Perform one `auto` step from a plan (its JSON); returns a message.
#[pyfunction]
fn sync_run_step(step: &str) -> PyResult<String> {
    let step: core::sync::Step = serde_json::from_str(step).map_err(|e| PyValueError::new_err(e.to_string()))?;
    core::sync::run_step(&step, &core::profiles::Ctx::from_process()).map_err(py_err)
}

/// The rehydrate Agent Skill (SKILL.md text) for a plan.
#[pyfunction]
#[pyo3(signature = (plan, lock_name="harness.lock".to_string()))]
fn sync_skill(plan: &str, lock_name: String) -> PyResult<String> {
    let plan: core::sync::Plan = serde_json::from_str(plan).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(core::sync::rehydrate_skill(&plan, &lock_name))
}

/// The Dockerfile and entrypoint for a docker-mode plan, as JSON.
#[pyfunction]
#[pyo3(signature = (plan, path="harness.lock".to_string(), manifest=None))]
fn sync_dockerfile(plan: &str, path: String, manifest: Option<String>) -> PyResult<String> {
    let plan: core::sync::Plan = serde_json::from_str(plan).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let (reg, _, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let profile = reg.require_harness(&plan.harness).map_err(py_err)?;
    let (dockerfile, entrypoint) = core::sync::dockerfile(&plan, &lock, profile);
    Ok(json_str(serde_json::json!({"dockerfile": dockerfile, "entrypoint": entrypoint})))
}

/// Prepare `hlock launch`: verify, materialize the isolated root, and return
/// JSON with the argv and environment to exec, plus what was done and notes.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), harness=None, prefix=None, allow_version_mismatch=false, in_place=false, args=Vec::new(), manifest=None, prompt=None))]
fn launch_prepare(
    path: String,
    harness: Option<String>,
    prefix: Option<String>,
    allow_version_mismatch: bool,
    in_place: bool,
    args: Vec<String>,
    manifest: Option<String>,
    prompt: Option<String>,
) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let opts = core::launch::LaunchOptions {
        prompt,
        target: harness,
        prefix: prefix.map(PathBuf::from),
        allow_version_mismatch,
        in_place,
        workspace: std::env::current_dir().ok(),
        ..Default::default()
    };
    let prep = core::launch::prepare(&lock, Path::new(&path), &reg, &ctx, &opts, &args).map_err(py_err)?;
    Ok(json_str(serde_json::json!(prep)))
}

/// Run a locked code harness (`[harnesses.<id>]`) on a prompt through the
/// execution contract. Returns JSON with success, output, cost, latency and log.
#[pyfunction]
#[pyo3(signature = (prompt, path="harness.lock".to_string(), workspace=None, timeout=600, manifest=None))]
fn run_code_harness(prompt: String, path: String, workspace: Option<String>, timeout: u64, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let lock = core::Lockfile::load(Path::new(&path)).map_err(py_err)?;
    let profile = reg.require_harness(&lock.harness.name).map_err(py_err)?;
    if profile.kind != "custom" {
        return Err(PyValueError::new_err(format!("{} isn't a code harness", profile.id)));
    }
    // the agent works where the user is; the project root stays on PYTHONPATH
    let ws = workspace.map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| ctx.cwd.clone());
    let inv = core::contract::invocation(profile, Some(&lock), Some(Path::new(&path)), None, &prompt, &ws, 0, &ctx).map_err(py_err)?;
    let r = core::contract::run(&inv, &ctx, std::time::Duration::from_secs(timeout)).map_err(py_err)?;
    Ok(json_str(serde_json::json!(r)))
}

/// Write a localized (all-local) copy of a lock to `out`. Evaluates candidates on
/// `evals` (an eval pack path; `tasks` is its old name) or `./harness.toml`'s eval
/// packs when available.
/// Returns JSON with the lock and the localize report.
#[pyfunction]
#[pyo3(signature = (path="harness.lock".to_string(), out="harness.local.lock".to_string(), harness=None, runtime=None, models=Vec::new(), memory_gb=None, evals=None, tasks=None, strict=false, allow_network=false, workdir=None, manifest=None))]
fn localize(
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
        workdir: workdir.map(PathBuf::from).unwrap_or_else(|| ctx.cwd.join(".harnesslock/localize")),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let abs = |p: &str| if Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) };
    let o = core::localize::localize_to(&abs(&path), &abs(&out), &reg, &ctx, &opts).map_err(py_err)?;
    Ok(json_str(serde_json::json!({"lock": o.lock, "report": o.report, "path": out})))
}

/// Free text with credential-looking words replaced by `[redacted]`.
#[pyfunction]
fn redact_text(text: String) -> String {
    core::capture::redact_text(&text)
}

/// Load the eval pack at `path` and check it (see `EvalPack::check`). Returns JSON
/// with the pack's header, its task ids, and the issues found.
#[pyfunction]
fn check_eval_pack(path: String) -> PyResult<String> {
    let pack = core::tasks::EvalPack::load(&path, None).map_err(py_err)?;
    let issues = pack.check();
    Ok(json_str(serde_json::json!({
        "name": pack.name,
        "version": pack.version,
        "domain": pack.domain,
        "path": pack.path,
        "hash": pack.hash,
        "tasks": pack.tasks.iter().map(|t| &t.id).collect::<Vec<_>>(),
        "issues": issues,
    })))
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
fn meta_init(harness: Option<String>, provider: Option<String>, model: Option<String>, runtime: Option<String>, memory_gb: Option<f64>, base_url: Option<String>, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), None)?;
    let opts = core::meta::InitOptions { harness, provider, model, runtime, memory_gb, base_url };
    let c = core::meta::choose(&reg, &ctx, &opts).map_err(py_err)?;
    Ok(json_str(serde_json::json!({"choice": c, "manifest": core::meta::manifest_text(&c)})))
}

/// Pin the meta harness into `out`: write `harness.meta.toml`'s `[model]` into the
/// meta harness's own config root, then capture that config.
#[pyfunction]
#[pyo3(signature = (out="harness.meta.lock".to_string(), meta_manifest="harness.meta.toml".to_string(), vendor=true, force=false, manifest=None))]
fn meta_pin(out: String, meta_manifest: String, vendor: bool, force: bool, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, manifest) = registry(manifest.as_deref(), Some(&lock_dir(&out)))?;
    let out = if Path::new(&out).is_absolute() { out } else { std::env::current_dir().unwrap_or_default().join(&out).to_string_lossy().into_owned() };
    let meta = self::meta_manifest(Some(&meta_manifest))?
        .ok_or_else(|| PyValueError::new_err(format!("cannot read {meta_manifest}: no such file; create one with `hlock meta init`")))?;
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
}

/// Drift between `harness.meta.toml`, the meta lock, and the meta harness's own
/// config, as a JSON list of lines (empty when they match).
#[pyfunction]
#[pyo3(signature = (path="harness.meta.lock".to_string(), meta_manifest=None, manifest=None))]
fn meta_check(path: String, meta_manifest: Option<String>, manifest: Option<String>) -> PyResult<String> {
    let (reg, ctx, _) = registry(manifest.as_deref(), Some(&lock_dir(&path)))?;
    let meta = self::meta_manifest(meta_manifest.as_deref())?;
    let abs = if Path::new(&path).is_absolute() { PathBuf::from(&path) } else { std::env::current_dir().unwrap_or_default().join(&path) };
    let lock = core::Lockfile::load(&abs).map_err(py_err)?;
    let drift = core::meta::check(&reg, &ctx, meta.as_ref(), &lock, &abs).map_err(py_err)?;
    Ok(json_str(serde_json::json!(drift)))
}

/// Prepare a meta job: render its prompt and build the command that runs it in the
/// meta harness pinned in `meta_lock`. Returns JSON (see `meta::Prepared`).
#[pyfunction]
#[pyo3(signature = (job, lock="harness.lock".to_string(), meta_lock="harness.meta.lock".to_string(), meta_manifest=None, manifest=None, out=None, count=10, instructions=String::new(), guide=None, headless=None, allow_version_mismatch=false, dry_run=false))]
#[allow(clippy::too_many_arguments)]
fn meta_prepare(
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
) -> PyResult<String> {
    let meta = self::meta_manifest(meta_manifest.as_deref())?;
    if !Path::new(&meta_lock).is_file() {
        return Err(PyValueError::new_err(format!("no meta harness pinned: {meta_lock} doesn't exist; pin one with `hlock meta pin`")));
    }
    let (input, reg, ctx) = meta_input(&lock, &meta_lock, manifest.as_deref(), out, count, instructions, guide)?;
    let ml = core::Lockfile::load(&input.meta_lock_path).map_err(py_err)?;
    let opts = core::meta::RunOptions { headless, allow_version_mismatch, dry_run };
    let prep = core::meta::prepare(&job, &input, meta.as_ref(), &ml, &reg, &ctx, &opts).map_err(py_err)?;
    Ok(json_str(serde_json::json!(prep)))
}

/// hlock's own check of a meta job's result. Returns JSON (see `meta::Verdict`).
#[pyfunction]
#[pyo3(signature = (job, lock="harness.lock".to_string(), meta_lock="harness.meta.lock".to_string(), meta_manifest=None, manifest=None, out=None, count=0))]
fn meta_verify(job: String, lock: String, meta_lock: String, meta_manifest: Option<String>, manifest: Option<String>, out: Option<String>, count: usize) -> PyResult<String> {
    let meta = self::meta_manifest(meta_manifest.as_deref())?;
    let (input, reg, ctx) = meta_input(&lock, &meta_lock, manifest.as_deref(), out, count, String::new(), None)?;
    let v = core::meta::verify(&job, &input, meta.as_ref(), &reg, &ctx).map_err(py_err)?;
    Ok(json_str(serde_json::json!(v)))
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
    let cfg = core::agent::config_from_lock(&lock, &project, interactive, &ctx.env).map_err(py_err)?;
    // release the GIL while the agent waits on the model and on commands; take it back
    // only to call the callbacks, which also gives Ctrl-C a chance to stop the run
    let out = py.allow_threads(|| {
        let mut say_fn = |kind: &str, text: &str| -> bool {
            Python::with_gil(|py| {
                if let Some(f) = &say {
                    match f.call1(py, (kind, text)) {
                        Ok(r) if !r.bind(py).is_truthy().unwrap_or(true) => return false,
                        Ok(_) => {}
                        Err(_) => return false,
                    }
                }
                py.check_signals().is_ok()
            })
        };
        let mut ask_fn = |command: &str| -> String {
            Python::with_gil(|py| {
                ask.as_ref()
                    .and_then(|f| f.call1(py, (command,)).ok())
                    .and_then(|r| r.extract::<String>(py).ok())
                    .unwrap_or_else(|| "n".into())
            })
        };
        core::agent::run(&cfg, &prompt, &mut say_fn, &mut ask_fn)
    });
    Ok(json_str(serde_json::json!(out)))
}

#[pymodule]
fn _harness_lock(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyManifest>()?;
    m.add_class::<PyObjective>()?;
    m.add_class::<PyBudget>()?;
    m.add_class::<PySearchSpace>()?;
    m.add_class::<PyLockfile>()?;
    m.add_class::<PyLockedAgent>()?;
    m.add_function(wrap_pyfunction!(resolve, m)?)?;
    m.add_function(wrap_pyfunction!(load, m)?)?;
    m.add_function(wrap_pyfunction!(upgrade, m)?)?;
    m.add_function(wrap_pyfunction!(tune, m)?)?;
    m.add_function(wrap_pyfunction!(harnesses, m)?)?;
    m.add_function(wrap_pyfunction!(runtimes, m)?)?;
    m.add_function(wrap_pyfunction!(harness_profile, m)?)?;
    m.add_function(wrap_pyfunction!(select_harness, m)?)?;
    m.add_function(wrap_pyfunction!(pin, m)?)?;
    m.add_function(wrap_pyfunction!(pull, m)?)?;
    m.add_function(wrap_pyfunction!(pull_find, m)?)?;
    m.add_function(wrap_pyfunction!(pull_list, m)?)?;
    m.add_function(wrap_pyfunction!(pull_remove, m)?)?;
    m.add_function(wrap_pyfunction!(pull_env, m)?)?;
    m.add_function(wrap_pyfunction!(is_pull_source, m)?)?;
    m.add_function(wrap_pyfunction!(check_pin, m)?)?;
    m.add_function(wrap_pyfunction!(sync_plan, m)?)?;
    m.add_function(wrap_pyfunction!(sync_run_step, m)?)?;
    m.add_function(wrap_pyfunction!(sync_skill, m)?)?;
    m.add_function(wrap_pyfunction!(sync_dockerfile, m)?)?;
    m.add_function(wrap_pyfunction!(launch_prepare, m)?)?;
    m.add_function(wrap_pyfunction!(run_code_harness, m)?)?;
    m.add_function(wrap_pyfunction!(localize, m)?)?;
    m.add_function(wrap_pyfunction!(check_eval_pack, m)?)?;
    m.add_function(wrap_pyfunction!(redact_text, m)?)?;
    m.add_function(wrap_pyfunction!(agent_run, m)?)?;
    m.add_function(wrap_pyfunction!(meta_config, m)?)?;
    m.add_function(wrap_pyfunction!(meta_init, m)?)?;
    m.add_function(wrap_pyfunction!(meta_pin, m)?)?;
    m.add_function(wrap_pyfunction!(meta_check, m)?)?;
    m.add_function(wrap_pyfunction!(meta_prepare, m)?)?;
    m.add_function(wrap_pyfunction!(meta_verify, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("LOCK_VERSION", core::LOCK_VERSION)?;
    Ok(())
}
