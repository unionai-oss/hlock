//! Operational commands built on the resolver: `hlock upgrade` (§5.8) and
//! `hlock tune` (§5.9, §8.5).
//!
//! - **upgrade** moves locked components to newer/breaking versions, keeping a
//!   `harness.lock.prev` for `--rollback`, and gates the apply on evidence not
//!   regressing.
//! - **tune** is the compute-heavy step: it searches hyperparameter/decision
//!   axes (and, at deeper depths, records prompt edits and fine-tuning) to produce
//!   a derived `harness.tune.lock` (§5.9) that re-measures gains vs the base lock.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::hash::sha256_text;
use crate::lockfile::{Decision, Evidence, Harness, Lockfile};
use crate::manifest::Manifest;
use crate::search::{Stack, StackScore};
use crate::resolver::Resolver;
use crate::LOCKFILE_PATH;

// --------------------------------------------------------------------------- //
// Reference-model ladder (an upgrade graph, local/offline stand-in for real
// registry version resolvers).
// --------------------------------------------------------------------------- //
pub const REFERENCE_MODELS: [&str; 3] = ["qwen-1.5b", "qwen-7b", "qwen-32b"];

pub fn latest_model() -> &'static str {
    "qwen-32b"
}

pub fn next_model(model: &str) -> Option<String> {
    let idx = REFERENCE_MODELS.iter().position(|m| *m == model)?;
    REFERENCE_MODELS.get(idx + 1).map(|s| s.to_string())
}

/// Resolve the target model for an upgrade of `model`.
fn target_for(model: &str, to: Option<&str>, all: bool) -> Option<String> {
    if let Some(t) = to {
        return Some(t.to_string());
    }
    if all {
        return Some(latest_model().to_string());
    }
    next_model(model)
}

// --------------------------------------------------------------------------- //
// Upgrade
// --------------------------------------------------------------------------- //
#[derive(Debug, Clone, Default)]
pub struct UpgradeSpec {
    /// Explicit target model(s); only planner is downgrade/upgrade-able here.
    pub to: Option<String>,
    /// Upgrade every upgradeable role to the latest.
    pub all: bool,
    /// Perform the apply (default is plan/dry-run).
    pub apply: bool,
    /// Restore `harness.lock` from `harness.lock.prev`.
    pub rollback: bool,
    /// Skip the evidence gate (apply even if success-rate regresses).
    pub force: bool,
    /// Where `.hlock/` store + runs live.
    pub workdir: PathBuf,
    /// The lock to upgrade (default `./harness.lock`).
    pub lock: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct UpgradeOutcome {
    pub plan: serde_json::Value,
    pub applied: bool,
    pub rolled_back: bool,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub lock_path: PathBuf,
    pub message: String,
}


/// Compute the upgrade plan (role -> from -> to) from a lock (or manifest).
pub fn plan_upgrade(lock: Option<&Lockfile>, manifest: &Manifest, spec: &UpgradeSpec) -> serde_json::Value {
    let mut roles: BTreeMap<String, String> = BTreeMap::new();
    if let Some(l) = lock {
        for llm in &l.llm {
            roles.insert(llm.role.clone(), llm.model.clone());
        }
    } else if let Some(first) = manifest
        .candidates
        .llm
        .get("planner")
        .and_then(|c| c.any_of.first())
    {
        roles.insert("planner".to_string(), model_of(first));
    }
    let mut entries = Vec::new();
    for (role, model) in &roles {
        // `--to` names the planner's target, not every role's
        if spec.to.is_some() && role != "planner" {
            continue;
        }
        if let Some(t) = target_for(model, spec.to.as_deref(), spec.all) {
            entries.push(serde_json::json!({"role": role, "from": model, "to": t}));
        }
    }
    // deciders: a newer version of the same model
    for d in lock.map(|l| l.decider.as_slice()).unwrap_or_default() {
        if let Some(t) = crate::decide::newer_version(&d.provider, &d.model) {
            entries.push(serde_json::json!({
                "role": format!("decider · {}", d.use_),
                "decider": d.use_,
                "from": format!("{}:{}", d.provider, d.model),
                "to": format!("{}:{t}", d.provider),
            }));
        }
    }
    serde_json::json!({"upgradeable": entries})
}

fn model_of(spec: &str) -> String {
    spec.rsplit_once(':').map(|(_, m)| m.to_string()).unwrap_or_else(|| spec.to_string())
}

/// `hlock upgrade`. `manifest` should be loaded from `harness.toml`.
pub fn upgrade(manifest: Manifest, spec: &UpgradeSpec) -> Result<UpgradeOutcome> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let lock_path = spec.lock.clone().unwrap_or_else(|| cwd.join(LOCKFILE_PATH));
    let prev_path = PathBuf::from(format!("{}.prev", lock_path.display()));

