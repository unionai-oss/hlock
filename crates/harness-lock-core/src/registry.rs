//! The registry client: `hlock login`, `logout`, `whoami`, `push`, `search`, and
//! `hlock pull NS/NAME`, over the hlock registry HTTP API v1 (`specs/registry.md`).
//!
//! A registry is any server that implements that API; hlock has no default one. Its URL
//! comes from `--registry`, else `HLOCK_REGISTRY`, else `[registry] url` in
//! `~/.hlock/config.toml` (written by `login`). Its token comes from
//! `HLOCK_REGISTRY_TOKEN`, else the OS keychain, else `~/.hlock/credentials.toml`.
//!
//! A setup travels as a deterministic `tar.gz`: `harness.toml`, `harness.lock` and the
//! files it vendors, `apm.yml`, `apm.lock.yaml` and `README.md`, and the harness files under
//! `.hlock/` (`.hlock/locks/harness.pi.lock`, say) with the files they vendor, sorted, with zero times
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
    // `//` in the path would read as the start of a package path in a `[pulled] from`
    if host.is_empty() || host.contains('@') || rest.contains("//") || u.chars().any(|c| c.is_whitespace() || c.is_control()) || u.contains(['?', '#']) {
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

/// A package in a registry, and optionally a path in it (a lock, or a directory of locks).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ref {
    pub namespace: String,
    pub name: String,
    pub version: Option<String>,
    pub path: Option<String>,
}

impl Ref {
    pub fn package(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// `NS/NAME[//PATH][@VERSION]`, as it's written.
    pub fn spec(&self) -> String {
        let path = self.path.as_deref().map(|p| format!("//{p}")).unwrap_or_default();
        let v = self.version.as_deref().map(|v| format!("@{v}")).unwrap_or_default();
        format!("{}{path}{v}", self.package())
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

/// `s` without a trailing `@VERSION` (one after its last `/`), and the version.
fn split_version(s: &str) -> std::result::Result<(&str, Option<String>), ()> {
    match s.rfind('@') {
        Some(i) if !s[i..].contains('/') => {
            let v = &s[i + 1..];
            if version_ok(v) { Ok((&s[..i], Some(v.to_string()))) } else { Err(()) }
        }
        _ => Ok((s, None)),
    }
}

/// A path in a package: `/`-separated, inside it, without `@`.
fn ref_path(p: &str) -> Option<String> {
    let p = p.trim_matches('/');
    (!p.is_empty() && !p.contains('@') && safe_rel(p) && !p.split('/').any(str::is_empty)).then(|| p.to_string())
}

/// Parse `NS/NAME[//PATH][@VERSION]`; `None` if it isn't one.
pub fn parse_ref(s: &str) -> Option<Ref> {
    let (body, version) = split_version(s).ok()?;
    let (pkg, path) = match body.split_once("//") {
        Some((pkg, p)) => (pkg, Some(ref_path(p)?)),
        None => (body, None),
    };
    let (ns, name) = pkg.split_once('/')?;
    (namespace_ok(ns) && name_ok(name)).then(|| Ref { namespace: ns.into(), name: name.into(), version, path })
}

/// A reference with the version before the path (`NS/NAME@V//PATH`): the hint for it.
fn misordered(s: &str) -> Option<String> {
    let (head, path) = s.split_once("//")?;
    let (pkg, v) = head.split_once('@')?;
    let fixed = format!("{pkg}//{path}@{v}");
    parse_ref(&fixed).is_some().then(|| format!("{s:?} puts the version before the path; the version goes last: {fixed}"))
}

/// Parse a registry reference, with an error saying what's wrong with it.
pub fn parse_ref_checked(s: &str) -> Result<Ref> {
    if let Some(hint) = misordered(s) {
        return Err(err(hint));
    }
    parse_ref(s).ok_or_else(|| err(format!("{s:?} isn't NAMESPACE/NAME[//PATH][@VERSION]")))
}

/// True if `arg` is a registry reference rather than a file, a git source or a pulled name.
pub fn is_ref(arg: &str) -> bool {
    (parse_ref(arg).is_some() || misordered(arg).is_some()) && !pull::is_source(arg) && std::fs::symlink_metadata(arg).is_err()
}

/// A `[pulled] from` of a registry pull: `registry:<url>/<ns>/<name>[//<path>][@<version>]`.
pub fn pulled_spec(url: &str, r: &Ref) -> String {
    format!("registry:{url}/{}", r.spec())
}

/// Split a `registry:<url>/<ns>/<name>[//<path>][@version]` back into its URL and reference.
pub fn parse_pulled_spec(spec: &str) -> Result<(String, Ref)> {
    let bad = || err(format!("{spec:?} isn't a registry source (registry:URL/NAMESPACE/NAME[//PATH][@VERSION])"));
    let rest = spec.strip_prefix("registry:").ok_or_else(bad)?;
    let (rest, version) = split_version(rest).map_err(|_| bad())?;
    // the path starts at the first `//` after the URL's scheme
    let after_scheme = rest.find("://").map(|i| i + 3).ok_or_else(bad)?;
    let (head, path) = match rest[after_scheme..].find("//") {
        Some(i) => (&rest[..after_scheme + i], Some(ref_path(&rest[after_scheme + i + 2..]).ok_or_else(bad)?)),
        None => (rest, None),
    };
    let (head, name) = head.rsplit_once('/').ok_or_else(bad)?;
    let (url, ns) = head.rsplit_once('/').ok_or_else(bad)?;
    let mut r = parse_ref(&format!("{ns}/{name}")).ok_or_else(bad)?;
    r.version = version;
    r.path = path;
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

    /// APM's version list: `{package, versions: [{version, digest, published_at, …}]}`.
    pub fn versions(&self, r: &Ref) -> Result<Value> {
        self.get_json(&format!("/packages/{}/{}/versions", enc(&r.namespace), enc(&r.name)))
    }

    /// A version's archive and the digest the server sent with it (`X-Hlock-Digest`).
    pub fn download(&self, r: &Ref, version: &str) -> Result<(Vec<u8>, Option<String>)> {
        self.download_at_most(r, version, MAX_ARCHIVE)
    }

    /// [`Client::download`], refusing an archive larger than `limit` bytes.
    pub fn download_at_most(&self, r: &Ref, version: &str, limit: usize) -> Result<(Vec<u8>, Option<String>)> {
        let url = self.endpoint(&format!("/packages/{}/{}/versions/{}/download", enc(&r.namespace), enc(&r.name), enc(version)));
        let mut resp = self.auth(self.agent.get(&url)).header("accept", "application/gzip").call().map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(self.problem(status, &text));
        }
        let header = resp.headers().get("x-hlock-digest").and_then(|h| h.to_str().ok()).map(str::to_string);
        let bytes = resp.body_mut().with_config().limit(limit as u64 + 1).read_to_vec().map_err(|e| err(format!("can't download {url}: {e}")))?;
        if bytes.len() > limit {
            return Err(err(format!("{} is larger than {} MiB; refusing it", r.package(), limit >> 20)));
        }
        Ok((bytes, header))
    }

    /// Whether a package exists (`GET /packages/{ns}/{name}` isn't a 404).
    pub fn exists(&self, r: &Ref) -> Result<bool> {
        let url = self.endpoint(&format!("/packages/{}/{}", enc(&r.namespace), enc(&r.name)));
        let mut resp = self.auth(self.agent.get(&url)).header("accept", "application/json").call().map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        match status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(self.problem(status, &resp.body_mut().read_to_string().unwrap_or_default())),
        }
    }

    /// A version's evals archive and the digest the server sent with it.
    pub fn download_evals(&self, r: &Ref, version: &str) -> Result<(Vec<u8>, Option<String>)> {
        let url = self.endpoint(&format!("/packages/{}/{}/versions/{}/evals", enc(&r.namespace), enc(&r.name), enc(version)));
        let mut resp = self.auth(self.agent.get(&url)).header("accept", "application/gzip").call().map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(self.problem(status, &text));
        }
        let header = resp.headers().get("x-hlock-digest").and_then(|h| h.to_str().ok()).map(str::to_string);
        let limit = crate::evals_archive::MAX_ARCHIVE;
        let bytes = resp.body_mut().with_config().limit(limit as u64 + 1).read_to_vec().map_err(|e| err(format!("can't download {url}: {e}")))?;
        if bytes.len() > limit {
            return Err(err(format!("{}@{version}'s eval packs are larger than {} MiB; refusing them", r.package(), limit >> 20)));
        }
        Ok((bytes, header))
    }

