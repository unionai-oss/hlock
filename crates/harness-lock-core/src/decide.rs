//! Decision models ("deciders"): typed questions answered with calibrated
//! probabilities instead of text (`specs/decision-models.md`).
//!
//! Two wire protocols cover the providers: **System One** (`POST /v1/systemone`:
//! TypeSafe's Jev, Kev on Ollama, Ollaya or its own server, Cloudflare's Clef,
//! OpenRouter's passthrough) and **Decisions** (`POST /v1/decisions`: OpenAI's
//! `gpt-6-luna`). [`decide`] takes one request shape and speaks either.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// `(provider, protocol, default base URL, base-URL variable, key variable)`.
pub const PROVIDERS: &[(&str, &str, &str, &str, &str)] = &[
    ("typesafe", "systemone", "https://api.typesafe.ai", "TYPESAFE_BASE_URL", "TYPESAFE_API_KEY"),
    ("openai-decisions", "decisions", "https://api.openai.com/v1", "", "OPENAI_API_KEY"),
    ("openrouter", "systemone", "https://openrouter.ai/api", "", "OPENROUTER_API_KEY"),
    ("ollama", "systemone", "http://localhost:11434", "OLLAMA_HOST", ""),
    ("ollaya", "systemone", "http://127.0.0.1:11435", "OLLAYA_HOST", "OLLAYA_API_KEY"),
    ("cloudflare", "systemone", "", "", "CLOUDFLARE_API_TOKEN"),
    ("system-one", "systemone", "", "SYSTEM_ONE_BASE_URL", "SYSTEM_ONE_API_KEY"),
];

/// Input price per million tokens, where it's published (output is free everywhere).
const PRICES: &[(&str, f64)] = &[("typesafe", 0.042), ("openai-decisions", 0.10), ("cloudflare", 0.24), ("ollama", 0.0), ("ollaya", 0.0)];

/// Model families that are decision models, for providers that also serve LLMs
/// (`ollama:kev:4b` is a decider, `ollama:qwen3:8b` an LLM).
pub const DECIDER_FAMILIES: &[&str] = &["jev", "kev", "clef", "span-0", "decider", "laya", "winnow", "decima", "arbiter", "nimble", "luna-decisions", "gpt-6-luna"];

/// The most questions one request may carry, per protocol (Clef's 64 is the tightest
/// System One limit we know of; Decisions allows 200).
const MAX_QUESTIONS: &[(&str, usize)] = &[("systemone", 64), ("decisions", 200)];

/// How to reach a decider, as written in `harness.toml`, `pack.toml` or a lock.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct DeciderConfig {
    /// `provider:model`, e.g. `typesafe:jev-1.13.0`, `ollama:kev:4b`.
    pub model: String,
    /// `systemone` or `decisions`; inferred from the provider when it's known.
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    /// TypeSafe's long-context mode (262k tokens; slower and less accurate).
    pub long_context: bool,
    /// Seconds to wait for an answer.
    pub timeout_seconds: Option<u64>,
}

fn provider(name: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str, &'static str)> {
    PROVIDERS.iter().find(|p| p.0 == name)
}

/// Split `provider:model`.
pub fn split_ref(reference: &str) -> Option<(&str, &str)> {
    reference.split_once(':').filter(|(p, m)| !p.is_empty() && !m.is_empty())
}

/// Whether a provider's API needs a key (self-hosted System One servers and local
/// runtimes don't).
pub fn needs_key(provider: &str) -> bool {
    matches!(provider, "typesafe" | "openai-decisions" | "openrouter" | "cloudflare")
}

/// Whether a `provider:model` reference (with an optional explicit protocol) names a
/// decision model rather than an LLM.
pub fn is_decider(reference: &str, protocol: Option<&str>) -> bool {
    match protocol {
        Some(p) => p == "systemone" || p == "decisions",
        None => match split_ref(reference) {
            Some(("typesafe" | "openai-decisions" | "ollaya" | "cloudflare" | "system-one", _)) => true,
            Some((_, model)) => {
                let m = model.to_ascii_lowercase();
                let base = m.rsplit('/').next().unwrap_or(&m);
                DECIDER_FAMILIES.iter().any(|f| base.starts_with(f) || m.contains(&format!("/{f}")))
            }
            None => false,
        },
    }
}

/// A decider resolved to an endpoint: what [`decide`] calls.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub provider: String,
    pub model: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub long_context: bool,
    pub timeout: Duration,
}

impl DeciderConfig {
    /// A config for `provider:model` with the provider's defaults.
    pub fn from_ref(model: &str) -> DeciderConfig {
        DeciderConfig { model: model.into(), ..Default::default() }
    }

    pub fn is_set(&self) -> bool {
        !self.model.trim().is_empty()
    }

