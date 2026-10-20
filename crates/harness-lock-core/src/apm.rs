//! APM interop (`specs/dependencies.md` §6): `apm.yml` as a manifest hlock reads,
//! `apm.lock.yaml` as an input, `apm:` packages resolved with OpenAPM v0.1's rules, and
//! `hlock export --format apm-lock`.
//!
//! Resolution follows the OpenAPM v0.1 spec, where it and APM's own CLI differ:
//! breadth-first in declaration order, the highest version every constraint on a package
//! allows (an empty intersection fails, naming both chains), `conflict_resolution: nest`
//! refused, and a depth cap of 50 that fails rather than skips. What hlock *writes* for
//! APM to read (`apm.lock.yaml`) follows APM's implementation, so `apm install --frozen`
//! reads it as its own: `owner/repo` with a separate `host`, entries sorted by depth and
//! repository, and APM's own `content_hash` and per-file hashes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::deps::{self, Decl, Entry, Env, GitSource, Kind, Pick, Resolved, Source, VersionRange};
use crate::error::{Error, Result};
use crate::lockfile::{Declared, Instructions, Lockfile, Package, Skill, Tool};

/// The OpenAPM version hlock resolves by (recorded as `[provenance] openapm`).
pub const OPENAPM_VERSION: &str = "0.1";
/// APM's manifest and lockfile names.
pub const APM_MANIFEST: &str = "apm.yml";
pub const APM_LOCK: &str = "apm.lock.yaml";
/// How deep a dependency chain may go (OpenAPM req-rs-006).
pub const MAX_DEPTH: u32 = 50;
/// The header `hlock export` writes, which `check` looks for to report a stale export.
pub const EXPORT_HEADER: &str = "# generated_by: hlock";

fn err(msg: impl Into<String>) -> Error {
    Error::Dependency(msg.into())
}

// --------------------------------------------------------------------------- //
// apm.yml
// --------------------------------------------------------------------------- //

/// An `apm.yml`: what hlock reads from it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ApmManifest {
    pub name: String,
    pub version: String,
    pub license: Option<String>,
    /// `targets:` (or `target:`), as APM names harnesses (`claude`, `copilot`, …).
    pub targets: Vec<String>,
    pub deps: Vec<ApmDep>,
    pub mcp: Vec<ApmMcp>,
}

/// One `dependencies.apm` entry.
#[derive(Debug, Clone, Serialize)]
pub struct ApmDep {
    /// As written (a string, or an object's `git`/`path`).
    pub raw: String,
    /// The hlock source it stands for: `apm:…`, `git+…`, or a path.
    pub spec: String,
    pub alias: Option<String>,
    pub prerelease: bool,
    /// A registry package (`id:`), which hlock doesn't resolve yet.
    pub registry: Option<String>,
    /// `git: parent`: a path in the declaring package's own repository.
    pub parent_path: Option<String>,
}

/// One `dependencies.mcp` entry: a registry reference, or a self-defined server.
#[derive(Debug, Clone, Serialize)]
pub struct ApmMcp {
    pub name: String,
    /// From the MCP registry (a reference hlock records but can't configure by itself).
    pub registry: bool,
    /// A self-defined server, as a config entry (`command`/`args`/`env`, or `url`).
    pub entry: Value,
}

/// Reject YAML anchors and aliases (OpenAPM req-mf-020): `&name` and `*name` where a
/// value starts, outside quotes and comments.
fn check_safe_yaml(text: &str) -> Result<()> {
    for (n, line) in text.lines().enumerate() {
        let mut quote: Option<char> = None;
        let mut prev = ' ';
        for c in line.chars() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '#' && (prev == ' ' || prev == '\t') => break,
                None if c == '"' || c == '\'' => quote = Some(c),
                None if (c == '&' || c == '*') && matches!(prev, ' ' | '\t' | '[' | ',' | '{' | '-' | ':') => {
                    return Err(err(format!("apm.yml line {}: YAML anchors and aliases ({c}…) aren't allowed", n + 1)));
                }
                None => {}
            }
            prev = c;
        }
    }
    Ok(())
}

/// `[host/]owner/repo[/virtual/path][#ref]`, a git URL, or a local path → an hlock spec.
fn dep_spec(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with("~/") || s.starts_with(".\\") || s.starts_with("..\\") {
        return Ok(s.replace('\\', "/"));
    }
    let (body, r) = match s.split_once('#') {
        Some((b, r)) => (b, Some(r)),
        None => (s, None),
    };
    let at = r.map(|r| format!("#{r}")).unwrap_or_default();
    // URL forms: https://host/owner/repo[.git][/vpath], ssh://git@host[:port]/…, git@host:owner/repo
    let rest = body.strip_prefix("https://").or_else(|| body.strip_prefix("http://")).or_else(|| body.strip_prefix("ssh://git@")).or_else(|| body.strip_prefix("git@"));
    if let Some(rest) = rest {
        let rest = rest.replacen(':', "/", if body.starts_with("git@") { 1 } else { 0 });
        let segs: Vec<&str> = rest.split('/').filter(|x| !x.is_empty()).collect();
        if segs.len() < 3 {
            return Err(err(format!("{raw:?} isn't HOST/OWNER/REPO")));
        }
        let host = segs[0].split(':').next().unwrap_or(segs[0]);
        let repo = segs[2].trim_end_matches(".git");
        let vpath = segs[3..].join("/");
        return Ok(apm_spec(host, segs[1], repo, &vpath, &at));
    }
    let segs: Vec<&str> = body.split('/').filter(|x| !x.is_empty()).collect();
    let (host, segs) = match segs.first() {
        Some(h) if h.contains('.') => (*h, &segs[1..]),
        _ => ("github.com", &segs[..]),
    };
    if segs.len() < 2 {
        return Err(err(format!("{raw:?} isn't [HOST/]OWNER/REPO")));
    }
    Ok(apm_spec(host, segs[0], segs[1].trim_end_matches(".git"), &segs[2..].join("/"), &at))
}

fn apm_spec(host: &str, owner: &str, repo: &str, vpath: &str, at: &str) -> String {
    let sub = if vpath.is_empty() { String::new() } else { format!("/{vpath}") };
    if host == "github.com" {
        format!("apm:{owner}/{repo}{sub}{at}")
    } else {
        format!("apm:{host}/{owner}/{repo}{sub}{at}")
    }
}

const DEP_KEYS: [&str; 8] = ["git", "id", "registry", "version", "ref", "path", "alias", "prerelease"];

fn parse_dep(v: &Value) -> Result<ApmDep> {
    if let Some(s) = v.as_str() {
        return Ok(ApmDep { raw: s.to_string(), spec: dep_spec(s)?, alias: None, prerelease: false, registry: None, parent_path: None });
    }
    let m = v.as_object().ok_or_else(|| err(format!("a dependency must be a string or a mapping, not {v}")))?;
    for k in m.keys() {
        if !DEP_KEYS.contains(&k.as_str()) && !k.starts_with("x-") && !matches!(k.as_str(), "skills" | "type" | "allow_insecure" | "targets") {
            return Err(err(format!("dependency field {k:?} isn't one APM defines")));
        }
    }
    let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
    let alias = s("alias");
    if let Some(a) = &alias {
        if !crate::fsutil::plain_name(a) {
            return Err(err(format!("alias {a:?} must be a plain name")));
        }
    }
    let prerelease = m.get("prerelease").and_then(Value::as_bool).unwrap_or(false);
    if let Some(id) = s("id") {
        if m.contains_key("git") {
            return Err(err("a dependency has `git` or `id`, not both"));
        }
        return Ok(ApmDep { raw: id.clone(), spec: String::new(), alias, prerelease, registry: Some(id), parent_path: None });
    }
    match (s("git"), s("path")) {
        (Some(g), path) if g == "parent" => {
            let p = path.ok_or_else(|| err("`git: parent` needs a `path`"))?;
            Ok(ApmDep { raw: format!("parent/{p}"), spec: String::new(), alias, prerelease, registry: None, parent_path: Some(p) })
        }
        (Some(g), path) => {
            if m.contains_key("version") {
                return Err(err("a git dependency takes `ref`, not `version`"));
            }
            let mut spec = dep_spec(&g)?;
            let (base, r) = match spec.split_once('#') {
                Some((b, r)) => (b.to_string(), Some(r.to_string())),
                None => (spec.clone(), None),
            };
            spec = base;
            if let Some(p) = path.filter(|p| !p.is_empty()) {
                spec = format!("{spec}/{}", p.trim_matches('/'));
            }
            if let Some(r) = s("ref").or(r) {
                spec = format!("{spec}#{r}");
            }
            Ok(ApmDep { raw: g, spec, alias, prerelease, registry: None, parent_path: None })
        }
        (None, Some(p)) => Ok(ApmDep { raw: p.clone(), spec: dep_spec(&p)?, alias, prerelease, registry: None, parent_path: None }),
        (None, None) if s("registry").is_some() => Err(err("a registry dependency needs an `id`")),
        (None, None) => Err(err("a dependency needs `git`, `id`, `path` or `registry`")),
    }
}

