//! The search layer: stacks, trials, aggregation, Pareto front, selection.

use std::collections::BTreeMap;

use serde_json::json;

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
#[derive(Debug, Clone)]
pub struct TrialResult {
    pub stack_hash: String,
    pub task_id: String,
    pub seed: u64,
    pub success: bool,
    pub score: f64,
    pub cost_usd: f64,
    pub latency_s: f64,
    pub steps: u64,
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
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
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
    front.sort_by(|a, b| {
        b.success_rate()
            .partial_cmp(&a.success_rate())
            .unwrap()
            .then(a.mean_cost().partial_cmp(&b.mean_cost()).unwrap())
    });
    front
}

/// Select winner from the Pareto front honoring objective constraints + select rule.
pub fn select_winner(scores: Vec<StackScore>, _primary: &str, constraints: &BTreeMap<String, String>, select: &str) -> Option<StackScore> {
    let front = pareto_front(scores);
    if front.is_empty() {
        return None;
    }
    fn ok(s: &StackScore, constraints: &BTreeMap<String, String>) -> bool {
        for (metric, expr) in constraints {
            let val = match metric.as_str() {
                "success-rate" => Some(s.success_rate()),
                "cost-usd-per-task" => Some(s.mean_cost()),
                _ => None,
            };
            if let Some(v) = val {
                if let Some(op) = expr.strip_prefix(">=") {
                    if !(v >= op.trim().parse::<f64>().unwrap_or(-1.0)) {
                        return false;
                    }
                } else if let Some(op) = expr.strip_prefix("<=") {
                    if !(v <= op.trim().parse::<f64>().unwrap_or(f64::MAX)) {
                        return false;
                    }
                } else if let Some(op) = expr.strip_prefix('>') {
                    if !(v > op.trim().parse::<f64>().unwrap_or(-1.0)) {
                        return false;
                    }
                } else if let Some(op) = expr.strip_prefix('<') {
                    if !(v < op.trim().parse::<f64>().unwrap_or(f64::MAX)) {
                        return false;
                    }
                }
            }
        }
        true
    }
    let eligible: Vec<StackScore> = front.iter().filter(|s| ok(s, constraints)).cloned().collect();
    let pool = if eligible.is_empty() { front } else { eligible };
    if pool.is_empty() {
        return None;
    }
    match select {
        "max-quality" => pool
            .into_iter()
            .max_by(|a, b| a.success_rate().partial_cmp(&b.success_rate()).unwrap()),
        _ => pool.into_iter().min_by(|a, b| a.mean_cost().partial_cmp(&b.mean_cost()).unwrap()),
    }
}

fn roundn(v: f64, n: i32) -> f64 {
    let m = 10f64.powi(n);
    (v * m).round() / m
}
