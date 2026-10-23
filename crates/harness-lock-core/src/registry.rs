//! The registry client: `hlock login`, `logout`, `whoami`, `push`, `search`, and
//! `hlock pull NS/NAME`, over the hlock registry HTTP API v1 (`specs/registry.md`).
//!
//! A registry is any server that implements that API; hlock has no default one. Its URL
//! comes from `--registry`, else `HLOCK_REGISTRY`, else `[registry] url` in
//! `~/.hlock/config.toml` (written by `login`). Its token comes from
//! `HLOCK_REGISTRY_TOKEN`, else the OS keychain, else `~/.hlock/credentials.toml`.
//!
//! A setup travels as a deterministic `tar.gz`: `harness.toml`, `harness.lock` and the
//! files it vendors, `apm.yml`, `apm.lock.yaml` and `README.md`, sorted, with zero times
//! and owners, so the same files always give the same digest.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::fsutil::safe_rel;
use crate::lockfile::Lockfile;
use crate::pull::{self, Fetched, Source, Target, SETUP_FILES};

/// The API prefix every endpoint is under.
pub const API: &str = "/api/v1";
/// The largest archive `push` sends and `pull` accepts (compressed).
pub const MAX_ARCHIVE: usize = 4 << 20;
/// The most entries an archive may have.
pub const MAX_ENTRIES: usize = 500;
/// The most an archive may unpack to.
const MAX_UNPACKED: u64 = 64 << 20;
/// The environment variable holding a token for CI.
pub const TOKEN_ENV: &str = "HLOCK_REGISTRY_TOKEN";
/// The environment variable naming the registry.
pub const URL_ENV: &str = "HLOCK_REGISTRY";

fn err(msg: impl Into<String>) -> Error {
    Error::Registry(msg.into())
}

// --------------------------------------------------------------------------- //
// configuration: the registry URL and its token
// --------------------------------------------------------------------------- //

/// `~/.hlock/config.toml` (or under `HLOCK_HOME`).
pub fn config_path() -> PathBuf {
    pull::home_dir().join("config.toml")
}

/// `~/.hlock/credentials.toml`: tokens, when there's no keychain.
pub fn credentials_path() -> PathBuf {
    pull::home_dir().join("credentials.toml")
}

/// A registry URL as hlock keeps it: `http(s)://host[/path]` without a trailing `/`.
/// Plain `http` is accepted only for loopback hosts, since a token is sent with requests.
pub fn normalize_url(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")).ok_or_else(|| err(format!("{u:?} isn't a registry URL: it must start with https://")))?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains('@') || u.chars().any(|c| c.is_whitespace() || c.is_control()) || u.contains(['?', '#']) {
        return Err(err(format!("{u:?} isn't a registry URL")));
    }
    let hostname = host.rsplit_once(':').map_or(host, |(h, _)| h);
    if u.starts_with("http://") && !matches!(hostname, "localhost" | "127.0.0.1" | "[::1]") {
        return Err(err(format!("{u:?} uses plain http; a registry needs https (http is allowed only for localhost)")));
    }
    Ok(u.to_string())
}

fn read_toml(p: &Path) -> toml_edit::DocumentMut {
    std::fs::read_to_string(p).ok().and_then(|t| t.parse().ok()).unwrap_or_default()
}

/// The registry URL in `~/.hlock/config.toml`, if `login` wrote one.
pub fn configured_url() -> Option<String> {
    url_in(&config_path())
}

fn url_in(config: &Path) -> Option<String> {
    read_toml(config).get("registry")?.get("url")?.as_str().map(str::to_string)
}

/// The registry to use: `flag`, else `HLOCK_REGISTRY`, else the one `login` saved.
pub fn resolve_url(flag: Option<&str>) -> Result<String> {
    let env = std::env::var(URL_ENV).ok().filter(|v| !v.trim().is_empty());
    match flag.map(str::to_string).or(env).or_else(configured_url) {
        Some(u) => normalize_url(&u),
        None => Err(err(format!("no registry configured: run `hlock login URL`, or set {URL_ENV}"))),
    }
}

/// Save `url` as the registry in `~/.hlock/config.toml`, keeping the rest of the file.
pub fn save_url(url: &str) -> Result<()> {
    save_url_in(&config_path(), url)
}

fn save_url_in(p: &Path, url: &str) -> Result<()> {
    let p = p.to_path_buf();
    let mut doc = read_toml(&p);
    if !doc.contains_table("registry") {
        doc["registry"] = toml_edit::table();
    }
    doc["registry"]["url"] = toml_edit::value(url);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| crate::error::io_at(d, e))?;
    }
    std::fs::write(&p, doc.to_string()).map_err(|e| crate::error::io_at(&p, e))
}

/// The keychain account a registry's token is kept under: `HLOCK_SECRET_REGISTRY_<HOST>`
/// (keychain names are plain variable names).
pub fn keychain_name(url: &str) -> String {
    let host = url.split_once("://").map_or(url, |(_, r)| r);
    let up: String = host.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("{}REGISTRY_{}", crate::secrets::PREFIX, up.trim_matches('_'))
}