    // ---- rollback ---- (even from a lock that no longer parses)
    if spec.rollback {
        let current_lock = Lockfile::load(&lock_path).ok();
        let mut outcome = UpgradeOutcome {
            plan: plan_upgrade(current_lock.as_ref(), &manifest, spec),
            applied: false,
            rolled_back: false,
            before: current_lock.as_ref().map(baseline_sr),
            after: None,
            lock_path: lock_path.clone(),
            message: String::new(),
        };
        if prev_path.exists() {
            std::fs::copy(&prev_path, &lock_path)?;
            outcome.rolled_back = true;
            outcome.message = "restored the previous harness.lock from harness.lock.prev".into();
            outcome.after = Lockfile::load(&lock_path).ok().map(|l| baseline_sr(&l));
            return Ok(outcome);
        }
        return Err(Error::Tuning("harness.lock.prev not found — nothing to roll back".into()));
    }

    // a missing lock plans from the manifest; one that doesn't load is an error, not a
    // baseline of 0 that any upgrade beats
    let current_lock = load_if_present(&lock_path)?;
    let plan = plan_upgrade(current_lock.as_ref(), &manifest, spec);
    let mut outcome = UpgradeOutcome {
        plan: plan.clone(),
        applied: false,
        rolled_back: false,
        before: current_lock.as_ref().map(baseline_sr),
        after: None,
        lock_path: lock_path.clone(),
        message: String::new(),
    };

    // ---- plan only (dry-run) ----
    let upgradeable = plan.get("upgradeable").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    if !spec.apply {
        if upgradeable.is_empty() {
            outcome.message = "nothing to upgrade (already at the latest).".into();
        } else {
            outcome.message = format!(
                "plan: {} upgrade(s) available — rerun with --apply to perform.",
                upgradeable.len()
            );
        }
        return Ok(outcome);
    }

    if upgradeable.is_empty() {
        outcome.message = "nothing to apply (no upgradeable component).".into();
        return Ok(outcome);
    }

    // ---- apply: derive a manifest targeting the upgraded models and deciders ----
    let (decider_ups, llm_ups): (Vec<&serde_json::Value>, Vec<&serde_json::Value>) = upgradeable.iter().partition(|e| e.get("decider").is_some());
    // each role's own target; a role without an upgrade (or a decider-only upgrade) keeps its models
    let targets: Vec<(String, String)> = llm_ups
        .iter()
        .filter_map(|e| Some((e["role"].as_str()?.to_string(), e["to"].as_str()?.to_string())))
        .collect();

    if lock_path.exists() {
        std::fs::copy(&lock_path, &prev_path)?; // keep harness.lock.prev for --rollback
    }
    let mut derived = manifest.clone();
    let mut cand = crate::manifest::Candidates::default();
    cand.llm = manifest.candidates.llm.clone();
    for (role, to) in &targets {
        // the provider the lock has for this role
        let provider = current_lock.as_ref().and_then(|l| l.llm.iter().find(|x| &x.role == role)).map(|x| x.provider.clone()).unwrap_or_else(|| "reference".into());
        let spec_str = format!("{provider}:{to}");
        cand.llm
            .entry(role.clone())
            .and_modify(|c| c.any_of = vec![spec_str.clone()])
            .or_insert(crate::manifest::LLMCandidate { any_of: vec![spec_str] });
    }
    cand.decision = manifest.candidates.decision.clone();
    cand.harness = manifest.candidates.harness.clone();
    cand.policy = manifest.candidates.policy.clone();
    derived.candidates = cand;
    for e in &decider_ups {
        let (Some(use_), Some(to)) = (e["decider"].as_str(), e["to"].as_str()) else { continue };
        let mut spec = derived.deciders.get(use_).cloned().unwrap_or_else(|| serde_json::json!({}));
        if let Some(m) = spec.as_object_mut() {
            m.remove("any-of");
            m.insert("model".into(), serde_json::json!(to));
        }
        derived.deciders.insert(use_.to_string(), spec);
    }

    let mut resolver = Resolver::new(derived, &spec.workdir);
    let new_lock = resolver.resolve_lock()?;
    new_lock.save(&lock_path)?;

    let after = baseline_sr(&new_lock);
    outcome.after = Some(after);
    let before = outcome.before;
    let regressed = before.map(|b| after < b).unwrap_or(false);
    if regressed && !spec.force {
        // evidence gate: roll back automatically
        if prev_path.exists() {
            std::fs::copy(&prev_path, &lock_path)?;
        }
        outcome.applied = false;
        outcome.message = format!(
            "upgrade regressed success-rate ({:?} -> {:.2}) and was rolled back; use --force to apply anyway.",
            before, after
        );
        return Ok(outcome);
    }
    outcome.applied = true;
    let what: Vec<String> = targets.iter().map(|(role, t)| format!("{role} to {t}")).chain(decider_ups.iter().map(|e| format!("the {} decider to {}", e["decider"].as_str().unwrap_or("?"), e["to"].as_str().unwrap_or("?")))).collect();
    outcome.message = format!(
        "upgraded {}: success-rate {:.2} -> {:.2}. previous lock kept at harness.lock.prev.",
        what.join(" and "),
        before.unwrap_or(0.0),
        after
    );
    Ok(outcome)
}

