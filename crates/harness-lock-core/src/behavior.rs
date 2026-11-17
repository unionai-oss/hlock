//! How an agent worked, not only whether it succeeded: tokens, tool calls, the longest
//! run of tool calls without visible text, subagent launches, and two heuristic reads of
//! its last message (asking for approval, stopping early). And the gate that keeps
//! `upgrade` and `tune` from taking a change that holds success but costs much more.
//!
//! A metric is computed only from what a trial's trace shows; one the trace can't
//! support is `None` (unknown), never a made-up 0. The trace formats read:
//!
//! - hlock's `llm.call` events (the reference harness and the Python worker): tokens,
//!   from `usage`.
//! - Claude Code's stream-json or session events (`assistant`, `user`, `result`):
//!   tokens, tool calls, text, and `Task`/`Agent` calls as subagent launches.
//! - Codex's `exec --json` events (`item.completed`, `turn.completed`).
//! - Chat messages (`{"role": "assistant", "content": …, "tool_calls": […]}`), and plain
//!   events: `{"type": "text", "text": …}`, `{"type": "tool.call", "name": …}`,
//!   `{"type": "subagent", …}`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool names that launch a subagent.
const SUBAGENT_TOOLS: &[&str] = &["Task", "Agent", "spawn_agent", "subagent"];

/// Phrases in a last message that ask the user for approval (a heuristic).
const APPROVAL_PHRASES: &[&str] = &[
    "should i proceed", "shall i proceed", "do you want me to proceed", "would you like me to proceed",
    "may i proceed", "can i proceed", "ok to proceed", "okay to proceed", "should i go ahead",
    "shall i go ahead", "want me to go ahead", "please confirm", "can you confirm", "do i have your permission",
    "with your permission", "once you approve", "if you approve",
];

/// Phrases in a last message that stop short: an offer to go on, or work left over (a
/// heuristic).
const EARLY_STOP_PHRASES: &[&str] = &[
    "would you like me to", "do you want me to", "want me to continue", "shall i continue", "should i continue",
    "let me know if you want me to", "let me know if you'd like me to", "let me know if you would like me to",
    "i can continue", "i'll continue in", "remaining work", "remaining steps", "remaining tasks", "still to do",
    "left to do", "not yet implemented", "next steps would be", "in a follow-up",
];

/// One trial's behavior. `None` is unknown: the trace doesn't show it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrialBehavior {
    /// Input tokens, cached ones included.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    /// The most tool calls in a row without any visible text between them.
    pub longest_silent_stretch: Option<u64>,
    pub subagents: Option<u64>,
    /// Heuristic: the last message asks the user for approval.
    pub asks_approval: Option<bool>,
    /// Heuristic: the last message offers to continue, or lists work left to do.
    pub early_stop: Option<bool>,
}

impl TrialBehavior {
    pub fn is_unknown(&self) -> bool {
        *self == TrialBehavior::default()
    }
}

/// What a set of trials did, on average: `[evidence.behavior]` in a lock. A field is
/// absent when no trial's trace showed it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EvidenceBehavior {
    /// Trials whose trace showed at least one of these.
    pub trials: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_per_task: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_per_task: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls_per_task: Option<f64>,
    /// The mean, over trials, of each trial's longest silent stretch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_silent_stretch: Option<f64>,
    /// The longest in any trial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_silent_stretch_max: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents_per_task: Option<f64>,
    /// Heuristic: the share of trials whose last message asks for approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_request_rate: Option<f64>,
    /// Heuristic: the share of trials whose last message offers to continue or lists work left.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub early_stop_rate: Option<f64>,
}

/// Whether `text` asks for approval, and whether it stops early: phrase matches, so a
/// heuristic.
pub fn classify_final(text: &str) -> (bool, bool) {
    let t = text.to_lowercase().replace('’', "'");
    let has = |phrases: &[&str]| phrases.iter().any(|p| t.contains(p));
    (has(APPROVAL_PHRASES), has(EARLY_STOP_PHRASES))
}