/// A registry's token and where it came from (`environment`, `keychain`, `credentials file`).
pub fn token_for(url: &str) -> Option<(String, &'static str)> {
    if let Some(t) = std::env::var(TOKEN_ENV).ok().filter(|t| !t.trim().is_empty()) {
        return Some((t.trim().to_string(), "environment"));
    }
    let ctx = crate::profiles::Ctx::from_process();
    if let Some(t) = crate::secrets::keychain_get(&keychain_name(url), &ctx) {
        return Some((t, "keychain"));
    }
    file_token(&credentials_path(), url).map(|t| (t, "credentials file"))
}

fn file_token(creds: &Path, url: &str) -> Option<String> {
    read_toml(creds).get("registries")?.get(url)?.get("token")?.as_str().map(str::to_string)
}

/// Keep `token` for `url`: in the keychain, else in `~/.hlock/credentials.toml` (mode 0600).
/// Returns where it went.
pub fn save_token(url: &str, token: &str) -> Result<&'static str> {
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(err("a registry token is one word with no spaces"));
    }
    let ctx = crate::profiles::Ctx::from_process();
    if crate::secrets::keychain(&ctx).is_some() && crate::secrets::keychain_set(&keychain_name(url), token, &ctx).is_ok() {
        remove_file_token(&credentials_path(), url)?;
        return Ok("keychain");
    }
    save_file_token(&credentials_path(), url, token)?;
    Ok("credentials file")
}

fn save_file_token(p: &Path, url: &str, token: &str) -> Result<()> {
    let p = p.to_path_buf();
    let mut doc = read_toml(&p);
    if !doc.contains_table("registries") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["registries"] = toml_edit::Item::Table(t);
    }
    let mut entry = toml_edit::Table::new();
    entry["token"] = toml_edit::value(token);
    doc["registries"][url] = toml_edit::Item::Table(entry);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| crate::error::io_at(d, e))?;
    }
    // created owner-only before the token is written into it
    std::fs::write(&p, "").map_err(|e| crate::error::io_at(&p, e))?;
    crate::secrets::restrict(&p)?;
    std::fs::write(&p, doc.to_string()).map_err(|e| crate::error::io_at(&p, e))
}

fn remove_file_token(p: &Path, url: &str) -> Result<bool> {
    let mut doc = read_toml(p);
    let removed = doc.get_mut("registries").and_then(|r| r.as_table_like_mut()).and_then(|r| r.remove(url)).is_some();
    if removed {
        std::fs::write(p, doc.to_string()).map_err(|e| crate::error::io_at(p, e))?;
    }
    Ok(removed)
}

/// Forget `url`'s token (keychain and credentials file). True if one was removed.
pub fn forget(url: &str) -> Result<bool> {
    let ctx = crate::profiles::Ctx::from_process();
    let in_keychain = crate::secrets::keychain(&ctx).is_some() && crate::secrets::keychain_delete(&keychain_name(url), &ctx).unwrap_or(false);
    Ok(remove_file_token(&credentials_path(), url)? || in_keychain)
}

// --------------------------------------------------------------------------- //
// references: NS/NAME[@VERSION]
// --------------------------------------------------------------------------- //

/// A package in a registry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ref {
    pub namespace: String,
    pub name: String,
    pub version: Option<String>,
}

impl Ref {
    pub fn package(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

fn namespace_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 39 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !s.starts_with('-') && !s.ends_with('-')
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c)) && s.starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// A version: letters, digits and `._+-`, starting with a letter or digit.
pub fn version_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)) && s.starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// Parse `NS/NAME[@VERSION]`; `None` if it isn't one.
pub fn parse_ref(s: &str) -> Option<Ref> {
    let (body, version) = match s.split_once('@') {
        Some((b, v)) if version_ok(v) => (b, Some(v.to_string())),
        Some(_) => return None,
        None => (s, None),
    };
    let (ns, name) = body.split_once('/')?;
    (namespace_ok(ns) && name_ok(name)).then(|| Ref { namespace: ns.into(), name: name.into(), version })
}

/// True if `arg` is a registry reference rather than a file, a git source or a pulled name.
pub fn is_ref(arg: &str) -> bool {
    parse_ref(arg).is_some() && !pull::is_source(arg) && std::fs::symlink_metadata(arg).is_err()
}

/// A `[pulled] from` of a registry pull: `registry:<url>/<ns>/<name>[@<version>]`.
pub fn pulled_spec(url: &str, r: &Ref) -> String {
    let v = r.version.as_deref().map(|v| format!("@{v}")).unwrap_or_default();
    format!("registry:{url}/{}{v}", r.package())
}

/// Split a `registry:<url>/<ns>/<name>[@version]` back into its URL and reference.
pub fn parse_pulled_spec(spec: &str) -> Result<(String, Ref)> {
    let bad = || err(format!("{spec:?} isn't a registry source (registry:URL/NAMESPACE/NAME)"));
    let rest = spec.strip_prefix("registry:").ok_or_else(bad)?;
    let (head, name) = rest.rsplit_once('/').ok_or_else(bad)?;
    let (url, ns) = head.rsplit_once('/').ok_or_else(bad)?;
    let r = parse_ref(&format!("{ns}/{name}")).ok_or_else(bad)?;
    Ok((normalize_url(url)?, r))
}

// --------------------------------------------------------------------------- //
// HTTP
// --------------------------------------------------------------------------- //

/// A client for one registry.
pub struct Client {
    pub url: String,
    token: Option<String>,
    agent: ureq::Agent,
}