/// The lock at `path`, or `None` when there's no file there. A file that can't be read
/// or parsed is an error.
fn load_if_present(path: &std::path::Path) -> Result<Option<Lockfile>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => Lockfile::load(path).map(Some),
    }
}

fn baseline_sr(l: &Lockfile) -> f64 {
    l.evidence.success_rate.get("mean").copied().unwrap_or(0.0)
}

// --------------------------------------------------------------------------- //
// Tune
// --------------------------------------------------------------------------- //
#[derive(Debug, Clone)]
pub struct TuneSpec {
    /// probe | prompt | hpo | ft | full | auto
    pub depth: String,
    /// "user" | "user:<paths>" | "synthetic[:n]" | "hub:<pack>"
    pub data: String,
    pub harness: Option<String>,
    /// Output path (default `harness.tune.lock`; user-renamable).
    pub out: PathBuf,
    pub strict: bool,
    pub apply: bool,
    pub max_trials: Option<usize>,
    /// local | devbox | cluster (overrides the manifest's `[backend] target`).
    pub backend: Option<String>,
    pub workdir: PathBuf,
    /// The base lock (default `./harness.lock`); `apply` writes back to it.
    pub lock: Option<PathBuf>,
}

impl Default for TuneSpec {
    fn default() -> Self {
        TuneSpec {
            depth: "auto".into(),
            data: "user".into(),
            harness: None,
            out: PathBuf::from("harness.tune.lock"),
            strict: false,
            apply: false,
            max_trials: None,
            backend: None,
            workdir: PathBuf::from(".hlock"),
            lock: None,
        }
    }
}

impl TuneSpec {
    pub fn resolved_depth(&self) -> &str {
        match self.depth.as_str() {
            "auto" => "hpo",
            other => other,
        }
    }
}

/// The stack `tune` starts from: the base lock's (its harness, models, skills, tools,
/// instructions, deciders and policy), else one built from the manifest's candidates.
fn base_stack(base: Option<&Lockfile>, manifest: &Manifest, harness: Option<&str>) -> Stack {
    let mut s = match base {
        Some(lock) => {
            let mut s = crate::deps::lock_stack(lock);
            s.decision = if lock.decision.kind.is_empty() { "none".into() } else { lock.decision.kind.clone() };
            s.retries = lock.policy.retries;
            let params = lock.llm.iter().find(|l| l.role == "planner").or(lock.llm.first()).map(|l| l.params.clone()).unwrap_or_default();
            s.temperature = Some(params.get("temperature").and_then(serde_json::Value::as_f64).unwrap_or(0.2));
            s.tool_timeout_seconds = Some(params.get("tool_timeout_seconds").and_then(serde_json::Value::as_u64).unwrap_or(120));
            s
        }
        None => Stack {
            llm: BTreeMap::from([("planner".to_string(), manifest.candidates.llm.get("planner").and_then(|c| c.any_of.first().cloned()).unwrap_or_else(|| "reference:qwen-7b".into()))]),
            harness: manifest.target_harness().or_else(|| manifest.candidates.harness.first().map(|h| h.name.clone())).unwrap_or_else(|| "reference".into()),
            decision: manifest.candidates.decision.first().cloned().unwrap_or_else(|| "none".into()),
            tools: manifest.tools.keys().cloned().collect(),
            skills: manifest.skills.keys().cloned().collect(),
            temperature: Some(0.2),
            tool_timeout_seconds: Some(120),
            ..Default::default()
        },
    };
    if let Some(h) = harness {
        s.harness = h.to_string();
    }
    s
}

/// The role `tune` varies the model of: the planner, else the base's first role.
fn tuned_role(base: &Stack) -> String {
    if base.llm.contains_key("planner") || base.llm.is_empty() { "planner".into() } else { base.llm.keys().next().cloned().unwrap_or_default() }
}