fn parse_mcp(v: &Value) -> Result<ApmMcp> {
    if let Some(s) = v.as_str() {
        return Ok(ApmMcp { name: s.to_string(), registry: true, entry: Value::Null });
    }
    let m = v.as_object().ok_or_else(|| err(format!("an MCP dependency must be a string or a mapping, not {v}")))?;
    let name = m.get("name").and_then(Value::as_str).ok_or_else(|| err("an MCP dependency needs a `name`"))?.to_string();
    let registry = !matches!(m.get("registry"), Some(Value::Bool(false)));
    let mut entry = serde_json::Map::new();
    for k in ["command", "args", "env", "url", "headers", "cwd"] {
        if let Some(x) = m.get(k) {
            entry.insert(k.to_string(), x.clone());
        }
    }
    if let Some(t) = m.get("transport").or_else(|| m.get("type")) {
        entry.insert("type".into(), t.clone());
    }
    if !registry && !entry.contains_key("command") && !entry.contains_key("url") {
        return Err(err(format!("MCP server {name}: a self-defined server needs `command` or `url`")));
    }
    Ok(ApmMcp { name, registry, entry: Value::Object(entry) })
}

/// Parse an `apm.yml`.
pub fn parse_manifest(text: &str) -> Result<ApmManifest> {
    check_safe_yaml(text)?;
    let v: Value = serde_norway::from_str(text).map_err(|e| err(format!("invalid apm.yml: {e}")))?;
    let m = v.as_object().ok_or_else(|| err("apm.yml must be a mapping"))?;
    let str_of = |k: &str| m.get(k).and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_f64().map(|f| f.to_string())));
    let name = str_of("name").filter(|n| !n.trim().is_empty()).ok_or_else(|| err("apm.yml needs a `name`"))?;
    let version = str_of("version").ok_or_else(|| err("apm.yml needs a `version`"))?;
    if m.contains_key("target") && m.contains_key("targets") {
        return Err(err("apm.yml has both `target` and `targets`"));
    }
    if let Some(regs) = m.get("registries").and_then(Value::as_object) {
        for (rname, r) in regs {
            let Some(r) = r.as_object() else { continue };
            for k in r.keys() {
                if !matches!(k.as_str(), "url" | "insecure" | "aliases") && !k.starts_with("x-") {
                    return Err(err(format!("registries.{rname}: {k:?} isn't a registry field")));
                }
            }
            if let Some(u) = r.get("url").and_then(Value::as_str) {
                if !(u.starts_with("https://") || u.starts_with("http://")) {
                    return Err(err(format!("registries.{rname}: {u:?} isn't an http(s) URL")));
                }
            }
        }
    }
    let targets = match m.get("targets").or_else(|| m.get("target")) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    let deps_block = m.get("dependencies").cloned().unwrap_or(Value::Null);
    if let Some(c) = deps_block.get("conflict_resolution").and_then(Value::as_str) {
        if c != "intersection-pick" {
            return Err(err(format!("conflict_resolution: {c} isn't supported (OpenAPM v0.1 reserves it); only intersection-pick")));
        }
    }
    let mut deps = Vec::new();
    for (i, d) in deps_block.get("apm").and_then(Value::as_array).into_iter().flatten().enumerate() {
        deps.push(parse_dep(d).map_err(|e| err(format!("dependencies.apm[{i}]: {e}")))?);
    }
    let mut mcp = Vec::new();
    for (i, d) in deps_block.get("mcp").and_then(Value::as_array).into_iter().flatten().enumerate() {
        mcp.push(parse_mcp(d).map_err(|e| err(format!("dependencies.mcp[{i}]: {e}")))?);
    }
    Ok(ApmManifest { name, version, license: str_of("license"), targets, deps, mcp })
}

/// The project's `apm.yml`, if it has one.
pub fn read_project_manifest(project: &Path) -> Result<Option<ApmManifest>> {
    let p = project.join(APM_MANIFEST);
    match std::fs::read_to_string(&p) {
        Ok(t) => parse_manifest(&t).map(Some).map_err(|e| err(format!("{}: {e}", p.display()))),
        Err(_) => Ok(None),
    }
}

/// The name a package is known by: its alias, else the last segment of its path or repo.
pub fn package_name(d: &ApmDep, src: &Source) -> String {
    if let Some(a) = &d.alias {
        return a.clone();
    }
    deps::default_name(src)
}

/// The dependencies the project's `apm.yml` declares, as declarations of packages and
/// MCP servers (`from = "apm.yml"`).
pub fn declared(project: &Path) -> Result<Vec<Decl>> {
    let Some(m) = read_project_manifest(project)? else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for d in &m.deps {
        if let Some(id) = &d.registry {
            return Err(err(format!("apm.yml: {id} is a registry package; hlock resolves git and local packages only")));
        }
        if d.parent_path.is_some() {
            return Err(err("apm.yml: `git: parent` is only for a package's own dependencies"));
        }
        let src = deps::parse(&d.spec, Kind::Package)?;
        let src = match src {
            Source::Git(mut g) => {
                g.prerelease = d.prerelease;
                Source::Git(g)
            }
            other => other,
        };
        out.push(Decl { kind: Kind::Package, name: package_name(d, &src), spec: d.spec.clone(), source: Some(src), scope: "project".into(), editable: false, from: APM_MANIFEST.into() });
    }
    // the project's own MCP servers
    for m in &m.mcp {
        let src = Source::Defined { name: m.name.clone(), entry: (!m.registry).then(|| m.entry.clone()) };
        let spec = if m.registry { format!("mcp-registry:{}", m.name) } else { format!("apm-mcp:{}", m.name) };
        out.push(Decl { kind: Kind::Mcp, name: deps::default_name(&src), spec, source: Some(src), scope: "project".into(), editable: false, from: APM_MANIFEST.into() });
    }
    Ok(out)
}

// --------------------------------------------------------------------------- //
// apm.lock.yaml (reading)
// --------------------------------------------------------------------------- //

/// One `apm.lock.yaml` dependency: what hlock reads from it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ApmLocked {
    pub repo_url: String,
    pub host: Option<String>,
    pub resolved_commit: Option<String>,
    pub resolved_ref: Option<String>,
    pub virtual_path: Option<String>,
    pub depth: Option<u32>,
    pub constraint: Option<String>,
    pub resolved_tag: Option<String>,
    pub content_hash: Option<String>,
    pub deployed_files: Vec<String>,
    pub source: Option<String>,
    pub local_path: Option<String>,
}

impl ApmLocked {
    /// `host/owner/repo[/virtual/path]`, lowercased: what identifies a package in both tools.
    pub fn identity(&self) -> String {
        let repo = self.repo_url.trim_end_matches(".git").to_lowercase();
        let has_host = repo.split('/').next().is_some_and(|h| h.contains('.'));
        let base = match (&self.host, has_host) {
            (_, true) => repo,
            (Some(h), false) => format!("{}/{repo}", h.to_lowercase()),
            (None, false) => format!("github.com/{repo}"),
        };
        match self.virtual_path.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => format!("{base}/{}", v.trim_matches('/')),
            None => base,
        }
    }
}