/// What the device-token endpoint said.
pub enum Poll {
    Pending,
    SlowDown,
    Token(Value),
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

impl Client {
    pub fn new(url: &str, token: Option<String>) -> Client {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .proxy(ureq::Proxy::try_from_env())
            .user_agent(format!("hlock/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Client { url: url.to_string(), token, agent }
    }

    /// A client with the token saved for `url`, if any.
    pub fn for_url(url: &str) -> Client {
        Client::new(url, token_for(url).map(|(t, _)| t))
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{API}{path}", self.url)
    }

    fn auth<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        match &self.token {
            Some(t) => req.header("authorization", format!("Bearer {t}")),
            None => req,
        }
    }

    fn unreachable(&self, e: impl std::fmt::Display) -> Error {
        err(format!("can't reach the registry at {}: {e}", self.url))
    }

    /// An error from a non-2xx response: the problem's title and detail, its
    /// `extensions.errors`, and a hint for the status.
    fn problem(&self, status: u16, body: &str) -> Error {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let title = v["title"].as_str().unwrap_or("");
        let detail = v["detail"].as_str().unwrap_or("");
        let mut msg = match (title.is_empty(), detail.is_empty()) {
            (false, false) => format!("{title}: {detail}"),
            (false, true) => title.to_string(),
            (true, false) => detail.to_string(),
            (true, true) => body.chars().take(300).collect::<String>().trim().to_string(),
        };
        if msg.is_empty() {
            msg = format!("HTTP {status}");
        }
        let errors: Vec<String> = v.pointer("/extensions/errors").or_else(|| v.get("errors")).and_then(Value::as_array).into_iter().flatten().map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())).collect();
        for e in errors {
            msg.push_str(&format!("\n  - {e}"));
        }
        let hint = match status {
            401 => format!("\nlog in with `hlock login {}` (or set {TOKEN_ENV})", self.url),
            403 => "\nyour token doesn't allow this: check its scopes and the namespace".into(),
            _ => String::new(),
        };
        err(format!("the registry refused ({status}): {msg}{hint}"))
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let url = self.endpoint(path);
        let mut resp = self.auth(self.agent.get(&url)).header("accept", "application/json").call().map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| self.unreachable(e))?;
        if !(200..300).contains(&status) {
            return Err(self.problem(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| err(format!("{url} returned invalid JSON: {e}")))
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value, String)> {
        let url = self.endpoint(path);
        let mut resp = self.auth(self.agent.post(&url)).header("content-type", "application/json").header("accept", "application/json").send(body.to_string()).map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| self.unreachable(e))?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null), text))
    }

    /// Start a device login (RFC 8628).
    pub fn device_start(&self) -> Result<Value> {
        let (status, v, text) = self.post_json("/auth/device", &json!({ "client_name": format!("hlock {}", env!("CARGO_PKG_VERSION")) }))?;
        if !(200..300).contains(&status) {
            return Err(self.problem(status, &text));
        }
        for k in ["device_code", "user_code", "verification_uri"] {
            if !v[k].is_string() {
                return Err(err(format!("the registry's device login response has no {k}")));
            }
        }
        Ok(v)
    }

    /// Poll for the token of a device login.
    pub fn device_poll(&self, device_code: &str) -> Result<Poll> {
        let (status, v, text) = self.post_json("/auth/device/token", &json!({ "device_code": device_code }))?;
        if (200..300).contains(&status) {
            if !v["access_token"].is_string() {
                return Err(err("the registry's token response has no access_token"));
            }
            return Ok(Poll::Token(v));
        }
        match v["error"].as_str() {
            Some("authorization_pending") => Ok(Poll::Pending),
            Some("slow_down") => Ok(Poll::SlowDown),
            Some("expired_token") => Err(err("the login code expired; run `hlock login` again")),
            Some("access_denied") => Err(err("the login was denied in the browser")),
            _ => Err(self.problem(status, &text)),
        }
    }

    pub fn whoami(&self) -> Result<Value> {
        if self.token.is_none() {
            return Err(err(format!("not logged in to {}: run `hlock login {}` (or set {TOKEN_ENV})", self.url, self.url)));
        }
        self.get_json("/whoami")
    }

    /// Search: `q` and the filters (`type`, `kind`, `source`, …) as query parameters.
    pub fn search(&self, q: &str, filters: &[(&str, String)]) -> Result<Value> {
        let mut qs = format!("q={}", enc(q));
        for (k, v) in filters.iter().filter(|(_, v)| !v.is_empty()) {
            qs.push_str(&format!("&{k}={}", enc(v)));
        }
        self.get_json(&format!("/search?{qs}"))
    }

    pub fn package(&self, r: &Ref) -> Result<Value> {
        self.get_json(&format!("/packages/{}/{}", enc(&r.namespace), enc(&r.name)))
    }

    pub fn version(&self, r: &Ref, version: &str) -> Result<Value> {
        self.get_json(&format!("/packages/{}/{}/versions/{}", enc(&r.namespace), enc(&r.name), enc(version)))
    }

    /// A version's archive and the digest the server sent with it (`X-Hlock-Digest`).
    pub fn download(&self, r: &Ref, version: &str) -> Result<(Vec<u8>, Option<String>)> {
        let url = self.endpoint(&format!("/packages/{}/{}/versions/{}/download", enc(&r.namespace), enc(&r.name), enc(version)));
        let mut resp = self.auth(self.agent.get(&url)).header("accept", "application/gzip").call().map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(self.problem(status, &text));
        }
        let header = resp.headers().get("x-hlock-digest").and_then(|h| h.to_str().ok()).map(str::to_string);
        let bytes = resp.body_mut().with_config().limit(MAX_ARCHIVE as u64 + 1).read_to_vec().map_err(|e| err(format!("can't download {url}: {e}")))?;
        if bytes.len() > MAX_ARCHIVE {
            return Err(err(format!("{} is larger than {} MiB; refusing it", r.package(), MAX_ARCHIVE >> 20)));
        }
        Ok((bytes, header))
    }

    /// Publish an archive. `Ok(None)` when the version already exists (409).
    pub fn publish(&self, r: &Ref, version: &str, bytes: &[u8]) -> Result<Option<Value>> {
        let url = self.endpoint(&format!("/packages/{}/{}/versions/{}", enc(&r.namespace), enc(&r.name), enc(version)));
        let mut resp = self.auth(self.agent.put(&url)).header("content-type", "application/gzip").header("accept", "application/json").send(bytes).map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        match status {
            200..=299 => Ok(Some(serde_json::from_str(&text).unwrap_or(Value::Null))),
            409 => Ok(None),
            _ => Err(self.problem(status, &text)),
        }
    }
}

