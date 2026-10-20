//! Decision models ("deciders"): typed questions answered with calibrated
//! probabilities instead of text (`specs/decision-models.md`).
//!
//! Two wire protocols cover the providers: **System One** (`POST /v1/systemone`:
//! TypeSafe's Jev, Kev on Ollama, Ollaya or its own server, Cloudflare's Clef,
//! OpenRouter's passthrough) and **Decisions** (`POST /v1/decisions`: OpenAI's
//! `gpt-6-luna`). [`decide`] takes one request shape and speaks either.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One decider provider: `(provider, protocol, default base URL, base-URL variable, key
/// variable, traits)`. Base URLs end where the API's own paths start (`/v1` included
/// when the provider puts it there); [`Endpoint`] adds `/v1/<protocol>` as needed.
#[derive(Debug, Clone, Copy)]
pub struct Provider(pub &'static str, pub &'static str, pub &'static str, pub &'static str, pub &'static str, pub Traits);

/// What kind of provider it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Traits {
    /// A hosted API that needs a key.
    pub needs_key: bool,
    /// A server the user runs (a local runtime or a self-hosted System One server): no
    /// key is required.
    pub self_hosted: bool,
    /// A local runtime that pulls and serves models (`ollama pull kev:4b`).
    pub runtime: bool,
    /// It serves only decision models, so any of its models is a decider.
    pub decider_only: bool,
}

const HOSTED: Traits = Traits { needs_key: true, self_hosted: false, runtime: false, decider_only: true };
const RUNTIME: Traits = Traits { needs_key: false, self_hosted: true, runtime: true, decider_only: false };

pub const PROVIDERS: &[Provider] = &[
    Provider("typesafe", "systemone", "https://api.typesafe.ai", "TYPESAFE_BASE_URL", "TYPESAFE_API_KEY", HOSTED),
    Provider("openai-decisions", "decisions", "https://api.openai.com/v1", "", "OPENAI_API_KEY", HOSTED),
    // also serves LLMs: only its decision-model families are deciders
    Provider("openrouter", "systemone", "https://openrouter.ai/api/v1", "", "OPENROUTER_API_KEY", Traits { decider_only: false, ..HOSTED }),
    Provider("ollama", "systemone", "http://localhost:11434", "OLLAMA_HOST", "", RUNTIME),
    Provider("ollaya", "systemone", "http://127.0.0.1:11435", "OLLAYA_HOST", "OLLAYA_API_KEY", Traits { decider_only: true, ..RUNTIME }),
    Provider("cloudflare", "systemone", "", "", "CLOUDFLARE_API_TOKEN", HOSTED),
    Provider("system-one", "systemone", "", "SYSTEM_ONE_BASE_URL", "SYSTEM_ONE_API_KEY", Traits { needs_key: false, self_hosted: true, runtime: false, decider_only: true }),
];

impl Provider {
    fn default_url(&self) -> Option<&'static str> {
        Some(self.2).filter(|u| !u.is_empty())
    }
    fn base_var(&self) -> Option<&'static str> {
        Some(self.3).filter(|v| !v.is_empty())
    }
    fn key_var(&self) -> Option<&'static str> {
        Some(self.4).filter(|v| !v.is_empty())
    }
}