    /// Upload a version's evals archive. `Ok(None)` on 409 (it has one, or declared another).
    pub fn publish_evals(&self, r: &Ref, version: &str, bytes: &[u8]) -> Result<Option<Value>> {
        let url = self.endpoint(&format!("/packages/{}/{}/versions/{}/evals", enc(&r.namespace), enc(&r.name), enc(version)));
        let mut resp = self.auth(self.agent.put(&url)).header("content-type", "application/gzip").header("accept", "application/json").send(bytes).map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        match status {
            200..=299 => Ok(Some(serde_json::from_str(&text).unwrap_or(Value::Null))),
            409 => Ok(None),
            _ => Err(self.problem(status, &text)),
        }
    }

    /// Publish an archive. `Ok(None)` when the version already exists (409).
    pub fn publish(&self, r: &Ref, version: &str, bytes: &[u8]) -> Result<Option<Value>> {
        self.publish_as(r, version, bytes, "application/gzip", None)
    }

    /// [`Client::publish`] with another archive type (`application/zip` for APM packages),
    /// and the digest of the evals archive the version will have (`X-Hlock-Evals-Digest`).
    pub fn publish_as(&self, r: &Ref, version: &str, bytes: &[u8], content_type: &str, evals_digest: Option<&str>) -> Result<Option<Value>> {
        let url = self.endpoint(&format!("/packages/{}/{}/versions/{}", enc(&r.namespace), enc(&r.name), enc(version)));
        let mut req = self.auth(self.agent.put(&url)).header("content-type", content_type).header("accept", "application/json");
        if let Some(d) = evals_digest {
            req = req.header("x-hlock-evals-digest", d);
        }
        let mut resp = req.send(bytes).map_err(|e| self.unreachable(e))?;
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
    /// The package's default name, slugified: the git remote's repository name (when the
    /// project is its work tree's root), else `[project] name`, else `apm.yml`'s `name`, else
    /// the directory's.
    pub project: String,
    /// Where `project` came from.
    pub name_from: NameFrom,
    /// The names the package would have without the git remote, in order (`[project] name`
    /// unless it's `hlock init`'s `demo`, `apm.yml`'s `name`, the directory's), slugified.
    pub fallback_names: Vec<String>,
    /// `apm.yml`'s `version`, if it has one.
    pub apm_version: Option<String>,
    pub warnings: Vec<String>,
}

/// Whether `path` may be in a setup archive.
pub fn allowed_path(path: &str) -> bool {
    SETUP_FILES.contains(&path) || path == "README.md" || pull::is_hlock_setup_path(path) || pull::is_vendored_path(path)
}

/// Whether `path` is one of a setup's own files (not README.md or a vendored file).
fn setup_path(path: &str) -> bool {
    SETUP_FILES.contains(&path) || pull::is_hlock_setup_path(path)
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
pub(crate) fn secret_lines(v: &Value, file: &str, key: &str, out: &mut Vec<String>) {
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

pub(crate) fn parsed(path: &str, bytes: &[u8]) -> Option<Value> {
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
    // harness files under .hlock/ too, such as .hlock/locks/harness.pi.lock
    let nested = pull::hlock_setup_files(dir);
    for p in &nested {
        add(&mut entries, p, dir.join(p), &mut warnings);
    }
    if entries.is_empty() {
        return Err(err(format!("{} has none of {}, apm.yaml, or harness files under .hlock/; nothing to push", dir.display(), SETUP_FILES.join(", "))));
    }
    add(&mut entries, "README.md", dir.join("README.md"), &mut warnings);
    let mut project = None;
    let locks = std::iter::once("harness.lock".to_string()).filter(|l| dir.join(l).is_file()).chain(nested.iter().filter(|p| p.ends_with(".lock")).cloned());
    for l in locks {
        let lock = Lockfile::load(&dir.join(&l))?;
        // vendored paths are relative to the lock's directory
        let at = pull::parent_dir(&l).to_string();
        for v in pull::vendored_paths(&lock) {
            if !safe_rel(&v) || !v.starts_with(".hlock/vendor/") {
                warnings.push(format!("{l} vendors {v}, outside .hlock/vendor/; left out"));
                continue;
            }
            let p = pull::in_dir(&at, &v);
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
                Err(_) => warnings.push(format!("{p} (vendored by {l}) is missing; left out")),
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(dir.join("harness.toml")) {
        let m: toml::Value = toml::from_str(&t).map_err(|e| err(format!("harness.toml isn't valid TOML: {e}")))?;
        project = m.get("project").and_then(|p| p.get("name")).and_then(|n| n.as_str()).map(str::to_string);
    }
    let mut apm_name = None;
    let mut apm_version = None;
    for (rel, p) in &entries {
        if rel == "apm.yml" || rel == "apm.lock.yaml" {
            let t = std::fs::read_to_string(p).map_err(|e| crate::error::io_at(p, e))?;
            let v: Value = serde_norway::from_str(&t).map_err(|e| err(format!("{rel} isn't valid YAML: {e}")))?;
            if rel == "apm.yml" {
                apm_name = v["name"].as_str().map(str::to_string);
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
    // the names push defaulted to before the git remote's, for the rename guard; not
    // `hlock init`'s default `demo`, which many unrelated projects share
    let mut fallback_names: Vec<String> = Vec::new();
    for n in [project.as_deref().filter(|p| slug(p) != "demo"), apm_name.as_deref(), Some(dir_name.as_str())].into_iter().flatten() {
        let s = slug(n);
        if !s.is_empty() && !fallback_names.contains(&s) {
            fallback_names.push(s);
        }
    }
    let (name, name_from) = [
        (git_repo_name(dir), NameFrom::GitRemote),
        (project, NameFrom::Project),
        (apm_name, NameFrom::Apm),
        (Some(dir_name), NameFrom::Directory),
    ]
    .into_iter()
    .find_map(|(n, from)| n.map(|n| slug(&n)).filter(|s| !s.is_empty()).map(|s| (s, from)))
    .unwrap_or_else(|| ("setup".into(), NameFrom::Directory));
    Ok(Packed { size_bytes: bytes.len() as u64, bytes, digest, files, project: name, name_from, fallback_names, apm_version, warnings })
}

/// Where a package's default name came from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NameFrom {
    /// The repository name of the git remote (the project is its work tree's root).
    GitRemote,
    /// `[project] name` in `harness.toml`.
    Project,
    /// `apm.yml`'s `name`.
    Apm,
    /// The project directory's name.
    Directory,
}

impl NameFrom {
    pub fn describe(self) -> &'static str {
        match self {
            NameFrom::GitRemote => "the git remote",
            NameFrom::Project => "[project] name",
            NameFrom::Apm => "apm.yml's name",
            NameFrom::Directory => "the directory's name",
        }
    }
}

/// The repository name of `dir`'s git remote (`origin`, else the only one), when `dir` is the
/// root of a git work tree; `None` otherwise (a project in a subdirectory of a repository, no
/// remote, no git).
pub fn git_repo_name(dir: &Path) -> Option<String> {
    let run = |args: &[&str]| git_in(dir, args);
    let top = git_toplevel(dir)?;
    if top != std::fs::canonicalize(dir).ok()? {
        return None;
    }
    let remotes = run(&["remote"])?;
    let remotes: Vec<&str> = remotes.lines().map(str::trim).filter(|r| !r.is_empty()).collect();
    let remote = if remotes.contains(&"origin") { "origin" } else if remotes.len() == 1 { remotes[0] } else { return None };
    repo_name_of(&run(&["remote", "get-url", remote])?)
}

#[cfg(test)]
thread_local! {
    /// What this thread's tests add to every git command's environment (a ceiling, no
    /// system or user config), so tests running in parallel don't share it.
    static GIT_TEST_ENV: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `git -C dir ARGS`: the caller's `GIT_DIR` and `GIT_WORK_TREE` don't leak in, git never
/// prompts, and its messages are untranslated.
fn git_command(dir: &Path, args: &[&str]) -> std::process::Command {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C").env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").stdin(std::process::Stdio::null());
    #[cfg(test)]
    GIT_TEST_ENV.with(|e| {
        for (k, v) in e.borrow().iter() {
            c.env(k, v);
        }
    });
    c
}

/// `git -C dir ARGS`'s output, trimmed, when it succeeds.
fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(dir, args).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The root of the git work tree `dir` is in, canonicalized: `Ok(None)` when git says `dir`
/// isn't in one, and `Err` with why when git can't tell (it isn't installed, the repository
/// isn't trusted (`safe.directory`), its `.git` is broken).
pub fn git_toplevel_checked(dir: &Path) -> std::result::Result<Option<PathBuf>, String> {
    let out = match git_command(dir, &["rev-parse", "--show-toplevel"]).stderr(std::process::Stdio::piped()).output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("git isn't installed".into()),
        Err(e) => return Err(format!("can't run git: {e}")),
    };
    if out.status.success() {
        let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return std::fs::canonicalize(&top).map(Some).map_err(|e| format!("can't read the repository's root {top}: {e}"));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // "not a git repository (or any of the parent directories)", or "… (or any parent up to
    // mount point …)", worded so since git 1.7 (capitalized before 2.25); "not a git
    // repository: PATH" is a `.git` file pointing nowhere
    if stderr.to_ascii_lowercase().contains("not a git repository (or any") {
        return Ok(None);
    }
    Err(stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("git rev-parse failed").to_string())
}

/// The root of the git work tree `dir` is in, canonicalized; `None` outside one, or when git
/// can't tell.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    git_toplevel_checked(dir).ok().flatten()
}

/// Where a push's root came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootFrom {
    /// `--root DIR` (or `-C`, `--dir`).
    Flag,
    /// The root of the git work tree the working directory is in.
    Repository,
    /// The working directory, outside any git work tree.
    CurrentDirectory,
    /// The working directory, because git couldn't tell whether it's in a work tree.
    CurrentDirectoryGitFailed,
}

/// The directory `hlock push` packs: `root` (relative to `cwd`), else the root of the git work
/// tree `cwd` is in, else `cwd`. When git can't tell whether `cwd` is in a work tree, `cwd`,
/// with a warning that says why.
pub fn push_root(root: Option<&Path>, cwd: &Path) -> (PathBuf, RootFrom, Option<String>) {
    match root {
        Some(r) => (cwd.join(r), RootFrom::Flag, None),
        None => match git_toplevel_checked(cwd) {
            Ok(Some(top)) => (top, RootFrom::Repository, None),
            Ok(None) => (cwd.to_path_buf(), RootFrom::CurrentDirectory, None),
            Err(why) => (cwd.to_path_buf(), RootFrom::CurrentDirectoryGitFailed, Some(format!("couldn't find the git repository ({why}); pushing the current directory. Pass --root DIR to choose"))),
        },
    }
}

/// `p` for display: without the `\\?\` prefix Windows' canonical paths have.
fn shown_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
    }
}

/// Whether `dir` has any file a setup archive is made of.
pub fn has_setup(dir: &Path) -> bool {
    SETUP_FILES.iter().chain(["apm.yaml"].iter()).any(|f| dir.join(f).is_file()) || !pull::hlock_setup_files(dir).is_empty()
}

/// For a repository root with nothing to push: the `--root` that would push the nearest
/// directory with a setup, from `cwd` up to (not including) `top`, relative to `cwd`.
fn nearer_root(cwd: &Path, top: &Path) -> Option<String> {
    let cwd = std::fs::canonicalize(cwd).ok()?;
    let mut up = 0;
    for d in cwd.ancestors() {
        if d == top || !d.starts_with(top) {
            return None;
        }
        if has_setup(d) {
            return Some(if up == 0 { ".".into() } else { vec![".."; up].join("/") });
        }
        up += 1;
    }
    None
}

/// The repository name in a git remote URL: its last path segment without `.git`
/// (`https://…/owner/repo.git`, `git@host:owner/repo.git`, `file:///…/repo`, a local path).
pub fn repo_name_of(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let u = u.split(['?', '#']).next().unwrap_or(u);
    let last = u.rsplit(['/', ':', '\\']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty()).then(|| name.to_string())
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
        // the size the body is read with (a PAX `size` overrides the ustar one), which must be
        // the header's, so the limit counts what's actually written
        let size = entry.size();
        if entry.header().size().ok() != Some(size) {
            return Err(bad(format!("{path:?} has a size that differs from its header's")));
        }
        total += size;
        if total > MAX_UNPACKED {
            return Err(bad(format!("it unpacks to more than {} MiB", MAX_UNPACKED >> 20)));
        }
        let to = dest.join(&path);
        crate::fsutil::check_within(&path, dest, &to)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::io_at(parent, e))?;
        }
        let mut buf = Vec::new();
        entry.by_ref().take(size + 1).read_to_end(&mut buf).map_err(|e| bad(e.to_string()))?;
        if buf.len() as u64 != size {
            return Err(bad(format!("{path:?} isn't the size its header says")));
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&to).map_err(|e| bad(format!("{path}: {e}")))?;
        f.write_all(&buf).map_err(|e| crate::error::io_at(&to, e))?;
        out.push(path);
    }
    if !out.iter().any(|p| setup_path(p)) {
        return Err(bad(format!("it has none of {}, or harness files under .hlock/", SETUP_FILES.join(", "))));
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

/// `hlock push`: pack the project at `root` (see [`push_root`]: by default the git
/// repository's root), and its eval packs unless `no_evals`, then publish it as
/// `name@version`.
pub fn push(url: &str, root: Option<&Path>, name: Option<&str>, version: Option<&str>, dry_run: bool, no_evals: bool) -> Result<Value> {
    let cwd = std::env::current_dir().map_err(|e| err(format!("can't read the working directory: {e}")))?;
    let (dir, root_from, git_warning) = push_root(root, &cwd);
    let dir = dir.as_path();
    if root_from == RootFrom::Repository && !has_setup(dir) {
        let hint = match nearer_root(&cwd, dir) {
            Some(r) => format!("; push the project here with `hlock push --root {r}`"),
            None => "; pass --root DIR to push another directory".into(),
        };
        return Err(err(format!(
            "the repository root {} has none of {}, apm.yaml, or harness files under .hlock/; nothing to push{hint}",
            shown_path(dir),
            SETUP_FILES.join(", ")
        )));
    }
    let mut root_warnings: Vec<String> = git_warning.into_iter().collect();
    // a project in a subdirectory of the repository, pushed from there: say it isn't the one pushed
    if root_from == RootFrom::Repository && std::fs::canonicalize(&cwd).ok().as_deref() != Some(dir) && has_setup(&cwd) {
        root_warnings.push("this directory has its own hlock setup; pushing the repository root. Pass --root . to push this one".into());
    }
    let client = Client::for_url(url);
    let packed = match pack(dir) {
        Ok(p) => p,
        Err(e) if !root_warnings.is_empty() => return Err(err(format!("{e}\n{}", root_warnings.join("\n")))),
        Err(e) => return Err(e),
    };
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
    let mut warnings = root_warnings;
    warnings.extend(packed.warnings.iter().cloned());
    let evals = if no_evals { None } else { crate::evals_archive::pack(dir)? };
    if let Some(e) = &evals {
        warnings.extend(e.warnings.iter().cloned());
        warnings.extend(stale_evidence(dir, &packed));
    }
    let version = match version {
        Some(v) => v.to_string(),
        None => packed.apm_version.clone().filter(|v| version_ok(v)).unwrap_or_else(|| {
            // a change to the eval packs alone gives a new version too
            let d = match &evals {
                Some(e) => crate::hash::sha256_text(&format!("{}\n{}", packed.digest, e.digest)),
                None => packed.digest["sha256:".len()..].to_string(),
            };
            format!("sha-{}", &d[d.len() - 64..][..12])
        }),
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
        "warnings": warnings,
        "registry": url,
        "name_from": if name.is_some() { Value::Null } else { json!(packed.name_from) },
        "root": shown_path(dir),
        "root_from": root_from,
    });
    if let Some(e) = &evals {
        out["evals"] = json!({ "digest": e.digest, "size_bytes": e.size_bytes, "packs": e.packs, "files": e.files });
    }
    if dry_run {
        out["dry_run"] = json!(true);
        return Ok(out);
    }
    let r = parse_ref(&package).filter(|r| r.path.is_none()).ok_or_else(|| err(format!("{package:?} isn't NAMESPACE/NAME (lowercase letters, digits, - and, in the name, _ .)")))?;
    if !client.has_token() {
        return Err(err(format!("not logged in to {url}: run `hlock login {url}` (or set {TOKEN_ENV})")));
    }
    if name.is_none() && packed.name_from == NameFrom::GitRemote {
        rename_guard(&client, &r, &packed)?;
    }
    let evals_digest = evals.as_ref().map(|e| e.digest.as_str());
    // the version as the registry has it: from the 201, or read back after a 409
    let ver = match client.publish_as(&r, &version, &packed.bytes, "application/gzip", evals_digest)? {
        Some(resp) => {
            if let Value::Object(m) = &resp {
                for (k, v) in m {
                    if !v.is_null() && k != "files" && k != "evals" {
                        out[k] = v.clone();
                    }
                }
            }
            out["published"] = json!(true);
            resp
        }
        None => {
            let existing = client.version(&r, &version)?;
            if existing["digest"].as_str() != Some(packed.digest.as_str()) {
                return Err(err(format!(
                    "{package}@{version} is already published with other content ({}); versions can't change. Push with another --version",
                    existing["digest"].as_str().unwrap_or("unknown digest")
                )));
            }
            out["published"] = json!(false);
            out["already_published"] = json!(true);
            out["published_at"] = existing["published_at"].clone();
            existing
        }
    };
    let Some(e) = &evals else { return Ok(out) };
    // whether the registry takes eval packs is in its answer: a `Version` with an `evals` field
    match crate::evals_archive::version_evals(&ver) {
        None => {
            out["warnings"].as_array_mut().unwrap().push(json!("the registry doesn't take eval packs; published without them"));
            out["evals"]["published"] = json!(false);
            out["evals"]["unsupported"] = json!(true);
        }
        Some(declared) if declared.and_then(|d| d["digest"].as_str()) != Some(e.digest.as_str()) => {
            return Err(err(format!(
                "{package}@{version} is published with {}; versions can't change. Push with another --version",
                if declared.is_some() { "other eval packs" } else { "no eval packs" }
            )));
        }
        Some(declared) if declared.is_some_and(|d| d["uploaded"].as_bool() == Some(true)) => {
            out["evals"]["published"] = json!(false);
            out["evals"]["already_published"] = json!(true);
        }
        Some(_) => match client.publish_evals(&r, &version, &e.bytes)? {
            Some(_) => out["evals"]["published"] = json!(true),
            None => {
                let now = client.version(&r, &version)?;
                let same = now.pointer("/evals/digest").and_then(Value::as_str) == Some(e.digest.as_str()) && now.pointer("/evals/uploaded").and_then(Value::as_bool) == Some(true);
                if !same {
                    return Err(err(format!("{package}@{version} has other eval packs; versions can't change. Push with another --version")));
                }
                out["evals"]["published"] = json!(false);
                out["evals"]["already_published"] = json!(true);
            }
        },
    }
    Ok(out)
}