// --------------------------------------------------------------------------- //
// archives
// --------------------------------------------------------------------------- //

/// One file in an archive.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PackFile {
    pub path: String,
    pub size: u64,
    pub digest: String,
}

/// A setup packed for `push`.
#[derive(Debug, Clone, Serialize)]
pub struct Packed {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub digest: String,
    pub size_bytes: u64,
    pub files: Vec<PackFile>,
    /// The project's name (`[project] name`, else `apm.yml`'s `name`, else the directory's), slugified.
    pub project: String,
    /// `apm.yml`'s `version`, if it has one.
    pub apm_version: Option<String>,
    pub warnings: Vec<String>,
}

/// Whether `path` may be in a setup archive.
pub fn allowed_path(path: &str) -> bool {
    SETUP_FILES.contains(&path) || path == "README.md" || (path.starts_with(".hlock/vendor/") && path.len() > ".hlock/vendor/".len())
}

/// `s` as a package name: lowercase letters, digits and `-_.`.
pub fn slug(s: &str) -> String {
    let out: String = s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let mut out = out.trim_matches(['-', '.', '_']).to_string();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.chars().take(100).collect()
}

/// Strings in a parsed file that look like a credential, as `file: key` lines.
fn secret_lines(v: &Value, file: &str, key: &str, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let named = !key.is_empty() && crate::capture::is_secret_key(key) && s.len() >= 8 && !s.contains("HLOCK_SECRET_") && !s.trim_start().starts_with('$');
            if crate::capture::looks_secret(s) || named {
                out.push(format!("{file}: {}", if key.is_empty() { "a value" } else { key }));
            }
        }
        Value::Array(a) => a.iter().for_each(|x| secret_lines(x, file, key, out)),
        Value::Object(m) => m.iter().for_each(|(k, x)| secret_lines(x, file, k, out)),
        _ => {}
    }
}

fn parsed(path: &str, bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    match path.rsplit_once('.').map(|(_, e)| e) {
        Some("toml") | Some("lock") => toml::from_str::<toml::Value>(text).ok().and_then(|v| serde_json::to_value(v).ok()),
        Some("json") => serde_json::from_str(text).ok(),
        Some("yml") | Some("yaml") => serde_norway::from_str(text).ok(),
        _ => None,
    }
}

