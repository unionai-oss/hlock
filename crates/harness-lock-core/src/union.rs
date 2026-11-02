//! `hlock push/pull union:` — an artifact store on Union.ai (Flyte 2).
//!
//! A "union" source pushes a setup archive straight into a Union **Artifact** (the
//! `flyteidl2.artifact.ArtifactService` over Connect-JSON) instead of through a registry
//! HTTP server. `hlock push union:NS/NAME` and `hlock pull union:NS/NAME[@VERSION]` treat a
//! Union Artifact as a versioned, content-addressed store: each version is a separate,
//! immutable artifact named after the package, whose value is the exact setup archive
//! bytes (a `BINARY` literal).
//!
//! Where to push is taken from a Flyte config file (`admin.endpoint`, `task.org`,
//! `task.project`, `task.domain`), the same file the `flyte` CLI reads, usually at
//! `./.flyte/config.yaml` or `~/.flyte/config.yaml` (`--flyte-config PATH` overrides).
//! The credential is a Union API key in `FLYTE_API_KEY`, `base64("endpoint:client_id:client_secret:org")`.
//! We mirror the reference client in `unionai-oss/hlock-cloud` (`web/lib/union.ts`): the
//! config says where the store is, the key authenticates (OAuth `client_credentials`).
//!
//! Artifact names are `setup.<ns>.<name>` (lowercased, unsafe characters slugged) with a
//! `.h<sha256>⁸` suffix when cleaning changed a segment, so two packages never collide;
//! versions are sanitized to the characters Union allows. A version is write-once: pushing
//! a version that exists is detected and reported like a registry's 409.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::registry::{self, Dest, Ref, RootFrom};

fn err(msg: impl Into<String>) -> Error {
    Error::Union(msg.into())
}

/// The environment variable for the config file (after `--flyte-config`, before the
/// conventional `~/.flyte` / `./.flyte` paths).
pub const CONFIG_ENV: &str = "HLOCK_UNION_CONFIG";
/// The environment variable holding a Union API key (`base64("endpoint:client_id:client_secret:org")`),
/// the same key the hlock-cloud web app reads (`FLYTE_API_KEY`).
pub const KEY_ENV: &str = "FLYTE_API_KEY";

// --------------------------------------------------------------------------- //
// configuration: the flyte config file
// --------------------------------------------------------------------------- //

/// What `hlock push/pull union:` needs to find the artifact store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnionConfig {
    /// The host a store lives on, without scheme or `dns:///` (`demo.hosted.unionai.cloud`).
    pub endpoint: String,
    pub org: String,
    pub project: String,
    pub domain: String,
}

/// The user's flyte home: `~/.flyte` (or `FLYTE_CONFIG_DIR`). `pull::home_dir` is hlock's
/// own `~/.hlock`; flyte keeps its config in `~/.flyte` regardless.
fn flyte_home() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    home.join(".flyte")
}

/// The config files hlock looks in, in order: `./.flyte/config.yaml`, `~/.flyte/config.yaml`.
fn default_config_paths() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    vec![cwd.join(".flyte").join("config.yaml"), flyte_home().join("config.yaml")]
}