/// The configurations `tune` tries: the base stack, varied over what tune tunes
/// (retries, temperature, and at `full` the tool timeout), and over the planner model
/// when `[candidates.llm.planner]` lists more than one. Deeper depths widen the grid.
fn tune_grid(base: &Stack, manifest: &Manifest, depth: &str) -> Vec<Stack> {
    let role = tuned_role(base);
    let cands: Vec<String> = manifest.candidates.llm.get("planner").map(|c| c.any_of.clone()).unwrap_or_default();
    let models: Vec<Option<String>> = if cands.len() > 1 { cands.into_iter().map(Some).collect() } else { vec![base.llm.get(&role).cloned()] };
    let retries: Vec<u32> = match depth {
        "probe" => vec![0],
        "prompt" => vec![0, 1],
        _ => vec![0, 1, 3],
    };
    let temps: Vec<f64> = if matches!(depth, "hpo" | "ft" | "full") { vec![0.2, 0.7] } else { vec![0.2] };
    let timeouts: Vec<u64> = if depth == "full" { vec![30, 120] } else { vec![120] };
    let mut out = Vec::new();
    for m in &models {
        for &r in &retries {
            for &t in &temps {
                for &tt in &timeouts {
                    let mut s = base.clone();
                    if let Some(m) = m {
                        s.llm.insert(role.clone(), m.clone());
                    }
                    s.retries = r;
                    s.temperature = Some(t);
                    s.tool_timeout_seconds = Some(tt);
                    out.push(s);
                }
            }
        }
    }
    out
}

/// A stage of the tune pipeline, for the report: whether it ran, and why not.
fn stage(name: &str, ran: bool, detail: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"stage": name, "ran": ran, "detail": detail.into()})
}