/// Pack the setup in `dir` into a deterministic `tar.gz`, after checking it the way a
/// pull would: valid files, no credential-looking values, no hidden characters.
pub fn pack(dir: &Path) -> Result<Packed> {
    let mut warnings = Vec::new();
    // (path in the archive, file on disk)
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    let add = |entries: &mut Vec<(String, PathBuf)>, rel: &str, p: PathBuf, warnings: &mut Vec<String>| match std::fs::symlink_metadata(&p) {
        Ok(m) if m.is_file() => entries.push((rel.to_string(), p)),
        Ok(_) => warnings.push(format!("{rel} isn't a regular file; left out")),
        Err(_) => {}
    };
    for f in SETUP_FILES.iter().filter(|f| **f != "apm.yml") {
        add(&mut entries, f, dir.join(f), &mut warnings);
    }
    match (dir.join("apm.yml").is_file(), dir.join("apm.yaml").is_file()) {
        (true, true) => {
            warnings.push("both apm.yml and apm.yaml exist; pushing apm.yml".into());
            add(&mut entries, "apm.yml", dir.join("apm.yml"), &mut warnings);
        }
        (true, false) => add(&mut entries, "apm.yml", dir.join("apm.yml"), &mut warnings),
        (false, true) => add(&mut entries, "apm.yml", dir.join("apm.yaml"), &mut warnings),
        _ => {}
    }
    if entries.is_empty() {
        return Err(err(format!("{} has none of {}, apm.yaml; nothing to push", dir.display(), SETUP_FILES.join(", "))));
    }
    add(&mut entries, "README.md", dir.join("README.md"), &mut warnings);
    let mut project = None;
    if dir.join("harness.lock").is_file() {
        let lock = Lockfile::load(&dir.join("harness.lock"))?;
        let mut paths: Vec<String> = Vec::new();
        paths.extend(lock.skill.iter().filter_map(|s| s.vendored.clone()));
        paths.extend(lock.instructions.iter().filter_map(|i| i.vendored.clone()));
        paths.extend(lock.config.iter().filter_map(|c| c.vendored.clone()));
        for fc in lock.subagent.iter().chain(&lock.script).chain(&lock.middleware) {
            paths.extend(fc.vendored.clone());
        }
        paths.sort();
        paths.dedup();
        for p in paths {
            if !safe_rel(&p) || !p.starts_with(".hlock/vendor/") {
                warnings.push(format!("harness.lock vendors {p}, outside .hlock/vendor/; left out"));
                continue;
            }
            let from = dir.join(&p);
            match std::fs::symlink_metadata(&from) {
                Ok(m) if m.file_type().is_symlink() => warnings.push(format!("{p} is a symlink; left out")),
                Ok(m) if m.is_dir() => {
                    for f in crate::scan::text_files(&from) {
                        if let Ok(r) = f.strip_prefix(dir) {
                            entries.push((r.to_string_lossy().replace('\\', "/"), f));
                        }
                    }
                }
                Ok(_) => entries.push((p, from)),
                Err(_) => warnings.push(format!("{p} (vendored by harness.lock) is missing; left out")),
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(dir.join("harness.toml")) {
        let m: toml::Value = toml::from_str(&t).map_err(|e| err(format!("harness.toml isn't valid TOML: {e}")))?;
        project = m.get("project").and_then(|p| p.get("name")).and_then(|n| n.as_str()).map(str::to_string);
    }
    let mut apm_version = None;
    for (rel, p) in &entries {
        if rel == "apm.yml" || rel == "apm.lock.yaml" {
            let t = std::fs::read_to_string(p).map_err(|e| crate::error::io_at(p, e))?;
            let v: Value = serde_norway::from_str(&t).map_err(|e| err(format!("{rel} isn't valid YAML: {e}")))?;
            if rel == "apm.yml" {
                project = project.or_else(|| v["name"].as_str().map(str::to_string));
                apm_version = v["version"].as_str().map(str::to_string).or_else(|| v["version"].as_f64().map(|f| f.to_string()));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|a, b| a.0 == b.0);
    if entries.len() > MAX_ENTRIES {
        return Err(err(format!("the setup has {} files; a registry archive holds at most {MAX_ENTRIES}", entries.len())));
    }
    let mut files = Vec::new();
    let mut secrets = Vec::new();
    let mut hidden = Vec::new();
    let gz = flate2::GzBuilder::new().mtime(0).operating_system(255).write(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for (rel, p) in &entries {
        let bytes = crate::fsutil::read_regular(p, MAX_UNPACKED as usize).map_err(|e| crate::error::io_at(p, e))?;
        if let Some(v) = parsed(rel, &bytes) {
            secret_lines(&v, rel, "", &mut secrets);
        }
        hidden.extend(crate::scan::scan_bytes(&bytes, rel));
        let mut h = tar::Header::new_ustar();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut h, rel, bytes.as_slice()).map_err(|e| err(format!("can't pack {rel}: {e}")))?;
        files.push(PackFile { path: rel.clone(), size: bytes.len() as u64, digest: crate::sha256_hex(&bytes) });
    }
    if !secrets.is_empty() {
        secrets.sort();
        secrets.dedup();
        return Err(Error::Audit(format!(
            "these look like credentials, not placeholders; nothing was pushed:\n{}\nre-pin to redact them (`hlock pin`), or replace them with ${{HLOCK_SECRET_…}} references",
            secrets.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n")
        )));
    }
    crate::audit::blocking(&hidden, &[], "the setup")?;
    let bytes = tar.into_inner().and_then(|gz| gz.finish()).map_err(|e| err(format!("can't pack the setup: {e}")))?;
    if bytes.len() > MAX_ARCHIVE {
        return Err(err(format!("the setup packs to {} bytes; a registry archive holds at most {} MiB", bytes.len(), MAX_ARCHIVE >> 20)));
    }
    let digest = format!("sha256:{}", crate::sha256_hex(&bytes));
    let dir_name = std::fs::canonicalize(dir).ok().and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "setup".into());
    let project = slug(&project.unwrap_or(dir_name));
    let project = if project.is_empty() { "setup".into() } else { project };
    Ok(Packed { size_bytes: bytes.len() as u64, bytes, digest, files, project, apm_version, warnings })
}

/// Unpack a setup archive into `dest` (an empty directory), refusing anything that isn't
/// a plain file at an allowed path. `apm.yaml` is written as `apm.yml`. Returns the paths.
pub fn unpack(bytes: &[u8], dest: &Path) -> Result<Vec<String>> {
    let bad = |why: String| err(format!("the archive is unsafe or invalid: {why}; nothing was written"));
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut out = Vec::new();
    let mut total = 0u64;
    let entries = archive.entries().map_err(|e| bad(e.to_string()))?;
    for (i, entry) in entries.enumerate() {
        if i >= MAX_ENTRIES {
            return Err(bad(format!("more than {MAX_ENTRIES} entries")));
        }
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        let raw = entry.path_bytes().into_owned();
        let path = String::from_utf8(raw).map_err(|_| bad("a path that isn't UTF-8".into()))?;
        let path = path.trim_start_matches("./").to_string();
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                if !safe_rel(path.trim_end_matches('/')) {
                    return Err(bad(format!("{path:?} is outside the archive")));
                }
                continue;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {}
            t => return Err(bad(format!("{path:?} is a {t:?}, not a regular file"))),
        }
        if path.starts_with('/') || !safe_rel(&path) {
            return Err(bad(format!("{path:?} is outside the archive")));
        }
        let path = if path == "apm.yaml" { "apm.yml".to_string() } else { path };
        if !allowed_path(&path) {
            return Err(bad(format!("{path:?} isn't a file a setup may contain")));
        }
        total += entry.header().size().unwrap_or(0);
        if total > MAX_UNPACKED {
            return Err(bad(format!("it unpacks to more than {} MiB", MAX_UNPACKED >> 20)));
        }
        let to = dest.join(&path);
        crate::fsutil::check_within(&path, dest, &to)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::io_at(parent, e))?;
        }
        let mut buf = Vec::new();
        entry.by_ref().take(MAX_UNPACKED + 1).read_to_end(&mut buf).map_err(|e| bad(e.to_string()))?;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&to).map_err(|e| bad(format!("{path}: {e}")))?;
        f.write_all(&buf).map_err(|e| crate::error::io_at(&to, e))?;
        out.push(path);
    }
    if !out.iter().any(|p| SETUP_FILES.contains(&p.as_str())) {
        return Err(bad(format!("it has none of {}", SETUP_FILES.join(", "))));
    }
    Ok(out)
}