/// The config file to use: `--flyte-config`, else `HLOCK_UNION_CONFIG`, else the first of
/// `./.flyte/config.yaml` / `~/.flyte/config.yaml` that exists (an error if flag or env
/// named a file that doesn't exist).
pub fn resolve_config_path(flag: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = flag.map(PathBuf::from) {
        if !p.is_file() {
            return Err(err(format!("no flyte config at {}: pass a path to a config.yaml for the Union artifact store", p.display())));
        }
        return Ok(p);
    }
    if let Some(p) = std::env::var_os(CONFIG_ENV).map(PathBuf::from) {
        if !p.is_file() {
            return Err(err(format!("{} = {} but that file doesn't exist", CONFIG_ENV, p.display())));
        }
        return Ok(p);
    }
    for p in default_config_paths() {
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(err(format!(
        "no flyte config to find the Union artifact store: pass --flyte-config PATH, set {CONFIG_ENV}, or put config.yaml in ./.flyte or ~/.flyte"
    )))
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_str()
}

/// The endpoint from `admin.endpoint`, minus `dns:///` and any `http(s)://`, without a
/// trailing `/`.
fn normalize_endpoint(s: &str) -> String {
    let e = s.trim().trim_end_matches('/');
    let e = e.strip_prefix("dns:///").unwrap_or(e);
    let e = e.strip_prefix("https://").or_else(|| e.strip_prefix("http://")).unwrap_or(e);
    e.to_string()
}

/// Read `admin.endpoint` and `task.org/project/domain` out of a flyte `config.yaml`.
pub fn load_config(path: &Path) -> Result<UnionConfig> {
    let text = std::fs::read_to_string(path).map_err(|e| err(format!("can't read the flyte config {}: {e}", path.display())))?;
    let v: Value = serde_norway::from_str(&text).map_err(|e| err(format!("{} isn't valid YAML: {e}", path.display())))?;
    let endpoint = str_at(&v, &["admin", "endpoint"]).map(normalize_endpoint).filter(|e| !e.is_empty())
        .ok_or_else(|| err(format!("{} has no admin.endpoint; a Union artifact store needs the host to reach it (admin.endpoint)", path.display())))?;
    let org = str_at(&v, &["task", "org"]).unwrap_or("").to_string();
    let project = str_at(&v, &["task", "project"]).unwrap_or("").to_string();
    let domain = str_at(&v, &["task", "domain"]).unwrap_or("").to_string();
    if project.is_empty() || domain.is_empty() {
        return Err(err(format!("{} needs task.project and task.domain to name the artifact store", path.display())));
    }
    Ok(UnionConfig { endpoint, org, project, domain })
}

// --------------------------------------------------------------------------- //
// the Union API key and how we authenticate
// --------------------------------------------------------------------------- //

/// The decoded client credentials in a Union API key.
#[derive(Debug, Clone)]
struct KeyParts {
    /// The endpoint named in the key; kept for the error a mismatch deserves if we later
    /// check it against the config's. Parsed here so a malformed key fails loud.
    #[allow(dead_code)]
    endpoint: String,
    client_id: String,
    client_secret: String,
    /// The key's org; `None` when the key spells it "None". The artifact scope falls back
    /// to this when the config's `task.org` is empty.
    org: Option<String>,
}

/// `base64("endpoint:client_id:client_secret:org")`, where the endpoint may itself contain
/// colons, so the last three parts are fixed (mirrors hlock-cloud's `decodeApiKey`).
fn decode_api_key(key: &str) -> Result<KeyParts> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key.trim())
        .map_err(|_| err("FLYTE_API_KEY isn't a Union API key: it should be base64 of endpoint:client_id:client_secret:org"))?;
    let text = String::from_utf8(bytes).map_err(|_| err("FLYTE_API_KEY isn't valid UTF-8"))?;
    let mut parts: Vec<&str> = text.split(':').collect();
    if parts.len() < 4 {
        return Err(err("FLYTE_API_KEY isn't a Union API key: it should be base64 of endpoint:client_id:client_secret:org"));
    }
    let org = parts.pop().unwrap_or("");
    let secret = parts.pop().unwrap_or("");
    let client_id = parts.pop().unwrap_or("");
    let endpoint = parts.join(":").replace("dns:///", "").replace("https://", "").replace("http://", "");
    if endpoint.is_empty() || client_id.is_empty() || secret.is_empty() {
        return Err(err("FLYTE_API_KEY is missing pieces: it should be base64 of endpoint:client_id:client_secret:org"));
    }
    Ok(KeyParts { endpoint, client_id: client_id.to_string(), client_secret: secret.to_string(), org: (org != "None" && !org.is_empty()).then(|| org.to_string()) })
}

fn api_key() -> Result<KeyParts> {
    let key = std::env::var(KEY_ENV).ok().filter(|k| !k.trim().is_empty())
        .ok_or_else(|| err(format!("no Union API key: set {KEY_ENV} to base64 of endpoint:client_id:client_secret:org")))?;
    decode_api_key(&key)
}

// --------------------------------------------------------------------------- //
// naming: setup archives as artifacts
// --------------------------------------------------------------------------- //