/// Locks being pushed whose `[evidence] eval-pack-digest` isn't the eval packs' digest now.
fn stale_evidence(dir: &Path, packed: &Packed) -> Vec<String> {
    let Ok(m) = crate::manifest::Manifest::load(&dir.join("harness.toml")) else { return Vec::new() };
    let now = crate::tasks::packs_digest(&m);
    packed
        .files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| *p == crate::LOCKFILE_PATH || (pull::is_hlock_setup_path(p) && p.ends_with(".lock")))
        .filter_map(|p| {
            let l = Lockfile::load(&dir.join(p)).ok()?;
            let recorded = l.evidence.eval_pack_digest.clone().filter(|d| !d.is_empty())?;
            (recorded != now).then(|| format!("{p}'s evidence was measured on other eval packs than these (eval-pack-digest {}…); re-run `hlock resolve` to refresh it", &recorded[..recorded.len().min(19)]))
        })
        .collect()
}

/// Refuse to publish under the repository's name a project that may have been published under
/// a name push used to default to (`[project] name`, `apm.yml`'s, the directory's), so people
/// pulling the old name don't silently stop getting updates.
fn rename_guard(client: &Client, r: &Ref, packed: &Packed) -> Result<()> {
    let olds: Vec<&String> = packed.fallback_names.iter().filter(|n| **n != r.name).collect();
    if olds.is_empty() || client.exists(r)? {
        return Ok(());
    }
    for old in olds {
        let before = Ref { name: old.to_string(), version: None, path: None, ..r.clone() };
        if client.exists(&before)? {
            return Err(err(format!(
                "a package named {} exists, and this project's default name is now its repository's, {}; pass {} to publish under the repository's name, or {} to publish there",
                before.package(),
                r.package(),
                r.package(),
                before.package()
            )));
        }
    }
    Ok(())
}

