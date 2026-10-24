//! The built-in agent: a small tool-calling loop that ships inside hlock, so the meta
//! harness works with no install (`specs/meta-harness.md`).
//!
//! It talks to Anthropic's Messages API, or to any OpenAI-compatible chat-completions
//! endpoint (OpenAI, OpenRouter, Gemini's OpenAI endpoint, and local runtimes such as
//! Ollama, LM Studio, llama.cpp and vLLM). Its tools read, search and edit files in the
//! project and run commands. In an interactive run every command that isn't allowed, and
//! every file outside the project (or one of hlock's own files it would write), is put to
//! the user first; a headless run executes only allowlisted commands and stays in the project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::fsutil;
use crate::process::{self, Limits};

/// Commands a headless run may execute without asking: hlock's safe commands and read-only git.
pub const DEFAULT_ALLOW: &[&str] = &["@hlock", "git log", "git show", "git diff", "git status"];

/// What `@hlock` in an allowlist stands for: the hlock commands the built-in jobs use that
/// read and check, or write where hlock writes by default (the project, `.hlock/`), and
/// that don't run code from the project (test graders, code harnesses) or install
/// software. A `!` entry refuses a command that starts like it and has all of its `-` flags.
/// A path argument that's absolute or climbs out with `..` isn't covered either.
pub const HLOCK_SAFE: &[&str] = &[
    "hlock check", "hlock show", "hlock tree", "hlock harness list", "hlock harness show",
    "hlock runtime list", "hlock backend status",
    "hlock evals init", "!hlock evals init --skill-dir", "!hlock evals init --force", "hlock evals templates",
    "hlock evals check", "!hlock evals check --run",
    "hlock meta check", "hlock meta show", "hlock meta jobs",
    // only the built-in jobs' checks: a custom job's runs a shell command from
    // harness.meta.toml, a file the agent can edit (hlock runs it after the job anyway)
    "hlock meta verify install", "hlock meta verify bootstrap-evals", "hlock meta verify update", "hlock meta verify environment",
    // the environment job's list; `--write` writes harness.toml, which the job edits anyway
    "hlock env suggest",
    // the install job's last step, without package installs, model downloads, builds, or the user's own config
    "hlock sync", "!hlock sync --install", "!hlock sync --pull", "!hlock sync --build", "!hlock sync --in-place",
    "!hlock sync --prefix", "!hlock sync --skill-dir",
];

/// git options that write files, run a program, or point git at another repository or
/// config. An allowlisted git command with any of them isn't allowlisted.
const GIT_UNSAFE: &[&str] = &["--output", "--ext-diff", "--textconv", "--no-index", "-c", "--config-env", "--exec-path", "--git-dir", "--work-tree"];
pub const DEFAULT_MAX_STEPS: u32 = 60;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
/// The most a tool result sends back to the model, in bytes.
const MAX_RESULT: usize = 24_000;
/// The most of a file `read_file` reads.
const MAX_READ: usize = 8 << 20;
/// `search` skips files bigger than this (model weights, archives, databases).
const MAX_SEARCH_FILE: u64 = 2 << 20;

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
    /// A decision model (`[deciders.default]` in `harness.meta.toml`) that checks the job
    /// is done before the agent stops, and notes a command's risk when the user is asked.
    /// It never allows anything.
    pub decider: Option<crate::decide::DeciderConfig>,
}

/// How many extra turns the decider may give the agent when it isn't sure the job's done.
const MAX_CONTINUATIONS: u32 = 2;

/// P(done) below which the agent gets another turn.
const DONE_BELOW: f64 = 0.5;