/// `hlock tune` -> `harness.tune.lock` (§5.9).
///
/// Starts from the base lock's stack and varies only what tune tunes: **data** ->
/// (depth>=prompt) **prompt search** -> **HPO trials** -> (depth>=ft) **LoRA
/// fine-tune** -> (depth==full) **holdout re-validation**. The base configuration is
/// measured on the same tasks first, as the baseline. Stages that need the worker run
/// on `devbox`/`cluster` only, and a stage that didn't run isn't recorded.
pub fn tune(manifest: Manifest, spec: &TuneSpec) -> Result<TuneOutcome> {
    manifest.check_search()?;
    let depth = spec.resolved_depth().to_string();
    let backend = crate::worker::Backend::parse(spec.backend.as_deref().unwrap_or(manifest.backend_target().as_str()));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let lock_path = spec.lock.clone().unwrap_or_else(|| cwd.join(LOCKFILE_PATH));
    let base_lock = load_if_present(&lock_path)?;
    let base = base_stack(base_lock.as_ref(), &manifest, spec.harness.as_deref());

    // trials run the base's harness: its code harness for real, else the reference adapter
    let mut m = manifest.clone();
    m.candidates.harness = vec![crate::manifest::HarnessCandidate { name: base.harness.clone(), ..Default::default() }];
    let mut resolver = Resolver::with_backend(m, &spec.workdir, backend);
    resolver.harness_override = Some(base.harness.clone());
    resolver.prepare_trials()?;
    let notes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = notes.clone();
    *resolver.on_event.lock().map_err(|_| Error::Tuning("tune: event channel poisoned".into()))? = Some(Box::new(move |e: serde_json::Value| {
        if let (Some("note"), Some(t)) = (e["kind"].as_str(), e["text"].as_str()) {
            if let Ok(mut n) = sink.lock() {
                n.push(t.to_string());
            }
        }
    }));
    let note = |t: String| {
        if let Ok(mut n) = notes.lock() {
            n.push(t);
        }
    };
    let search_tasks = resolver.load_packs();
    if search_tasks.is_empty() {
        return Err(Error::Tuning("no tasks loaded — cannot tune".into()));
    }
    let seeds = manifest.seeds();
    let ws_root = spec.workdir.join("runs");
    std::fs::create_dir_all(&ws_root)?;
    let worker = backend.needs_worker().then(crate::worker::PythonWorker::new);
    if worker.is_none() && (prompt_depth(&depth) && depth != "hpo" || ft_depth(&depth)) {
        note(format!("only the HPO grid runs on the {} backend; the data, prompt and fine-tune stages need devbox or cluster", backend.as_str()));
    }

    let n_synthetic = parse_synthetic(&spec.data);
    let data_mode = if n_synthetic > 0 { "synthetic" } else { "user-traces" };
    let grid = tune_grid(&base, &manifest, &depth);
    let plan_value = serde_json::json!({
        "depth": depth,
        "data": spec.data,
        "data_mode": data_mode,
        "harness": base.harness,
        "max_trials": spec.max_trials,
        "axes": {
            "retries": grid.iter().map(|s| s.retries).collect::<std::collections::BTreeSet<_>>(),
            "temperature": grid.iter().filter_map(|s| s.temperature).map(|t| (t * 100.0) as i64).collect::<std::collections::BTreeSet<_>>().into_iter().map(|t| t as f64 / 100.0).collect::<Vec<_>>(),
            "tool_timeout_seconds": grid.iter().filter_map(|s| s.tool_timeout_seconds).collect::<std::collections::BTreeSet<_>>(),
        },
    });
    let mut stages = Vec::new();

    // ---- stage 1: data (user traces or synthetic) ----
    let mut data_meta = serde_json::json!({});
    match &worker {
        Some(w) => {
            let kind = if n_synthetic > 0 { "data.synthetic" } else { "data.load" };
            match stage_run(w, backend.as_str(), kind, &ws_root.join("stage_data"), &plan_value) {
                Ok(v) => {
                    let rows = v.get("rows").and_then(serde_json::Value::as_u64).unwrap_or(0);
                    if rows == 0 && n_synthetic == 0 {
                        note(format!("--data {} found no traces, so there's no data to tune on", spec.data));
                    }
                    stages.push(stage("data", rows > 0, format!("{rows} rows")));
                    if rows > 0 {
                        data_meta = v;
                    }
                }
                Err(Error::Worker(e)) => {
                    note(format!("tune data stage: {e}"));
                    stages.push(stage("data", false, e));
                }
                Err(e) => return Err(e),
            }
        }
        None => stages.push(stage("data", false, format!("needs devbox or cluster, not {}", backend.as_str()))),
    }

    // ---- stage 2: prompt search (depth >= prompt) ----
    let mut prompt_meta = serde_json::json!({});
    let mut tuned_prompt: Vec<String> = Vec::new();
    if prompt_depth(&depth) {
        match &worker {
            Some(w) => match stage_run(w, backend.as_str(), "prompt.search", &ws_root.join("stage_prompt"), &plan_value) {
                Ok(v) => {
                    if let Some(p) = v.get("best_prompt").and_then(serde_json::Value::as_str).filter(|_| v.get("_placeholder").is_none()) {
                        tuned_prompt = vec![p.to_string()];
                        prompt_meta = v;
                    }
                    stages.push(stage("prompt", !tuned_prompt.is_empty(), if tuned_prompt.is_empty() { "no prompt found" } else { "searched" }));
                }
                Err(Error::Worker(e)) => {
                    note(format!("tune prompt stage: {e}"));
                    stages.push(stage("prompt", false, e));
                }
                Err(e) => return Err(e),
            },
            None => stages.push(stage("prompt", false, format!("needs devbox or cluster, not {}", backend.as_str()))),
        }
    }
    let with_prompt = |s: &Stack| {
        let mut s = s.clone();
        s.instructions.extend(tuned_prompt.iter().cloned());
        s
    };

    // ---- baseline: the base configuration, on the same tasks and seeds ----
    let pairs: Vec<(&crate::tasks::Task, u64)> = (0..seeds).flat_map(|seed| search_tasks.iter().map(move |t| (t, seed))).collect();
    let run_all = |stack: &Stack| -> Vec<crate::search::TrialResult> { pairs.iter().map(|(t, seed)| resolver.run_trial(stack, t, *seed, &ws_root)).collect() };
    let baseline_trials = run_all(&base);
    let baseline = StackScore { stack: base.clone(), n: baseline_trials.len(), trials: baseline_trials };

    // ---- stage 3: HPO trials over the grid, the budget spent round-robin ----
    let configs: Vec<Stack> = grid.iter().map(&with_prompt).collect();
    let mut results: Vec<Vec<crate::search::TrialResult>> = vec![Vec::new(); configs.len()];
    let max = spec.max_trials.unwrap_or(usize::MAX);
    let mut n = 0usize;
    'budget: for (i, (task, seed)) in pairs.iter().enumerate() {
        for (cfg, out) in configs.iter().zip(results.iter_mut()) {
            // the base configuration's trials are already the baseline's
            if cfg.hash() == base.hash() {
                out.push(baseline.trials[i].clone());
                continue;
            }
            if n >= max {
                break 'budget;
            }
            out.push(resolver.run_trial(cfg, task, *seed, &ws_root));
            n += 1;
        }
    }
    if n >= max && spec.max_trials.is_some() {
        note(format!("--max-trials {max} cut the grid; configurations were measured round-robin"));
    }
    let scored: Vec<StackScore> = configs.iter().cloned().zip(results).filter(|(_, t)| !t.is_empty()).map(|(stack, trials)| StackScore { stack, n: trials.len(), trials }).collect();
    stages.push(stage("hpo", true, format!("{} configs", scored.len())));

    // the best: highest success, then lowest cost, then closest to the base
    let role = tuned_role(&base);
    let distance = |s: &Stack| (s.retries.abs_diff(base.retries), s.temperature != base.temperature, s.llm.get(&role) != base.llm.get(&role), s.tool_timeout_seconds != base.tool_timeout_seconds);
    let winner = scored
        .iter()
        .min_by(|a, b| b.success_rate().total_cmp(&a.success_rate()).then(a.mean_cost().total_cmp(&b.mean_cost())).then(distance(&a.stack).cmp(&distance(&b.stack))))
        .cloned()
        .ok_or_else(|| Error::Tuning("tune produced no candidate".into()))?;

    if spec.strict && winner.success_rate() <= baseline.success_rate() {
        return Err(Error::Tuning(format!(
            "--strict: tuned success-rate {:.2} does not beat the base configuration's {:.2}, measured on the same tasks",
            winner.success_rate(),
            baseline.success_rate()
        )));
    }

    // what the tuned prompt adds: the winner with and without it
    let prompt_gain = (!tuned_prompt.is_empty()).then(|| {
        let mut plain = winner.stack.clone();
        plain.instructions.retain(|i| !tuned_prompt.contains(i));
        let t = run_all(&plain);
        winner.success_rate() - StackScore { stack: plain, n: t.len(), trials: t }.success_rate()
    });

    // ---- stage 4: fine-tune (depth >= ft) ----
    let mut ft_meta = serde_json::json!({});
    if ft_depth(&depth) {
        match &worker {
            Some(w) => {
                let ft_plan = serde_json::json!({
                    "depth": depth,
                    "data": spec.data,
                    "data_mode": data_mode,
                    "trainset_digest": data_meta.get("digest").cloned().unwrap_or(serde_json::Value::Null),
                });
                match stage_run(w, backend.as_str(), "ft.lora", &ws_root.join("stage_ft"), &ft_plan) {
                    Ok(v) if v.get("adapter_digest").is_some() && v.get("_placeholder").is_none() => {
                        stages.push(stage("finetune", true, "trained"));
                        ft_meta = v;
                    }
                    Ok(_) => stages.push(stage("finetune", false, "no adapter trained")),
                    Err(Error::Worker(e)) => {
                        note(format!("tune fine-tune stage: {e}"));
                        stages.push(stage("finetune", false, e));
                    }
                    Err(e) => return Err(e),
                }
            }
            None => stages.push(stage("finetune", false, format!("needs devbox or cluster, not {}", backend.as_str()))),
        }
    }

    // ---- stage 5: holdout re-validation (depth == full) ----
    let mut measured = winner.clone();
    let mut split = "search".to_string();
    if depth == "full" {
        let hold: Vec<crate::tasks::Task> = resolver.holdout().to_vec();
        if hold.is_empty() {
            stages.push(stage("holdout", false, "no holdout tasks"));
        } else {
            let t: Vec<crate::search::TrialResult> = (0..seeds).flat_map(|seed| hold.iter().map(move |t| (t, seed))).map(|(t, seed)| resolver.run_trial(&winner.stack, t, seed, &ws_root)).collect();
            stages.push(stage("holdout", true, format!("{} trials", t.len())));
            measured = StackScore { stack: winner.stack.clone(), n: t.len(), trials: t };
            split = "holdout".into();
        }
    }

    let simulated = resolver.adapter_for(&winner.stack) == "reference" && base.harness != "reference";
    let tune_lock = build_tuned_lock(&manifest, base_lock.as_ref(), &winner, &measured, &split, scored.len(), backend.as_str(), &prompt_meta, prompt_gain, &ft_meta, &data_meta, &spec.data, data_mode, simulated, &plan_value);

    if let Some(parent) = spec.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    tune_lock.save(&spec.out)?;
    let previous = if spec.apply { tune_apply(&spec.out, &lock_path, Some(&manifest))? } else { None };

    let best = &winner.stack;
    let notes = notes.lock().map(|n| n.clone()).unwrap_or_default();
    let report = serde_json::json!({
        "depth": depth,
        "data": spec.data,
        "data_mode": data_mode,
        "backend": backend.as_str(),
        "harness": base.harness,
        "eval_split": split,
        "grid": scored.len(),
        "trials": n,
        "stages": stages,
        "best": {"model": best.llm.get(&role), "retries": best.retries, "temperature": best.temperature, "tool_timeout_seconds": best.tool_timeout_seconds, "success_rate": measured.success_rate(), "cost_usd": measured.mean_cost(), "trials": measured.n},
        "baseline": {"success_rate": baseline.success_rate(), "trials": baseline.n, "measured": true},
        "baseline_success_rate": baseline.success_rate(),
        "gain_success_rate": winner.success_rate() - baseline.success_rate(),
        "simulated": simulated,
        "notes": notes,
        "path": spec.out.to_string_lossy(),
        "applied": spec.apply,
        "previous": previous.map(|p| p.to_string_lossy().into_owned()),
    });
    Ok(TuneOutcome { path: spec.out.clone(), lock: tune_lock, report })
}

