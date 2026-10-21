//! The resolver: turns a manifest into an `harness.lock`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::adapters::{HarnessAdapter, ReferenceAdapter};
use crate::error::{Error, Result};
use crate::lockfile::{Context, Decision, Evidence, Harness, Instructions, LLM, Lockfile, Policy, Provenance, Skill, Tool, Verify};
use crate::manifest::{HarnessCandidate, Manifest};
use crate::profiles::{probe_harness, Ctx, Registry};
use crate::search::{select_winner, Stack, StackScore, TrialResult};
use crate::store::Store;
use crate::tasks::EvalPack;
use crate::worker::{Backend, PythonAdapter};

/// Aggregated metrics container produced by the resolver.

pub struct Resolver {
    pub manifest: Manifest,
    pub workdir: PathBuf,
    pub store: Store,
    pub backend: Backend,
    pack_by_task: BTreeMap<String, crate::tasks::EvalPack>,
    adapter: Box<dyn HarnessAdapter>,
    holdout: Vec<crate::tasks::Task>,
    /// Target harness from the CLI (`--harness`), overriding `[target] harness`.
    pub harness_override: Option<String>,
    /// Code harnesses (`[harnesses.<id>]`) that trials run through their entrypoint.
    code_harnesses: BTreeMap<String, crate::profiles::HarnessProfile>,
    /// Profiles of the candidate harnesses, for how each reaches a decider.
    profiles: BTreeMap<String, crate::profiles::HarnessProfile>,
    /// Locks a code harness's trials get the skills and MCP servers of, by stack hash
    /// (`hlock add --eval` measures the lock with and without a dependency).
    pub trial_locks: BTreeMap<String, (Lockfile, PathBuf)>,
}

impl Resolver {
    /// Create a resolver that runs on the local, in-process reference harness.
    pub fn new(manifest: Manifest, workdir: impl Into<PathBuf>) -> Self {
        Self::with_backend(manifest, workdir, Backend::Local)
    }

    /// Create a resolver for a specific backend. `devbox`/`cluster` delegate trials
    /// to the Python worker (which may lift them to real Flyte tasks); `local`
    /// keeps the deterministic offline reference harness.
    pub fn with_backend(manifest: Manifest, workdir: impl Into<PathBuf>, backend: Backend) -> Self {
        let workdir: PathBuf = workdir.into();
        let store = Store::new(workdir.join("store"));
        let adapter: Box<dyn HarnessAdapter> = if backend.needs_worker() {
            Box::new(PythonAdapter::with_backend(backend))
        } else {
            Box::new(ReferenceAdapter::default())
        };
        Resolver {
            manifest,
            workdir,
            store,
            backend,
            pack_by_task: BTreeMap::new(),
            adapter,
            holdout: Vec::new(),
            harness_override: None,
            code_harnesses: BTreeMap::new(),
            profiles: BTreeMap::new(),
            trial_locks: BTreeMap::new(),
        }
    }

    /// Load the manifest's eval packs and return the search-split tasks.
    pub fn load_packs(&mut self) -> Vec<crate::tasks::Task> {
        let mut search: Vec<crate::tasks::Task> = Vec::new();
        let base = self.manifest.root();
        for ref_ in self.manifest.eval_packs() {
            match EvalPack::load(&ref_, Some(&base)) {
                Ok(pack) => {
                    let errors = pack.check().iter().filter(|i| i.level == "error").count();
                    if errors > 0 {
                        eprintln!("warn: eval pack {ref_} has {errors} problem(s); run `hlock evals check {ref_}`");
                    }
                    let (s, h) = pack.split(self.manifest.search_frac(), self.manifest.budget.random_seed);
                    for t in pack.tasks.clone() {
                        self.pack_by_task.insert(t.id.clone(), pack.clone());
                    }
                    search.extend(s);
                    self.holdout.extend(h);
                }
                Err(e) => eprintln!("warn: skipping eval pack {ref_}: {e}"),
            }
        }
        search
    }