/// Parse an `apm.lock.yaml` (`lockfile_version` "1" or "2"; unknown fields are ignored).
pub fn parse_lock(text: &str) -> Result<Vec<ApmLocked>> {
    let v: Value = serde_norway::from_str(text).map_err(|e| err(format!("invalid apm.lock.yaml: {e}")))?;
    let version = v.get("lockfile_version").map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).unwrap_or_else(|| "1".into());
    if !matches!(version.as_str(), "1" | "2") {
        return Err(err(format!("apm.lock.yaml lockfile_version {version:?} isn't one hlock reads (1 or 2); upgrade hlock or regenerate it")));
    }
    let mut out = Vec::new();
    for d in v.get("dependencies").and_then(Value::as_array).into_iter().flatten() {
        let mut d = d.clone();
        // numbers APM may write unquoted
        if let Some(m) = d.as_object_mut() {
            for k in ["resolved_ref", "resolved_tag", "constraint"] {
                if let Some(n) = m.get(k).filter(|x| x.is_number()).cloned() {
                    m.insert(k.into(), json!(n.to_string()));
                }
            }
        }
        out.push(serde_json::from_value(d).map_err(|e| err(format!("apm.lock.yaml: {e}")))?);
    }
    Ok(out)
}

/// The project's `apm.lock.yaml` entries by identity; empty without one.
pub fn read_project_lock(project: &Path) -> Result<BTreeMap<String, ApmLocked>> {
    match std::fs::read_to_string(project.join(APM_LOCK)) {
        Ok(t) => Ok(parse_lock(&t)?.into_iter().map(|e| (e.identity(), e)).collect()),
        Err(_) => Ok(BTreeMap::new()),
    }
}

/// A git source's identity, matching [`ApmLocked::identity`].
pub fn source_identity(g: &GitSource) -> String {
    // a GitHub stand-in (`HLOCK_GITHUB_URL`) is still github.com
    let github = crate::pull::base_url("HLOCK_GITHUB_URL", "https://github.com");
    let url = match g.url.strip_prefix(&format!("{github}/")) {
        Some(rest) => format!("https://github.com/{rest}"),
        None => g.url.clone(),
    };
    let u = url.trim_end_matches('/').trim_end_matches(".git");
    let rest = u.split_once("://").map(|(_, r)| r).unwrap_or(u);
    let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest).replacen(':', "/", if u.contains("://") { 0 } else { 1 });
    let base = rest.trim_start_matches('/').to_lowercase();
    match g.subpath.as_deref() {
        Some(s) => format!("{base}/{s}"),
        None => base,
    }
}

// --------------------------------------------------------------------------- //
// APM's hashes
// --------------------------------------------------------------------------- //

