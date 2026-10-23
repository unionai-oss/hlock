//! `hlock pin`: capture the live setup of one harness into a pinned
//! `harness.lock` (SPEC §7.8.4).
//!
//! Everything here is driven by the harness's [profile](crate::profiles): which
//! files to read, where the model, MCP servers, skills and plugins live, and how
//! they're shaped. Secrets are replaced with `${HLOCK_SECRET_…}` placeholders
//! before anything is written, and content a new machine couldn't fetch (hand
//! written skills, user-level config) is collected for vendoring next to the lock.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::hash::{content_hash, sha256_bytes};
use crate::lockfile::{
    ConfigFile, Context, Decision, Harness, Instructions, Lockfile, Native, Provenance, Runtime, RuntimeModel,
    SecretRef, Skill, Tool, LLM,
};
use crate::profiles::{probe_harness, probe_runtime, Ctx, HarnessProfile, Registry, RuntimeProfile};

/// Where vendored files go, relative to the lock's directory.
pub const VENDOR_DIR: &str = ".hlock/vendor";

/// A file to write under the vendor dir.
#[derive(Debug, Clone)]
pub struct VendorFile {
    /// Path relative to the lock's directory (starts with [`VENDOR_DIR`]).
    pub rel: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// The result of capturing one harness.
#[derive(Debug, Clone)]
pub struct Capture {
    pub lock: Lockfile,
    pub vendor: Vec<VendorFile>,
    pub warnings: Vec<String>,
    /// User-level items left out because the capture was project-only, by kind
    /// (`skills`, `mcp`, `instructions`, `config`, `plugins`, `providers`).
    pub skipped: BTreeMap<String, usize>,
}

// --------------------------------------------------------------------------- //
// Reading config files
// --------------------------------------------------------------------------- //

/// Parse a config file into JSON, whatever its format. `Ok(None)` when it doesn't exist.
pub fn read_config(path: &Path, format: &str) -> Result<Option<Value>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(None) };
    parse_config(&text, format).map(Some).map_err(|e| Error::Profile(format!("cannot parse {}: {e}", path.display())))
}

pub fn parse_config(text: &str, format: &str) -> std::result::Result<Value, String> {
    match format {
        "json" | "jsonc" => {
            if text.trim().is_empty() {
                return Ok(json!({}));
            }
            serde_json::from_str(text).or_else(|_| serde_json::from_str(&strip_jsonc(text))).map_err(|e| e.to_string())
        }
        // JSON5 (OpenClaw): unquoted keys, single quotes, comments. Written back as JSON,
        // which is valid JSON5.
        "json5" => {
            if text.trim().is_empty() {
                return Ok(json!({}));
            }
            serde_json::from_str(text).or_else(|_| serde_json::from_str(&strip_jsonc(&json5_to_jsonc(text)))).map_err(|e| e.to_string())
        }
        "toml" => toml::from_str::<Value>(text).map_err(|e| e.to_string()),
        "yaml" => {
            let v: Value = serde_norway::from_str(text).map_err(|e| e.to_string())?;
            Ok(if v.is_null() { json!({}) } else { v })
        }
        other => Err(format!("unknown config format {other:?}")),
    }
}

/// Rewrite the JSON5 that config files use into JSON with trailing commas: drop
/// comments, double-quote single-quoted strings and unquoted keys, rewrite the escapes
/// and line continuations JSON lacks, and turn hex, `+`-signed and `.5`/`5.` numbers
/// into plain ones. `Infinity` and `NaN` have no JSON form and stay as they are (and
/// fail to parse).
pub fn json5_to_jsonc(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '"' || c == '\'' {
            out.push('"');
            i += 1;
            while i < b.len() && b[i] != c {
                match b[i] {
                    '\\' if b.get(i + 1) == Some(&'\'') => out.push('\''),
                    // a line continuation, `\` then LF, CRLF or CR
                    '\\' if b.get(i + 1) == Some(&'\n') => {}
                    '\\' if b.get(i + 1) == Some(&'\r') => {
                        if b.get(i + 2) == Some(&'\n') {
                            i += 1;
                        }
                    }
                    // escapes JSON lacks
                    '\\' if b.get(i + 1) == Some(&'0') => out.push_str("\\u0000"),
                    '\\' if b.get(i + 1) == Some(&'v') => out.push_str("\\u000b"),
                    '\\' if b.get(i + 1) == Some(&'x') => {
                        out.push_str("\\u00");
                        out.extend(b.iter().skip(i + 2).take(2));
                        i += 2;
                    }
                    '\\' => {
                        out.push('\\');
                        out.extend(b.get(i + 1));
                    }
                    ch => {
                        match ch {
                            '"' => out.push_str("\\\""),
                            // JSON5 allows a raw tab in a string; JSON doesn't
                            '\t' => out.push_str("\\t"),
                            ch => out.push(ch),
                        }
                        i += 1;
                        continue;
                    }
                }
                i += 2;
            }
            out.push('"');
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if (c.is_ascii_digit() && !matches!(b.get(i + 1), Some('x') | Some('X'))) || (c == '.' && b.get(i + 1).is_some_and(char::is_ascii_digit)) {
            // a number, exponent included, so `1e5` isn't read as `1` and a key `e5`;
            // JSON5's `.5` and `5.` become `0.5` and `5.0`
            if c == '.' {
                out.push('0');
            }
            while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], '.' | 'e' | 'E') || (matches!(b[i], '+' | '-') && matches!(b[i - 1], 'e' | 'E'))) {
                out.push(b[i]);
                i += 1;
            }
            if b[i - 1] == '.' {
                out.push('0');
            }
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                i += 1;
            }
            let word: String = b[start..i].iter().collect();
            if matches!(word.as_str(), "true" | "false" | "null" | "Infinity" | "NaN") {
                out.push_str(&word);
            } else {
                out.push('"');
                out.push_str(&word);
                out.push('"');
            }
        } else if c == '0' && matches!(b.get(i + 1), Some('x') | Some('X')) {
            let start = i + 2;
            i = start;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                i += 1;
            }
            let hex: String = b[start..i].iter().collect();
            out.push_str(&u64::from_str_radix(&hex, 16).map(|n| n.to_string()).unwrap_or(hex));
        } else if c == '+' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit() || *d == '.') {
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Remove `//` and `/* */` comments and trailing commas, leaving strings intact.
pub fn strip_jsonc(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut in_str, mut esc) = (0, false, false);
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == ',' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_whitespace() {
                j += 1;
            }
            if !matches!(b.get(j), Some('}') | Some(']')) {
                out.push(c);
            }
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Serialize a JSON value back into a config format (comments aren't preserved).
pub fn render_config(v: &Value, format: &str) -> std::result::Result<String, String> {
    match format {
        "toml" => toml::to_string_pretty(&drop_nulls(v)).map_err(|e| e.to_string()),
        "yaml" => serde_norway::to_string(v).map_err(|e| e.to_string()),
        _ => serde_json::to_string_pretty(v).map(|s| s + "\n").map_err(|e| e.to_string()),
    }
}

pub(crate) fn drop_nulls(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), drop_nulls(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().filter(|v| !v.is_null()).map(drop_nulls).collect()),
        other => other.clone(),
    }
}

pub(crate) fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// Reads profile sources (`env:VAR` or `<file>#<pointer>`), caching parsed files.
struct Sources<'a> {
    profile: &'a HarnessProfile,
    ctx: &'a Ctx,
    cache: RefCell<BTreeMap<String, Option<Value>>>,
    warnings: RefCell<Vec<String>>,
}

impl<'a> Sources<'a> {
    fn new(profile: &'a HarnessProfile, ctx: &'a Ctx) -> Self {
        Sources { profile, ctx, cache: RefCell::new(BTreeMap::new()), warnings: RefCell::new(Vec::new()) }
    }

    fn file(&self, id: &str) -> Option<Value> {
        if let Some(v) = self.cache.borrow().get(id) {
            return v.clone();
        }
        let v = match (self.profile.files.get(id), self.profile.file_path(id, self.ctx)) {
            (Some(spec), Some(path)) => match read_config(&path, &spec.format) {
                Ok(v) => v,
                Err(e) => {
                    self.warnings.borrow_mut().push(e.to_string());
                    None
                }
            },
            _ => None,
        };
        self.cache.borrow_mut().insert(id.to_string(), v.clone());
        v
    }

    fn get(&self, src: &str) -> Option<Value> {
        if let Some(var) = src.strip_prefix("env:") {
            return self.ctx.var(var).map(|v| Value::String(v.to_string()));
        }
        let (file, ptr) = src.split_once('#').unwrap_or((src, ""));
        let ptr = ptr.replace("{cwd}", &escape_pointer(&self.ctx.cwd.to_string_lossy()));
        let v = self.file(file)?;
        v.pointer(&ptr).cloned().filter(|v| !v.is_null())
    }

    /// The first source with a non-empty string value, and which source it was.
    fn first_str(&self, srcs: &[String]) -> Option<(String, String)> {
        srcs.iter().find_map(|s| {
            self.get(s).and_then(|v| v.as_str().map(str::to_string)).filter(|v| !v.is_empty()).map(|v| (v, s.clone()))
        })
    }
}

// --------------------------------------------------------------------------- //
// Secrets
// --------------------------------------------------------------------------- //

const SECRET_WORDS: &[&str] = &[
    "key", "apikey", "token", "secret", "password", "passwd", "pwd", "credential", "credentials", "auth",
    "authorization", "bearer", "cookie", "accesskey", "privatekey", "clientsecret",
];
const SECRET_PREFIXES: &[&str] = &[
    "sk-", "sk_", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "xapp-", "AKIA", "AIza", "hf_", "glpat-",
    "Bearer ", "bearer ", "Basic ", "ya29.", "eyJ",
];

/// Split a key into lowercase words (`apiKey` → `api`, `key`; `x-api-key` → `x`, `api`, `key`).
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = key.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let boundary = c.is_ascii_uppercase() && i > 0 && chars[i - 1].is_ascii_lowercase();
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Whether a config key names a secret.
/// A value local runtimes accept in place of an API key (`"local"`, `"ollama"`, vLLM's
/// `"EMPTY"`): not a secret, so it's kept as is.
pub fn is_placeholder_key(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "local" | "none" | "empty" | "dummy" | "ollama" | "lm-studio" | "lmstudio" | "llama.cpp" | "not-needed" | "no-key" | "sk-no-key-required"
    )
}

pub fn is_secret_key(key: &str) -> bool {
    let words = key_words(key);
    let joined = words.concat();
    words.iter().any(|w| SECRET_WORDS.contains(&w.as_str())) || SECRET_WORDS[1..].contains(&joined.as_str())
}

/// Keys whose whole subtree is credentials (`auth`, `credentials`, `secrets`, `oauth`).
/// Narrower than [`is_secret_key`], so a provider named `qwen-token-plan` isn't swallowed.
fn is_secret_container(key: &str) -> bool {
    let words = key_words(key);
    words.iter().any(|w| matches!(w.as_str(), "auth" | "oauth" | "credential" | "credentials" | "secret" | "secrets"))
}

/// Whether a value is already a reference to an environment variable.
fn is_env_ref(v: &str) -> bool {
    let t = v.trim();
    (t.starts_with("${") && t.ends_with('}'))
        || (t.starts_with("{env:") && t.ends_with('}'))
        || t.starts_with("env:")
        || (t.starts_with('$') && t[1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && t.len() > 1)
}

/// Whether a string value looks like a credential on its own.
pub fn looks_secret(v: &str) -> bool {
    if v.len() < 12 || is_env_ref(v) {
        return false;
    }
    if SECRET_PREFIXES.iter().any(|p| v.starts_with(p)) {
        return true;
    }
    let hexish = v.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if v.len() < 32 || hexish || v.contains(char::is_whitespace) || v.contains("://") {
        return false;
    }
    if v.starts_with(['/', '.', '~']) || v.contains('@') {
        return false;
    }
    let charset = v.chars().all(|c| c.is_ascii_alphanumeric() || "_-.=+/".contains(c));
    let digits = v.chars().any(|c| c.is_ascii_digit());
    let upper = v.chars().any(|c| c.is_ascii_uppercase());
    let lower = v.chars().any(|c| c.is_ascii_lowercase());
    charset && digits && upper && lower
}

/// Free text with every credential-looking word replaced by `[redacted]`, for prompts
/// drafted from traces. A word is split at `=` and `:` so `TOKEN=sk-…` is caught too.
pub fn redact_text(s: &str) -> String {
    let trim = |c: char| "\"'`()[]{}<>,;.!?".contains(c);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        let core = word.trim_matches(trim);
        let mut kept = word.to_string();
        if !core.contains(['=', ':']) {
            if looks_secret(core) {
                kept = word.replacen(core, "[redacted]", 1);
            }
        } else {
            for part in core.split(['=', ':']).map(|p| p.trim_matches(trim)).filter(|p| looks_secret(p)) {
                kept = kept.replacen(part, "[redacted]", 1);
            }
        }
        out.push_str(&kept);
        let ws = tail.find(|c: char| !c.is_whitespace()).unwrap_or(tail.len());
        out.push_str(&tail[..ws]);
        rest = &tail[ws..];
    }
    out
}

/// Whether a URL scheme's user name is only a name (`ssh://git@host`), not a credential
/// as it is in `https://<token>@host`.
fn userinfo_is_a_name(scheme: &str) -> bool {
    matches!(scheme.to_ascii_lowercase().as_str(), "ssh" | "git+ssh" | "ssh+git" | "sftp")
}