    /// The holdout-split tasks (populated by [`load_packs`]).
    pub fn holdout(&self) -> &[crate::tasks::Task] {
        &self.holdout
    }

    /// Cartesian product over the manifest's candidate axes. Errors when `[deciders]`
    /// doesn't parse.
    pub fn enumerate_stacks(&self) -> Result<Vec<Stack>> {
        let c = &self.manifest.candidates;
        let llm_combos = llm_combos(&c.llm);
        let decisions = if c.decision.is_empty() {
            vec!["none".to_string()]
        } else {
            c.decision.clone()
        };
        let harnesses = harness_combos(&c.harness);
        let specs = self.manifest.deciders()?;

        let mut stacks = Vec::new();
        for llm in &llm_combos {
            for decision in &decisions {
                for (harness, pattern) in &harnesses {
                  for deciders in self.decider_combos(&specs, harness) {
                    stacks.push(Stack {
                        llm: llm.clone(),
                        harness: harness.clone(),
                        pattern: pattern.clone(),
                        decision: decision.clone(),
                        tools: self.manifest.tools.keys().cloned().collect(),
                        skills: self.manifest.skills.keys().cloned().collect(),
                        instructions: instructions_files(&self.manifest.instructions),
                        max_steps: 40,
                        max_cost_usd_per_task: None,
                        retries: policy_axis(&self.manifest.candidates.policy, "retries", 0),
                        temperature: policy_axis_float(&self.manifest.candidates.policy, "temperature"),
                        tool_timeout_seconds: None,
                        deciders,
                    });
                  }
                }
            }
        }
        if stacks.len() > 200 {
            stacks.truncate(200);
        }
        Ok(stacks)
    }

    /// Every combination of the agent-side deciders `harness` can use: a fixed `model`,
    /// or each of `any-of` (`none` leaves the use out).
    fn decider_combos(&self, specs: &BTreeMap<String, crate::decide::DeciderSpec>, harness: &str) -> Vec<BTreeMap<String, String>> {
        let mut combos = vec![BTreeMap::new()];
        for (use_, spec) in specs.iter().filter(|(u, _)| crate::decide::AGENT_USES.contains(&u.as_str())) {
            if crate::decide::wiring(use_, harness, self.profiles.get(harness)).is_none() {
                eprintln!("warn: {harness} can't make {use_} decisions; [deciders.{use_}] is left out of its stacks");
                continue;
            }
            // each model, at each `min-probability` candidate, in the manifest's order
            let mut options: Vec<String> = Vec::new();
            for m in spec.model.iter().chain(spec.any_of.iter()) {
                for t in spec.thresholds() {
                    let o = match t {
                        Some(p) if m != "none" => format!("{m}@{p}"),
                        _ => m.clone(),
                    };
                    if !options.contains(&o) {
                        options.push(o);
                    }
                }
            }
            combos = combos
                .into_iter()
                .flat_map(|c| {
                    options.iter().map(move |o| {
                        let mut c = c.clone();
                        if o != "none" {
                            c.insert(use_.clone(), o.clone());
                        }
                        c
                    })
                })
                .collect();
        }
        combos
    }

    /// The manifest's `[deciders.*]` for a trial, with policy paths made absolute: the
    /// trial's lock lives in its workspace, not next to the manifest.
    fn trial_deciders(&self) -> Result<BTreeMap<String, crate::decide::DeciderSpec>> {
        let root = self.manifest.root();
        let mut specs = self.manifest.deciders()?;
        for s in specs.values_mut() {
            if let Some(p) = &s.policy {
                s.policy = Some(root.join(p).to_string_lossy().into_owned());
            }
        }
        Ok(specs)
    }

    /// An empty workspace for a trial at `ws` under `ws_root`, for task `id`: the id
    /// must be a plain name and the path must stay under `ws_root`, since whatever is
    /// there is removed first.
    fn fresh_workspace(ws_root: &Path, ws: &Path, id: &str) -> Result<()> {
        if !crate::tasks::valid_task_id(id) {
            return Err(Error::EvalPack(format!("task id {id:?} can't name a workspace directory")));
        }
        crate::fsutil::check_within("trial workspace", ws_root, ws)?;
        if std::fs::symlink_metadata(ws).is_ok() {
            std::fs::remove_dir_all(ws)?;
        }
        std::fs::create_dir_all(ws)?;
        Ok(())
    }

