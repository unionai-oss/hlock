//! The search layer: stacks, trials, aggregation, Pareto front, selection.

use std::collections::BTreeMap;

use serde_json::json;

use crate::error::{Error, Result};
use crate::hash::content_hash;

/// One concrete agent configuration (an axis of the search space).
#[derive(Debug, Clone)]
pub struct Stack {
    pub llm: BTreeMap<String, String>, // role -> "provider:model" spec
    pub harness: String,
    pub pattern: String,
    pub decision: String,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub instructions: Vec<String>,
    pub max_steps: u32,
    pub max_cost_usd_per_task: Option<f64>,
    /// Number of agent-level retries (a policy under test; the reference harness
    /// retries a "failed" step up to this many times, so more retries raise
    /// success at the cost of more tokens/latency).
    pub retries: u32,
    /// Sampling temperature (recorded; the reference harness is deterministic).
    pub temperature: Option<f64>,
    /// Per-step tool timeout in seconds (an HPO axis at deeper depths).
    pub tool_timeout_seconds: Option<u64>,
    /// Decision models, by use (`approve` → `ollama:kev:4b`).
    pub deciders: BTreeMap<String, String>,
    /// Instruction files and skills a tune candidate puts in place of the lock's.
    pub overlay: Option<Overlay>,
}

/// A tune candidate's content, as a trial gets it (`crate::optimize::overlay`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// The candidate's digest, so each candidate is its own stack.
    pub digest: String,
    /// Its instruction files, which a single-completion trial gets as the system prompt.
    pub system_prompt: String,
}

impl Default for Stack {
    fn default() -> Self {
        Stack {
            llm: BTreeMap::new(),
            harness: "reference".into(),
            pattern: "single-agent".into(),
            decision: "none".into(),
            tools: Vec::new(),
            skills: Vec::new(),
            instructions: Vec::new(),
            max_steps: 40,
            max_cost_usd_per_task: None,
            retries: 0,
            temperature: None,
            tool_timeout_seconds: None,
            deciders: BTreeMap::new(),
            overlay: None,
        }
    }
}

impl Stack {
    pub fn label(&self) -> String {
        let llm = self.llm.values().cloned().collect::<Vec<_>>().join("+");
        format!("{}/{}/d={}", self.harness, llm, self.decision)
    }

    pub fn to_value(&self) -> serde_json::Value {
        let mut v = json!({
            "llm": self.llm,
            "harness": self.harness,
            "pattern": self.pattern,
            "decision": self.decision,
            "tools": sorted_list(&self.tools),
            "skills": sorted_list(&self.skills),
            "instructions": sorted_list(&self.instructions),
            "max_steps": self.max_steps,
            "retries": self.retries,
            "temperature": self.temperature,
            "tool_timeout_seconds": self.tool_timeout_seconds,
        });
        if !self.deciders.is_empty() {
            v["deciders"] = json!(self.deciders); // only when set, so other stacks keep their hashes
        }
        if let Some(o) = &self.overlay {
            v["overlay"] = json!(o.digest);
            v["system_prompt"] = json!(o.system_prompt);
        }
        v
    }

    pub fn hash(&self) -> String {
        content_hash(&self.to_value())
    }
}

fn sorted_list(v: &[String]) -> Vec<String> {
    let mut c = v.to_vec();
    c.sort();
    c.dedup();
    c
}

/// One executed trial.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrialResult {
    pub stack_hash: String,
    pub task_id: String,
    pub seed: u64,
    pub success: bool,
    pub score: f64,
    pub cost_usd: f64,
    pub latency_s: f64,
    pub steps: u64,
    /// Why the harness couldn't run the trial (it counts as failed).
    pub error: Option<String>,
    /// What judged the trial's `judge` and `trace` graders, when any ran.
    pub judging: Option<TrialJudging>,
    /// Why it failed: the first failing grader and its detail, or the run's error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The judging of one trial: for the lock's `evidence.judge`, from the winner's trials.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TrialJudging {
    /// The judge configured for the task's pack.
    pub configured: Option<String>,
    /// The models that answered, as the provider named them.
    pub answered: Vec<String>,
    pub grades: usize,
    pub uncertain: usize,
    /// What judging cost, `None` when a price is unknown.
    pub cost_usd: Option<f64>,
}

/// Aggregated metrics for one stack.
#[derive(Debug, Clone)]
pub struct StackScore {
    pub stack: Stack,
    pub trials: Vec<TrialResult>,
    pub n: usize,
}

impl StackScore {
    pub fn success_rate(&self) -> f64 {
        let ok = self.trials.iter().filter(|t| t.success).count();
        ok as f64 / self.trials.len().max(1) as f64
    }

    pub fn mean_cost(&self) -> f64 {
        let s: f64 = self.trials.iter().map(|t| t.cost_usd).sum();
        s / self.trials.len().max(1) as f64
    }

    pub fn mean_latency(&self) -> f64 {
        let s: f64 = self.trials.iter().map(|t| t.latency_s).sum();
        s / self.trials.len().max(1) as f64
    }

    pub fn p95_latency(&self) -> f64 {
        let mut v: Vec<f64> = self.trials.iter().map(|t| t.latency_s).collect();
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            return 0.0;
        }
        let idx = (((v.len() as f64) * 0.95).ceil() as usize).min(v.len()) - 1;
        v[idx]
    }
}

fn dominates(a: (f64, f64), b: (f64, f64)) -> bool {
    // (quality, cost); a dominates b if >= quality, <= cost and strictly better in one
    (a.0 >= b.0 && a.1 <= b.1) && (a.0 > b.0 || a.1 < b.1)
}

