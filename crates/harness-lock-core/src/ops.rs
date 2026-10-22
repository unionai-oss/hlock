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
    /// A backend name: offline | local | flyte | a custom one (overrides the manifest's
    /// `[backend] target`).
    pub backend: Option<String>,
    pub workdir: PathBuf,
    /// The base lock (default `./harness.lock`); `apply` writes back to it.
    pub lock: Option<PathBuf>,
    /// Overrides of `[tune]`: the optimizer, what it may edit, its budget.
    pub optimizer: Option<String>,
    pub edit: Option<Vec<String>>,
    pub budget_rollouts: Option<u64>,
    pub budget_usd: Option<f64>,
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
            optimizer: None,
            edit: None,
            budget_rollouts: None,
            budget_usd: None,
        }
    }
}

/// `[tune]` in `harness.toml`, with `tune`'s flags applied.
#[derive(Debug, Clone, PartialEq)]
pub struct TuneConfig {
    /// The optimizer that searches: `hlock` (built in), `harness2`, or another
    /// (`specs/optimizers.md`). Where it runs is the backend's business.
    pub optimizer: String,
    /// The component kinds it may edit; empty means no optimize stage.
    pub edit: Vec<String>,
    pub proposer: Option<String>,
    pub budget: crate::optimize::Budget,
    /// Trials per task in each evaluation, and in the acceptance runs.
    pub trials: u32,
    /// The sign test's significance level for accepting edits.
    pub alpha: f64,
    /// How much more per task an accepted candidate may cost (0.25 = 25%).
    pub max_cost_increase: f64,
}

impl TuneConfig {
    pub fn new(manifest: &Manifest, spec: &TuneSpec, depth: &str) -> Result<TuneConfig> {
        let t = &manifest.tune;
        let str_list = |v: &serde_json::Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        let declared = t.get("edit").map(str_list);
        let edit = spec.edit.clone().or(declared).unwrap_or_else(|| if prompt_depth(depth) { vec!["instructions".into()] } else { vec![] });
        if let Some(k) = edit.iter().find(|k| !crate::optimize::KINDS.contains(&k.as_str())) {
            return Err(Error::Tuning(format!("[tune] edit: {k:?} can't be tuned yet (use {})", crate::optimize::KINDS.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>().join(" or "))));
        }
        let budget = t.get("budget");
        let num = |k: &str| t.get(k).and_then(serde_json::Value::as_f64);
        Ok(TuneConfig {
            optimizer: spec.optimizer.clone().or_else(|| t.get("optimizer").and_then(|v| v.as_str()).map(str::to_string)).unwrap_or_else(|| "hlock".into()),
            edit,
            proposer: t.get("proposer").and_then(|v| v.as_str()).map(str::to_string),
            budget: crate::optimize::Budget {
                rollouts: spec.budget_rollouts.or_else(|| budget.and_then(|b| b.get("rollouts")).and_then(serde_json::Value::as_u64)).or(Some(100)),
                usd: spec.budget_usd.or_else(|| budget.and_then(|b| b.get("usd")).and_then(serde_json::Value::as_f64)),
            },
            trials: t.get("trials").and_then(serde_json::Value::as_u64).map(|n| n.clamp(1, 20) as u32).unwrap_or(2),
            alpha: num("alpha").unwrap_or(0.05),
            max_cost_increase: num("max-cost-increase").unwrap_or(0.25),
        })
    }
}

/// What the optimize stage did: its candidate, whether hlock accepted it, and the record
/// for the tune lock.
struct Optimized {
    /// The candidate bundle, after the leak check.
    candidate: PathBuf,
    seed: PathBuf,
    spec: crate::optimize::BundleSpec,
    changes: Vec<crate::optimize::Change>,
    ledger: Vec<serde_json::Value>,
    accepted: bool,
    /// `[tune.optimizer]` and `[tune.acceptance]`.
    optimizer: serde_json::Value,
    acceptance: serde_json::Value,
}