    /// Run one trial (copying fixtures, running the harness, grading). When the harness
    /// can't run it, the trial fails and records why in `error` (and its stored trace).
    pub fn run_trial(&self, stack: &Stack, task: &crate::tasks::Task, seed: u64, ws_root: &Path) -> TrialResult {
        let hash = stack.hash();
        let ws = ws_root.join(format!("{}_{}_{}", &hash[..10.min(hash.len())], task.id, seed));

        let outcome = Self::fresh_workspace(ws_root, &ws, &task.id).and_then(|()| match self.code_harnesses.get(&stack.harness) {
            Some(p) => crate::contract::CommandAdapter::new(p.clone(), Ctx::from_process())
                .with_deciders(self.trial_deciders()?)
                .with_lock(self.trial_locks.get(&hash).cloned())
                .run(stack, task, &ws, seed),
            None => self.adapter.run(stack, task, &ws, seed),
        });
        let (outcome, error) = match outcome {
            Ok(o) => (o, None),
            Err(e) => {
                eprintln!("warn: {} couldn't run task {} (seed {seed}): {e}", stack.label(), task.id);
                let failed = crate::adapters::RunOutcome { success: false, cost_usd: 0.0, latency_s: 0.0, steps: 0, trace: json!([]) };
                (failed, Some(e.to_string()))
            }
        };
        let opts = crate::tasks::GradeOptions {
            judge: self.manifest.judge(),
            default_judge: self.manifest.default_judge(),
            auto: true,
            trace: Some(json!({"events": outcome.trace})),
            ..Default::default()
        };
        // a run that didn't happen has nothing to grade
        let pack = self.pack_by_task.get(&task.id).filter(|_| error.is_none());
        let grades = pack.map(|p| p.grade_with(task, &ws, &opts)).unwrap_or_default();
        let judged: Vec<&crate::tasks::Grade> = grades.iter().filter(|g| g.grader.starts_with("judge:") || g.grader.starts_with("trace:")).collect();
        let judging = (!judged.is_empty()).then(|| crate::search::TrialJudging {
            configured: pack.and_then(|p| p.judge_for(&opts)).map(|j| j.model),
            answered: judged.iter().filter_map(|g| g.judged_by.clone()).collect(),
            grades: judged.len(),
            uncertain: judged.iter().filter(|g| g.uncertain).count(),
            cost_usd: judged.iter().map(|g| g.cost_usd).sum::<Option<f64>>(),
        });
        let success = error.is_none() && outcome.success && grades.iter().all(|g| g.success);
        let score = if error.is_some() { 0.0 } else { grades.iter().map(|g| g.score).sum::<f64>() / grades.len().max(1) as f64 };

        // persist the trace to the store (content-addressed)
        let mut trace = json!({
            "trial": {"stack": hash, "task": task.id, "seed": seed},
            "events": outcome.trace,
            "result": {"artifacts": task.expected_artifacts()},
            "metrics": {"steps": outcome.steps, "cost_usd": outcome.cost_usd, "latency_s": outcome.latency_s},
            "grades": grades,
        });
        if let Some(e) = &error {
            trace["error"] = json!(e);
        }
        let _ = self.store.put_value(&trace);

        TrialResult {
            stack_hash: hash,
            task_id: task.id.clone(),
            seed,
            success,
            score,
            cost_usd: outcome.cost_usd,
            latency_s: outcome.latency_s,
            steps: outcome.steps,
            error,
            judging,
        }
    }

    /// Resolve: enumerate, run, aggregate, select, and write `harness.lock`.
    pub fn resolve(&mut self) -> Result<Lockfile> {
        let lock = self.resolve_lock()?;
        lock.save(std::path::Path::new(crate::LOCKFILE_PATH))?;
        Ok(lock)
    }