/// Adopt the tune lock at `tuned` as the lock at `lock_path` (`hlock tune apply`, `tune
/// --apply`), keeping the old one as `<lock>.prev`. Refuses a tune lock for another harness
/// than the manifest's `[target] harness`. Returns the `.prev` path when there was a lock.
pub fn tune_apply(tuned: &std::path::Path, lock_path: &std::path::Path, manifest: Option<&Manifest>) -> Result<Option<PathBuf>> {
    let t = Lockfile::load(tuned)?;
    if t.kind.as_deref() != Some("tuned") {
        return Err(Error::Lockfile(format!("{} isn't a tune lock; write one with hlock tune", tuned.display())));
    }
    if let Some(target) = manifest.and_then(Manifest::target_harness) {
        let ctx = crate::profiles::Ctx::from_process();
        let want = crate::profiles::Registry::load_for(&ctx, manifest).ok().and_then(|r| r.harness(&target).map(|p| p.id.clone())).unwrap_or(target);
        if want != t.harness.name {
            return Err(Error::Tuning(format!("{} is for harness {:?}, but harness.toml targets {want:?}; re-run hlock tune", tuned.display(), t.harness.name)));
        }
    }
    let prev = PathBuf::from(format!("{}.prev", lock_path.display()));
    let kept = if lock_path.is_file() {
        std::fs::copy(lock_path, &prev)?;
        Some(prev)
    } else {
        None
    };
    t.save(lock_path)?;
    Ok(kept)
}

