//! The built-in agent: a small tool-calling loop that ships inside hlock, so the meta
//! harness works with no install (`specs/meta-harness.md`).
//!
//! It talks to Anthropic's Messages API, or to any OpenAI-compatible chat-completions
//! endpoint (OpenAI, OpenRouter, Gemini's OpenAI endpoint, and local runtimes such as
//! Ollama, LM Studio, llama.cpp and vLLM). Its tools read, search and edit files in the
//! project and run commands. In an interactive run every command that isn't allowed is
//! put to the user first; a headless run executes only allowlisted commands.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};

/// Commands a headless run may execute without asking: hlock itself and read-only git.
pub const DEFAULT_ALLOW: &[&str] = &["hlock", "git log", "git show", "git diff", "git status"];
pub const DEFAULT_MAX_STEPS: u32 = 60;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
/// The most a tool result sends back to the model, in bytes.
const MAX_RESULT: usize = 24_000;

/// Where each API provider lives, and the variable its key is read from.
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("anthropic", "https://api.anthropic.com/v1", "ANTHROPIC_API_KEY"),
    ("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    ("google", "https://generativelanguage.googleapis.com/v1beta/openai", "GEMINI_API_KEY"),
    ("openrouter", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY"),
];

/// How the built-in agent reaches its model and what it may do.
#[derive(Debug, Clone, Default)]
pub struct AgentConfig {
    /// `anthropic` uses the Messages API; anything else, chat completions.
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub max_steps: u32,
    pub timeout_seconds: u64,
    /// Command prefixes that run without asking (`"git log"` allows `git log -5`).
    pub allow: Vec<String>,
    /// Ask the user about commands that aren't allowed; otherwise refuse them.
    pub interactive: bool,
    pub project: PathBuf,
}

/// The default base URL for an API provider.
pub fn provider_base_url(provider: &str) -> Option<&'static str> {
    PROVIDERS.iter().find(|(p, _, _)| *p == provider).map(|(_, u, _)| *u)
}

/// The variable an API provider's key is read from.
pub fn provider_key_env(provider: &str) -> Option<&'static str> {
    PROVIDERS.iter().find(|(p, _, _)| *p == provider).map(|(_, _, k)| *k)
}

/// What the loop reports as it goes: `text` (the model's words), `tool` (a call),
/// `result` (a one-line summary of a tool's result), `denied`, and `error`. Returning
/// false stops the run (the user pressed Ctrl-C).
pub type Say<'a> = dyn FnMut(&str, &str) -> bool + 'a;
/// Asks the user about an action: returns `y` (yes), `n` (no) or `a` (always, for this run).
pub type Ask<'a> = dyn FnMut(&str) -> String + 'a;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Outcome {
    /// `done` (the model finished), `max-steps`, `stopped`, or `error`.
    pub status: String,
    pub steps: u32,
    /// The model's last words: its summary.
    pub summary: String,
    pub error: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

// --------------------------------------------------------------------------- //
// Conversation
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone)]
struct Call {
    id: String,
    name: String,
    args: Value,
}

#[derive(Debug, Clone)]
enum Msg {
    User(String),
    Assistant { text: String, calls: Vec<Call> },
    Results(Vec<(String, String)>),
}