    /// What runs a stack's trials: a code harness's id, the worker, or `reference`.
    pub fn adapter_for(&self, stack: &Stack) -> String {
        match self.code_harnesses.get(&stack.harness) {
            Some(p) => format!("code harness {}", p.id),
            None => self.adapter.name().to_string(),
        }
    }

    /// Load the profiles trials run through (code harnesses, and how each candidate
    /// harness reaches a decider), narrowing the candidates to the target harness.
    pub fn prepare_trials(&mut self) -> Result<()> {
        let registry = Registry::load_for(&Ctx::from_process(), Some(&self.manifest))?;
        apply_target(&mut self.manifest, self.harness_override.as_deref(), &registry)?;
        self.code_harnesses = registry
            .harnesses
            .iter()
            .filter(|h| h.kind == "custom" && (h.run.entrypoint.is_some() || !h.run.command.is_empty()))
            .map(|h| (h.id.clone(), h.clone()))
            .collect();
        self.profiles = self.manifest.candidates.harness.iter().filter_map(|h| registry.harness(&h.name).map(|p| (h.name.clone(), p.clone()))).collect();
        Ok(())
    }

    /// Resolve and return the winning lock without writing it.
    pub fn resolve_lock(&mut self) -> Result<Lockfile> {
        self.prepare_trials()?;
        // problems in the objective or the deciders stop the resolve before any trial runs
        crate::search::parse_constraints(&self.manifest.objective.constraints)?;
        let mut stacks = self.enumerate_stacks()?;
        // stacks the project's policy denies (a provider, a model, a harness) aren't measured
        let policy = crate::policy::Policy::for_dir(&self.manifest.root())?;
        let mut dropped: Vec<String> = Vec::new();
        if !policy.is_empty() {
            stacks.retain(|s| {
                let mut v: Vec<crate::policy::Violation> = s.llm.values().filter_map(|spec| spec.split_once(':')).flat_map(|(p, m)| policy.check_model(p, m)).collect();
                v.extend(s.deciders.values().filter_map(|spec| spec.split('@').next()?.split_once(':')).flat_map(|(p, m)| policy.check_model(p, m)));
                if s.harness != "reference" {
                    v.extend(policy.check_harness(&s.harness, None).into_iter().filter(|x| !x.message.contains("version isn't known")));
                }
                if let Some(first) = v.first() {
                    dropped.push(format!("{}: {}", s.label(), first.message));
                    return false;
                }
                true
            });
            for d in &dropped {
                eprintln!("warn: policy dropped {d}");
            }
            if stacks.is_empty() {
                return Err(Error::Tuning(format!("the project's policy denies every candidate stack ({} dropped)", dropped.len())));
            }
        }
        let search_tasks = self.load_packs();
        if search_tasks.is_empty() {
            return Err(Error::EvalPack(format!(
                "no tasks to measure the stacks on: the eval packs ({}) have none, or didn't load",
                self.manifest.eval_packs().join(", ")
            )));
        }
        let seeds = self.manifest.seeds();
        let ws_root = self.workdir.join("runs");
        let _ = std::fs::create_dir_all(&ws_root);

        // every (task, seed), seed by seed so a cut budget still covers each task
        let plan: Vec<(&crate::tasks::Task, u64)> = (0..seeds).flat_map(|seed| search_tasks.iter().map(move |t| (t, seed))).collect();
        // `max-trials` is spent round-robin across the stacks, so none is dropped whole
        // and no trial runs that the budget then discards
        let max = self.manifest.budget.max_trials.unwrap_or(usize::MAX);
        let mut results: Vec<Vec<TrialResult>> = vec![Vec::new(); stacks.len()];
        let mut n = 0usize;
        'budget: for (task, seed) in &plan {
            for (stack, out) in stacks.iter().zip(results.iter_mut()) {
                if n >= max {
                    break 'budget;
                }
                out.push(self.run_trial(stack, task, *seed, &ws_root));
                n += 1;
            }
        }
        if n < plan.len() * stacks.len() {
            eprintln!("warn: budget.max-trials ({max}) ran {n} of {} trials, spread across the stacks", plan.len() * stacks.len());
        }