/// Per-task success over `trials` seeds, and mean cost per trial.
fn per_task(dir: &std::path::Path, resolver: &Resolver, stack: &Stack, tasks: &[crate::tasks::Task], trials: u32) -> Result<(BTreeMap<String, f64>, f64, usize)> {
    let mut by = BTreeMap::new();
    let (mut cost, mut n) = (0.0, 0usize);
    for t in tasks {
        let mut ok = 0.0;
        for s in 0..trials {
            let (r, _) = crate::optimize::cached_trial(dir, resolver, stack, t, u64::from(s))?;
            ok += if r.success { 1.0 } else { 0.0 };
            cost += r.cost_usd;
            n += 1;
        }
        by.insert(t.id.clone(), ok / f64::from(trials));
    }
    Ok((by, cost / n.max(1) as f64, n))
}

/// The optimize stage (`specs/tune-optimizers.md` §3): write the bundle and the session,
/// measure the noise on dev, hand the bundle to the optimizer, drop edits that copy the
/// pack, then measure the candidate against the seed on the held-out tasks.
#[allow(clippy::too_many_arguments)]
fn optimize_stage(
    w: &crate::worker::Worker,
    optimizer: &crate::optimize::Optimizer,
    cfg: &TuneConfig,
    manifest: &Manifest,
    spec: &TuneSpec,
    resolver: &mut Resolver,
    base: &Stack,
    base_lock: Option<&Lockfile>,
    lock_path: &std::path::Path,
    search: &[crate::tasks::Task],
    note: &dyn Fn(String),
) -> Result<Optimized> {
    use crate::optimize as opt;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let id: String = crate::hash::sha256_hex(format!("{nanos}{}", std::process::id()).as_bytes()).chars().take(8).collect();
    let workdir = crate::fsutil::absolute(&spec.workdir);
    let dir = opt::Session::dir(&workdir, &id);
    std::fs::create_dir_all(&dir)?;
    let lock_dir = crate::fsutil::absolute(lock_path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")));
    let seed = dir.join("seed");
    let reg = crate::profiles::Registry::load_for(&crate::profiles::Ctx::from_process(), Some(manifest)).ok();
    let profile = reg.as_ref().and_then(|r| r.harness(&base.harness));
    let bundle = opt::write_seed(base_lock, profile, &lock_dir, &base.harness, &tuned_instructions_name(base_lock, manifest, &base.harness), &cfg.edit, &seed)?;

    // train and dev from the search split; the holdout is the test split, which only hlock runs
    let mut ids: Vec<&crate::tasks::Task> = search.iter().collect();
    ids.sort_by(|a, b| a.id.cmp(&b.id));
    let (mut train, mut dev): (Vec<crate::tasks::Task>, Vec<crate::tasks::Task>) = (Vec::new(), Vec::new());
    for (i, t) in ids.into_iter().enumerate() {
        if search.len() >= 3 && i % 3 == 2 { dev.push(t.clone()) } else { train.push(t.clone()) }
    }
    if dev.is_empty() {
        note(format!("the pack has {} search tasks, too few to set some aside as dev: the optimizer's dev split is its train split", search.len()));
        dev = train.clone();
    }
    let mut test: Vec<crate::tasks::Task> = resolver.holdout().to_vec();
    let test_split = if test.is_empty() {
        note("the pack has no holdout tasks, so the edits are checked on the dev split, which the optimizer saw; add a holdout split for an unbiased check".into());
        test = dev.clone();
        "dev"
    } else {
        if test.len() < 20 {
            note(format!("the pack holds out {} tasks: a gain measured on fewer than 20 is unlikely to be real", test.len()));
        }
        "holdout"
    };
    let manifest_path = manifest.source_path.clone().map(|p| crate::fsutil::absolute(&p)).unwrap_or_else(|| manifest.root().join(crate::MANIFEST_PATH));
    let session = opt::Session {
        id: id.clone(),
        manifest: manifest_path,
        lock: Some(crate::fsutil::absolute(lock_path)).filter(|p| p.is_file()),
        backend: spec.backend.clone(),
        harness: base.harness.clone(),
        workdir: workdir.clone(),
        train: train.iter().map(|t| t.id.clone()).collect(),
        dev: dev.iter().map(|t| t.id.clone()).collect(),
        trials: cfg.trials,
        budget: cfg.budget.clone(),
        used: opt::Usage::default(),
    };
    session.save(&dir)?;

    // the seed as a stack: what the candidate is measured against, with the same system prompt
    let seed_stack = opt::overlay(base, &seed);
    resolver.trial_locks.insert(seed_stack.hash(), opt::trial_lock(base_lock, &seed, &bundle)?);

    // ---- the noise band: the seed's spread over seeds on dev ----
    let mut rates = Vec::new();
    for s in 0..cfg.trials {
        let mut ok = 0usize;
        for t in &dev {
            ok += usize::from(opt::cached_trial(&dir, resolver, &seed_stack, t, u64::from(s))?.0.success);
        }
        rates.push(ok as f64 / dev.len().max(1) as f64);
    }
    let mean = rates.iter().sum::<f64>() / rates.len().max(1) as f64;
    let noise = (rates.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rates.len().max(1) as f64).sqrt();

    // ---- the search ----
    let hlock_bin = std::env::var("HLOCK_BIN").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "hlock".into());
    let payload = serde_json::json!({
        "session": dir,
        "seed": seed,
        "tasks": {"train": session.train, "dev": session.dev},
        // what an optimizer that runs its own trials needs: the prompt, the files the task
        // starts from, and what it should leave (never how it's graded)
        "task_meta": train.iter().chain(dev.iter()).map(|t| {
            let fixtures = t.fixtures.as_ref().zip(t.pack_dir.as_ref()).map(|(f, p)| crate::fsutil::absolute(&p.join(f)));
            (t.id.clone(), serde_json::json!({"prompt_text": t.prompt_text, "fixtures": fixtures, "expected": t.expected_artifacts()}))
        }).collect::<serde_json::Map<_, _>>(),
        "evaluate": {"command": [hlock_bin, "tune", "eval", "--session", dir.to_string_lossy()]},
        "budget": cfg.budget,
        "trials": cfg.trials,
        "models": {"task": base.llm.get(&tuned_role(base)), "proposer": cfg.proposer},
        "noise": {"dev_success_sd": noise},
    });
    let r = w.optimize(optimizer, &dir.join("opt"), &serde_json::json!({"edit": cfg.edit}), payload).map_err(|e| match e {
        Error::Worker(m) => Error::Tuning(format!("the {} optimizer failed: {m}", cfg.optimizer)),
        e => e,
    })?;
    let cand_src = r.candidate.clone().map(PathBuf::from).ok_or_else(|| Error::Tuning(format!("the {} optimizer returned no candidate", cfg.optimizer)))?;
    let ledger: Vec<serde_json::Value> = r
        .ledger
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default();
    // hlock's copy of the candidate: the optimizer's directory isn't trusted to stay put
    let candidate = dir.join("final");
    let _ = std::fs::remove_dir_all(&candidate);
    for sub in ["instructions", "skills"] {
        if cand_src.join(sub).is_dir() {
            crate::fsutil::copy_tree(&cand_src.join(sub), &candidate.join(sub))?;
        }
    }
    std::fs::create_dir_all(&candidate)?;
    let mut changes = opt::validate(&seed, &bundle, &candidate).map_err(|e| Error::Tuning(format!("the {} optimizer's candidate: {e}", cfg.optimizer)))?;

    // ---- edits that copy the pack are dropped ----
    let mut all = search.to_vec();
    all.extend(resolver.holdout().iter().cloned());
    let leaked = opt::leaks(&changes, &opt::pack_literals(&all));
    for (cid, hits) in &leaked {
        note(format!("dropped the edit to {cid}: it copies text from the eval pack ({})", hits.iter().map(|h| format!("{h:?}")).collect::<Vec<_>>().join(", ")));
        opt::revert(&seed, &candidate, cid)?;
    }
    if !leaked.is_empty() {
        changes = opt::validate(&seed, &bundle, &candidate)?;
    }

    let used = opt::Session::load(&dir).map(|s| s.used).unwrap_or_default();
    let mut optimizer = serde_json::json!({
        "name": cfg.optimizer,
        "backend": w.backend.name,
        "evaluator": if used.rollouts > 0 { "hlock" } else { "optimizer" },
        "edit": cfg.edit,
        "budget": {"rollouts": cfg.budget.rollouts, "usd": cfg.budget.usd},
        "usage": {"rollouts": used.rollouts, "usd": round6(used.usd)},
        "session": id,
        "splits": crate::hash::content_hash(&serde_json::json!({"train": session.train, "dev": session.dev, "test": test.iter().map(|t| &t.id).collect::<Vec<_>>()})),
    });
    if let Some(o) = r.optimizer.as_object() {
        for (k, v) in o.iter().filter(|(k, _)| matches!(k.as_str(), "version" | "proposer")) {
            optimizer[k] = v.clone();
        }
    }
    if optimizer.get("proposer").is_none_or(serde_json::Value::is_null) {
        optimizer["proposer"] = serde_json::json!(cfg.proposer);
    }

    if changes.is_empty() {
        let acceptance = serde_json::json!({"accepted": false, "reason": "the optimizer made no edits that hlock kept", "noise-band": round6(noise)});
        return Ok(Optimized { candidate, seed, spec: bundle, changes, ledger, accepted: false, optimizer, acceptance });
    }

    // ---- acceptance: the seed and the candidate, paired, on the test split ----
    let cand_stack = opt::overlay(base, &candidate);
    resolver.trial_locks.insert(cand_stack.hash(), opt::trial_lock(base_lock, &candidate, &bundle)?);
    let (b, b_cost, n) = per_task(&dir, resolver, &seed_stack, &test, cfg.trials)?;
    let (c, c_cost, _) = per_task(&dir, resolver, &cand_stack, &test, cfg.trials)?;
    let wins = test.iter().filter(|t| c[&t.id] > b[&t.id]).count();
    let losses = test.iter().filter(|t| c[&t.id] < b[&t.id]).count();
    let p = opt::sign_test(wins, losses);
    let (bsr, csr) = (b.values().sum::<f64>() / b.len().max(1) as f64, c.values().sum::<f64>() / c.len().max(1) as f64);
    let reason = if csr - bsr <= noise {
        Some(format!("success {csr:.2} vs {bsr:.2} isn't above the noise band ({noise:.2})"))
    } else if p > cfg.alpha {
        Some(format!("the gain isn't significant: p = {p:.3} > {} ({wins} tasks better, {losses} worse)", cfg.alpha))
    } else if b_cost > 0.0 && c_cost > b_cost * (1.0 + cfg.max_cost_increase) {
        Some(format!("cost per trial rose from ${b_cost:.4} to ${c_cost:.4}, more than {:.0}%", cfg.max_cost_increase * 100.0))
    } else {
        None
    };
    let mut acceptance = serde_json::json!({
        "accepted": reason.is_none(),
        "noise-band": round6(noise),
        "test": {"split": test_split, "tasks": test.len(), "trials": cfg.trials, "base": round6(bsr), "candidate": round6(csr), "better": wins, "worse": losses, "p": round6(p)},
        "cost-usd-per-trial": {"base": round6(b_cost), "candidate": round6(c_cost)},
        "runs": n * 2,
    });
    if let Some(why) = &reason {
        acceptance["reason"] = serde_json::json!(why);
        note(format!("hlock didn't accept the {} optimizer's edits: {why}", cfg.optimizer));
    }
    Ok(Optimized { candidate, seed, spec: bundle, changes, ledger, accepted: reason.is_none(), optimizer, acceptance })
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
pub(crate) fn base_stack(base: Option<&Lockfile>, manifest: &Manifest, harness: Option<&str>) -> Stack {
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
/// measured on the same tasks first, as the baseline. A stage runs when the backend
/// says it runs that work item (never on `offline`), and a stage that didn't run isn't
/// recorded.
pub fn tune(manifest: Manifest, spec: &TuneSpec) -> Result<TuneOutcome> {
    manifest.check_search()?;
    let depth = spec.resolved_depth().to_string();
    let backend = crate::worker::Backend::resolve(&manifest, spec.backend.as_deref())?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let lock_path = spec.lock.clone().unwrap_or_else(|| cwd.join(LOCKFILE_PATH));
    let base_lock = load_if_present(&lock_path)?;
    let base = base_stack(base_lock.as_ref(), &manifest, spec.harness.as_deref());

    // trials run the base's harness: its code harness for real, else the reference adapter
    let mut m = manifest.clone();
    m.candidates.harness = vec![crate::manifest::HarnessCandidate { name: base.harness.clone(), ..Default::default() }];
    let mut resolver = Resolver::with_backend(m, &spec.workdir, backend.clone());
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
    if let Some(n) = backend.renamed_note() {
        note(n);
    }
    let worker = resolver.worker.clone();
    if worker.is_none() && (prompt_depth(&depth) && depth != "hpo" || ft_depth(&depth)) {
        note(format!("only the HPO grid runs on the {} backend; the data, prompt and fine-tune stages need local, flyte or another backend", backend.as_str()));
    }
    // a backend that can't describe itself, or can't run the base's trials, stops here
    if let Some(w) = &worker {
        if !resolver.adapter_for(&base).starts_with("code harness") {
            w.preflight(std::slice::from_ref(&base))?;
        }
    }
    // the backend that runs `kind`, else why the stage can't run
    let runs = |kind: &str| -> Result<std::result::Result<&crate::worker::Worker, String>> {
        match &worker {
            None => Ok(Err(format!("not on {}: use local, flyte or another backend", backend.as_str()))),
            Some(w) => Ok(match w.unsupported(kind)? {
                Some(why) => Err(why),
                None => Ok(&**w),
            }),
        }
    };

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
    let data_kind = if n_synthetic > 0 { "data.synthetic" } else { "data.load" };
    match runs(data_kind)? {
        Ok(w) => {
            match stage_run(w, data_kind, &ws_root.join("stage_data"), &plan_value) {
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
        Err(why) => stages.push(stage("data", false, why)),
    }

    // ---- stage 2: optimize the harness's text (depth >= prompt, or [tune] edit) ----
    let tcfg = TuneConfig::new(&manifest, spec, &depth)?;
    let mut prompt_meta = serde_json::json!({});
    let mut tuned_prompt: Vec<String> = Vec::new();
    let mut optimized: Option<Optimized> = None;
    let mut legacy_prompt = false;
    if !tcfg.edit.is_empty() {
        // what searches (the optimizer) and where (the backend, when it hosts searches, else
        // hlock's local worker; the trials run on the backend either way)
        let optimizer = crate::optimize::Optimizer::resolve(&manifest, &tcfg.optimizer)?;
        let host: Option<crate::worker::Worker>;
        let found = match runs("harness.optimize")? {
            Ok(w) => Ok(w),
            Err(why) if worker.is_none() => Err(why),
            Err(_) => {
                let local = crate::worker::Backend::resolve(&manifest, Some("local"))?;
                host = crate::worker::Worker::for_backend(&local, spec.workdir.join("backend").join("local"));
                match &host {
                    None => Err("hlock's local worker isn't available to run the search".to_string()),
                    Some(w) => match w.unsupported("harness.optimize") {
                        Ok(None) => Ok(w),
                        Ok(Some(why)) => Err(why),
                        Err(Error::Worker(e)) => Err(format!("the {} backend doesn't host the search, and hlock's local worker can't: {e}", backend.as_str())),
                        Err(e) => return Err(e),
                    },
                }
            }
        };
        match found {
            Ok(w) => match optimize_stage(w, &optimizer, &tcfg, &manifest, spec, &mut resolver, &base, base_lock.as_ref(), &lock_path, &search_tasks, &note) {
                Ok(o) => {
                    let detail = if o.accepted { format!("{} edit{} accepted", o.changes.len(), if o.changes.len() == 1 { "" } else { "s" }) } else { "no edits accepted".to_string() };
                    stages.push(stage("optimize", true, detail));
                    optimized = Some(o);
                }
                Err(Error::Tuning(e) | Error::Worker(e)) => {
                    note(format!("tune optimize stage: {e}"));
                    stages.push(stage("optimize", false, e));
                }
                Err(e) => return Err(e),
            },
            // a backend from before harness.optimize: its prompt search, when it has one
            Err(why) => match runs("prompt.search")? {
                Ok(_) if tcfg.optimizer == "hlock" && tcfg.edit == ["instructions"] => {
                    note(format!("the {} backend doesn't run harness.optimize; using its prompt.search, which is deprecated", backend.as_str()));
                    legacy_prompt = true;
                }
                _ => stages.push(stage("optimize", false, why)),
            },
        }
    }
    if legacy_prompt {
        match runs("prompt.search")? {
            Ok(w) => match stage_run(w, "prompt.search", &ws_root.join("stage_prompt"), &plan_value) {
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
            Err(why) => stages.push(stage("prompt", false, why)),
        }
    }
    // after an optimize stage, every trial runs the lock's text (the seed) or, when hlock
    // accepted the edits, the candidate's
    let text = optimized.as_ref().map(|o| (if o.accepted { o.candidate.clone() } else { o.seed.clone() }, o.seed.clone(), o.spec.clone()));
    let with_prompt = |s: &Stack| {
        let mut s = s.clone();
        s.instructions.extend(tuned_prompt.iter().cloned());
        match &text {
            Some((dir, _, _)) => crate::optimize::overlay(&s, dir),
            None => s,
        }
    };
    let base_run = match &text {
        Some((_, seed, _)) => crate::optimize::overlay(&base, seed),
        None => base.clone(),
    };
    let configs: Vec<Stack> = grid.iter().map(&with_prompt).collect();
    if let Some((dir, seed, bspec)) = &text {
        resolver.trial_locks.insert(base_run.hash(), crate::optimize::trial_lock(base_lock.as_ref(), seed, bspec)?);
        for c in &configs {
            resolver.trial_locks.insert(c.hash(), crate::optimize::trial_lock(base_lock.as_ref(), dir, bspec)?);
        }
    }

    // ---- baseline: the base configuration, on the same tasks and seeds ----
    let pairs: Vec<(&crate::tasks::Task, u64)> = (0..seeds).flat_map(|seed| search_tasks.iter().map(move |t| (t, seed))).collect();
    let run_all = |stack: &Stack| -> Vec<crate::search::TrialResult> { pairs.iter().map(|(t, seed)| resolver.run_trial(stack, t, *seed, &ws_root)).collect() };
    let baseline_trials = run_all(&base_run);
    let baseline = StackScore { stack: base_run.clone(), n: baseline_trials.len(), trials: baseline_trials };

    // ---- stage 3: HPO trials over the grid, the budget spent round-robin ----
    let mut results: Vec<Vec<crate::search::TrialResult>> = vec![Vec::new(); configs.len()];
    let max = spec.max_trials.unwrap_or(usize::MAX);
    let mut n = 0usize;
    'budget: for (i, (task, seed)) in pairs.iter().enumerate() {
        for (cfg, out) in configs.iter().zip(results.iter_mut()) {
            // the base configuration's trials are already the baseline's
            if cfg.hash() == base_run.hash() {
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
        match runs("ft.lora")? {
            Ok(w) => {
                let ft_plan = serde_json::json!({
                    "depth": depth,
                    "data": spec.data,
                    "data_mode": data_mode,
                    "trainset_digest": data_meta.get("digest").cloned().unwrap_or(serde_json::Value::Null),
                });
                match stage_run(w, "ft.lora", &ws_root.join("stage_ft"), &ft_plan) {
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
            Err(why) => stages.push(stage("finetune", false, why)),
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
    let out_dir = spec.out.parent().filter(|p| !p.as_os_str().is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let tuned_file = match tuned_prompt.first() {
        Some(p) => Some(write_tuned_instructions(base_lock.as_ref(), &manifest, &base.harness, &out_dir, lock_path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")), p)?),
        None => None,
    };
    let mut tune_lock = build_tuned_lock(&manifest, base_lock.as_ref(), &winner, &measured, &split, scored.len(), backend.as_str(), &prompt_meta, prompt_gain, tuned_file.as_ref(), &ft_meta, &data_meta, &spec.data, data_mode, simulated, &plan_value);
    let mut optimize_report = serde_json::Value::Null;
    if let Some(o) = &optimized {
        let mut t = tune_lock.tune.take().unwrap_or_else(|| serde_json::json!({}));
        t["optimizer"] = o.optimizer.clone();
        t["acceptance"] = o.acceptance.clone();
        let edits = if o.accepted { crate::optimize::apply_to_lock(&mut tune_lock, crate::profiles::Registry::load_for(&crate::profiles::Ctx::from_process(), Some(&manifest)).ok().as_ref().and_then(|r| r.harness(&base.harness)), &out_dir, &o.candidate, &o.changes, &o.ledger)? } else { Vec::new() };
        if !edits.is_empty() {
            t["edit"] = serde_json::json!(edits);
        }
        tune_lock.tune = Some(t);
        optimize_report = serde_json::json!({"optimizer": o.optimizer, "acceptance": o.acceptance, "edits": edits, "proposed": o.changes.len()});
    }

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
        "optimize": optimize_report,
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
    copy_tuned_files(&t, tuned, lock_path)?;
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
    tuned_file: Option<&TunedFile>,
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
        let mut applied = serde_json::json!({
            "component": "harness.system-prompt",
            "before": str_field(prompt_meta, "before_digest").unwrap_or_else(|| sha256_text("base system prompt")),
            "after": str_field(prompt_meta, "after_digest").unwrap_or_else(|| sha256_text(prompt_meta["best_prompt"].as_str().unwrap_or_default())),
            "gain": {"success-rate": format!("{gain:+.2}")},
        });
        // the instruction file the prompt went into: its text is next to the lock, and the
        // lock's [[instructions]] entry for it is what sync and launch place
        if let Some(f) = tuned_file {
            applied["file"] = serde_json::json!(f.name);
            applied["before"] = serde_json::json!(f.before);
            applied["after"] = serde_json::json!(f.after);
            applied["path"] = serde_json::json!(f.rel);
            let entry = crate::lockfile::Instructions {
                name: f.name.clone(),
                digest: Some(f.after.clone()),
                scope: "project".into(),
                vendored: Some(f.rel.clone()),
                source: Some("tune".into()),
                scan: Some(f.scan.clone()),
                ..Default::default()
            };
            match lf.instructions.iter_mut().find(|i| i.name == f.name) {
                Some(i) => *i = entry,
                None => lf.instructions.push(entry),
            }
        }
        tune["prompt"] = serde_json::json!({"applied": [applied]});
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
    // provenance: Flyte run ids + budget (gpu-h / spend) aggregated across stages
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

/// A tuned system prompt, written into the instruction file the harness reads.
pub struct TunedFile {
    /// The instruction file, as the lock names it (`CLAUDE.md`).
    pub name: String,
    /// Digests of the file before and after the prompt was added.
    pub before: String,
    pub after: String,
    /// Where its new text is, relative to the tune lock (`.hlock/tuned/<digest>/CLAUDE.md`).
    pub rel: String,
    /// The hidden-Unicode scan of the new text.
    pub scan: String,
}

/// The project instruction file a tuned prompt goes into: the base lock's own, else the
/// first one the harness reads from the project, else `AGENTS.md`.
fn tuned_instructions_name(base: Option<&Lockfile>, manifest: &Manifest, harness: &str) -> String {
    let plain = |n: &str| crate::fsutil::safe_rel(n) && !n.contains('{') && !n.starts_with('~');
    if let Some(i) = base.and_then(|l| l.instructions.iter().find(|i| i.scope == "project" && plain(&i.name))) {
        return i.name.clone();
    }
    let ctx = crate::profiles::Ctx::from_process();
    let reg = crate::profiles::Registry::load_for(&ctx, Some(manifest)).ok();
    reg.and_then(|r| r.harness(harness).and_then(|p| p.instructions.iter().find(|t| plain(t)).cloned())).unwrap_or_else(|| "AGENTS.md".into())
}

/// Write the instruction file with the tuned `prompt` added at its end, under
/// `<out_dir>/.hlock/tuned/<digest>/`, so the tune lock points at text a reviewer can read
/// and `sync` can place. The base text is the base lock's vendored copy, else the
/// project's file next to the base lock. The prompt is redacted like any text drafted
/// from traces.
fn write_tuned_instructions(base: Option<&Lockfile>, manifest: &Manifest, harness: &str, out_dir: &std::path::Path, base_dir: &std::path::Path, prompt: &str) -> Result<TunedFile> {
    let name = tuned_instructions_name(base, manifest, harness);
    let entry = base.and_then(|l| l.instructions.iter().find(|i| i.name == name));
    let from = entry.and_then(|i| i.vendored.as_deref()).filter(|v| crate::fsutil::safe_rel(v)).map(|v| base_dir.join(v)).filter(|p| p.is_file()).unwrap_or_else(|| base_dir.join(&name));
    let before = crate::fsutil::read_regular_text(&from, 1 << 20).unwrap_or_default();
    let prompt = crate::capture::redact_text(prompt.trim());
    let after = if before.trim().is_empty() { format!("{prompt}\n") } else { format!("{}\n\n{prompt}\n", before.trim_end()) };
    let digest = crate::hash::sha256_bytes(after.as_bytes());
    let file = std::path::Path::new(&name).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "AGENTS.md".into());
    let short: String = digest.trim_start_matches("sha256:").chars().take(8).collect();
    let rel = format!(".hlock/tuned/{short}/{file}");
    let dest = out_dir.join(&rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::fsutil::write_atomic(&dest, &after)?;
    let scan = crate::scan::record(&crate::scan::scan_text(&after, &rel), false);
    Ok(TunedFile { name, before: crate::hash::sha256_bytes(before.as_bytes()), after: digest, rel, scan })
}

/// A tune lock's tuned files, copied next to `lock_path` when it lives in another directory,
/// so the applied lock's vendored paths resolve.
fn copy_tuned_files(t: &Lockfile, tuned: &std::path::Path, lock_path: &std::path::Path) -> Result<()> {
    let dir = |p: &std::path::Path| crate::fsutil::absolute(p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")));
    let (from, to) = (dir(tuned), dir(lock_path));
    if crate::fsutil::normalize(&from) == crate::fsutil::normalize(&to) {
        return Ok(());
    }
    let files = t.subagent.iter().chain(t.script.iter()).chain(t.middleware.iter()).filter_map(|c| c.vendored.as_deref());
    let tuned = t.instructions.iter().filter_map(|i| i.vendored.as_deref()).chain(t.skill.iter().filter_map(|s| s.vendored.as_deref())).chain(files);
    for rel in tuned.filter(|v| v.starts_with(".hlock/tuned/") && crate::fsutil::safe_rel(v)) {
        let (src, dest) = (from.join(rel), to.join(rel));
        if src.is_dir() {
            crate::fsutil::copy_tree(&src, &dest)?;
        } else if src.is_file() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dest)?;
        }
    }
    Ok(())
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

/// Run a single stage on the backend and normalize its metadata.
fn stage_run(worker: &crate::worker::Worker, kind: &str, ws: &PathBuf, plan: &serde_json::Value) -> Result<serde_json::Value> {
    let r = worker.call(kind, None, None, 0, ws, plan)?;
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