/// A URL recorded as a source (a skill's git remote, a marketplace) with its
/// credentials removed: all userinfo for http(s) and other schemes
/// (`https://ghp_…@github.com/a/b` → `https://github.com/a/b`), the password for ssh.
/// `git@host:path` (scp-like) keeps its user name. A query's credential-looking
/// parameters are dropped.
pub fn strip_userinfo(url: &str) -> String {
    let (base, query) = url.split_once('?').map(|(b, q)| (b, Some(q))).unwrap_or((url, None));
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let (auth, path) = rest.split_once('/').map(|(a, p)| (a, Some(p))).unwrap_or((rest, None));
            let host = match auth.rsplit_once('@') {
                Some((cred, host)) if userinfo_is_a_name(scheme) => match cred.split_once(':') {
                    Some((user, _)) => format!("{user}@{host}"),
                    None => format!("{cred}@{host}"),
                },
                Some((_, host)) => host.to_string(),
                None => auth.to_string(),
            };
            format!("{scheme}://{host}{}", path.map(|p| format!("/{p}")).unwrap_or_default())
        }
        // scp-like: user@host:path, a user name only
        None => match base.split_once('@') {
            Some((user, rest)) if user.contains(':') => format!("{}@{rest}", user.split(':').next().unwrap_or_default()),
            _ => base.to_string(),
        },
    };
    match query {
        Some(q) => {
            let kept: Vec<&str> = q.split('&').filter(|kv| !kv.split_once('=').is_some_and(|(k, v)| is_secret_key(k) || looks_secret(v))).collect();
            if kept.is_empty() { base } else { format!("{base}?{}", kept.join("&")) }
        }
        None => base,
    }
}

/// Collects placeholders for the secrets a capture redacts.
#[derive(Default)]
struct Secrets {
    refs: Vec<SecretRef>,
    by_value: BTreeMap<String, String>,
}

impl Secrets {
    fn placeholder_name(&self, context: &[&str]) -> String {
        let mut raw = String::from("HLOCK_SECRET");
        for part in context {
            raw.push('_');
            raw.push_str(part);
        }
        let mut name = String::new();
        let mut last_us = false;
        for c in raw.chars() {
            if c.is_ascii_alphanumeric() {
                name.push(c.to_ascii_uppercase());
                last_us = false;
            } else if !last_us {
                name.push('_');
                last_us = true;
            }
        }
        let mut name = name.trim_end_matches('_').to_string();
        name.truncate(64);
        let base = name.clone();
        let mut n = 2;
        while self.refs.iter().any(|r| r.name == name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        name
    }

    /// Replace `value` with a placeholder, reusing one for a value seen before.
    fn redact(&mut self, context: &[&str], used_by: &str, value: &str) -> String {
        let id = sha256_bytes(value.as_bytes());
        if let Some(name) = self.by_value.get(&id) {
            return format!("${{{name}}}");
        }
        let name = self.placeholder_name(context);
        self.by_value.insert(id, name.clone());
        self.refs.push(SecretRef { name: name.clone(), used_by: used_by.to_string() });
        format!("${{{name}}}")
    }

    /// Redact a string if its key or its shape says it's a secret; URLs get their
    /// credential-bearing query parameters and userinfo redacted.
    /// `used_by` describes where the value lives, for the lock's `[[secret]]` entry.
    fn scrub(&mut self, context: &[&str], key: &str, value: &str, force: bool, used_by: &str) -> String {
        if value.is_empty() || is_env_ref(value) || is_placeholder_key(value) {
            return value.to_string();
        }
        if force || is_secret_key(key) || looks_secret(value) {
            return self.redact(context, used_by, value);
        }
        if value.contains("://") {
            return self.scrub_url(context, value, used_by);
        }
        value.to_string()
    }

    fn scrub_url(&mut self, context: &[&str], url: &str, used_by: &str) -> String {
        let (base, query) = match url.split_once('?') {
            Some((b, q)) => (b.to_string(), Some(q.to_string())),
            None => (url.to_string(), None),
        };
        // userinfo: scheme://user:pass@host, or a token alone (https://ghp_…@host); an ssh
        // user name without a password (ssh://git@host) isn't a secret
        let base = match base.split_once("://") {
            Some((scheme, rest)) => {
                let (auth, host) = rest.split_once('/').map(|(a, h)| (a.to_string(), format!("/{h}"))).unwrap_or((rest.to_string(), String::new()));
                match auth.rsplit_once('@') {
                    Some((cred, hostpart)) if cred.contains(':') || !userinfo_is_a_name(scheme) => {
                        let mut ctx2: Vec<&str> = context.to_vec();
                        ctx2.push("url_credentials");
                        let ph = self.redact(&ctx2, &format!("{used_by} (credentials)"), cred);
                        format!("{scheme}://{ph}@{hostpart}{host}")
                    }
                    _ => base.clone(),
                }
            }
            None => base,
        };
        let Some(q) = query else { return base };
        let parts: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) if is_secret_key(k) || looks_secret(v) => {
                    let mut ctx2: Vec<&str> = context.to_vec();
                    ctx2.push(k);
                    let label = format!("{used_by} ?{k}");
                    format!("{k}={}", self.redact(&ctx2, &label, v))
                }
                _ => kv.to_string(),
            })
            .collect();
        format!("{base}?{}", parts.join("&"))
    }

    /// Where a config value lives: `<file>#/<json/pointer>` for `[harness, file, keys…]`.
    fn config_label(context: &[String]) -> String {
        match context {
            [_, file, rest @ ..] => format!("{file}#/{}", rest.join("/")),
            _ => context.join("/"),
        }
    }

    /// Redact secrets anywhere in a JSON value (`context` names its location).
    fn scrub_value(&mut self, context: &mut Vec<String>, v: &mut Value, inherited: bool) {
        match v {
            Value::Object(map) => {
                for (k, child) in map.iter_mut() {
                    let force = inherited || (is_secret_container(k) && !child.is_string());
                    context.push(k.clone());
                    if let Value::String(s) = child {
                        let label = Self::config_label(context);
                        let ctx: Vec<&str> = context.iter().map(String::as_str).collect();
                        *s = self.scrub(&ctx, k, s, inherited, &label);
                    } else {
                        self.scrub_value(context, child, force);
                    }
                    context.pop();
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    if let Value::String(s) = item {
                        let label = Self::config_label(context);
                        let ctx: Vec<&str> = context.iter().map(String::as_str).collect();
                        let key = context.last().cloned().unwrap_or_default();
                        *s = self.scrub(&ctx, &key, s, inherited, &label);
                    } else {
                        self.scrub_value(context, item, inherited);
                    }
                }
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------------- //
// Paths and digests
// --------------------------------------------------------------------------- //

/// A path written so it means the same thing on another machine: `{root}/…`,
/// `~/…`, a project-relative path, or (last resort) the absolute path.
pub fn portable_path(p: &Path, root: Option<&Path>, ctx: &Ctx) -> String {
    if let Some(r) = root {
        if let Ok(rel) = p.strip_prefix(r) {
            return if rel.as_os_str().is_empty() { "{root}".into() } else { format!("{{root}}/{}", rel.to_string_lossy()) };
        }
    }
    if let Ok(rel) = p.strip_prefix(&ctx.cwd) {
        return rel.to_string_lossy().into_owned();
    }
    if let Ok(rel) = p.strip_prefix(&ctx.home) {
        return format!("~/{}", rel.to_string_lossy());
    }
    p.to_string_lossy().into_owned()
}

/// A project file: under the working directory, but not under the harness's own config
/// root (an isolated root, like the meta harness's, can live inside the project).
pub(crate) fn is_project_path(p: &Path, root: &Path, ctx: &Ctx) -> bool {
    p.starts_with(&ctx.cwd) && !ctx.home.starts_with(&ctx.cwd) && !p.starts_with(root)
}

/// Whether a profile source lives in the project: a `file#/pointer` source by its file,
/// a path template by the path it expands to. Claude Code's per-project entries in
/// `~/.claude.json` (`#/projects/{cwd}/…`) are the user's own, not the repository's.
pub(crate) fn source_is_project(profile: &HarnessProfile, source: &str, root: &Path, ctx: &Ctx) -> bool {
    source_path(profile, source, root, ctx).is_some_and(|p| is_project_path(&p, root, ctx))
}

/// The file a profile source reads: a `file#/pointer` source's file, or a path template.
pub(crate) fn source_path(profile: &HarnessProfile, source: &str, root: &Path, ctx: &Ctx) -> Option<PathBuf> {
    match source.split_once('#') {
        Some((file, _)) => profile.file_path(file, ctx),
        None => Some(ctx.expand(source, Some(root))),
    }
}

/// How much of the user scope a pin captures, besides the project's setup.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UserScope {
    /// The project's setup only (`hlock pin`).
    #[default]
    None,
    /// Everything in the user scope (`hlock pin --user`).
    All,
    /// User-level items whose path matches one of these globs (`--include-user-scope`).
    /// Relative patterns are relative to the harness's config root.
    Matching(Vec<String>),
}

impl UserScope {
    /// The user scope a pinned lock was captured with, so `check` captures the same.
    pub fn of(lock: &Lockfile) -> UserScope {
        if !lock.provenance.user_include.is_empty() {
            UserScope::Matching(lock.provenance.user_include.clone())
        } else if lock.pins_user_scope() {
            UserScope::All
        } else {
            UserScope::None
        }
    }

    /// The `hlock pin` flags that select this scope.
    pub fn flags(&self) -> String {
        match self {
            UserScope::None => String::new(),
            UserScope::All => "--user".into(),
            UserScope::Matching(pats) => pats.iter().map(|p| format!("-i {}", shell_quote(p))).collect::<Vec<_>>().join(" "),
        }
    }
}

/// Quote `s` for a POSIX shell the way Python's `shlex.quote` does.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// [`UserScope`] resolved against one harness's paths, remembering which patterns matched.
struct UserFilter<'a> {
    scope: &'a UserScope,
    patterns: Vec<String>,
    hit: RefCell<BTreeSet<usize>>,
}

impl<'a> UserFilter<'a> {
    fn new(scope: &'a UserScope, root: &Path, ctx: &Ctx) -> Self {
        let patterns = match scope {
            UserScope::Matching(pats) => pats.iter().map(|p| expand_pattern(p, root, ctx)).collect(),
            _ => Vec::new(),
        };
        UserFilter { scope, patterns, hit: RefCell::new(BTreeSet::new()) }
    }

    /// Whether a user-level item at `path` is captured. A pattern that matches a directory
    /// takes everything under it.
    fn allows(&self, path: Option<&Path>) -> bool {
        match self.scope {
            UserScope::None => false,
            UserScope::All => true,
            UserScope::Matching(_) => {
                let Some(path) = path else { return false };
                let mut any = false;
                for (i, pat) in self.patterns.iter().enumerate() {
                    if path.ancestors().any(|a| glob_match(pat.as_bytes(), a.to_string_lossy().as_bytes())) {
                        self.hit.borrow_mut().insert(i);
                        any = true;
                    }
                }
                any
            }
        }
    }

    /// Patterns that matched nothing.
    fn unmatched(&self) -> Vec<String> {
        let UserScope::Matching(pats) = self.scope else { return Vec::new() };
        let hit = self.hit.borrow();
        pats.iter().enumerate().filter(|(i, _)| !hit.contains(i)).map(|(_, p)| p.clone()).collect()
    }
}

/// An `--include-user-scope` pattern as an absolute glob: `~/` is the home directory,
/// `{root}` and relative patterns are the harness's config root.
fn expand_pattern(pat: &str, root: &Path, ctx: &Ctx) -> String {
    let full = if let Some(rest) = pat.strip_prefix("{root}") {
        format!("{}{rest}", root.display())
    } else if pat == "~" {
        ctx.home.display().to_string()
    } else if let Some(rest) = pat.strip_prefix("~/") {
        ctx.home.join(rest).display().to_string()
    } else if Path::new(pat).is_absolute() {
        pat.to_string()
    } else {
        root.join(pat).display().to_string()
    };
    full.trim_end_matches('/').to_string()
}

/// Glob match over a path: `*` and `?` stay within one segment, `**` crosses segments
/// (and `**/` also matches no directory at all).
fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    match pat.first() {
        None => text.is_empty(),
        Some(b'*') if pat.get(1) == Some(&b'*') => {
            let rest = &pat[2..];
            if rest.first() == Some(&b'/') && glob_match(&rest[1..], text) {
                return true;
            }
            (0..=text.len()).any(|i| glob_match(rest, &text[i..]))
        }
        Some(b'*') => {
            for i in 0..=text.len() {
                if glob_match(&pat[1..], &text[i..]) {
                    return true;
                }
                if i < text.len() && text[i] == b'/' {
                    break;
                }
            }
            false
        }
        Some(b'?') => text.first().is_some_and(|c| *c != b'/') && glob_match(&pat[1..], &text[1..]),
        Some(c) => text.first() == Some(c) && glob_match(&pat[1..], &text[1..]),
    }
}

/// Files under `dir` (sorted, relative), skipping VCS metadata and OS litter.
/// Symlinks inside it are never followed (a link to `~/.ssh` isn't vendored, and a link
/// to a parent can't loop); they're listed in the second vector.
fn walk(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    fn rec(base: &Path, dir: &Path, out: &mut Vec<PathBuf>, links: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), ".git" | ".DS_Store" | "__pycache__" | "node_modules") {
                continue;
            }
            let p = e.path();
            let rel = || p.strip_prefix(base).map(Path::to_path_buf).ok();
            // the entry's own type: a symlink is a symlink, whatever it points to
            match e.file_type() {
                Ok(t) if t.is_symlink() => links.extend(rel()),
                Ok(t) if t.is_dir() => rec(base, &p, out, links),
                Ok(t) if t.is_file() => out.extend(rel()),
                _ => {}
            }
        }
    }
    let (mut out, mut links) = (Vec::new(), Vec::new());
    rec(dir, dir, &mut out, &mut links);
    out.sort();
    links.sort();
    (out, links)
}

pub(crate) fn walk_files(dir: &Path) -> Vec<PathBuf> {
    walk(dir).0
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    false
}

/// Merkle-style tree digest (SPEC §7.7.2): normalized relative paths, contents,
/// and executable bits. A vendored copy and a checkout of the same tree match.
/// The files in a skill's `scripts/` directory, relative to it and sorted: what the skill
/// can run.
pub fn skill_scripts(skill: &Path) -> Vec<String> {
    let dir = skill.join("scripts");
    if !dir.is_dir() {
        return Vec::new();
    }
    crate::scan::text_files(&dir).iter().filter_map(|p| p.strip_prefix(&dir).ok().map(|r| r.to_string_lossy().replace('\\', "/"))).collect()
}