/// Walks a trace in order, tracking the main thread's text and tool calls.
#[derive(Default)]
struct Walk {
    input: Option<u64>,
    output: Option<u64>,
    /// Totals a final `result` event reports, which replace the summed ones.
    result_usage: Option<(u64, u64)>,
    tools: Option<u64>,
    run: u64,
    longest: u64,
    subagents: Option<u64>,
    last_text: Option<String>,
    /// Claude Code repeats a message's usage on each of its content events.
    seen_messages: BTreeSet<String>,
}

impl Walk {
    fn usage(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(Value::as_u64);
        let input = [n("input_tokens").or(n("prompt_tokens")), n("cache_creation_input_tokens"), n("cache_read_input_tokens")];
        let output = n("output_tokens").or(n("completion_tokens"));
        if input.iter().any(Option::is_some) {
            *self.input.get_or_insert(0) += input.iter().flatten().sum::<u64>();
        }
        if let Some(o) = output {
            *self.output.get_or_insert(0) += o;
        }
    }

    /// Visible text ends a silent stretch.
    fn text(&mut self, t: &str) {
        if !t.trim().is_empty() {
            self.run = 0;
            self.last_text = Some(t.to_string());
        }
    }

    /// The trace's format records every tool call: none seen is 0, not unknown.
    fn shows_tools(&mut self) {
        self.tools.get_or_insert(0);
    }

    fn tool(&mut self, name: &str) {
        *self.tools.get_or_insert(0) += 1;
        self.run += 1;
        self.longest = self.longest.max(self.run);
        if SUBAGENT_TOOLS.contains(&name) {
            *self.subagents.get_or_insert(0) += 1;
        }
    }