    /// Resolve the endpoint, reading the key and base-URL variables through `var`.
    pub fn endpoint(&self, var: &dyn Fn(&str) -> Option<String>) -> Result<Endpoint, String> {
        let (prov, model) = split_ref(&self.model).ok_or_else(|| format!("decider {:?} isn't written provider:model", self.model))?;
        let known = provider(prov);
        let protocol = self.protocol.clone().or_else(|| known.map(|p| p.1.to_string())).unwrap_or_else(|| "systemone".into());
        if protocol != "systemone" && protocol != "decisions" {
            return Err(format!("unknown decider protocol {protocol:?} (use systemone or decisions)"));
        }
        let from_env = known.map(|p| p.3).filter(|v| !v.is_empty()).and_then(|v| var(v));
        let base_url = self
            .base_url
            .clone()
            .or(from_env)
            .or_else(|| known.map(|p| p.2.to_string()).filter(|u| !u.is_empty()))
            .ok_or_else(|| format!("hlock doesn't know where {prov:?} is; set the decider's base-url"))?;
        let base_url = if base_url.contains("://") { base_url } else { format!("http://{base_url}") };
        let key_env = self.key_env.clone().or_else(|| known.map(|p| p.4.to_string()).filter(|k| !k.is_empty()));
        let api_key = key_env.as_deref().and_then(|k| var(k)).filter(|v| !v.is_empty());
        let local = matches!(prov, "ollama" | "ollaya" | "system-one") || self.base_url.is_some();
        if api_key.is_none() && !local {
            return Err(format!("{} isn't set; the decider reads its {prov} API key from it", key_env.unwrap_or_default()));
        }
        Ok(Endpoint {
            provider: prov.into(),
            model: model.into(),
            protocol,
            base_url,
            api_key,
            long_context: self.long_context,
            timeout: Duration::from_secs(self.timeout_seconds.unwrap_or(60).max(1)),
        })
    }
}

impl Endpoint {
    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = if self.protocol == "decisions" { "decisions" } else { "systemone" };
        if base.ends_with("/v1") { format!("{base}/{path}") } else { format!("{base}/v1/{path}") }
    }

    /// The input price per million tokens, when it's known.
    pub fn price(&self) -> Option<f64> {
        PRICES.iter().find(|(p, _)| *p == self.provider).map(|(_, c)| *c)
    }
}

// --------------------------------------------------------------------------- //
// Questions and answers
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Is this statement true? `criteria` may be `{"true": …, "false": …}`.
    Noul,
    /// Which option? `criteria` maps each option to a description.
    Choice,
    /// Where on these ordered levels (worst first)? `criteria` is a list of descriptions.
    Score,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub criteria: Value,
}

impl Question {
    pub fn noul(id: &str, instructions: &str) -> Question {
        Question { id: id.into(), kind: Kind::Noul, instructions: instructions.into(), criteria: Value::Null }
    }

    pub fn choice(id: &str, instructions: &str, options: &[(&str, &str)]) -> Question {
        let m: Map<String, Value> = options.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        Question { id: id.into(), kind: Kind::Choice, instructions: instructions.into(), criteria: Value::Object(m) }
    }

    pub fn score(id: &str, instructions: &str, levels: &[String]) -> Question {
        Question { id: id.into(), kind: Kind::Score, instructions: instructions.into(), criteria: json!(levels) }
    }

    fn levels(&self) -> Vec<String> {
        self.criteria.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    }
}

/// One answer. `value` is P(true) for a noul, the option for a choice, and the
/// probability-weighted level (0 to n−1) for a score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Answer {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub value: Value,
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: Option<f64>,
    #[serde(default)]
    pub refused: bool,
}

impl Answer {
    /// P(true) for a noul; for a choice, the chosen option's probability.
    pub fn probability(&self) -> f64 {
        match self.kind {
            Kind::Noul => self.value.as_f64().unwrap_or(0.0),
            Kind::Choice => self.value.as_str().and_then(|c| self.probabilities.get(c)).copied().unwrap_or(0.0),
            Kind::Score => self.probabilities.values().cloned().fold(0.0, f64::max),
        }
    }