pub fn tree_digest(dir: &Path) -> String {
    tree_digest_checked(dir).0
}

/// [`tree_digest`], and the files it couldn't read. An unreadable file is digested as
/// unreadable, never as empty, so it can't match an empty file.
fn tree_digest_checked(dir: &Path) -> (String, Vec<String>) {
    let mut acc = String::new();
    let mut unreadable = Vec::new();
    for rel in walk_files(dir) {
        let p = dir.join(&rel);
        let digest = match std::fs::read(&p) {
            Ok(bytes) => sha256_bytes(&bytes),
            Err(e) => {
                unreadable.push(format!("{}: {e}", p.display()));
                "unreadable".into()
            }
        };
        acc.push_str(&format!("{}\0{}\0{}\n", rel.to_string_lossy().replace('\\', "/"), if is_executable(&p) { "x" } else { "-" }, digest));
    }
    (sha256_bytes(acc.as_bytes()), unreadable)
}

const MAX_VENDOR_BYTES: u64 = 20 * 1024 * 1024;

/// Vendor the files under `dir` as `dest/…`: all of them, or none (an unreadable file
/// is an error, not an empty file). Symlinks aren't followed; each one is a warning.
pub(crate) fn vendor_tree(dir: &Path, dest: &str, out: &mut Vec<VendorFile>, warnings: &mut Vec<String>) -> std::result::Result<(), String> {
    let (files, links) = walk(dir);
    let total: u64 = files.iter().filter_map(|f| std::fs::metadata(dir.join(f)).ok()).map(|m| m.len()).sum();
    if total > MAX_VENDOR_BYTES {
        return Err(format!("{} is {} MB, too large to vendor", dir.display(), total / 1_000_000));
    }
    let mut add = Vec::new();
    for rel in files {
        let p = dir.join(&rel);
        let bytes = std::fs::read(&p).map_err(|e| format!("can't read {}: {e}; not vendored", p.display()))?;
        add.push(VendorFile { rel: format!("{dest}/{}", rel.to_string_lossy().replace('\\', "/")), bytes, executable: is_executable(&p) });
    }
    out.extend(add);
    warnings.extend(links.iter().map(|l| format!("{} is a symlink; not vendored", dir.join(l).display())));
    Ok(())
}

// --------------------------------------------------------------------------- //
// Models and local runtimes
// --------------------------------------------------------------------------- //

/// Host and port of a URL (`http://localhost:11434/v1` → `localhost`, `11434`).
pub(crate) fn host_port(url: &str) -> Option<(String, Option<u16>)> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let auth = rest.split(['/', '?']).next()?;
    let auth = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    if let Some(stripped) = auth.strip_prefix('[') {
        let (h, p) = stripped.split_once(']')?;
        return Some((h.to_string(), p.strip_prefix(':').and_then(|p| p.parse().ok())));
    }
    match auth.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok())),
        None => Some((auth.to_string(), None)),
    }
}

fn is_local_host(h: &str) -> bool {
    matches!(h, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "host.docker.internal") || h.ends_with(".localhost")
}

/// Private-network hosts: RFC 1918, CGNAT/Tailscale (100.64/10), link-local, mDNS and tailnet names.
fn is_private_host(h: &str) -> bool {
    if h.ends_with(".local") || h.ends_with(".lan") || h.ends_with(".internal") || h.ends_with(".ts.net") || h.ends_with(".home.arpa") {
        return true;
    }
    if !h.contains('.') && !h.contains(':') && !h.is_empty() {
        return true; // a bare hostname on the LAN
    }
    let o: Vec<u8> = h.split('.').filter_map(|x| x.parse().ok()).collect();
    if o.len() != 4 {
        return false;
    }
    o[0] == 10 || (o[0] == 172 && (16..32).contains(&o[1])) || (o[0] == 192 && o[1] == 168) || (o[0] == 100 && (64..128).contains(&o[1])) || (o[0] == 169 && o[1] == 254)
}

/// The runtime behind a provider, and where it runs (`local` or `network`): by
/// provider name, then by the base URL's port. Public hosts are API providers.
fn match_runtime<'r>(reg: &'r Registry, provider: Option<&str>, base_url: Option<&str>) -> Option<(&'r RuntimeProfile, &'static str)> {
    let hp = base_url.and_then(host_port);
    let location = match hp.as_ref().map(|(h, _)| h.as_str()) {
        None => "local",
        Some(h) if is_local_host(h) => "local",
        Some(h) if is_private_host(h) => "network",
        Some(_) => return None,
    };
    if let Some(p) = provider.map(str::to_ascii_lowercase) {
        if let Some(r) = reg.runtimes.iter().find(|r| r.provider_names.iter().any(|n| n.to_ascii_lowercase() == p) || r.matches(&p)) {
            return Some((r, location));
        }
    }
    let port = hp.and_then(|(_, p)| p)?;
    reg.runtimes.iter().find(|r| r.ports.contains(&port)).map(|r| (r, location))
}

fn new_runtime(rt: &RuntimeProfile, location: &str, endpoint: Option<String>, ctx: &Ctx) -> Runtime {
    let p = if location == "local" { probe_runtime(rt, ctx) } else { Default::default() };
    let endpoint = endpoint.as_deref().map(strip_userinfo);
    let name = runtime_key(rt, location, endpoint.as_deref());
    Runtime { name, location: location.into(), version: p.version, source: p.source, endpoint, model: vec![] }
}

fn runtime_key(rt: &RuntimeProfile, location: &str, endpoint: Option<&str>) -> String {
    match (location, endpoint.and_then(host_port)) {
        ("network", Some((h, _))) => format!("{}@{h}", rt.id),
        _ => rt.id.clone(),
    }
}

/// Network runtimes can't be inspected, so their models are recorded, not located.
fn runtime_model(rt: &RuntimeProfile, location: &str, id: &str, ctx: &Ctx) -> RuntimeModel {
    if location == "local" {
        locate_model(rt, id, ctx)
    } else {
        RuntimeModel { id: id.to_string(), ..Default::default() }
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Look a model up in a runtime's local store.
pub(crate) fn locate_model(rt: &RuntimeProfile, model: &str, ctx: &Ctx) -> RuntimeModel {
    let mut m = RuntimeModel { id: model.to_string(), ..Default::default() };
    let dir = rt.models_dir(ctx);
    match rt.models.layout.as_str() {
        "ollama" => {
            // hf.co/<org>/<repo>:<quant> | <ns>/<name>:<tag> | <name>:<tag>
            let (name, tag) = model.rsplit_once(':').filter(|(_, t)| !t.contains('/')).unwrap_or((model, "latest"));
            let rel = if name.starts_with("hf.co/") || name.starts_with("huggingface.co/") {
                format!("{name}/{tag}")
            } else if name.contains('/') {
                format!("registry.ollama.ai/{name}/{tag}")
            } else {
                format!("registry.ollama.ai/library/{name}/{tag}")
            };
            m.source = Some(match name.strip_prefix("hf.co/").or_else(|| name.strip_prefix("huggingface.co/")) {
                Some(repo) => format!("hf://{repo}"),
                None => format!("ollama:{name}:{tag}"),
            });
            if let Some(path) = dir.map(|d| d.join("manifests").join(rel)) {
                if let Ok(bytes) = std::fs::read(&path) {
                    m.present = true;
                    m.digest = Some(sha256_bytes(&bytes));
                    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                        let size: u64 = v["layers"].as_array().map(|ls| ls.iter().filter_map(|l| l["size"].as_u64()).sum()).unwrap_or(0);
                        m.size_bytes = Some(size + v["config"]["size"].as_u64().unwrap_or(0));
                    }
                }
            }
        }
        "publisher-repo" => {
            let want = norm(model.rsplit('/').next().unwrap_or(model));
            let found = dir.and_then(|d| {
                let pubs = std::fs::read_dir(&d).ok()?;
                for p in pubs.flatten().filter(|p| p.path().is_dir()) {
                    for r in std::fs::read_dir(p.path()).ok()?.flatten().filter(|r| r.path().is_dir()) {
                        let repo = r.file_name().to_string_lossy().into_owned();
                        let n = norm(repo.trim_end_matches("-GGUF").trim_end_matches("-gguf").trim_end_matches("-MLX"));
                        if !want.is_empty() && (n.contains(&want) || want.contains(&n)) {
                            return Some((p.file_name().to_string_lossy().into_owned(), repo, r.path()));
                        }
                    }
                }
                None
            });
            if let Some((publisher, repo, path)) = found {
                m.present = true;
                m.source = Some(format!("hf://{publisher}/{repo}"));
                m.size_bytes = Some(walk_files(&path).iter().filter_map(|f| std::fs::metadata(path.join(f)).ok()).map(|x| x.len()).sum());
            }
        }
        "hf-cache" => {
            if model.contains('/') {
                m.source = Some(format!("hf://{model}"));
                if let Some(d) = dir {
                    let cache = d.join(format!("models--{}", model.replace('/', "--")));
                    if let Ok(rev) = std::fs::read_to_string(cache.join("refs/main")) {
                        m.present = true;
                        m.revision = Some(rev.trim().to_string());
                    }
                }
            }
        }
        _ => {}
    }
    m
}

/// Model ids configured under one provider entry (object keys or a list of ids).
fn provider_models(entry: &Value, pointers: &[String]) -> Vec<String> {
    for p in pointers {
        match entry.pointer(p) {
            Some(Value::Object(m)) => return m.keys().cloned().collect(),
            Some(Value::Array(a)) => {
                return a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string).or_else(|| x["id"].as_str().or(x["name"].as_str()).map(str::to_string)))
                    .collect()
            }
            _ => {}
        }
    }
    Vec::new()
}

// --------------------------------------------------------------------------- //
// MCP servers
// --------------------------------------------------------------------------- //

/// The package a launcher command runs (`npx -y pkg@1` → `npm:pkg@1`).
fn launcher_source(command: &str, args: &[String]) -> Option<String> {
    let bin = Path::new(command).file_name()?.to_string_lossy().into_owned();
    let first_pkg = |skip: &[&str]| args.iter().find(|a| !a.starts_with('-') && !skip.contains(&a.as_str())).cloned();
    match bin.as_str() {
        "npx" | "bunx" => first_pkg(&[]).map(|p| format!("npm:{p}")),
        "pnpm" | "yarn" if args.first().map(String::as_str) == Some("dlx") => first_pkg(&["dlx"]).map(|p| format!("npm:{p}")),
        "uvx" | "pipx" => first_pkg(&["run"]).map(|p| format!("pypi:{p}")),
        "uv" if args.first().map(String::as_str) == Some("tool") => first_pkg(&["tool", "run"]).map(|p| format!("pypi:{p}")),
        "docker" | "podman" => {
            let mut it = args.iter().skip_while(|a| *a != "run").skip(1);
            let mut image = None;
            while let Some(a) = it.next() {
                if a.starts_with('-') {
                    if matches!(a.as_str(), "-e" | "--env" | "-v" | "--volume" | "--name" | "-p" | "--network") {
                        it.next();
                    }
                    continue;
                }
                image = Some(a.clone());
                break;
            }
            image.map(|i| format!("oci:{i}"))
        }
        _ => None,
    }
}

fn str_map(v: Option<&Value>, secrets: &mut Secrets, context: &[&str]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(Value::Object(m)) = v {
        for (k, val) in m {
            let s = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let mut c: Vec<&str> = context.to_vec();
            c.push(k);
            let label = format!("mcp {} {k}", context.join(" "));
            out.insert(k.clone(), secrets.scrub(&c, k, &s, false, &label));
        }
    }
    out
}

/// A server entry as `pin` would record it (secrets replaced with placeholders), for an
/// entry hlock writes itself (`hlock add mcp`).
pub(crate) fn mcp_tool(name: &str, v: &Value, scope: &str) -> Option<Tool> {
    normalize_mcp(name, v, scope, &mut Secrets::default())
}

/// Normalize one MCP server entry from any harness's config shape.
fn normalize_mcp(name: &str, v: &Value, scope: &str, secrets: &mut Secrets) -> Option<Tool> {
    let mut obj = v.as_object()?.clone();
    // Zed's older form: `"command": {"path": …, "args": […], "env": {…}}`
    if let Some(Value::Object(c)) = obj.get("command").cloned() {
        obj.remove("command");
        for (from, to) in [("path", "command"), ("args", "args"), ("env", "env")] {
            if let Some(x) = c.get(from) {
                obj.insert(to.into(), x.clone());
            }
        }
    }
    let obj = &obj;
    let ctx = [name];
    let mut cmd_parts: Vec<String> = match obj.get("command").or_else(|| obj.get("cmd")) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => vec![],
    };
    let mut args: Vec<String> =
        obj.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    if cmd_parts.len() > 1 {
        let rest = cmd_parts.split_off(1);
        args = rest.into_iter().chain(args).collect();
    }
    let command = cmd_parts.pop();
    let url = ["url", "serverUrl", "httpUrl", "uri", "endpoint"].iter().find_map(|k| obj.get(*k).and_then(Value::as_str)).map(str::to_string);
    let declared = obj.get("type").or_else(|| obj.get("transport")).and_then(Value::as_str).unwrap_or("");
    let transport = match declared.to_ascii_lowercase().as_str() {
        "sse" => "sse",
        "http" | "remote" | "streamable-http" | "streamablehttp" | "streamable_http" => "http",
        "stdio" | "local" => "stdio",
        _ if url.is_some() && command.is_none() => "http",
        _ => "stdio",
    };
    let args: Vec<String> = args.iter().map(|a| secrets.scrub(&ctx, "arg", a, false, &format!("mcp {name} args"))).collect();
    let env = str_map(obj.get("env").or_else(|| obj.get("environment")).or_else(|| obj.get("envs")), secrets, &ctx);
    let headers = str_map(obj.get("headers").or_else(|| obj.get("http_headers")), secrets, &[name, "header"]);
    let enabled = match (obj.get("enabled").and_then(Value::as_bool), obj.get("disabled").and_then(Value::as_bool)) {
        (Some(e), _) => Some(e),
        (None, Some(d)) => Some(!d),
        _ => None,
    };
    let source = command.as_deref().and_then(|c| launcher_source(c, &args)).or_else(|| url.as_ref().map(|_| "remote".to_string()));
    Some(Tool {
        name: name.to_string(),
        kind: "mcp".into(),
        source,
        transport: Some(transport.into()),
        command,
        args,
        url: url.map(|u| secrets.scrub(&ctx, "url", &u, false, &format!("mcp {name} url"))),
        env,
        headers,
        scope: Some(scope.into()),
        enabled,
        ..Default::default()
    })
}