fn tools() -> Vec<(&'static str, &'static str, Value)> {
    let s = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req});
    vec![
        ("list_dir", "List a directory's entries (directories end with /).", s(json!({"path": {"type": "string", "description": "Directory, relative to the project (default: the project)"}}), &[])),
        ("read_file", "Read a text file, optionally a range of lines.", s(json!({
            "path": {"type": "string"},
            "offset": {"type": "integer", "description": "First line to read, from 1"},
            "limit": {"type": "integer", "description": "How many lines to read"}
        }), &["path"])),
        ("search", "Find lines containing a string in the files under a directory.", s(json!({
            "pattern": {"type": "string", "description": "Text to look for (case-sensitive)"},
            "path": {"type": "string", "description": "Directory or file to search (default: the project)"}
        }), &["pattern"])),
        ("write_file", "Create or overwrite a file inside the project.", s(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"])),
        ("edit_file", "Replace one exact, unique occurrence of a string in a file inside the project.", s(json!({
            "path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}
        }), &["path", "old", "new"])),
        ("run", "Run a shell command in the project directory and return its exit code and output.", s(json!({"command": {"type": "string"}}), &["command"])),
    ]
}

fn system_prompt(cfg: &AgentConfig) -> String {
    format!(
        "You are the built-in agent of hlock (harness-lock), working in the project at {}. \
         You act through tools: read and search files, write and edit files inside the project, and run shell commands. \
         Work step by step, check what you did, and keep going until the job is done. \
         {} When you're finished, reply with a short summary of what you did and what's left, and call no tools.",
        cfg.project.display(),
        if cfg.interactive {
            "Commands that aren't on the allowlist are shown to the user, who approves or refuses each one."
        } else {
            "This run is unattended: only allowlisted commands run, and others are refused; when you need one, say which and why in your summary."
        }
    )
}

// --------------------------------------------------------------------------- //
// Providers
// --------------------------------------------------------------------------- //

struct Reply {
    text: String,
    calls: Vec<Call>,
    input_tokens: u64,
    output_tokens: u64,
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(600)))
        .proxy(ureq::Proxy::try_from_env())
        .build()
        .into()
}

fn post(cfg: &AgentConfig, path: &str, headers: &[(&str, String)], body: &Value) -> Result<Value> {
    let url = format!("{}/{path}", cfg.base_url.trim_end_matches('/'));
    let mut req = http().post(&url).header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let mut resp = req.send(body.to_string()).map_err(|e| Error::Profile(format!("cannot reach {url}: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().map_err(|e| Error::Profile(format!("bad response from {url}: {e}")))?;
    if !(200..300).contains(&status) {
        let detail: String = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(300).collect());
        return Err(Error::Profile(format!("{url} returned {status}: {detail}")));
    }
    serde_json::from_str(&text).map_err(|e| Error::Profile(format!("{url} returned invalid JSON: {e}")))
}

fn ask_anthropic(cfg: &AgentConfig, history: &[Msg]) -> Result<Reply> {
    let mut messages = Vec::new();
    for m in history {
        messages.push(match m {
            Msg::User(t) => json!({"role": "user", "content": t}),
            Msg::Assistant { text, calls } => {
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                blocks.extend(calls.iter().map(|c| json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.args})));
                json!({"role": "assistant", "content": blocks})
            }
            Msg::Results(rs) => json!({"role": "user", "content": rs.iter().map(|(id, out)| json!({"type": "tool_result", "tool_use_id": id, "content": out})).collect::<Vec<_>>()}),
        });
    }
    let body = json!({
        "model": cfg.model,
        "max_tokens": 8192,
        "system": system_prompt(cfg),
        "tools": tools().into_iter().map(|(n, d, s)| json!({"name": n, "description": d, "input_schema": s})).collect::<Vec<_>>(),
        "messages": messages,
    });
    let mut headers = vec![("anthropic-version", "2023-06-01".to_string())];
    if let Some(k) = &cfg.api_key {
        headers.push(("x-api-key", k.clone()));
    }
    let v = post(cfg, "messages", &headers, &body)?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in v["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => text.push_str(b["text"].as_str().unwrap_or_default()),
            Some("tool_use") => calls.push(Call { id: b["id"].as_str().unwrap_or_default().into(), name: b["name"].as_str().unwrap_or_default().into(), args: b["input"].clone() }),
            _ => {}
        }
    }
    Ok(Reply { text, calls, input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0), output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0) })
}