pub struct TuneOutcome {
    pub path: PathBuf,
    pub lock: Lockfile,
    pub report: serde_json::Value,
}

/// The tune lock: the base lock with the winner's model and policy, the measured
/// evidence, and a `[tune]` table recording only the stages that ran.
#[allow(clippy::too_many_arguments)]
fn build_tuned_lock(
    manifest: &Manifest,
    base: Option<&Lockfile>,
    winner: &StackScore,
    measured: &StackScore,
    split: &str,
    grid_size: usize,
    backend: &str,
    prompt_meta: &serde_json::Value,
    prompt_gain: Option<f64>,
    ft_meta: &serde_json::Value,
    data_meta: &serde_json::Value,
    data_arg: &str,
    data_mode: &str,
    simulated: bool,
    plan: &serde_json::Value,
) -> Lockfile {
    let stack = &winner.stack;
    let mut lf = base.cloned().unwrap_or_default();
    lf.base_lock = base.map(|l| sha256_text(&serde_json::to_string(l).unwrap_or_default()));
    lf.manifest_hash = Some(manifest.manifest_hash());
    lf.kind = Some("tuned".to_string());
    lf.stack.entry("name".to_string()).or_insert_with(|| serde_json::json!(manifest.name()));
    lf.stack.insert("selection".to_string(), serde_json::json!("tune"));
    if base.is_none() {
        lf.harness = Harness { name: stack.harness.clone(), pattern: Some(stack.pattern.clone()), ..Default::default() };
        lf.decision = Decision {
            kind: stack.decision.clone(),
            decisions: if stack.decision == "none" { vec![] } else { vec!["retry".into(), "stop".into()] },
            ..Default::default()
        };
        lf.policy = crate::lockfile::Policy { max_steps: 40, human_approval: "never".into(), ..Default::default() };
    }
    // the tuned model and its parameters, keeping the provider
    let role = tuned_role(stack);
    let spec = stack.llm.get(&role).cloned().unwrap_or_default();
    let (provider, model) = match spec.split_once(':') {
        Some((p, m)) if !m.is_empty() => (p.to_string(), m.to_string()),
        _ => ("reference".to_string(), spec.clone()),
    };
    let params = [
        ("retries".to_string(), serde_json::json!(stack.retries)),
        ("temperature".to_string(), serde_json::json!(stack.temperature.unwrap_or(0.2))),
        ("tool_timeout_seconds".to_string(), serde_json::json!(stack.tool_timeout_seconds.unwrap_or(120))),
    ];
    match lf.llm.iter_mut().find(|l| l.role == role) {
        Some(l) => {
            if l.provider != provider || l.model != model {
                l.fingerprint = None;
                l.resolved_from = Some(spec.clone());
            }
            l.provider = provider;
            l.model = model;
            l.params.extend(params);
        }
        None => lf.llm.push(crate::lockfile::LLM { role, provider, model, resolved_from: Some(spec.clone()), params: params.into_iter().collect(), ..Default::default() }),
    }
    lf.policy.retries = stack.retries;
    let r4 = |v: f64| (v * 1e4).round() / 1e4;
    let mut success = BTreeMap::from([("mean".to_string(), r4(measured.success_rate()))]);
    if split == "holdout" {
        success.insert("search".to_string(), r4(winner.success_rate()));
    }
    lf.evidence = Evidence {
        eval_pack: Some(manifest.eval_packs().join(",")),
        eval_pack_version: crate::tasks::packs_version(manifest),
        eval_pack_digest: Some(crate::tasks::packs_digest(manifest)),
        split: split.into(),
        trials: measured.n,
        success_rate: success,
        cost_usd_per_task: BTreeMap::from([("mean".to_string(), round6(measured.mean_cost()))]),
        latency_s_per_task: BTreeMap::from([("p50".to_string(), r4(measured.mean_latency())), ("p95".to_string(), r4(measured.p95_latency()))]),
        simulated,
        ..Default::default()
    };
    lf.provenance.backend = backend.to_string();
    if lf.context.isolation.is_empty() {
        lf.context.isolation = manifest.context.get("isolation").and_then(serde_json::Value::as_str).unwrap_or("locked-only").into();
    }

    let mut tune = serde_json::json!({
        "hpo": {
            "axes": plan["axes"],
            "trials": grid_size,
            "best": {"model": spec, "retries": stack.retries, "temperature": stack.temperature, "tool_timeout_seconds": stack.tool_timeout_seconds},
        },
    });
    if let Some(gain) = prompt_gain {
        tune["prompt"] = serde_json::json!({"applied": [{
            "component": "harness.system-prompt",
            "before": str_field(prompt_meta, "before_digest").unwrap_or_else(|| sha256_text("base system prompt")),
            "after": str_field(prompt_meta, "after_digest").unwrap_or_else(|| sha256_text(prompt_meta["best_prompt"].as_str().unwrap_or_default())),
            "gain": {"success-rate": format!("{gain:+.2}")},
        }]});
    }
    if let Some(d) = str_field(ft_meta, "adapter_digest") {
        tune["finetune"] = serde_json::json!([{
            "target": "decision",
            "base": str_field(ft_meta, "base_model").unwrap_or_default(),
            "data": format!("traces://{data_arg}"),
            "adapter": {"lora": d},
            "samples": num_field(ft_meta, "samples").unwrap_or(0.0) as i64,
            "loss": num_field(ft_meta, "loss").unwrap_or(0.0),
        }]);
    }
    if data_meta.get("rows").is_some() {
        let mut data = serde_json::json!({"source": data_arg, "mode": data_mode, "redact": true, "rows": data_meta["rows"]});
        for k in ["digest", "split"] {
            if let Some(v) = data_meta.get(k) {
                data[k] = v.clone();
            }
        }
        tune["data"] = data;
    }
    // provenance: union run ids + budget (gpu-h / spend) aggregated across stages
    let metas = [prompt_meta, ft_meta, data_meta];
    let run_ids: Vec<String> = metas.iter().filter_map(|m| str_field(m, "run")).collect();
    let stage_gpu: f64 = metas.iter().map(|m| num_field(m, "gpu_h").unwrap_or(0.0)).sum();
    let stage_spend: f64 = metas.iter().map(|m| num_field(m, "cost_usd").unwrap_or(0.0)).sum();
    tune["provenance"] = serde_json::json!({
        "run": if run_ids.is_empty() { format!("worker://{backend}") } else { run_ids.join(",") },
        "budget": {"spend-usd": round6(winner.mean_cost() * winner.n as f64 + stage_spend), "gpu-h": round6(stage_gpu)},
    });
    lf.tune = Some(tune);
    lf
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(serde_json::Value::as_str).map(|s| s.to_string())
}