/// APM's `content_hash` of a package tree: SHA-256 over each file's relative POSIX path
/// then its raw bytes, in path order, skipping symlinks, `.git` and `__pycache__`
/// anywhere and `.apm-pin` at the root.
pub fn content_hash(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink() || name == ".git" || name == "__pycache__" {
                continue;
            }
            let p = e.path();
            if t.is_dir() {
                walk(base, &p, out);
            } else if t.is_file() {
                let rel = p.strip_prefix(base).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                if rel != ".apm-pin" {
                    out.push(rel);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    let mut h = Sha256::new();
    for rel in &files {
        h.update(rel.as_bytes());
        h.update(std::fs::read(dir.join(rel)).unwrap_or_default());
    }
    format!("sha256:{}", h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// APM's hash of one deployed file: CRLF read as LF for UTF-8 text without NUL bytes,
/// raw bytes otherwise; the path isn't part of it.
pub fn file_hash(bytes: &[u8]) -> String {
    if !bytes.contains(&0) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return crate::hash::sha256_bytes(text.replace("\r\n", "\n").as_bytes());
        }
    }
    crate::hash::sha256_bytes(bytes)
}

// --------------------------------------------------------------------------- //
// Packages: what's in one
// --------------------------------------------------------------------------- //

/// A primitive found in a package: a path relative to it (`""` for the package itself).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Primitive {
    /// `skill`, `instructions`, `prompt`, `agent`, `hook`.
    pub kind: &'static str,
    pub name: String,
    pub rel: String,
}

fn files_in(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn dirs_in(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// What a package holds, where APM looks: a root `SKILL.md`; `skills/<n>/` and
/// `.apm/skills/<n>/`; `.apm/instructions/*.instructions.md`; prompts, agents and hooks
/// (which hlock doesn't install). A single-file package is classified by extension.
pub fn primitives(content: &Path, name: &str) -> Vec<Primitive> {
    let mut out = Vec::new();
    if content.is_file() {
        let f = content.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for (ext, kind) in [(".instructions.md", "instructions"), (".prompt.md", "prompt"), (".agent.md", "agent")] {
            if let Some(stem) = f.strip_suffix(ext) {
                out.push(Primitive { kind, name: stem.to_string(), rel: String::new() });
            }
        }
        return out;
    }
    if content.join("SKILL.md").is_file() {
        out.push(Primitive { kind: "skill", name: name.to_string(), rel: String::new() });
    }
    for base in ["skills", ".apm/skills"] {
        for d in dirs_in(&content.join(base)) {
            if content.join(base).join(&d).join("SKILL.md").is_file() && crate::fsutil::plain_name(&d) {
                out.push(Primitive { kind: "skill", name: d.clone(), rel: format!("{base}/{d}") });
            }
        }
    }
    for f in files_in(&content.join(".apm/instructions")) {
        if let Some(stem) = f.strip_suffix(".instructions.md").filter(|s| crate::fsutil::plain_name(s)) {
            out.push(Primitive { kind: "instructions", name: stem.to_string(), rel: format!(".apm/instructions/{f}") });
        }
    }
    for (dir, ext, kind) in [("", ".prompt.md", "prompt"), (".apm/prompts", ".prompt.md", "prompt"), ("", ".agent.md", "agent"), (".apm/agents", ".md", "agent")] {
        for f in files_in(&content.join(dir)) {
            if let Some(stem) = f.strip_suffix(ext) {
                out.push(Primitive { kind, name: stem.to_string(), rel: if dir.is_empty() { f.clone() } else { format!("{dir}/{f}") } });
            }
        }
    }
    for dir in ["hooks", ".apm/hooks"] {
        if content.join(dir).is_dir() {
            out.push(Primitive { kind: "hook", name: dir.to_string(), rel: dir.to_string() });
        }
    }
    out
}

// --------------------------------------------------------------------------- //
// Resolution: breadth-first, intersection-pick
// --------------------------------------------------------------------------- //

/// One requirement on a package: what a declaration asked for, and the chain that led there.
#[derive(Debug, Clone)]
struct Requirement {
    source: GitSource,
    chain: Vec<String>,
}

impl Requirement {
    fn label(&self) -> String {
        self.chain.join(" -> ")
    }
}

/// A package in the resolved graph.
#[derive(Debug, Clone)]
struct Node {
    key: String,
    name: String,
    spec: String,
    source: GitSource,
    depth: u32,
    /// The packages above it, root first.
    chain: Vec<String>,
}

/// The commit for a package from everything that constrains it: literal refs must agree
/// on one commit; ranges pick the highest tag they all allow (and a literal tag must be
/// in every range); with neither, the default branch.
fn pick_for(key: &str, reqs: &[Requirement], env: &Env) -> Result<Pick> {
    let first = &reqs[0].source;
    // `apm.lock.yaml`'s commit counts when every requirement allows it
    if let Some(l) = env.apm_locked.get(key) {
        let name = first.package_name();
        let tag = l.resolved_tag.as_deref().or(l.resolved_ref.as_deref());
        let version = tag.and_then(|t| deps::tag_version(t, &name).or_else(|| deps::lenient_version(t)));
        let agrees = reqs.iter().all(|r| match (&r.source.constraint, &r.source.reference) {
            (Some(c), _) => l.constraint.as_deref() == Some(c.as_str()) || VersionRange::parse(c).ok().zip(version.as_ref()).is_some_and(|(range, v)| range.allows(v, r.source.prerelease)),
            (None, Some(x)) => l.resolved_ref.as_deref() == Some(x.as_str()) || l.resolved_commit.as_deref() == Some(x.as_str()),
            (None, None) => true,
        });
        if let (true, Some(c)) = (agrees, l.resolved_commit.clone().filter(|c| crate::fsutil::plain_token(c))) {
            return Ok(Pick { commit: c, resolved_ref: l.resolved_ref.clone(), resolved_tag: l.resolved_tag.clone() });
        }
    }
    let literals: Vec<&Requirement> = reqs.iter().filter(|r| r.source.reference.is_some() && r.source.constraint.is_none()).collect();
    let ranges: Vec<&Requirement> = reqs.iter().filter(|r| r.source.constraint.is_some()).collect();
    let mut literal: Option<(Pick, &Requirement)> = None;
    for r in &literals {
        let p = env.pick(&r.source)?;
        match &literal {
            Some((q, other)) if q.commit != p.commit => {
                return Err(err(format!(
                    "{key} is required at two different commits:\n  {}#{} ({})\n  {}#{} ({})",
                    other.label(),
                    other.source.reference.as_deref().unwrap_or(""),
                    crate::fsutil::prefix_chars(&q.commit, 10),
                    r.label(),
                    r.source.reference.as_deref().unwrap_or(""),
                    crate::fsutil::prefix_chars(&p.commit, 10)
                )))
            }
            Some(_) => {}
            None => literal = Some((p, r)),
        }
    }
    if ranges.is_empty() {
        return match literal {
            Some((p, _)) => Ok(p),
            None => env.pick(first),
        };
    }
    let refs = env.refs(first)?;
    let name = first.package_name();
    let versions = deps::tag_versions(refs.tags.keys(), &name);
    let parsed: Vec<(VersionRange, &Requirement)> = ranges.iter().map(|r| VersionRange::parse(r.source.constraint.as_deref().unwrap_or("*")).map(|v| (v, *r))).collect::<Result<_>>()?;
    let prerelease = ranges.iter().any(|r| r.source.prerelease);
    if let Some((p, lr)) = &literal {
        // a literal tag must be in every range
        let tag = p.resolved_tag.as_deref().or(lr.source.reference.as_deref()).unwrap_or("");
        let v = deps::tag_version(tag, &name).or_else(|| deps::lenient_version(tag));
        for (range, rr) in &parsed {
            if !v.as_ref().is_some_and(|v| range.allows(v, prerelease)) {
                return Err(err(format!("{key}: no version satisfies both\n  {}#{tag}\n  {}#{}", lr.label(), rr.label(), rr.source.constraint.as_deref().unwrap_or(""))));
            }
        }
        return Ok(p.clone());
    }
    let best = versions
        .iter()
        .filter(|(v, _)| parsed.iter().all(|(r, _)| r.allows(v, prerelease)))
        .max_by(|a, b| {
            let k = |v: &semver::Version| (v.major, v.minor, v.patch, v.pre.clone());
            k(&a.0).cmp(&k(&b.0)).then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
        });
    match best {
        Some((_, tag)) => Ok(Pick { commit: refs.tags[*tag].clone(), resolved_ref: Some((*tag).clone()), resolved_tag: Some((*tag).clone()) }),
        None if parsed.len() > 1 => {
            let lines: Vec<String> = parsed.iter().map(|(_, r)| format!("  {}#{}", r.label(), r.source.constraint.as_deref().unwrap_or(""))).collect();
            Err(err(format!("{key}: no version satisfies every constraint on it:\n{}", lines.join("\n"))))
        }
        None => deps::pick(first, &refs),
    }
}

/// A package's content: fetched into the store at `commit`.
fn fetch_package(g: &GitSource, p: &Pick, env: &Env) -> Result<PathBuf> {
    Ok(deps::fetch_git(&env.project, g, &p.commit, p.resolved_ref.as_deref(), None)?.1)
}

/// The packages a package's `apm.yml` declares, as sources (local and `git: parent`
/// paths resolve inside the package's own repository).
fn children(content: &Path, parent: &GitSource, parent_name: &str) -> Result<(Option<ApmManifest>, Vec<(String, GitSource, Option<String>)>)> {
    let Ok(text) = std::fs::read_to_string(content.join(APM_MANIFEST)) else { return Ok((None, Vec::new())) };
    let m = parse_manifest(&text).map_err(|e| err(format!("{parent_name}'s apm.yml: {e}")))?;
    let mut out = Vec::new();
    for d in &m.deps {
        if let Some(id) = &d.registry {
            return Err(err(format!("{parent_name} depends on {id}, a registry package; hlock resolves git and local packages only")));
        }
        let rel = d.parent_path.clone().or_else(|| d.spec.starts_with(['.', '/', '~']).then(|| d.spec.clone()));
        let (src, alias) = match rel {
            Some(rel) => {
                // a path in the parent's repository, relative to the parent package
                let base = parent.subpath.clone().unwrap_or_default();
                let joined = crate::fsutil::normalize(&Path::new(&base).join(rel.trim_start_matches("./")));
                let joined = joined.to_string_lossy().replace('\\', "/");
                if !crate::fsutil::safe_rel(&joined) {
                    return Err(err(format!("{parent_name} depends on {rel:?}, which is outside its repository")));
                }
                let mut g = parent.clone();
                g.subpath = Some(joined);
                g.reference = None;
                g.constraint = None;
                (g, d.alias.clone())
            }
            None => match deps::parse(&d.spec, Kind::Package)? {
                Source::Git(mut g) => {
                    g.prerelease = d.prerelease;
                    (g, d.alias.clone())
                }
                _ => return Err(err(format!("{parent_name} depends on {:?}, which isn't a git package", d.raw))),
            },
        };
        let name = d.alias.clone().unwrap_or_else(|| deps::default_name(&Source::Git(src.clone())));
        out.push((name, src, alias));
    }
    Ok((Some(m), out))
}

/// Resolve one declared package and every package under it (with no others).
pub fn resolve_package(d: &Decl, env: &Env, lock: Option<&Lockfile>) -> Result<(Vec<Resolved>, Vec<Package>)> {
    resolve_packages(std::slice::from_ref(d), env, lock)
}

/// Resolve every declared package together: one graph, walked breadth-first from the
/// roots in declaration order, so constraints two packages put on a third are
/// intersected. A package reached from several roots belongs to the first.
pub fn resolve_packages(roots: &[Decl], env: &Env, lock: Option<&Lockfile>) -> Result<(Vec<Resolved>, Vec<Package>)> {
    let mut git_roots = Vec::new();
    let mut out: (Vec<Resolved>, Vec<Package>) = (Vec::new(), Vec::new());
    let mut taken = own_names(lock);
    for d in roots {
        match d.source()? {
            Source::Git(g) => git_roots.push((d, g)),
            Source::Path { path, .. } => {
                let (r, p) = resolve_local_package(d, &path, env, &mut taken)?;
                out.0.extend(r);
                out.1.extend(p);
            }
            other => return Err(err(format!("package {}: {} isn't an apm: or path source", d.name, other.label()))),
        }
    }
    if git_roots.is_empty() {
        return Ok(out);
    }
    // fixpoint: walking with one set of picks can find constraints that change them
    let mut picks: BTreeMap<String, Pick> = BTreeMap::new();
    for _ in 0..8 {
        let mut reqs: BTreeMap<String, Vec<Requirement>> = BTreeMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        let mut contents: BTreeMap<String, (PathBuf, Option<ApmManifest>)> = BTreeMap::new();
        let mut queue: VecDeque<Node> = git_roots.iter().map(|(d, g)| Node { key: source_identity(g), name: d.name.clone(), spec: d.spec.clone(), source: g.clone(), depth: 1, chain: Vec::new() }).collect();
        while let Some(n) = queue.pop_front() {
            if n.depth > MAX_DEPTH {
                return Err(err(format!("{} goes deeper than {MAX_DEPTH} packages ({})", n.chain.first().unwrap_or(&n.name), n.chain.join(" -> "))));
            }
            let mut chain = n.chain.clone();
            chain.push(n.name.clone());
            reqs.entry(n.key.clone()).or_default().push(Requirement { source: n.source.clone(), chain: chain.clone() });
            if nodes.iter().any(|x| x.key == n.key) {
                continue;
            }
            let p = match picks.get(&n.key) {
                Some(p) => p.clone(),
                None => pick_for(&n.key, &reqs[&n.key], env)?,
            };
            let content = fetch_package(&n.source, &p, env)?;
            let (manifest, kids) = children(&content, &n.source, &n.name)?;
            for (name, g, _) in kids {
                queue.push_back(Node { key: source_identity(&g), spec: spec_of(&g), name, source: g, depth: n.depth + 1, chain: chain.clone() });
            }
            contents.insert(n.key.clone(), (content, manifest));
            nodes.push(n);
        }
        let mut next = BTreeMap::new();
        for (key, rs) in &reqs {
            next.insert(key.clone(), pick_for(key, rs, env)?);
        }
        if next == picks {
            let (r, p) = assemble(env, &nodes, &picks, &contents, &reqs, &mut taken)?;
            out.0.extend(r);
            out.1.extend(p);
            return Ok(out);
        }
        picks = next;
    }
    Err(err("the package dependency graph didn't settle"))
}

/// Names the project's own skills, instruction files and MCP servers have (declared
/// directly, or pinned): a package's primitive of the same name is shadowed.
fn own_names(lock: Option<&Lockfile>) -> BTreeSet<(Kind, String)> {
    let mut taken = BTreeSet::new();
    if let Some(l) = lock {
        for s in l.skill.iter().filter(|s| s.dep.required_by.is_empty() && s.origin != "harness-builtin") {
            taken.insert((Kind::Skill, s.name.clone()));
        }
        for i in l.instructions.iter().filter(|i| i.dep.required_by.is_empty() && i.dep.declared) {
            taken.insert((Kind::Instructions, deps::decl_name_of(i)));
        }
        for t in l.tool.iter().filter(|t| t.dep.required_by.is_empty()) {
            taken.insert((Kind::Mcp, t.name.clone()));
        }
    }
    taken
}

/// The spec for a package found under another one: `apm:OWNER/REPO[/PATH][#REF]` on
/// GitHub, a `git+` URL elsewhere.
fn spec_of(g: &GitSource) -> String {
    let r = g.constraint.as_deref().or(g.reference.as_deref());
    let github = crate::pull::base_url("HLOCK_GITHUB_URL", "https://github.com");
    if let Some(rest) = g.url.strip_prefix(&format!("{github}/")) {
        let sub = g.subpath.as_deref().map(|s| format!("/{s}")).unwrap_or_default();
        return format!("apm:{}{sub}{}", rest.trim_end_matches(".git"), r.map(|r| format!("#{r}")).unwrap_or_default());
    }
    let mut frag = Vec::new();
    if let Some(s) = &g.subpath {
        frag.push(format!("path={s}"));
    }
    match (&g.constraint, &g.reference) {
        (Some(c), _) => frag.push(format!("version={c}")),
        (None, Some(r)) => frag.push(format!("ref={r}")),
        _ => {}
    }
    format!("git+{}{}", g.url, if frag.is_empty() { String::new() } else { format!("#{}", frag.join("&")) })
}

/// A package from a path in the project: its primitives, no commit.
fn resolve_local_package(d: &Decl, path: &str, env: &Env, taken: &mut BTreeSet<(Kind, String)>) -> Result<(Vec<Resolved>, Vec<Package>)> {
    let abs = env.ctx.expand(path, None);
    if !abs.exists() {
        return Err(err(format!("package {}: {path} doesn't exist", d.name)));
    }
    let manifest = std::fs::read_to_string(abs.join(APM_MANIFEST)).ok().map(|t| parse_manifest(&t)).transpose()?;
    if manifest.as_ref().is_some_and(|m| !m.deps.is_empty()) {
        return Err(err(format!("package {}: a local package's own dependencies aren't resolved yet; declare them in the project", d.name)));
    }
    let node = Node { key: format!("local/{path}"), name: d.name.clone(), spec: d.spec.clone(), source: GitSource { url: String::new(), fetch_url: String::new(), subpath: Some(path.trim_end_matches('/').to_string()), reference: None, constraint: None, apm: true, prerelease: false }, depth: 1, chain: Vec::new() };
    let mut contents = BTreeMap::new();
    contents.insert(node.key.clone(), (abs, manifest));
    assemble(env, &[node], &BTreeMap::new(), &contents, &BTreeMap::new(), taken)
}

/// Lock entries for a resolved package graph: one `[[package]]` per package, and an entry
/// per skill, instruction file and MCP server, first-declared winning a name.
#[allow(clippy::too_many_arguments)]
fn assemble(
    env: &Env,
    nodes: &[Node],
    picks: &BTreeMap<String, Pick>,
    contents: &BTreeMap<String, (PathBuf, Option<ApmManifest>)>,
    reqs: &BTreeMap<String, Vec<Requirement>>,
    taken: &mut BTreeSet<(Kind, String)>,
) -> Result<(Vec<Resolved>, Vec<Package>)> {
    let mut resolved: Vec<Resolved> = Vec::new();
    let mut packages = Vec::new();
    let mut warnings = Vec::new();
    for n in nodes {
        let (content, manifest) = &contents[&n.key];
        let pick = picks.get(&n.key).cloned().unwrap_or_default();
        let mut chain = n.chain.clone();
        chain.push(n.name.clone());
        let local = n.source.url.is_empty();
        let dep = |required_by: Vec<String>| Declared {
            declared: true,
            commit: (!local).then(|| pick.commit.clone()),
            resolved_ref: pick.resolved_ref.clone(),
            resolved_tag: pick.resolved_tag.clone(),
            required_by,
            ..Default::default()
        };
        let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
        for prim in primitives(content, &n.name) {
            let kind = match prim.kind {
                "skill" => Kind::Skill,
                "instructions" => Kind::Instructions,
                other => {
                    *skipped.entry(format!("{other}s")).or_default() += 1;
                    continue;
                }
            };
            if !taken.insert((kind, prim.name.clone())) {
                warnings.push(format!("{kind} {} from {} is shadowed by one declared first", prim.name, n.name));
                continue;
            }
            let path = if prim.rel.is_empty() { content.clone() } else { content.join(&prim.rel) };
            let (digest, stored) = deps::store(&env.project, &path)?;
            let sub = [if local { String::new() } else { n.source.subpath.clone().unwrap_or_default() }, prim.rel.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/");
            let source = if local {
                format!("{}/{}", n.source.subpath.as_deref().unwrap_or("."), prim.rel).trim_end_matches('/').to_string()
            } else if sub.is_empty() {
                format!("git+{}", n.source.url)
            } else {
                format!("git+{}#path={sub}", n.source.url)
            };
            let entry = match kind {
                Kind::Skill => Entry::Skill(Skill { name: prim.name.clone(), origin: "apm".into(), source: Some(source), digest: Some(digest), scripts: Some(crate::capture::skill_scripts(&path)), enabled: true, scope: Some("project".into()), dep: dep(chain.clone()), ..Default::default() }),
                _ => Entry::Instructions(Instructions { name: prim.name.clone(), digest: Some(digest), scope: "project".into(), source: Some(source), dep: Declared { declared_as: Some(prim.name.clone()), ..dep(chain.clone()) }, ..Default::default() }),
            };
            resolved.push(Resolved { kind, name: prim.name.clone(), entry, content: Some(stored), locked: false });
        }
        for m in manifest.iter().flat_map(|m| m.mcp.iter()) {
            let short = m.name.rsplit('/').next().unwrap_or(&m.name).to_string();
            let short = crate::pull::sanitize(&short);
            if !taken.insert((Kind::Mcp, short.clone())) {
                warnings.push(format!("MCP server {short} from {} is shadowed by one declared first", n.name));
                continue;
            }
            let mut t = if m.registry {
                Tool { name: short.clone(), kind: "mcp".into(), source: Some(format!("mcp-registry:{}", m.name)), scope: Some("project".into()), ..Default::default() }
            } else {
                let mut t = crate::capture::mcp_tool(&short, &m.entry, "project").ok_or_else(|| err(format!("{}: MCP server {} isn't one hlock can configure", n.name, m.name)))?;
                t.source = Some(format!("apm-mcp:{}", m.name));
                t
            };
            t.dep = Declared { declared: true, required_by: chain.clone(), ..Default::default() };
            resolved.push(Resolved { kind: Kind::Mcp, name: short, entry: Entry::Tool(t), content: None, locked: false });
        }
        let constraint = reqs.get(&n.key).and_then(|rs| rs.iter().find_map(|r| r.source.constraint.clone()));
        packages.push(Package {
            name: n.name.clone(),
            source: n.spec.clone(),
            digest: Some(if content.is_dir() { content_hash(content) } else { file_hash(&std::fs::read(content).unwrap_or_default()) }),
            depth: n.depth,
            version: manifest.as_ref().map(|m| m.version.clone()),
            license: manifest.as_ref().and_then(|m| m.license.clone()),
            skipped,
            dep: Declared { declared: n.depth == 1, constraint, ..dep(n.chain.clone()) },
        });
    }
    for w in warnings {
        eprintln!("warn: {w}");
    }
    Ok((resolved, packages))
}

/// A declared package taken from the lock as it is, when its source is unchanged.
pub fn locked_package(lock: &Lockfile, d: &Decl, env: &Env) -> Option<(Vec<Resolved>, Option<Vec<Package>>)> {
    let root = lock.package.iter().find(|p| p.name == d.name && p.depth == 1 && p.dep.declared && p.source == d.spec)?;
    let mine = |rb: &[String]| rb.first() == Some(&d.name);
    let packages: Vec<Package> = std::iter::once(root.clone()).chain(lock.package.iter().filter(|p| p.depth > 1 && mine(&p.dep.required_by)).cloned()).collect();
    let mut out = Vec::new();
    let content = |dg: Option<&String>| dg.and_then(|dg| deps::stored(&env.project, dg));
    for s in lock.skill.iter().filter(|s| mine(&s.dep.required_by)) {
        out.push(Resolved { kind: Kind::Skill, name: s.name.clone(), content: content(s.digest.as_ref()), entry: Entry::Skill(s.clone()), locked: true });
    }
    for i in lock.instructions.iter().filter(|i| mine(&i.dep.required_by)) {
        out.push(Resolved { kind: Kind::Instructions, name: deps::decl_name_of(i), content: content(i.digest.as_ref()), entry: Entry::Instructions(i.clone()), locked: true });
    }
    for t in lock.tool.iter().filter(|t| mine(&t.dep.required_by)) {
        out.push(Resolved { kind: Kind::Mcp, name: t.name.clone(), content: None, entry: Entry::Tool(t.clone()), locked: true });
    }
    Some((out, Some(packages)))
}

// --------------------------------------------------------------------------- //
// hlock export --format apm-lock
// --------------------------------------------------------------------------- //

/// The APM target a harness corresponds to.
pub fn apm_target(harness: &str) -> &'static str {
    match harness {
        "claude-code" | "claude-agent-sdk" => "claude",
        "codex" => "codex",
        "cursor-agent" => "cursor",
        "copilot-cli" => "copilot",
        "gemini-cli" => "gemini",
        "opencode" => "opencode",
        "hermes" => "hermes",
        _ => "agent-skills",
    }
}

/// Where APM puts a skill or instruction file for a target (its `KNOWN_TARGETS`);
/// `None` when the target has nowhere for it (instructions compiled into `AGENTS.md`).
pub fn apm_path(target: &str, kind: Kind, name: &str) -> Option<String> {
    match kind {
        Kind::Skill => Some(match target {
            "claude" => format!(".claude/skills/{name}"),
            "kiro" => format!(".kiro/skills/{name}"),
            "grok-build" => format!(".grok/skills/{name}"),
            _ => format!(".agents/skills/{name}"),
        }),
        Kind::Instructions => match target {
            "claude" => Some(format!(".claude/rules/{name}.md")),
            "cursor" => Some(format!(".cursor/rules/{name}.mdc")),
            "copilot" => Some(format!(".github/instructions/{name}.instructions.md")),
            "windsurf" => Some(format!(".windsurf/rules/{name}.md")),
            "kiro" => Some(format!(".kiro/steering/{name}.md")),
            "antigravity" => Some(format!(".agents/rules/{name}.md")),
            "grok-build" => Some(format!(".grok/rules/{name}.md")),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct OutDep {
    repo_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    materialization_repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    virtual_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_virtual: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    package_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deployed_files: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    deployed_file_hashes: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    constraint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_license: Option<String>,
    #[serde(skip)]
    depth_sort: u32,
}

#[derive(Debug, Clone, Serialize)]
struct OutLock {
    lockfile_version: String,
    dependencies: Vec<OutDep>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcp_servers: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    mcp_configs: BTreeMap<String, Value>,
}

/// What [`export`] wrote, or would.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportOutcome {
    pub path: Option<PathBuf>,
    pub text: String,
    pub packages: usize,
    pub files: usize,
    pub lockfile_version: String,
    /// What the lock has that APM has no place for.
    pub left_out: Vec<String>,
    /// Where hlock placed a file somewhere other than APM would.
    pub notes: Vec<String>,
    pub targets: Vec<String>,
}

/// `owner/repo` (lowercased, as APM writes it), the host, and the original spelling when
/// that differs.
fn apm_identity(g: &GitSource) -> (String, String, Option<String>) {
    let id = source_identity(g);
    let base = match &g.subpath {
        Some(s) => id.strip_suffix(&format!("/{s}")).unwrap_or(&id).to_string(),
        None => id,
    };
    let (host, repo) = base.split_once('/').map(|(h, r)| (h.to_string(), r.to_string())).unwrap_or(("github.com".into(), base.clone()));
    let github = crate::pull::base_url("HLOCK_GITHUB_URL", "https://github.com");
    let original = g.url.strip_prefix(&format!("{github}/")).or_else(|| g.url.split_once("://").and_then(|(_, r)| r.split_once('/')).map(|(_, r)| r)).map(|r| r.trim_end_matches(".git").to_string());
    let materialized = original.filter(|o| o != &repo && o.to_lowercase() == repo);
    (repo, host, materialized)
}

/// The deployed files of a skill directory or an instruction file at APM's path, with
/// APM's per-file hashes. The top directory is listed too, unhashed, as APM lists it.
fn deployed(content: &Path, at: &str, out: &mut Vec<String>, hashes: &mut BTreeMap<String, String>) {
    if content.is_file() {
        out.push(at.to_string());
        hashes.insert(at.to_string(), file_hash(&std::fs::read(content).unwrap_or_default()));
        return;
    }
    out.push(at.to_string());
    fn walk(base: &Path, d: &Path, at: &str, out: &mut Vec<String>, hashes: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = std::fs::read_dir(d).into_iter().flatten().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(t) = e.file_type() else { continue };
            let p = e.path();
            let rel = p.strip_prefix(base).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            let path = format!("{at}/{rel}");
            // APM lists the skill's own directory and its files, not the directories in it
            if t.is_dir() {
                walk(base, &p, at, out, hashes);
            } else if t.is_file() {
                hashes.insert(path.clone(), file_hash(&std::fs::read(&p).unwrap_or_default()));
                out.push(path);
            }
        }
    }
    walk(content, content, at, out, hashes);
}

/// `resolved_ref` as APM writes it: the tag a range resolved to, or the ref the
/// declaration names; nothing for a declaration that names none (the default branch).
fn asked_ref(g: &GitSource, dep: &Declared) -> Option<String> {
    if g.constraint.is_some() || dep.constraint.is_some() {
        return dep.resolved_tag.clone().or_else(|| dep.resolved_ref.clone());
    }
    g.reference.clone()
}

/// The version a tag names (`v1.3.0`, `sql--v1.3.0` → `1.3.0`).
fn tag_to_version(tag: &str, name: &str) -> Option<String> {
    deps::tag_version(tag, name).or_else(|| deps::lenient_version(tag)).map(|v| v.to_string())
}

/// Build `apm.lock.yaml` from a lock: one entry per APM package and per declared skill or
/// instruction file from git, with the files APM would deploy for the project's targets
/// (`apm.yml` `targets:`, else the lock's harness). Writes it to `out` (refusing to replace
/// one that pins a commit the lock doesn't know, unless `force`), or only returns it.
pub fn export(lock: &Lockfile, env: &Env, out: Option<&Path>, force: bool) -> Result<ExportOutcome> {
    let targets: Vec<String> = match read_project_manifest(&env.project)? {
        Some(m) if !m.targets.is_empty() && !m.targets.iter().any(|t| t == "all") => m.targets,
        _ => vec![apm_target(&lock.harness.name).to_string()],
    };
    let mut deps_out: Vec<OutDep> = Vec::new();
    let mut notes = Vec::new();
    let mut files = 0usize;
    let content_of = |e: &Entry| -> Result<PathBuf> { deps::fetch_entry(e, env) };
    let deploy = |e: &Entry, decl_name: &str, d: &mut OutDep, notes: &mut Vec<String>| -> Result<()> {
        let content = content_of(e)?;
        let kind = e.kind();
        for t in &targets {
            let Some(at) = apm_path(t, kind, decl_name) else { continue };
            deployed(&content, &at, &mut d.deployed_files, &mut d.deployed_file_hashes);
            let placed = match e {
                Entry::Skill(s) => s.path.clone(),
                Entry::Instructions(i) => Some(i.name.clone()),
                _ => None,
            };
            if let Some(p) = placed.filter(|p| p != &at && t == apm_target(&lock.harness.name)) {
                notes.push(format!("{kind} {decl_name}: hlock placed it at {p}; APM's {t} target uses {at}"));
            }
        }
        Ok(())
    };
    let pkg_by_name: BTreeMap<&str, &Package> = lock.package.iter().map(|p| (p.name.as_str(), p)).collect();
    for p in &lock.package {
        let Source::Git(g) = deps::parse(&p.source, Kind::Package)? else {
            notes.push(format!("package {}: a local package isn't exported", p.name));
            continue;
        };
        let (repo_url, host, materialized) = apm_identity(&g);
        let parent = p.dep.required_by.last().and_then(|n| pkg_by_name.get(n.as_str())).and_then(|pp| deps::parse(&pp.source, Kind::Package).ok()).and_then(|s| s.git().map(apm_identity)).map(|x| x.0);
        let mut d = OutDep {
            repo_url,
            materialization_repo_url: materialized,
            host: Some(host),
            name: Some(p.name.clone()),
            resolved_commit: p.dep.commit.clone(),
            // APM records a ref only when the declaration names one
            resolved_ref: asked_ref(&g, &p.dep),
            version: p.version.clone().or_else(|| p.dep.resolved_tag.as_deref().and_then(|t| tag_to_version(t, &g.package_name()))).or_else(|| Some("unknown".into())),
            virtual_path: g.subpath.clone(),
            is_virtual: g.subpath.as_ref().map(|_| true),
            depth: (p.depth != 1).then_some(p.depth),
            resolved_by: parent,
            package_type: p.version.as_ref().map(|_| "apm_package".to_string()),
            content_hash: p.digest.clone(),
            constraint: p.dep.constraint.clone(),
            resolved_tag: p.dep.resolved_tag.clone(),
            declared_license: p.license.clone(),
            depth_sort: p.depth,
            ..Default::default()
        };
        let mut chain = p.dep.required_by.clone();
        chain.push(p.name.clone());
        for s in lock.skill.iter().filter(|s| s.dep.required_by == chain) {
            deploy(&Entry::Skill(s.clone()), &s.name, &mut d, &mut notes)?;
        }
        for i in lock.instructions.iter().filter(|i| i.dep.required_by == chain) {
            deploy(&Entry::Instructions(i.clone()), &deps::decl_name_of(i), &mut d, &mut notes)?;
        }
        if d.package_type.is_none() && lock.skill.iter().any(|s| s.dep.required_by == chain && s.source.as_deref().is_some_and(|src| !src.contains("#path=") || g.subpath.as_deref().is_some_and(|sp| src.ends_with(sp)))) {
            d.package_type = Some("claude_skill".into());
        }
        files += d.deployed_file_hashes.len();
        deps_out.push(d);
    }
    let mut left_out = Vec::new();
    let mut outside = 0;
    // declared skills and instruction files from git are virtual packages to APM
    let direct: Vec<Entry> = lock
        .skill
        .iter()
        .filter(|s| s.dep.declared && s.dep.required_by.is_empty())
        .cloned()
        .map(Entry::Skill)
        .chain(lock.instructions.iter().filter(|i| i.dep.declared && i.dep.required_by.is_empty()).cloned().map(Entry::Instructions))
        .collect();
    for e in &direct {
        let kind = if e.kind() == Kind::Instructions { Kind::Instructions } else { Kind::Skill };
        let Some(Source::Git(g)) = e.source().and_then(|s| deps::parse(s, kind).ok()) else {
            outside += 1;
            continue;
        };
        let (repo_url, host, materialized) = apm_identity(&g);
        let content = content_of(e)?;
        let name = match e {
            Entry::Instructions(i) => deps::decl_name_of(i),
            other => other.name().to_string(),
        };
        let mut d = OutDep {
            repo_url,
            materialization_repo_url: materialized,
            host: Some(host),
            name: Some(name.clone()),
            resolved_commit: e.dep().commit.clone(),
            resolved_ref: asked_ref(&g, e.dep()),
            version: e.dep().resolved_tag.as_deref().and_then(|t| tag_to_version(t, &g.package_name())).or_else(|| Some("unknown".into())),
            virtual_path: g.subpath.clone(),
            is_virtual: g.subpath.as_ref().map(|_| true),
            package_type: (kind == Kind::Skill).then(|| "claude_skill".into()),
            content_hash: content.is_dir().then(|| content_hash(&content)),
            constraint: e.dep().constraint.clone(),
            resolved_tag: e.dep().resolved_tag.clone(),
            depth_sort: 1,
            ..Default::default()
        };
        deploy(e, &name, &mut d, &mut notes)?;
        files += d.deployed_file_hashes.len();
        deps_out.push(d);
    }
    deps_out.sort_by(|a, b| (a.depth_sort, &a.repo_url, &a.virtual_path).cmp(&(b.depth_sort, &b.repo_url, &b.virtual_path)));
    // MCP servers, in APM's shape
    let mut mcp_configs = BTreeMap::new();
    for t in lock.tool.iter().filter(|t| t.dep.declared) {
        let (name, cfg) = match t.source.as_deref().and_then(|s| s.strip_prefix("mcp-registry:")) {
            Some(reg) => (reg.to_string(), json!({"name": reg})),
            None => {
                let mut c = serde_json::Map::new();
                c.insert("name".into(), json!(t.name));
                c.insert("transport".into(), json!(t.transport.clone().unwrap_or_else(|| "stdio".into())));
                if !t.env.is_empty() {
                    c.insert("env".into(), json!(t.env));
                }
                if !t.args.is_empty() {
                    c.insert("args".into(), json!(t.args));
                }
                if let Some(v) = &t.version {
                    c.insert("version".into(), json!(v));
                }
                c.insert("registry".into(), json!(false));
                if let Some(pkg) = t.source.as_deref().and_then(|s| s.split_once(':')).map(|(k, _)| k).filter(|k| matches!(*k, "npm" | "pypi")) {
                    c.insert("package".into(), json!(pkg));
                }
                if !t.headers.is_empty() {
                    c.insert("headers".into(), json!(t.headers));
                }
                if let Some(u) = &t.url {
                    c.insert("url".into(), json!(u));
                }
                if let Some(cmd) = &t.command {
                    c.insert("command".into(), json!(cmd));
                }
                (t.name.clone(), Value::Object(c))
            }
        };
        mcp_configs.insert(name, cfg);
    }
    let version = if deps_out.iter().any(|d| d.constraint.is_some() || d.resolved_tag.is_some() && d.constraint.is_some()) { "2" } else { "1" };
    let doc = OutLock { lockfile_version: version.into(), dependencies: deps_out.clone(), mcp_servers: mcp_configs.keys().cloned().collect(), mcp_configs };
    let body = serde_norway::to_string(&doc).map_err(|e| err(format!("can't write apm.lock.yaml: {e}")))?;
    let text = format!("{EXPORT_HEADER} {} from harness.lock; edit harness.toml and re-export rather than editing this file\n{body}", env!("CARGO_PKG_VERSION"));
    if !lock.llm.is_empty() || !lock.harness.name.is_empty() {
        left_out.push("harness".to_string());
    }
    if !lock.llm.is_empty() {
        left_out.push(format!("{} model{}", lock.llm.len(), if lock.llm.len() == 1 { "" } else { "s" }));
    }
    if !lock.runtime.is_empty() {
        left_out.push(format!("{} runtime{}", lock.runtime.len(), if lock.runtime.len() == 1 { "" } else { "s" }));
    }
    if !lock.config.is_empty() {
        left_out.push("config".into());
    }
    if lock.evidence.trials > 0 || lock.skill.iter().any(|s| s.dep.evidence.is_some()) {
        left_out.push("evidence".into());
    }
    let plugins = lock.native.iter().filter(|n| n.dep.declared).count();
    if plugins > 0 {
        left_out.push(format!("{plugins} plugin{}", if plugins == 1 { "" } else { "s" }));
    }
    if outside > 0 {
        left_out.push(format!("{outside} path dependenc{}", if outside == 1 { "y" } else { "ies" }));
    }
    if let Some(path) = out {
        if path.is_file() && !force {
            let known: BTreeSet<String> = lock.package.iter().filter_map(|p| p.dep.commit.clone()).chain(direct.iter().filter_map(|e| e.dep().commit.clone())).collect();
            let theirs = parse_lock(&std::fs::read_to_string(path)?)?;
            if let Some(newer) = theirs.iter().find(|e| e.resolved_commit.as_ref().is_some_and(|c| !known.contains(c))) {
                return Err(err(format!(
                    "{} pins {} at {}, which harness.lock doesn't know (did someone run `apm install`?); declare it and run `hlock sync`, or pass --force to overwrite it",
                    path.display(),
                    newer.identity(),
                    crate::fsutil::prefix_chars(newer.resolved_commit.as_deref().unwrap_or(""), 7)
                )));
            }
        }
        crate::fsutil::write_atomic(path, &text)?;
    }
    Ok(ExportOutcome { path: out.map(Path::to_path_buf), text, packages: deps_out.len(), files, lockfile_version: version.into(), left_out, notes, targets })
}

/// `check`'s report when the project's `apm.lock.yaml` was exported by hlock and the
/// lock's dependencies have changed since.
pub fn stale_export(lock: &Lockfile, project: &Path) -> Option<deps::Issue> {
    let path = project.join(APM_LOCK);
    let text = std::fs::read_to_string(&path).ok()?;
    if !text.starts_with(EXPORT_HEADER) {
        return None;
    }
    let theirs: BTreeSet<(String, Option<String>)> = parse_lock(&text).ok()?.into_iter().map(|e| (e.identity(), e.resolved_commit)).collect();
    let ours: BTreeSet<(String, Option<String>)> = lock
        .package
        .iter()
        .filter_map(|p| match deps::parse(&p.source, Kind::Package).ok()? {
            Source::Git(g) => Some((source_identity(&g), p.dep.commit.clone())),
            _ => None,
        })
        .chain(lock.skill.iter().filter(|s| s.dep.declared && s.dep.required_by.is_empty()).filter_map(|s| match deps::parse(s.source.as_deref()?, Kind::Skill).ok()? {
            Source::Git(g) => Some((source_identity(&g), s.dep.commit.clone())),
            _ => None,
        }))
        .chain(lock.instructions.iter().filter(|i| i.dep.declared && i.dep.required_by.is_empty()).filter_map(|i| match deps::parse(i.source.as_deref()?, Kind::Instructions).ok()? {
            Source::Git(g) => Some((source_identity(&g), i.dep.commit.clone())),
            _ => None,
        }))
        .collect();
    (theirs != ours).then(|| deps::Issue {
        problem: "stale-export".into(),
        kind: Kind::Package,
        name: APM_LOCK.into(),
        detail: "apm.lock.yaml was exported from harness.lock, and the dependencies have changed since".into(),
        fix: "hlock export --format apm-lock".into(),
        fails: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_strings_become_hlock_sources() {
        let s = |x: &str| dep_spec(x).unwrap();
        assert_eq!(s("microsoft/apm-sample-package"), "apm:microsoft/apm-sample-package");
        assert_eq!(s("microsoft/apm-sample-package#v1.0.0"), "apm:microsoft/apm-sample-package#v1.0.0");
        assert_eq!(s("owner/repo/skills/foo#main"), "apm:owner/repo/skills/foo#main");
        assert_eq!(s("gitlab.com/acme/coding-standards"), "apm:gitlab.com/acme/coding-standards");
        assert_eq!(s("https://github.com/microsoft/apm-sample-package.git"), "apm:microsoft/apm-sample-package");
        assert_eq!(s("git@github.com:microsoft/apm-sample-package.git"), "apm:microsoft/apm-sample-package");
        assert_eq!(s("acme/foo#^1.2.0"), "apm:acme/foo#^1.2.0");
        assert_eq!(s("./packages/x"), "./packages/x");
        let d = parse_dep(&json!({"git": "https://gitlab.com/acme/repo.git", "path": "instructions/security", "ref": "v2.0", "alias": "acme-sec"})).unwrap();
        assert_eq!((d.spec.as_str(), d.alias.as_deref()), ("apm:gitlab.com/acme/repo/instructions/security#v2.0", Some("acme-sec")));
        assert!(parse_dep(&json!({"git": "a/b", "version": "^1"})).is_err());
        assert!(parse_dep(&json!({"version": "^1"})).is_err());
        assert!(parse_dep(&json!({"unsupported_source_kind": "x"})).is_err());
        assert_eq!(parse_dep(&json!({"git": "parent", "path": "skills/shared"})).unwrap().parent_path.as_deref(), Some("skills/shared"));
    }

    #[test]
    fn manifests_and_their_mcp_servers() {
        let m = parse_manifest(
            "name: demo\nversion: \"1.0.0\"\nlicense: MIT\ntargets: [claude, copilot]\ndependencies:\n  apm:\n    - acme/skills/skills/sql#^1.2.0\n  mcp:\n    - io.github.github/github-mcp-server\n    - name: local\n      registry: false\n      transport: stdio\n      command: ./bin/server\n      args: [\"--port\", \"3000\"]\n",
        )
        .unwrap();
        assert_eq!((m.name.as_str(), m.license.as_deref(), m.targets.len(), m.deps.len()), ("demo", Some("MIT"), 2, 1));
        assert!(m.mcp[0].registry && !m.mcp[1].registry);
        assert_eq!(m.mcp[1].entry["command"], "./bin/server");
        assert!(parse_manifest("name: x\nversion: '1'\ndependencies:\n  conflict_resolution: nest\n").unwrap_err().to_string().contains("nest"));
        assert!(parse_manifest("name: x\nversion: '1'\ndependencies:\n  mcp:\n    - name: y\n      registry: false\n").is_err());
        // a quoted ampersand isn't an anchor
        assert!(parse_manifest("name: \"a & b *c\"\nversion: '1' # &no\n").is_ok());
    }

    #[test]
    fn hashes_match_apm() {
        // the vector the research ran through APM's own compute_package_hash
        let d = std::env::temp_dir().join(format!("hlock-apm-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("skills/a")).unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join("__pycache__")).unwrap();
        std::fs::write(d.join("apm.yml"), b"name: x\nversion: 1.0.0\n").unwrap();
        std::fs::write(d.join("skills/a/SKILL.md"), b"hello\r\nworld\n").unwrap();
        std::fs::write(d.join(".git/HEAD"), b"ref").unwrap();
        std::fs::write(d.join("__pycache__/m.pyc"), b"x").unwrap();
        std::fs::write(d.join(".apm-pin"), b"pin").unwrap();
        assert_eq!(content_hash(&d), "sha256:535a530f84dfe7575b1199ae78efcfce1144fb0dfe7546fe6e56bef6bb3e88f6");
        assert_eq!(file_hash(b"hello\r\nworld\n"), "sha256:4a1e67f2fe1d1cc7b31d0ca2ec441da4778203a036a77da10344c85e24ff0f92");
        assert_eq!(file_hash(b"\x00\r\n"), crate::hash::sha256_bytes(b"\x00\r\n"), "binary is hashed raw");
        let empty = d.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(content_hash(&empty), crate::hash::sha256_bytes(b""));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn identities_match_between_sources_and_apm_locks() {
        let g = match deps::parse("apm:Acme/Skills/skills/sql", Kind::Package).unwrap() {
            Source::Git(g) => g,
            _ => unreachable!(),
        };
        let a = ApmLocked { repo_url: "acme/skills".into(), host: Some("github.com".into()), virtual_path: Some("skills/sql".into()), ..Default::default() };
        let b = ApmLocked { repo_url: "github.com/acme/skills".into(), virtual_path: Some("skills/sql".into()), ..Default::default() };
        assert_eq!(a.identity(), b.identity());
        assert_eq!(source_identity(&g).to_lowercase(), a.identity().replace("skills/sql", "skills/sql"));
    }
}