fn ask_openai(cfg: &AgentConfig, history: &[Msg]) -> Result<Reply> {
    let mut messages = vec![json!({"role": "system", "content": system_prompt(cfg)})];
    for m in history {
        match m {
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::Assistant { text, calls } => {
                let mut msg = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    msg["tool_calls"] = calls.iter().map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.args.to_string()}})).collect();
                }
                messages.push(msg);
            }
            Msg::Results(rs) => messages.extend(rs.iter().map(|(id, out)| json!({"role": "tool", "tool_call_id": id, "content": out}))),
        }
    }
    let body = json!({
        "model": cfg.model,
        "messages": messages,
        "tools": tools().into_iter().map(|(n, d, s)| json!({"type": "function", "function": {"name": n, "description": d, "parameters": s}})).collect::<Vec<_>>(),
    });
    let headers: Vec<(&str, String)> = cfg.api_key.iter().map(|k| ("authorization", format!("Bearer {k}"))).collect();
    let v = post(cfg, "chat/completions", &headers, &body)?;
    let msg = &v["choices"][0]["message"];
    let calls = msg["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, c)| {
            let raw = &c["function"]["arguments"];
            let args = match raw {
                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({"_unparsed": s})),
                other => other.clone(),
            };
            Call { id: c["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("call_{i}")), name: c["function"]["name"].as_str().unwrap_or_default().into(), args }
        })
        .collect();
    let mut text = msg["content"].as_str().unwrap_or_default().to_string();
    let mut calls: Vec<Call> = calls;
    if calls.is_empty() {
        if let Some(c) = text_call(&text) {
            calls.push(c);
            text.clear();
        }
    }
    Ok(Reply {
        text,
        calls,
        input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    })
}