/// Ask the decider one yes/no question; `None` when there's none or it can't answer.
fn ask_decider(cfg: &AgentConfig, state: &Value, question: crate::decide::Question) -> Option<(f64, String)> {
    let var = |k: &str| std::env::var(k).ok();
    let ep = cfg.decider.as_ref()?.endpoint(&var).ok()?;
    let d = crate::decide::decide(&ep, state, &[question]).ok()?;
    let a = d.answers.first().filter(|a| !a.refused)?;
    Some((a.probability(), format!("{}:{}", ep.provider, d.model)))
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
        ("list_dir", "List a directory's entries (directories end with /). Outside the project, the user must approve.", s(json!({"path": {"type": "string", "description": "Directory, relative to the project (default: the project)"}}), &[])),
        ("read_file", "Read a text file, optionally a range of lines.", s(json!({
            "path": {"type": "string"},
            "offset": {"type": "integer", "description": "First line to read, from 1"},
            "limit": {"type": "integer", "description": "How many lines to read"}
        }), &["path"])),
        ("search", "Find lines containing a string in the files under a directory.", s(json!({
            "pattern": {"type": "string", "description": "Text to look for (case-sensitive)"},
            "path": {"type": "string", "description": "Directory or file to search (default: the project)"}
        }), &["pattern"])),
        ("write_file", "Create or overwrite a file inside the project. Files outside it, hlock's own files (harness.toml, harness.meta.toml, harness*.lock) and anything under .git/ or .hlock/ need the user's approval.", s(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"])),
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
            "Commands that aren't on the allowlist, files outside the project, and writes to hlock's own files or under .git/ or .hlock/ are shown to the user, who approves or refuses each one."
        } else {
            "This run is unattended: only allowlisted commands run, and others are refused; files outside the project, and writes to hlock's own files or under .git/ or .hlock/, are refused too. When you need one, say which and why in your summary."
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

/// Whether `p` is in the project, after resolving `..` and symlinks (a symlink in the
/// project that points out of it is outside).
fn inside_project(cfg: &AgentConfig, p: &Path) -> bool {
    fsutil::within(&cfg.project, p)
}

/// Why a write to `p` (in the project) needs the user's approval, if it does: hlock's
/// own files say what commands run and what's installed, and `.git/` and `.hlock/` hold
/// hooks, config and run records.
fn protected(cfg: &AgentConfig, p: &Path) -> Option<&'static str> {
    let real = fsutil::resolve(p);
    let rel = real.strip_prefix(fsutil::resolve(&cfg.project)).ok()?;
    if rel.components().any(|c| c.as_os_str() == ".git" || c.as_os_str() == ".hlock") {
        return Some("under .git/ or .hlock/");
    }
    let name = rel.file_name()?.to_string_lossy();
    let own = matches!(name.as_ref(), "harness.toml" | "harness.meta.toml" | "harness.lock") || (name.starts_with("harness.") && name.ends_with(".lock"));
    own.then_some("one of hlock's own files")
}

#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    Write,
}

/// What the user approved during a run: command prefixes, and paths.
struct Grants {
    allow: Vec<String>,
    paths: Vec<(Access, PathBuf)>,
}

/// Ask the user to approve reading or writing `path` (`why` says what's unusual about
/// it). A headless run refuses. "Always" covers the directory for reads, the one file
/// for writes.
fn approve(cfg: &AgentConfig, grants: &mut Grants, access: Access, path: &Path, why: &str, say: &mut Say, ask: &mut Ask) -> std::result::Result<(), String> {
    let real = fsutil::resolve(path);
    if grants.paths.iter().any(|(a, p)| *a == access && (real == *p || (access == Access::Read && real.starts_with(p)))) {
        return Ok(());
    }
    let verb = if access == Access::Read { "read" } else { "write" };
    let what = format!("{verb} {} ({why})", path.display());
    if !cfg.interactive {
        say("denied", &what);
        return Err(format!("refused: {what}, and this run is unattended. Do without it, or say in your summary that the user should do it"));
    }
    match ask(&what).trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        "a" | "always" => {
            let scope = if access == Access::Read && !real.is_dir() { real.parent().map(Path::to_path_buf).unwrap_or(real) } else { real };
            grants.paths.push((access, scope));
            Ok(())
        }
        _ => {
            say("denied", &what);
            Err(format!("the user refused to let you {verb} {}; don't try again, and ask what they'd prefer", path.display()))
        }
    }
}

fn check_read(cfg: &AgentConfig, grants: &mut Grants, path: &Path, say: &mut Say, ask: &mut Ask) -> std::result::Result<(), String> {
    if inside_project(cfg, path) {
        return Ok(());
    }
    approve(cfg, grants, Access::Read, path, "outside the project", say, ask)
}