// --------------------------------------------------------------------------- //
// commands
// --------------------------------------------------------------------------- //

/// `hlock login URL --token T`: check the token against the registry, then keep both.
pub fn login_with_token(url: &str, token: &str) -> Result<Value> {
    let url = normalize_url(url)?;
    let who = Client::new(&url, Some(token.trim().to_string())).whoami()?;
    let stored = save_token(&url, token)?;
    save_url(&url)?;
    Ok(json!({ "url": url, "whoami": who, "stored": stored }))
}

/// `hlock push`: pack `dir`, then publish it as `name@version`.
pub fn push(url: &str, dir: &Path, name: Option<&str>, version: Option<&str>, dry_run: bool) -> Result<Value> {
    let client = Client::for_url(url);
    let packed = pack(dir)?;
    let namespace = match name {
        Some(_) => None,
        None if client.has_token() => {
            let who = client.whoami()?;
            Some(who.pointer("/user/handle").and_then(Value::as_str).ok_or_else(|| err("the registry's whoami has no user.handle; pass NAMESPACE/NAME"))?.to_string())
        }
        None if dry_run => None,
        None => return Err(err(format!("not logged in to {url}: run `hlock login {url}`, or pass NAMESPACE/NAME and set {TOKEN_ENV}"))),
    };
    let package = match (name, &namespace) {
        (Some(n), _) => n.to_string(),
        (None, Some(ns)) => format!("{ns}/{}", packed.project),
        (None, None) => format!("<your-namespace>/{}", packed.project),
    };
    let version = match version {
        Some(v) => v.to_string(),
        None => packed.apm_version.clone().filter(|v| version_ok(v)).unwrap_or_else(|| format!("sha-{}", &packed.digest["sha256:".len().."sha256:".len() + 12])),
    };
    if !version_ok(&version) {
        return Err(err(format!("{version:?} isn't a version: letters, digits and ._+- only, starting with a letter or digit")));
    }
    let mut out = json!({
        "package": package,
        "version": version,
        "digest": packed.digest,
        "size_bytes": packed.size_bytes,
        "files": packed.files,
        "warnings": packed.warnings,
        "registry": url,
    });
    if dry_run {
        out["dry_run"] = json!(true);
        return Ok(out);
    }
    let r = parse_ref(&package).ok_or_else(|| err(format!("{package:?} isn't NAMESPACE/NAME (lowercase letters, digits, - and, in the name, _ .)")))?;
    if !client.has_token() {
        return Err(err(format!("not logged in to {url}: run `hlock login {url}` (or set {TOKEN_ENV})")));
    }
    match client.publish(&r, &version, &packed.bytes)? {
        Some(resp) => {
            if let Value::Object(m) = resp {
                for (k, v) in m {
                    if !v.is_null() && k != "files" {
                        out[k] = v;
                    }
                }
            }
            out["published"] = json!(true);
        }
        None => {
            let existing = client.version(&r, &version)?;
            if existing["digest"].as_str() == Some(packed.digest.as_str()) {
                out["published"] = json!(false);
                out["already_published"] = json!(true);
                out["published_at"] = existing["published_at"].clone();
            } else {
                return Err(err(format!(
                    "{package}@{version} is already published with other content ({}); versions can't change. Push with another --version",
                    existing["digest"].as_str().unwrap_or("unknown digest")
                )));
            }
        }
    }
    Ok(out)
}