/// One completion with no tools, as judge graders use: returns the reply's text and its
/// input and output token counts.
pub fn complete(cfg: &AgentConfig, system: &str, prompt: &str) -> Result<(String, u64, u64)> {
    if cfg.provider == "anthropic" {
        let body = json!({"model": cfg.model, "max_tokens": 2048, "system": system, "messages": [{"role": "user", "content": prompt}]});
        let mut headers = vec![("anthropic-version", "2023-06-01".to_string())];
        if let Some(k) = &cfg.api_key {
            headers.push(("x-api-key", k.clone()));
        }
        let v = post(cfg, "messages", &headers, &body)?;
        let text: String = v["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
        return Ok((text, v["usage"]["input_tokens"].as_u64().unwrap_or(0), v["usage"]["output_tokens"].as_u64().unwrap_or(0)));
    }
    let body = json!({"model": cfg.model, "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}]});
    let headers: Vec<(&str, String)> = cfg.api_key.iter().map(|k| ("authorization", format!("Bearer {k}"))).collect();
    let v = post(cfg, "chat/completions", &headers, &body)?;
    let text = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
    Ok((text, v["usage"]["prompt_tokens"].as_u64().unwrap_or(0), v["usage"]["completion_tokens"].as_u64().unwrap_or(0)))
}

/// A tool call a model wrote as text instead of a structured call, as small local models
/// often do: `{"name": "write_file", "parameters": {...}}`, optionally in a code fence.
fn text_call(text: &str) -> Option<Call> {
    let t = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let v: Value = serde_json::from_str(t).ok()?;
    let name = v.get("name")?.as_str()?;
    if !tools().iter().any(|(n, _, _)| *n == name) {
        return None;
    }
    let args = v.get("parameters").or_else(|| v.get("arguments")).cloned().unwrap_or_else(|| json!({}));
    let args = match args {
        Value::String(s) => serde_json::from_str(&s).ok()?,
        other => other,
    };
    Some(Call { id: "text_call".into(), name: name.into(), args })
}

// --------------------------------------------------------------------------- //
// Tools
// --------------------------------------------------------------------------- //

fn resolve(cfg: &AgentConfig, p: &str) -> PathBuf {
    let p = p.trim();
    if p.is_empty() || p == "." {
        return cfg.project.clone();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        cfg.project.join(path)
    }
}

/// Lexically normalized, so `project/../x` isn't mistaken for a path inside the project.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn inside_project(cfg: &AgentConfig, p: &Path) -> bool {
    normalize(p).starts_with(normalize(&cfg.project))
}

fn truncate(s: String) -> String {
    if s.len() <= MAX_RESULT {
        return s;
    }
    let mut cut = MAX_RESULT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… ({} more bytes not shown)", &s[..cut], s.len() - cut)
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__", ".hlock"];

fn search(dir: &Path, pattern: &str, out: &mut Vec<String>, base: &Path) {
    if out.len() >= 200 {
        return;
    }
    if dir.is_file() {
        if let Ok(text) = std::fs::read_to_string(dir) {
            for (i, line) in text.lines().enumerate().filter(|(_, l)| l.contains(pattern)) {
                let rel = dir.strip_prefix(base).unwrap_or(dir).display();
                out.push(format!("{rel}:{}: {}", i + 1, line.trim_end().chars().take(200).collect::<String>()));
                if out.len() >= 200 {
                    return;
                }
            }
        }
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        search(&e.path(), pattern, out, base);
    }
}

/// Split a command into words, honoring simple quotes; `None` when it uses shell syntax
/// (pipes, redirects, substitution, chaining), which an allowlist can't vouch for.
pub fn command_words(cmd: &str) -> Option<Vec<String>> {
    if cmd.chars().any(|c| matches!(c, ';' | '|' | '&' | '>' | '<' | '$' | '`' | '\n' | '(' | ')')) {
        return None;
    }
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in cmd.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !cur.is_empty() || any {
        words.push(cur);
    }
    Some(words)
}

/// Whether `cmd` is on the allowlist: plain words (no shell syntax) starting with an
/// allowed prefix's words. A program may be named by path (`/usr/bin/git log`).
pub fn allowed(cmd: &str, allow: &[String]) -> bool {
    let Some(words) = command_words(cmd) else { return false };
    let Some(first) = words.first() else { return false };
    let prog = Path::new(first).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    allow.iter().any(|a| {
        let want: Vec<&str> = a.split_whitespace().collect();
        !want.is_empty() && want.len() <= words.len() && want[0] == prog && want[1..].iter().zip(&words[1..]).all(|(w, x)| w == x)
    })
}

/// The prefix "always" approves: the program, plus its subcommand when it has one.
fn always_prefix(cmd: &str) -> Option<String> {
    let words = command_words(cmd)?;
    let prog = Path::new(words.first()?).file_name()?.to_string_lossy().into_owned();
    Some(match words.get(1) {
        Some(sub) if !sub.starts_with('-') && !sub.contains('/') && !sub.contains('.') => format!("{prog} {sub}"),
        _ => prog,
    })
}

fn run_command(cfg: &AgentConfig, cmd: &str) -> String {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    c.current_dir(&cfg.project).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return format!("cannot run it: {e}"),
    };
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let limit = Duration::from_secs(cfg.timeout_seconds.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let out = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    let head = match status {
        Some(s) => format!("exit code {}", s.code().unwrap_or(-1)),
        None => format!("timed out after {}s and was stopped", cfg.timeout_seconds),
    };
    truncate(format!("{head}\n{out}{}{err}", if err.is_empty() || out.ends_with('\n') || out.is_empty() { "" } else { "\n" }))
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Run one tool call. Returns the result for the model and a one-line summary.
fn call_tool(cfg: &AgentConfig, allow: &mut Vec<String>, call: &Call, say: &mut Say, ask: &mut Ask) -> (String, String) {
    let a = &call.args;
    let need = |k: &str| arg(a, k).ok_or_else(|| format!("missing `{k}`"));
    let result: std::result::Result<(String, String), String> = (|| match call.name.as_str() {
        "list_dir" => {
            let dir = resolve(cfg, arg(a, "path").unwrap_or("."));
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .map_err(|e| format!("cannot list {}: {e}", dir.display()))?
                .flatten()
                .map(|e| format!("{}{}", e.file_name().to_string_lossy(), if e.path().is_dir() { "/" } else { "" }))
                .collect();
            names.sort();
            let n = names.len();
            names.truncate(500);
            Ok((names.join("\n"), format!("{n} entries")))
        }
        "read_file" => {
            let path = resolve(cfg, need("path")?);
            let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            // models often send 0 for "no limit"
            let limit = a.get("limit").and_then(Value::as_u64).filter(|n| *n > 0).unwrap_or(2000) as usize;
            let lines: Vec<&str> = text.lines().skip(offset - 1).take(limit).collect();
            Ok((truncate(lines.join("\n")), format!("{} lines", lines.len())))
        }
        "search" => {
            let pattern = need("pattern")?;
            let base = resolve(cfg, arg(a, "path").unwrap_or("."));
            let mut hits = Vec::new();
            search(&base, pattern, &mut hits, &cfg.project);
            let n = hits.len();
            Ok((if hits.is_empty() { "no matches".into() } else { hits.join("\n") }, format!("{n} matches")))
        }
        "write_file" | "edit_file" => {
            let path = resolve(cfg, need("path")?);
            if !inside_project(cfg, &path) {
                return Err(format!("{} is outside the project; write files there with `run`, which the user approves", path.display()));
            }
            let body = if call.name == "write_file" {
                need("content")?.to_string()
            } else {
                let (old, new) = (need("old")?, need("new")?);
                let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                match text.matches(old).count() {
                    1 => text.replacen(old, new, 1),
                    0 => return Err("`old` doesn't appear in the file".into()),
                    n => return Err(format!("`old` appears {n} times; include more context so it's unique")),
                }
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, &body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            let rel = path.strip_prefix(&cfg.project).unwrap_or(&path).display().to_string();
            Ok((format!("wrote {rel} ({} bytes)", body.len()), format!("wrote {rel}")))
        }
        "run" => {
            let cmd = need("command")?.trim().to_string();
            if !allowed(&cmd, allow) {
                if !cfg.interactive {
                    say("denied", &cmd);
                    return Err(format!(
                        "refused: `{cmd}` isn't on the allowlist ({}), and this run is unattended. Do without it, or say in your summary that the user should run it",
                        allow.join(", ")
                    ));
                }
                match ask(&cmd).trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" => {}
                    "a" | "always" => {
                        if let Some(p) = always_prefix(&cmd) {
                            allow.push(p);
                        }
                    }
                    _ => {
                        say("denied", &cmd);
                        return Err(format!("the user refused `{cmd}`; don't run it again, and ask what they'd prefer"));
                    }
                }
            }
            let out = run_command(cfg, &cmd);
            let first = out.lines().next().unwrap_or_default().to_string();
            Ok((out, first))
        }
        other => Err(format!("unknown tool {other:?}")),
    })();
    match result {
        Ok((full, summary)) => (full, summary),
        Err(e) => (format!("error: {e}"), format!("error: {e}")),
    }
}

fn describe(call: &Call) -> String {
    let a = &call.args;
    match call.name.as_str() {
        "run" => format!("$ {}", arg(a, "command").unwrap_or_default()),
        "search" => format!("search {:?} in {}", arg(a, "pattern").unwrap_or_default(), arg(a, "path").unwrap_or(".")),
        "list_dir" => format!("list {}", arg(a, "path").unwrap_or(".")),
        name => format!("{} {}", name.replace('_', " "), arg(a, "path").unwrap_or_default()),
    }
}

// --------------------------------------------------------------------------- //
// The loop
// --------------------------------------------------------------------------- //

/// Run the agent on `prompt` until the model stops calling tools, or `max_steps`.
pub fn run(cfg: &AgentConfig, prompt: &str, say: &mut Say, ask: &mut Ask) -> Outcome {
    let mut history = vec![Msg::User(prompt.to_string())];
    let mut allow = cfg.allow.clone();
    let mut out = Outcome { status: "max-steps".into(), ..Default::default() };
    while out.steps < cfg.max_steps.max(1) {
        out.steps += 1;
        let reply = if cfg.provider == "anthropic" { ask_anthropic(cfg, &history) } else { ask_openai(cfg, &history) };
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                say("error", &msg);
                out.status = "error".into();
                out.error = Some(msg);
                return out;
            }
        };
        out.input_tokens += reply.input_tokens;
        out.output_tokens += reply.output_tokens;
        if !reply.text.trim().is_empty() && !say("text", reply.text.trim()) {
            out.status = "stopped".into();
            return out;
        }
        if reply.calls.is_empty() {
            out.status = "done".into();
            out.summary = reply.text.trim().to_string();
            return out;
        }
        let mut results = Vec::new();
        for c in &reply.calls {
            if !say("tool", &describe(c)) {
                out.status = "stopped".into();
                return out;
            }
            let (full, summary) = call_tool(cfg, &mut allow, c, say, ask);
            say("result", &summary);
            results.push((c.id.clone(), full));
        }
        history.push(Msg::Assistant { text: reply.text, calls: reply.calls });
        history.push(Msg::Results(results));
    }
    say("error", &format!("stopped after {} steps", out.steps));
    out
}

/// Build the config for a lock's built-in agent from its `[[llm]]` and `[policy]`.
pub fn config_from_lock(lock: &crate::lockfile::Lockfile, project: &Path, interactive: bool, env: &BTreeMap<String, String>) -> Result<AgentConfig> {
    let llm = lock.llm.iter().find(|l| l.role == "default").or_else(|| lock.llm.first()).ok_or_else(|| {
        Error::Lockfile("the meta lock has no model; set [model] in harness.meta.toml and run `hlock meta pin`".into())
    })?;
    let base_url = llm
        .endpoint
        .clone()
        .or_else(|| provider_base_url(&llm.provider).map(str::to_string))
        .ok_or_else(|| Error::Lockfile(format!("no endpoint for provider {:?}; set [model] base-url", llm.provider)))?;
    let key_env = llm.params.get("key-env").and_then(Value::as_str).map(str::to_string).or_else(|| provider_key_env(&llm.provider).map(str::to_string));
    let api_key = key_env.as_ref().and_then(|k| env.get(k)).filter(|v| !v.is_empty()).cloned();
    if llm.runtime.is_none() && api_key.is_none() {
        return Err(Error::Profile(format!("{} isn't set; the built-in agent reads its {} API key from it", key_env.unwrap_or_default(), llm.provider)));
    }
    Ok(AgentConfig {
        provider: llm.provider.clone(),
        model: llm.model.clone(),
        base_url,
        api_key,
        max_steps: if lock.policy.max_steps > 0 { lock.policy.max_steps } else { DEFAULT_MAX_STEPS },
        timeout_seconds: lock.policy.tool_timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        allow: if lock.policy.tool_allowlist.is_empty() { DEFAULT_ALLOW.iter().map(|s| s.to_string()).collect() } else { lock.policy.tool_allowlist.clone() },
        interactive,
        project: project.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allowlist_matching() {
        let a = allow(DEFAULT_ALLOW);
        assert!(allowed("hlock meta verify install", &a));
        assert!(allowed("git log -5 --oneline", &a));
        assert!(allowed("/usr/bin/git show HEAD", &a));
        assert!(!allowed("git push", &a));
        assert!(!allowed("git log; rm -rf /", &a));
        assert!(!allowed("hlock sync > out.txt", &a));
        assert!(!allowed("git log $(whoami)", &a));
        assert!(!allowed("rm -rf tasks", &a));
        assert!(allowed("brew install pi", &allow(&["brew install"])));
        assert!(!allowed("brew uninstall pi", &allow(&["brew install"])));
        assert_eq!(command_words("git commit -m 'a b'").unwrap(), ["git", "commit", "-m", "a b"]);
        assert_eq!(always_prefix("npm install -g x").as_deref(), Some("npm install"));
        assert_eq!(always_prefix("ls -la").as_deref(), Some("ls"));
    }

    #[test]
    fn tool_calls_written_as_text() {
        let c = text_call("```json\n{\"name\": \"write_file\", \"parameters\": {\"path\": \"a.md\", \"content\": \"x\"}}\n```").unwrap();
        assert_eq!((c.name.as_str(), c.args["path"].as_str()), ("write_file", Some("a.md")));
        assert!(text_call("{\"name\": \"touch\", \"parameters\": {}}").is_none());
        assert!(text_call("All done: wrote the notes.").is_none());
    }

    #[test]
    fn writes_stay_in_the_project() {
        let cfg = AgentConfig { project: PathBuf::from("/p/proj"), ..Default::default() };
        assert!(inside_project(&cfg, &resolve(&cfg, "evals/pack.toml")));
        assert!(!inside_project(&cfg, &resolve(&cfg, "../other/x")));
        assert!(!inside_project(&cfg, &resolve(&cfg, "/etc/hosts")));
    }
}