        let mut scores: Vec<StackScore> = Vec::new();
        for (stack, trials) in stacks.iter().zip(results) {
            if trials.is_empty() {
                eprintln!("warn: {} got no trials within budget.max-trials", stack.label());
                continue;
            }
            // a stack whose harness couldn't run a single trial is broken, not slow
            if trials.iter().all(|t| t.error.is_some()) {
                let e = trials[0].error.clone().unwrap_or_default();
                return Err(Error::Tuning(format!("every trial of {} failed to run: {e}", stack.label())));
            }
            scores.push(StackScore { stack: stack.clone(), n: trials.len(), trials });
        }

        let winner = select_winner(
            scores.clone(),
            &self.manifest.objective.primary,
            &self.manifest.objective.constraints,
            &self.manifest.objective.select,
        )?;

        let mut lock = self.build_resolve(&winner, &scores)?;
        if !dropped.is_empty() {
            lock.provenance.search.insert("dropped-by-policy".into(), serde_json::json!(dropped));
        }
        let _ = self.write_report(&lock, &scores);
        Ok(lock)
    }

    fn build_resolve(&self, winner: &StackScore, _scores: &[StackScore]) -> Result<Lockfile> {
        let s = &winner.stack;
        let llms = s
            .llm
            .iter()
            .map(|(role, spec)| {
                let (provider, model) = match spec.split_once(':') {
                    Some((p, m)) if !m.is_empty() => (p.to_string(), m.to_string()),
                    _ => ("reference".to_string(), spec.clone()),
                };
                LLM {
                    role: role.clone(),
                    provider,
                    model,
                    resolved_from: Some(spec.clone()),
                    params: BTreeMap::new(),
                    fingerprint: Some(format!("sha256:{}", crate::hash::sha256_hex(spec.as_bytes()))),
                    ..Default::default()
                }
            })
            .collect();

        let mut success_rate = BTreeMap::new();
        success_rate.insert("mean".to_string(), roundf(winner.success_rate()));
        let mut cost = BTreeMap::new();
        cost.insert("mean".to_string(), roundf(winner.mean_cost()));
        let mut latency = BTreeMap::new();
        latency.insert("p50".to_string(), roundf(winner.mean_latency()));
        latency.insert("p95".to_string(), roundf(winner.p95_latency()));

        let mut stack_map = BTreeMap::new();
        stack_map.insert("name".to_string(), serde_json::json!(self.manifest.name()));
        stack_map.insert("selection".to_string(), serde_json::json!(self.manifest.objective.select));
        let mut search_map = BTreeMap::new();
        search_map.insert("strategy".to_string(), serde_json::json!(self.manifest.budget.strategy));
        search_map.insert("candidates".to_string(), serde_json::json!(winner.n));

        Ok(Lockfile {
            manifest_hash: Some(self.manifest.manifest_hash()),
            stack: stack_map,
            llm: llms,
            decision: Decision {
                kind: s.decision.clone(),
                decisions: if s.decision == "none" { vec![] } else { vec!["retry".into(), "stop".into()] },
                ..Default::default()
            },
            decider: self.lock_deciders(s)?,
            harness: self.pin_harness(&s.harness, &s.pattern),
            tool: s.tools.iter().map(|t| Tool { name: t.clone(), kind: "builtin".into(), ..Default::default() }).collect(),
            skill: s.skills.iter().map(|sk| Skill { name: sk.clone(), origin: "first-party".into(), editable: true, evidence_digest: Some(String::new()), ..Default::default() }).collect(),
            instructions: s.instructions.iter().map(|i| Instructions { name: i.clone(), scope: "project".into(), ..Default::default() }).collect(),
            policy: Policy {
                retries: 1,
                max_steps: 40,
                human_approval: "never".into(),
                ..Default::default()
            },
            context: Context { isolation: self.manifest.context.get("isolation").and_then(serde_json::Value::as_str).unwrap_or("locked-only").into(), ..Default::default() },
            evidence: Evidence {
                eval_pack: Some(self.manifest.eval_packs().join(",")),
                eval_pack_version: crate::tasks::packs_version(&self.manifest),
                eval_pack_digest: Some(crate::tasks::packs_digest(&self.manifest)),
                split: "search".into(),
                trials: winner.n,
                success_rate,
                cost_usd_per_task: cost,
                latency_s_per_task: latency,
                judge: self.judge_model(winner),
                ..Default::default()
            },
            verify: Verify::default(),
            provenance: Provenance {
                backend: self.manifest.backend_target(),
                search: search_map,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// The judge that scored the winner's `judge` and `trace` graders, if any ran.
    fn judge_model(&self, winner: &StackScore) -> Option<crate::lockfile::EvidenceJudge> {
        let judged: Vec<&crate::search::TrialJudging> = winner.trials.iter().filter_map(|t| t.judging.as_ref()).collect();
        let grades: usize = judged.iter().map(|j| j.grades).sum();
        if grades == 0 {
            return None;
        }
        let configured: std::collections::BTreeSet<&String> = judged.iter().filter_map(|j| j.configured.as_ref()).collect();
        let answered: std::collections::BTreeSet<&String> = judged.iter().flat_map(|j| &j.answered).collect();
        let uncertain: usize = judged.iter().map(|j| j.uncertain).sum();
        let cost = judged.iter().map(|j| j.cost_usd).sum::<Option<f64>>();
        let model = configured.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(",");
        let decider = configured.iter().all(|m| crate::decide::is_decider(m, None));
        let protocol = configured.iter().next().and_then(|m| crate::decide::split_ref(m)).map(|(p, _)| {
            crate::decide::PROVIDERS.iter().find(|x| x.0 == p).map(|x| x.1.to_string()).unwrap_or_else(|| if decider { "systemone".into() } else { "chat".into() })
        });
        Some(crate::lockfile::EvidenceJudge {
            model,
            answered: (!answered.is_empty()).then(|| answered.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(",")),
            protocol,
            fallback: self.manifest.judge().and_then(|j| j.fallback),
            uncertain_grades: decider.then(|| roundf(uncertain as f64 / grades as f64)),
            cost_usd_per_task: cost.map(|c| (c / judged.len() as f64 * 1e8).round() / 1e8),
        })
    }

    /// The winner's `[[decider]]` entries. A decider that can't be locked is an error,
    /// not a lock that silently leaves it out.
    fn lock_deciders(&self, s: &Stack) -> Result<Vec<crate::lockfile::Decider>> {
        let specs = self.manifest.deciders()?;
        let root = self.manifest.root();
        let mut out = Vec::new();
        for (use_, reference) in &s.deciders {
            let spec = specs.get(use_).ok_or_else(|| Error::Manifest(format!("[deciders.{use_}] is gone from the manifest")))?;
            let (via, wiring) = crate::decide::wiring(use_, &s.harness, self.profiles.get(&s.harness))
                .ok_or_else(|| Error::Manifest(format!("{} can't make {use_} decisions", s.harness)))?;
            out.push(crate::decide::lock_entry(use_, reference, spec, &via, wiring, &root).map_err(|e| Error::Manifest(format!("[deciders.{use_}]: {e}")))?);
        }
        Ok(out)
    }

    /// The `[harness]` table for the winner. A harness with a profile gets the
    /// version and install source of the copy on this machine, and `adapter`
    /// records what actually ran the trials when that differs from the harness.
    fn pin_harness(&self, name: &str, pattern: &str) -> Harness {
        let mut h = Harness { name: name.to_string(), pattern: Some(pattern.to_string()), ..Default::default() };
        let ctx = Ctx::from_process();
        let Ok(registry) = Registry::load_for(&ctx, Some(&self.manifest)) else { return h };
        if let Some(profile) = registry.harness(name) {
            let probe = probe_harness(profile, &ctx);
            h.version = probe.version;
            h.source = probe.source;
            h.adapter = Some(if self.code_harnesses.contains_key(name) { name.to_string() } else { self.adapter.name().to_string() });
        }
        h
    }

    fn write_report(&self, lock: &Lockfile, _scores: &[StackScore]) -> Result<()> {
        let report = self.workdir.join("report.md");
        std::fs::write(&report, format!("# harness-lock report\n\n{}\n", lock.summary()))?;
        Ok(())
    }
}

/// Narrow the harness candidates to the target harness (`explicit`, else the
/// manifest's `[target] harness`). With no harness candidates, or only the
/// default `reference`, the target becomes the sole candidate; otherwise it must
/// be one of them. Returns the canonical target id.
pub fn apply_target(manifest: &mut Manifest, explicit: Option<&str>, registry: &Registry) -> Result<Option<String>> {
    let Some(name) = explicit.map(str::to_string).or_else(|| manifest.target_harness()) else {
        return Ok(None);
    };
    if name == "reference" {
        manifest.candidates.harness.retain(|h| h.name == "reference");
        return Ok(Some(name));
    }
    let profile = registry.require_harness(&name)?;
    let cands = &mut manifest.candidates.harness;
    let only_default = cands.iter().all(|h| h.name == "reference");
    if only_default {
        let pattern = cands.first().map(|h| h.pattern.clone()).unwrap_or_default();
        *cands = vec![HarnessCandidate { name: profile.id.clone(), pattern, ..Default::default() }];
    } else {
        let before: Vec<String> = cands.iter().map(|h| h.name.clone()).collect();
        cands.retain(|h| profile.matches(&h.name));
        if cands.is_empty() {
            return Err(Error::Manifest(format!(
                "target harness {:?} is not one of [candidates] harness ({})",
                profile.id,
                before.join(", ")
            )));
        }
        for h in cands.iter_mut() {
            h.name = profile.id.clone();
        }
    }
    Ok(Some(profile.id.clone()))
}

fn roundf(v: f64) -> f64 {
    (v * 1e4).round() / 1e4
}

fn llm_combos(cands: &BTreeMap<String, crate::manifest::LLMCandidate>) -> Vec<BTreeMap<String, String>> {
    if cands.is_empty() {
        let mut m = BTreeMap::new();
        m.insert("planner".to_string(), "reference:qwen-7b".to_string());
        return vec![m];
    }
    let roles: Vec<String> = cands.keys().cloned().collect();
    let mut per_role: Vec<Vec<(String, String)>> = Vec::new();
    for role in &roles {
        let opts = cands.get(role).map(|c| c.any_of.clone()).unwrap_or_default();
        let opts = if opts.is_empty() {
            vec!["reference:qwen-7b".to_string()]
        } else {
            opts
        };
        per_role.push(opts.into_iter().map(|o| (role.clone(), o)).collect());
    }
    // cartesian product
    let mut combos: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for options in per_role {
        let mut next = Vec::new();
        for c in &combos {
            for (role, model) in &options {
                let mut cc = c.clone();
                cc.insert(role.clone(), model.clone());
                next.push(cc);
            }
        }
        combos = next;
    }
    combos
}

fn harness_combos(cands: &[HarnessCandidate]) -> Vec<(String, String)> {
    if cands.is_empty() {
        return vec![("reference".to_string(), "single-agent".to_string())];
    }
    let mut out = Vec::new();
    for h in cands {
        let patterns = if h.pattern.is_empty() { vec!["single-agent".to_string()] } else { h.pattern.clone() };
        for p in patterns {
            out.push((h.name.clone(), p));
        }
    }
    out
}

fn instructions_files(ins: &BTreeMap<String, serde_json::Value>) -> Vec<String> {
    ins.get("files")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

fn policy_axis(policy: &BTreeMap<String, Vec<serde_json::Value>>, key: &str, default: u32) -> u32 {
    policy
        .get(key)
        .and_then(|v| v.first())
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(default)
}

fn policy_axis_float(policy: &BTreeMap<String, Vec<serde_json::Value>>, key: &str) -> Option<f64> {
    policy
        .get(key)
        .and_then(|v| v.first())
        .and_then(serde_json::Value::as_f64)
}