fn check_write(cfg: &AgentConfig, grants: &mut Grants, path: &Path, say: &mut Say, ask: &mut Ask) -> std::result::Result<(), String> {
    if !inside_project(cfg, path) {
        return approve(cfg, grants, Access::Write, path, "outside the project", say, ask);
    }
    match protected(cfg, path) {
        Some(why) => approve(cfg, grants, Access::Write, path, why, say, ask),
        None => Ok(()),
    }
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

/// Search `dir` (a file or a directory, already resolved) for `pattern`. Symlinks are
/// skipped, never followed, and so are files over [`MAX_SEARCH_FILE`] and binary files
/// (a NUL in the first 8 KB).
fn search(dir: &Path, pattern: &str, out: &mut Vec<String>, base: &Path) {
    if out.len() >= 200 {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(dir) else { return };
    if meta.is_file() {
        if meta.len() > MAX_SEARCH_FILE {
            return;
        }
        let Ok(bytes) = fsutil::read_regular(dir, MAX_SEARCH_FILE as usize) else { return };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate().filter(|(_, l)| l.contains(pattern)) {
            let rel = dir.strip_prefix(base).unwrap_or(dir).display();
            out.push(format!("{rel}:{}: {}", i + 1, line.trim_end().chars().take(200).collect::<String>()));
            if out.len() >= 200 {
                return;
            }
        }
        return;
    }
    if !meta.is_dir() {
        return; // symlinks, FIFOs, sockets, devices
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
/// allowed prefix's words. The program must be a bare name, found on `PATH`: one named by
/// path (`./git log`, `/usr/bin/git log`) could be anything, so it's never allowlisted
/// (the user can still approve it). `@hlock` stands for [`HLOCK_SAFE`], and an entry
/// starting with `!` refuses a command that starts with its words and has all of its
/// flags (`!hlock sync --install`). git with an option in [`GIT_UNSAFE`] isn't allowlisted.
pub fn allowed(cmd: &str, allow: &[String]) -> bool {
    let Some(words) = command_words(cmd) else { return false };
    let Some(first) = words.first() else { return false };
    if first.is_empty() || first.contains(['/', '\\']) {
        return false;
    }
    let has = |w: &str, f: &str| w == f || w.starts_with(&format!("{f}="));
    if first == "git" && words[1..].iter().any(|w| GIT_UNSAFE.iter().any(|f| has(w, f))) {
        return false;
    }
    let starts = |want: &[&str]| !want.is_empty() && want.len() <= words.len() && want.iter().zip(&words).all(|(w, x)| w == x);
    let denies = |entry: &str| {
        let want: Vec<&str> = entry.split_whitespace().collect();
        let n = want.iter().position(|w| w.starts_with('-')).unwrap_or(want.len());
        starts(&want[..n]) && want[n..].iter().all(|f| words[n..].iter().any(|x| x == f || x.starts_with(&format!("{f}="))))
    };
    let allows = |entries: &mut dyn Iterator<Item = &str>| {
        let (deny, ok): (Vec<&str>, Vec<&str>) = entries.partition(|e| e.starts_with('!'));
        !deny.iter().any(|d| denies(&d[1..])) && ok.iter().any(|a| starts(&a.split_whitespace().collect::<Vec<_>>()))
    };
    // `@hlock` covers paths in the project: none absolute, under `~`, or climbing out with `..`
    let leaves_project = words[1..].iter().any(|w| {
        let v = if w.starts_with("--") { w.split_once('=').map_or("", |(_, v)| v) } else { w.as_str() };
        let p = Path::new(v);
        p.is_absolute() || v.starts_with(['~', '/', '\\']) || p.components().any(|c| c == std::path::Component::ParentDir)
    });
    let (deny, ok): (Vec<&String>, Vec<&String>) = allow.iter().partition(|e| e.starts_with('!'));
    !deny.iter().any(|d| denies(&d[1..]))
        && ok.iter().any(|a| match a.as_str() {
            "@hlock" => !leaves_project && allows(&mut HLOCK_SAFE.iter().copied()),
            a => allows(&mut std::iter::once(a)),
        })
}

/// The prefix "always" approves: the program, plus its subcommand when it has one. None
/// for a program named by path, which no allowlist entry matches.
fn always_prefix(cmd: &str) -> Option<String> {
    let words = command_words(cmd)?;
    let prog = words.first().filter(|w| !w.is_empty() && !w.contains(['/', '\\']))?.clone();
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
    c.current_dir(&cfg.project).stdin(Stdio::null());
    // what it leaves running (`server &`) is stopped with it
    let res = match process::run(c, &Limits::timeout(Duration::from_secs(cfg.timeout_seconds.max(1)))) {
        Ok(r) => r,
        Err(e) => return format!("cannot run it: {e}"),
    };
    let (out, err) = (res.stdout_text(), res.stderr_text());
    let head = if res.timed_out { format!("timed out after {}s and was stopped", cfg.timeout_seconds) } else { format!("exit code {}", res.code.unwrap_or(-1)) };
    truncate(format!("{head}\n{out}{}{err}", if err.is_empty() || out.ends_with('\n') || out.is_empty() { "" } else { "\n" }))
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Run one tool call. Returns the result for the model and a one-line summary.
fn call_tool(cfg: &AgentConfig, grants: &mut Grants, call: &Call, say: &mut Say, ask: &mut Ask) -> (String, String) {
    let a = &call.args;
    let need = |k: &str| arg(a, k).ok_or_else(|| format!("missing `{k}`"));
    let result: std::result::Result<(String, String), String> = (|| match call.name.as_str() {
        "list_dir" => {
            let dir = resolve(cfg, arg(a, "path").unwrap_or("."));
            check_read(cfg, grants, &dir, say, ask)?;
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
            check_read(cfg, grants, &path, say, ask)?;
            // through symlinks (it was checked where they lead), but only a regular file, and only its start
            let bytes = fsutil::read_regular(&fsutil::resolve(&path), MAX_READ).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let text = String::from_utf8_lossy(&bytes);
            let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            // models often send 0 for "no limit"
            let limit = a.get("limit").and_then(Value::as_u64).filter(|n| *n > 0).unwrap_or(2000) as usize;
            let lines: Vec<&str> = text.lines().skip(offset - 1).take(limit).collect();
            Ok((truncate(lines.join("\n")), format!("{} lines", lines.len())))
        }
        "search" => {
            let pattern = need("pattern")?;
            let base = resolve(cfg, arg(a, "path").unwrap_or("."));
            check_read(cfg, grants, &base, say, ask)?;
            let mut hits = Vec::new();
            search(&fsutil::resolve(&base), pattern, &mut hits, &fsutil::resolve(&cfg.project));
            let n = hits.len();
            Ok((if hits.is_empty() { "no matches".into() } else { hits.join("\n") }, format!("{n} matches")))
        }
        "write_file" | "edit_file" => {
            let path = resolve(cfg, need("path")?);
            check_write(cfg, grants, &path, say, ask)?;
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
            if !allowed(&cmd, &grants.allow) {
                if !cfg.interactive {
                    say("denied", &cmd);
                    return Err(format!(
                        "refused: `{cmd}` isn't on the allowlist ({}), and this run is unattended. Do without it, or say in your summary that the user should run it",
                        grants.allow.join(", ")
                    ));
                }
                let risk = crate::decide::Question::noul(
                    "risk",
                    "Running this shell command could delete or overwrite data, change things outside the project, or reach the network.",
                );
                if let Some((p, who)) = ask_decider(cfg, &json!({"command": cmd, "project": cfg.project}), risk) {
                    say("note", &format!("risk P={p:.2} that it deletes or overwrites data, or reaches outside the project ({who})"));
                }
                match ask(&cmd).trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" => {}
                    "a" | "always" => {
                        if let Some(p) = always_prefix(&cmd) {
                            grants.allow.push(p);
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
    // absolute, so "inside the project" means something for a relative one
    let cfg = &AgentConfig { project: fsutil::absolute(&cfg.project), ..cfg.clone() };
    let mut history = vec![Msg::User(prompt.to_string())];
    let mut grants = Grants { allow: cfg.allow.clone(), paths: Vec::new() };
    let mut out = Outcome { status: "max-steps".into(), ..Default::default() };
    let mut continued = 0;
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
            if continued < MAX_CONTINUATIONS {
                let done = crate::decide::use_question("stop", None).unwrap();
                if let Some((p, who)) = ask_decider(cfg, &json!({"request": prompt, "last_message": reply.text}), done).filter(|(p, _)| *p < DONE_BELOW) {
                    continued += 1;
                    say("note", &format!("not sure the job is done (P={p:.2}, {who}); one more turn"));
                    history.push(Msg::Assistant { text: reply.text, calls: vec![] });
                    history.push(Msg::User(format!(
                        "hlock's check isn't sure the job is done (P={p:.2}). Look at what's left of the job and finish it, \
                         or reply with why it's done."
                    )));
                    continue;
                }
            }
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
            let (full, summary) = call_tool(cfg, &mut grants, c, say, ask);
            if !say("result", &summary) {
                out.status = "stopped".into();
                return out;
            }
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
        project: fsutil::absolute(project),
        decider: None,
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
        assert!(allowed("hlock evals check ./evals", &a));
        assert!(allowed("hlock sync --yes", &a));
        // commands that run project code or install software aren't in @hlock
        for cmd in ["hlock evals check ./evals --run", "hlock evals grade t", "hlock resolve", "hlock run hi",
                    "hlock sync --yes --install", "hlock sync --docker --build", "hlock sync --yes --prefix=/tmp/x",
                    "hlock evals init --skill-dir /home/u/.claude/skills/x",
                    "hlock -m other.toml evals check", "hlock"] {
            assert!(!allowed(cmd, &a), "{cmd}");
        }
        // plain "hlock" still allows everything, and a ! entry takes something back
        assert!(allowed("hlock resolve", &allow(&["hlock"])));
        assert!(!allowed("hlock resolve --backend local", &allow(&["hlock", "!hlock resolve"])));
        assert!(!allowed("npm install --global=true x", &allow(&["npm install", "!npm install --global"])));
        assert!(allowed("npm install x", &allow(&["npm install", "!npm install --global"])));
        assert!(allowed("git log -5 --oneline", &a));
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
        assert_eq!(always_prefix("./hlock check"), None);
    }

    #[test]
    fn programs_named_by_path_are_never_allowlisted() {
        let a = allow(DEFAULT_ALLOW);
        for cmd in ["./git log", "/usr/bin/git show HEAD", "./hlock check", "bin/hlock show", ".\\hlock check"] {
            assert!(!allowed(cmd, &a), "{cmd}");
        }
        assert!(!allowed("./build.sh", &allow(&["build.sh"])));
    }

    #[test]
    fn read_only_git_stays_read_only() {
        let a = allow(DEFAULT_ALLOW);
        for cmd in ["git log --output=/tmp/x", "git log --output /tmp/x", "git diff --ext-diff", "git show --textconv HEAD:a",
                    "git diff --no-index /etc/passwd x", "git log -c", "git diff --git-dir=/x", "git status --work-tree=/x"] {
            assert!(!allowed(cmd, &a), "{cmd}");
        }
        assert!(allowed("git diff --stat", &a) && allowed("git show HEAD:README.md", &a));
        // even an explicit "git" entry doesn't cover them unattended
        assert!(!allowed("git log --output=x", &allow(&["git"])));
    }

    #[test]
    fn hlock_safe_stays_in_the_project() {
        let a = allow(DEFAULT_ALLOW);
        // a custom job's check runs a shell command from harness.meta.toml, which the agent can write
        assert!(!allowed("hlock meta verify notes", &a));
        for job in ["install", "update", "bootstrap-evals --out evals", "environment"] {
            assert!(allowed(&format!("hlock meta verify {job}"), &a), "{job}");
        }
        for cmd in ["hlock sync --skill-dir x", "hlock evals init --force", "hlock evals init evals --force",
                    "hlock evals init /tmp/pack", "hlock evals init ../pack", "hlock evals init ~/pack",
                    "hlock meta verify bootstrap-evals --out=/tmp/x", "hlock evals check a/../../b"] {
            assert!(!allowed(cmd, &a), "{cmd}");
        }
        assert!(allowed("hlock evals init ./evals --template qa", &a));
        // plain "hlock" still allows everything
        assert!(allowed("hlock evals init /tmp/pack", &allow(&["hlock"])));
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        fsutil::resolve(&d)
    }

    fn call(name: &str, args: Value) -> Call {
        Call { id: "1".into(), name: name.into(), args }
    }

    /// Run one tool call; `answer` is what the user says when asked. Returns the result
    /// and what was asked.
    fn tool(cfg: &AgentConfig, c: Call, answer: &str) -> (String, Vec<String>) {
        let mut grants = Grants { allow: cfg.allow.clone(), paths: vec![] };
        let mut asked = Vec::new();
        let (full, _) = call_tool(cfg, &mut grants, &c, &mut |_, _| true, &mut |q| {
            asked.push(q.to_string());
            answer.to_string()
        });
        (full, asked)
    }

    #[test]
    fn hlocks_own_files_need_approval() {
        let d = tmp("own");
        let headless = AgentConfig { project: d.clone(), ..Default::default() };
        for p in ["harness.lock", "harness.meta.toml", "harness.toml", "harness.dev.lock", ".git/hooks/pre-commit", ".hlock/x", "sub/harness.lock"] {
            let (out, _) = tool(&headless, call("write_file", json!({"path": p, "content": "x"})), "y");
            assert!(out.starts_with("error: refused"), "{p}: {out}");
            assert!(!d.join(p).exists(), "{p}");
        }
        let (out, _) = tool(&headless, call("write_file", json!({"path": "notes/a.md", "content": "x"})), "y");
        assert!(out.starts_with("wrote"), "{out}");
        // interactive: the user is asked, and decides
        let interactive = AgentConfig { interactive: true, ..headless.clone() };
        let (out, asked) = tool(&interactive, call("write_file", json!({"path": "harness.lock", "content": "x"})), "n");
        assert!(out.contains("refused") && asked.len() == 1 && asked[0].starts_with("write "), "{out} {asked:?}");
        let (out, _) = tool(&interactive, call("write_file", json!({"path": "harness.lock", "content": "x"})), "y");
        assert!(out.starts_with("wrote"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_outside_the_project_need_approval() {
        let d = tmp("reads");
        std::fs::create_dir_all(d.join("proj")).unwrap();
        std::fs::write(d.join("secret.txt"), "s3cret").unwrap();
        std::fs::write(d.join("proj/a.txt"), "one\ntwo\nthree\n").unwrap();
        let cfg = AgentConfig { project: d.join("proj"), ..Default::default() };
        for c in [call("read_file", json!({"path": "../secret.txt"})), call("list_dir", json!({"path": ".."})), call("search", json!({"pattern": "s3", "path": ".."}))] {
            let (out, _) = tool(&cfg, c, "y");
            assert!(out.starts_with("error: refused") && !out.contains("s3cret"), "{out}");
        }
        #[cfg(unix)]
        {
            // a symlink in the project that leads out of it is outside
            std::os::unix::fs::symlink(d.join("secret.txt"), d.join("proj/link")).unwrap();
            assert!(tool(&cfg, call("read_file", json!({"path": "link"})), "y").0.starts_with("error: refused"));
        }
        let (out, _) = tool(&cfg, call("read_file", json!({"path": "a.txt", "offset": 2, "limit": 1})), "y");
        assert_eq!(out, "two");
        let interactive = AgentConfig { interactive: true, ..cfg.clone() };
        let (out, asked) = tool(&interactive, call("read_file", json!({"path": "../secret.txt"})), "y");
        assert_eq!((out.as_str(), asked.len()), ("s3cret", 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn search_skips_big_and_binary_files_and_symlinked_dirs() {
        let d = tmp("search");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.txt"), "needle\n").unwrap();
        std::fs::write(d.join("big.bin"), format!("needle\n{}", "x".repeat(MAX_SEARCH_FILE as usize))).unwrap();
        std::fs::write(d.join("nul.bin"), b"needle\n\0").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&d, d.join("src/loop")).unwrap();
        let mut hits = Vec::new();
        search(&d, "needle", &mut hits, &d);
        assert_eq!(hits, ["src/a.txt:1: needle"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_relative_project_is_made_absolute() {
        let cfg = AgentConfig { project: PathBuf::from("."), ..Default::default() };
        assert!(!inside_project(&cfg, Path::new("/etc/hosts")));
        let lock = crate::lockfile::Lockfile {
            llm: vec![crate::lockfile::LLM { role: "default".into(), provider: "local".into(), model: "m".into(), endpoint: Some("http://x".into()), runtime: Some("ollama".into()), ..Default::default() }],
            ..Default::default()
        };
        let cfg = config_from_lock(&lock, Path::new("."), false, &BTreeMap::new()).unwrap();
        assert!(cfg.project.is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn commands_are_bounded() {
        let d = tmp("cmd");
        let cfg = AgentConfig { project: d.clone(), timeout_seconds: 1, ..Default::default() };
        let out = run_command(&cfg, "head -c 300000 /dev/zero | tr '\\0' x");
        assert!(out.starts_with("exit code 0") && out.contains("more bytes not shown"), "{}", out.chars().take(80).collect::<String>());
        // the timeout is tested in tests/bounded.rs, away from process::stop_all's test
        let _ = std::fs::remove_dir_all(&d);
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