// --------------------------------------------------------------------------- //
// Skills
// --------------------------------------------------------------------------- //

/// Directories under `dir` (up to three levels) that contain a `SKILL.md`.
fn find_skills(dir: &Path, include_hidden: bool) -> Vec<PathBuf> {
    fn rec(d: &Path, depth: usize, hidden: bool, out: &mut Vec<PathBuf>) {
        if d.join("SKILL.md").is_file() {
            out.push(d.to_path_buf());
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut subs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                // a copy sync set aside (`x.hlock-bak`) isn't a skill
                (hidden || !n.starts_with('.')) && n != "node_modules" && !n.ends_with(".hlock-bak") && e.path().is_dir()
            })
            .map(|e| e.path())
            .collect();
        subs.sort();
        for s in subs {
            rec(&s, depth - 1, false, out);
        }
    }
    let mut out = Vec::new();
    rec(dir, 3, include_hidden, &mut out);
    out
}

/// `name:` from a SKILL.md's YAML frontmatter.
fn skill_name(dir: &Path) -> String {
    let fallback = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else { return fallback };
    let Some(rest) = text.strip_prefix("---") else { return fallback };
    let front = rest.split("\n---").next().unwrap_or("");
    front
        .lines()
        .find_map(|l| l.strip_prefix("name:").map(|v| v.trim().trim_matches(['"', '\'']).to_string()))
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback)
}