    /// How sure the answer is, 0–1: the provider's confidence, or for a noul its
    /// distance from 0.5.
    pub fn certainty(&self) -> f64 {
        self.confidence.unwrap_or_else(|| (2.0 * self.probability() - 1.0).abs())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Decided {
    /// The model that answered, as the provider names it (`jev-1.13.0`, not `jev-latest`).
    pub model: String,
    pub answers: Vec<Answer>,
    pub input_tokens: u64,
    pub cost_usd: Option<f64>,
}

impl Decided {
    pub fn get(&self, id: &str) -> Option<&Answer> {
        self.answers.iter().find(|a| a.id == id)
    }
}

/// The text of a state: strings as they are, anything else as JSON.
fn state_text(state: &Value) -> String {
    match state {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn systemone_body(ep: &Endpoint, state: &Value, questions: &[Question]) -> Value {
    let qs: Map<String, Value> = questions
        .iter()
        .map(|q| {
            let mut v = json!({"type": q.kind, "instructions": q.instructions});
            if !q.criteria.is_null() {
                v["criteria"] = q.criteria.clone();
            }
            (q.id.clone(), v)
        })
        .collect();
    json!({"model": ep.model, "state": state, "questions": qs})
}

fn decisions_body(ep: &Endpoint, state: &Value, questions: &[Question]) -> Value {
    let qs: Vec<Value> = questions
        .iter()
        .map(|q| match q.kind {
            Kind::Noul => {
                let mut ins = q.instructions.clone();
                if let (Some(t), Some(f)) = (q.criteria["true"].as_str(), q.criteria["false"].as_str()) {
                    ins.push_str(&format!("\nTrue means: {t}\nFalse means: {f}"));
                }
                json!({"type": "predicate", "name": q.id, "instructions": ins})
            }
            Kind::Choice => {
                let choices: Vec<Value> = q.criteria.as_object().into_iter().flatten().map(|(k, d)| json!({"value": k, "description": d})).collect();
                json!({"type": "choice", "name": q.id, "instructions": q.instructions, "choices": choices})
            }
            Kind::Score => {
                let levels: Vec<Value> = q.levels().iter().enumerate().map(|(i, d)| json!({"label": i.to_string(), "description": d})).collect();
                json!({"type": "score", "name": q.id, "instructions": q.instructions, "levels": levels})
            }
        })
        .collect();
    json!({"model": ep.model, "input": state_text(state), "questions": qs})
}

fn num_map(v: &Value) -> BTreeMap<String, f64> {
    match v {
        Value::Object(m) => m.iter().filter_map(|(k, p)| p.as_f64().map(|p| (k.clone(), p))).collect(),
        // Decisions: [{value|label, probability}]
        Value::Array(a) => a
            .iter()
            .filter_map(|e| {
                let key = e.get("label").or_else(|| e.get("value")).map(|k| k.as_str().map(str::to_string).unwrap_or_else(|| k.to_string()))?;
                Some((key, e["probability"].as_f64()?))
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn weighted(probabilities: &BTreeMap<String, f64>) -> Option<f64> {
    let total: f64 = probabilities.values().sum();
    if total <= 0.0 {
        return None;
    }
    let s: f64 = probabilities.iter().filter_map(|(k, p)| k.parse::<f64>().ok().map(|l| l * p)).sum();
    Some(s / total)
}

/// Read one answer from either protocol's shape.
fn read_answer(q: &Question, a: &Value) -> Answer {
    let probabilities = num_map(&a["probabilities"]);
    let confidence = a["confidence"].as_f64();
    let refused = a["type"].as_str() == Some("refusal");
    let value = match q.kind {
        Kind::Noul => json!(a.get("noul").or_else(|| a.get("probability")).and_then(Value::as_f64).unwrap_or(0.5)),
        Kind::Choice => a["choice"].clone(),
        Kind::Score => json!(weighted(&probabilities).or_else(|| a["score"].as_f64()).unwrap_or(0.0)),
    };
    let probabilities = match q.kind {
        Kind::Noul => {
            let p = value.as_f64().unwrap_or(0.5);
            BTreeMap::from([("true".to_string(), p), ("false".to_string(), 1.0 - p)])
        }
        _ => probabilities,
    };
    Answer { id: q.id.clone(), kind: q.kind, value, probabilities, confidence, refused }
}

fn read_response(ep: &Endpoint, questions: &[Question], v: &Value) -> Result<(String, Vec<Answer>, u64), String> {
    // Cloudflare's REST API wraps the body in `result`
    let v = if v.get("answers").is_none() && v.get("result").is_some() { &v["result"] } else { v };
    let answers = &v["answers"];
    let mut out = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let a = match answers {
            Value::Object(m) => m.get(&q.id),
            Value::Array(arr) => arr.iter().find(|a| a["name"].as_str() == Some(&q.id)).or_else(|| arr.get(i)),
            _ => None,
        };
        let a = a.ok_or_else(|| format!("the decider didn't answer {:?}", q.id))?;
        out.push(read_answer(q, a));
    }
    let model = v["model"].as_str().map(str::to_string).unwrap_or_else(|| ep.model.clone());
    Ok((model, out, v["usage"]["input_tokens"].as_u64().unwrap_or(0)))
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .proxy(ureq::Proxy::try_from_env())
        .build()
        .into()
}

fn post(ep: &Endpoint, body: &Value) -> Result<Value, String> {
    let url = ep.url();
    let mut wait = Duration::from_millis(250);
    for attempt in 0..3 {
        let mut req = agent(ep.timeout).post(&url).header("content-type", "application/json");
        if let Some(k) = &ep.api_key {
            req = req.header("authorization", &format!("Bearer {k}"));
        }
        if ep.long_context {
            req = req.header("TypeSafe-Danger-Zone-Long-Context", "true");
        }
        let mut resp = req.send(body.to_string()).map_err(|e| format!("cannot reach {url}: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| format!("bad response from {url}: {e}"))?;
        if matches!(status, 429 | 503 | 529) && attempt < 2 {
            std::thread::sleep(wait);
            wait *= 4;
            continue;
        }
        if !(200..300).contains(&status) {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.pointer("/error/message").or_else(|| v.get("error")).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(format!("{url} returned {status}: {detail}"));
        }
        return serde_json::from_str(&text).map_err(|e| format!("{url} returned invalid JSON: {e}"));
    }
    unreachable!()
}

/// Ask `questions` about `state`. Batches over a provider's limit are split into
/// several requests.
pub fn decide(ep: &Endpoint, state: &Value, questions: &[Question]) -> Result<Decided, String> {
    let limit = MAX_QUESTIONS.iter().find(|(p, _)| *p == ep.protocol).map(|(_, n)| *n).unwrap_or(64);
    let mut out = Decided { model: ep.model.clone(), ..Default::default() };
    for chunk in questions.chunks(limit.max(1)) {
        let body = if ep.protocol == "decisions" { decisions_body(ep, state, chunk) } else { systemone_body(ep, state, chunk) };
        let v = post(ep, &body)?;
        let (model, answers, tokens) = read_response(ep, chunk, &v)?;
        out.model = model;
        out.answers.extend(answers);
        out.input_tokens += tokens;
    }
    out.cost_usd = ep.price().map(|c| out.input_tokens as f64 * c / 1e6);
    Ok(out)
}

// --------------------------------------------------------------------------- //
// `[deciders.<use>]` in harness.toml
// --------------------------------------------------------------------------- //

/// One `[deciders.<use>]` table: a fixed `model` or `any-of` candidates for `resolve`,
/// and, for uses inside the agent, what it may decide and how sure it must be.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct DeciderSpec {
    pub model: Option<String>,
    pub any_of: Vec<String>,
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    pub long_context: bool,
    /// `approve` only: which of `allow`, `ask`, `deny` it may answer.
    pub may: Vec<String>,
    /// A file of the project's rules, added to the question.
    pub policy: Option<String>,
    /// How sure it must be to act: a number, or per answer (`{ deny = 0.8 }`).
    pub min_probability: Option<Value>,
    /// How long a hook waits before deferring.
    pub timeout_ms: Option<u64>,
    /// Override the harness events it's wired to (e.g. `["PermissionRequest"]`).
    pub hooks: Vec<String>,
}

/// What `approve` may decide by default: it can only make the agent more careful.
pub const DEFAULT_MAY: &[&str] = &["deny", "ask"];
pub const DEFAULT_TIMEOUT_MS: u64 = 2000;

impl DeciderSpec {
    /// Check a spec for `use_`; returns it with defaults filled in.
    pub fn validated(mut self, use_: &str) -> Result<DeciderSpec, String> {
        if !AGENT_USES.contains(&use_) && !HLOCK_USES.contains(&use_) {
            return Err(format!("[deciders.{use_}]: unknown use (expected one of {}, {})", AGENT_USES.join(", "), HLOCK_USES.join(", ")));
        }
        if self.model.is_some() == !self.any_of.is_empty() {
            return Err(format!("[deciders.{use_}] needs a `model` or `any-of` (one of them)"));
        }
        for m in self.model.iter().chain(&self.any_of).filter(|m| *m != "none") {
            if split_ref(m).is_none() {
                return Err(format!("[deciders.{use_}]: {m:?} isn't written provider:model"));
            }
            if !is_decider(m, self.protocol.as_deref()) {
                return Err(format!("[deciders.{use_}]: {m:?} isn't a decision model; set `protocol` if it is one"));
            }
        }
        if use_ != "approve" && !self.may.is_empty() {
            return Err(format!("[deciders.{use_}]: `may` is only for approve"));
        }
        if let Some(bad) = self.may.iter().find(|m| !["allow", "ask", "deny"].contains(&m.as_str())) {
            return Err(format!("[deciders.approve]: `may` takes allow, ask, deny, not {bad:?}"));
        }
        if use_ == "approve" && self.may.is_empty() {
            self.may = DEFAULT_MAY.iter().map(|s| s.to_string()).collect();
        }
        match &self.min_probability {
            None => {}
            Some(Value::Number(n)) if n.as_f64().is_some_and(|p| (0.0..=1.0).contains(&p)) => {}
            Some(Value::Object(m)) if use_ == "approve" && m.iter().all(|(k, v)| ["allow", "ask", "deny"].contains(&k.as_str()) && v.as_f64().is_some_and(|p| (0.0..=1.0).contains(&p))) => {}
            Some(_) => return Err(format!("[deciders.{use_}]: `min-probability` is a number from 0 to 1{}", if use_ == "approve" { ", or a table of allow, ask, deny" } else { "" })),
        }
        Ok(self)
    }

    pub fn config(&self, model: &str) -> DeciderConfig {
        DeciderConfig {
            model: model.into(),
            protocol: self.protocol.clone(),
            base_url: self.base_url.clone(),
            key_env: self.key_env.clone(),
            long_context: self.long_context,
            timeout_seconds: None,
        }
    }
}

/// How sure a decider must be to give `answer` for `use_`: the lock's `min-probability`,
/// else hlock's default (allow 0.9, ask 0.5, deny 0.8; stop 0.9).
pub fn min_probability(use_: &str, setting: &Value, answer: &str) -> f64 {
    let default = match (use_, answer) {
        ("approve", "allow") => 0.9,
        ("approve", "ask") => 0.5,
        ("approve", _) => 0.8,
        _ => 0.9,
    };
    match setting {
        Value::Number(n) => n.as_f64().unwrap_or(default),
        Value::Object(m) => m.get(answer).and_then(Value::as_f64).unwrap_or(default),
        _ => default,
    }
}

// --------------------------------------------------------------------------- //
// Lock entries
// --------------------------------------------------------------------------- //

/// How a harness reaches a decider for `use_`: `(via, wiring)`, or `None` when it
/// can't. The reference harness simulates every use, and code harnesses get any use
/// through the environment.
pub fn wiring(use_: &str, harness: &str, profile: Option<&crate::profiles::HarnessProfile>) -> Option<(String, Vec<crate::lockfile::Wiring>)> {
    use crate::lockfile::Wiring;
    if harness == "reference" {
        return Some(("simulated".into(), vec![]));
    }
    let p = profile?;
    if matches!(p.kind.as_str(), "framework" | "custom") {
        return Some(("env".into(), vec![]));
    }
    // a native setting is read by `pin`, never written: only hooks are wired in
    let h = p.deciders.get(use_)?.hook.as_ref()?;
    Some(("hook".into(), h.events.iter().map(|e| Wiring { file: h.file.clone(), event: e.clone() }).collect()))
}

/// The `[[decider]]` entry for `use_` answered by `reference`, wired as `via`/`wiring`.
/// `root` is the project directory the policy path is relative to.
pub fn lock_entry(
    use_: &str,
    reference: &str,
    spec: &DeciderSpec,
    via: &str,
    mut wiring: Vec<crate::lockfile::Wiring>,
    root: &std::path::Path,
) -> Result<crate::lockfile::Decider, String> {
    let (prov, model) = split_ref(reference).ok_or_else(|| format!("{reference:?} isn't written provider:model"))?;
    let known = provider(prov);
    let protocol = spec.protocol.clone().or_else(|| known.map(|p| p.1.to_string())).unwrap_or_else(|| "systemone".into());
    if !spec.hooks.is_empty() && via == "hook" {
        let file = wiring.first().map(|w| w.file.clone()).unwrap_or_default();
        wiring = spec.hooks.iter().map(|e| crate::lockfile::Wiring { file: file.clone(), event: e.clone() }).collect();
    }
    let policy = match &spec.policy {
        Some(path) => {
            let text = std::fs::read_to_string(root.join(path)).map_err(|e| format!("[deciders.{use_}] policy {path}: {e}"))?;
            Some(crate::lockfile::PolicyRef { path: path.clone(), digest: crate::hash::sha256_text(&text) })
        }
        None => None,
    };
    Ok(crate::lockfile::Decider {
        use_: use_.into(),
        provider: prov.into(),
        model: model.into(),
        protocol,
        endpoint: spec.base_url.clone().or_else(|| known.map(|p| p.2.to_string()).filter(|u| !u.is_empty())),
        key_env: spec.key_env.clone().or_else(|| known.map(|p| p.4.to_string()).filter(|k| !k.is_empty())),
        runtime: matches!(prov, "ollama" | "ollaya").then(|| prov.to_string()),
        resolved_from: Some(reference.into()),
        via: via.into(),
        wiring,
        may: spec.may.clone(),
        min_probability: spec.min_probability.clone().unwrap_or(Value::Null),
        timeout_ms: spec.timeout_ms,
        questions: questions_digest(use_),
        policy,
    })
}

/// A lock entry as a [`DeciderConfig`], to call it.
pub fn entry_config(d: &crate::lockfile::Decider) -> DeciderConfig {
    DeciderConfig {
        model: format!("{}:{}", d.provider, d.model),
        protocol: Some(d.protocol.clone()),
        base_url: d.endpoint.clone(),
        key_env: d.key_env.clone(),
        long_context: false,
        timeout_seconds: d.timeout_ms.map(|ms| ms.div_ceil(1000)),
    }
}

/// The environment a code harness gets for its deciders: `HLOCK_DECIDER_<USE>` (the
/// reference), `_PROTOCOL` and `_BASE_URL` for each, and the profile's framework
/// variables (`SYSTEM_ONE_BASE_URL`/`SYSTEM_ONE_API_KEY`, …) for the first System One
/// one, so PydanticAI, LangChain or DSPy pick it up with no code.
pub fn decider_env(deciders: &[(String, DeciderConfig)], framework_vars: &[(String, String)], var: &dyn Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let mut framework_set = false;
    for (use_, cfg) in deciders {
        let key = format!("HLOCK_DECIDER_{}", use_.to_ascii_uppercase());
        env.insert(key.clone(), cfg.model.clone());
        let Ok(ep) = cfg.endpoint(var) else { continue };
        env.insert(format!("{key}_PROTOCOL"), ep.protocol.clone());
        env.insert(format!("{key}_BASE_URL"), ep.base_url.clone());
        if ep.protocol == "systemone" && !framework_set {
            framework_set = true;
            for (base, key_var) in framework_vars {
                env.insert(base.clone(), ep.base_url.clone());
                if let Some(k) = &ep.api_key {
                    env.insert(key_var.clone(), k.clone());
                }
            }
        }
    }
    env
}

/// What changed in a lock's deciders that the lock alone can tell: hlock's questions
/// for a use (a newer hlock), and the policy files' contents.
pub fn drift(lock: &crate::lockfile::Lockfile, root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for d in &lock.decider {
        if d.questions.is_some() && d.questions != questions_digest(&d.use_) {
            out.push(format!("decider {}: hlock's questions for {} changed since the lock was written; re-resolve or re-pin to lock them", d.use_, d.use_));
        }
        if let Some(p) = &d.policy {
            match std::fs::read_to_string(root.join(&p.path)) {
                Ok(text) if crate::hash::sha256_text(&text) != p.digest => out.push(format!("decider {}: policy {} changed since the lock was written", d.use_, p.path)),
                Ok(_) => {}
                Err(_) => out.push(format!("decider {}: policy {} is missing", d.use_, p.path)),
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- //
// Finding a local decider
// --------------------------------------------------------------------------- //

/// A decision model a local runtime is serving: `ollama:kev:4b` when Ollama (or
/// Ollaya) is running with a Kev model pulled.
pub fn local_decider(var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    for (prov, family) in [("ollama", "kev"), ("ollaya", "kev")] {
        let cfg = DeciderConfig { model: format!("{prov}:x"), ..Default::default() };
        let Ok(ep) = cfg.endpoint(var) else { continue };
        let url = format!("{}/api/tags", ep.base_url.trim_end_matches('/'));
        let Ok(mut resp) = agent(Duration::from_millis(400)).get(&url).call() else { continue };
        let Ok(text) = resp.body_mut().read_to_string() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let names: Vec<String> = v["models"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str().map(str::to_string)).collect();
        // prefer the 4B, a good judge on modest hardware
        let pick = names.iter().find(|n| n.starts_with(&format!("{family}:4b"))).or_else(|| names.iter().find(|n| n.starts_with(family)));
        if let Some(n) = pick {
            return Some(format!("{prov}:{}", n.trim_end_matches(":latest")));
        }
    }
    None
}

// --------------------------------------------------------------------------- //
// Uses: the questions hlock asks for each decision in an agent
// --------------------------------------------------------------------------- //

/// Uses a decider can be locked for. In the agent: `approve`, `stop`, `route`,
/// `tool`. hlock's own: `default`, `judge`, `draft`.
pub const AGENT_USES: &[&str] = &["approve", "stop", "route", "tool"];
pub const HLOCK_USES: &[&str] = &["default", "judge", "draft"];

const APPROVE: &str = "An AI coding agent wants to make the tool call in the state. Decide whether it should run \
    without the user's review, need the user's review first, or be refused. Refuse calls that are destructive, \
    that reach outside the task, or that the project's policy forbids.";
const APPROVE_OPTIONS: &[(&str, &str)] = &[
    ("allow", "Safe to run without review: it reads, or makes changes the task and policy clearly allow"),
    ("ask", "The user should look at it first: it changes things the task may not cover, or it's hard to undo"),
    ("deny", "It shouldn't run: destructive, outside the task, or against the policy"),
];
const STOP: &str = "An AI agent is about to stop. Given the user's request and the agent's last message in the \
    state, has the agent done what the user asked?";

/// The question hlock asks for `use_`, with the project's `policy` added to it.
pub fn use_question(use_: &str, policy: Option<&str>) -> Option<Question> {
    let with_policy = |base: &str| match policy.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => format!("{base}\n\nThe project's policy:\n{p}"),
        None => base.to_string(),
    };
    match use_ {
        "approve" => Some(Question::choice("approve", &with_policy(APPROVE), APPROVE_OPTIONS)),
        "stop" => Some(Question {
            id: "done".into(),
            kind: Kind::Noul,
            instructions: with_policy(STOP),
            criteria: json!({"true": "The request is fully done, and the last message shows it", "false": "Something the user asked for is missing, failing, or unverified"}),
        }),
        _ => None,
    }
}

/// A digest of the questions hlock asks for a use (without the policy), locked so
/// that a change in hlock's questions shows up as drift.
pub fn questions_digest(use_: &str) -> Option<String> {
    use_question(use_, None).map(|q| crate::hash::sha256_text(&serde_json::to_string(&q).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_and_endpoints() {
        assert!(is_decider("typesafe:jev-1.13.0", None));
        assert!(is_decider("ollama:kev:4b", None));
        assert!(is_decider("openrouter:jaredpalmer/kev-4b", None));
        assert!(is_decider("openrouter:cloudflare/clef", None));
        assert!(!is_decider("ollama:qwen3:8b", None));
        assert!(!is_decider("anthropic:claude-sonnet-5-5", None));
        assert!(is_decider("mylab:thing", Some("systemone")));
        assert!(!is_decider("ollama:kev:4b", Some("chat")));

        let env = |k: &str| match k {
            "TYPESAFE_API_KEY" => Some("k".to_string()),
            "OLLAMA_HOST" => Some("127.0.0.1:9999".to_string()),
            _ => None,
        };
        let ep = DeciderConfig { model: "typesafe:jev-1.13.0".into(), ..Default::default() }.endpoint(&env).unwrap();
        assert_eq!((ep.url().as_str(), ep.api_key.as_deref(), ep.price()), ("https://api.typesafe.ai/v1/systemone", Some("k"), Some(0.042)));
        let ep = DeciderConfig { model: "ollama:kev:4b".into(), ..Default::default() }.endpoint(&env).unwrap();
        assert_eq!(ep.url(), "http://127.0.0.1:9999/v1/systemone");
        let ep = DeciderConfig { model: "openai-decisions:gpt-6-luna".into(), key_env: Some("TYPESAFE_API_KEY".into()), ..Default::default() }.endpoint(&env).unwrap();
        assert_eq!(ep.url(), "https://api.openai.com/v1/decisions");
        let err = DeciderConfig { model: "openai-decisions:gpt-6-luna".into(), ..Default::default() }.endpoint(&env).unwrap_err();
        assert_eq!(err, "OPENAI_API_KEY isn't set; the decider reads its openai-decisions API key from it");
        assert!(DeciderConfig { model: "cloudflare:clef".into(), ..Default::default() }.endpoint(&env).unwrap_err().contains("base-url"));
    }

    #[test]
    fn both_protocols_read_into_one_answer_shape() {
        let ep = Endpoint { provider: "x".into(), model: "m".into(), protocol: "systemone".into(), base_url: String::new(), api_key: None, long_context: false, timeout: Duration::from_secs(1) };
        let qs = vec![
            Question::noul("ok", "Is it ok?"),
            Question::choice("pick", "Which?", &[("a", "A"), ("b", "B")]),
            Question::score("lvl", "How good?", &["bad".into(), "ok".into(), "good".into()]),
        ];
        let s1 = json!({"model": "m-1.0", "usage": {"input_tokens": 12}, "answers": {
            "ok": {"type": "noul", "noul": 0.9},
            "pick": {"type": "choice", "choice": "b", "probabilities": {"a": 0.2, "b": 0.8}, "confidence": 0.6},
            "lvl": {"type": "score", "score": 1.5, "probabilities": {"0": 0.0, "1": 0.5, "2": 0.5}, "confidence": 0.3}}});
        let (model, a, tokens) = read_response(&ep, &qs, &s1).unwrap();
        assert_eq!((model.as_str(), tokens), ("m-1.0", 12));
        assert_eq!((a[0].probability(), a[1].probability(), a[2].value.as_f64()), (0.9, 0.8, Some(1.5)));
        assert!((a[0].certainty() - 0.8).abs() < 1e-9);

        let d = json!({"model": "gpt-6-luna", "answers": [
            {"type": "predicate", "name": "ok", "probability": 0.1},
            {"type": "choice", "name": "pick", "choice": "a", "confidence": 0.9, "probabilities": [{"value": "a", "probability": 0.95}, {"value": "b", "probability": 0.05}]},
            {"type": "score", "name": "lvl", "score": 2.0, "confidence": 0.9, "probabilities": [{"label": "0", "value": 0, "probability": 0.0}, {"label": "1", "value": 1, "probability": 0.0}, {"label": "2", "value": 2, "probability": 1.0}]}]});
        let (_, a, _) = read_response(&Endpoint { protocol: "decisions".into(), ..ep.clone() }, &qs, &d).unwrap();
        assert_eq!((a[0].probability(), a[1].value.as_str(), a[2].value.as_f64()), (0.1, Some("a"), Some(2.0)));

        // Cloudflare wraps its body in `result`
        let cf = json!({"result": {"model": "clef", "answers": {"ok": {"type": "noul", "noul": 0.7}}}});
        let (_, a, _) = read_response(&ep, &qs[..1], &cf).unwrap();
        assert_eq!(a[0].probability(), 0.7);
        assert!(read_response(&ep, &qs, &json!({"answers": {}})).unwrap_err().contains("didn't answer"));
    }

    #[test]
    fn decisions_requests_use_its_own_names() {
        let ep = Endpoint { provider: "openai-decisions".into(), model: "gpt-6-luna".into(), protocol: "decisions".into(), base_url: String::new(), api_key: None, long_context: false, timeout: Duration::from_secs(1) };
        let mut q = Question::noul("ok", "Is it ok?");
        q.criteria = json!({"true": "yes", "false": "no"});
        let b = decisions_body(&ep, &json!({"a": 1}), &[q, Question::score("s", "How?", &["x".into(), "y".into()])]);
        assert_eq!(b["questions"][0]["type"], "predicate");
        assert!(b["questions"][0]["instructions"].as_str().unwrap().ends_with("True means: yes\nFalse means: no"));
        assert_eq!(b["questions"][1]["levels"][1], json!({"label": "1", "description": "y"}));
        assert!(b["input"].as_str().unwrap().contains("\"a\": 1"));
    }

    #[test]
    fn use_questions_carry_the_policy_and_a_stable_digest() {
        let q = use_question("approve", Some("Never push to main.")).unwrap();
        assert!(q.instructions.ends_with("The project's policy:\nNever push to main."));
        assert_eq!(q.criteria.as_object().unwrap().keys().collect::<Vec<_>>(), ["allow", "ask", "deny"]);
        assert_eq!(questions_digest("approve"), questions_digest("approve"));
        assert_ne!(questions_digest("approve"), questions_digest("stop"));
        assert!(use_question("judge", None).is_none());
    }
}

// --------------------------------------------------------------------------- //
// Hooks: `hlock decide hook <use> --format <harness>`
// --------------------------------------------------------------------------- //

/// What a hook command prints and exits with. An empty `stdout` and exit 0 make no
/// decision, so the harness does what it would have done.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookOutput {
    pub stdout: String,
    pub exit: i32,
    /// What happened, for the decision log and `--verbose`.
    pub note: String,
}

impl HookOutput {
    fn defer(note: impl Into<String>) -> HookOutput {
        HookOutput { stdout: String::new(), exit: 0, note: note.into() }
    }
}

/// Marks hook commands hlock owns, so `sync` can replace them and `pin` can ignore them.
pub const HOOK_MARK: &str = "decide hook";

/// The hook command `sync` writes for `use_`.
pub fn hook_command(bin: &str, use_: &str, format: &str, lock: &std::path::Path) -> String {
    let q = |s: &str| if s.chars().any(|c| c.is_whitespace() || "'\"$`\\".contains(c)) { format!("'{}'", s.replace('\'', "'\\''")) } else { s.to_string() };
    format!("{} {HOOK_MARK} {use_} --format {format} --lock {}", q(bin), q(&lock.to_string_lossy()))
}

/// The `{event: [groups]}` hook entries for a lock's hook-wired deciders, in the
/// Claude Code and Codex layout.
pub fn hook_entries(lock: &crate::lockfile::Lockfile, file: &str, format: &str, bin: &str, lock_path: &std::path::Path) -> Map<String, Value> {
    let mut events = Map::new();
    for d in lock.decider.iter().filter(|d| d.via == "hook") {
        for w in d.wiring.iter().filter(|w| w.file == file) {
            let timeout = d.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).div_ceil(1000) + 1;
            let handler = json!({"type": "command", "command": hook_command(bin, &d.use_, format, lock_path), "timeout": timeout});
            let group = if w.event == "Stop" { json!({"hooks": [handler]}) } else { json!({"matcher": "*", "hooks": [handler]}) };
            events.entry(w.event.clone()).or_insert_with(|| json!([])).as_array_mut().unwrap().push(group);
        }
    }
    events
}

/// Remove hlock's own hook handlers from a config document (any `hooks` object laid
/// out as event → groups → handlers), dropping groups and events left empty.
pub fn strip_hooks(doc: &mut Value) -> bool {
    let mut changed = false;
    match doc {
        Value::Object(m) => {
            if let Some(Value::Object(events)) = m.get_mut("hooks") {
                for groups in events.values_mut() {
                    if let Value::Array(gs) = groups {
                        for g in gs.iter_mut() {
                            if let Some(Value::Array(hs)) = g.get_mut("hooks") {
                                let before = hs.len();
                                hs.retain(|h| !h["command"].as_str().is_some_and(|c| c.contains(HOOK_MARK)));
                                changed |= hs.len() != before;
                            }
                        }
                        gs.retain(|g| !g.get("hooks").and_then(Value::as_array).is_some_and(|hs| hs.is_empty()));
                    }
                }
                events.retain(|_, gs| !gs.as_array().is_some_and(|a| a.is_empty()));
                if events.is_empty() && changed {
                    m.remove("hooks");
                }
            }
            for v in m.values_mut() {
                changed |= strip_hooks(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                changed |= strip_hooks(v);
            }
        }
        _ => {}
    }
    changed
}

/// Merge `entries` into `doc` at `pointer` (default `/hooks`), after removing hlock's
/// old handlers.
pub fn merge_hooks(doc: &mut Value, pointer: &str, entries: &Map<String, Value>) {
    strip_hooks(doc);
    if !doc.is_object() {
        *doc = json!({});
    }
    let mut cur = &mut *doc;
    for part in pointer.trim_start_matches('/').split('/').filter(|p| !p.is_empty()) {
        if !cur.get(part).is_some_and(Value::is_object) {
            cur[part] = json!({});
        }
        cur = cur.get_mut(part).unwrap();
    }
    for (event, groups) in entries {
        let slot = &mut cur[event.as_str()];
        if !slot.is_array() {
            *slot = json!([]);
        }
        slot.as_array_mut().unwrap().extend(groups.as_array().cloned().unwrap_or_default());
    }
}

/// The text of the first user message in a JSONL transcript (Claude Code, Codex).
fn first_user_message(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let msg = v.get("message").unwrap_or(&v);
        let role = msg["role"].as_str().or_else(|| v["type"].as_str());
        if role != Some("user") {
            continue;
        }
        let content = &msg["content"];
        let s = match content {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => continue,
        };
        if !s.trim().is_empty() {
            return Some(s);
        }
    }
    None
}

/// Answer one hook event for the locked decider of `use_`: ask it, and print the
/// harness's own answer when it's sure enough, else defer. Never fails: an error,
/// a timeout or a low probability defers.
pub fn hook(use_: &str, format: &str, input: &Value, lock: &crate::lockfile::Lockfile, root: &std::path::Path, var: &dyn Fn(&str) -> Option<String>) -> HookOutput {
    let Some(d) = lock.decider.iter().find(|d| d.use_ == use_) else {
        return HookOutput::defer(format!("no {use_} decider in the lock"));
    };
    let event = input["hook_event_name"].as_str().unwrap_or_default();
    if event == "Stop" && input["stop_hook_active"].as_bool() == Some(true) {
        return HookOutput::defer("already continuing after a stop hook");
    }
    let policy = d.policy.as_ref().and_then(|p| std::fs::read_to_string(root.join(&p.path)).ok());
    let Some(question) = use_question(use_, policy.as_deref()) else {
        return HookOutput::defer(format!("hooks can't make {use_} decisions"));
    };
    let state = match use_ {
        "approve" => json!({"tool": input["tool_name"], "input": input["tool_input"], "cwd": input["cwd"]}),
        _ => {
            let request = input["transcript_path"].as_str().and_then(first_user_message).unwrap_or_default();
            json!({"request": request, "last_message": input["last_assistant_message"]})
        }
    };
    let mut cfg = entry_config(d);
    cfg.timeout_seconds = Some(d.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).div_ceil(1000));
    let decided = match cfg.endpoint(var).and_then(|ep| decide(&ep, &state, std::slice::from_ref(&question))) {
        Ok(x) => x,
        Err(e) => return HookOutput::defer(format!("decider unavailable: {e}")),
    };
    let Some(a) = decided.answers.first() else { return HookOutput::defer("no answer") };
    let who = format!("hlock {use_} decider ({}:{})", d.provider, decided.model);
    let out = |v: Value, note: String| HookOutput { stdout: v.to_string(), exit: 0, note };
    match use_ {
        "approve" => {
            let choice = a.value.as_str().unwrap_or_default().to_string();
            let p = a.probability();
            let need = min_probability("approve", &d.min_probability, &choice);
            if !d.may.contains(&choice) || p < need {
                return HookOutput::defer(format!("{choice} at P={p:.2}: not allowed or below {need}"));
            }
            let reason = format!("{who}: {choice} at P={p:.2}");
            match (event, choice.as_str()) {
                ("PermissionRequest", "ask") => HookOutput::defer(format!("{reason}; the user is asked anyway")),
                ("PermissionRequest", c) => {
                    let mut decision = json!({"behavior": c});
                    if c == "deny" {
                        decision["message"] = json!(reason);
                    }
                    out(json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}), reason)
                }
                ("PreToolUse", "ask") if format == "codex" => HookOutput::defer(format!("{reason}; Codex's PreToolUse can't ask")),
                ("PreToolUse", c) => out(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": c, "permissionDecisionReason": reason}}), reason),
                _ => HookOutput::defer(format!("{event} isn't an approval event")),
            }
        }
        "stop" if event == "Stop" => {
            let p = a.probability();
            let need = min_probability("stop", &d.min_probability, "done");
            if p >= need {
                return HookOutput::defer(format!("done at P={p:.2}"));
            }
            let reason = format!("{who} isn't sure the request is done (P={p:.2}). Check each part of what the user asked for, finish what's missing, and verify it before stopping.");
            out(json!({"decision": "block", "reason": reason}), format!("continue at P(done)={p:.2}"))
        }
        _ => HookOutput::defer(format!("{event} isn't a {use_} event")),
    }
}