/// Where a registry pull writes.
pub enum Dest<'a> {
    /// The store or a project lock file, as a git pull does.
    Target(&'a Target),
    /// Every file of the setup, into a directory.
    Into(&'a Path),
}

/// Whether `path` is one of a package's locks: the root `harness.lock` or `harness.*.lock`, or
/// a nested harness lock under `.hlock/`.
fn package_lock(path: &str) -> bool {
    path.ends_with(".lock") && ((!path.contains('/') && pull::is_harness_file(path)) || pull::is_hlock_setup_path(path))
}

/// The lock a registry reference picks in a package's `files`, and the package's other locks
/// (when no path was given and the root lock was taken).
fn choose_lock(r: &Ref, files: &[String]) -> Result<(String, Vec<String>)> {
    let locks: Vec<&String> = files.iter().filter(|p| package_lock(p)).collect();
    let refs = |ps: &[&String]| ps.iter().map(|p| format!("  hlock pull {}", Ref { path: Some(p.to_string()), ..r.clone() }.spec())).collect::<Vec<_>>().join("\n");
    let pkg = r.package();
    match r.path.as_deref() {
        Some(p) if p.ends_with(".lock") => match locks.iter().find(|l| l.as_str() == p) {
            Some(_) => Ok((p.to_string(), Vec::new())),
            None if locks.is_empty() => Err(err(format!("{pkg} has no lock {p}, nor any other"))),
            None => Err(err(format!("{pkg} has no lock {p}; its locks:\n{}", refs(&locks)))),
        },
        Some(d) => {
            let here: Vec<&String> = locks.iter().copied().filter(|l| pull::parent_dir(l) == d).collect();
            if let Some(l) = here.iter().find(|l| l.ends_with("/harness.lock")) {
                return Ok((l.to_string(), Vec::new()));
            }
            match here.len() {
                1 => Ok((here[0].clone(), Vec::new())),
                0 => Err(err(format!("{pkg} has no lock in {d}/{}", if locks.is_empty() { String::new() } else { format!("; its locks:\n{}", refs(&locks)) }))),
                _ => Err(err(format!("{pkg} has {} locks in {d}/; pick one:\n{}", here.len(), refs(&here)))),
            }
        }
        None => {
            if locks.iter().any(|l| l.as_str() == crate::LOCKFILE_PATH) {
                let others = locks.iter().filter(|l| l.as_str() != crate::LOCKFILE_PATH).map(|l| l.to_string()).collect();
                return Ok((crate::LOCKFILE_PATH.to_string(), others));
            }
            match locks.len() {
                1 => Ok((locks[0].clone(), Vec::new())),
                0 => Err(err(format!("{pkg} has no harness.lock; write its files into a project with --root DIR"))),
                _ => Err(err(format!("{pkg} has {} locks and no harness.lock; pick one:\n{}", locks.len(), refs(&locks)))),
            }
        }
    }
}

/// `hlock pull NS/NAME[//PATH][@VERSION]` (or a `registry:` spec from a `[pulled]` table). A
/// remote package (`source` github or gitlab) is pulled from git, as its `pull` field says.
/// `no_evals` leaves out the eval packs `--into` writes.
pub fn pull(url_flag: Option<&str>, spec: &str, dest: Dest<'_>, force: bool, allow_hidden: &[String], no_evals: bool) -> Result<Value> {
    let (url, r) = if spec.starts_with("registry:") {
        parse_pulled_spec(spec)?
    } else {
        (resolve_url(url_flag)?, parse_ref_checked(spec)?)
    };
    if let (Dest::Into(_), Some(p)) = (&dest, &r.path) {
        return Err(err(format!("--root writes the whole package; drop //{p}, or pull that lock into the store without --root")));
    }
    let client = Client::for_url(&url);
    let pkg = client.package(&r)?;
    let ty = pkg["type"].as_str().unwrap_or("setup");
    if ty == "apm-package" {
        return Err(err(format!(
            "{} is an APM package, not a setup: fetch it with `hlock apm pull {}`, or install it with APM (`apm install {}` with {}/api in apm.yml's `registries:`)",
            r.package(),
            r.package(),
            r.package(),
            url
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
        let src = pull::parse(&git)?;
        if let Some(p) = &r.path {
            // PATH is inside the package, which may itself be a directory of its repository
            git = pull::with_path(&src, &pull::in_dir(src.path.as_deref().unwrap_or(""), p));
            pull::parse(&git)?;
        }
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
    let files = unpack(&bytes, &scratch.0)?;
    // the lock: the one PATH names, else the root harness.lock, else the only one (for
    // --into, the root lock if there is one; every file is written anyway)
    let (lock_path, others) = match dest {
        Dest::Target(_) => choose_lock(&r, &files)?,
        Dest::Into(_) => (crate::LOCKFILE_PATH.to_string(), Vec::new()),
    };
    // `from` names the lock unless it's the root one, so a refresh never switches locks
    let chosen = Ref { path: (lock_path != crate::LOCKFILE_PATH).then(|| lock_path.clone()), ..r.clone() };
    let mut pulled = json!({ "from": pulled_spec(&url, &chosen), "url": format!("{url}/{}", r.package()), "path": lock_path, "version": version, "digest": expected });
    if let Some(v) = &r.version {
        pulled["ref"] = json!(v);
    }
    let (eval_packs, eval_files) = match dest {
        Dest::Into(_) if !no_evals => match crate::evals_archive::version_evals(&ver).flatten() {
            Some(e) => {
                let digest = e["digest"].as_str().unwrap_or_default().to_string();
                if e["uploaded"].as_bool() != Some(true) {
                    return Err(err(format!("{}@{version}'s eval packs aren't uploaded yet; nothing was written. Try again later, or pass --no-evals", r.package())));
                }
                let (ebytes, eheader) = client.download_evals(&r, &version)?;
                let got = format!("sha256:{}", crate::sha256_hex(&ebytes));
                if got != digest || eheader.as_ref().is_some_and(|h| *h != digest) {
                    return Err(err(format!("{}@{version}'s eval packs don't match their digest (expected {digest}, got {got}); nothing was written", r.package())));
                }
                let manifest = std::fs::read_to_string(scratch.0.join("harness.toml")).ok().and_then(|t| toml::from_str::<toml::Value>(&t).ok());
                let declared = manifest.as_ref().map(crate::evals_archive::declared_packs).unwrap_or_default();
                pulled["evals-digest"] = json!(digest);
                crate::evals_archive::unpack(&ebytes, &scratch.0, &declared)?
            }
            None => (Vec::new(), Vec::new()),
        },
        _ => (Vec::new(), Vec::new()),
    };
    let src = Source { spec: chosen.spec(), url: format!("{url}/{}", r.package()), fetch_url: String::new(), reference: r.version.clone(), path: chosen.path.clone(), owner: r.name.clone() };
    let others = others.into_iter().map(|p| pull::OtherLock { source: Ref { path: Some(p.clone()), ..r.clone() }.spec(), path: p }).collect();
    let lock = scratch.0.join(&lock_path);
    let fetched = Fetched { src, root: &scratch.0, commit: version.clone(), remote_lock: lock.is_file().then_some(lock), others, git: false, pulled, digest: Some(expected), eval_packs, eval_files };
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
        assert_eq!(parse_ref("acme/review-bot"), Some(Ref { namespace: "acme".into(), name: "review-bot".into(), version: None, path: None }));
        assert_eq!(parse_ref("acme/review-bot@1.2.0").unwrap().version.as_deref(), Some("1.2.0"));
        for bad in ["acme", "Acme/x", "acme/x/y", "acme/x@", "acme/x@-1", "-a/x", "acme/.x", "github:a/b", "./a/b", "a/b@1 2", "a/b//", "a/b//x/../../y", "a/b//x@y/z", "a/b@1//x"] {
            assert!(parse_ref(bad).is_none(), "{bad}");
        }
        // a path in the package, the version last
        let r = parse_ref("acme/hlock//.hlock/locks/harness.pi.lock@1.2.0").unwrap();
        assert_eq!((r.package().as_str(), r.path.as_deref(), r.version.as_deref()), ("acme/hlock", Some(".hlock/locks/harness.pi.lock"), Some("1.2.0")));
        assert_eq!(r.spec(), "acme/hlock//.hlock/locks/harness.pi.lock@1.2.0");
        let e = parse_ref_checked("acme/hlock@1.2.0//.hlock/locks").unwrap_err().to_string();
        assert!(e.contains("the version goes last: acme/hlock//.hlock/locks@1.2.0"), "{e}");
        assert!(is_ref("acme/hlock@1.2.0//.hlock/locks"));
        let (url, r) = parse_pulled_spec("registry:https://registry.example.com/sub/acme/bot@2").unwrap();
        assert_eq!((url.as_str(), r.package().as_str(), r.version.as_deref()), ("https://registry.example.com/sub", "acme/bot", Some("2")));
        assert_eq!(pulled_spec(&url, &r), "registry:https://registry.example.com/sub/acme/bot@2");
        // a [pulled] from with a path: the `//` after the scheme isn't the path's
        for (spec, path, version) in [
            ("registry:https://h.example.com/acme/hlock//.hlock/locks/harness.pi.lock", Some(".hlock/locks/harness.pi.lock"), None),
            ("registry:https://h.example.com/acme/hlock//.hlock/locks/harness.pi.lock@1.0", Some(".hlock/locks/harness.pi.lock"), Some("1.0")),
            ("registry:http://127.0.0.1:8080/acme/hlock", None, None),
        ] {
            let (url, r) = parse_pulled_spec(spec).unwrap();
            assert_eq!((r.package().as_str(), r.path.as_deref(), r.version.as_deref()), ("acme/hlock", path, version), "{spec}");
            assert_eq!(pulled_spec(&url, &r), spec);
        }
        assert!(parse_pulled_spec("github:a/b").is_err());
        assert!(normalize_url("https://h.example.com/a//b").is_err());
    }

    #[test]
    fn repository_names() {
        for (url, name) in [
            ("https://github.com/cosmicbboy/hlock.git", "hlock"),
            ("https://github.com/cosmicbboy/hlock/", "hlock"),
            ("git@github.com:cosmicbboy/hlock.git", "hlock"),
            ("ssh://git@host:2222/team/agents.git", "agents"),
            ("file:///srv/repos/setup", "setup"),
            ("/srv/repos/setup.git", "setup"),
            ("C:\\repos\\setup", "setup"),
        ] {
            assert_eq!(repo_name_of(url).as_deref(), Some(name), "{url}");
        }
        assert_eq!(repo_name_of(""), None);
    }

    #[test]
    fn the_push_root_is_the_repository_root_by_default() {
        let d = std::fs::canonicalize(tmp("root")).unwrap();
        // git never looks above the temp directory, nor at the machine's or the user's config
        let env: Vec<(String, String)> = [
            ("GIT_CEILING_DIRECTORIES", d.parent().unwrap().display().to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        GIT_TEST_ENV.with(|e| *e.borrow_mut() = env.clone());
        // outside any git work tree: the working directory
        assert_eq!(push_root(None, &d), (d.clone(), RootFrom::CurrentDirectory, None));
        let ok = std::process::Command::new("git").arg("-C").arg(&d).args(["init", "-q"]).envs(env.iter().map(|(k, v)| (k, v))).env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").status().is_ok_and(|s| s.success());
        if !ok {
            return; // no git here
        }
        let sub = d.join("agents/review");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(push_root(None, &d), (d.clone(), RootFrom::Repository, None));
        assert_eq!(push_root(None, &sub), (d.clone(), RootFrom::Repository, None));
        assert_eq!(push_root(Some(Path::new("agents/review")), &d), (sub.clone(), RootFrom::Flag, None));
        assert_eq!(push_root(Some(Path::new(".")), &sub), (sub.join("."), RootFrom::Flag, None));
        // a `.git` that points nowhere: git can't tell, so the working directory, with why
        let broken = d.join("agents/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(".git"), "gitdir: /nonexistent/hlock-test\n").unwrap();
        let (got, from, warning) = push_root(None, &broken);
        assert_eq!((got, from), (broken.clone(), RootFrom::CurrentDirectoryGitFailed));
        assert!(warning.as_deref().is_some_and(|w| w.contains("couldn't find the git repository (fatal: not a git repository")), "{warning:?}");
        std::fs::remove_dir_all(&broken).unwrap();
        // nothing at the root: the nearest directory up from the working directory that has a setup
        assert!(!has_setup(&d));
        assert_eq!(nearer_root(&sub, &d), None);
        std::fs::write(d.join("agents/harness.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(nearer_root(&sub, &d).as_deref(), Some(".."));
        std::fs::write(sub.join("harness.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(nearer_root(&sub, &d).as_deref(), Some("."));
        // a project in a subdirectory doesn't take the repository's name
        assert_eq!(git_repo_name(&sub), None);
        GIT_TEST_ENV.with(|e| e.borrow_mut().clear());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn windows_verbatim_prefixes_are_not_shown() {
        assert_eq!(shown_path(Path::new(r"\\?\C:\repos\hlock")), r"C:\repos\hlock");
        assert_eq!(shown_path(Path::new(r"\\?\UNC\server\share\hlock")), r"\\server\share\hlock");
        assert_eq!(shown_path(Path::new("/home/alice/hlock")), "/home/alice/hlock");
    }

    #[test]
    fn choosing_a_lock_in_a_package() {
        let files: Vec<String> = ["harness.lock", "harness.toml", ".hlock/locks/harness.pi.lock", ".hlock/locks/harness.codex.lock", ".hlock/team/harness.lock", ".hlock/vendor/x/harness.lock", ".hlock/locks/.hlock/vendor/y/harness.z.lock"].iter().map(|s| s.to_string()).collect();
        let r = |s: &str| parse_ref(s).unwrap();
        let (l, others) = choose_lock(&r("a/p"), &files).unwrap();
        assert_eq!(l, "harness.lock");
        assert_eq!(others, [".hlock/locks/harness.pi.lock", ".hlock/locks/harness.codex.lock", ".hlock/team/harness.lock"]);
        assert_eq!(choose_lock(&r("a/p//.hlock/locks/harness.pi.lock"), &files).unwrap(), (".hlock/locks/harness.pi.lock".into(), vec![]));
        assert_eq!(choose_lock(&r("a/p//.hlock/team"), &files).unwrap().0, ".hlock/team/harness.lock");
        let e = choose_lock(&r("a/p//.hlock/locks@1"), &files).unwrap_err().to_string();
        assert!(e.contains("has 2 locks in .hlock/locks/") && e.contains("hlock pull a/p//.hlock/locks/harness.pi.lock@1"), "{e}");
        assert!(choose_lock(&r("a/p//.hlock/vendor/x/harness.lock"), &files).unwrap_err().to_string().contains("has no lock"));
        // without a root lock: the only one, else a choice
        let one = vec![".hlock/locks/harness.pi.lock".to_string()];
        assert_eq!(choose_lock(&r("a/p"), &one).unwrap().0, ".hlock/locks/harness.pi.lock");
        let e = choose_lock(&r("a/p"), &files[2..4]).unwrap_err().to_string();
        assert!(e.contains("2 locks and no harness.lock"), "{e}");
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
    fn packing_takes_harness_files_under_dot_hlock() {
        let d = tmp("nested");
        std::fs::write(d.join("harness.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(d.join(".hlock/locks")).unwrap();
        std::fs::write(d.join(".hlock/locks/harness.pi.lock"), LOCK).unwrap();
        std::fs::write(d.join(".hlock/locks/harness.pi.lock.prev"), LOCK).unwrap();
        std::fs::write(d.join(".hlock/locks/notes.txt"), "not pushed").unwrap();
        std::fs::create_dir_all(d.join(".hlock/team")).unwrap();
        std::fs::write(d.join(".hlock/team/harness.toml"), "[project]\nname = \"team\"\n").unwrap();
        // a lock's vendored paths are relative to its own directory
        std::fs::create_dir_all(d.join(".hlock/locks/.hlock/vendor/pi/skills/x")).unwrap();
        std::fs::write(d.join(".hlock/locks/.hlock/vendor/pi/skills/x/SKILL.md"), "Run the tests.\n").unwrap();
        std::fs::write(d.join(".hlock/locks/.hlock/vendor/harness.y.lock"), LOCK).unwrap();
        // the root's vendor directory belongs to the root lock, which there isn't
        std::fs::create_dir_all(d.join(".hlock/vendor/pi/skills/x")).unwrap();
        std::fs::write(d.join(".hlock/vendor/pi/skills/x/SKILL.md"), "not pushed\n").unwrap();
        std::fs::write(d.join(".hlock/vendor/harness.lock"), LOCK).unwrap();
        // hlock's own state: an eval environment's candidate lock, a stored dependency tree
        for state in [".hlock/env/eval/x", ".hlock/store/trees/abc", ".hlock/tune/1"] {
            std::fs::create_dir_all(d.join(state)).unwrap();
            std::fs::write(d.join(state).join("harness.lock"), LOCK).unwrap();
        }
        let a = pack(&d).unwrap();
        let paths: Vec<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".hlock/locks/.hlock/vendor/pi/skills/x/SKILL.md", ".hlock/locks/harness.pi.lock", ".hlock/team/harness.toml", "harness.toml"]);
        assert!(a.warnings.is_empty(), "{:?}", a.warnings);
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert_eq!(unpack(&a.bytes, &out).unwrap().len(), 4);
        assert_eq!(std::fs::read_to_string(out.join(".hlock/locks/harness.pi.lock")).unwrap(), LOCK);
        assert_eq!(std::fs::read_to_string(out.join(".hlock/locks/.hlock/vendor/pi/skills/x/SKILL.md")).unwrap(), "Run the tests.\n");
        // nested harness files alone are a setup
        std::fs::remove_file(d.join("harness.toml")).unwrap();
        assert!(pack(&d).is_ok());
        assert!(allowed_path(".hlock/locks/harness.pi.lock") && !allowed_path(".hlock/locks/notes.txt") && !allowed_path(".hlock/../harness.lock"));
        assert!(allowed_path(".hlock/locks/.hlock/vendor/pi/x") && allowed_path(".hlock/vendor/pi/x"));
        for bad in ["src/.hlock/vendor/x", ".hlock/locks/.hlock/vendor/", ".hlock/locks/.hlock/vendor/x/../../../../../x"] {
            assert!(!allowed_path(bad), "{bad}");
        }
        assert!(!pull::is_hlock_setup_path(".hlock/locks/.hlock/vendor/harness.y.lock"));
        assert!(!allowed_path(".hlock/env/eval/x/harness.lock") && allowed_path(".hlock/harness.x.lock"));
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
        // a PAX size bigger than the header's, which the limit wouldn't count
        let out = d.join("pax");
        std::fs::create_dir_all(&out).unwrap();
        let bytes = crate::evals_archive::pax_sized_tarball(&[("harness.lock", b"x")], "README.md", &vec![b'x'; 4096]);
        let e = unpack(&bytes, &out).unwrap_err().to_string();
        assert!(e.contains("differs from its header"), "{e}");
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
