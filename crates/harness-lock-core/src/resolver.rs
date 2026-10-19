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
    /// What judged the trials: answered models, and how many grades were uncertain.
    judging: std::cell::RefCell<JudgeTally>,
}

#[derive(Default)]
struct JudgeTally {
    configured: std::collections::BTreeSet<String>,
    answered: std::collections::BTreeSet<String>,
    grades: usize,
    uncertain: usize,
    /// Trials with judge grades, and what their judging cost (`None` once a price is unknown).
    trials: usize,
    cost: Option<f64>,
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
            Box::new(PythonAdapter::new())
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
            judging: Default::default(),
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

    /// Cartesian product over the manifest's candidate axes.
    pub fn enumerate_stacks(&self) -> Vec<Stack> {
        let c = &self.manifest.candidates;
        let llm_combos = llm_combos(&c.llm);
        let decisions = if c.decision.is_empty() {
            vec!["none".to_string()]
        } else {
            c.decision.clone()
        };
        let harnesses = harness_combos(&c.harness);
        let specs = self.manifest.deciders().unwrap_or_default();

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
        stacks
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
            // each model, at each `min-probability` candidate
            let models: Vec<String> = spec.model.iter().cloned().chain(spec.any_of.iter().cloned()).collect();
            let options: Vec<String> = models
                .iter()
                .flat_map(|m| {
                    spec.thresholds().into_iter().map(move |t| match t {
                        Some(p) if m != "none" => format!("{m}@{p}"),
                        _ => m.clone(),
                    })
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
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

    /// Run one trial (copying fixtures, running the harness, grading).
    pub fn run_trial(&self, stack: &Stack, task: &crate::tasks::Task, seed: u64, ws_root: &Path) -> TrialResult {
        let ws = ws_root.join(format!("{}_{}_{}", &stack.hash()[..10.min(stack.hash().len())], task.id, seed));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::create_dir_all(&ws);

        let outcome = match self.code_harnesses.get(&stack.harness) {
            Some(p) => crate::contract::CommandAdapter::new(p.clone(), Ctx::from_process())
                .with_deciders(self.manifest.deciders().unwrap_or_default())
                .run(stack, task, &ws, seed),
            None => self.adapter.run(stack, task, &ws, seed),
        };
        let outcome = outcome.unwrap_or_else(|_| {
            crate::adapters::RunOutcome {
                success: false,
                cost_usd: 0.0,
                latency_s: 0.0,
                steps: 0,
                trace: json!([]),
            }
        });
        let opts = crate::tasks::GradeOptions {
            judge: self.manifest.judge(),
            default_judge: self.manifest.default_judge(),
            auto: true,
            trace: Some(json!({"events": outcome.trace})),
            ..Default::default()
        };
        let pack = self.pack_by_task.get(&task.id);
        let grades = pack.map(|p| p.grade_with(task, &ws, &opts)).unwrap_or_default();
        {
            let mut tally = self.judging.borrow_mut();
            let judged: Vec<&crate::tasks::Grade> = grades.iter().filter(|g| g.grader.starts_with("judge:") || g.grader.starts_with("trace:")).collect();
            if !judged.is_empty() {
                if let Some(j) = pack.and_then(|p| p.judge_for(&opts)) {
                    tally.configured.insert(j.model);
                }
            }
            if !judged.is_empty() {
                let cost = judged.iter().map(|g| g.cost_usd).sum::<Option<f64>>();
                tally.cost = if tally.trials == 0 { cost } else { tally.cost.zip(cost).map(|(a, b)| a + b) };
                tally.trials += 1;
            }
            tally.grades += judged.len();
            tally.uncertain += judged.iter().filter(|g| g.uncertain).count();
            tally.answered.extend(judged.iter().filter_map(|g| g.judged_by.clone()));
        }
        let success = outcome.success && grades.iter().all(|g| g.success);
        let score = grades.iter().map(|g| g.score).sum::<f64>() / grades.len().max(1) as f64;

        // persist the trace to the store (content-addressed)
        let trace = json!({
            "trial": {"stack": stack.hash(), "task": task.id, "seed": seed},
            "events": outcome.trace,
            "result": {"artifacts": task.expected_artifacts()},
            "metrics": {"steps": outcome.steps, "cost_usd": outcome.cost_usd, "latency_s": outcome.latency_s},
            "grades": grades,
        });
        let _ = self.store.put_value(&trace);

        TrialResult {
            stack_hash: stack.hash(),
            task_id: task.id.clone(),
            seed,
            success,
            score,
            cost_usd: outcome.cost_usd,
            latency_s: outcome.latency_s,
            steps: outcome.steps,
        }
    }

    /// Resolve: enumerate, run, aggregate, select, and write `harness.lock`.
    pub fn resolve(&mut self) -> Result<Lockfile> {
        let lock = self.resolve_lock()?;
        lock.save(std::path::Path::new(crate::LOCKFILE_PATH))?;
        Ok(lock)
    }

    /// Resolve and return the winning lock without writing it.
    pub fn resolve_lock(&mut self) -> Result<Lockfile> {
        let registry = Registry::load_for(&Ctx::from_process(), Some(&self.manifest))?;
        apply_target(&mut self.manifest, self.harness_override.as_deref(), &registry)?;
        self.code_harnesses = registry
            .harnesses
            .iter()
            .filter(|h| h.kind == "custom" && (h.run.entrypoint.is_some() || !h.run.command.is_empty()))
            .map(|h| (h.id.clone(), h.clone()))
            .collect();
        self.profiles = self.manifest.candidates.harness.iter().filter_map(|h| registry.harness(&h.name).map(|p| (h.name.clone(), p.clone()))).collect();
        let search_tasks = self.load_packs();
        let stacks = self.enumerate_stacks();
        let seeds = self.manifest.seeds();
        let ws_root = self.workdir.join("runs");
        let _ = std::fs::create_dir_all(&ws_root);

        let mut results_by_stack: BTreeMap<String, Vec<TrialResult>> = BTreeMap::new();
        for stack in &stacks {
            for task in &search_tasks {
                for seed in 0..seeds {
                    let tr = self.run_trial(stack, task, seed, &ws_root);
                    results_by_stack.entry(stack.hash()).or_default().push(tr);
                }
            }
        }
        // respect max-trials globally
        let mut n = 0usize;
        let max = self.manifest.budget.max_trials.unwrap_or(usize::MAX);
        for v in results_by_stack.values_mut() {
            if n >= max {
                v.clear();
            } else {
                v.truncate(max.saturating_sub(n));
                n += v.len();
            }
        }

        let mut scores: Vec<StackScore> = Vec::new();
        for stack in &stacks {
            if let Some(trials) = results_by_stack.get(&stack.hash()) {
                if !trials.is_empty() {
                    scores.push(StackScore {
                        stack: stack.clone(),
                        trials: trials.clone(),
                        n: trials.len(),
                    });
                }
            }
        }

        let winner = select_winner(
            scores.clone(),
            &self.manifest.objective.primary,
            &self.manifest.objective.constraints,
            &self.manifest.objective.select,
        )
        .ok_or_else(|| Error::Tuning("no viable stack found".into()))?;

        let lock = self.build_resolve(&winner, &scores);
        let _ = self.write_report(&lock, &scores);
        Ok(lock)
    }

    fn build_resolve(&self, winner: &StackScore, _scores: &[StackScore]) -> Lockfile {
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

        Lockfile {
            manifest_hash: Some(self.manifest.manifest_hash()),
            stack: stack_map,
            llm: llms,
            decision: Decision {
                kind: s.decision.clone(),
                decisions: if s.decision == "none" { vec![] } else { vec!["retry".into(), "stop".into()] },
                ..Default::default()
            },
            decider: self.lock_deciders(s),
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
                split: "search".into(),
                trials: winner.n,
                success_rate,
                cost_usd_per_task: cost,
                latency_s_per_task: latency,
                judge: self.judge_model(),
                ..Default::default()
            },
            verify: Verify::default(),
            provenance: Provenance {
                backend: self.manifest.backend_target(),
                search: search_map,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The judge that scored the trials' `judge` and `trace` graders, if any ran.
    fn judge_model(&self) -> Option<crate::lockfile::EvidenceJudge> {
        let t = self.judging.borrow();
        if t.grades == 0 {
            return None;
        }
        let model = t.configured.iter().cloned().collect::<Vec<_>>().join(",");
        let decider = t.configured.iter().all(|m| crate::decide::is_decider(m, None));
        let protocol = t.configured.iter().next().and_then(|m| crate::decide::split_ref(m)).map(|(p, _)| {
            crate::decide::PROVIDERS.iter().find(|x| x.0 == p).map(|x| x.1.to_string()).unwrap_or_else(|| if decider { "systemone".into() } else { "chat".into() })
        });
        Some(crate::lockfile::EvidenceJudge {
            model,
            answered: (!t.answered.is_empty()).then(|| t.answered.iter().cloned().collect::<Vec<_>>().join(",")),
            protocol,
            fallback: self.manifest.judge().and_then(|j| j.fallback),
            uncertain_grades: decider.then(|| roundf(t.uncertain as f64 / t.grades as f64)),
            cost_usd_per_task: t.cost.filter(|_| t.trials > 0).map(|c| (c / t.trials as f64 * 1e8).round() / 1e8),
        })
    }

    /// The winner's `[[decider]]` entries.
    fn lock_deciders(&self, s: &Stack) -> Vec<crate::lockfile::Decider> {
        let specs = self.manifest.deciders().unwrap_or_default();
        let root = self.manifest.root();
        s.deciders
            .iter()
            .filter_map(|(use_, reference)| {
                let spec = specs.get(use_)?;
                let (via, wiring) = crate::decide::wiring(use_, &s.harness, self.profiles.get(&s.harness))?;
                match crate::decide::lock_entry(use_, reference, spec, &via, wiring, &root) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        eprintln!("warn: {e}");
                        None
                    }
                }
            })
            .collect()
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