/// Remote, commit and cleanliness of the git checkout containing `dir`, if any.
fn git_origin(dir: &Path, ctx: &Ctx) -> Option<(String, String, String, bool)> {
    let git = ctx.which("git")?;
    // stdout of a command that succeeded; `None` when git fails (not a checkout, no remote)
    let run = |args: &[&str]| {
        let mut cmd = std::process::Command::new(&git);
        cmd.arg("-C").arg(dir).args(args).current_dir(&ctx.cwd).env_clear().envs(&ctx.env).stdin(std::process::Stdio::null());
        let out = crate::process::run(cmd, &crate::process::Limits::timeout(Duration::from_secs(5))).ok()?;
        out.success.then(|| out.stdout_text().trim().to_string())
    };
    let top = run(&["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top.lines().next()?);
    if top == ctx.home || ctx.cwd.starts_with(&top) {
        return None; // a dotfiles repo at ~, or the project itself
    }
    let url = run(&["config", "--get", "remote.origin.url"]).and_then(|s| s.lines().next().map(str::to_string)).filter(|s| !s.is_empty())?;
    let commit = run(&["rev-parse", "HEAD"]).and_then(|s| s.lines().next().map(str::to_string))?;
    let rel = dir.strip_prefix(&top).ok()?.to_string_lossy().into_owned();
    let dirty = run(&["status", "--porcelain", "--", "."]).map(|s| !s.trim().is_empty()).unwrap_or(true);
    Some((url, commit, rel, dirty))
}

fn builtin_names(profile: &HarnessProfile, root: &Path, ctx: &Ctx) -> BTreeSet<String> {
    let Some(tpl) = &profile.skills.builtin_manifest else { return BTreeSet::new() };
    let Ok(text) = std::fs::read_to_string(ctx.expand(tpl, Some(root))) else { return BTreeSet::new() };
    text.lines().filter_map(|l| l.split(':').next()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

// --------------------------------------------------------------------------- //
// Bundles (plugins, packages)
// --------------------------------------------------------------------------- //

fn npm_name_version(spec: &str) -> (String, Option<String>) {
    let s = spec.strip_prefix("npm:").unwrap_or(spec);
    let (scope, rest) = if let Some(r) = s.strip_prefix('@') { ("@", r) } else { ("", s) };
    match rest.split_once('@') {
        Some((n, v)) if !v.is_empty() => (format!("{scope}{n}"), Some(v.to_string())),
        _ => (format!("{scope}{rest}"), None),
    }
}

fn claude_plugins(profile: &HarnessProfile, src: &Sources, path: &Path, out: &mut Vec<Native>) {
    let Ok(Some(installed)) = read_config(path, "json") else { return };
    let enabled = src.get("settings#/enabledPlugins").unwrap_or(json!({}));
    let markets = path.parent().map(|d| d.join("known_marketplaces.json")).and_then(|p| read_config(&p, "json").ok().flatten()).unwrap_or(json!({}));
    let mut used = BTreeSet::new();
    if let Some(Value::Object(plugins)) = installed.get("plugins") {
        for (id, entries) in plugins {
            let e = entries.as_array().and_then(|a| a.first()).cloned().unwrap_or(json!({}));
            let market = id.split_once('@').map(|(_, m)| m.to_string());
            if let Some(m) = &market {
                used.insert(m.clone());
            }
            out.push(Native {
                kind: format!("{}.plugin", profile.id),
                name: id.clone(),
                version: e["version"].as_str().map(str::to_string),
                source: market.map(|m| format!("marketplace:{m}")),
                revision: e["gitCommitSha"].as_str().map(str::to_string),
                integrity: None,
                enabled: enabled.get(id).and_then(Value::as_bool).unwrap_or(false),
                config: e["scope"].as_str().map(|s| json!({"scope": s})),
                ..Default::default()
            });
        }
    }
    for m in used {
        let s = &markets[&m]["source"];
        let (source, revision) = match s["source"].as_str() {
            Some("github") => (s["repo"].as_str().map(|r| format!("github:{r}")), s["ref"].as_str().map(str::to_string)),
            Some("git") => (s["url"].as_str().map(|u| format!("git+{}", strip_userinfo(u))), s["ref"].as_str().map(str::to_string)),
            Some("directory") | Some("local") => (s["path"].as_str().map(|p| format!("path:{p}")), None),
            Some("url") => (s["url"].as_str().map(strip_userinfo), None),
            _ => (None, None),
        };
        out.push(Native {
            kind: format!("{}.marketplace", profile.id),
            name: m,
            source,
            revision,
            enabled: true,
            ..Default::default()
        });
    }
}

fn pi_packages(profile: &HarnessProfile, list: &Value, lockfile: Option<&Path>, out: &mut Vec<Native>) {
    let lock = lockfile.and_then(|p| read_config(p, "json").ok().flatten()).unwrap_or(json!({}));
    for item in list.as_array().cloned().unwrap_or_default() {
        let (spec, extra) = match &item {
            Value::String(s) => (s.clone(), None),
            Value::Object(o) => {
                let mut rest = o.clone();
                let s = rest.remove("source").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                (s, (!rest.is_empty()).then_some(Value::Object(rest)))
            }
            _ => continue,
        };
        let mut n = Native { kind: format!("{}.package", profile.id), enabled: true, config: extra, ..Default::default() };
        if spec.starts_with("npm:") {
            let (name, ver) = npm_name_version(&spec);
            let entry = &lock["packages"][format!("node_modules/{name}")];
            n.version = entry["version"].as_str().map(str::to_string).or(ver);
            n.integrity = entry["integrity"].as_str().map(str::to_string);
            n.source = Some(match &n.version {
                Some(v) => format!("npm:{name}@{v}"),
                None => format!("npm:{name}"),
            });
            n.name = name;
        } else {
            n.name = spec.clone();
            n.source = Some(spec);
        }
        out.push(n);
    }
}

fn npm_list(profile: &HarnessProfile, kind: &str, list: &Value, out: &mut Vec<Native>) {
    for item in list.as_array().cloned().unwrap_or_default() {
        let Some(spec) = item.as_str() else { continue };
        let local = spec.starts_with("file:") || spec.starts_with('.') || spec.starts_with('/');
        let (name, version) = if local { (spec.to_string(), None) } else { npm_name_version(spec) };
        out.push(Native {
            kind: format!("{}.{kind}", profile.id),
            source: Some(if local { format!("path:{}", spec.trim_start_matches("file:")) } else { format!("npm:{spec}") }),
            name,
            version,
            enabled: true,
            ..Default::default()
        });
    }
}

// --------------------------------------------------------------------------- //
// Capture
// --------------------------------------------------------------------------- //

/// Capture `profile`'s live setup. Nothing is written; `vendor` holds the files
/// [`pin`] would place under [`VENDOR_DIR`].
///
/// Skills, MCP servers, plugins, instruction files and config come from the project, plus
/// what `user` selects from the harness's user-level config (`~/.claude`, `~/.pi/agent`,
/// …): nothing, everything, or the items matching its patterns. The harness, its models and
/// runtimes, and its decision models are always captured: they say what the stack *is*,
/// wherever they're set.
pub fn capture(profile: &HarnessProfile, reg: &Registry, ctx: &Ctx, project: Option<&str>, user: &UserScope) -> Result<Capture> {
    let src = Sources::new(profile, ctx);
    let root = profile.config_root(ctx);
    let users = UserFilter::new(user, &root, ctx);
    let mut secrets = Secrets::default();
    let mut vendor: Vec<VendorFile> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut skip = |kind: &str, n: usize| {
        if n > 0 {
            *skipped.entry(kind.to_string()).or_default() += n;
        }
    };
    let vroot = format!("{VENDOR_DIR}/{}", profile.id);

    // ---- harness ----
    let probe = probe_harness(profile, ctx);
    if (profile.kind == "cli" || profile.is_app()) && !probe.installed {
        warnings.push(format!("{} isn't installed here; pinning its config only", profile.name));
    }

    // ---- models ----
    let mut llms = Vec::new();
    let mut runtimes: BTreeMap<String, Runtime> = BTreeMap::new();
    let provider_entry = |provider: &str| -> Option<Value> {
        let spec = profile.providers.as_ref()?;
        spec.source.iter().find_map(|s| src.get(s).and_then(|m| m.get(provider).cloned()))
    };
    for (role, srcs) in [("default", &profile.model.default), ("small", &profile.model.small)] {
        let found = src.first_str(srcs);
        if found.is_none() && role == "small" {
            continue;
        }
        let (mut model, from) = found.unwrap_or_else(|| ("harness-default".into(), String::new()));
        let mut provider = src.first_str(&profile.model.provider).map(|(p, _)| p);
        if profile.model.format.as_deref() == Some("provider/model") {
            if let Some((p, m)) = model.split_once('/') {
                provider = Some(p.to_string());
                model = m.to_string();
            }
        }
        let provider = provider.or_else(|| profile.model.default_provider.clone());
        let entry = provider.as_deref().and_then(provider_entry);
        let base = src.first_str(&profile.model.base_url).map(|(b, _)| b).or_else(|| {
            let spec = profile.providers.as_ref()?;
            entry.as_ref().and_then(|e| spec.base_url.iter().find_map(|p| e.pointer(p).and_then(Value::as_str).map(str::to_string)))
        });
        let rt = match_runtime(reg, provider.as_deref(), base.as_deref());
        if let Some((rt, location)) = rt {
            let key = runtime_key(rt, location, base.as_deref());
            let r = runtimes.entry(key).or_insert_with(|| new_runtime(rt, location, base.clone(), ctx));
            let mut ids = vec![model.clone()];
            if let (Some(e), Some(spec)) = (&entry, &profile.providers) {
                ids.extend(provider_models(e, &spec.models));
            }
            for id in ids {
                if id != "harness-default" && !r.model.iter().any(|m| m.id == id) {
                    r.model.push(runtime_model(rt, location, &id, ctx));
                }
            }
        }
        llms.push(LLM {
            role: role.into(),
            provider: provider.unwrap_or_else(|| "unknown".into()),
            model,
            resolved_from: (!from.is_empty()).then_some(from),
            runtime: rt.map(|(r, loc)| runtime_key(r, loc, base.as_deref())),
            endpoint: base.map(|b| secrets.scrub(&["llm", role], "url", &b, false, &format!("llm {role} endpoint"))),
            ..Default::default()
        });
    }

    // other configured providers served by a local runtime
    if let Some(spec) = &profile.providers {
        for s in &spec.source {
            let Some(Value::Object(providers)) = src.get(s) else { continue };
            if !source_is_project(profile, s, &root, ctx) && !users.allows(source_path(profile, s, &root, ctx).as_deref()) {
                // the active model's runtime is already recorded above; the rest of a
                // user-level provider list is personal configuration
                skip("providers", providers.len());
                continue;
            }
            for (name, entry) in &providers {
                let base = spec.base_url.iter().find_map(|p| entry.pointer(p).and_then(Value::as_str).map(str::to_string));
                let Some((rt, location)) = match_runtime(reg, Some(name), base.as_deref()) else { continue };
                let key = runtime_key(rt, location, base.as_deref());
                let r = runtimes.entry(key).or_insert_with(|| new_runtime(rt, location, base.clone(), ctx));
                for id in provider_models(entry, &spec.models) {
                    if !r.model.iter().any(|m| m.id == id) {
                        r.model.push(runtime_model(rt, location, &id, ctx));
                    }
                }
            }
        }
    }

    // ---- MCP servers ----
    let mut tools: Vec<Tool> = Vec::new();
    for s in &profile.mcp.sources {
        let scope = if source_is_project(profile, s, &root, ctx) { "project" } else { "user" };
        let file = source_path(profile, s, &root, ctx);
        for (name, cfg) in &named_servers(src.get(s)) {
            if tools.iter().any(|t| &t.name == name) {
                continue;
            }
            if scope == "user" && !users.allows(file.as_deref()) {
                skip("mcp", 1);
                continue;
            }
            if let Some(t) = normalize_mcp(name, cfg, scope, &mut secrets) {
                tools.push(t);
            }
        }
    }

    // ---- skills ----
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen = BTreeSet::new();
    let builtins = builtin_names(profile, &root, ctx);
    let builtin_dirs: Vec<PathBuf> = profile.skills.builtin_dirs.iter().map(|d| ctx.expand(d, Some(&root))).collect();
    let dirs: Vec<(PathBuf, bool)> = profile
        .skills
        .dirs
        .iter()
        .map(|d| (ctx.expand(d, Some(&root)), false))
        .chain(builtin_dirs.iter().map(|d| (d.clone(), true)))
        .collect();
    for (dir, is_builtin_dir) in dirs {
        for sk in find_skills(&dir, is_builtin_dir) {
            let real = std::fs::canonicalize(&sk).unwrap_or(sk.clone());
            if !seen.insert(real.clone()) {
                continue;
            }
            let name = plain_skill_name(&sk, &mut warnings);
            let project = is_project_path(&sk, &root, ctx);
            let builtin = is_builtin_dir || builtin_dirs.iter().any(|b| sk.starts_with(b)) || builtins.contains(&name);
            // a harness's built-in skills come with its version, so they're always recorded
            if !project && !builtin && !users.allows(Some(&sk)) {
                skip("skills", 1);
                continue;
            }
            let (digest, unreadable) = tree_digest_checked(&sk);
            warnings.extend(unreadable.into_iter().map(|u| format!("skill {name}: can't read {u}")));
            let mut s = Skill {
                name: name.clone(),
                digest: Some(digest),
                scripts: Some(skill_scripts(&sk)),
                scope: Some(if project { "project" } else { "user" }.into()),
                path: Some(portable_path(&sk, Some(&root), ctx)),
                enabled: true,
                ..Default::default()
            };
            let realstr = real.to_string_lossy();
            if builtin {
                s.origin = "harness-builtin".into();
                s.provided_by = Some(format!("harness:{}", profile.id));
            } else if realstr.contains("/plugins/") || realstr.contains("/node_modules/") {
                s.origin = "bundled".into();
            } else if project {
                s.origin = "first-party".into();
                s.source = Some(format!("path:{}", portable_path(&sk, None, ctx)));
            } else if let Some((url, commit, rel, dirty)) = git_origin(&real, ctx) {
                s.origin = "third-party".into();
                s.source = Some(format!("git+{}@{commit}#path={rel}", strip_userinfo(&url)));
                if dirty {
                    warnings.push(format!("skill {name} has uncommitted changes; vendoring it"));
                }
                if dirty {
                    vendor_skill(&sk, &vroot, &name, &mut s, &mut vendor, &mut warnings);
                }
            } else {
                s.origin = "local".into();
                vendor_skill(&sk, &vroot, &name, &mut s, &mut vendor, &mut warnings);
            }
            skills.push(s);
        }
    }

    // ---- bundles ----
    let mut natives: Vec<Native> = Vec::new();
    for b in &profile.bundles {
        let mut found = Vec::new();
        match b.extractor.as_str() {
            "claude-plugins" => claude_plugins(profile, &src, &ctx.expand(&b.source, Some(&root)), &mut found),
            "pi-packages" => {
                let lock = b.lockfile.as_ref().map(|l| ctx.expand(l, Some(&root)));
                pi_packages(profile, &src.get(&b.source).unwrap_or(json!([])), lock.as_deref(), &mut found)
            }
            "npm-list" => npm_list(profile, &b.kind, &src.get(&b.source).unwrap_or(json!([])), &mut found),
            other => warnings.push(format!("unknown bundle extractor {other:?} in the {} profile", profile.id)),
        }
        if source_is_project(profile, &b.source, &root, ctx) || users.allows(source_path(profile, &b.source, &root, ctx).as_deref()) {
            natives.extend(found);
        } else {
            skip("plugins", found.iter().filter(|n| !n.kind.ends_with(".marketplace")).count());
        }
    }
    // code harnesses pin their packages
    for (pkg, version) in &probe.packages {
        natives.push(Native {
            kind: "package".into(),
            name: pkg.clone(),
            version: Some(version.clone()),
            source: Some(format!("pypi:{}=={version}", pkg.trim_start_matches("pypi:"))),
            enabled: true,
            ..Default::default()
        });
    }

    // ---- instructions ----
    let mut instructions = Vec::new();
    for tpl in &profile.instructions {
        let p = ctx.expand(tpl, Some(&root));
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let project = is_project_path(&p, &root, ctx);
        if !project && !users.allows(Some(&p)) {
            skip("instructions", 1);
            continue;
        }
        let name = portable_path(&p, Some(&root), ctx);
        let sections = crate::components::sections_in(&String::from_utf8_lossy(&bytes));
        let mut ins = Instructions { name: name.clone(), digest: Some(sha256_bytes(&bytes)), scope: if project { "project" } else { "user" }.into(), sections, ..Default::default() };
        if !project {
            // by file name; a second file of the same name (two AGENTS.md) gets `-2`
            let file = Path::new(p.file_name().unwrap_or_default());
            let (stem, ext) = (file.file_stem().unwrap_or_default().to_string_lossy(), file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default());
            let rel = (1..)
                .map(|n| if n == 1 { format!("{vroot}/instructions/{stem}{ext}") } else { format!("{vroot}/instructions/{stem}-{n}{ext}") })
                .find(|r| !vendor.iter().any(|f| &f.rel == r))
                .unwrap_or_default();
            vendor.push(VendorFile { rel: rel.clone(), bytes, executable: false });
            ins.vendored = Some(rel);
        }
        instructions.push(ins);
    }
    // the project's rules directory: every file there is always-on, and a file named for a
    // role (guardrails.md, memory.md, tool-guidance.md) has that role
    if let Some(rules) = &profile.rules {
        let dir = ctx.cwd.join(&rules.dir);
        for p in walk_files(&dir).into_iter().map(|r| dir.join(r)).filter(|p| p.to_string_lossy().ends_with(&rules.ext)) {
            let name = portable_path(&p, Some(&root), ctx);
            let Ok(bytes) = std::fs::read(&p) else { continue };
            if instructions.iter().any(|i: &Instructions| i.name == name) {
                continue;
            }
            instructions.push(Instructions { name: name.clone(), digest: Some(sha256_bytes(&bytes)), scope: "project".into(), role: crate::components::role_of_file(&name).map(str::to_string), ..Default::default() });
        }
    }

    // ---- subagents, scripts, middleware ----
    let mut component_files = |spec: Option<&crate::profiles::ComponentDirs>, extra: Option<PathBuf>, kind: &str, vendor: &mut Vec<VendorFile>| -> Vec<crate::lockfile::FileComponent> {
        let mut found: Vec<PathBuf> = Vec::new();
        if let Some(s) = spec {
            for d in &s.dirs {
                let dir = ctx.expand(d, Some(&root));
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                let mut files: Vec<PathBuf> = rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.path()).collect();
                files.sort();
                found.extend(files.into_iter().filter(|p| {
                    let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    (s.ext.is_empty() || s.ext.iter().any(|e| n.ends_with(e.as_str()))) && !s.skip.contains(&n)
                }));
            }
            found.extend(s.files.iter().map(|f| ctx.expand(f, Some(&root))).filter(|p| p.is_file()));
        }
        if let Some(dir) = extra {
            found.extend(walk_files(&dir).into_iter().map(|r| dir.join(r)));
        }
        let mut out: Vec<crate::lockfile::FileComponent> = Vec::new();
        for p in found {
            let real = std::fs::canonicalize(&p).unwrap_or(p.clone());
            let project = is_project_path(&p, &root, ctx);
            if !project && !users.allows(Some(&p)) {
                skip(kind, 1);
                continue;
            }
            let Ok(bytes) = std::fs::read(&real) else {
                warnings.push(format!("{kind} {}: can't read it", p.display()));
                continue;
            };
            let file = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // a script keeps its file name; a subagent or plugin is named by its file's stem
            let name = if kind == "script" { file.clone() } else { file.split('.').next().unwrap_or(&file).to_string() };
            if out.iter().any(|c| c.name == name && c.scope == if project { "project" } else { "user" }) {
                continue;
            }
            let mut c = crate::lockfile::FileComponent {
                name,
                path: portable_path(&p, Some(&root), ctx),
                digest: Some(sha256_bytes(&bytes)),
                scope: if project { "project" } else { "user" }.into(),
                format: spec.and_then(|s| s.format.clone()),
                ..Default::default()
            };
            if !project {
                let rel = format!("{vroot}/{kind}/{file}");
                vendor.push(VendorFile { rel: rel.clone(), bytes, executable: is_executable(&real) });
                c.vendored = Some(rel);
            }
            out.push(c);
        }
        out
    };
    let subagents = component_files(profile.subagents.as_ref(), None, "subagent", &mut vendor);
    let scripts = component_files(None, Some(ctx.cwd.join(crate::components::SCRIPTS_DIR)), "script", &mut vendor);
    let middleware = component_files(profile.middleware.as_ref(), None, "middleware", &mut vendor);

    // ---- config files ----
    let mut configs = Vec::new();
    for (id, spec) in &profile.files {
        let Some(path) = profile.file_path(id, ctx) else { continue };
        let Ok(raw) = std::fs::read(&path) else { continue };
        let project = is_project_path(&path, &root, ctx);
        if !project && !users.allows(Some(&path)) {
            skip("config", 1);
            continue;
        }
        let mut cf = ConfigFile {
            id: id.clone(),
            path: portable_path(&path, Some(&root), ctx),
            format: spec.format.clone(),
            restore: if project { "record".into() } else { spec.restore.clone() },
            pointers: spec.pointers.clone(),
            digest: Some(sha256_bytes(&raw)),
            vendored: None,
            scan: None,
        };
        if cf.restore != "record" {
            match src.file(id) {
                Some(mut v) => {
                    let (mut body, fmt) = if cf.restore == "pointers" {
                        let mut m = Map::new();
                        for p in &spec.pointers {
                            if let Some(sub) = v.pointer(p) {
                                m.insert(p.clone(), sub.clone());
                            }
                        }
                        (Value::Object(m), "json")
                    } else {
                        (std::mem::take(&mut v), if spec.format == "jsonc" { "json" } else { spec.format.as_str() })
                    };
                    // hooks hlock writes for its deciders come from the lock's [[decider]]
                    // entries at sync, not from the vendored config
                    crate::decide::strip_hooks(&mut body);
                    secrets.scrub_value(&mut vec![profile.id.clone(), id.clone()], &mut body, false);
                    // digest what's restored, redacted and parsed, so a sync that reformats the
                    // file or fills in a different secret value isn't drift
                    cf.digest = Some(content_hash(&body));
                    match render_config(&body, fmt) {
                        Ok(text) => {
                            let ext = if cf.restore == "pointers" { "pointers.json".to_string() } else { fmt.to_string() };
                            let rel = format!("{vroot}/config/{id}.{ext}");
                            vendor.push(VendorFile { rel: rel.clone(), bytes: text.into_bytes(), executable: false });
                            cf.vendored = Some(rel);
                        }
                        Err(e) => warnings.push(format!("can't vendor {}: {e}", path.display())),
                    }
                }
                None => warnings.push(format!("can't parse {}; recording its digest only", path.display())),
            }
        }
        configs.push(cf);
    }
    // ---- deciders: a native setting the harness has, and [deciders.*] in harness.toml ----
    let mut deciders = Vec::new();
    for (use_, w) in &profile.deciders {
        let Some(n) = &w.native else { continue };
        let Some(Value::Object(m)) = src.get(&format!("{}#{}", n.file, n.pointer)) else { continue };
        let Some(model) = m.get(&n.model_key).and_then(Value::as_str) else { continue };
        deciders.push(crate::lockfile::Decider {
            use_: use_.clone(),
            provider: n.provider_key.as_ref().and_then(|k| m.get(k)).and_then(Value::as_str).unwrap_or_default().to_string(),
            model: model.to_string(),
            protocol: "systemone".into(),
            endpoint: n.base_url_key.as_ref().and_then(|k| m.get(k)).and_then(Value::as_str).map(str::to_string),
            resolved_from: Some(format!("{}#{}", n.file, n.pointer)),
            via: "native".into(),
            ..Default::default()
        });
    }
    if let Some(mpath) = crate::manifest::find_manifest(&ctx.cwd) {
        match crate::manifest::Manifest::load(&mpath).and_then(|m| Ok((m.deciders()?, m.root()))) {
            Ok((specs, mroot)) => {
                for (use_, spec) in specs.iter().filter(|(u, _)| crate::decide::AGENT_USES.contains(&u.as_str())) {
                    let Some(model) = spec.model.as_deref().filter(|m| *m != "none") else {
                        warnings.push(format!("[deciders.{use_}] has any-of candidates; pin locks a fixed `model` (hlock resolve searches any-of)"));
                        continue;
                    };
                    if deciders.iter().any(|d: &crate::lockfile::Decider| d.use_ == *use_) {
                        continue; // the harness's own setting wins
                    }
                    let Some((via, wiring)) = crate::decide::wiring(use_, &profile.id, Some(profile)) else {
                        warnings.push(format!("{} can't make {use_} decisions; [deciders.{use_}] isn't locked", profile.name));
                        continue;
                    };
                    match crate::decide::lock_entry(use_, model, spec, &via, wiring, &mroot) {
                        Ok(d) => deciders.push(d),
                        Err(e) => warnings.push(e),
                    }
                }
            }
            Err(e) => warnings.push(format!("{}: {e}", mpath.display())),
        }
    }
    // the sandbox the lock runs in, as harness.toml declares it; a bad one stops the pin
    let sandbox = match crate::manifest::find_manifest(&ctx.cwd) {
        Some(mpath) => crate::manifest::Manifest::load(&mpath)?.sandbox_spec()?,
        None => None,
    };
    warnings.extend(src.warnings.borrow().iter().cloned());
    for pat in users.unmatched() {
        warnings.push(format!("--include-user-scope {pat:?} matched nothing in {}'s user config ({})", profile.name, portable_path(&root, None, ctx)));
    }

    // ---- assemble ----
    let config_digest = content_hash(&json!(configs.iter().map(|c| (&c.id, &c.digest)).collect::<Vec<_>>()));
    let mut stack = BTreeMap::new();
    stack.insert("name".into(), json!(project.unwrap_or(&profile.id)));
    stack.insert("harness".into(), json!(profile.id));
    stack.insert("selection".into(), json!("pin"));
    let lock = Lockfile {
        kind: Some("pin".into()),
        stack,
        llm: llms,
        decision: Decision { kind: "none".into(), ..Default::default() },
        decider: deciders,
        harness: Harness {
            name: profile.id.clone(),
            version: probe.version.clone(),
            source: probe.source.clone(),
            config: Some(config_digest),
            ..Default::default()
        },
        tool: tools,
        skill: skills,
        instructions,
        subagent: subagents,
        script: scripts,
        middleware,
        context: Context { isolation: "locked-only".into(), ..Default::default() },
        policy: crate::lockfile::Policy { sandbox: sandbox.map(|s| s.to_map()).unwrap_or_default(), ..Default::default() },
        provenance: Provenance {
            backend: "pin".into(),
            scopes: if *user == UserScope::None { vec!["project".into()] } else { vec!["project".into(), "user".into()] },
            user_include: match user {
                UserScope::Matching(pats) => pats.clone(),
                _ => Vec::new(),
            },
            ..Default::default()
        },
        native: natives,
        runtime: runtimes.into_values().collect(),
        config: configs,
        secret: secrets.refs,
        ..Default::default()
    };
    Ok(Capture { lock, vendor, warnings, skipped })
}

fn vendor_skill(dir: &Path, vroot: &str, name: &str, s: &mut Skill, vendor: &mut Vec<VendorFile>, warnings: &mut Vec<String>) {
    // a plain name (capture checks), and one directory per skill: two skills of the same
    // name in different places get `-2`, `-3`, …
    let base = format!("{vroot}/skills/{name}");
    let taken = |d: &str| vendor.iter().any(|f| f.rel.starts_with(&format!("{d}/")));
    let dest = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|d| !taken(d)).unwrap_or(base);
    match vendor_tree(dir, &dest, vendor, warnings) {
        Ok(()) => s.vendored = Some(dest),
        Err(e) => warnings.push(format!("skill {name}: {e}")),
    }
}

/// A skill's name as the lock records it: its frontmatter `name`, when that's a plain
/// directory name; else the directory's own name. It names the vendored copy and the
/// directory `sync` writes.
fn plain_skill_name(dir: &Path, warnings: &mut Vec<String>) -> String {
    let name = skill_name(dir);
    if crate::fsutil::plain_name(&name) {
        return name;
    }
    let fallback = dir.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| crate::fsutil::plain_name(n)).unwrap_or_else(|| "skill".into());
    warnings.push(format!("skill {name:?} in {}: its name isn't a plain directory name; locked as {fallback:?}", dir.display()));
    fallback
}

/// The raw MCP server entries a harness has configured right now, by name.
pub(crate) fn live_mcp_servers(profile: &HarnessProfile, ctx: &Ctx) -> BTreeMap<String, Value> {
    let src = Sources::new(profile, ctx);
    let mut out = BTreeMap::new();
    for s in &profile.mcp.sources {
        for (k, v) in named_servers(src.get(s)) {
            out.entry(k).or_insert(v);
        }
    }
    out
}

/// MCP servers from a config value: a map by name, or a list of servers that each
/// carry a `name` (ZeroClaw's `[[mcp.servers]]`).
pub(crate) fn named_servers(v: Option<Value>) -> Vec<(String, Value)> {
    match v {
        Some(Value::Object(m)) => m.into_iter().collect(),
        Some(Value::Array(a)) => a.into_iter().filter_map(|v| Some((v.get("name")?.as_str()?.to_string(), v))).collect(),
        _ => Vec::new(),
    }
}

// --------------------------------------------------------------------------- //
// Diff
// --------------------------------------------------------------------------- //

fn index<T, F: Fn(&T) -> String>(items: &[T], key: F) -> BTreeMap<String, &T> {
    items.iter().map(|i| (key(i), i)).collect()
}

fn diff_maps<T, S: Fn(&&T) -> String>(out: &mut Vec<String>, label: &str, old: BTreeMap<String, &T>, new: BTreeMap<String, &T>, sig: S) {
    for (k, n) in &new {
        match old.get(k) {
            None => out.push(format!("+ {label} {k}")),
            Some(o) if sig(o) != sig(n) => out.push(format!("~ {label} {k}: {} → {}", sig(o), sig(n))),
            _ => {}
        }
    }
    for k in old.keys().filter(|k| !new.contains_key(*k)) {
        out.push(format!("- {label} {k}"));
    }
}

fn short(d: &Option<String>) -> String {
    d.as_deref().map(|d| d.trim_start_matches("sha256:").chars().take(10).collect()).unwrap_or_else(|| "—".into())
}

/// Human-readable changes from `old` to `new` (`+` added, `-` removed, `~` changed).
pub fn diff_locks(old: &Lockfile, new: &Lockfile) -> Vec<String> {
    let mut out = Vec::new();
    if old.harness.name != new.harness.name {
        out.push(format!("~ harness: {} → {}", old.harness.name, new.harness.name));
    } else if old.harness.version != new.harness.version || old.harness.source != new.harness.source {
        let v = |h: &Harness| h.source.clone().or(h.version.clone()).unwrap_or_else(|| "—".into());
        out.push(format!("~ harness {}: {} → {}", new.harness.name, v(&old.harness), v(&new.harness)));
    }
    let llm_sig = |l: &&LLM| format!("{}:{}{}", l.provider, l.model, l.endpoint.as_deref().map(|e| format!(" @ {e}")).unwrap_or_default());
    diff_maps(&mut out, "llm", index(&old.llm, |l| l.role.clone()), index(&new.llm, |l| l.role.clone()), llm_sig);
    let decider_sig = |d: &&crate::lockfile::Decider| {
        let events: Vec<&str> = d.wiring.iter().map(|w| w.event.as_str()).collect();
        // a changed policy file is reported by `decide::drift`, with its name
        let may = if d.may.is_empty() { String::new() } else { format!(" may {}", d.may.join(",")) };
        format!("{}:{} via {} {}{may}", d.provider, d.model, d.via, events.join(","))
    };
    diff_maps(&mut out, "decider", index(&old.decider, |d| d.use_.clone()), index(&new.decider, |d| d.use_.clone()), decider_sig);
    let tool_sig = |t: &&Tool| {
        let env: Vec<&String> = t.env.keys().collect();
        format!("{} {} {} {} env={env:?} enabled={:?}", t.transport.as_deref().unwrap_or(""), t.command.as_deref().unwrap_or(""), t.args.join(" "), t.url.as_deref().unwrap_or(""), t.enabled)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    diff_maps(&mut out, "mcp", index(&old.tool, |t| t.name.clone()), index(&new.tool, |t| t.name.clone()), tool_sig);
    let skill_key = |s: &Skill| format!("{} ({})", s.name, s.scope.as_deref().unwrap_or("?"));
    diff_maps(&mut out, "skill", index(&old.skill, skill_key), index(&new.skill, skill_key), |s: &&Skill| short(&s.digest));
    let nat_key = |n: &Native| format!("{} {}", n.kind, n.name);
    let nat_sig = |n: &&Native| {
        let state = if n.enabled { "" } else { " (disabled)" };
        format!("{}{}", n.version.clone().or(n.revision.clone()).unwrap_or_else(|| "—".into()), state)
    };
    diff_maps(&mut out, "plugin", index(&old.native, nat_key), index(&new.native, nat_key), nat_sig);
    diff_maps(&mut out, "runtime", index(&old.runtime, |r| r.name.clone()), index(&new.runtime, |r| r.name.clone()), |r: &&Runtime| {
        r.version.clone().unwrap_or_else(|| "—".into())
    });
    let models = |l: &Lockfile| -> Vec<(String, String)> {
        l.runtime.iter().flat_map(|r| r.model.iter().map(move |m| (format!("{}/{}", r.name, m.id), m.digest.clone().or(m.revision.clone()).unwrap_or_else(|| if m.present { "present".into() } else { "missing".into() })))).collect()
    };
    let (om, nm) = (models(old), models(new));
    diff_maps(&mut out, "model", om.iter().map(|(k, v)| (k.clone(), v)).collect(), nm.iter().map(|(k, v)| (k.clone(), v)).collect(), |v: &&String| short(&Some((*v).clone())));
    let ins_sig = |i: &&Instructions| {
        let role = i.role.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
        let sections = if i.sections.is_empty() { String::new() } else { format!(" [{}]", i.sections.join(", ")) };
        format!("{}{role}{sections}", short(&i.digest))
    };
    diff_maps(&mut out, "instructions", index(&old.instructions, |i| i.name.clone()), index(&new.instructions, |i| i.name.clone()), ins_sig);
    let file_key = |c: &crate::lockfile::FileComponent| format!("{} ({})", c.name, c.scope);
    let file_sig = |c: &&crate::lockfile::FileComponent| short(&c.digest);
    diff_maps(&mut out, "subagent", index(&old.subagent, file_key), index(&new.subagent, file_key), file_sig);
    diff_maps(&mut out, "script", index(&old.script, file_key), index(&new.script, file_key), file_sig);
    diff_maps(&mut out, "middleware", index(&old.middleware, file_key), index(&new.middleware, file_key), file_sig);
    diff_maps(&mut out, "config", index(&old.config, |c| c.path.clone()), index(&new.config, |c| c.path.clone()), |c: &&ConfigFile| short(&c.digest));
    out.extend(diff_sandbox(old, new));
    out
}

/// The sandbox's change, marked `weakened` when it protects less than before.
fn diff_sandbox(old: &Lockfile, new: &Lockfile) -> Vec<String> {
    let (o, n) = (crate::sandbox::Spec::from_lock(old).ok().flatten(), crate::sandbox::Spec::from_lock(new).ok().flatten());
    if o == n {
        return Vec::new();
    }
    let show = |s: &crate::sandbox::Spec| {
        let mut t = s.summary();
        if let Some(i) = &s.image {
            t.push_str(&format!(" {i}"));
        }
        t
    };
    let line = match (&o, &n) {
        (None, Some(b)) => format!("+ sandbox {}", show(b)),
        (Some(a), None) => format!("- sandbox {}", show(a)),
        (Some(a), Some(b)) => format!("~ sandbox: {} → {}", show(a), show(b)),
        (None, None) => return Vec::new(),
    };
    let weaker = crate::sandbox::weakened(o.as_ref(), n.as_ref());
    if weaker.is_empty() {
        vec![line]
    } else {
        vec![format!("{line}  weakened: {}", weaker.join("; "))]
    }
}

// --------------------------------------------------------------------------- //
// pin / check
// --------------------------------------------------------------------------- //

/// The vendor namespace for a lock file, so two locks in one directory never share
/// vendored files: `harness.lock` has none; `harness.<name>.lock` and `<name>.lock`
/// vendor into `.hlock/vendor/<name>/<harness>/`.
pub fn vendor_namespace(lock: &Path) -> Option<String> {
    let name = lock.file_name()?.to_str()?;
    let stem = name.strip_suffix(".lock").unwrap_or(name);
    let ns = stem.strip_prefix("harness.").unwrap_or(stem);
    (stem != "harness" && !ns.is_empty()).then(|| ns.to_string())
}

/// Move a capture's vendored paths from `.hlock/vendor/<id>/` to `.hlock/vendor/<ns>/<id>/`.
fn relocate_vendored(cap: &mut Capture, id: &str, ns: &str) {
    let from = format!("{VENDOR_DIR}/{id}/");
    let to = format!("{VENDOR_DIR}/{ns}/{id}/");
    let mv = |p: &mut String| {
        if let Some(rest) = p.strip_prefix(&from) {
            *p = format!("{to}{rest}");
        }
    };
    cap.vendor.iter_mut().for_each(|f| mv(&mut f.rel));
    cap.lock.skill.iter_mut().filter_map(|s| s.vendored.as_mut()).for_each(mv);
    cap.lock.instructions.iter_mut().filter_map(|i| i.vendored.as_mut()).for_each(mv);
    cap.lock.config.iter_mut().filter_map(|c| c.vendored.as_mut()).for_each(mv);
    let lock = &mut cap.lock;
    lock.subagent.iter_mut().chain(lock.script.iter_mut()).chain(lock.middleware.iter_mut()).filter_map(|c| c.vendored.as_mut()).for_each(mv);
}

/// Replace the vendor directory `vdir` with `files` (each `rel` is `prefix` and a path
/// inside `vdir`): they're written into a sibling directory first, which then takes the
/// old one's place, so a failed pin leaves the previous vendored files as they were.
/// With nothing to vendor, `vdir` is removed and nothing is created.
fn write_vendor_dir(vdir: &Path, prefix: &str, files: &[VendorFile]) -> Result<()> {
    if files.is_empty() {
        let _ = std::fs::remove_dir_all(vdir);
        return Ok(());
    }
    let parent = vdir.parent().ok_or_else(|| Error::Lockfile(format!("{} has no parent directory", vdir.display())))?;
    std::fs::create_dir_all(parent)?;
    let name = vdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let new = parent.join(format!(".{name}.hlock-new-{}", std::process::id()));
    let old = parent.join(format!(".{name}.hlock-old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&new);
    let written = (|| -> Result<()> {
        std::fs::create_dir(&new)?;
        for f in files {
            let rel = f.rel.strip_prefix(prefix).filter(|r| crate::fsutil::safe_rel(r)).ok_or_else(|| Error::Lockfile(format!("vendored path {:?} isn't under {prefix}", f.rel)))?;
            let dest = new.join(rel);
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&dest, &f.bytes)?;
            #[cfg(unix)]
            if f.executable {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
            }
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(&new);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&old);
    let had_old = std::fs::symlink_metadata(vdir).is_ok();
    if had_old {
        std::fs::rename(vdir, &old)?;
    }
    if let Err(e) = std::fs::rename(&new, vdir) {
        if had_old {
            let _ = std::fs::rename(&old, vdir);
        }
        let _ = std::fs::remove_dir_all(&new);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// What [`pin`] did.
#[derive(Debug, Clone)]
pub struct PinOutcome {
    pub lock: Lockfile,
    pub path: PathBuf,
    pub selection: crate::profiles::Selection,
    /// Changes from the previous pinned lock at `path` (empty for a first pin).
    pub diff: Vec<String>,
    pub previous: bool,
    pub vendored: Vec<String>,
    pub warnings: Vec<String>,
    /// User-level items left out of a project-only pin, by kind.
    pub skipped: BTreeMap<String, usize>,
}

/// Capture the target harness and write a pinned lock to `out`, vendoring into
/// `<out dir>/.hlock/vendor/<harness>/` unless `vendor` is false. Captures the
/// project's setup, plus the user-level config root when `user` is set (see
/// [`capture`]). Refuses to replace a resolved lock (one with evidence) unless `force`.
pub fn pin(
    reg: &Registry,
    ctx: &Ctx,
    harness: Option<&str>,
    manifest_target: Option<&str>,
    project: Option<&str>,
    out: &Path,
    vendor: bool,
    force: bool,
    user: UserScope,
) -> Result<PinOutcome> {
    let selection = reg.select(harness, manifest_target, ctx)?;
    let profile = reg.require_harness(&selection.id)?;
    // almost everything a desktop app reads is user-level: pin all of it unless told what
    let user = if profile.is_app() && user == UserScope::None { UserScope::All } else { user };
    let previous = if out.exists() {
        match Lockfile::load(out) {
            Ok(l) if !l.is_pin() && l.evidence.trials > 0 && !force => {
                return Err(Error::Lockfile(format!(
                    "{} is a resolved lock with evidence; pass --force to replace it, or pin to another file with -o",
                    out.display()
                )))
            }
            Ok(l) => Some(l),
            Err(e) if !force => return Err(Error::Lockfile(format!("{e} (pass --force to replace it)"))),
            Err(_) => None,
        }
    } else {
        None
    };
    let mut cap = capture(profile, reg, ctx, project, &user)?;
    // declared dependencies stay declared (and fetched, not vendored)
    let lock_dir = out.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| ctx.cwd.clone());
    crate::deps::carry_declared(previous.as_ref(), &mut cap, &lock_dir);
    // what each entry would place, scanned for characters that hide text from a reviewer
    let scan_warnings = crate::audit::scan_capture(&mut cap.lock, profile, ctx, &cap.vendor);
    cap.warnings.extend(scan_warnings);
    // the user scope doesn't carry over from the previous pin; say so when it changes
    if let Some(prev) = previous.as_ref().filter(|p| p.is_pin()).map(UserScope::of) {
        if prev != UserScope::None && prev != user {
            cap.warnings.push(format!("the previous pin included user-level setup; pass {} to keep it", prev.flags()));
        }
    }
    let ns = vendor_namespace(out);
    if let Some(ns) = &ns {
        relocate_vendored(&mut cap, &profile.id, ns);
    }
    let base = out.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| ctx.cwd.clone());
    let mut vendored = Vec::new();
    if vendor {
        let vdir = match &ns {
            Some(ns) => base.join(VENDOR_DIR).join(ns).join(&profile.id),
            None => base.join(VENDOR_DIR).join(&profile.id),
        };
        let prefix = match &ns {
            Some(ns) => format!("{VENDOR_DIR}/{ns}/{}/", profile.id),
            None => format!("{VENDOR_DIR}/{}/", profile.id),
        };
        write_vendor_dir(&vdir, &prefix, &cap.vendor)?;
        vendored.extend(cap.vendor.iter().map(|f| f.rel.clone()));
    } else {
        for s in cap.lock.skill.iter_mut() {
            s.vendored = None;
        }
        for i in cap.lock.instructions.iter_mut() {
            i.vendored = None;
        }
        for c in cap.lock.config.iter_mut() {
            c.vendored = None;
        }
        let lock = &mut cap.lock;
        for c in lock.subagent.iter_mut().chain(lock.script.iter_mut()).chain(lock.middleware.iter_mut()) {
            c.vendored = None;
        }
    }
    // the project's policy: a denied source, server, provider or harness stops the pin
    let policy = crate::policy::Policy::for_dir(&base)?;
    let policy_warnings = policy.enforce(policy.check_lock(&cap.lock), &format!("the {} setup", profile.name))?;
    cap.warnings.extend(policy_warnings);
    let diff = previous.as_ref().filter(|p| p.is_pin()).map(|p| diff_locks(p, &cap.lock)).unwrap_or_default();
    cap.lock.save(out)?;
    // what it captured already runs on this machine: approve it, so syncing here never asks
    let mut store = crate::trust::TrustStore::load(ctx)?;
    if store.approve(&crate::trust::executables(&cap.lock, &base, Some(profile), ctx)) > 0 {
        store.save(ctx)?;
    }
    Ok(PinOutcome {
        lock: cap.lock,
        path: out.to_path_buf(),
        selection,
        diff,
        previous: previous.is_some_and(|p| p.is_pin()),
        vendored,
        warnings: cap.warnings,
        skipped: cap.skipped,
    })
}

/// Re-capture the harness a pinned lock is for and list what drifted.
pub fn check_pin(lock: &Lockfile, reg: &Registry, ctx: &Ctx) -> Result<Vec<String>> {
    check_pin_at(lock, reg, ctx, None)
}

/// [`check_pin`] against the isolated root `hlock sync --prefix <prefix>` writes, or
/// the user's real config when `prefix` is `None`.
pub fn check_pin_at(lock: &Lockfile, reg: &Registry, ctx: &Ctx, prefix: Option<&Path>) -> Result<Vec<String>> {
    let profile = reg.require_harness(&lock.harness.name)?;
    let project = lock.stack.get("name").and_then(Value::as_str);
    // re-capture what the pin captured, so a project-only lock ignores your user config
    let user = UserScope::of(lock);
    // declared dependencies are compared by `deps::check`, which says how they differ
    let Some(prefix) = prefix else {
        let mut cap = capture(profile, reg, ctx, project, &user)?;
        crate::deps::carry_from(lock, &mut cap);
        keep_app_version(profile, lock, &mut cap.lock);
        return Ok(diff_locks(lock, &cap.lock));
    };
    let target = crate::sync::target_ctx(profile, ctx, &crate::sync::SyncOptions { prefix: Some(prefix.to_path_buf()), ..Default::default() }).0;
    let mut cap = capture(profile, reg, &target, project, &user)?;
    crate::deps::carry_from(lock, &mut cap);
    // a file outside the root normally (`~/.claude.json`) lives inside an isolated one:
    // the same file, so compare it under the lock's path
    for c in cap.lock.config.iter_mut() {
        if let Some(locked) = lock.config.iter().find(|l| l.id == c.id) {
            c.path = locked.path.clone();
        }
    }
    keep_app_version(profile, lock, &mut cap.lock);
    Ok(diff_locks(lock, &cap.lock))
}

/// A desktop app updates itself, so a newer installed version isn't drift: keep the
/// locked version (and the source that carries it) when the install method is the same.
fn keep_app_version(profile: &HarnessProfile, lock: &Lockfile, cap: &mut Lockfile) {
    if !profile.is_app() || cap.harness.version.is_none() {
        return;
    }
    let method = |s: &Option<String>| s.as_deref().map(|s| s.split([':', '@']).next().unwrap_or(s).to_string());
    if method(&lock.harness.source) == method(&cap.harness.source) {
        cap.harness.version = lock.harness.version.clone();
        cap.harness.source = lock.harness.source.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_updating_itself_isnt_drift() {
        let reg = Registry::builtin();
        let lock = |v: &str, src: &str| Lockfile { harness: crate::lockfile::Harness { name: "claude-desktop".into(), version: Some(v.into()), source: Some(src.into()), ..Default::default() }, ..Default::default() };
        let pinned = lock("1.0.3", "app@1.0.3");
        let mut now = lock("1.0.4", "app@1.0.4");
        keep_app_version(reg.harness("claude-desktop").unwrap(), &pinned, &mut now);
        assert!(diff_locks(&pinned, &now).is_empty());
        // a changed install method is still reported
        let mut now = lock("1.0.4", "brew-cask:claude@1.0.4");
        keep_app_version(reg.harness("claude-desktop").unwrap(), &pinned, &mut now);
        assert!(!diff_locks(&pinned, &now).is_empty());
        // and a CLI's version is drift as before
        let cli = |v: &str| Lockfile { harness: crate::lockfile::Harness { name: "claude-code".into(), version: Some(v.into()), ..Default::default() }, ..Default::default() };
        let mut now = cli("2.1.1");
        keep_app_version(reg.harness("claude-code").unwrap(), &cli("2.1.0"), &mut now);
        assert!(!diff_locks(&cli("2.1.0"), &now).is_empty());
    }

    #[test]
    fn globs_stay_within_a_segment_unless_doubled() {
        let m = |p: &str, t: &str| glob_match(p.as_bytes(), t.as_bytes());
        assert!(m("/h/.claude/skills/review-*", "/h/.claude/skills/review-pr"));
        assert!(!m("/h/.claude/skills/*", "/h/.claude/skills/a/b"));
        assert!(m("/h/.claude/skills/**", "/h/.claude/skills/a/b"));
        assert!(m("/h/**/SKILL.md", "/h/SKILL.md") && m("/h/**/SKILL.md", "/h/a/b/SKILL.md"));
        assert!(m("/h/.claude/CLAUDE.?d", "/h/.claude/CLAUDE.md") && !m("/h/x?y", "/h/x/y"));
        assert!(!m("/h/.claude/skills/review-*", "/h/.claude/skills/write-tests"));
    }

    #[test]
    fn user_scope_patterns_resolve_against_the_config_root() {
        let ctx = Ctx { home: "/h".into(), cwd: "/work".into(), env: BTreeMap::new(), path: Vec::new() };
        let root = Path::new("/h/.claude");
        assert_eq!(expand_pattern("skills/review-*", root, &ctx), "/h/.claude/skills/review-*");
        assert_eq!(expand_pattern("{root}/CLAUDE.md", root, &ctx), "/h/.claude/CLAUDE.md");
        assert_eq!(expand_pattern("~/.claude.json", root, &ctx), "/h/.claude.json");
        assert_eq!(expand_pattern("/opt/skills/", root, &ctx), "/opt/skills");

        let scope = UserScope::Matching(vec!["skills".into(), "nothing-*".into()]);
        let f = UserFilter::new(&scope, root, &ctx);
        // a directory pattern takes what's under it
        assert!(f.allows(Some(Path::new("/h/.claude/skills/review-pr"))));
        assert!(!f.allows(Some(Path::new("/h/.claude/CLAUDE.md"))));
        assert_eq!(f.unmatched(), vec!["nothing-*".to_string()]);
        assert_eq!(scope.flags(), "-i skills -i 'nothing-*'");
    }

    #[test]
    fn placeholder_keys_stay() {
        assert!(is_placeholder_key("local") && is_placeholder_key("EMPTY") && !is_placeholder_key("sk-ant-abc123"));
        let mut s = Secrets::default();
        assert_eq!(s.scrub(&["pi", "models"], "apiKey", "local", false, "models#/apiKey"), "local");
        assert_ne!(s.scrub(&["pi", "models"], "apiKey", "sk-ant-0123456789abcdef", false, "models#/apiKey"), "sk-ant-0123456789abcdef");
    }

    #[test]
    fn vendor_namespaces() {
        assert_eq!(vendor_namespace(Path::new("harness.lock")), None);
        assert_eq!(vendor_namespace(Path::new("/p/harness.meta.lock")).as_deref(), Some("meta"));
        assert_eq!(vendor_namespace(Path::new("pinned.lock")).as_deref(), Some("pinned"));
    }

    #[test]
    fn jsonc() {
        let t = "{\n // c\n \"a\": \"http://x//y\", /* b */ \"b\": [1, 2,],\n}";
        let v = parse_config(t, "jsonc").unwrap();
        assert_eq!(v["a"], "http://x//y");
        assert_eq!(v["b"], json!([1, 2]));
    }

    #[test]
    fn json5() {
        let t = r#"{
          // c
          agents: { defaults: { model: { primary: 'anthropic/claude-opus-4-6' }, heartbeat: { every: "30m" } } },
          mcp: { servers: { fetch: { command: "uvx", args: ['mcp-server-fetch'], }, /* off */ }, },
          'quoted': 'it\'s "fine" // not a comment',
          $x_1: [0x1F, +2, 1.5e3, -1e-2, true, null],
        }"#;
        let v = parse_config(t, "json5").unwrap();
        assert_eq!(v.pointer("/agents/defaults/model/primary"), Some(&json!("anthropic/claude-opus-4-6")));
        assert_eq!(v.pointer("/agents/defaults/heartbeat/every"), Some(&json!("30m")));
        assert_eq!(v.pointer("/mcp/servers/fetch/args/0"), Some(&json!("mcp-server-fetch")));
        assert_eq!(v["quoted"], json!("it's \"fine\" // not a comment"));
        assert_eq!(v["$x_1"], json!([31, 2, 1500.0, -0.01, true, null]));
        assert_eq!(parse_config("", "json5").unwrap(), json!({}));
        // escapes, line continuations and numbers JSON lacks
        let v = parse_config("{a: 'x\\x41\\0\\v\ty', b: 'one \\\r\ntwo', c: [.5, 5., -.25, +.5]}", "json5").unwrap();
        assert_eq!(v["a"], json!("xA\u{0}\u{b}\ty"));
        assert_eq!(v["b"], json!("one two"));
        assert_eq!(v["c"], json!([0.5, 5.0, -0.25, 0.5]));
        // written back as JSON, which reads as JSON5
        assert_eq!(parse_config(&render_config(&v, "json5").unwrap(), "json5").unwrap(), v);
    }

    #[test]
    fn yaml_and_toml_round_trip() {
        let y = parse_config("model:\n  default: qwen3\n  provider: ollama\nmcp_servers:\n  fs:\n    command: uvx\n", "yaml").unwrap();
        assert_eq!(y.pointer("/model/default"), Some(&json!("qwen3")));
        assert_eq!(parse_config(&render_config(&y, "yaml").unwrap(), "yaml").unwrap(), y);
        let t = parse_config("model = \"gpt-5\"\n[mcp_servers.gh]\ncommand = \"npx\"\n", "toml").unwrap();
        assert_eq!(t.pointer("/mcp_servers/gh/command"), Some(&json!("npx")));
        assert_eq!(parse_config(&render_config(&t, "toml").unwrap(), "toml").unwrap(), t);
        assert_eq!(parse_config("", "yaml").unwrap(), json!({}));
    }

    #[test]
    fn secret_detection() {
        for k in ["apiKey", "api_key", "OPENAI_API_KEY", "Authorization", "x-api-key", "token", "client_secret", "password"] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in ["keybindings", "maxTokens", "model", "baseURL", "theme", "command"] {
            assert!(!is_secret_key(k), "{k}");
        }
        assert!(looks_secret("sk-ant-api03-abcdefghijklmnop"));
        assert!(looks_secret("ghp_0123456789abcdefABCDEF0123456789"));
        assert!(looks_secret("aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5"));
        assert!(!looks_secret("3b1f4e4c0e6aac15f2fd7f55e151bda9b1f4e4c0")); // a git sha
        assert!(!looks_secret("6f1d2c1a-2b3c-4d5e-8f90-1a2b3c4d5e6f")); // a uuid
        assert!(!looks_secret("http://localhost:1234/v1"));
        assert!(!looks_secret("${GITHUB_TOKEN}"));
    }

    #[test]
    fn redact_free_text() {
        let t = "use ghp_0123456789abcdefABCDEF0123456789, or export OPENAI_API_KEY=sk-live-123456789012\nthen run `make`";
        let r = redact_text(t);
        assert_eq!(r, "use [redacted], or export OPENAI_API_KEY=[redacted]\nthen run `make`");
        let plain = "fix the bug in src/auth/login.py (see https://example.com/issues/42)";
        assert_eq!(redact_text(plain), plain);
    }

    #[test]
    fn scrub_redacts_and_reuses_placeholders() {
        let mut s = Secrets::default();
        let mut v = json!({
            "provider": {"vllm": {"options": {"baseURL": "http://localhost:8000/v1", "apiKey": "sk-live-123456789012"}}},
            "mcp": {"composio": {"url": "https://mcp.example.com/x?api_key=abcd1234efgh&user=me", "headers": {"Authorization": "Bearer tok_abcdefghijkl"}}},
            "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}", "NODE_ENV": "production"},
            "again": {"apiKey": "sk-live-123456789012"},
            "auth": {"email": "me@example.com"},
            "qwen-token-plan": {"name": "Qwen plan", "models": [{"id": "qwen3-coder"}]},
        });
        s.scrub_value(&mut vec!["pi".into(), "settings".into()], &mut v, false);
        let text = v.to_string();
        assert!(!text.contains("sk-live") && !text.contains("tok_abc") && !text.contains("abcd1234efgh") && !text.contains("me@example.com"), "{text}");
        assert_eq!(v["provider"]["vllm"]["options"]["baseURL"], "http://localhost:8000/v1");
        assert_eq!(v["env"]["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
        assert_eq!(v["env"]["NODE_ENV"], "production");
        assert_eq!(v["again"]["apiKey"], v["provider"]["vllm"]["options"]["apiKey"]);
        assert!(v["mcp"]["composio"]["url"].as_str().unwrap().contains("user=me"));
        assert_eq!(v["qwen-token-plan"]["models"][0]["id"], "qwen3-coder");
        assert_eq!(s.refs.len(), 4);
        assert!(s.refs.iter().all(|r| r.name.starts_with("HLOCK_SECRET_")));
    }

    #[test]
    fn mcp_shapes() {
        let mut s = Secrets::default();
        let claude = normalize_mcp("gh", &json!({"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "ghp_0123456789abcdefABCDEF0123456789"}}), "user", &mut s).unwrap();
        assert_eq!(claude.transport.as_deref(), Some("stdio"));
        assert_eq!(claude.source.as_deref(), Some("npm:@modelcontextprotocol/server-github"));
        assert!(claude.env["GITHUB_TOKEN"].starts_with("${HLOCK_SECRET_GH_GITHUB_TOKEN"));
        let oc = normalize_mcp("fs", &json!({"type": "local", "command": ["uvx", "mcp-server-fs"], "enabled": false}), "user", &mut s).unwrap();
        assert_eq!((oc.command.as_deref(), oc.args.clone(), oc.enabled), (Some("uvx"), vec!["mcp-server-fs".to_string()], Some(false)));
        assert_eq!(oc.source.as_deref(), Some("pypi:mcp-server-fs"));
        let remote = normalize_mcp("flyte", &json!({"type": "remote", "url": "https://x.dev/mcp", "headers": {"Authorization": "Bearer abcdefghijklmnop"}}), "user", &mut s).unwrap();
        assert_eq!(remote.transport.as_deref(), Some("http"));
        assert!(remote.headers["Authorization"].starts_with("${HLOCK_SECRET_FLYTE_HEADER_AUTHORIZATION"));
        let docker = normalize_mcp("d", &json!({"command": "docker", "args": ["run", "-i", "--rm", "-e", "X", "ghcr.io/a/b:1"]}), "user", &mut s).unwrap();
        assert_eq!(docker.source.as_deref(), Some("oci:ghcr.io/a/b:1"));
        // Zed's older forms nest the command, with or without `source`
        for v in [json!({"command": {"path": "npx", "args": ["-y", "pkg"], "env": {"A": "b"}}}), json!({"source": "custom", "command": {"path": "npx", "args": ["-y", "pkg"], "env": {"A": "b"}}})] {
            let zed = normalize_mcp("z", &v, "user", &mut s).unwrap();
            assert_eq!((zed.command.as_deref(), zed.args.clone(), zed.env["A"].as_str()), (Some("npx"), vec!["-y".to_string(), "pkg".to_string()], "b"));
        }
        let vscode = normalize_mcp("v", &json!({"type": "stdio", "command": "uvx", "args": ["x"]}), "user", &mut s).unwrap();
        assert_eq!((vscode.transport.as_deref(), vscode.command.as_deref()), (Some("stdio"), Some("uvx")));
    }

    #[test]
    fn runtime_matching() {
        let r = Registry::builtin();
        let m = |p: &str, u: Option<&str>| match_runtime(&r, Some(p), u).map(|(rt, loc)| (rt.id.clone(), loc));
        assert_eq!(m("lmstudio", Some("http://localhost:1234/v1")), Some(("lmstudio".into(), "local")));
        assert_eq!(m("my-local", Some("http://127.0.0.1:11434")), Some(("ollama".into(), "local")));
        assert_eq!(m("ollama", None), Some(("ollama".into(), "local")));
        assert_eq!(m("vllm", Some("http://10.0.0.5:8000/v1")), Some(("vllm".into(), "network")));
        assert_eq!(m("box", Some("http://100.70.1.2:1234/v1")), Some(("lmstudio".into(), "network")));
        assert_eq!(m("vllm", Some("https://api.example.com/v1")), None);
        assert_eq!(m("anthropic", None), None);
    }

    #[test]
    fn npm_specs() {
        assert_eq!(npm_name_version("npm:pi-free"), ("pi-free".into(), None));
        assert_eq!(npm_name_version("@scope/pkg@1.2.3"), ("@scope/pkg".into(), Some("1.2.3".into())));
        assert_eq!(launcher_source("npx", &["-y".into(), "pkg@2".into()]).as_deref(), Some("npm:pkg@2"));
    }

    #[test]
    fn recorded_urls_carry_no_credentials() {
        assert_eq!(strip_userinfo("https://ghp_x@github.com/a/b"), "https://github.com/a/b");
        assert_eq!(strip_userinfo("https://user:pass@gitlab.com/a/b.git"), "https://gitlab.com/a/b.git");
        assert_eq!(strip_userinfo("https://x-access-token:ghs_abc@github.com/a/b?private_token=zzz&ref=main"), "https://github.com/a/b?ref=main");
        // an ssh user name is a name, not a credential; its password isn't kept
        assert_eq!(strip_userinfo("ssh://git@github.com/a/b"), "ssh://git@github.com/a/b");
        assert_eq!(strip_userinfo("ssh://me:pw@host/a"), "ssh://me@host/a");
        assert_eq!(strip_userinfo("git@github.com:a/b.git"), "git@github.com:a/b.git");
        let rt = Registry::builtin();
        let ctx = Ctx { home: scratch("rt"), cwd: scratch("rt-cwd"), env: BTreeMap::new(), path: vec![] };
        let r = new_runtime(rt.runtime("vllm").unwrap(), "network", Some("http://admin:pw@10.0.0.5:8000/v1".into()), &ctx);
        assert_eq!(r.endpoint.as_deref(), Some("http://10.0.0.5:8000/v1"));
        // in a config value, a token alone is redacted to a placeholder like user:pass
        let mut s = Secrets::default();
        let v = s.scrub(&["mcp", "x"], "url", "https://ghp_x@github.com/a/b", false, "mcp x url");
        assert!(!v.contains("ghp_x") && v.starts_with("https://${HLOCK_SECRET_") && v.ends_with("@github.com/a/b"), "{v}");
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-capture-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    #[test]
    fn skills_with_symlinks_are_walked_without_following_them() {
        let d = scratch("links");
        let sk = d.join("skill");
        std::fs::create_dir_all(sk.join("sub")).unwrap();
        std::fs::write(sk.join("SKILL.md"), "---\nname: s\n---\n").unwrap();
        std::fs::write(d.join("id_rsa"), "PRIVATE").unwrap();
        std::os::unix::fs::symlink(&sk, sk.join("sub/loop")).unwrap(); // a loop
        std::os::unix::fs::symlink(d.join("id_rsa"), sk.join("key")).unwrap(); // a file outside
        assert_eq!(walk_files(&sk), [PathBuf::from("SKILL.md")]);
        let digest = tree_digest(&sk);
        let (mut out, mut warnings) = (Vec::new(), Vec::new());
        vendor_tree(&sk, ".hlock/vendor/x/skills/s", &mut out, &mut warnings).unwrap();
        assert_eq!(out.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), [".hlock/vendor/x/skills/s/SKILL.md"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        // the same tree without the links has the same digest
        std::fs::remove_file(sk.join("key")).unwrap();
        std::fs::remove_file(sk.join("sub/loop")).unwrap();
        assert_eq!(tree_digest(&sk), digest);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_files_are_never_vendored_or_digested_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("unreadable");
        std::fs::write(d.join("a"), "").unwrap();
        let empty = tree_digest(&d);
        std::fs::set_permissions(d.join("a"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(d.join("a")).is_ok() {
            return; // running as root: everything is readable
        }
        let (digest, unreadable) = tree_digest_checked(&d);
        assert_ne!(digest, empty);
        assert_eq!(unreadable.len(), 1);
        let (mut out, mut warnings) = (Vec::new(), Vec::new());
        assert!(vendor_tree(&d, "v", &mut out, &mut warnings).unwrap_err().contains("can't read"));
        assert!(out.is_empty());
        std::fs::set_permissions(d.join("a"), std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn skill_and_instruction_vendor_names_never_escape_or_collide() {
        let d = scratch("names");
        let sk = d.join("my-skill");
        std::fs::create_dir_all(&sk).unwrap();
        std::fs::write(sk.join("SKILL.md"), "---\nname: ../../../.ssh\n---\n").unwrap();
        let mut warnings = Vec::new();
        assert_eq!(plain_skill_name(&sk, &mut warnings), "my-skill");
        assert_eq!(warnings.len(), 1);
        // two skills of one name get two directories
        let (mut vendor, mut s) = (Vec::new(), Skill::default());
        vendor_skill(&sk, "v", "my-skill", &mut s, &mut vendor, &mut warnings);
        vendor_skill(&sk, "v", "my-skill", &mut s, &mut vendor, &mut warnings);
        assert_eq!(s.vendored.as_deref(), Some("v/skills/my-skill-2"));
        assert_eq!(vendor.len(), 2);

        // two user instruction files called AGENTS.md
        let home = d.join("home");
        for sub in ["a", "b"] {
            std::fs::create_dir_all(home.join(sub)).unwrap();
            std::fs::write(home.join(sub).join("AGENTS.md"), sub).unwrap();
        }
        let ctx = Ctx { home: home.clone(), cwd: d.join("proj"), env: BTreeMap::new(), path: vec![] };
        let profile = HarnessProfile {
            id: "t".into(),
            name: "T".into(),
            instructions: vec!["~/a/AGENTS.md".into(), "~/b/AGENTS.md".into()],
            config_root: crate::profiles::ConfigRoot { default: "~/.t".into(), ..Default::default() },
            ..Default::default()
        };
        let cap = capture(&profile, &Registry::default(), &ctx, None, &UserScope::All).unwrap();
        let rels: Vec<&str> = cap.lock.instructions.iter().filter_map(|i| i.vendored.as_deref()).collect();
        assert_eq!(rels, [".hlock/vendor/t/instructions/AGENTS.md", ".hlock/vendor/t/instructions/AGENTS-2.md"]);
        assert_eq!(cap.vendor.iter().map(|f| f.bytes.clone()).collect::<Vec<_>>(), [b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn the_vendor_dir_is_replaced_whole() {
        let d = scratch("swap");
        let vdir = d.join(".hlock/vendor/t");
        std::fs::create_dir_all(vdir.join("skills/old")).unwrap();
        std::fs::write(vdir.join("skills/old/SKILL.md"), "old").unwrap();
        let file = |rel: &str| VendorFile { rel: rel.into(), bytes: b"new".to_vec(), executable: false };
        // a file outside the prefix fails the write, and the old directory stays
        assert!(write_vendor_dir(&vdir, ".hlock/vendor/t/", &[file(".hlock/vendor/t/a"), file(".hlock/vendor/t/../../x")]).is_err());
        assert!(vdir.join("skills/old/SKILL.md").is_file());
        write_vendor_dir(&vdir, ".hlock/vendor/t/", &[file(".hlock/vendor/t/skills/new/SKILL.md")]).unwrap();
        assert!(!vdir.join("skills/old").exists() && vdir.join("skills/new/SKILL.md").is_file());
        // nothing is left beside it
        assert_eq!(std::fs::read_dir(d.join(".hlock/vendor")).unwrap().count(), 1);
        // nothing to vendor removes it
        write_vendor_dir(&vdir, ".hlock/vendor/t/", &[]).unwrap();
        assert!(!vdir.exists());
    }
}
