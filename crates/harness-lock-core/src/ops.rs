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
use crate::search::Stack;
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
    /// Where `.harnesslock/` store + runs live.
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
        if let Some(t) = target_for(model, spec.to.as_deref(), spec.all) {
            entries.push(serde_json::json!({"role": role, "from": model, "to": t}));
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

    let current_lock = Lockfile::load(&lock_path).ok();
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

    // ---- rollback ----
    if spec.rollback {
        if prev_path.exists() {
            std::fs::copy(&prev_path, &lock_path)?;
            outcome.rolled_back = true;
            outcome.message = "restored the previous harness.lock from harness.lock.prev".into();
            outcome.after = Lockfile::load(&lock_path).ok().map(|l| baseline_sr(&l));
            return Ok(outcome);
        }
        return Err(Error::Tuning("harness.lock.prev not found — nothing to roll back".into()));
    }

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

    // ---- apply: derive a manifest targeting the upgraded planner ----
    let target = upgradeable[0].get("to").and_then(serde_json::Value::as_str).unwrap_or(latest_model()).to_string();
    let spec_str = "reference:".to_string() + &target;
    if lock_path.exists() {
        std::fs::copy(&lock_path, &prev_path)?; // keep harness.lock.prev for --rollback
    }
    let mut derived = manifest.clone();
    let mut cand = crate::manifest::Candidates::default();
    cand.llm = manifest.candidates.llm.clone();
    cand.llm
        .entry("planner".to_string())
        .and_modify(|c| c.any_of = vec![spec_str.clone()])
        .or_insert(crate::manifest::LLMCandidate { any_of: vec![spec_str.clone()] });
    cand.decision = manifest.candidates.decision.clone();
    cand.harness = manifest.candidates.harness.clone();
    cand.policy = manifest.candidates.policy.clone();
    derived.candidates = cand;

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
    outcome.message = format!(
        "upgraded planner to {target}: success-rate {:.2} -> {:.2}. previous lock kept at harness.lock.prev.",
        before.unwrap_or(0.0),
        after
    );
    Ok(outcome)
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
            workdir: PathBuf::from(".harnesslock"),
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

/// Grid axes searched by `tune`. Deeper depths broaden the grid.
/// Returns `(model, retries, temperature, tool_timeout_seconds)`.
fn search_grid(manifest: &Manifest, depth: &str) -> Vec<(String, u32, f64, Option<u64>)> {
    let mut models: Vec<String> = manifest
        .candidates
        .llm
        .get("planner")
        .map(|c| c.any_of.iter().map(|s| model_of(s)).collect())
        .unwrap_or_default();
    if models.is_empty() {
        models.push("qwen-7b".to_string());
    }
    if matches!(depth, "ft" | "full") && !models.contains(&latest_model().to_string()) {
        models.push(latest_model().to_string()); // "fine-tune" may serve the best open model
    }
    let retries: Vec<u32> = match depth {
        "probe" => vec![0],
        "prompt" => vec![0, 1],
        _ => vec![0, 1, 3], // hpo | ft | full
    };
    // temperature is an HPO axis only once we are doing hyperparameter search
    let temps: Vec<f64> = if matches!(depth, "hpo" | "ft" | "full") {
        vec![0.2, 0.7]
    } else {
        vec![0.2]
    };
    // tool timeout-seconds is a full-depth HPO axis
    let timeouts: Vec<Option<u64>> = if depth == "full" {
        vec![Some(30), Some(120)]
    } else {
        vec![Some(120)]
    };
    let mut out = Vec::new();
    for m in &models {
        for &r in &retries {
            for &t in &temps {
                for &tt in &timeouts {
                    out.push((m.clone(), r, t, tt));
                }
            }
        }
    }
    out
}

/// `hlock tune` -> `harness.tune.lock` (§5.9).
///
/// Runs the depth-aware pipeline: **data** -> (depth>=prompt) **prompt search** ->
/// **HPO trials** -> (depth>=ft) **LoRA fine-tune** -> (depth==full) **holdout
/// re-validation**, on the configured backend. On `cluster`/Union the heavy stages
/// are lifted to real Flyte tasks by the Python worker (GPUs for fine-tuning,
/// images, caching), and their provenance/digests are recorded in the tune lock.
pub fn tune(manifest: Manifest, spec: &TuneSpec) -> Result<TuneOutcome> {
    let depth = spec.resolved_depth().to_string();
    let backend = crate::worker::Backend::parse(
        spec.backend.as_deref().unwrap_or(manifest.backend_target().as_str()),
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let lock_path = spec.lock.clone().unwrap_or_else(|| cwd.join(LOCKFILE_PATH));
    let base_lock = Lockfile::load(&lock_path).ok();
    let baseline_sr = base_lock.as_ref().map(baseline_sr).unwrap_or(0.0);

    let mut resolver = crate::resolver::Resolver::with_backend(manifest.clone(), &spec.workdir, backend);
    let search_tasks = resolver.load_packs();
    if search_tasks.is_empty() {
        return Err(Error::Tuning("no tasks loaded — cannot tune".into()));
    }
    let seeds = manifest.seeds();
    let ws_root = spec.workdir.join("runs");
    std::fs::create_dir_all(&ws_root)?;

    let n_synthetic = parse_synthetic(&spec.data);
    let data_mode = if n_synthetic > 0 { "synthetic" } else { "user-traces" };

    let worker = if backend.needs_worker() {
        Some(crate::worker::PythonWorker::new())
    } else {
        None
    };
    let plan_value = serde_json::json!({
        "depth": depth,
        "data": spec.data,
        "data_mode": data_mode,
        "harness": spec.harness,
        "max_trials": spec.max_trials,
        "axes": {
            "retries": if depth == "probe" { serde_json::json!([0]) } else { serde_json::json!([0,1,3]) },
            "temperature": if matches!(depth.as_str(), "hpo" | "ft" | "full") { serde_json::json!([0.2,0.7]) } else { serde_json::json!([0.2]) },
            "tool_timeout_seconds": if depth == "full" { serde_json::json!([30,120]) } else { serde_json::json!([120]) },
        },
    });

    // ---- stage 1: data (user traces or synthetic) ----
    let mut data_meta = serde_json::json!({});
    if let Some(w) = &worker {
        let kind = if n_synthetic > 0 { "data.synthetic" } else { "data.load" };
        let ws = ws_root.join("stage_data");
        match stage_run(w, backend.as_str(), kind, &ws, &plan_value) {
            Ok(v) => data_meta = v,
            Err(Error::Worker(e)) => eprintln!("warn: tune data stage: {e}"),
            Err(e) => return Err(e),
        }
    } else {
        data_meta = serde_json::json!({ "_placeholder": true, "rows": 0 });
    }

    // ---- stage 2: prompt search (depth >= prompt) ----
    let mut prompt_meta = serde_json::json!({});
    let mut tuned_prompt: Vec<String> = Vec::new();
    if prompt_depth(&depth) {
        if let Some(w) = &worker {
            let ws = ws_root.join("stage_prompt");
            match stage_run(w, backend.as_str(), "prompt.search", &ws, &plan_value) {
                Ok(v) => {
                    if let Some(p) = v.get("best_prompt").and_then(serde_json::Value::as_str) {
                        tuned_prompt = vec![p.to_string()];
                    }
                    prompt_meta = v;
                }
                Err(Error::Worker(e)) => eprintln!("warn: tune prompt stage: {e}"),
                Err(e) => return Err(e),
            }
        }
    }

    // ---- stage 3: HPO trials over the grid ----
    let mut scored: Vec<(String, u32, f64, Option<u64>, f64, f64, usize)> = Vec::new();
    for (model, retries, temp, tto) in search_grid(&manifest, &depth) {
        let stack = Stack {
            llm: BTreeMap::from([("planner".to_string(), format!("reference:{model}"))]),
            harness: spec.harness.clone().unwrap_or_else(|| "reference".into()),
            pattern: "single-agent".into(),
            decision: manifest.candidates.decision.first().cloned().unwrap_or_else(|| "none".into()),
            tools: manifest.tools.keys().cloned().collect(),
            skills: manifest.skills.keys().cloned().collect(),
            instructions: tuned_prompt.clone(),
            max_steps: 40,
            max_cost_usd_per_task: None,
            retries,
            temperature: Some(temp),
            tool_timeout_seconds: tto,
        };
        let mut ok = 0u64;
        let mut n = 0u64;
        let mut cost = 0.0;
        let mut budget_hit = false;
        for task in &search_tasks {
            for seed in 0..seeds {
                if let Some(max) = spec.max_trials {
                    if n >= max as u64 {
                        budget_hit = true;
                        break;
                    }
                }
                let tr = resolver.run_trial(&stack, task, seed, &ws_root);
                if tr.success {
                    ok += 1;
                }
                cost += tr.cost_usd;
                n += 1;
            }
            if budget_hit {
                break;
            }
        }
        if n > 0 {
            scored.push((model, retries, temp, tto, ok as f64 / n as f64, cost / n as f64, n as usize));
        }
    }

    // pick best (highest success, then lowest cost)
    let best = scored
        .iter()
        .max_by(|a, b| a.4.partial_cmp(&b.4).unwrap().then(b.5.partial_cmp(&a.5).unwrap()));
    let Some((bm, br, btemp, btto, bs, bc, bn_trials)) = best.cloned() else {
        return Err(Error::Tuning("tune produced no candidate".into()));
    };

    if spec.strict && bs <= baseline_sr {
        return Err(Error::Tuning(format!(
            "--strict: tuned success-rate {bs:.2} does not beat the base {baseline_sr:.2}"
        )));
    }

    let winner_stack = Stack {
        llm: BTreeMap::from([("planner".to_string(), format!("reference:{bm}"))]),
        harness: spec.harness.clone().unwrap_or_else(|| "reference".into()),
        pattern: "single-agent".into(),
        decision: manifest.candidates.decision.first().cloned().unwrap_or_else(|| "none".into()),
        tools: manifest.tools.keys().cloned().collect(),
        skills: manifest.skills.keys().cloned().collect(),
        instructions: tuned_prompt.clone(),
        max_steps: 40,
        max_cost_usd_per_task: None,
        retries: br,
        temperature: Some(btemp),
        tool_timeout_seconds: btto,
    };

    // ---- stage 4: fine-tune (depth >= ft) ----
    let mut ft_meta = serde_json::json!({});
    if ft_depth(&depth) {
        if let Some(w) = &worker {
            let ws = ws_root.join("stage_ft");
            let ft_plan = serde_json::json!({
                "depth": depth,
                "data": spec.data,
                "data_mode": data_mode,
                "trainset_digest": data_meta.get("digest").cloned().unwrap_or(serde_json::Value::Null),
            });
            match stage_run(w, backend.as_str(), "ft.lora", &ws, &ft_plan) {
                Ok(v) => ft_meta = v,
                Err(Error::Worker(e)) => eprintln!("warn: tune fine-tune stage: {e}"),
                Err(e) => return Err(e),
            }
        }
    }

    // ---- stage 5: holdout re-validation (depth == full) ----
    let (es, ec, en, esplit) = if depth == "full" {
        let hold = resolver.holdout();
        if hold.is_empty() {
            (bs, bc, bn_trials as usize, "search".to_string())
        } else {
            let mut ok = 0u64;
            let mut n = 0u64;
            let mut cost = 0.0;
            for task in hold {
                for seed in 0..seeds {
                    let tr = resolver.run_trial(&winner_stack, task, seed, &ws_root);
                    if tr.success {
                        ok += 1;
                    }
                    cost += tr.cost_usd;
                    n += 1;
                }
            }
            (ok as f64 / n.max(1) as f64, cost / (n as f64).max(1.0), n as usize, "holdout".to_string())
        }
    } else {
        (bs, bc, bn_trials as usize, "search".to_string())
    };

    let tune_lock = build_tuned_lock(
        &manifest,
        winner_stack,
        es,
        ec,
        &base_lock,
        &data_mode,
        &spec.data,
        &depth,
        scored.len(),
        en,
        backend.as_str(),
        &prompt_meta,
        &ft_meta,
        &data_meta,
        &esplit,
    );

    let out_path = &spec.out;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    tune_lock.save(out_path)?;

    if spec.apply {
        tune_lock.save(&lock_path)?;
    }

    let report = serde_json::json!({
        "depth": depth,
        "data": spec.data,
        "data_mode": data_mode,
        "backend": backend.as_str(),
        "eval_split": esplit,
        "grid": scored.len(),
        "best": {"model": bm, "retries": br, "temperature": btemp, "tool_timeout_seconds": btto, "success_rate": es, "cost_usd": ec, "trials": en},
        "baseline_success_rate": baseline_sr,
        "gain_success_rate": es - baseline_sr,
        "path": out_path.to_string_lossy(),
        "applied": spec.apply,
    });
    Ok(TuneOutcome {
        path: out_path.clone(),
        lock: tune_lock,
        report,
    })
}

pub struct TuneOutcome {
    pub path: PathBuf,
    pub lock: Lockfile,
    pub report: serde_json::Value,
}

fn build_tuned_lock(
    manifest: &Manifest,
    stack: Stack,
    success: f64,
    cost: f64,
    base: &Option<Lockfile>,
    data_mode: &str,
    data_arg: &str,
    depth: &str,
    grid_size: usize,
    trials: usize,
    backend: &str,
    prompt_meta: &serde_json::Value,
    ft_meta: &serde_json::Value,
    data_meta: &serde_json::Value,
    eval_split: &str,
) -> Lockfile {
    let mut success_map = BTreeMap::new();
    success_map.insert("mean".to_string(), (success * 1e4).round() / 1e4);
    let mut cost_map = BTreeMap::new();
    cost_map.insert("mean".to_string(), (cost * 1e6).round() / 1e6);

    // Prompt + fine-tune digests come from the real worker stages when they ran
    // (prompt.search / ft.lora); only when a depth demands a stage but there was
    // no worker (pure-local) do we fall back to a flagged placeholder.
    let mut proof_prompt = Vec::new();
    if prompt_depth(depth) {
        let placeholder = prompt_meta
            .get("_placeholder")
            .map(|_| true)
            .or_else(|| prompt_meta.get("best_prompt").map(|_| false))
            .unwrap_or(true);
        proof_prompt.push(serde_json::json!({
            "component": "harness.system-prompt",
            "before": str_field(prompt_meta, "before_digest").unwrap_or_else(|| sha256_text("base system prompt")),
            "after": str_field(prompt_meta, "after_digest").unwrap_or_else(|| sha256_text("tuned system prompt (depth >= prompt)")),
            "gain": {"success-rate": format!("{success:+.2}")},
            "_placeholder": placeholder,
        }));
    }

    let mut finetune = Vec::new();
    if ft_depth(depth) {
        let placeholder = ft_meta.get("adapter_digest").is_none()
            || ft_meta.get("_placeholder").is_some();
        finetune.push(serde_json::json!({
            "target": "decision",
            "base": str_field(ft_meta, "base_model").unwrap_or_else(|| "hf://hlock/decider-1.5b".into()),
            "data": format!("traces://{data_arg}"),
            "adapter": {"lora": str_field(ft_meta, "adapter_digest").unwrap_or_else(|| "store://sha256:PLAN".into())},
            "samples": num_field(ft_meta, "samples").unwrap_or(0.0) as i64,
            "loss": num_field(ft_meta, "loss").unwrap_or(0.0),
            "_placeholder": placeholder,
        }));
    }

    let base_lock_hash = base
        .as_ref()
        .map(|l| sha256_text(&serde_json::to_string(l).unwrap_or_default()))
        .unwrap_or_default();

    let mut lf = Lockfile::default();
    lf.manifest_hash = Some(manifest.manifest_hash());
    lf.base_lock = if base_lock_hash.is_empty() { None } else { Some(base_lock_hash) };
    lf.kind = Some("tuned".to_string());
    let mut stack_map = BTreeMap::new();
    stack_map.insert("name".to_string(), serde_json::json!(manifest.name()));
    stack_map.insert("selection".to_string(), serde_json::json!("tune"));
    lf.stack = stack_map;
    lf.decision = Decision {
        kind: stack.decision.clone(),
        decisions: if stack.decision == "none" { vec![] } else { vec!["retry".into(), "stop".into()] },
        ..Default::default()
    };
    lf.harness = Harness { name: stack.harness.clone(), pattern: Some(stack.pattern.clone()), ..Default::default() };
    lf.llm = vec![crate::lockfile::LLM {
        role: "planner".into(),
        provider: "reference".into(),
        model: stack.llm.get("planner").cloned().unwrap_or_default(),
        resolved_from: None,
        params: BTreeMap::from([
            ("retries".to_string(), serde_json::json!(stack.retries)),
            ("temperature".to_string(), serde_json::json!(stack.temperature.unwrap_or(0.2))),
            ("tool_timeout_seconds".to_string(), serde_json::json!(stack.tool_timeout_seconds.unwrap_or(120))),
        ]),
        fingerprint: None,
        ..Default::default()
    }];
    lf.policy = crate::lockfile::Policy {
        retries: stack.retries,
        max_steps: 40,
        human_approval: "never".into(),
        ..Default::default()
    };
    lf.evidence = Evidence {
        task_pack: Some(manifest.task_packs().join(",")),
        split: eval_split.into(),
        trials,
        success_rate: success_map,
        cost_usd_per_task: cost_map,
        ..Default::default()
    };

    // data provenance (real digest/rows/split when a worker stage ran)
    let mut data_block = serde_json::json!({"source": data_arg, "mode": data_mode, "redact": true});
    if let Some(d) = data_meta.get("digest").and_then(serde_json::Value::as_str) {
        data_block["digest"] = serde_json::json!(d);
    }
    if let Some(v) = data_meta.get("rows") {
        data_block["rows"] = v.clone();
    }
    if let Some(v) = data_meta.get("split") {
        data_block["split"] = v.clone();
    }
    if data_meta.get("_placeholder").is_some() {
        data_block["_placeholder"] = serde_json::json!(true);
    }

    // provenance: union run ids + budget (gpu-h / spend) aggregated across stages
    let mut run_ids: Vec<String> = Vec::new();
    for m in [prompt_meta, ft_meta, data_meta] {
        if let Some(r) = m.get("run").and_then(serde_json::Value::as_str) {
            run_ids.push(r.to_string());
        }
    }
    let run = if run_ids.is_empty() {
        format!("worker://{backend}")
    } else {
        run_ids.join(",")
    };
    let stage_gpu: f64 = [prompt_meta, ft_meta, data_meta]
        .iter()
        .map(|m| num_field(m, "gpu_h").unwrap_or(0.0))
        .sum();
    let stage_spend: f64 = [prompt_meta, ft_meta, data_meta]
        .iter()
        .map(|m| num_field(m, "cost_usd").unwrap_or(0.0))
        .sum();
    let spend = cost * trials as f64 + stage_spend;

    lf.tune = Some(serde_json::json!({
        "hpo": {
            "axes": {
                "retries": [0,1,3],
                "temperature": [0.2,0.7],
                "tool_timeout_seconds": [30,120],
            },
            "trials": grid_size,
            "best": {"model": stack.llm.get("planner"), "retries": stack.retries, "temperature": stack.temperature, "tool_timeout_seconds": stack.tool_timeout_seconds},
        },
        "prompt": {"applied": proof_prompt},
        "finetune": finetune,
        "data": data_block,
        "provenance": {"run": run, "budget": {"spend-usd": round6(spend), "gpu-h": round6(stage_gpu)}},
    }));
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