    /// A message's content: a string, or blocks of text and tool calls.
    fn content(&mut self, content: &Value) {
        match content {
            Value::String(s) => self.text(s),
            Value::Array(blocks) => {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text" | "output_text") => self.text(b["text"].as_str().unwrap_or_default()),
                        Some("tool_use" | "toolCall" | "server_tool_use") => self.tool(b["name"].as_str().unwrap_or_default()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn event(&mut self, e: &Value) {
        let kind = e["type"].as_str().unwrap_or_default();
        // a subagent's own messages aren't the main thread's
        if e["isSidechain"].as_bool() == Some(true) || e["parent_tool_use_id"].as_str().is_some() {
            return;
        }
        match kind {
            "llm.call" => {
                self.usage(&e["usage"]);
                if let Some(t) = e["text"].as_str() {
                    self.text(t);
                }
                for c in e["tool_calls"].as_array().into_iter().flatten() {
                    self.tool(c.as_str().or_else(|| c["name"].as_str()).unwrap_or_default());
                }
            }
            // Claude Code: stream-json and session logs
            "assistant" => {
                let msg = &e["message"];
                let id = msg["id"].as_str().map(str::to_string);
                if id.as_ref().is_none_or(|i| self.seen_messages.insert(i.clone())) {
                    self.usage(&msg["usage"]);
                }
                self.shows_tools();
                self.subagents.get_or_insert(0);
                self.content(&msg["content"]);
            }
            "result" => {
                let u = &e["usage"];
                if let (Some(i), Some(o)) = (u["input_tokens"].as_u64(), u["output_tokens"].as_u64()) {
                    let cached = u["cache_creation_input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    self.result_usage = Some((i + cached, o));
                }
                if let Some(t) = e["result"].as_str().filter(|t| !t.trim().is_empty()) {
                    self.last_text = Some(t.to_string());
                }
            }
            // Codex: exec --json
            "item.completed" => {
                let item = &e["item"];
                self.shows_tools();
                match item["type"].as_str() {
                    Some("agent_message") => self.text(item["text"].as_str().unwrap_or_default()),
                    Some("command_execution" | "file_change" | "mcp_tool_call" | "web_search") => self.tool(item["tool"].as_str().unwrap_or_default()),
                    _ => {}
                }
            }
            "turn.completed" => self.usage(&e["usage"]),
            // plain events
            "text" | "message" | "assistant.text" if e["role"].as_str().is_none_or(|r| r == "assistant") => {
                self.text(e["text"].as_str().or(e["content"].as_str()).unwrap_or_default())
            }
            "tool.call" | "tool_call" | "tool_use" | "tool" => self.tool(e["name"].as_str().or(e["tool"].as_str()).or(e["tool_name"].as_str()).unwrap_or_default()),
            "subagent" | "subagent.start" | "agent.spawn" => {
                *self.subagents.get_or_insert(0) += 1;
            }
            _ if e["role"].as_str() == Some("assistant") => {
                self.shows_tools();
                if let Some(u) = e.get("usage") {
                    self.usage(u);
                }
                self.content(&e["content"]);
                for c in e["tool_calls"].as_array().into_iter().flatten() {
                    self.tool(c["function"]["name"].as_str().or(c["name"].as_str()).unwrap_or_default());
                }
            }
            _ => {}
        }
    }
}

/// One trial's behavior, from its trace: an array of events, or `{"events": […]}`.
pub fn from_trace(trace: &Value) -> TrialBehavior {
    let events = trace.get("events").unwrap_or(trace);
    let mut w = Walk::default();
    for e in events.as_array().into_iter().flatten() {
        w.event(e);
    }
    let (input, output) = match w.result_usage {
        Some((i, o)) => (Some(i), Some(o)),
        None => (w.input, w.output),
    };
    let (approval, early) = match &w.last_text {
        Some(t) => {
            let (a, e) = classify_final(t);
            (Some(a), Some(e))
        }
        None => (None, None),
    };
    TrialBehavior {
        input_tokens: input,
        output_tokens: output,
        tool_calls: w.tools,
        longest_silent_stretch: w.tools.map(|_| w.longest),
        subagents: w.subagents,
        asks_approval: approval,
        early_stop: early,
    }
}

/// The trials' behavior on average; `None` when no trial's trace showed any of it.
pub fn aggregate<'a>(trials: impl IntoIterator<Item = &'a TrialBehavior>) -> Option<EvidenceBehavior> {
    let all: Vec<&TrialBehavior> = trials.into_iter().filter(|b| !b.is_unknown()).collect();
    if all.is_empty() {
        return None;
    }
    let mean = |f: &dyn Fn(&TrialBehavior) -> Option<f64>| {
        let v: Vec<f64> = all.iter().filter_map(|b| f(b)).collect();
        (!v.is_empty()).then(|| round2(v.iter().sum::<f64>() / v.len() as f64))
    };
    let rate = |f: &dyn Fn(&TrialBehavior) -> Option<bool>| {
        let v: Vec<bool> = all.iter().filter_map(|b| f(b)).collect();
        (!v.is_empty()).then(|| round4(v.iter().filter(|x| **x).count() as f64 / v.len() as f64))
    };
    Some(EvidenceBehavior {
        trials: all.len(),
        input_tokens_per_task: mean(&|b| b.input_tokens.map(|n| n as f64)),
        output_tokens_per_task: mean(&|b| b.output_tokens.map(|n| n as f64)),
        tool_calls_per_task: mean(&|b| b.tool_calls.map(|n| n as f64)),
        longest_silent_stretch: mean(&|b| b.longest_silent_stretch.map(|n| n as f64)),
        longest_silent_stretch_max: all.iter().filter_map(|b| b.longest_silent_stretch).max(),
        subagents_per_task: mean(&|b| b.subagents.map(|n| n as f64)),
        approval_request_rate: rate(&|b| b.asks_approval),
        early_stop_rate: rate(&|b| b.early_stop),
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round4(v: f64) -> f64 {
    (v * 1e4).round() / 1e4
}

// --------------------------------------------------------------------------- //
// The cost gate
// --------------------------------------------------------------------------- //

/// The keys of `[gate]` in `harness.toml`.
pub const GATE_KEYS: &[&str] = &["max-cost-increase", "max-token-increase"];

/// How much more a change may cost than what it replaces, as a share (0.2 = 20%):
/// `[gate]` in `harness.toml`, for `upgrade --apply` and the configurations and edits
/// `tune` accepts. Unset, nothing is rejected for cost.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gate {
    /// On mean cost per task; on output tokens per task when the cost isn't known.
    pub max_cost_increase: Option<f64>,
    /// On output tokens per task.
    pub max_token_increase: Option<f64>,
    /// Check output tokens when the cost isn't known (off only for `[tune]`'s implicit default).
    pub tokens_when_cost_unknown: bool,
}

/// One side of a comparison: mean cost and output tokens per task, `None` when unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spend {
    pub cost_usd: Option<f64>,
    pub output_tokens: Option<f64>,
}

impl Spend {
    /// A cost of 0 is a cost no trial reported: unknown.
    pub fn new(cost_usd: f64, output_tokens: Option<f64>) -> Spend {
        Spend { cost_usd: (cost_usd > 0.0).then_some(cost_usd), output_tokens }
    }

    pub fn to_value(self) -> Value {
        serde_json::json!({"cost-usd-per-task": self.cost_usd.map(round6), "output-tokens-per-task": self.output_tokens.map(round2)})
    }
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

impl Gate {
    /// `[gate]`, checked: each a number at least 0.
    pub fn from_manifest(m: &crate::manifest::Manifest) -> crate::error::Result<Gate> {
        let num = |k: &str| -> crate::error::Result<Option<f64>> {
            match m.gate.get(k) {
                None => Ok(None),
                Some(v) => v.as_f64().filter(|x| x.is_finite() && *x >= 0.0).map(Some).ok_or_else(|| crate::error::Error::Manifest(format!("[gate] {k} = {v}: write a share at least 0, like 0.2 for 20%"))),
            }
        };
        Ok(Gate { max_cost_increase: num("max-cost-increase")?, max_token_increase: num("max-token-increase")?, tokens_when_cost_unknown: true })
    }

    pub fn is_set(&self) -> bool {
        self.max_cost_increase.is_some() || self.max_token_increase.is_some()
    }

    /// Why `cand` costs too much more than `base`, or `None` when it's within the gate
    /// (or the gate can't tell: nothing known to compare).
    pub fn check(&self, base: Spend, cand: Spend) -> Option<String> {
        let rose = |b: f64, c: f64, max: f64| b > 0.0 && c > b * (1.0 + max) + 1e-12;
        if let Some(max) = self.max_cost_increase {
            match (base.cost_usd, cand.cost_usd) {
                (Some(b), Some(c)) if rose(b, c, max) => {
                    return Some(format!("cost per task rose from ${b:.4} to ${c:.4} ({}), more than the {:.0}% max-cost-increase allows", signed_pct(c / b - 1.0), max * 100.0));
                }
                (Some(_), Some(_)) => {}
                _ if self.tokens_when_cost_unknown => {
                    if let (Some(b), Some(c)) = (base.output_tokens, cand.output_tokens) {
                        if rose(b, c, max) {
                            return Some(format!("the cost isn't known, and output tokens per task rose from {b:.0} to {c:.0} ({}), more than the {:.0}% max-cost-increase allows", signed_pct(c / b - 1.0), max * 100.0));
                        }
                    }
                }
                _ => {}
            }
        }
        if let (Some(max), Some(b), Some(c)) = (self.max_token_increase, base.output_tokens, cand.output_tokens) {
            if rose(b, c, max) {
                return Some(format!("output tokens per task rose from {b:.0} to {c:.0} ({}), more than the {:.0}% max-token-increase allows", signed_pct(c / b - 1.0), max * 100.0));
            }
        }
        None
    }

    /// What the gate couldn't check, for a note: `None` when it checked what it's set to.
    pub fn unchecked(&self, base: Spend, cand: Spend) -> Option<String> {
        let tokens = base.output_tokens.is_some() && cand.output_tokens.is_some();
        let cost = base.cost_usd.is_some() && cand.cost_usd.is_some();
        if self.max_cost_increase.is_some() && !cost && !(self.tokens_when_cost_unknown && tokens) {
            return Some("[gate] max-cost-increase wasn't checked: neither the cost nor the output tokens are known on both sides".into());
        }
        if self.max_token_increase.is_some() && !tokens {
            return Some("[gate] max-token-increase wasn't checked: the output tokens aren't known on both sides".into());
        }
        None
    }
}

/// `+80%`, `-5%`.
pub fn signed_pct(v: f64) -> String {
    format!("{:+.0}%", v * 100.0)
}

/// `cost/task $0.0040 -> $0.0072 (+80%), output tokens/task 1200 -> 2160 (+80%)`, for a
/// message: what's known of each.
pub fn spend_line(base: Spend, cand: Spend) -> String {
    let one = |what: &str, b: Option<f64>, c: Option<f64>, fmt: &dyn Fn(f64) -> String| match (b, c) {
        (Some(b), Some(c)) if b > 0.0 => format!("{what} {} -> {} ({})", fmt(b), fmt(c), signed_pct(c / b - 1.0)),
        (Some(b), Some(c)) => format!("{what} {} -> {}", fmt(b), fmt(c)),
        (None, Some(c)) => format!("{what} unknown -> {}", fmt(c)),
        (Some(b), None) => format!("{what} {} -> unknown", fmt(b)),
        (None, None) => format!("{what} unknown"),
    };
    format!(
        "{}, {}",
        one("cost/task", base.cost_usd, cand.cost_usd, &|v| format!("${v:.4}")),
        one("output tokens/task", base.output_tokens, cand.output_tokens, &|v| format!("{v:.0}"))
    )
}

/// The cost and token change from `base` to `cand`, for a report: each `{base,
/// candidate, change}`, `null` where unknown.
pub fn delta(base: Spend, cand: Spend) -> Value {
    let one = |b: Option<f64>, c: Option<f64>, r: fn(f64) -> f64| {
        let change = match (b, c) {
            (Some(b), Some(c)) if b > 0.0 => Some(round4(c / b - 1.0)),
            _ => None,
        };
        serde_json::json!({"base": b.map(r), "candidate": c.map(r), "change": change})
    };
    serde_json::json!({
        "cost-usd-per-task": one(base.cost_usd, cand.cost_usd, round6),
        "output-tokens-per-task": one(base.output_tokens, cand.output_tokens, round2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hlock_llm_calls_give_tokens_and_nothing_else() {
        let t = json!([
            {"type": "llm.call", "usage": {"input_tokens": 200, "output_tokens": 50}},
            {"type": "llm.call", "usage": {"input_tokens": 200, "output_tokens": 50}},
        ]);
        let b = from_trace(&t);
        assert_eq!((b.input_tokens, b.output_tokens), (Some(400), Some(100)));
        // the trace doesn't show tools or the last message: unknown, not 0
        assert_eq!((b.tool_calls, b.longest_silent_stretch, b.subagents, b.asks_approval, b.early_stop), (None, None, None, None, None));
        assert!(from_trace(&json!([])).is_unknown());
        assert!(from_trace(&json!({"events": [{"type": "harness.exit", "code": 0}]})).is_unknown());
    }

    #[test]
    fn claude_code_stream_json() {
        let t = json!({"events": [
            {"type": "system", "subtype": "init"},
            {"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "Looking."}, {"type": "tool_use", "name": "Read"}], "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 5}}},
            // the same message's next block repeats its usage
            {"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "name": "Grep"}], "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 5}}},
            {"type": "user", "message": {"content": [{"type": "tool_result"}]}},
            {"type": "assistant", "message": {"id": "m2", "content": [{"type": "tool_use", "name": "Task"}, {"type": "tool_use", "name": "Edit"}], "usage": {"input_tokens": 20, "output_tokens": 7}}},
            // a subagent's messages aren't the main thread's
            {"type": "assistant", "parent_tool_use_id": "toolu_1", "message": {"id": "s1", "content": [{"type": "tool_use", "name": "Bash"}]}},
            {"type": "assistant", "message": {"id": "m3", "content": [{"type": "text", "text": "Done. Would you like me to continue with the tests?"}], "usage": {"input_tokens": 30, "output_tokens": 9}}},
        ]});
        let b = from_trace(&t);
        assert_eq!((b.input_tokens, b.output_tokens), (Some(150), Some(21)));
        assert_eq!((b.tool_calls, b.longest_silent_stretch, b.subagents), (Some(4), Some(4), Some(1)));
        assert_eq!((b.asks_approval, b.early_stop), (Some(false), Some(true)));

        // a result event's totals win, and its text is the last message
        let mut ev = t["events"].as_array().unwrap().clone();
        ev.push(json!({"type": "result", "result": "Should I proceed with the migration?", "usage": {"input_tokens": 1000, "output_tokens": 300}}));
        let b = from_trace(&json!(ev));
        assert_eq!((b.input_tokens, b.output_tokens), (Some(1000), Some(300)));
        assert_eq!((b.asks_approval, b.early_stop), (Some(true), Some(false)));

        // no Task call: 0 subagents, known
        let b = from_trace(&json!([{"type": "assistant", "message": {"content": [{"type": "text", "text": "All done."}]}}]));
        assert_eq!((b.subagents, b.tool_calls, b.longest_silent_stretch, b.early_stop), (Some(0), Some(0), Some(0), Some(false)));
    }

    #[test]
    fn codex_chat_and_plain_events() {
        let codex = json!([
            {"type": "item.completed", "item": {"type": "command_execution"}},
            {"type": "item.completed", "item": {"type": "command_execution"}},
            {"type": "item.completed", "item": {"type": "agent_message", "text": "Fixed it."}},
            {"type": "item.completed", "item": {"type": "file_change"}},
            {"type": "turn.completed", "usage": {"input_tokens": 500, "cached_input_tokens": 100, "output_tokens": 40}},
        ]);
        let b = from_trace(&codex);
        assert_eq!((b.tool_calls, b.longest_silent_stretch, b.output_tokens, b.subagents), (Some(3), Some(2), Some(40), None));

        let chat = json!([{"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "run"}}]}, {"role": "assistant", "content": "ok"}]);
        assert_eq!(from_trace(&chat).tool_calls, Some(1));

        let plain = json!([{"type": "tool.call", "name": "a"}, {"type": "tool.call", "name": "b"}, {"type": "subagent"}, {"type": "text", "text": "Let me know if you want me to finish the rest."}]);
        let b = from_trace(&plain);
        assert_eq!((b.tool_calls, b.longest_silent_stretch, b.subagents, b.early_stop), (Some(2), Some(2), Some(1), Some(true)));
        // text alone doesn't say there were no tool calls
        let b = from_trace(&json!([{"type": "text", "text": "Done."}]));
        assert_eq!((b.tool_calls, b.longest_silent_stretch, b.early_stop), (None, None, Some(false)));
    }

    #[test]
    fn aggregates_skip_unknowns() {
        let a = TrialBehavior { output_tokens: Some(100), tool_calls: Some(4), longest_silent_stretch: Some(4), early_stop: Some(true), ..Default::default() };
        let b = TrialBehavior { output_tokens: Some(200), early_stop: Some(false), ..Default::default() };
        let agg = aggregate([&a, &b, &TrialBehavior::default()]).unwrap();
        assert_eq!(agg.trials, 2);
        assert_eq!(agg.output_tokens_per_task, Some(150.0));
        assert_eq!(agg.tool_calls_per_task, Some(4.0));
        assert_eq!(agg.longest_silent_stretch_max, Some(4));
        assert_eq!(agg.early_stop_rate, Some(0.5));
        assert_eq!((agg.input_tokens_per_task, agg.subagents_per_task, agg.approval_request_rate), (None, None, None));
        assert!(aggregate([&TrialBehavior::default()]).is_none());
        // in a lock, unknowns are left out
        let toml = toml::to_string(&agg).unwrap();
        assert!(toml.contains("output-tokens-per-task = 150.0") && !toml.contains("subagents"), "{toml}");
    }

    #[test]
    fn the_gate_rejects_a_change_that_costs_too_much_more() {
        let gate = Gate { max_cost_increase: Some(0.2), max_token_increase: None, tokens_when_cost_unknown: true };
        let base = Spend { cost_usd: Some(0.01), output_tokens: Some(1000.0) };
        assert!(gate.check(base, Spend { cost_usd: Some(0.0119), output_tokens: Some(5000.0) }).is_none());
        let why = gate.check(base, Spend { cost_usd: Some(0.018), output_tokens: Some(1000.0) }).unwrap();
        assert!(why.contains("$0.0100 to $0.0180 (+80%)") && why.contains("20% max-cost-increase"), "{why}");
        // the cost unknown: output tokens stand in
        let unknown = Spend { cost_usd: None, output_tokens: Some(1000.0) };
        let why = gate.check(unknown, Spend { cost_usd: None, output_tokens: Some(1800.0) }).unwrap();
        assert!(why.contains("the cost isn't known") && why.contains("1000 to 1800 (+80%)"), "{why}");
        assert!(Gate { tokens_when_cost_unknown: false, ..gate.clone() }.check(unknown, Spend { cost_usd: None, output_tokens: Some(1800.0) }).is_none());
        // nothing known: not rejected, but noted
        assert!(gate.check(Spend::default(), Spend::default()).is_none());
        assert!(gate.unchecked(Spend::default(), Spend::default()).unwrap().contains("wasn't checked"));
        assert!(gate.unchecked(base, base).is_none());

        let tokens = Gate { max_token_increase: Some(0.5), ..Default::default() };
        assert!(tokens.check(base, Spend { cost_usd: Some(0.01), output_tokens: Some(1500.0) }).is_none());
        assert!(tokens.check(base, Spend { cost_usd: Some(0.01), output_tokens: Some(1501.0) }).unwrap().contains("max-token-increase"));
        assert!(!Gate::default().is_set() && Gate::default().check(base, Spend { cost_usd: Some(1.0), output_tokens: Some(1e9) }).is_none());

        let d = delta(base, Spend { cost_usd: Some(0.018), output_tokens: None });
        assert_eq!(d["cost-usd-per-task"]["change"], json!(0.8));
        assert_eq!(d["output-tokens-per-task"]["change"], json!(null));
        assert_eq!(spend_line(base, Spend { cost_usd: Some(0.018), output_tokens: None }), "cost/task $0.0100 -> $0.0180 (+80%), output tokens/task 1000 -> unknown");
    }

    #[test]
    fn gate_config_is_checked() {
        let m = crate::manifest::Manifest::from_str("[gate]\nmax-cost-increase = 0.2\n").unwrap();
        assert_eq!(Gate::from_manifest(&m).unwrap().max_cost_increase, Some(0.2));
        let bad = crate::manifest::Manifest::from_str("[gate]\nmax-token-increase = -1\n").unwrap();
        assert!(Gate::from_manifest(&bad).unwrap_err().to_string().contains("[gate] max-token-increase"));
        assert!(!Gate::from_manifest(&crate::manifest::Manifest::default()).unwrap().is_set());
    }

    #[test]
    fn final_message_heuristics() {
        assert_eq!(classify_final("I’ve fixed the bug and the tests pass."), (false, false));
        assert_eq!(classify_final("Here's the plan. Should I proceed?"), (true, false));
        assert_eq!(classify_final("Remaining work: wire up the CLI."), (false, true));
        assert_eq!(classify_final("Let me know if you'd like me to add docs."), (false, true));
    }
}