/// Non-dominated stacks on (quality, cost). Quality higher is better.
pub fn pareto_front(mut scores: Vec<StackScore>) -> Vec<StackScore> {
    let pts: Vec<(f64, f64)> = scores
        .iter()
        .map(|s| (roundn(s.success_rate(), 6), roundn(s.mean_cost(), 8)))
        .collect();
    let mut front: Vec<StackScore> = Vec::new();
    for (i, s) in scores.drain(..).enumerate() {
        let dominated = pts
            .iter()
            .enumerate()
            .any(|(j, o)| j != i && dominates(*o, pts[i]));
        if !dominated {
            front.push(s);
        }
    }
    front.sort_by(|a, b| b.success_rate().total_cmp(&a.success_rate()).then(a.mean_cost().total_cmp(&b.mean_cost())));
    front
}

/// The metrics `[objective.constraints]` can limit.
pub const CONSTRAINT_METRICS: [&str; 2] = ["success-rate", "cost-usd-per-task"];

/// One `[objective.constraints]` entry, parsed: `metric op bound`.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub metric: String,
    pub op: String,
    pub bound: f64,
}

impl Constraint {
    fn value(&self, s: &StackScore) -> f64 {
        match self.metric.as_str() {
            "success-rate" => s.success_rate(),
            _ => s.mean_cost(),
        }
    }

    pub fn holds(&self, s: &StackScore) -> bool {
        let v = self.value(s);
        match self.op.as_str() {
            ">=" => v >= self.bound,
            "<=" => v <= self.bound,
            ">" => v > self.bound,
            _ => v < self.bound,
        }
    }
}

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}{}", self.metric, self.op, self.bound)
    }
}

/// Parse `[objective.constraints]`: each a known metric and a comparison (`>=`, `<=`,
/// `>`, `<`) with a number. An entry that doesn't parse is an error naming it.
pub fn parse_constraints(constraints: &BTreeMap<String, String>) -> Result<Vec<Constraint>> {
    constraints
        .iter()
        .map(|(metric, expr)| {
            let bad = |why: String| Error::Manifest(format!("[objective.constraints] {metric} = {expr:?}: {why}"));
            if !CONSTRAINT_METRICS.contains(&metric.as_str()) {
                return Err(bad(format!("unknown metric (use {})", CONSTRAINT_METRICS.join(" or "))));
            }
            let e = expr.trim();
            let op = [">=", "<=", ">", "<"].into_iter().find(|op| e.starts_with(op)).ok_or_else(|| bad("write a comparison like \">=0.8\" (>=, <=, >, or <)".into()))?;
            let bound = e[op.len()..].trim().parse::<f64>().ok().filter(|b| b.is_finite()).ok_or_else(|| bad(format!("{:?} isn't a number", e[op.len()..].trim())))?;
            Ok(Constraint { metric: metric.clone(), op: op.into(), bound })
        })
        .collect()
}

/// Select the winner: drop the stacks that miss a constraint, keep the Pareto front of
/// the rest, and apply the select rule. Errors when a constraint doesn't parse, and when
/// no stack meets every constraint (naming the closest one), rather than picking a
/// stack that misses one.
pub fn select_winner(scores: Vec<StackScore>, primary: &str, constraints: &BTreeMap<String, String>, select: &str) -> Result<StackScore> {
    let constraints = parse_constraints(constraints)?;
    // an unknown rule is an error, not a silent `min-cost`
    if !primary.is_empty() && !crate::manifest::PRIMARIES.contains(&primary) {
        return Err(Error::Manifest(format!("[objective] primary = {primary:?} isn't supported (use \"success-rate\")")));
    }
    if !select.is_empty() && !crate::manifest::SELECTS.contains(&select) {
        return Err(Error::Manifest(format!("[objective] select = {select:?} isn't supported (use \"min-cost\" or \"max-quality\")")));
    }
    let no_stack = || Error::Tuning("no viable stack found".into());
    let (eligible, missed): (Vec<StackScore>, Vec<StackScore>) = scores.into_iter().partition(|s| constraints.iter().all(|c| c.holds(s)));
    if eligible.is_empty() {
        // the closest: fewest constraints missed, then the best success rate
        let missing = |s: &StackScore| constraints.iter().filter(|c| !c.holds(s)).count();
        let best = missed.iter().min_by(|a, b| missing(a).cmp(&missing(b)).then(b.success_rate().total_cmp(&a.success_rate()))).ok_or_else(no_stack)?;
        let failed: Vec<String> = constraints.iter().filter(|c| !c.holds(best)).map(Constraint::to_string).collect();
        return Err(Error::Tuning(format!(
            "no stack meets [objective.constraints]; the closest, {}, has success-rate {:.4} and cost-usd-per-task {:.6}, missing {}",
            best.stack.label(),
            best.success_rate(),
            best.mean_cost(),
            failed.join(", ")
        )));
    }
    let pool = pareto_front(eligible);
    let winner = match select {
        "max-quality" => pool.into_iter().max_by(|a, b| a.success_rate().total_cmp(&b.success_rate())),
        _ => pool.into_iter().min_by(|a, b| a.mean_cost().total_cmp(&b.mean_cost())),
    };
    winner.ok_or_else(no_stack)
}

fn roundn(v: f64, n: i32) -> f64 {
    let m = 10f64.powi(n);
    (v * m).round() / m
}