/// A Union Artifact name for a kind and segments, exactly as hlock-cloud names them:
/// `kind.<segment>…`, lowercase, unsafe characters slugged, with a `.h<sha256>⁸` suffix
/// when cleaning changed a segment so two packages never share a name.
pub fn artifact_name(kind: &str, segments: &[&str]) -> String {
    let raw: Vec<String> = segments.iter().flat_map(|s| s.to_lowercase().split('/').map(str::to_string).collect::<Vec<_>>()).collect();
    let clean: Vec<String> = raw.iter().map(|s| {
        let c: String = s.replace(|ch: char| !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' && ch != '-', "-");
        let c = c.trim_matches('-').to_string();
        if c.is_empty() { "_".to_string() } else { c }
    }).collect();
    let name = [kind.to_string()].into_iter().chain(clean.iter().cloned()).collect::<Vec<_>>().join(".");
    let unchanged = clean.iter().zip(raw.iter()).all(|(c, r)| c == r);
    if unchanged {
        name
    } else {
        let h = crate::sha256_hex(raw.join("/").as_bytes());
        format!("{name}.h{}", &h[..8])
    }
}

/// A Union version is an opaque string; keep it to the characters elsewhere allowed.
pub fn artifact_version(version: &str) -> String {
    version.replace(|c: char| !c.is_ascii_alphanumeric() && !"._+-".contains(c), "_")
}

// --------------------------------------------------------------------------- //
// the client: OAuth + the ArtifactService over Connect-JSON
// --------------------------------------------------------------------------- //

/// A cached bearer token: the header name (`authorizationMetadataKey`, default
/// "authorization"), the value (`Bearer …`), and when it expires (unix seconds).
struct Token {
    header: String,
    value: String,
    expires: u64,
}

pub struct UnionClient {
    /// `https://{endpoint}`, what every request is rooted at.
    base: String,
    config: UnionConfig,
    key: KeyParts,
    agent: ureq::Agent,
    token: Option<Token>,
}

impl UnionClient {
    pub fn connect(config: UnionConfig) -> Result<UnionClient> {
        let key = api_key()?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .proxy(ureq::Proxy::try_from_env())
            .user_agent(format!("hlock/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        // plain http only for loopback (a local store, tests); a real Union host is always https
        let scheme = if ["localhost", "127.0.0.1", "[::1]"].iter().any(|h| config.endpoint.starts_with(h)) { "http" } else { "https" };
        Ok(UnionClient { base: format!("{scheme}://{}", config.endpoint), config, key, agent, token: None })
    }

    /// The org artifacts live in: the config's `task.org`, else the key's.
    fn org(&self) -> &str {
        if !self.config.org.is_empty() {
            &self.config.org
        } else {
            self.key.org.as_deref().unwrap_or("")
        }
    }

    fn unreachable(&self, e: impl std::fmt::Display) -> Error {
        err(format!("can't reach the Union artifact store at {}: {e}", self.config.endpoint))
    }

    fn refused(&self, status: u16, body: &str) -> Error {
        let mut msg = body.trim().chars().take(300).collect::<String>();
        if msg.is_empty() {
            msg = format!("HTTP {status}");
        }
        let hint = match status {
            401 => format!("\ncheck {KEY_ENV}: the Union API key didn't authenticate"),
            403 => format!("\n{KEY_ENV} doesn't allow writing to this project/domain; check its org and role"),
            _ => String::new(),
        };
        err(format!("the Union artifact store refused ({status}): {msg}{hint}"))
    }

    /// A GET that returns JSON (the OAuth discovery document).
    fn get_json(&self, url: &str) -> Result<Value> {
        let mut resp = self.agent.get(url).header("accept", "application/json").call().map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| self.unreachable(e))?;
        if !(200..300).contains(&status) {
            return Err(self.refused(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| err(format!("{url} returned invalid JSON: {e}")))
    }

    /// A POST whose URL and body are given verbatim (the OAuth metadata RPC).
    fn post_json(&self, url: &str, body: &str) -> Result<Value> {
        let mut resp = self.agent.post(url).header("content-type", "application/json").send(body.as_bytes().to_vec()).map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| self.unreachable(e))?;
        if !(200..300).contains(&status) {
            return Err(self.refused(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| err(format!("{url} returned invalid JSON: {e}")))
    }

    fn json(&self, url: &str, body: &str) -> Result<Value> {
        self.post_json(url, body)
    }

    /// A bearer token, cached until just before it expires.
    fn auth(&mut self) -> Result<Token> {
        if let Some(t) = &self.token {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            if t.expires > now + 30 {
                return Ok(Token { header: t.header.clone(), value: t.value.clone(), expires: t.expires });
            }
        }
        // where the token endpoint is (RFC 8414 discovery document) …
        let meta = self.get_json(&format!("{}/.well-known/oauth-authorization-server", self.base)).map_err(|_| {
            err(format!("can't discover {}'s OAuth: the host isn't a Union endpoint, or {KEY_ENV}'s endpoint doesn't match the config's", self.config.endpoint))
        })?;
        let token_endpoint = meta["tokenEndpoint"].as_str().or_else(|| meta["token_endpoint"].as_str()).map(str::to_string).ok_or_else(|| err("the Union endpoint's OAuth metadata has no tokenEndpoint"))?;
        // …and which scopes the backend wants
        let pubcfg = self.json(&format!("{}/flyteidl.service.AuthMetadataService/GetPublicClientConfig", self.base), "{}")?;
        let scopes: Vec<&str> = pubcfg["scopes"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect()).unwrap_or_default();
        let header = pubcfg["authorizationMetadataKey"].as_str().unwrap_or("authorization").to_string();
        let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{}:{}", self.key.client_id, self.key.client_secret));
        let mut resp = self.agent.post(&token_endpoint)
            .header("authorization", format!("Basic {basic}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .send(format!("grant_type=client_credentials&scope={}", scopes.join(" ")).into_bytes())
            .map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| self.unreachable(e))?;
        if !(200..300).contains(&status) {
            return Err(self.refused(status, &text));
        }
        let tok: Value = serde_json::from_str(&text).map_err(|e| err(format!("the token endpoint returned invalid JSON: {e}")))?;
        let access = tok["access_token"].as_str().ok_or_else(|| err("the token endpoint returned no access_token"))?;
        let expires = tok["expires_in"].as_u64().unwrap_or(300);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let t = Token { header, value: format!("Bearer {access}"), expires: now + expires };
        self.token = Some(Token { header: t.header.clone(), value: t.value.clone(), expires: t.expires });
        Ok(t)
    }

    fn rpc(&mut self, method: &str, body: &Value) -> Result<Value> {
        let t = self.auth()?;
        let url = format!("{}/flyteidl2.artifact.ArtifactService/{}", self.base, method);
        let mut resp = self.agent.post(&url)
            .header("content-type", "application/json")
            .header(&t.header, &t.value)
            .send(body.to_string().into_bytes())
            .map_err(|e| self.unreachable(e))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| self.unreachable(e))?;
        if !(200..300).contains(&status) {
            return Err(self.refused(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| err(format!("{} returned invalid JSON: {e}", self.config.endpoint)))
    }

    fn name(&self, name: &str) -> Value {
        json!({ "org": self.org(), "project": self.config.project, "domain": self.config.domain, "name": name })
    }

    pub fn artifact_url(&self, name: &str, version: &str) -> String {
        format!("{}/v2/domain/{}/project/{}/artifacts/{}?version={}", self.base, self.config.domain, self.config.project, name, artifact_version(version))
    }

    /// Create an immutable artifact whose value is `bytes` (a `BINARY` literal).
    pub fn create_artifact(&mut self, name: &str, version: &str, bytes: &[u8], tag: &str, description: &str, attrs: &Value) -> Result<CreateOutcome> {
        let body = json!({
            "artifactId": { "name": self.name(name), "version": artifact_version(version) },
            "spec": {
                "value": { "scalar": { "binary": { "value": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes), "tag": tag } } },
                "type": { "simple": "BINARY" },
                "info": { "description": description, "userMetadata": attrs }
            }
        });
        match self.rpc("CreateArtifact", &body) {
            Ok(_) => Ok(CreateOutcome::Created),
            Err(Error::Union(m)) if m.to_lowercase().contains("already exists") => Ok(CreateOutcome::AlreadyExists),
            Err(e) => Err(e),
        }
    }

    /// Read the artifact back: its bytes and metadata, or `None` if there's no such version.
    pub fn get_artifact(&mut self, name: &str, version: &str) -> Result<Option<(Vec<u8>, String, Value)>> {
        let body = json!({ "name": self.name(name), "version": artifact_version(version) });
        let r = match self.rpc("GetArtifact", &body) {
            Ok(v) => v,
            Err(Error::Union(m)) if m.to_lowercase().contains("not found") => return Ok(None),
            Err(e) => return Err(e),
        };
        let art = &r["artifact"];
        let bin = art.pointer("/spec/value/scalar/binary").and_then(|b| b["value"].as_str());
        let tag = art.pointer("/spec/value/scalar/binary").and_then(|b| b["tag"].as_str()).unwrap_or("").to_string();
        let attrs = art.pointer("/spec/info/userMetadata").cloned().unwrap_or(Value::Null);
        match bin {
            Some(b) => {
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b).map_err(|_| err(format!("{name}@{version}'s artifact isn't valid base64")))?;
                Ok(Some((bytes, tag, attrs)))
            }
            None => Err(err(format!("{name}@{version}'s artifact has no binary value; it isn't a setup archive"))),
        }
    }
}

/// What creating an artifact did: written, or already there (Union writes a version once).
pub enum CreateOutcome {
    Created,
    AlreadyExists,
}

// --------------------------------------------------------------------------- //
// specs: `union:NS/NAME[//PATH][@VERSION]`
// --------------------------------------------------------------------------- //

/// True if `arg` names a Union source (`union:…`) rather than a git, registry or local lock.
pub fn is_source(arg: &str) -> bool {
    arg.starts_with("union:")
}

fn bad(spec: &str, why: &str) -> Error {
    err(format!("can't read union source {spec:?}: {why}"))
}

/// The `union:NS/NAME[//PATH][@VERSION]` reference (the `Ref` without the `//PATH` guard).
pub fn parse_ref(spec: &str) -> Result<RegistryRef> {
    let rest = spec.strip_prefix("union:").ok_or_else(|| bad(spec, "a union source starts with union: NS/NAME[@VERSION]"))?;
    let r = registry::parse_ref(rest).ok_or_else(|| bad(spec, "expected union:NAMESPACE/NAME[//PATH][@VERSION]"))?;
    let name = artifact_name("setup", &[&r.namespace, &r.name]);
    Ok(RegistryRef { r, name, source_spec: spec.to_string() })
}

/// A parsed union source: the package ref plus the artifact it maps to.
pub struct RegistryRef {
    pub r: Ref,
    /// The artifact name (`setup.<ns>.<name>`).
    pub name: String,
    /// The source as typed.
    pub source_spec: String,
}

// --------------------------------------------------------------------------- //
// push
// --------------------------------------------------------------------------- //

/// The description and attributes on each artifact: useful provenance in the Union UI.
fn attrs_for(pkg: &RegistryRef, digest: &str, config: &UnionConfig) -> Value {
    json!({
        "package": format!("{}/{}", pkg.r.namespace, pkg.r.name),
        "digest": digest,
        "url": format!("union://{}/{}/{}", config.endpoint, config.project, config.domain),
    })
}

/// `hlock push union:NS/NAME`: pack this project's setup and store it as a Union Artifact
/// at `version`. Mirrors `registry::push` in what it packs and how it defaults the name and
/// version, but the archive is written straight to the artifact store.
#[allow(clippy::too_many_arguments)]
pub fn push(root: Option<&Path>, config_flag: Option<&str>, name: Option<&str>, version: Option<&str>, dry_run: bool, no_evals: bool) -> Result<Value> {
    let cwd = std::env::current_dir().map_err(|e| err(format!("can't read the working directory: {e}")))?;
    let (dir, root_from, git_warning) = registry::push_root(root, &cwd);
    let dir = dir.as_path();
    if root_from == RootFrom::Repository && !registry::has_setup(dir) {
        return Err(err(format!(
            "the repository root {} has nothing a setup archive is made of; pass --root DIR to push another directory",
            dir.display()
        )));
    }
    let mut warnings: Vec<String> = git_warning.into_iter().collect();
    let config_path = resolve_config_path(config_flag)?;
    let config = load_config(&config_path)?;
    let packed = registry::pack(dir)?;
    warnings.extend(packed.warnings.iter().cloned());
    let evals = if no_evals { None } else { crate::evals_archive::pack(dir)? };
    if let Some(e) = &evals {
        warnings.extend(e.warnings.iter().cloned());
    }
    let package = match name {
        Some(n) => n.to_string(),
        None => format!("<ns>/{}", packed.project),
    };
    let version = match version {
        Some(v) => v.to_string(),
        None => match &evals {
            Some(e) => {
                let d = crate::hash::sha256_text(&format!("{}\n{}", packed.digest, e.digest));
                format!("sha-{}", &d[d.len() - 64..][..12])
            }
            None => packed.digest["sha256:".len()..].to_string(),
        },
    };
    let mut out = json!({
        "package": package.clone(),
        "provider": "union",
        "version": version.clone(),
        "digest": packed.digest,
        "size_bytes": packed.size_bytes,
        "files": packed.files,
        "warnings": warnings,
        "name_from": if name.is_some() { Value::Null } else { json!(packed.name_from) },
        "root": registry::shown_path(dir),
        "root_from": root_from,
        "store": { "endpoint": config.endpoint, "project": config.project, "domain": config.domain, "org": config.org },
    });
    if let Some(e) = &evals {
        out["evals"] = json!({ "digest": e.digest, "size_bytes": e.size_bytes, "packs": e.packs, "files": e.files });
    }
    if dry_run {
        out["dry_run"] = json!(true);
        return Ok(out);
    }
    let Some(n) = name else {
        return Err(err("union push needs an explicit NAMESPACE/NAME: `hlock push union:NS/NAME` (Union has no whoami to default it)"));
    };
    let pkg = parse_ref(&format!("union:{n}"))?;
    if pkg.r.path.is_some() {
        return Err(err("--root and //PATH and aren't for union push; push the whole setup"));
    }
    let mut client = UnionClient::connect(config)?;
    let ver = artifact_version(&version);
    let url = client.artifact_url(&pkg.name, &ver);
    out["url"] = json!(url);
    // the setup archive
    match client.create_artifact(&pkg.name, &ver, &packed.bytes, &packed.digest, &format!("hlock setup {}", package), &attrs_for(&pkg, &packed.digest, &client.config)) {
        Ok(CreateOutcome::Created) => out["published"] = json!(true),
        Ok(CreateOutcome::AlreadyExists) => {
            // a version that exists must hold the same content, or the version can't change
            let (bytes, _, _) = client.get_artifact(&pkg.name, &ver)?.ok_or_else(|| err("the version already exists but couldn't be read back"))?;
            let got = format!("sha256:{}", crate::sha256_hex(&bytes));
            if got != packed.digest {
                return Err(err(format!(
                    "{package}@{version} is already stored with other content ({}); Union artifacts are immutable. Push with another --version",
                    &got[..got.len().min(19)]
                )));
            }
            out["published"] = json!(false);
            out["already_published"] = json!(true);
        }
        Err(e) => return Err(e),
    }
    let Some(e) = &evals else { return Ok(out) };
    // eval packs: a sibling artifact at the same version (write-once too)
    let eval_name = format!("{}.evals", pkg.name);
    match client.create_artifact(&eval_name, &ver, &e.bytes, &e.digest, &format!("hlock eval packs for {package}"), &attrs_for(&pkg, &e.digest, &client.config)) {
        Ok(CreateOutcome::Created) => out["evals"]["published"] = json!(true),
        Ok(CreateOutcome::AlreadyExists) => out["evals"]["already_published"] = json!(true),
        Err(e) => return Err(e),
    }
    Ok(out)
}


// --------------------------------------------------------------------------- //
// pull
// --------------------------------------------------------------------------- //

/// `hlock pull union:NS/NAME[//PATH][@VERSION]`: fetch the version's artifact, verify its
/// digest, and place it like a registry pull (`[pulled] from = union:…`).
#[allow(clippy::too_many_arguments)]
pub fn pull(spec: &str, config_flag: Option<&str>, dest: Dest<'_>, force: bool, allow_hidden: &[String], no_evals: bool) -> Result<Value> {
    let pkg = parse_ref(spec)?;
    let version = pkg.r.version.clone().ok_or_else(|| err("a union pull needs an explicit @VERSION: `hlock pull union:NS/NAME@VERSION` (Union has no 'latest' without a registry tracking versions)"))?;
    let config = load_config(&resolve_config_path(config_flag)?)?;
    let mut client = UnionClient::connect(config)?;
    let ver = artifact_version(&version);
    let (bytes, _, _) = client.get_artifact(&pkg.name, &ver)?.ok_or_else(|| err(format!("no {}@{version} in the Union artifact store", pkg.r.package())))?;
    let expected = format!("sha256:{}", crate::sha256_hex(&bytes));
    let scratch = crate::pull::scratch_dir()?;
    let files = registry::unpack(&bytes, &scratch.0)?;
    let (lock_path, others) = match dest {
        Dest::Target(_) => choose_lock(&pkg, &files)?,
        Dest::Into(_) => (crate::LOCKFILE_PATH.to_string(), Vec::new()),
    };
    let chosen = Ref { path: (lock_path != crate::LOCKFILE_PATH).then(|| lock_path.clone()), ..pkg.r.clone() };
    let mut pulled = json!({
        "from": format!("union:{}/{}@{}", pkg.r.namespace, pkg.r.name, version),
        "url": format!("{}/{}", self_url(&client), pkg.name),
        "path": lock_path,
        "version": version,
        "digest": expected,
    });
    if let Some(v) = &pkg.r.version {
        pulled["ref"] = json!(v);
    }
    let (eval_packs, eval_files) = match dest {
        Dest::Into(_) if !no_evals => {
            let eval_name = format!("{}.evals", pkg.name);
            match client.get_artifact(&eval_name, &ver)? {
                Some((ebytes, _, _)) => {
                    let digest = format!("sha256:{}", crate::sha256_hex(&ebytes));
                    let manifest = std::fs::read_to_string(scratch.0.join("harness.toml")).ok().and_then(|t| toml::from_str::<toml::Value>(&t).ok());
                    let declared = manifest.as_ref().map(crate::evals_archive::declared_packs).unwrap_or_default();
                    pulled["evals-digest"] = json!(digest);
                    crate::evals_archive::unpack(&ebytes, &scratch.0, &declared)?
                }
                None => (Vec::new(), Vec::new()),
            }
        }
        _ => (Vec::new(), Vec::new()),
    };
    let src = crate::pull::Source {
        spec: pkg.source_spec.clone(),
        url: format!("union://{}/{}/{}/{}", client.config.endpoint, client.config.project, client.config.domain, pkg.name),
        fetch_url: String::new(),
        reference: pkg.r.version.clone(),
        path: chosen.path.clone(),
        owner: pkg.r.name.clone(),
    };
    let others = others.into_iter().map(|p| crate::pull::OtherLock { source: Ref { path: Some(p.clone()), ..pkg.r.clone() }.spec(), path: p }).collect();
    let lock = scratch.0.join(&lock_path);
    let fetched = crate::pull::Fetched { src, root: &scratch.0, commit: version.clone(), remote_lock: lock.is_file().then_some(lock), others, git: false, pulled, digest: Some(expected), eval_packs, eval_files };
    let v = match dest {
        Dest::Target(t) => serde_json::to_value(crate::pull::place(fetched, t, force, allow_hidden)?)?,
        Dest::Into(d) => {
            let base = scratch.0.clone();
            serde_json::to_value(crate::pull::place_into(fetched, &base, d, force, allow_hidden)?)?
        }
    };
    Ok(v)
}

fn self_url(client: &UnionClient) -> String {
    format!("{}://{}/{}/{}", "union", client.config.endpoint, client.config.project, client.config.domain)
}

/// which lock PATH (or the only one) a `union:` pull places — same rule as a registry pull.
fn choose_lock(r: &RegistryRef, files: &[String]) -> Result<(String, Vec<String>)> {
    let package = r.r.package();
    // a package's locks: the root `harness.lock` or `harness.*.lock`, or a nested harness lock
    let is_lock = |p: &String| p.ends_with(".lock") && ((!p.contains('/') && crate::pull::is_harness_file(p)) || crate::pull::is_hlock_setup_path(p));
    let locks: Vec<String> = files.iter().filter(|p| is_lock(p)).cloned().collect();
    let here_in = |d: &str| locks.iter().filter(|l| crate::pull::parent_dir(l) == d).cloned().collect::<Vec<_>>();
    let picked = match r.r.path.as_deref() {
        Some(p) if p.ends_with(".lock") => {
            if locks.contains(&p.to_string()) {
                p.to_string()
            } else {
                return Err(err(format!("{package} has no lock {p}")));
            }
        }
        Some(d) => {
            let here = here_in(d);
            if here.len() != 1 {
                return Err(err(format!("{package} has {} lock(s) in {d}/; pick one with //PATH", here.len())));
            }
            here[0].clone()
        }
        None => {
            if locks.contains(&crate::LOCKFILE_PATH.to_string()) {
                crate::LOCKFILE_PATH.to_string()
            } else if locks.len() == 1 {
                locks[0].clone()
            } else if locks.is_empty() {
                return Err(err(format!("{package} has no harness.lock")));
            } else {
                return Err(err(format!("{package} has {} locks; pick one with //PATH", locks.len())));
            }
        }
    };
    let others = locks.into_iter().filter(|l| *l != picked).collect();
    Ok((picked, others))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_name_slugs_and_disambiguates() {
        assert_eq!(artifact_name("setup", &["alice", "hlock"]), "setup.alice.hlock");
        // a dot in a segment is cleaned and a short hash disambiguates
        let n = artifact_name("setup", &["acme", "my.repo"]).to_string();
        assert!(n.starts_with("setup.acme.my-repo.h"), "got {n}");
        assert_eq!(artifact_name("setup", &["acme", "my-repo"]), "setup.acme.my-repo");
        assert_eq!(artifact_name("setup", &["a/b", "c"]), "setup.a.b.c");
        assert_eq!(artifact_name("setup", &["hi", "x_y"]), "setup.hi.x_y");
    }

    #[test]
    fn artifact_version_sanitizes() {
        assert_eq!(artifact_version("1.2.0"), "1.2.0");
        assert_eq!(artifact_version("sha-abc123"), "sha-abc123");
        assert_eq!(artifact_version("v 1/2"), "v_1_2");
    }

    #[test]
    fn decode_api_key_last_three_parts_are_fixed() {
        let key = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "demo.hosted.unionai.cloud:client:sec:demo");
        let k = decode_api_key(&key).unwrap();
        assert_eq!(k.endpoint, "demo.hosted.unionai.cloud");
        assert_eq!(k.client_id, "client");
        assert_eq!(k.client_secret, "sec");
        assert_eq!(k.org.as_deref(), Some("demo"));
        // the org may be None → empty
        let key2 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "h:cid:csec:None");
        let k2 = decode_api_key(&key2).unwrap();
        assert_eq!(k2.org, None);
        assert!(decode_api_key("not-base64").is_err());
    }

    #[test]
    fn config_is_flyte_style() {
        let d = std::env::temp_dir().join(format!("hlock-union-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.yaml");
        std::fs::write(&p, "admin:\n  endpoint: dns:///demo.hosted.unionai.cloud\nimage:\n  builder: remote\ntask:\n  org: demo\n  project: hlock-cloud\n  domain: development\n").unwrap();
        let c = load_config(&p).unwrap();
        assert_eq!(c.endpoint, "demo.hosted.unionai.cloud");
        assert_eq!(c.org, "demo");
        assert_eq!(c.project, "hlock-cloud");
        assert_eq!(c.domain, "development");
        // no task.org is fine (falls back to the key's); no project is not
        std::fs::write(&p, "admin:\n  endpoint: dns:///h.example.com\ntask:\n  domain: development\n").unwrap();
        assert!(load_config(&p).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    // E2E: a tiny in-test Union artifact store on 127.0.0.1 (OAuth + the artifact RPCs), so
    // push then pull round-trip through the whole client. A Version-less project (no evals)
    // writes exactly one setup artifact, which pull then reads back.
    mod e2e {
        use serde_json::Value;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        fn raw_str(b: &[u8]) -> std::borrow::Cow<'_, str> {
            String::from_utf8_lossy(b)
        }

        pub struct Store {
            pub artifact: Option<(Vec<u8>, String)>,
        }

        pub fn serve(store: Arc<Mutex<Store>>) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}");
            let base2 = base.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut s) = stream else { continue };
                    // read headers + body by Content-Length so a split or large body arrives whole
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 8192];
                    'req: loop {
                        let n = match s.read(&mut buf) {
                            Ok(0) | Err(_) => break 'req,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                        if let Some(hdr) = raw_str(&raw).find("\r\n\r\n") {
                            let head = raw_str(&raw)[..hdr].to_string();
                            let len: usize = head.split("\r\n").find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                            if raw.len() >= hdr + 4 + len {
                                break 'req;
                            }
                        }
                    }
                    let req = String::from_utf8_lossy(&raw).to_string();
                    let line = req.lines().next().unwrap_or("").to_string();
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = match path {
                        p if p.ends_with("/.well-known/oauth-authorization-server") => {
                            format!(r#"{{"tokenEndpoint":"{base2}/token"}}"#)
                        }
                        p if p.ends_with("/flyteidl.service.AuthMetadataService/GetPublicClientConfig") => {
                            r#"{"scopes":["all"],"authorizationMetadataKey":"authorization"}"#.to_string()
                        }
                        p if p.ends_with("/token") => r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
                        p if p.ends_with("/flyteidl2.artifact.ArtifactService/CreateArtifact") => {
                            let sep = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
                            let json: Value = serde_json::from_str(&req[sep..]).unwrap_or(Value::Null);
                            let bin = &json["spec"]["value"]["scalar"]["binary"];
                            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, bin["value"].as_str().unwrap_or("")).unwrap();
                            let tag = bin["tag"].as_str().unwrap_or("").to_string();
                            store.lock().unwrap().artifact = Some((bytes, tag));
                            r#"{}"#.to_string()
                        }
                        p if p.ends_with("/flyteidl2.artifact.ArtifactService/GetArtifact") => {
                            let st = store.lock().unwrap().artifact.clone().unwrap_or((Vec::new(), String::new()));
                            format!(
                                r#"{{"artifact":{{"spec":{{"value":{{"scalar":{{"binary":{{"value":"{}","tag":"{}"}}}}}}}}}}}}"#,
                                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &st.0),
                                st.1
                            )
                        }
                        _ => r#"{}"#.to_string(),
                    };
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes());
                }
            });
            base
        }
    }

    #[test]
    fn push_then_pull_round_trips_through_a_store() {
        use crate::pull::Target;
        use std::sync::{Arc, Mutex};
        let d = std::env::temp_dir().join(format!("hlock-union-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".flyte")).unwrap();
        std::fs::create_dir_all(d.join("proj")).unwrap();
        std::fs::write(d.join("proj/harness.lock"), "lock-version = 1\ngenerated-by = \"t\"\ngenerated-at = \"\"\n[harness]\nname = \"pi\"\n").unwrap();
        std::fs::write(d.join("proj/harness.toml"), "[project]\nname = \"demo\"\n").unwrap();

        let store = Arc::new(Mutex::new(e2e::Store { artifact: None }));
        let base = e2e::serve(store.clone());
        let port = base.rsplit(':').next().unwrap();
        let endpoint = format!("127.0.0.1:{port}");
        std::fs::write(d.join(".flyte/config.yaml"), format!("admin:\n  endpoint: {endpoint}\ntask:\n  org: demo\n  project: hlock-cloud\n  domain: development\n")).unwrap();
        std::env::set_var("FLYTE_API_KEY", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{endpoint}:client:secret:demo")));
        let root = d.join("proj").to_path_buf();
        // dry-run: no network
        let dry = push(Some(&root), Some(d.join(".flyte/config.yaml").to_str().unwrap()), Some("acme/review"), None, true, true).unwrap();
        assert_eq!(dry["provider"], "union");
        // real push
        let pushed = push(Some(&root), Some(d.join(".flyte/config.yaml").to_str().unwrap()), Some("acme/review"), Some("1.0"), false, true).unwrap();
        assert_eq!(pushed["published"], true);
        let version = pushed["version"].as_str().unwrap().to_string();
        // pull into a temp project lock file (never the real user store)
        let spec = format!("union:acme/review@{version}");
        let out = pull(&spec, Some(d.join(".flyte/config.yaml").to_str().unwrap()), Dest::Target(&Target::File(d.join("pulled.lock"))), false, &[], true).unwrap();
        assert_eq!(out["version"].as_str().unwrap(), &version);
        let _ = std::fs::remove_dir_all(&d);
        std::env::remove_var("FLYTE_API_KEY");
    }

    #[test]
    fn parse_ref_validates() {
        let r = parse_ref("union:acme/review-bot@1.2.0").unwrap();
        assert_eq!(r.r.package(), "acme/review-bot");
        assert_eq!(r.r.version.as_deref(), Some("1.2.0"));
        assert_eq!(r.name, "setup.acme.review-bot");
        assert!(parse_ref("union:acme").is_err());
        assert!(parse_ref("acme/review-bot").is_err());
    }
}