/// Where a registry pull writes.
pub enum Dest<'a> {
    /// The store or a project lock file, as a git pull does.
    Target(&'a Target),
    /// Every file of the setup, into a directory.
    Into(&'a Path),
}

/// `hlock pull NS/NAME[@VERSION]` (or a `registry:` spec from a `[pulled]` table). A
/// remote package (`source` github or gitlab) is pulled from git, as its `pull` field says.
pub fn pull(url_flag: Option<&str>, spec: &str, dest: Dest<'_>, force: bool, allow_hidden: &[String]) -> Result<Value> {
    let (url, r) = if spec.starts_with("registry:") {
        parse_pulled_spec(spec)?
    } else {
        let r = parse_ref(spec).ok_or_else(|| err(format!("{spec:?} isn't NAMESPACE/NAME[@VERSION]")))?;
        (resolve_url(url_flag)?, r)
    };
    let client = Client::for_url(&url);
    let pkg = client.package(&r)?;
    let ty = pkg["type"].as_str().unwrap_or("setup");
    if ty == "apm-package" {
        return Err(err(format!(
            "{} is an APM package, not a setup: install it with APM (`apm install {}` with this registry in apm.yml's `registries:`)",
            r.package(),
            r.package()
        )));
    }
    let source = pkg["source"].as_str().unwrap_or("hosted");
    if source != "hosted" {
        // a remote package lives in its repository: pull it from there
        let mut git = match r.version.as_deref() {
            Some(v) => client.version(&r, v)?["pull"].as_str().or_else(|| pkg["pull"].as_str()).map(str::to_string),
            None => pkg["pull"].as_str().map(str::to_string),
        }
        .ok_or_else(|| err(format!("{} is a {source} package but the registry gave no source to pull it from", r.package())))?;
        if !pull::is_source(&git) {
            git = format!("{source}:{git}");
        }
        pull::parse(&git)?;
        let mut v = match dest {
            Dest::Target(t) => serde_json::to_value(pull::pull_allowing(&git, t, force, allow_hidden)?)?,
            Dest::Into(d) => serde_json::to_value(pull::pull_into(&git, d, force, allow_hidden)?)?,
        };
        v["registry"] = json!({ "url": url, "package": r.package(), "source": source });
        return Ok(v);
    }
    let version = r.version.clone().unwrap_or_else(|| "latest".into());
    let ver = client.version(&r, &version)?;
    let version = ver["version"].as_str().map(str::to_string).unwrap_or(version);
    let expected = ver["digest"].as_str().map(str::to_string).ok_or_else(|| err(format!("the registry gave no digest for {}@{version}", r.package())))?;
    let (bytes, header) = client.download(&r, &version)?;
    let got = format!("sha256:{}", crate::sha256_hex(&bytes));
    if got != expected || header.as_ref().is_some_and(|h| *h != expected) {
        return Err(err(format!("{}@{version}'s archive doesn't match its digest (expected {expected}, got {got}); nothing was written", r.package())));
    }
    let scratch = pull::scratch_dir()?;
    unpack(&bytes, &scratch.0)?;
    let reference = Ref { version: r.version.clone(), ..r.clone() };
    let mut pulled = json!({ "from": pulled_spec(&url, &reference), "url": format!("{url}/{}", r.package()), "path": crate::LOCKFILE_PATH, "version": version, "digest": expected });
    if let Some(v) = &r.version {
        pulled["ref"] = json!(v);
    }
    let shown = match &r.version {
        Some(v) => format!("{}@{v}", r.package()),
        None => r.package(),
    };
    let src = Source { spec: shown, url: format!("{url}/{}", r.package()), fetch_url: String::new(), reference: r.version.clone(), path: None, owner: r.name.clone() };
    let lock = scratch.0.join(crate::LOCKFILE_PATH);
    let fetched = Fetched { src, root: &scratch.0, commit: version.clone(), remote_lock: lock.is_file().then_some(lock), others: Vec::new(), git: false, pulled, digest: Some(expected) };
    let mut v = match dest {
        Dest::Target(t) => serde_json::to_value(pull::place(fetched, t, force, allow_hidden)?)?,
        Dest::Into(d) => {
            let base = scratch.0.clone();
            serde_json::to_value(pull::place_into(fetched, &base, d, force, allow_hidden)?)?
        }
    };
    v["registry"] = json!({ "url": url, "package": r.package(), "source": "hosted" });
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-registry-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const LOCK: &str = "lock-version = 1\ngenerated-by = \"t\"\ngenerated-at = \"\"\n[harness]\nname = \"pi\"\n[[skill]]\nname = \"x\"\norigin = \"local\"\nvendored = \".hlock/vendor/pi/skills/x\"\n";

    fn setup(d: &Path) {
        std::fs::write(d.join("harness.lock"), LOCK).unwrap();
        std::fs::write(d.join("harness.toml"), "[project]\nname = \"My Setup\"\n").unwrap();
        std::fs::write(d.join("apm.yaml"), "name: demo\nversion: 1.2.0\n").unwrap();
        std::fs::create_dir_all(d.join(".hlock/vendor/pi/skills/x")).unwrap();
        std::fs::write(d.join(".hlock/vendor/pi/skills/x/SKILL.md"), "Run the tests.\n").unwrap();
        std::fs::write(d.join("notes.txt"), "not pushed").unwrap();
    }

    #[test]
    fn refs() {
        assert_eq!(parse_ref("acme/review-bot"), Some(Ref { namespace: "acme".into(), name: "review-bot".into(), version: None }));
        assert_eq!(parse_ref("acme/review-bot@1.2.0").unwrap().version.as_deref(), Some("1.2.0"));
        for bad in ["acme", "Acme/x", "acme/x/y", "acme/x@", "acme/x@-1", "-a/x", "acme/.x", "github:a/b", "./a/b", "a/b@1 2"] {
            assert!(parse_ref(bad).is_none(), "{bad}");
        }
        let (url, r) = parse_pulled_spec("registry:https://registry.example.com/sub/acme/bot@2").unwrap();
        assert_eq!((url.as_str(), r.package().as_str(), r.version.as_deref()), ("https://registry.example.com/sub", "acme/bot", Some("2")));
        assert_eq!(pulled_spec(&url, &r), "registry:https://registry.example.com/sub/acme/bot@2");
        assert!(parse_pulled_spec("github:a/b").is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(normalize_url("https://registry.example.com/").unwrap(), "https://registry.example.com");
        assert_eq!(normalize_url("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert!(normalize_url("http://registry.example.com").is_err());
        assert!(normalize_url("ftp://x").is_err());
        assert!(normalize_url("https://user:pw@x.com").is_err());
        assert_eq!(keychain_name("https://registry.example.com:8443"), "HLOCK_SECRET_REGISTRY_REGISTRY_EXAMPLE_COM_8443");
        assert_eq!(slug("My Setup!"), "my-setup");
    }

    #[test]
    fn packing_is_deterministic_and_round_trips() {
        let d = tmp("pack");
        setup(&d);
        let a = pack(&d).unwrap();
        let b = pack(&d).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.bytes, b.bytes);
        let paths: Vec<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".hlock/vendor/pi/skills/x/SKILL.md", "apm.yml", "harness.lock", "harness.toml"]);
        assert_eq!((a.project.as_str(), a.apm_version.as_deref()), ("my-setup", Some("1.2.0")));
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let got = unpack(&a.bytes, &out).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(std::fs::read_to_string(out.join(".hlock/vendor/pi/skills/x/SKILL.md")).unwrap(), "Run the tests.\n");
        assert!(!out.join("notes.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn packing_refuses_secrets_hidden_characters_and_nothing() {
        let d = tmp("refuse");
        assert!(pack(&d).unwrap_err().to_string().contains("nothing to push"));
        std::fs::write(d.join("harness.toml"), "[project]\nname = \"x\"\n[env]\nOPENAI_API_KEY = \"sk-proj-abcdefghijklmnopqrstuvwxyz0123456789\"\n").unwrap();
        let e = pack(&d).unwrap_err().to_string();
        assert!(e.contains("credentials") && e.contains("OPENAI_API_KEY"), "{e}");
        std::fs::write(d.join("harness.toml"), "[project]\nname = \"x\"\n[env]\nOPENAI_API_KEY = \"${HLOCK_SECRET_OPENAI_API_KEY}\"\n").unwrap();
        pack(&d).unwrap();
        std::fs::write(d.join("README.md"), "hi\u{202E}there\n").unwrap();
        assert!(pack(&d).unwrap_err().to_string().contains("U+202E"));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tarball(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut t = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        for (path, data, kind) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            // set_path refuses `..`; write the name bytes directly, as a hostile archive would
            let name = &mut h.as_gnu_mut().unwrap().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            if *kind == tar::EntryType::Symlink {
                h.set_link_name("/etc/passwd").unwrap();
            }
            h.set_cksum();
            t.append(&h, *data).unwrap();
        }
        t.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn unsafe_archives_are_refused() {
        let d = tmp("unsafe");
        let reg = tar::EntryType::Regular;
        for (entries, why) in [
            (vec![("../evil", b"x".as_slice(), reg)], "outside"),
            (vec![("/etc/evil", b"x".as_slice(), reg)], "outside"),
            (vec![("harness.lock", b"x".as_slice(), reg), ("run.sh", b"x".as_slice(), reg)], "may contain"),
            (vec![("harness.lock", b"".as_slice(), tar::EntryType::Symlink)], "not a regular file"),
            (vec![("README.md", b"x".as_slice(), reg)], "none of"),
        ] {
            let out = d.join(why).join(entries.len().to_string());
            std::fs::create_dir_all(&out).unwrap();
            let e = unpack(&tarball(&entries), &out).unwrap_err().to_string();
            assert!(e.contains(why), "{why}: {e}");
        }
        assert!(unpack(b"not a tarball", &d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_credentials_file_and_the_config() {
        let d = tmp("creds");
        let (creds, config) = (d.join("credentials.toml"), d.join("config.toml"));
        let (a, b) = ("https://registry.example.com", "https://other.example.com/sub");
        save_file_token(&creds, a, "hlk_a").unwrap();
        save_file_token(&creds, b, "hlk_b").unwrap();
        assert_eq!((file_token(&creds, a).as_deref(), file_token(&creds, b).as_deref()), (Some("hlk_a"), Some("hlk_b")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(remove_file_token(&creds, a).unwrap() && !remove_file_token(&creds, a).unwrap());
        assert_eq!((file_token(&creds, a), file_token(&creds, b).as_deref()), (None, Some("hlk_b")));
        std::fs::write(&config, "# mine\n[other]\nx = 1\n").unwrap();
        save_url_in(&config, a).unwrap();
        assert_eq!(url_in(&config).as_deref(), Some(a));
        assert!(std::fs::read_to_string(&config).unwrap().starts_with("# mine\n[other]\nx = 1"), "the rest of the file is kept");
        let _ = std::fs::remove_dir_all(&d);
    }
}