/// Whether `url` is on this machine (`localhost`, `127.0.0.1`, `::1`).
pub fn is_loopback(url: &str) -> bool {
    let url = if url.contains("://") { url.to_string() } else { format!("http://{url}") };
    crate::capture::host_port(&url).is_some_and(|(h, _)| matches!(h.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1"))
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// Input price per million tokens, where it's published (output is free everywhere).
const PRICES: &[(&str, f64)] = &[("typesafe", 0.042), ("openai-decisions", 0.10), ("cloudflare", 0.24), ("ollama", 0.0), ("ollaya", 0.0)];

/// Model families that are decision models, for providers that also serve LLMs
/// (`ollama:kev:4b` is a decider, `ollama:qwen3:8b` an LLM).
pub const DECIDER_FAMILIES: &[&str] = &["jev", "kev", "clef", "span-0", "decider", "laya", "winnow", "decima", "arbiter", "nimble", "luna-decisions", "gpt-6-luna"];

/// Known model versions per provider, oldest first, for `hlock upgrade`. Aliases
/// (`jev-latest`) aren't versions.
pub const DECIDER_VERSIONS: &[(&str, &[&str])] = &[("typesafe", &["jev-1.13.0", "jev-1.14.0"]), ("openrouter", &["typesafe/jev-1.13", "typesafe/jev-1.14"])];

/// A newer version of `provider:model`, when hlock knows one.
pub fn newer_version(provider: &str, model: &str) -> Option<String> {
    let (_, versions) = DECIDER_VERSIONS.iter().find(|(p, _)| *p == provider)?;
    let i = versions.iter().position(|v| *v == model)?;
    versions.get(i + 1..)?.last().map(|v| v.to_string())
}

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
    /// The variable the API key is read from (default: the provider's own). An empty
    /// name sends no key.
    pub key_env: Option<String>,
    /// TypeSafe's long-context mode (262k tokens; slower and less accurate).
    pub long_context: bool,
    /// Seconds to wait for an answer.
    pub timeout_seconds: Option<u64>,
}

/// A provider hlock knows, by name.
pub fn provider(name: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.0 == name)
}

/// A stack's decider is `provider:model`, or `provider:model@0.8` when `resolve` is
/// trying a `min-probability`.
pub fn split_threshold(reference: &str) -> (&str, Option<f64>) {
    match reference.rsplit_once('@') {
        // a probability, so a model id with `@<version>` keeps it
        Some((r, p)) if p.parse::<f64>().is_ok_and(|p| (0.0..=1.0).contains(&p)) => (r, p.parse().ok()),
        _ => (reference, None),
    }
}

/// Split `provider:model`.
pub fn split_ref(reference: &str) -> Option<(&str, &str)> {
    reference.split_once(':').filter(|(p, m)| !p.is_empty() && !m.is_empty())
}

/// Whether a provider's API needs a key (self-hosted System One servers and local
/// runtimes don't).
pub fn needs_key(name: &str) -> bool {
    provider(name).is_some_and(|p| p.5.needs_key)
}

/// Whether a `provider:model` reference (with an optional explicit protocol) names a
/// decision model rather than an LLM.
pub fn is_decider(reference: &str, protocol: Option<&str>) -> bool {
    match protocol {
        Some(p) => p == "systemone" || p == "decisions",
        None => match split_ref(reference) {
            Some((p, _)) if provider(p).is_some_and(|p| p.5.decider_only) => true,
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
        let from_env = known.and_then(Provider::base_var).and_then(|v| var(v));
        let base_url = self
            .base_url
            .clone()
            .or(from_env)
            .or_else(|| known.and_then(Provider::default_url).map(str::to_string))
            .ok_or_else(|| format!("hlock doesn't know where {prov:?} is; set the decider's base-url"))?;
        let base_url = if base_url.contains("://") { base_url } else { format!("http://{base_url}") };
        let key_env = self.key_env.clone().or_else(|| known.and_then(Provider::key_var).map(str::to_string));
        let api_key = key_env.as_deref().filter(|k| !k.is_empty()).and_then(|k| var(k)).filter(|v| !v.is_empty());
        let local = known.is_some_and(|p| p.5.self_hosted) || self.base_url.is_some();
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
    /// The decider refused, or its answer had no usable value (`value` is then null).
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

/// A probability from a response: a finite number, clamped to 0–1.
fn prob(v: &Value) -> Option<f64> {
    v.as_f64().filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 1.0))
}

fn num_map(v: &Value) -> BTreeMap<String, f64> {
    match v {
        Value::Object(m) => m.iter().filter_map(|(k, p)| prob(p).map(|p| (k.clone(), p))).collect(),
        // Decisions: [{value|label, probability}]
        Value::Array(a) => a
            .iter()
            .filter_map(|e| {
                let key = e.get("label").or_else(|| e.get("value")).map(|k| k.as_str().map(str::to_string).unwrap_or_else(|| k.to_string()))?;
                Some((key, prob(&e["probability"])?))
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

/// Read one answer from either protocol's shape. A refusal, or an answer without its
/// value (a noul with no probability, a choice with no option, a score with neither
/// level probabilities nor a score), comes back `refused`, with a null value.
fn read_answer(q: &Question, a: &Value) -> Answer {
    let probabilities = num_map(&a["probabilities"]);
    let confidence = prob(&a["confidence"]);
    let value = match q.kind {
        Kind::Noul => a.get("noul").or_else(|| a.get("probability")).and_then(prob).map(|p| json!(p)),
        Kind::Choice => a["choice"].as_str().map(|c| json!(c)),
        Kind::Score => weighted(&probabilities).or_else(|| a["score"].as_f64().filter(|s| s.is_finite())).map(|s| json!(s)),
    };
    let refused = a["type"].as_str() == Some("refusal") || value.is_none();
    let value = if refused { Value::Null } else { value.unwrap_or_default() };
    let probabilities = match (q.kind, value.as_f64()) {
        (Kind::Noul, Some(p)) => BTreeMap::from([("true".to_string(), p), ("false".to_string(), 1.0 - p)]),
        (Kind::Noul, None) => BTreeMap::new(),
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
            // by name; by position only when no answer carries a name
            Value::Array(arr) if arr.iter().any(|a| a.get("name").is_some()) => arr.iter().find(|a| a["name"].as_str() == Some(&q.id)),
            Value::Array(arr) => arr.get(i),
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

/// POST `body`, retrying a busy provider twice. With a `deadline`, every attempt and
/// every wait between them ends by it.
fn post(ep: &Endpoint, body: &Value, deadline: Option<Instant>) -> Result<Value, String> {
    let url = ep.url();
    let mut wait = Duration::from_millis(250);
    for attempt in 0..3 {
        let timeout = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()) {
                Some(left) => left.min(ep.timeout),
                None => return Err(format!("no answer from {url} in time")),
            },
            None => ep.timeout,
        };
        let mut req = agent(timeout).post(&url).header("content-type", "application/json");
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
            if deadline.is_some_and(|d| Instant::now() + wait >= d) {
                return Err(format!("{url} returned {status}, with no time left to retry"));
            }
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
    decide_by(ep, state, questions, None)
}

/// [`decide`], finishing by `deadline` (retries included) or failing.
pub fn decide_by(ep: &Endpoint, state: &Value, questions: &[Question], deadline: Option<Instant>) -> Result<Decided, String> {
    let limit = MAX_QUESTIONS.iter().find(|(p, _)| *p == ep.protocol).map(|(_, n)| *n).unwrap_or(64);
    let mut out = Decided { model: ep.model.clone(), ..Default::default() };
    for chunk in questions.chunks(limit.max(1)) {
        let body = if ep.protocol == "decisions" { decisions_body(ep, state, chunk) } else { systemone_body(ep, state, chunk) };
        let v = post(ep, &body, deadline)?;
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
            // candidates for `resolve` to choose between
            Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|v| v.as_f64().is_some_and(|p| (0.0..=1.0).contains(&p))) => {}
            Some(Value::Number(n)) if n.as_f64().is_some_and(|p| (0.0..=1.0).contains(&p)) => {}
            Some(Value::Object(m)) if use_ == "approve" && m.iter().all(|(k, v)| ["allow", "ask", "deny"].contains(&k.as_str()) && v.as_f64().is_some_and(|p| (0.0..=1.0).contains(&p))) => {}
            Some(_) => return Err(format!("[deciders.{use_}]: `min-probability` is a number from 0 to 1, a list of them for resolve to choose between{}", if use_ == "approve" { ", or a table of allow, ask, deny" } else { "" })),
        }
        Ok(self)
    }

    /// The `min-probability` candidates `resolve` tries: each of a list, else just the
    /// setting (`None`).
    pub fn thresholds(&self) -> Vec<Option<f64>> {
        match &self.min_probability {
            Some(Value::Array(a)) => a.iter().map(Value::as_f64).collect(),
            _ => vec![None],
        }
    }

    pub fn config(&self, model: &str) -> DeciderConfig {
        let (model, _) = split_threshold(model);
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
        // a tool is dropped only when it's clearly not needed
        ("tool", _) => 0.3,
        ("route", _) => 0.6,
        _ => 0.9,
    };
    // a lock's setting outside 0–1 is ignored
    let ok = |p: &f64| (0.0..=1.0).contains(p);
    match setting {
        Value::Number(n) => n.as_f64().filter(ok).unwrap_or(default),
        Value::Object(m) => m.get(answer).and_then(Value::as_f64).filter(ok).unwrap_or(default),
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
    Some(("hook".into(), h.events.iter().map(|e| Wiring { file: h.target(), event: e.clone() }).collect()))
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
    let (reference, threshold) = split_threshold(reference);
    let (prov, model) = split_ref(reference).ok_or_else(|| format!("{reference:?} isn't written provider:model"))?;
    let known = provider(prov);
    let protocol = spec.protocol.clone().or_else(|| known.map(|p| p.1.to_string())).unwrap_or_else(|| "systemone".into());
    // a tried threshold, else the setting (the first of a list, outside `resolve`)
    let min_probability = match (threshold, &spec.min_probability) {
        (Some(p), _) => json!(p),
        (None, Some(Value::Array(a))) => a.first().cloned().unwrap_or(Value::Null),
        (None, m) => m.clone().unwrap_or(Value::Null),
    };
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
        endpoint: spec.base_url.clone().or_else(|| known.and_then(Provider::default_url).map(str::to_string)),
        key_env: spec.key_env.clone().or_else(|| known.and_then(Provider::key_var).map(str::to_string)),
        runtime: known.is_some_and(|p| p.5.runtime).then(|| prov.to_string()),
        resolved_from: Some(reference.into()),
        via: via.into(),
        wiring,
        may: spec.may.clone(),
        min_probability,
        timeout_ms: spec.timeout_ms,
        questions: questions_digest(use_),
        policy,
    })
}

/// A lock entry as a [`DeciderConfig`], to call it.
///
/// A lock may come from someone else (`hlock pull`), so its endpoint and key variable
/// aren't trusted with this machine's keys:
///
/// - the key is always read from the provider's own variable (`TYPESAFE_API_KEY`), never
///   from a `key-env` the lock names;
/// - an endpoint on this machine (`localhost`, `127.0.0.1`, `::1`) or the provider's
///   default is used as written;
/// - any other endpoint is dropped, and the decider goes where this machine says
///   (the provider's base-URL variable, else its default).
///
/// [`entry_config_for`] also trusts what your own `harness.toml` says.
pub fn entry_config(d: &crate::lockfile::Decider) -> DeciderConfig {
    entry_config_for(d, None)
}

/// [`entry_config`], also trusting the lock's endpoint and key variable where they're
/// what `own` (the `[deciders.<use>]` of your own `harness.toml`) sets.
pub fn entry_config_for(d: &crate::lockfile::Decider, own: Option<&DeciderSpec>) -> DeciderConfig {
    let known = provider(&d.provider);
    let mine = |v: Option<&String>, own: Option<&String>| v.is_some() && v == own;
    let trusted = |u: &&String| is_loopback(u) || known.and_then(Provider::default_url).is_some_and(|def| same_url(u, def)) || mine(Some(u), own.and_then(|s| s.base_url.as_ref()));
    let key_env = match d.key_env.as_ref() {
        Some(k) if mine(Some(k), own.and_then(|s| s.key_env.as_ref())) => k.clone(),
        // the provider's own variable, or none at all for a provider hlock doesn't know
        _ => known.and_then(Provider::key_var).unwrap_or_default().to_string(),
    };
    DeciderConfig {
        model: format!("{}:{}", d.provider, d.model),
        protocol: Some(d.protocol.clone()),
        base_url: d.endpoint.as_ref().filter(trusted).cloned(),
        key_env: Some(key_env),
        long_context: false,
        timeout_seconds: d.timeout_ms.map(|ms| ms.div_ceil(1000)),
    }
}

/// The `[deciders.<use_>]` of the `harness.toml` in or above `root`, if there is one.
pub fn own_spec(root: &std::path::Path, use_: &str) -> Option<DeciderSpec> {
    let path = crate::manifest::find_manifest(root)?;
    crate::manifest::Manifest::load(&path).ok()?.deciders().ok()?.remove(use_)
}

/// The variable a locked decider's key is read from, as [`entry_config_for`] reads it.
pub fn entry_key_env(d: &crate::lockfile::Decider, own: Option<&DeciderSpec>) -> Option<String> {
    entry_config_for(d, own).key_env.filter(|k| !k.is_empty())
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
            match policy_text(root, &p.path) {
                Ok(text) if crate::hash::sha256_text(&text) != p.digest => out.push(format!("decider {}: policy {} changed since the lock was written", d.use_, p.path)),
                Ok(_) => {}
                Err(e) => out.push(format!("decider {}: {e}", d.use_)),
            }
        }
    }
    out
}

/// The most of a policy file hlock reads.
const MAX_POLICY: usize = 64 * 1024;

/// A locked policy file's text. Its path comes from the lock, so it must be a relative
/// path inside the project, and a regular file (not a symlink).
fn policy_text(root: &std::path::Path, path: &str) -> Result<String, String> {
    let p = root.join(path);
    if !crate::fsutil::safe_rel(path) || !crate::fsutil::within(root, &p) {
        return Err(format!("policy {path:?} isn't a path inside the project; not read"));
    }
    crate::fsutil::read_regular_text(&p, MAX_POLICY).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("policy {path} is missing"),
        _ => format!("policy {path}: {e}"),
    })
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

const ROUTE: &str = "An AI agent is about to take its next step. Given the state, which of its models should take it?";
const TOOL: &str = "Given the conversation in the state, the agent's next step may need this tool:";

/// The questions hlock asks for `use_`, with the project's `policy` added: one for
/// `approve` and `stop`, one choice over `options` (model roles) for `route`, and one
/// yes/no per tool in `options` for `tool`.
pub fn use_questions(use_: &str, policy: Option<&str>, options: &[String]) -> Vec<Question> {
    let with_policy = |base: String| match policy.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => format!("{base}\n\nThe project's policy:\n{p}"),
        None => base,
    };
    match use_ {
        "route" if !options.is_empty() => {
            let opts: Vec<(&str, String)> = options.iter().map(|o| (o.as_str(), format!("the {o} model"))).collect();
            let refs: Vec<(&str, &str)> = opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
            vec![Question::choice("route", &with_policy(ROUTE.to_string()), &refs)]
        }
        "tool" => options.iter().enumerate().map(|(i, t)| Question::noul(&format!("t{}", i + 1), &with_policy(format!("{TOOL} `{t}`")))).collect(),
        _ => use_question(use_, policy).into_iter().collect(),
    }
}

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
    let text = match use_ {
        "route" => ROUTE.to_string(),
        "tool" => TOOL.to_string(),
        _ => serde_json::to_string(&use_question(use_, None)?).unwrap_or_default(),
    };
    Some(crate::hash::sha256_text(&text))
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
    fn answers_are_matched_by_name_and_checked() {
        let ep = Endpoint { provider: "x".into(), model: "m".into(), protocol: "decisions".into(), base_url: String::new(), api_key: None, long_context: false, timeout: Duration::from_secs(1) };
        let qs = vec![Question::noul("a", "A?"), Question::noul("b", "B?")];
        // named answers are never matched by position: `a` isn't answered here
        let v = json!({"answers": [{"name": "zzz", "probability": 0.9}, {"name": "b", "probability": 0.2}]});
        assert!(read_response(&ep, &qs, &v).unwrap_err().contains("didn't answer \"a\""));
        // with no names at all, position it is
        let (_, a, _) = read_response(&ep, &qs, &json!({"answers": [{"probability": 0.9}, {"probability": 0.2}]})).unwrap();
        assert_eq!((a[0].probability(), a[1].probability()), (0.9, 0.2));
        // out of range, not a number, missing, refused
        let q = Question::noul("a", "A?");
        assert_eq!(read_answer(&q, &json!({"noul": 1.7})).probability(), 1.0);
        for bad in [json!({"noul": "0.9"}), json!({}), json!({"type": "refusal", "noul": 0.9})] {
            let a = read_answer(&q, &bad);
            assert!(a.refused && a.value.is_null() && a.probabilities.is_empty(), "{bad}");
        }
        let c = read_answer(&Question::choice("c", "?", &[("x", "")]), &json!({"choice": "x", "probabilities": {"x": f64::MAX, "y": -1}, "confidence": 3}));
        assert_eq!((c.probabilities["x"], c.probabilities["y"], c.confidence), (1.0, 0.0, Some(1.0)));
        assert!(read_answer(&Question::choice("c", "?", &[]), &json!({"choice": 3})).refused);
    }

    #[test]
    fn hook_commands_are_recognized_only_as_written() {
        let cmd = hook_command("/opt/my hlock/bin/hlock", "approve", "claude-code", Some("PreToolUse"), std::path::Path::new("/p/it's/harness.lock"));
        assert!(is_hook_command(&cmd), "{cmd}");
        assert!(is_hook_command(&hook_command_powershell("C:\\h\\hlock.exe", "stop", "copilot", None, std::path::Path::new("C:\\p\\harness.lock"))));
        for not in ["./decide hook approve", "hlock decide hook approve --format x --lock y && rm -rf ~", "hlock decide hook judge --format x --lock y", "echo 'unclosed", "my-decide hook runner"] {
            assert!(!is_hook_command(not), "{not}");
        }
    }

    #[test]
    fn one_provider_table() {
        assert!(needs_key("typesafe") && !needs_key("ollama") && !needs_key("system-one") && !needs_key("nope"));
        assert!(provider("ollama").unwrap().5.runtime && !provider("system-one").unwrap().5.runtime);
        assert!(is_decider("system-one:anything", None) && !is_decider("openrouter:qwen3", None));
        assert!(is_loopback("http://localhost:11434") && is_loopback("127.0.0.1:9") && is_loopback("http://[::1]:8000/v1"));
        assert!(!is_loopback("https://localhost.evil.example") && !is_loopback("http://10.0.0.1"));
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

/// The hook formats `hlock decide hook` answers in: the harnesses' own, and `plugin`,
/// which the plugins hlock writes for Amp, OpenCode and Pi read.
pub const HOOK_FORMATS: &[&str] = &["claude-code", "codex", "gemini", "cursor", "copilot", "plugin"];

/// The harness events `hlock decide hook` answers, per hook format. A lock that wires a
/// decider to anything else gets no hook.
pub const HOOK_EVENTS: &[(&str, &[&str])] = &[
    ("claude-code", &["PreToolUse", "PermissionRequest", "Stop"]),
    ("codex", &["PreToolUse", "PermissionRequest", "Stop"]),
    ("gemini", &["BeforeTool", "AfterAgent", "BeforeToolSelection"]),
    ("cursor", &["preToolUse", "beforeShellExecution"]),
    ("copilot", &["preToolUse"]),
];

/// The hook command `sync` writes for `use_` on `event`, for `sh`. Every word is quoted.
pub fn hook_command(bin: &str, use_: &str, format: &str, event: Option<&str>, lock: &std::path::Path) -> String {
    use crate::fsutil::shell_quote as q;
    let event = event.map(|e| format!(" --event {}", q(e))).unwrap_or_default();
    format!("{} {HOOK_MARK} {} --format {}{event} --lock {}", q(bin), q(use_), q(format), q(&lock.to_string_lossy()))
}

/// [`hook_command`] for PowerShell (Copilot on Windows): its own quoting, and `&` to
/// call a quoted path.
pub fn hook_command_powershell(bin: &str, use_: &str, format: &str, event: Option<&str>, lock: &std::path::Path) -> String {
    let q = |s: &str| if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:\\".contains(c)) { s.to_string() } else { format!("'{}'", s.replace('\'', "''")) };
    let event = event.map(|e| format!(" --event {}", q(e))).unwrap_or_default();
    format!("& {} {HOOK_MARK} {} --format {}{event} --lock {}", q(bin), q(use_), q(format), q(&lock.to_string_lossy()))
}

/// Why a lock's hook-wired decider can't be written as a hook for `format`, if it can't:
/// its use isn't one the agent has, or its event isn't one the hook answers.
pub fn hook_problem(d: &crate::lockfile::Decider, event: &str, format: &str) -> Option<String> {
    if !AGENT_USES.contains(&d.use_.as_str()) {
        return Some(format!("decider {:?}: not a use hlock answers in a hook ({})", d.use_, AGENT_USES.join(", ")));
    }
    let known = HOOK_EVENTS.iter().find(|(f, _)| *f == format).map(|(_, e)| *e);
    match known {
        Some(events) if !events.contains(&event) => Some(format!("decider {}: {event:?} isn't a {format} hook event hlock answers ({})", d.use_, events.join(", "))),
        // plugin formats name no event in a command, but the name still has to be plain
        None if !crate::fsutil::plain_token(event) => Some(format!("decider {}: {event:?} isn't an event name", d.use_)),
        _ => None,
    }
}

/// What `hook_entries` and `owned_hook_file` leave out of `file` for a lock, and why.
pub fn hook_problems(lock: &crate::lockfile::Lockfile, file: &str, format: &str) -> Vec<String> {
    lock.decider.iter().filter(|d| d.via == "hook").flat_map(|d| d.wiring.iter().filter(|w| w.file == file).filter_map(move |w| hook_problem(d, &w.event, format))).collect()
}

/// The `{event: [entries]}` hooks for a lock's hook-wired deciders in `file`, laid out
/// the way `format`'s harness reads them. Wiring with a [`hook_problem`] is left out.
pub fn hook_entries(lock: &crate::lockfile::Lockfile, file: &str, format: &str, bin: &str, lock_path: &std::path::Path) -> Map<String, Value> {
    let mut events = Map::new();
    for d in lock.decider.iter().filter(|d| d.via == "hook") {
        for w in d.wiring.iter().filter(|w| w.file == file && hook_problem(d, &w.event, format).is_none()) {
            let ms = d.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
            let secs = ms.div_ceil(1000) + 1;
            let cmd = hook_command(bin, &d.use_, format, Some(&w.event), lock_path);
            let entry = match format {
                // Gemini CLI: a regex matcher for tool events, none for the others; ms
                "gemini" => {
                    let handler = json!({"type": "command", "name": format!("hlock {}", d.use_), "command": cmd, "timeout": ms + 1000});
                    if w.event == "BeforeTool" { json!({"matcher": ".*", "hooks": [handler]}) } else { json!({"hooks": [handler]}) }
                }
                "cursor" => json!({"command": cmd, "timeout": secs}),
                "copilot" => {
                    let ps = hook_command_powershell(bin, &d.use_, format, Some(&w.event), lock_path);
                    json!({"type": "command", "bash": cmd, "powershell": ps, "timeoutSec": secs})
                }
                // Claude Code and Codex
                _ => {
                    let handler = json!({"type": "command", "command": cmd, "timeout": secs});
                    if w.event == "Stop" { json!({"hooks": [handler]}) } else { json!({"matcher": "*", "hooks": [handler]}) }
                }
            };
            events.entry(w.event.clone()).or_insert_with(|| json!([])).as_array_mut().unwrap().push(entry);
        }
    }
    events
}

/// The words of a command line hlock wrote: plain words, and `'…'` with `'\''` (sh) or
/// `''` (PowerShell) for a quote inside. `None` for an unclosed quote.
fn command_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur: Option<String> = None;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' => words.extend(cur.take()),
            '\'' => {
                let w = cur.get_or_insert_with(String::new);
                loop {
                    match it.next()? {
                        '\'' if it.peek() == Some(&'\'') => {
                            it.next();
                            w.push('\'');
                        }
                        '\'' => break,
                        ch => w.push(ch),
                    }
                }
            }
            '\\' if it.peek() == Some(&'\'') => {
                it.next();
                cur.get_or_insert_with(String::new).push('\'');
            }
            ch => cur.get_or_insert_with(String::new).push(ch),
        }
    }
    words.extend(cur);
    Some(words)
}