fn num_field(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(serde_json::Value::as_f64)
}

/// Depth ladder: does `depth` include prompt search?
fn prompt_depth(depth: &str) -> bool {
    matches!(depth, "prompt" | "hpo" | "ft" | "full")
}

/// Depth ladder: does `depth` include fine-tuning?
fn ft_depth(depth: &str) -> bool {
    matches!(depth, "ft" | "full")
}

/// Run a single stage against the Python worker and normalize its metadata.
fn stage_run(
    worker: &crate::worker::PythonWorker,
    backend: &str,
    kind: &str,
    ws: &PathBuf,
    plan: &serde_json::Value,
) -> Result<serde_json::Value> {
    let r = worker.call(kind, backend, None, None, 0, ws, plan)?;
    let mut m = r.model_digests;
    if m.is_null() {
        m = serde_json::json!({});
    }
    if let Some(o) = m.as_object_mut() {
        o.insert("gpu_h".into(), serde_json::json!(r.gpu_h));
        o.insert("cost_usd".into(), serde_json::json!(r.cost_usd));
    }
    Ok(m)
}

fn parse_synthetic(data: &str) -> usize {
    match data.strip_prefix("synthetic") {
        None => 0,
        Some("") => 1,
        Some(rest) => rest.strip_prefix(':').and_then(|n| n.trim().parse().ok()).unwrap_or(1),
    }
}