/// Whether a hook command is one [`hook_command`] wrote: `<bin> decide hook <use>
/// --format <f> [--event <e>] --lock <path>`, and nothing else. A user's own hook that
/// merely mentions `decide hook` isn't.
fn is_hook_command(cmd: &str) -> bool {
    let Some(mut w) = command_words(cmd) else { return false };
    if w.first().map(String::as_str) == Some("&") {
        w.remove(0);
    }
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    match w.as_slice() {
        [_, "decide", "hook", u, "--format", _, rest @ ..] if AGENT_USES.contains(u) => matches!(rest, ["--lock", _] | ["--event", _, "--lock", _]),
        _ => false,
    }
}

fn is_ours(entry: &Value) -> bool {
    ["command", "bash", "powershell"].iter().any(|k| entry[*k].as_str().is_some_and(is_hook_command))
}

/// Remove hlock's own hook entries from a config document: any `hooks` object laid out
/// as event → groups → handlers (Claude Code, Codex, Gemini CLI) or event → entries
/// (Cursor, Copilot), dropping the groups and events that removing them left empty.
/// Groups and events that were already empty are the user's, and stay.
pub fn strip_hooks(doc: &mut Value) -> bool {
    let mut changed = false;
    match doc {
        Value::Object(m) => {
            if let Some(Value::Object(events)) = m.get_mut("hooks") {
                let mut emptied = Vec::new();
                for (event, entries) in events.iter_mut() {
                    if let Value::Array(es) = entries {
                        let was_empty = es.is_empty();
                        let mut removed = false;
                        es.retain_mut(|g| {
                            if is_ours(g) {
                                removed = true;
                                return false;
                            }
                            match g.get_mut("hooks") {
                                Some(Value::Array(hs)) if !hs.is_empty() => {
                                    let before = hs.len();
                                    hs.retain(|h| !is_ours(h));
                                    removed |= hs.len() != before;
                                    // a group is dropped only when it held nothing but ours
                                    !hs.is_empty()
                                }
                                _ => true,
                            }
                        });
                        changed |= removed;
                        if !was_empty && es.is_empty() {
                            emptied.push(event.clone());
                        }
                    }
                }
                for e in &emptied {
                    events.remove(e);
                }
                if events.is_empty() && !emptied.is_empty() {
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
/// old entries. Cursor's file also needs `"version": 1`.
pub fn merge_hooks(doc: &mut Value, pointer: &str, entries: &Map<String, Value>) {
    strip_hooks(doc);
    if !doc.is_object() {
        *doc = json!({});
    }
    if entries.is_empty() {
        return;
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

/// The whole file hlock owns for a harness whose hooks live in a file of their own
/// (Copilot's `.github/hooks/hlock.json`) or in a plugin (Amp, OpenCode, Pi), or
/// `None` when the lock wires nothing into it.
pub fn owned_hook_file(lock_file: &crate::lockfile::Lockfile, file: &str, format: &str, bin: &str, lock_path: &std::path::Path) -> Option<String> {
    let uses: Vec<String> = lock_file
        .decider
        .iter()
        .filter(|d| d.via == "hook" && d.wiring.iter().any(|w| w.file == file && hook_problem(d, &w.event, format).is_none()))
        .map(|d| d.use_.clone())
        .collect();
    if uses.is_empty() {
        return None;
    }
    let lock = lock_path.to_string_lossy();
    let template = match format {
        "copilot" => {
            let entries = hook_entries(lock_file, file, format, bin, lock_path);
            return Some(serde_json::to_string_pretty(&json!({"version": 1, "hooks": entries})).unwrap_or_default() + "\n");
        }
        "amp" => AMP_PLUGIN,
        "opencode" => OPENCODE_PLUGIN,
        "pi" => PI_EXTENSION,
        _ => return None,
    };
    Some(
        template
            .replace("__BIN__", &serde_json::to_string(bin).unwrap_or_default())
            .replace("__LOCK__", &serde_json::to_string(&lock).unwrap_or_default())
            .replace("__USES__", &serde_json::to_string(&uses).unwrap_or_default()),
    )
}

/// A hook event, whatever harness sent it.
#[derive(Debug, Default)]
struct HookEvent {
    /// `tool` (a call about to run), `stop` (the agent is about to stop), or `tools`
    /// (the tools the next request offers).
    kind: &'static str,
    tool: Value,
    input: Value,
    cwd: Value,
    request: String,
    last_message: String,
    /// The agent is already continuing because of a stop hook.
    continuing: bool,
    tools: Vec<String>,
    messages: Value,
    /// `route`: the model roles to choose between, and the state to choose on.
    options: Vec<String>,
    state: Value,
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str().or_else(|| b.as_str())).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Read a hook's input into a [`HookEvent`]. `event` is the harness event the hook was
/// registered for (the command's `--event`), else the input's `hook_event_name`.
fn read_event(format: &str, event: Option<&str>, input: &Value) -> Option<HookEvent> {
    let name = event.map(str::to_string).or_else(|| input["hook_event_name"].as_str().map(str::to_string)).unwrap_or_default();
    let tool = |tool: &Value, inp: &Value| HookEvent { kind: "tool", tool: tool.clone(), input: inp.clone(), cwd: input["cwd"].clone(), ..Default::default() };
    match (format, name.as_str()) {
        ("plugin", _) => match input["event"].as_str() {
            Some("tool") => Some(tool(&input["tool"], &input["input"])),
            Some("stop") => Some(HookEvent { kind: "stop", request: text_of(&input["request"]), last_message: text_of(&input["last_message"]), ..Default::default() }),
            Some("route") => Some(HookEvent {
                kind: "route",
                options: input["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(str::to_string)).collect(),
                state: input["state"].clone(),
                ..Default::default()
            }),
            Some("tools") => Some(HookEvent {
                kind: "tools",
                tools: input["tools"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(str::to_string)).collect(),
                messages: input["state"].clone(),
                ..Default::default()
            }),
            _ => None,
        },
        ("claude-code" | "codex", "PreToolUse" | "PermissionRequest") | ("gemini", "BeforeTool") | ("cursor", "preToolUse") => {
            Some(tool(&input["tool_name"], &input["tool_input"]))
        }
        ("cursor", "beforeShellExecution") => Some(tool(&json!("Shell"), &json!({"command": input["command"]}))),
        ("copilot", "preToolUse") => {
            let args = input["toolArgs"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| input["toolArgs"].clone());
            Some(tool(&input["toolName"], &args))
        }
        ("claude-code" | "codex", "Stop") => Some(HookEvent {
            kind: "stop",
            request: input["transcript_path"].as_str().and_then(first_user_message).unwrap_or_default(),
            last_message: text_of(&input["last_assistant_message"]),
            continuing: input["stop_hook_active"].as_bool() == Some(true),
            ..Default::default()
        }),
        ("gemini", "AfterAgent") => Some(HookEvent {
            kind: "stop",
            request: text_of(&input["prompt"]),
            last_message: text_of(&input["prompt_response"]),
            continuing: input["stop_hook_active"].as_bool() == Some(true),
            ..Default::default()
        }),
        ("gemini", "BeforeToolSelection") => {
            let req = &input["llm_request"];
            let tools: Vec<String> = req["config"]["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|t| t["functionDeclarations"].as_array().cloned().unwrap_or_default())
                .filter_map(|f| f["name"].as_str().map(str::to_string))
                .collect();
            let messages = req["messages"].as_array().map(|m| json!(m[m.len().saturating_sub(6)..].to_vec())).unwrap_or(Value::Null);
            Some(HookEvent { kind: "tools", tools, messages, ..Default::default() })
        }
        _ => None,
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
        let s = text_of(&msg["content"]);
        if !s.trim().is_empty() {
            return Some(s);
        }
    }
    None
}

/// Answer one hook event for the locked decider of `use_`: ask it, and print the
/// harness's own answer when it's sure enough, else defer. Never fails: an error,
/// a refusal, a timeout or a low probability defers. The whole call, retries included,
/// finishes within the decider's `timeout-ms`, before the harness gives up on the hook.
pub fn hook(use_: &str, format: &str, event: Option<&str>, input: &Value, lock: &crate::lockfile::Lockfile, root: &std::path::Path, var: &dyn Fn(&str) -> Option<String>) -> HookOutput {
    let start = Instant::now();
    let Some(d) = lock.decider.iter().find(|d| d.use_ == use_) else {
        return HookOutput::defer(format!("no {use_} decider in the lock"));
    };
    let Some(ev) = read_event(format, event, input) else {
        return HookOutput::defer(format!("not an event a {use_} decider answers"));
    };
    let wanted = match use_ {
        "approve" => "tool",
        "stop" => "stop",
        "tool" => "tools",
        "route" => "route",
        _ => "",
    };
    if ev.kind != wanted {
        return HookOutput::defer(format!("a {use_} decider doesn't answer {} events", ev.kind));
    }
    if ev.continuing {
        return HookOutput::defer("already continuing after a stop hook");
    }
    if (ev.kind == "tools" && ev.tools.len() < 2) || (ev.kind == "route" && ev.options.len() < 2) {
        return HookOutput::defer("nothing to choose between");
    }
    let policy = match d.policy.as_ref().map(|p| policy_text(root, &p.path)) {
        Some(Err(e)) => return HookOutput::defer(e),
        Some(Ok(t)) => Some(t),
        None => None,
    };
    let options = if ev.kind == "route" { &ev.options } else { &ev.tools };
    let questions = use_questions(use_, policy.as_deref(), options);
    let state = match ev.kind {
        "tool" => json!({"tool": ev.tool, "input": ev.input, "cwd": ev.cwd}),
        "stop" => json!({"request": ev.request, "last_message": ev.last_message}),
        "route" => ev.state.clone(),
        _ => json!({"tools": ev.tools, "recent_messages": ev.messages}),
    };
    let ms = d.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let mut cfg = entry_config_for(d, own_spec(root, use_).as_ref());
    cfg.timeout_seconds = Some(ms.div_ceil(1000));
    let deadline = start + Duration::from_millis(ms);
    let decided = match cfg.endpoint(var).and_then(|ep| decide_by(&ep, &state, &questions, Some(deadline))) {
        Ok(x) => x,
        Err(e) => return HookOutput::defer(format!("decider unavailable: {e}")),
    };
    // one answer for approve, stop and route; a refusal or an answer without its value
    // makes no decision (for `tool`, an unanswered tool is kept)
    let first = match decided.answers.first() {
        Some(a) if a.refused && use_ != "tool" => return HookOutput::defer(format!("the decider refused or gave no answer for {}", a.id)),
        a => a,
    };
    let who = format!("hlock {use_} decider ({}:{})", d.provider, decided.model);
    let out = |v: Value, note: String| HookOutput { stdout: v.to_string(), exit: 0, note };
    match use_ {
        "approve" => {
            let Some(a) = first else { return HookOutput::defer("no answer") };
            let choice = a.value.as_str().unwrap_or_default().to_string();
            let p = a.probability();
            let need = min_probability("approve", &d.min_probability, &choice);
            if !["allow", "ask", "deny"].contains(&choice.as_str()) || !d.may.contains(&choice) || p < need {
                return HookOutput::defer(format!("{choice} at P={p:.2}: not allowed or below {need}"));
            }
            let reason = format!("{who}: {choice} at P={p:.2}");
            match (format, event.or_else(|| input["hook_event_name"].as_str()).unwrap_or_default(), choice.as_str()) {
                ("claude-code" | "codex", "PermissionRequest", "ask") => HookOutput::defer(format!("{reason}; the user is asked anyway")),
                ("claude-code" | "codex", "PermissionRequest", c) => {
                    let mut decision = json!({"behavior": c});
                    if c == "deny" {
                        decision["message"] = json!(reason);
                    }
                    out(json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}), reason)
                }
                ("codex", _, "ask") => HookOutput::defer(format!("{reason}; Codex's PreToolUse can't ask")),
                ("claude-code" | "codex", _, c) => out(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": c, "permissionDecisionReason": reason}}), reason),
                ("gemini", _, "ask") => HookOutput::defer(format!("{reason}; Gemini CLI's BeforeTool can't ask")),
                ("gemini", _, c) => out(json!({"decision": c, "reason": reason}), reason),
                ("cursor", _, c) => out(json!({"permission": c, "user_message": reason, "agent_message": reason}), reason),
                ("copilot", _, "deny") => out(json!({"permissionDecision": "deny", "permissionDecisionReason": reason}), reason),
                ("copilot", _, _) => HookOutput::defer(format!("{reason}; Copilot's preToolUse can only deny")),
                (_, _, c) => out(json!({"answer": c, "reason": reason}), reason),
            }
        }
        "stop" => {
            let Some(a) = first else { return HookOutput::defer("no answer") };
            let p = a.probability();
            let need = min_probability("stop", &d.min_probability, "done");
            if p >= need {
                return HookOutput::defer(format!("done at P={p:.2}"));
            }
            let reason = format!("{who} isn't sure the request is done (P={p:.2}). Check each part of what the user asked for, finish what's missing, and verify it before stopping.");
            let note = format!("continue at P(done)={p:.2}");
            match format {
                "gemini" => out(json!({"decision": "deny", "reason": reason}), note),
                "plugin" => out(json!({"answer": "continue", "reason": reason}), note),
                _ => out(json!({"decision": "block", "reason": reason}), note),
            }
        }
        "route" => {
            let Some(a) = first else { return HookOutput::defer("no answer") };
            let choice = a.value.as_str().unwrap_or_default().to_string();
            if !ev.options.contains(&choice) {
                return HookOutput::defer(format!("{choice:?} isn't one of the options ({})", ev.options.join(", ")));
            }
            let p = a.probability();
            let need = min_probability("route", &d.min_probability, &choice);
            if p < need {
                return HookOutput::defer(format!("{choice} at P={p:.2}: below {need}"));
            }
            let reason = format!("{who}: {choice} at P={p:.2}");
            out(json!({"answer": choice, "reason": reason}), reason)
        }
        _ => {
            // tool: keep the tools the next step may need
            let need = min_probability("tool", &d.min_probability, "keep");
            let keep: Vec<&String> = ev.tools.iter().enumerate().filter(|(i, _)| decided.get(&format!("t{}", i + 1)).is_none_or(|a| a.refused || a.probability() >= need)).map(|(_, t)| t).collect();
            if keep.is_empty() || keep.len() == ev.tools.len() {
                return HookOutput::defer(format!("keeps {} of {} tools", keep.len(), ev.tools.len()));
            }
            let note = format!("offers {} of {} tools", keep.len(), ev.tools.len());
            if format == "plugin" {
                return out(json!({"answer": keep, "reason": format!("{who}: {note}")}), note);
            }
            out(json!({"hookSpecificOutput": {"hookEventName": "BeforeToolSelection", "toolConfig": {"allowedFunctionNames": keep}}}), note)
        }
    }
}

// --------------------------------------------------------------------------- //
// Plugins: for harnesses whose hooks are code (Amp, OpenCode, Pi)
// --------------------------------------------------------------------------- //

/// Amp (`~/.config/amp/plugins/hlock-deciders.ts`, run by Bun): `tool.call` for
/// `approve`, `agent.end` for `stop`.
pub const AMP_PLUGIN: &str = r#"// Written by `hlock sync` for the decision models in harness.lock; hlock rewrites or
// removes it. It asks `hlock decide hook`, which says nothing when it isn't sure.
const BIN = __BIN__, LOCK = __LOCK__, USES = __USES__;

async function ask(amp, use, payload) {
  try {
    const r = await amp.$`${BIN} decide hook ${use} --format plugin --lock ${LOCK} --input ${JSON.stringify(payload)}`;
    const out = String(r?.stdout ?? "").trim();
    return out ? JSON.parse(out) : {};
  } catch {
    return {};
  }
}

function lastText(messages) {
  for (const m of [...(messages ?? [])].reverse()) {
    if (m?.role !== "assistant") continue;
    if (typeof m.content === "string") return m.content;
    if (Array.isArray(m.content)) return m.content.map((b) => b?.text ?? "").join("\n");
  }
  return "";
}

export default function (amp) {
  if (USES.includes("approve")) {
    amp.on("tool.call", async (event) => {
      const d = await ask(amp, "approve", { event: "tool", tool: event?.tool, input: event?.input });
      if (d.answer === "deny") return { action: "reject-and-continue", message: d.reason ?? "refused by hlock" };
      if (d.answer === "allow") return { action: "allow" };
      return undefined;
    });
  }
  if (USES.includes("stop")) {
    amp.on("agent.end", async (event) => {
      if (event?.status !== "done") return undefined;
      const d = await ask(amp, "stop", { event: "stop", request: event?.message ?? "", last_message: lastText(event?.messages) });
      if (d.answer === "continue") return { action: "continue", userMessage: d.reason, maxContinuations: 1 };
      return undefined;
    });
  }
}
"#;

/// OpenCode (`~/.config/opencode/plugins/hlock-deciders.js`, run by Bun):
/// `tool.execute.before` for `approve`, which can only refuse a call.
pub const OPENCODE_PLUGIN: &str = r#"// Written by `hlock sync` for the decision models in harness.lock; hlock rewrites or
// removes it. It asks `hlock decide hook`, which says nothing when it isn't sure.
const BIN = __BIN__, LOCK = __LOCK__, USES = __USES__;

async function ask($, use, payload) {
  try {
    const out = (await $`${BIN} decide hook ${use} --format plugin --lock ${LOCK} --input ${JSON.stringify(payload)}`.quiet().nothrow().text()).trim();
    return out ? JSON.parse(out) : {};
  } catch {
    return {};
  }
}

export const HlockDeciders = async ({ $ }) => ({
  "tool.execute.before": async (input, output) => {
    if (!USES.includes("approve")) return;
    const d = await ask($, "approve", { event: "tool", tool: input?.tool, input: output?.args });
    if (d.answer === "deny") throw new Error(d.reason ?? "refused by hlock");
  },
});
"#;

/// Pi (`~/.pi/agent/extensions/hlock-deciders.ts`, run by Node): `tool_call` for
/// `approve`, which can only refuse a call.
pub const PI_EXTENSION: &str = r#"// Written by `hlock sync` for the decision models in harness.lock; hlock rewrites or
// removes it. It asks `hlock decide hook`, which says nothing when it isn't sure.
import { spawnSync } from "node:child_process";

const BIN = __BIN__, LOCK = __LOCK__, USES = __USES__;

function ask(use, payload) {
  try {
    const r = spawnSync(BIN, ["decide", "hook", use, "--format", "plugin", "--lock", LOCK, "--input", JSON.stringify(payload)], { encoding: "utf8", timeout: 10000 });
    const out = (r.stdout ?? "").trim();
    return out ? JSON.parse(out) : {};
  } catch {
    return {};
  }
}

export default function (pi) {
  if (!USES.includes("approve")) return;
  pi.on("tool_call", async (event) => {
    const d = ask("approve", { event: "tool", tool: event?.toolName ?? event?.tool ?? event?.name, input: event?.input ?? event?.args });
    if (d.answer === "deny") return { block: true, reason: d.reason ?? "refused by hlock" };
    return undefined;
  });
}
"#;
