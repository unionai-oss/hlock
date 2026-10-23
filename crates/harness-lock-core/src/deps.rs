//! Declared dependencies (`specs/dependencies.md`): the sources `hlock add` takes, the
//! dependency resolver that turns them into exact commits, versions and digests, the shared
//! git cache, and the placement of each dependency into a harness.
//!
//! This is a different resolver from `hlock resolve` (which searches stacks and measures
//! them): it only turns declared sources into a fixed, flat set. User-facing text calls it
//! "dependencies", never "resolve".
//!
//! - **The lock is authoritative.** A declared dependency whose lock entry has the same
//!   source is taken from the lock, with no network; [`outdated`] is what looks upstream.
//! - **Refs resolve with `git ls-remote`**, one round trip per repository, in parallel.
//! - **One git cache** (`~/.hlock/cache/git/`, bare repositories fetched shallowly and
//!   without blobs until they're read) serves every project on the machine.
//! - **Trees are content-addressed** in the project's store (`.hlock/store/trees/<digest>`),
//!   so placing one is a copy, and an unchanged digest is skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::fsutil::{plain_token, safe_rel};
use crate::lockfile::{Declared, Instructions, Lockfile, Native, Package, Skill, Tool};
use crate::profiles::{Ctx, HarnessProfile};

fn err(msg: impl Into<String>) -> Error {
    Error::Dependency(msg.into())
}

// --------------------------------------------------------------------------- //
// Kinds
// --------------------------------------------------------------------------- //

/// What a dependency is, which says where it's declared, locked and installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Skill,
    Mcp,
    Instructions,
    Plugin,
    /// An APM package (`apm:`): its skills, instructions and MCP servers, transitively.
    Package,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Skill, Kind::Mcp, Kind::Instructions, Kind::Plugin, Kind::Package];

    pub fn parse(s: &str) -> Result<Kind> {
        Ok(match s {
            "skill" | "skills" => Kind::Skill,
            "mcp" | "tool" => Kind::Mcp,
            "instructions" | "instruction" | "rules" => Kind::Instructions,
            "plugin" | "plugins" => Kind::Plugin,
            "package" | "packages" | "apm" => Kind::Package,
            _ => return Err(err(format!("{s:?} isn't a dependency kind: use skill, mcp, instructions, plugin, or an apm: source"))),
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Skill => "skill",
            Kind::Mcp => "mcp",
            Kind::Instructions => "instructions",
            Kind::Plugin => "plugin",
            Kind::Package => "package",
        }
    }

    /// The `harness.toml` table it's declared in.
    pub fn section(&self) -> &'static str {
        match self {
            Kind::Skill => "skills",
            Kind::Mcp => "mcp",
            Kind::Instructions => "instructions",
            Kind::Plugin => "plugins",
            Kind::Package => "packages",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// --------------------------------------------------------------------------- //
// Sources
// --------------------------------------------------------------------------- //

/// A git repository, and what in it a dependency is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GitSource {
    /// The remote, without credentials.
    pub url: String,
    /// The remote as given, credentials included: only for fetching, never stored or shown.
    #[serde(skip)]
    pub fetch_url: String,
    /// A directory or a single file in the repository (no leading or trailing `/`).
    pub subpath: Option<String>,
    /// A branch, tag or commit; `None` (with no `constraint`) for the default branch.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// A version range over the repository's tags (`^1.2`).
    pub constraint: Option<String>,
    /// An APM package: its `apm.yml` dependencies are resolved too.
    pub apm: bool,
    /// Let a version range match pre-release tags (`prerelease: true` in `apm.yml`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
}

impl GitSource {
    /// The repository's name (the last segment of its URL).
    pub fn repo_name(&self) -> String {
        self.url.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', ':']).next().unwrap_or("").to_string()
    }

    /// The package name for `{name}--v{version}` tags: the subpath's last segment, else the repo.
    pub fn package_name(&self) -> String {
        match &self.subpath {
            Some(p) => p.rsplit('/').next().unwrap_or(p).to_string(),
            None => self.repo_name(),
        }
    }

    /// Whether the subpath names a single file (by extension), not a directory.
    pub fn is_file(&self) -> bool {
        self.subpath.as_deref().is_some_and(is_file_path)
    }
}

/// A file path by its last segment: `style.instructions.md`, `rules.mdc`, `x.yaml`.
fn is_file_path(p: &str) -> bool {
    let last = p.rsplit('/').next().unwrap_or(p);
    let Some((stem, ext)) = last.rsplit_once('.') else { return false };
    !stem.is_empty() && matches!(ext.to_ascii_lowercase().as_str(), "md" | "mdc" | "markdown" | "txt" | "json" | "yaml" | "yml" | "toml")
}

/// Where a dependency comes from (`specs/dependencies.md` §4).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Source {
    Git(GitSource),
    /// A local directory or file, relative to the project (or absolute, or `~/…`).
    Path { path: String, editable: bool },
    /// An MCP server package on npm, run with `npx`.
    Npm { package: String, constraint: Option<String> },
    /// An MCP server package on PyPI, run with `uvx`.
    Pypi { package: String, constraint: Option<String> },
    /// A remote MCP server, by URL.
    Remote { url: String },
    /// A harness plugin from a marketplace the harness knows (`plugin:MARKETPLACE/NAME`).
    Plugin { marketplace: String, name: String },
    /// An MCP server an `apm.yml` defines itself: its config entry, or (`None`) a reference
    /// to the MCP registry.
    Defined { name: String, entry: Option<Value> },
}

impl Source {
    /// A short label for messages: `acme/skills@^1.2 skills/sql`, `npm:x@1.2.0`.
    pub fn label(&self) -> String {
        match self {
            Source::Git(g) => {
                let repo = g.url.trim_end_matches(".git").rsplitn(3, ['/', ':']).take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
                let at = g.constraint.as_deref().or(g.reference.as_deref()).map(|r| format!("@{r}")).unwrap_or_default();
                let sub = g.subpath.as_deref().map(|s| format!(" {s}")).unwrap_or_default();
                format!("{repo}{at}{sub}")
            }
            Source::Path { path, .. } => path.clone(),
            Source::Npm { package, constraint } => format!("npm:{package}{}", constraint.as_deref().map(|c| format!("@{c}")).unwrap_or_default()),
            Source::Pypi { package, constraint } => format!("pypi:{package}{}", constraint.as_deref().map(|c| format!("@{c}")).unwrap_or_default()),
            Source::Remote { url } => url.clone(),
            Source::Plugin { marketplace, name } => format!("plugin:{marketplace}/{name}"),
            Source::Defined { name, .. } => format!("apm.yml mcp {name}"),
        }
    }

    pub fn git(&self) -> Option<&GitSource> {
        match self {
            Source::Git(g) => Some(g),
            _ => None,
        }
    }

    /// The version range asked for, if any.
    pub fn constraint(&self) -> Option<&str> {
        match self {
            Source::Git(g) => g.constraint.as_deref(),
            Source::Npm { constraint, .. } | Source::Pypi { constraint, .. } => constraint.as_deref(),
            _ => None,
        }
    }
}

/// Whether what follows `@` or `#` is a version range rather than a branch, tag or commit:
/// it starts with an operator (`^1.2`, `~1`, `>=1 <2`, `*`), or it's a bare version (`1.2.0`).
pub fn is_constraint(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    if s.starts_with(['^', '~', '<', '>', '=', '*']) || s.contains("||") || s.contains(' ') {
        return true;
    }
    // a bare version, maybe with x wildcards: 1, 1.2, 1.2.3, 1.x, 1.2.3-rc.1
    let core = s.split(['-', '+']).next().unwrap_or(s);
    !core.is_empty() && core.split('.').count() <= 3 && core.split('.').all(|p| !p.is_empty() && (p.chars().all(|c| c.is_ascii_digit()) || matches!(p, "x" | "X" | "*")))
        && core.starts_with(|c: char| c.is_ascii_digit())
}

/// Split `BODY@REF` or `BODY#REF` at the last `@`/`#` after every `/` (so `@scope/pkg`
/// and `git@host:path` aren't split). Returns the body and the ref or range.
fn split_at_ref(s: &str) -> (&str, Option<&str>) {
    let last_slash = s.rfind('/').map_or(0, |i| i + 1);
    match s[last_slash..].rfind(['@', '#']) {
        Some(i) if i > 0 && last_slash + i + 1 < s.len() => (&s[..last_slash + i], Some(&s[last_slash + i + 1..])),
        Some(i) if i > 0 => (&s[..last_slash + i], None),
        _ => (s, None),
    }
}

/// Put a ref or range on a git source, whichever it is.
fn with_ref(mut g: GitSource, r: Option<&str>) -> GitSource {
    match r.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) if is_constraint(r) => g.constraint = Some(r.to_string()),
        Some(r) => g.reference = Some(r.to_string()),
        None => {}
    }
    g
}

fn clean(p: &str) -> Option<String> {
    let p = p.trim_matches('/');
    (!p.is_empty()).then(|| p.to_string())
}

/// `github:OWNER/REPO[/SUBPATH][@REF]` (also `//SUBPATH`), on `HLOCK_GITHUB_URL` if set.
fn parse_github(body: &str, apm: bool) -> Result<GitSource> {
    let github = crate::pull::base_url("HLOCK_GITHUB_URL", "https://github.com");
    let (body, r) = split_at_ref(body);
    let (repo_part, sub) = match body.split_once("//") {
        Some((a, b)) => (a, clean(b)),
        None => (body, None),
    };
    let parts: Vec<&str> = repo_part.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return Err(err(format!("expected OWNER/REPO, got {repo_part:?}")));
    }
    let subpath = sub.or_else(|| clean(&parts[2..].join("/")));
    let repo = parts[1].trim_end_matches(".git");
    let url = format!("{github}/{}/{repo}.git", parts[0]);
    Ok(with_ref(GitSource { fetch_url: url.clone(), url, subpath, reference: None, constraint: None, apm, prerelease: false }, r))
}

/// `apm:[HOST/]OWNER/REPO[/SUBPATH][#REF]`: GitHub unless the first segment is a host.
fn parse_apm(body: &str) -> Result<GitSource> {
    let first = body.split('/').next().unwrap_or("");
    if first.contains('.') && !first.contains('@') {
        // a host: `gitlab.com/group/repo/sub` (a repo is the first two segments after it)
        let (b, r) = split_at_ref(body);
        let segs: Vec<&str> = b.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() < 3 {
            return Err(err(format!("expected apm:HOST/OWNER/REPO, got {body:?}")));
        }
        let url = format!("https://{}/{}/{}.git", segs[0], segs[1], segs[2].trim_end_matches(".git"));
        let subpath = clean(&segs[3..].join("/"));
        return Ok(with_ref(GitSource { fetch_url: url.clone(), url, subpath, reference: None, constraint: None, apm: true, prerelease: false }, r));
    }
    parse_github(body, true)
}

/// `git+URL[#path=SUB&tag=T&ref=R&commit=C&version=RANGE]`, and the lock's own
/// `git+URL@COMMIT#path=SUB` (what `pin` records for a skill checkout).
fn parse_git_plus(rest: &str) -> Result<GitSource> {
    let (url_part, frag) = rest.split_once('#').map(|(a, b)| (a, Some(b))).unwrap_or((rest, None));
    let mut g = GitSource { url: String::new(), fetch_url: String::new(), subpath: None, reference: None, constraint: None, apm: false, prerelease: false };
    let mut url = url_part.to_string();
    // `URL@COMMIT`: the `@` after the host and path (not `git@host`)
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    if let Some(at) = url.rfind('@').filter(|&at| at > after_scheme && url[..at].rfind('/').is_some_and(|s| s < at) && url[after_scheme..at].contains('/')) {
        let r = url[at + 1..].to_string();
        url.truncate(at);
        g = with_ref(g, Some(&r));
    }
    for kv in frag.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k {
            "path" | "subdirectory" => g.subpath = clean(v),
            "tag" | "ref" | "rev" | "branch" | "commit" => g.reference = Some(v.to_string()),
            "version" => g.constraint = Some(v.to_string()),
            other => return Err(err(format!("unknown {other:?} in {rest:?}: use path, tag, ref, commit or version"))),
        }
    }
    g.fetch_url = url.clone();
    g.url = crate::pull::strip_credentials(&url);
    Ok(g)
}

/// Parse a source spec for a dependency of `kind`.
pub fn parse(spec: &str, kind: Kind) -> Result<Source> {
    let spec = spec.trim();
    let src = parse_raw(spec, kind).map_err(|e| match e {
        Error::Dependency(m) => err(format!("can't read source {:?}: {m}", crate::pull::strip_credentials(spec))),
        e => e,
    })?;
    check_source(&src)?;
    Ok(src)
}

fn parse_raw(spec: &str, kind: Kind) -> Result<Source> {
    if spec.is_empty() {
        return Err(err("it's empty"));
    }
    if let Some(rest) = spec.strip_prefix("apm:") {
        return Ok(Source::Git(parse_apm(rest)?));
    }
    if let Some(rest) = spec.strip_prefix("github:").or_else(|| spec.strip_prefix("gh:")) {
        return Ok(Source::Git(parse_github(rest, kind == Kind::Package)?));
    }
    if let Some(rest) = spec.strip_prefix("git+") {
        let mut g = parse_git_plus(rest)?;
        g.apm = kind == Kind::Package;
        return Ok(Source::Git(g));
    }
    if spec.starts_with("gitlab:") || spec.starts_with("git:") {
        // `pull`'s syntax: `//PATH` for a subpath, `@REF` at the end
        let (body, r) = split_at_ref(spec);
        let constraint = r.filter(|r| is_constraint(r));
        let s = crate::pull::parse(if constraint.is_some() { body } else { spec }).map_err(|e| err(e.to_string()))?;
        let g = GitSource { url: s.url, fetch_url: s.fetch_url, subpath: s.path, reference: s.reference, constraint: constraint.map(str::to_string), apm: kind == Kind::Package, prerelease: false };
        return Ok(Source::Git(g));
    }
    if let Some(rest) = spec.strip_prefix("npm:") {
        let (package, c) = split_package(rest);
        return Ok(Source::Npm { package, constraint: c });
    }
    if let Some(rest) = spec.strip_prefix("pypi:") {
        let (package, c) = match rest.split_once("==") {
            Some((p, v)) => (p.to_string(), Some(format!("={v}"))),
            None => split_package(rest),
        };
        return Ok(Source::Pypi { package, constraint: c });
    }
    if let Some(rest) = spec.strip_prefix("plugin:") {
        let (marketplace, name) = rest.split_once('/').ok_or_else(|| err("expected plugin:MARKETPLACE/NAME"))?;
        return Ok(Source::Plugin { marketplace: marketplace.to_string(), name: name.to_string() });
    }
    if spec.starts_with("https://") || spec.starts_with("http://") {
        if kind == Kind::Mcp {
            return Ok(Source::Remote { url: spec.to_string() });
        }
        // a repository or file URL from the browser
        let (body, r) = split_at_ref(spec);
        let constraint = r.filter(|r| is_constraint(r));
        let s = crate::pull::parse(if constraint.is_some() { body } else { spec }).map_err(|e| err(e.to_string()))?;
        let g = GitSource { url: s.url, fetch_url: s.fetch_url, subpath: s.path, reference: s.reference, constraint: constraint.map(str::to_string), apm: kind == Kind::Package, prerelease: false };
        return Ok(Source::Git(g));
    }
    if let Some(rest) = spec.strip_prefix("path:") {
        return Ok(Source::Path { path: rest.to_string(), editable: false });
    }
    if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') || spec.starts_with("~/") || spec == "." {
        return Ok(Source::Path { path: spec.to_string(), editable: false });
    }
    Err(err(
        "expected github:OWNER/REPO[/PATH][@REF], git+URL#path=…, apm:OWNER/REPO, npm:PKG, pypi:PKG, plugin:MARKETPLACE/NAME, a URL, or ./PATH",
    ))
}

/// `@scope/name@range` or `name@range` → (package, range).
fn split_package(s: &str) -> (String, Option<String>) {
    let (scope, body) = s.strip_prefix('@').map(|b| ("@", b)).unwrap_or(("", s));
    match body.split_once('@') {
        Some((n, v)) if !v.is_empty() => (format!("{scope}{n}"), Some(v.to_string())),
        Some((n, _)) => (format!("{scope}{n}"), None),
        None => (format!("{scope}{body}"), None),
    }
}

/// What reaches `git` or a package manager from a source: no option-like or shell-special
/// values, no transport helpers, a subpath that stays inside the repository.
fn check_source(src: &Source) -> Result<()> {
    match src {
        Source::Git(g) => {
            let u = &g.fetch_url;
            if u.is_empty() || u.starts_with('-') || u.contains("::") || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(err(format!("the repository URL {:?} can't be empty, start with '-', or contain '::' or spaces", g.url)));
            }
            if let Some(r) = &g.reference {
                if !plain_token(r) {
                    return Err(err(format!("{r:?} isn't a branch, tag or commit (letters, digits and ._/@+- only)")));
                }
            }
            if let Some(c) = &g.constraint {
                VersionRange::parse(c)?;
            }
            if let Some(p) = &g.subpath {
                if !safe_rel(p) {
                    return Err(err(format!("{p:?} isn't a path inside the repository")));
                }
            }
        }
        Source::Npm { package, constraint } | Source::Pypi { package, constraint } => {
            if !plain_token(package) {
                return Err(err(format!("{package:?} isn't a package name")));
            }
            if let Some(c) = constraint {
                VersionRange::parse(c)?;
            }
        }
        Source::Plugin { marketplace, name } => {
            for v in [marketplace, name] {
                if !plain_token(v) || v.contains('/') {
                    return Err(err(format!("{v:?} isn't a marketplace or plugin name")));
                }
            }
        }
        Source::Remote { url } => {
            if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(err(format!("{url:?} isn't a URL")));
            }
            // the source is written into harness.toml and the lock, which are committed
            if crate::capture::strip_userinfo(url) != *url {
                return Err(err("the URL carries a credential (a user name, password, or a key in its query); hlock won't write it into harness.toml. Add the server without it, then give it the key the way its docs say (a header or an environment variable)"));
            }
        }
        Source::Path { path, .. } => {
            if path.contains('\0') {
                return Err(err("the path has a NUL character"));
            }
        }
        Source::Defined { .. } => {}
    }
    Ok(())
}

/// The name a dependency is declared under when `--as` doesn't give one: a skill's
/// directory, a file's stem, a server package without its `server-`/`mcp-` affixes.
pub fn default_name(src: &Source) -> String {
    let raw = match src {
        Source::Git(g) => match &g.subpath {
            Some(p) => {
                let last = p.rsplit('/').next().unwrap_or(p);
                if is_file_path(last) {
                    // `style.instructions.md` → `style`, `SKILL.md` → its directory
                    let stem = last.split('.').next().unwrap_or(last);
                    if stem.eq_ignore_ascii_case("skill") {
                        p.rsplit('/').nth(1).unwrap_or(stem).to_string()
                    } else {
                        stem.to_string()
                    }
                } else {
                    last.to_string()
                }
            }
            None => g.repo_name(),
        },
        Source::Path { path, .. } => {
            let p = path.trim_end_matches('/');
            let last = p.rsplit('/').next().unwrap_or(p);
            if is_file_path(last) { last.split('.').next().unwrap_or(last).to_string() } else { last.to_string() }
        }
        Source::Npm { package, .. } | Source::Pypi { package, .. } => {
            let n = package.rsplit('/').next().unwrap_or(package);
            let n = n.strip_prefix("mcp-server-").or_else(|| n.strip_prefix("server-")).unwrap_or(n);
            let n = n.strip_suffix("-mcp-server").or_else(|| n.strip_suffix("-mcp")).unwrap_or(n);
            n.to_string()
        }
        Source::Remote { url } => {
            let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
            let authority = rest.split(['/', '?']).next().unwrap_or("");
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h).split(':').next().unwrap_or("");
            host.split('.').find(|l| !matches!(*l, "mcp" | "www" | "api" | "")).unwrap_or(host).to_string()
        }
        Source::Plugin { name, .. } => name.clone(),
        Source::Defined { name, .. } => name.rsplit('/').next().unwrap_or(name).to_string(),
    };
    crate::pull::sanitize(&raw)
}

// --------------------------------------------------------------------------- //
// Versions
// --------------------------------------------------------------------------- //

/// An npm-style version range (`^1.2`, `~1.2.3`, `>=1.0 <2`, `1.x`, `1.2 - 1.4`, `a || b`),
/// as alternatives of `semver` requirements. A bare version (`1.2.0`) is exact.
#[derive(Debug, Clone)]
pub struct VersionRange {
    alternatives: Vec<semver::VersionReq>,
}

impl VersionRange {
    pub fn parse(s: &str) -> Result<VersionRange> {
        let bad = |why: String| err(format!("{s:?} isn't a version range: {why}"));
        let mut alternatives = Vec::new();
        for alt in s.split("||") {
            let alt = alt.trim();
            if alt.is_empty() {
                return Err(bad("an empty alternative".into()));
            }
            let mut parts: Vec<String> = Vec::new();
            if let Some((lo, hi)) = alt.split_once(" - ") {
                parts.push(format!(">={}", lo.trim()));
                parts.push(format!("<={}", hi.trim()));
            } else {
                // `>= 1.0` → `>=1.0`, then one comparator per word
                let mut words: Vec<String> = Vec::new();
                for w in alt.split_whitespace() {
                    match words.last_mut() {
                        Some(last) if last.chars().all(|c| "<>=~^".contains(c)) => last.push_str(w),
                        _ => words.push(w.to_string()),
                    }
                }
                for w in words {
                    let w = w.trim_end_matches(',').replace(['x', 'X'], "*");
                    let op_end = w.find(|c: char| !"<>=~^".contains(c)).unwrap_or(w.len());
                    let (op, ver) = w.split_at(op_end);
                    let ver = ver.strip_prefix('v').unwrap_or(ver);
                    // a bare version is exact, as npm reads it (Cargo would read it as `^`)
                    let op = if op.is_empty() && !ver.contains('*') { "=" } else { op };
                    parts.push(format!("{op}{ver}"));
                }
            }
            let req = semver::VersionReq::parse(&parts.join(", ")).map_err(|e| bad(e.to_string()))?;
            alternatives.push(req);
        }
        Ok(VersionRange { alternatives })
    }

    /// Whether `v` is in the range. A pre-release counts only when the range names one
    /// on the same `major.minor.patch` (as node-semver reads it), or with `prerelease`.
    pub fn allows(&self, v: &semver::Version, prerelease: bool) -> bool {
        if self.alternatives.iter().any(|r| r.matches(v)) {
            return true;
        }
        prerelease && !v.pre.is_empty() && {
            let release = semver::Version::new(v.major, v.minor, v.patch);
            self.alternatives.iter().any(|r| r.matches(&release))
        }
    }

    pub fn matches(&self, v: &semver::Version) -> bool {
        self.allows(v, false)
    }

    /// The highest of `versions` the range allows. Versions equal but for build metadata
    /// are ordered by their full text, bytewise (OpenAPM req-rs-014).
    pub fn best<T>(&self, versions: impl IntoIterator<Item = (semver::Version, T)>) -> Option<(semver::Version, T)>
    where
        T: AsRef<str>,
    {
        self.best_with(versions, false)
    }

    /// [`VersionRange::best`], counting pre-releases when `prerelease` is set.
    pub fn best_with<T>(&self, versions: impl IntoIterator<Item = (semver::Version, T)>, prerelease: bool) -> Option<(semver::Version, T)>
    where
        T: AsRef<str>,
    {
        versions.into_iter().filter(|(v, _)| self.allows(v, prerelease)).max_by(|a, b| {
            let key = |v: &semver::Version| (v.major, v.minor, v.patch, v.pre.clone());
            key(&a.0).cmp(&key(&b.0)).then_with(|| a.1.as_ref().as_bytes().cmp(b.1.as_ref().as_bytes()))
        })
    }
}

/// A version from a tag or a package index, leniently: `v1.2` → 1.2.0, `1` → 1.0.0.
pub fn lenient_version(s: &str) -> Option<semver::Version> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    if let Ok(v) = semver::Version::parse(s) {
        return Some(v);
    }
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (s, None),
    };
    let nums: Vec<&str> = core.split('.').collect();
    if nums.is_empty() || nums.len() > 3 || !nums.iter().all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let mut padded = nums.join(".");
    for _ in nums.len()..3 {
        padded.push_str(".0");
    }
    if let Some(p) = pre {
        padded = format!("{padded}-{p}");
    }
    semver::Version::parse(&padded).ok()
}

/// The version a tag names for `package`, by APM's tag patterns: `v{version}`,
/// `{package}--v{version}` or `{package}-v{version}` (and a bare `{version}`, which
/// [`tag_versions`] counts only when no tag matches the others).
pub fn tag_version(tag: &str, package: &str) -> Option<semver::Version> {
    let body = tag.strip_prefix(&format!("{package}--")).or_else(|| tag.strip_prefix(&format!("{package}-"))).unwrap_or(tag);
    full_version(body.strip_prefix('v')?)
}

/// A full `X.Y.Z[-pre][+build]` version, as tags spell them (no padding).
fn full_version(s: &str) -> Option<semver::Version> {
    let core = s.split(['-', '+']).next().unwrap_or(s);
    (core.split('.').count() == 3).then(|| semver::Version::parse(s).ok()).flatten()
}

/// The versions `tags` name for `package` ([`tag_version`]), else, when none do, the tags
/// that are bare versions (`1.2.3`).
pub fn tag_versions<'a>(tags: impl IntoIterator<Item = &'a String> + Clone, package: &str) -> Vec<(semver::Version, &'a String)> {
    let named: Vec<(semver::Version, &String)> = tags.clone().into_iter().filter_map(|t| tag_version(t, package).map(|v| (v, t))).collect();
    if !named.is_empty() {
        return named;
    }
    tags.into_iter().filter_map(|t| full_version(t).map(|v| (v, t))).collect()
}

// --------------------------------------------------------------------------- //
// git: the shared cache
// --------------------------------------------------------------------------- //

/// How long one git command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(600);

/// Run git in `dir`, never prompting, with only plain transports allowed. `extra` adds
/// `-c` settings (the credentialed remote URL, which is never written to disk).
fn git_in(dir: &Path, extra: &[(&str, &str)], args: &[&str], env: &[(&str, &Path)]) -> Result<String> {
    let mut cmd = Command::new("git");
    for (k, v) in extra {
        cmd.arg("-c").arg(format!("{k}={v}"));
    }
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        // no transport helpers (`ext::`, `fd::`) from a source someone else wrote
        .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = crate::process::run(cmd, &crate::process::Limits::timeout(GIT_TIMEOUT)).map_err(|e| err(format!("can't run git: {e} (dependencies need git on PATH)")))?;
    if !out.success {
        let why = if out.timed_out { format!("timed out after {}s", GIT_TIMEOUT.as_secs()) } else { crate::pull::redact_urls(out.stderr_text().trim()) };
        return Err(err(format!("git {} failed: {why}", args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""))));
    }
    Ok(out.stdout_text().trim().to_string())
}

/// Where bare repositories are cached: `~/.hlock/cache/git/`, shared by every project.
pub fn cache_root() -> PathBuf {
    crate::pull::home_dir().join("cache").join("git")
}

/// The cached bare repository for a remote, created on first use. Its `origin` is the
/// URL without credentials; [`remote_override`] supplies the credentialed one per call.
fn cache_repo(g: &GitSource) -> Result<PathBuf> {
    let norm = g.url.trim_end_matches('/').trim_end_matches(".git").to_lowercase();
    let tail: Vec<&str> = norm.rsplit(['/', ':']).take(2).collect();
    let label = crate::pull::sanitize(&tail.into_iter().rev().collect::<Vec<_>>().join("-"));
    let dir = cache_root().join(format!("{label}-{}", &crate::hash::sha256_hex(norm.as_bytes())[..12]));
    if !dir.join("HEAD").is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| crate::error::io_at(&dir, e))?;
        git_in(&dir, &[], &["init", "-q", "--bare"], &[])?;
        git_in(&dir, &[], &["remote", "add", "--", "origin", &g.url], &[])?;
    }
    Ok(dir)
}

/// `-c remote.origin.url=…` with credentials, when the source has them.
fn remote_override(g: &GitSource) -> Vec<(&'static str, &str)> {
    if g.fetch_url != g.url && !g.fetch_url.is_empty() {
        vec![("remote.origin.url", g.fetch_url.as_str())]
    } else {
        Vec::new()
    }
}

/// What a remote advertises: its default branch, branches and tags (tags peeled to commits).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RemoteRefs {
    pub head: Option<String>,
    /// The default branch's name, when the remote says.
    pub default_branch: Option<String>,
    pub heads: BTreeMap<String, String>,
    pub tags: BTreeMap<String, String>,
}

/// `git ls-remote --symref` for a source: one round trip, nothing cloned.
pub fn ls_remote(g: &GitSource) -> Result<RemoteRefs> {
    let tmp = std::env::temp_dir();
    let out = git_in(&tmp, &[], &["ls-remote", "--symref", "--", &g.fetch_url], &[]).map_err(|e| err(format!("can't list {}: {e}\nif the repository is private, check that `git ls-remote {}` works", g.url, g.url)))?;
    Ok(parse_ls_remote(&out))
}

fn parse_ls_remote(out: &str) -> RemoteRefs {
    let mut r = RemoteRefs::default();
    let mut peeled = BTreeMap::new();
    for line in out.lines() {
        let Some((a, b)) = line.split_once('\t') else { continue };
        if let Some(target) = a.strip_prefix("ref: ") {
            if b == "HEAD" {
                r.default_branch = target.strip_prefix("refs/heads/").map(str::to_string);
            }
            continue;
        }
        if b == "HEAD" {
            r.head = Some(a.to_string());
        } else if let Some(t) = b.strip_prefix("refs/tags/") {
            match t.strip_suffix("^{}") {
                Some(t) => {
                    peeled.insert(t.to_string(), a.to_string());
                }
                None => {
                    r.tags.insert(t.to_string(), a.to_string());
                }
            }
        } else if let Some(h) = b.strip_prefix("refs/heads/") {
            r.heads.insert(h.to_string(), a.to_string());
        }
    }
    // an annotated tag's own object isn't the commit: take the peeled one
    r.tags.extend(peeled);
    r
}

/// List several remotes at once, one thread each.
pub fn ls_remote_many(sources: &[&GitSource]) -> BTreeMap<String, Result<RemoteRefs>> {
    let mut urls: Vec<&GitSource> = Vec::new();
    for g in sources {
        if !urls.iter().any(|u| u.url == g.url) {
            urls.push(g);
        }
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = urls.iter().map(|g| (g.url.clone(), s.spawn(move || ls_remote(g)))).collect();
        handles.into_iter().map(|(u, h)| (u, h.join().unwrap_or_else(|_| Err(err("listing the remote panicked"))))).collect()
    })
}

/// A commit a source resolved to, and the tag or branch it came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Pick {
    pub commit: String,
    pub resolved_ref: Option<String>,
    pub resolved_tag: Option<String>,
}

fn is_full_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// The commit `g` asks for among a remote's refs: the highest tag a range matches, a
/// tag or branch by name, a full commit as is, or the default branch's head.
pub fn pick(g: &GitSource, refs: &RemoteRefs) -> Result<Pick> {
    if let Some(c) = &g.constraint {
        let range = VersionRange::parse(c)?;
        let name = g.package_name();
        let versions = tag_versions(refs.tags.keys(), &name);
        let Some((_, tag)) = range.best_with(versions.iter().map(|(v, t)| (v.clone(), t.as_str())), g.prerelease) else {
            let mut seen: Vec<&semver::Version> = versions.iter().map(|(v, _)| v).collect();
            seen.sort();
            let shown = if seen.is_empty() { "none".into() } else { seen.iter().rev().take(8).map(|v| v.to_string()).collect::<Vec<_>>().join(", ") };
            return Err(err(format!("no tag of {} matches {c:?} (versions tagged v{{version}} or {name}--v{{version}}: {shown})", g.url)));
        };
        let sha = &refs.tags[tag];
        return Ok(Pick { commit: sha.clone(), resolved_ref: Some(tag.to_string()), resolved_tag: Some(tag.to_string()) });
    }
    match &g.reference {
        Some(r) if is_full_sha(r) => Ok(Pick { commit: r.to_lowercase(), ..Default::default() }),
        Some(r) => {
            if let Some(sha) = refs.tags.get(r) {
                return Ok(Pick { commit: sha.clone(), resolved_ref: Some(r.clone()), resolved_tag: Some(r.clone()) });
            }
            if let Some(sha) = refs.heads.get(r) {
                return Ok(Pick { commit: sha.clone(), resolved_ref: Some(r.clone()), resolved_tag: None });
            }
            let hint = if r.chars().all(|c| c.is_ascii_hexdigit()) { "; to pin a commit, give its full SHA" } else { "" };
            Err(err(format!("{} has no branch or tag {r:?}{hint}", g.url)))
        }
        None => {
            let head = refs.head.clone().ok_or_else(|| err(format!("{} has no default branch", g.url)))?;
            Ok(Pick { commit: head, resolved_ref: refs.default_branch.clone(), resolved_tag: None })
        }
    }
}

/// Make sure the cache has `commit` (fetching `refname`, else the commit itself, else
/// every branch and tag), shallowly and without blobs: files are fetched when read.
fn ensure_commit(repo: &Path, g: &GitSource, commit: &str, refname: Option<&str>) -> Result<()> {
    let extra = remote_override(g);
    let has = |repo: &Path| git_in(repo, &extra, &["cat-file", "-e", &format!("{commit}^{{commit}}")], &[]).is_ok();
    if has(repo) {
        return Ok(());
    }
    let mut last = None;
    let mut attempts: Vec<Vec<&str>> = Vec::new();
    if let Some(r) = refname {
        attempts.push(vec!["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", "--", r]);
    }
    attempts.push(vec!["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", "--", commit]);
    attempts.push(vec!["fetch", "-q", "--filter=blob:none", "origin", "--", "+refs/heads/*:refs/hlock/heads/*", "+refs/tags/*:refs/tags/*"]);
    for args in attempts {
        match git_in(repo, &extra, &args, &[]) {
            Ok(_) if has(repo) => return Ok(()),
            Ok(_) => {}
            Err(e) => last = Some(e),
        }
    }
    let why = last.map(|e| format!(": {e}")).unwrap_or_default();
    Err(err(format!("{} has no commit {commit}{why}", g.url)))
}

/// Check out `subpath` (or everything) of `commit` from the cache into `dest`.
fn export(repo: &Path, g: &GitSource, commit: &str, dest: &Path) -> Result<PathBuf> {
    let extra = remote_override(g);
    let what = g.subpath.as_deref().unwrap_or(".");
    if let Some(sub) = &g.subpath {
        git_in(repo, &extra, &["cat-file", "-e", &format!("{commit}:{sub}")], &[]).map_err(|_| err(format!("{}@{} has no {sub}", g.url, crate::fsutil::prefix_chars(commit, 10))))?;
    }
    std::fs::create_dir_all(dest)?;
    let index = dest.with_extension("index");
    let work = format!("--work-tree={}", dest.display());
    let gitdir = format!("--git-dir={}", repo.display());
    // a throwaway index, so concurrent exports from one cache don't share one
    let r = git_in(repo, &extra, &[&gitdir, &work, "checkout", "-q", commit, "--", what], &[("GIT_INDEX_FILE", &index)]);
    let _ = std::fs::remove_file(&index);
    r?;
    Ok(if what == "." { dest.to_path_buf() } else { dest.join(what) })
}

// --------------------------------------------------------------------------- //
// The store: fetched trees by digest
// --------------------------------------------------------------------------- //

/// Where a project keeps fetched dependency trees: `.hlock/store/trees/<digest>/`.
pub fn trees_dir(project: &Path) -> PathBuf {
    project.join(".hlock").join("store").join("trees")
}

/// The digest of a dependency's content: a tree digest for a directory, `sha256` of the
/// bytes for a file.
pub fn content_digest(p: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(p).map_err(|e| crate::error::io_at(p, e))?;
    if meta.is_dir() {
        Ok(crate::capture::tree_digest(p))
    } else if meta.is_file() {
        Ok(crate::hash::sha256_bytes(&std::fs::read(p).map_err(|e| crate::error::io_at(p, e))?))
    } else {
        Err(err(format!("{} isn't a file or directory", p.display())))
    }
}

/// The stored copy of `digest`, if the store has it: the tree, or the file inside it.
pub fn stored(project: &Path, digest: &str) -> Option<PathBuf> {
    let hex = digest.split_once(':').map(|(_, h)| h).unwrap_or(digest);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) || hex.is_empty() {
        return None;
    }
    let dir = trees_dir(project).join(hex);
    let marker = std::fs::read_to_string(dir.join(".hlock-file")).ok();
    let file = marker.filter(|n| crate::fsutil::plain_name(n.trim())).map(|n| dir.join(n.trim())).filter(|p| p.is_file());
    // what's there is what the digest says, or it isn't used: the store is on disk, like anything else
    file.into_iter().chain(dir.is_dir().then(|| dir.clone())).find(|p| content_digest(p).ok().as_deref() == Some(digest))
}

/// Copy `from` (a directory or file, never following symlinks) into the store under its
/// digest. Returns the digest and the stored path.
pub fn store(project: &Path, from: &Path) -> Result<(String, PathBuf)> {
    let digest = content_digest(from)?;
    if let Some(p) = stored(project, &digest) {
        return Ok((digest, p));
    }
    let hex = digest.split_once(':').map(|(_, h)| h).unwrap_or(&digest).to_string();
    let root = trees_dir(project);
    std::fs::create_dir_all(&root).map_err(|e| crate::error::io_at(&root, e))?;
    let staged = root.join(format!(".{hex}.hlock-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staged);
    let is_file = std::fs::symlink_metadata(from).is_ok_and(|m| m.is_file());
    let copied = (|| -> Result<()> {
        if is_file {
            let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
            crate::fsutil::copy_tree(from, &staged.join(&name))?;
            std::fs::write(staged.join(".hlock-file"), &name)?;
        } else {
            crate::fsutil::copy_tree(from, &staged)?;
        }
        Ok(())
    })();
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }
    let dest = root.join(&hex);
    if std::fs::rename(&staged, &dest).is_err() {
        // another process stored it first
        let _ = std::fs::remove_dir_all(&staged);
    }
    stored(project, &digest).map(|p| (digest.clone(), p)).ok_or_else(|| err(format!("couldn't store {}", from.display())))
}

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Result<Scratch> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let p = std::env::temp_dir().join(format!("hlock-{tag}-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&p).map_err(|e| crate::error::io_at(&p, e))?;
        Ok(Scratch(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Fetch `g` at `commit` into the project's store (through the shared cache) and return
/// the digest and stored path. With `expect`, the store is checked first and the fetched
/// content must match it.
pub fn fetch_git(project: &Path, g: &GitSource, commit: &str, refname: Option<&str>, expect: Option<&str>) -> Result<(String, PathBuf)> {
    if let Some(p) = expect.and_then(|d| stored(project, d)) {
        return Ok((expect.unwrap_or_default().to_string(), p));
    }
    let repo = cache_repo(g)?;
    ensure_commit(&repo, g, commit, refname)?;
    let scratch = Scratch::new("dep")?;
    let out = export(&repo, g, commit, &scratch.0.join("tree"))?;
    let (digest, path) = store(project, &out)?;
    if let Some(want) = expect {
        if want != digest {
            return Err(err(format!("{}@{} {} has digest {digest}, but the lock says {want}", g.url, crate::fsutil::prefix_chars(commit, 10), g.subpath.as_deref().unwrap_or("."))));
        }
    }
    Ok((digest, path))
}

/// Read one file of `g` at `commit` (an `apm.yml`, a marketplace's index), through the cache.
pub fn read_git_file(g: &GitSource, commit: &str, refname: Option<&str>, rel: &str) -> Result<Option<String>> {
    let repo = cache_repo(g)?;
    ensure_commit(&repo, g, commit, refname)?;
    let extra = remote_override(g);
    match git_in(&repo, &extra, &["cat-file", "-p", &format!("{commit}:{rel}")], &[]) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Ok(None),
    }
}

// --------------------------------------------------------------------------- //
// Package indexes (MCP servers)
// --------------------------------------------------------------------------- //

fn http_json(url: &str) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build().into();
    let mut resp = agent.get(url).call().map_err(|e| err(format!("can't read {url}: {e}")))?;
    let text = resp.body_mut().read_to_string().map_err(|e| err(format!("can't read {url}: {e}")))?;
    serde_json::from_str(&text).map_err(|e| err(format!("{url} isn't JSON: {e}")))
}

/// The newest version of `versions` in `range` (or `latest` without one), skipping
/// pre-releases unless the range names one.
fn pick_version<'a>(versions: impl Iterator<Item = &'a String>, range: Option<&str>, latest: Option<&str>) -> Result<Option<String>> {
    let parsed: Vec<(semver::Version, &String)> = versions.filter_map(|v| lenient_version(v).map(|p| (p, v))).collect();
    match range {
        Some(r) => {
            let range = VersionRange::parse(r)?;
            Ok(range.best(parsed.into_iter()).map(|(_, v)| v.clone()))
        }
        None => Ok(latest.map(str::to_string).or_else(|| parsed.into_iter().filter(|(v, _)| v.pre.is_empty()).max_by(|a, b| a.0.cmp(&b.0)).map(|(_, v)| v.clone()))),
    }
}

/// An npm package's newest version in `range`, and its integrity (`sha512-…`).
pub fn npm_resolve(package: &str, range: Option<&str>) -> Result<(String, Option<String>)> {
    let base = crate::pull::base_url("HLOCK_NPM_REGISTRY", "https://registry.npmjs.org");
    let v = http_json(&format!("{base}/{}", package.replace('/', "%2f")))?;
    let versions = v["versions"].as_object().ok_or_else(|| err(format!("npm has no package {package}")))?;
    let version = pick_version(versions.keys(), range, v["dist-tags"]["latest"].as_str())?.ok_or_else(|| err(format!("no version of npm:{package} matches {:?}", range.unwrap_or("*"))))?;
    let integrity = versions.get(&version).and_then(|x| x["dist"]["integrity"].as_str()).map(str::to_string);
    Ok((version, integrity))
}

/// A PyPI package's newest version in `range`, and the sha256 of its first file.
pub fn pypi_resolve(package: &str, range: Option<&str>) -> Result<(String, Option<String>)> {
    let base = crate::pull::base_url("HLOCK_PYPI_URL", "https://pypi.org");
    let v = http_json(&format!("{base}/pypi/{package}/json"))?;
    let releases = v["releases"].as_object().ok_or_else(|| err(format!("PyPI has no package {package}")))?;
    let live: Vec<&String> = releases.iter().filter(|(_, files)| files.as_array().is_some_and(|f| !f.is_empty() && f.iter().any(|x| !x["yanked"].as_bool().unwrap_or(false)))).map(|(k, _)| k).collect();
    let latest = v["info"]["version"].as_str().filter(|l| live.iter().any(|x| x == l));
    let version = pick_version(live.into_iter(), range, latest)?.ok_or_else(|| err(format!("no version of pypi:{package} matches {:?}", range.unwrap_or("*"))))?;
    let files = releases[&version].as_array().cloned().unwrap_or_default();
    let file = files.iter().find(|f| f["packagetype"] == "sdist").or_else(|| files.first());
    let integrity = file.and_then(|f| f["digests"]["sha256"].as_str()).map(|h| format!("sha256:{h}"));
    Ok((version, integrity))
}

// --------------------------------------------------------------------------- //
// Declarations
// --------------------------------------------------------------------------- //

/// A dependency as `harness.toml` (or `apm.yml`) declares it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decl {
    pub kind: Kind,
    pub name: String,
    /// The source as written; a table is written as the string it stands for.
    pub spec: String,
    #[serde(skip)]
    pub source: Option<Source>,
    /// `project` or `user`.
    pub scope: String,
    pub editable: bool,
    /// Where it's declared: `harness.toml` or `apm.yml`.
    pub from: String,
}

impl Decl {
    pub fn source(&self) -> Result<Source> {
        match &self.source {
            Some(s) => Ok(s.clone()),
            None => parse(&self.spec, self.kind),
        }
    }
}

/// The source string a manifest table stands for (`{ git = …, path = …, tag = … }` is
/// `git+…#path=…&tag=…`), with its scope and whether it's editable. `None` for a table
/// that declares no source (`[skills.trust]`, `[instructions] files`).
fn entry_spec(v: &Value) -> Result<Option<(String, String, bool)>> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let scope = s("scope").unwrap_or_else(|| "project".into());
    if !matches!(scope.as_str(), "project" | "user") {
        return Err(err(format!("scope {scope:?} isn't project or user")));
    }
    let editable = v.get("editable").and_then(Value::as_bool).unwrap_or(false);
    if let Some(text) = v.as_str() {
        return Ok(Some((text.to_string(), scope, false)));
    }
    if !v.is_object() {
        return Ok(None);
    }
    let at = |base: String, key: &str| match s(key) {
        Some(r) => format!("{base}@{r}"),
        None => base,
    };
    let spec = if let Some(src) = s("source").or_else(|| s("mcp")) {
        src
    } else if let Some(url) = s("git") {
        let mut frag = Vec::new();
        if let Some(p) = s("path") {
            frag.push(format!("path={p}"));
        }
        for k in ["tag", "rev", "ref", "branch", "commit"] {
            if let Some(r) = s(k) {
                frag.push(format!("ref={r}"));
            }
        }
        if let Some(r) = s("version") {
            frag.push(format!("version={r}"));
        }
        let url = if url.contains("://") || url.contains('@') { url } else { format!("https://{url}") };
        if frag.is_empty() { format!("git+{url}") } else { format!("git+{url}#{}", frag.join("&")) }
    } else if let Some(gh) = s("gh").or_else(|| s("github")) {
        at(format!("github:{gh}"), "rev")
    } else if let Some(p) = s("apm") {
        at(format!("apm:{p}"), "ref")
    } else if let Some(p) = s("npm") {
        at(format!("npm:{p}"), "version")
    } else if let Some(p) = s("pypi") {
        at(format!("pypi:{p}"), "version")
    } else if let Some(p) = s("plugin") {
        format!("plugin:{}", p.strip_prefix("claude:").unwrap_or(&p))
    } else if let Some(u) = s("url") {
        u
    } else if let Some(p) = s("path") {
        if p.starts_with(['.', '/', '~']) { p } else { format!("./{p}") }
    } else {
        return Ok(None);
    };
    Ok(Some((spec, scope, editable)))
}

/// The dependencies `harness.toml` declares, by kind and name. `[tools]` entries with an
/// `mcp` source count as MCP servers, as v0's manifest spells them.
pub fn declared(m: &crate::manifest::Manifest) -> Result<Vec<Decl>> {
    let mut out: Vec<Decl> = Vec::new();
    let mut add = |kind: Kind, name: &str, v: &Value, section: &str| -> Result<()> {
        let Some((spec, scope, editable)) = entry_spec(v).map_err(|e| err(format!("[{section}] {name}: {e}")))? else { return Ok(()) };
        if !crate::fsutil::plain_name(name) {
            return Err(err(format!("[{section}] {name:?}: a dependency's name must be a plain name (no '/', '\\\\' or '..')")));
        }
        let source = parse(&spec, kind).map_err(|e| err(format!("[{section}] {name}: {e}")))?;
        if out.iter().any(|d| d.kind == kind && d.name == name) {
            return Err(err(format!("{kind} {name} is declared twice ([mcp] and [tools])")));
        }
        let editable = editable || matches!(source, Source::Path { editable: true, .. });
        out.push(Decl { kind, name: name.to_string(), spec: crate::pull::strip_credentials(&spec), source: Some(source), scope, editable, from: crate::MANIFEST_PATH.into() });
        Ok(())
    };
    for kind in Kind::ALL {
        let table = match kind {
            Kind::Skill => &m.skills,
            Kind::Mcp => &m.mcp,
            Kind::Instructions => &m.instructions,
            Kind::Plugin => &m.plugins,
            Kind::Package => &m.packages,
        };
        for (name, v) in table {
            if (kind == Kind::Instructions && name == "files") || (kind == Kind::Skill && matches!(name.as_str(), "builtin" | "trust")) {
                continue;
            }
            add(kind, name, v, kind.section())?;
        }
    }
    for (name, v) in &m.tools {
        if v.get("mcp").is_some() {
            add(Kind::Mcp, name, v, "tools")?;
        }
    }
    Ok(out)
}

// --------------------------------------------------------------------------- //
// Resolution
// --------------------------------------------------------------------------- //

/// A lock entry of any kind a dependency can be.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Entry {
    Skill(Skill),
    Tool(Tool),
    Instructions(Instructions),
    Native(Native),
}

impl Entry {
    pub fn kind(&self) -> Kind {
        match self {
            Entry::Skill(_) => Kind::Skill,
            Entry::Tool(_) => Kind::Mcp,
            Entry::Instructions(_) => Kind::Instructions,
            Entry::Native(_) => Kind::Plugin,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Entry::Skill(s) => &s.name,
            Entry::Tool(t) => &t.name,
            Entry::Instructions(i) => i.source.as_deref().map(|_| i.name.as_str()).unwrap_or(&i.name),
            Entry::Native(n) => &n.name,
        }
    }

    pub fn dep(&self) -> &Declared {
        match self {
            Entry::Skill(s) => &s.dep,
            Entry::Tool(t) => &t.dep,
            Entry::Instructions(i) => &i.dep,
            Entry::Native(n) => &n.dep,
        }
    }

    pub fn dep_mut(&mut self) -> &mut Declared {
        match self {
            Entry::Skill(s) => &mut s.dep,
            Entry::Tool(t) => &mut t.dep,
            Entry::Instructions(i) => &mut i.dep,
            Entry::Native(n) => &mut n.dep,
        }
    }

    pub fn source(&self) -> Option<&str> {
        match self {
            Entry::Skill(s) => s.source.as_deref(),
            Entry::Tool(t) => t.source.as_deref(),
            Entry::Instructions(i) => i.source.as_deref(),
            Entry::Native(n) => n.source.as_deref(),
        }
    }

    pub fn digest(&self) -> Option<&str> {
        match self {
            Entry::Skill(s) => s.digest.as_deref(),
            Entry::Instructions(i) => i.digest.as_deref(),
            Entry::Tool(t) => t.integrity.as_deref(),
            Entry::Native(n) => n.integrity.as_deref(),
        }
    }
}

/// What a declared dependency resolved to: its lock entry, and where its content is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolved {
    /// The declaration's kind and name (a package's entries carry its name in `required-by`).
    pub kind: Kind,
    pub name: String,
    pub entry: Entry,
    /// The fetched content in the store (a directory, or a file), when there is any.
    pub content: Option<PathBuf>,
    /// Taken from the lock as it was: nothing was looked up.
    pub locked: bool,
}

/// What a resolution needs: the project, the harness it installs into, and the remote
/// listings already made (one per repository per run).
pub struct Env<'a> {
    pub project: PathBuf,
    pub ctx: &'a Ctx,
    pub profile: &'a HarnessProfile,
    pub refs: std::sync::Mutex<BTreeMap<String, RemoteRefs>>,
    /// Commits `apm.lock.yaml` already pins, by package identity: taken as given.
    pub apm_locked: BTreeMap<String, crate::apm::ApmLocked>,
    /// Where declared dependencies are installed now: what [`install`] may replace. A
    /// file or skill anywhere else that's in the way is the user's, and isn't overwritten.
    pub owned: BTreeSet<PathBuf>,
}

impl<'a> Env<'a> {
    pub fn new(ctx: &'a Ctx, profile: &'a HarnessProfile) -> Env<'a> {
        // one hlock exported says what harness.lock says: harness.lock is the authority then
        let exported = std::fs::read_to_string(ctx.cwd.join(crate::apm::APM_LOCK)).is_ok_and(|t| t.starts_with(crate::apm::EXPORT_HEADER));
        let apm_locked = if exported { BTreeMap::new() } else { crate::apm::read_project_lock(&ctx.cwd).unwrap_or_default() };
        Env { project: ctx.cwd.clone(), ctx, profile, refs: std::sync::Mutex::new(BTreeMap::new()), apm_locked, owned: BTreeSet::new() }
    }

    /// [`Env::new`], owning the places the lock's declared dependencies are installed.
    pub fn for_lock(ctx: &'a Ctx, profile: &'a HarnessProfile, lock: &Lockfile) -> Env<'a> {
        let mut env = Env::new(ctx, profile);
        for entries in locked_declarations(lock).values() {
            for e in entries {
                if let (Some(p), _) = installed_state(e, &env) {
                    env.owned.insert(p);
                }
            }
        }
        env
    }

    /// For a fresh resolution (`add`, `upgrade dep:`): look upstream, whatever
    /// `apm.lock.yaml` pins.
    pub fn fresh(mut self) -> Self {
        self.apm_locked.clear();
        self
    }

    /// A remote's refs, listed once per run.
    pub fn refs(&self, g: &GitSource) -> Result<RemoteRefs> {
        if let Some(r) = self.refs.lock().ok().and_then(|m| m.get(&g.url).cloned()) {
            return Ok(r);
        }
        let r = ls_remote(g)?;
        if let Ok(mut m) = self.refs.lock() {
            m.insert(g.url.clone(), r.clone());
        }
        Ok(r)
    }

    /// The commit `g` asks for: as is for a full SHA, else from the remote's refs.
    pub fn pick(&self, g: &GitSource) -> Result<Pick> {
        match &g.reference {
            Some(r) if is_full_sha(r) && g.constraint.is_none() => Ok(Pick { commit: r.to_lowercase(), ..Default::default() }),
            _ => pick(g, &self.refs(g)?),
        }
    }
}

/// The lock entry a declaration resolved to before, when its source is unchanged.
fn locked_entry(lock: &Lockfile, d: &Decl) -> Option<Entry> {
    let same = |dep: &Declared, src: Option<&str>| dep.declared && dep.required_by.is_empty() && src == Some(d.spec.as_str());
    match d.kind {
        Kind::Skill => lock.skill.iter().find(|s| s.name == d.name && same(&s.dep, s.source.as_deref())).cloned().map(Entry::Skill),
        Kind::Mcp => lock.tool.iter().find(|t| t.name == d.name && same(&t.dep, t.source.as_deref())).cloned().map(Entry::Tool),
        Kind::Instructions => lock.instructions.iter().find(|i| decl_name_of(i) == d.name && same(&i.dep, i.source.as_deref())).cloned().map(Entry::Instructions),
        Kind::Plugin => lock.native.iter().find(|n| plugin_decl_name(n) == d.name && same(&n.dep, n.source.as_deref())).cloned().map(Entry::Native),
        Kind::Package => None,
    }
}

/// The name an `[[instructions]]` entry is declared under: `declared-as`, else its file's
/// stem.
pub fn decl_name_of(i: &Instructions) -> String {
    if let Some(n) = &i.dep.declared_as {
        return n.clone();
    }
    let file = i.name.rsplit('/').next().unwrap_or(&i.name);
    file.split('.').next().unwrap_or(file).to_string()
}

/// The name a plugin is declared under: `acme-tools` for `acme-tools@acme-marketplace`.
pub fn plugin_decl_name(n: &Native) -> String {
    n.name.split('@').next().unwrap_or(&n.name).to_string()
}

/// Resolve one declaration (a package: everything it brings). With `lock`, a declaration
/// whose locked source is unchanged is taken from it without looking anything up.
pub fn resolve(d: &Decl, env: &Env, lock: Option<&Lockfile>) -> Result<(Vec<Resolved>, Option<Vec<Package>>)> {
    if d.kind == Kind::Package {
        if let Some(l) = lock {
            if let Some(found) = crate::apm::locked_package(l, d, env) {
                return Ok(found);
            }
        }
        return crate::apm::resolve_package(d, env, lock).map(|(r, p)| (r, Some(p)));
    }
    if let Some(entry) = lock.and_then(|l| locked_entry(l, d)) {
        let content = entry.digest().and_then(|dg| stored(&env.project, dg));
        return Ok((vec![Resolved { kind: d.kind, name: d.name.clone(), entry, content, locked: true }], None));
    }
    Ok((vec![resolve_fresh(d, env)?], None))
}

fn declared_dep(d: &Decl) -> Declared {
    Declared { declared: true, constraint: d.source.as_ref().and_then(|s| s.constraint().map(str::to_string)), ..Default::default() }
}

/// Resolve a declaration from its source, looking it up upstream.
pub fn resolve_fresh(d: &Decl, env: &Env) -> Result<Resolved> {
    let src = d.source()?;
    let mut dep = declared_dep(d);
    let label = src.label();
    let at = |e: Error| match e {
        Error::Dependency(m) => err(format!("{} {}: {m}", d.kind, d.name)),
        e => e,
    };
    let (entry, content) = match (&src, d.kind) {
        (Source::Git(g), Kind::Skill | Kind::Instructions) => {
            // a skill's `…/sql/SKILL.md` is its directory
            let mut g = g.clone();
            if d.kind == Kind::Skill && g.subpath.as_deref().is_some_and(|p| p.rsplit('/').next().is_some_and(|f| f.eq_ignore_ascii_case("SKILL.md"))) {
                g.subpath = g.subpath.as_deref().and_then(|p| p.rsplit_once('/')).map(|(a, _)| a.to_string());
            }
            let p = env.pick(&g).map_err(at)?;
            let (digest, content) = fetch_git(&env.project, &g, &p.commit, p.resolved_ref.as_deref(), None).map_err(at)?;
            dep.commit = Some(p.commit);
            dep.resolved_ref = p.resolved_ref;
            dep.resolved_tag = p.resolved_tag;
            (content_entry(d, &content, digest, dep, "third-party")?, Some(content))
        }
        (Source::Path { path, .. }, Kind::Skill | Kind::Instructions) => {
            let abs = env.ctx.expand(path, None);
            let digest = content_digest(&abs).map_err(|e| err(format!("{} {}: {path}: {e}", d.kind, d.name)))?;
            let inside = crate::fsutil::within(&env.project, &abs);
            // a path outside the project is copied into the store, then vendored when placed
            let content = if inside { abs.clone() } else { store(&env.project, &abs)?.1 };
            (content_entry(d, &content, digest, dep, if inside { "first-party" } else { "local" })?, Some(content))
        }
        (Source::Npm { package, constraint }, Kind::Mcp) => {
            let (version, integrity) = npm_resolve(package, constraint.as_deref()).map_err(at)?;
            let entry = json!({"command": "npx", "args": ["-y", format!("{package}@{version}")]});
            (Entry::Tool(server_tool(d, &entry, Some(version), integrity, dep)?), None)
        }
        (Source::Pypi { package, constraint }, Kind::Mcp) => {
            let (version, integrity) = pypi_resolve(package, constraint.as_deref()).map_err(at)?;
            let entry = json!({"command": "uvx", "args": [format!("{package}=={version}")]});
            (Entry::Tool(server_tool(d, &entry, Some(version), integrity, dep)?), None)
        }
        (Source::Remote { url }, Kind::Mcp) => {
            let entry = json!({"type": "http", "url": url});
            (Entry::Tool(server_tool(d, &entry, None, None, dep)?), None)
        }
        (Source::Defined { entry, .. }, Kind::Mcp) => {
            let t = match entry {
                Some(e) => server_tool(d, e, None, None, dep)?,
                // a registry reference: recorded; it's configured by hand
                None => Tool { name: d.name.clone(), kind: "mcp".into(), source: Some(d.spec.clone()), scope: Some(d.scope.clone()), dep, ..Default::default() },
            };
            (Entry::Tool(t), None)
        }
        (Source::Plugin { marketplace, name }, Kind::Plugin) => {
            // the harness installs it; its version and commit are read back once it has
            let n = Native { kind: format!("{}.plugin", env.profile.id), name: format!("{name}@{marketplace}"), source: Some(d.spec.clone()), enabled: true, dep, ..Default::default() };
            (Entry::Native(n), None)
        }
        (_, kind) => {
            let what = match kind {
                Kind::Mcp => "an MCP server comes from npm:, pypi:, or a URL",
                Kind::Plugin => "a plugin comes from plugin:MARKETPLACE/NAME",
                Kind::Skill => "a skill comes from git (github:, git+…, apm:) or a path",
                Kind::Instructions => "an instruction file comes from git (github:, git+…) or a path",
                Kind::Package => "a package comes from apm:",
            };
            return Err(err(format!("{kind} {}: {label} can't be one: {what}", d.name)));
        }
    };
    Ok(Resolved { kind: d.kind, name: d.name.clone(), entry, content, locked: false })
}

/// The skill or instructions entry for fetched content.
fn content_entry(d: &Decl, content: &Path, digest: String, dep: Declared, origin: &str) -> Result<Entry> {
    let is_dir = content.is_dir();
    match d.kind {
        Kind::Skill => {
            if !is_dir || !content.join("SKILL.md").is_file() {
                return Err(err(format!("skill {}: {} has no SKILL.md", d.name, d.spec)));
            }
            Ok(Entry::Skill(Skill { name: d.name.clone(), origin: origin.into(), source: Some(d.spec.clone()), digest: Some(digest), scripts: Some(crate::capture::skill_scripts(content)), enabled: true, editable: d.editable, scope: Some(d.scope.clone()), dep, ..Default::default() }))
        }
        _ => {
            if is_dir {
                return Err(err(format!("instructions {}: {} is a directory; name the file", d.name, d.spec)));
            }
            let dep = Declared { declared_as: Some(d.name.clone()), ..dep };
            Ok(Entry::Instructions(Instructions { name: d.name.clone(), digest: Some(digest), scope: d.scope.clone(), source: Some(d.spec.clone()), dep, ..Default::default() }))
        }
    }
}

/// An MCP server entry as `pin` would record it, from the config entry hlock writes.
fn server_tool(d: &Decl, entry: &Value, version: Option<String>, integrity: Option<String>, dep: Declared) -> Result<Tool> {
    let mut t = crate::capture::mcp_tool(&d.name, entry, &d.scope).ok_or_else(|| err(format!("mcp {}: can't make a server entry", d.name)))?;
    t.source = Some(d.spec.clone());
    t.version = version;
    t.integrity = integrity;
    t.dep = dep;
    Ok(t)
}

// --------------------------------------------------------------------------- //
// Placement: where a dependency goes in a harness
// --------------------------------------------------------------------------- //

fn is_project_template(t: &str) -> bool {
    !t.starts_with("{root}") && !t.starts_with('~') && !t.starts_with("{home}") && !Path::new(t).is_absolute()
}

/// The harness's project skill directory (`.claude/skills`), if it reads one.
pub fn project_skills_dir(profile: &HarnessProfile) -> Option<&str> {
    profile.skills.dirs.iter().map(String::as_str).find(|d| is_project_template(d))
}

/// Where a skill named `name` goes: the project's skill directory, or the user's.
pub fn skill_dest(profile: &HarnessProfile, scope: &str, name: &str, ctx: &Ctx) -> Result<(PathBuf, String)> {
    let root = profile.config_root(ctx);
    if scope == "project" {
        if let Some(dir) = project_skills_dir(profile) {
            return Ok((ctx.expand(dir, Some(&root)).join(name), format!("{}/{name}", dir.trim_end_matches('/'))));
        }
        return Err(err(format!("{} reads no project skill directory; add it with --scope user", profile.name)));
    }
    let w = profile.skills.write.as_deref().or_else(|| profile.skills.dirs.iter().map(String::as_str).find(|d| !is_project_template(d))).ok_or_else(|| err(format!("{} has no user skill directory", profile.name)))?;
    let p = ctx.expand(w, Some(&root)).join(name);
    Ok((p.clone(), crate::capture::portable_path(&p, Some(&root), ctx)))
}

/// Where an instruction file named `name` goes: the harness's rules directory, else its
/// own project instruction file (`AGENTS.md`).
pub fn instructions_dest(profile: &HarnessProfile, scope: &str, name: &str, ctx: &Ctx) -> Result<(PathBuf, String)> {
    let root = profile.config_root(ctx);
    if scope == "user" {
        let tpl = profile.instructions.iter().find(|t| t.starts_with("{root}/")).ok_or_else(|| err(format!("{} reads no user instruction file", profile.name)))?;
        let p = ctx.expand(tpl, Some(&root));
        return Ok((p.clone(), crate::capture::portable_path(&p, Some(&root), ctx)));
    }
    if let Some(r) = &profile.rules {
        let rel = format!("{}/{name}{}", r.dir.trim_end_matches('/'), r.ext);
        return Ok((ctx.cwd.join(&rel), rel));
    }
    let tpl = profile.instructions.iter().find(|t| is_project_template(t)).ok_or_else(|| err(format!("{} reads no project instruction file", profile.name)))?;
    Ok((ctx.cwd.join(tpl), tpl.clone()))
}

/// Where a harness's MCP servers go for a scope: the file, its format, and the pointer
/// to the servers table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpDest {
    pub path: PathBuf,
    pub format: String,
    pub pointer: String,
    /// The profile's file id (`project-mcp`, `state`).
    pub file: String,
    pub shape: String,
}

pub fn mcp_dest(profile: &HarnessProfile, scope: &str, ctx: &Ctx) -> Result<McpDest> {
    let root = profile.config_root(ctx);
    let source = if scope == "project" {
        profile.mcp.sources.iter().find(|s| crate::capture::source_is_project(profile, s, &root, ctx))
    } else {
        profile.mcp.write.as_ref()
    };
    let Some(src) = source else {
        return Err(err(format!("{} has no {scope} MCP config hlock can write; try --scope {}", profile.name, if scope == "project" { "user" } else { "project" })));
    };
    let (file, pointer) = src.split_once('#').ok_or_else(|| err(format!("{}'s MCP source {src:?} names no file", profile.name)))?;
    let path = profile.file_path(file, ctx).ok_or_else(|| err(format!("{} has no file {file:?}", profile.name)))?;
    let format = profile.files.get(file).map(|f| f.format.clone()).unwrap_or_else(|| "json".into());
    Ok(McpDest { path, format, pointer: pointer.to_string(), file: file.to_string(), shape: profile.mcp.shape.clone().unwrap_or_else(|| "standard".into()) })
}

/// Set (or with `None`, remove) one MCP server in a config file, keeping the rest. A file
/// that may hold secrets keeps its mode; a new one is the owner's only.
pub fn write_mcp(dest: &McpDest, name: &str, entry: Option<&Value>) -> Result<bool> {
    let fmt = if dest.format == "jsonc" { "json" } else { dest.format.as_str() };
    let mut doc = crate::capture::read_config(&dest.path, &dest.format)?.unwrap_or_else(|| json!({}));
    let ptr = format!("{}/{}", dest.pointer, crate::capture::escape_pointer(name));
    let before = doc.pointer(&ptr).cloned();
    match entry {
        Some(e) => crate::sync::set_pointer(&mut doc, &ptr, e.clone()),
        None => {
            let parent = doc.pointer_mut(&dest.pointer).and_then(Value::as_object_mut);
            match parent {
                Some(m) => {
                    m.remove(name);
                }
                None => return Ok(false),
            }
        }
    }
    if before.as_ref() == entry {
        return Ok(false);
    }
    let text = crate::capture::render_config(&doc, fmt).map_err(err)?;
    if dest.path.exists() {
        crate::fsutil::write_atomic(&dest.path, text)?;
    } else {
        crate::fsutil::write_private(&dest.path, text)?;
    }
    Ok(true)
}

/// The config entry a locked server is written as, in the harness's shape.
pub fn mcp_entry(t: &Tool, shape: &str) -> Value {
    crate::launch::mcp_entry(t, shape)
}

/// What [`install`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Installed {
    pub messages: Vec<String>,
    /// Profile file ids whose content changed (their `[[config]]` entries need refreshing).
    pub files: Vec<String>,
    pub warnings: Vec<String>,
}

/// Move `path` out of the way, into `<project>/.hlock/bak/` (keeping every earlier copy),
/// so a set-aside skill isn't read as a skill. Returns where it went.
pub fn set_aside(path: &Path, project: &Path) -> Result<PathBuf> {
    let rel = match path.strip_prefix(project) {
        Ok(r) => r.to_path_buf(),
        Err(_) => PathBuf::from("outside").join(path.components().filter(|c| matches!(c, std::path::Component::Normal(_))).collect::<PathBuf>()),
    };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let mut dest = project.join(".hlock").join("bak").join(format!("{}.{stamp}", rel.display()));
    let mut n = 1;
    while std::fs::symlink_metadata(&dest).is_ok() {
        dest = project.join(".hlock").join("bak").join(format!("{}.{stamp}-{n}", rel.display()));
        n += 1;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| crate::error::io_at(parent, e))?;
    }
    if std::fs::rename(path, &dest).is_err() {
        // another filesystem: copy, then remove
        crate::fsutil::copy_tree(path, &dest)?;
        if path.is_dir() {
            std::fs::remove_dir_all(path).map_err(|e| crate::error::io_at(path, e))?;
        } else {
            std::fs::remove_file(path).map_err(|e| crate::error::io_at(path, e))?;
        }
    }
    Ok(dest)
}

/// Copy `content` to `dest` (a directory or a file). What's there already is replaced only
/// when it's the same, or a declared dependency owns the place (it's set aside first);
/// anything else is the user's, and the copy stops. Returns whether anything changed.
fn place_content(content: &Path, dest: &Path, digest: Option<&str>, env: &Env, what: &str) -> Result<bool> {
    if fsutil_same(content, dest) {
        return Ok(false);
    }
    if digest.is_some() && content_digest(dest).ok().as_deref() == digest {
        return Ok(false);
    }
    if std::fs::symlink_metadata(dest).is_ok() {
        if !env.owned.contains(dest) {
            return Err(err(format!("{what}: {} already exists and hlock didn't put it there; move it away, or add this under another name with --as", crate::sync::pretty(dest, env.ctx))));
        }
        set_aside(dest, &env.project)?;
    }
    if content.is_dir() {
        crate::fsutil::copy_tree(content, dest)?;
    } else {
        let bytes = std::fs::read(content).map_err(|e| crate::error::io_at(content, e))?;
        crate::fsutil::write_atomic(dest, bytes)?;
    }
    Ok(true)
}

fn fsutil_same(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Put a resolved dependency in place in the harness, and fill in where it went
/// (`path` for a skill, `name` for an instruction file, version and commit for a plugin).
pub fn install(r: &mut Resolved, env: &Env) -> Result<Installed> {
    let mut out = Installed::default();
    let profile = env.profile;
    let ctx = env.ctx;
    match &mut r.entry {
        Entry::Skill(s) => {
            let scope = s.scope.clone().unwrap_or_else(|| "project".into());
            let (dest, portable) = skill_dest(profile, &scope, &s.name, ctx)?;
            let content = match &r.content {
                Some(c) => c.clone(),
                None => fetch_entry(&Entry::Skill(s.clone()), env)?,
            };
            if s.origin == "local" {
                // from outside the project: vendored next to the lock, so it travels with it
                let rel = format!("{}/{}/skills/{}", crate::capture::VENDOR_DIR, profile.id, s.name);
                let vdest = env.project.join(&rel);
                if std::fs::symlink_metadata(&vdest).is_ok() {
                    set_aside(&vdest, &env.project)?;
                }
                crate::fsutil::copy_tree(&content, &vdest)?;
                s.vendored = Some(rel);
            }
            if place_content(&content, &dest, s.digest.as_deref(), env, &format!("skill {}", s.name))? {
                out.messages.push(format!("copied skill {} → {portable}", s.name));
            }
            s.path = Some(portable);
        }
        Entry::Instructions(i) => {
            let decl = r.name.clone();
            let (dest, portable) = instructions_dest(profile, &i.scope, &decl, ctx)?;
            let content = match &r.content {
                Some(c) => c.clone(),
                None => fetch_entry(&Entry::Instructions(i.clone()), env)?,
            };
            if dest.is_file() && !env.owned.contains(&dest) && content_digest(&dest).ok() != i.digest {
                let why = if profile.rules.is_none() || i.scope == "user" { format!("{} reads instructions from {portable}, which you already have", profile.name) } else { format!("{portable} already exists") };
                return Err(err(format!("instructions {decl}: {why}, and hlock won't replace a file it didn't put there; put the file's text in it yourself, or add this under another name with --as")));
            }
            if place_content(&content, &dest, i.digest.as_deref(), env, &format!("instructions {decl}"))? {
                out.messages.push(format!("wrote {portable}"));
            }
            i.name = portable;
        }
        Entry::Tool(t) => {
            let scope = t.scope.clone().unwrap_or_else(|| "project".into());
            if let Some(why) = crate::sync::transport_problem(t, profile) {
                return Err(err(format!("mcp {}: {why}", t.name)));
            }
            if profile.app.as_ref().is_some_and(|a| a.quit_to_write) && crate::sync::app_running(profile, ctx) {
                return Err(err(format!("{} is running and may overwrite its config; quit it, then add the server again", profile.name)));
            }
            if let Some(pkg) = t.dep.required_by.last() {
                // it starts a process, from a file the project didn't write: not until it's trusted here
                let trusted = crate::trust::executables(&Lockfile { tool: vec![t.clone()], ..Default::default() }, &env.project, None, ctx).into_iter().next().is_some_and(|e| crate::trust::TrustStore::load(ctx).ok().and_then(|s| s.status(&e.digest)) == Some("trusted"));
                if !trusted {
                    out.warnings.push(format!("mcp {}: {pkg} brings this MCP server; it isn't configured until you trust it (hlock trust, then hlock sync)", t.name));
                    return Ok(out);
                }
            }
            let dest = mcp_dest(profile, &scope, ctx)?;
            if t.command.is_none() && t.url.is_none() {
                out.warnings.push(format!("mcp {}: {} names a server hlock can't configure by itself; add it to {} by hand", t.name, t.source.as_deref().unwrap_or(""), crate::sync::pretty(&dest.path, ctx)));
                return Ok(out);
            }
            if write_mcp(&dest, &t.name, Some(&mcp_entry(t, &dest.shape)))? {
                out.messages.push(format!("configured MCP server {} in {}", t.name, crate::sync::pretty(&dest.path, ctx)));
                out.files.push(dest.file.clone());
            }
        }
        Entry::Native(n) => {
            let msg = install_plugin(n, profile, ctx)?;
            out.messages.push(msg);
            out.files.push("settings".into());
        }
    }
    Ok(out)
}

/// Install a plugin through the harness's own CLI, then read back what it installed.
fn install_plugin(n: &mut Native, profile: &HarnessProfile, ctx: &Ctx) -> Result<String> {
    if profile.id != "claude-code" {
        return Err(err(format!("hlock installs plugins for Claude Code only; {} isn't supported yet", profile.name)));
    }
    let bin = profile.binaries.first().cloned().unwrap_or_else(|| "claude".into());
    let exe = ctx.which(&bin).ok_or_else(|| err(format!("installing plugin {} needs `{bin}` on PATH", n.name)))?;
    let installed = || {
        let root = profile.config_root(ctx);
        let v = crate::capture::read_config(&root.join("plugins/installed_plugins.json"), "json").ok().flatten().unwrap_or_default();
        v["plugins"][&n.name][0].clone()
    };
    if installed().is_null() {
        let mut cmd = Command::new(&exe);
        cmd.args(["plugin", "install", &n.name]).current_dir(&ctx.cwd).env_clear().envs(&ctx.env).stdin(Stdio::null());
        let out = crate::process::run(cmd, &crate::process::Limits::timeout(Duration::from_secs(600))).map_err(|e| err(format!("can't run {bin}: {e}")))?;
        if !out.success {
            return Err(err(format!("`{bin} plugin install {}` failed: {}", n.name, out.text().trim().lines().last().unwrap_or(""))));
        }
    }
    let e = installed();
    n.version = e["version"].as_str().map(str::to_string);
    n.revision = e["gitCommitSha"].as_str().map(str::to_string);
    n.dep.commit = n.revision.clone().filter(|c| plain_token(c));
    Ok(format!("installed plugin {}{}", n.name, n.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default()))
}

/// Fetch a locked entry's content into the store (it has a commit, or a path in the project).
pub fn fetch_entry(e: &Entry, env: &Env) -> Result<PathBuf> {
    fetch_locked(e, &env.project, env.ctx)
}

/// [`fetch_entry`] for a project directory and context.
pub fn fetch_locked(e: &Entry, project: &Path, ctx: &Ctx) -> Result<PathBuf> {
    let digest = e.digest();
    if let Some(p) = digest.and_then(|d| stored(project, d)) {
        return Ok(p);
    }
    let spec = e.source().ok_or_else(|| err(format!("{} {} has no source to fetch", e.kind(), e.name())))?;
    let kind = if e.kind() == Kind::Instructions { Kind::Instructions } else { Kind::Skill };
    match parse(spec, kind)? {
        Source::Git(mut g) => {
            let commit = e.dep().commit.clone().ok_or_else(|| err(format!("{} {} has no commit in the lock", e.kind(), e.name())))?;
            if kind == Kind::Skill && g.subpath.as_deref().is_some_and(|p| p.rsplit('/').next().is_some_and(|f| f.eq_ignore_ascii_case("SKILL.md"))) {
                g.subpath = g.subpath.as_deref().and_then(|p| p.rsplit_once('/')).map(|(a, _)| a.to_string());
            }
            Ok(fetch_git(project, &g, &commit, e.dep().resolved_ref.as_deref(), digest)?.1)
        }
        Source::Path { path, .. } => {
            let abs = ctx.expand(&path, None);
            if let (Some(want), Ok(have)) = (digest, content_digest(&abs)) {
                if want != have {
                    return Err(err(format!("{} {}: {path} has changed since it was locked", e.kind(), e.name())));
                }
            }
            Ok(abs)
        }
        other => Err(err(format!("{} {}: {} has no content to fetch", e.kind(), e.name(), other.label()))),
    }
}

/// Where an installed entry is, and its digest there now (`None` when it's missing).
pub fn installed_state(e: &Entry, env: &Env) -> (Option<PathBuf>, Option<String>) {
    let ctx = env.ctx;
    match e {
        Entry::Skill(s) => {
            let root = env.profile.config_root(ctx);
            let p = s.path.as_deref().map(|p| ctx.expand(p, Some(&root)));
            let d = p.as_ref().filter(|p| p.join("SKILL.md").is_file()).map(|p| crate::capture::tree_digest(p));
            (p, d)
        }
        Entry::Instructions(i) => {
            let root = env.profile.config_root(ctx);
            let p = ctx.expand(&i.name, Some(&root));
            let d = std::fs::read(&p).ok().map(|b| crate::hash::sha256_bytes(&b));
            (Some(p), d)
        }
        Entry::Tool(t) => {
            let scope = t.scope.as_deref().unwrap_or("project");
            match mcp_dest(env.profile, scope, ctx) {
                Ok(dest) => {
                    let doc = crate::capture::read_config(&dest.path, &dest.format).ok().flatten();
                    let have = doc.as_ref().and_then(|d| d.pointer(&format!("{}/{}", dest.pointer, crate::capture::escape_pointer(&t.name)))).cloned();
                    let want = mcp_entry(t, &dest.shape);
                    let sig = |v: &Value| crate::hash::content_hash(v);
                    // a server configured with other values (a secret filled in) still counts as there
                    let d = have.map(|h| if crate::capture::mcp_tool(&t.name, &h, scope).is_some_and(|x| x.command == t.command && x.args == t.args && x.url == t.url) { sig(&want) } else { sig(&h) });
                    (Some(dest.path), d.map(|d| if d == sig(&want) { e.digest().map(str::to_string).unwrap_or(d) } else { d }))
                }
                Err(_) => (None, None),
            }
        }
        Entry::Native(n) => {
            let root = env.profile.config_root(ctx);
            let v = crate::capture::read_config(&root.join("plugins/installed_plugins.json"), "json").ok().flatten().unwrap_or_default();
            let have = v["plugins"][&n.name][0]["version"].as_str().map(str::to_string);
            (None, have.filter(|h| n.version.as_deref().is_none_or(|w| w == h)).map(|_| n.integrity.clone().unwrap_or_default()))
        }
    }
}

// --------------------------------------------------------------------------- //
// The lock
// --------------------------------------------------------------------------- //

/// Whether a lock entry belongs to the declaration `kind`/`name`: declared directly, or
/// brought by that package.
fn belongs(dep: &Declared, entry_kind: Kind, entry_decl: &str, kind: Kind, name: &str) -> bool {
    if kind == Kind::Package {
        return dep.required_by.first().map(String::as_str) == Some(name);
    }
    dep.required_by.is_empty() && entry_kind == kind && entry_decl == name
}

/// Take a declaration's entries (and, for a package, its packages) out of the lock.
pub fn forget(lock: &mut Lockfile, kind: Kind, name: &str) -> usize {
    let before = lock.skill.len() + lock.tool.len() + lock.instructions.len() + lock.native.len() + lock.package.len();
    lock.skill.retain(|s| !(s.dep.declared && belongs(&s.dep, Kind::Skill, &s.name, kind, name)));
    lock.tool.retain(|t| !(t.dep.declared && belongs(&t.dep, Kind::Mcp, &t.name, kind, name)));
    lock.instructions.retain(|i| !(i.dep.declared && belongs(&i.dep, Kind::Instructions, &decl_name_of(i), kind, name)));
    lock.native.retain(|n| !(n.dep.declared && belongs(&n.dep, Kind::Plugin, &plugin_decl_name(n), kind, name)));
    if kind == Kind::Package {
        lock.package.retain(|p| p.name != name && p.dep.required_by.first().map(String::as_str) != Some(name));
    }
    before - (lock.skill.len() + lock.tool.len() + lock.instructions.len() + lock.native.len() + lock.package.len())
}

/// Put resolved entries in the lock in place of what the declaration had, and in place of
/// a pinned entry of the same name (which the declaration now owns).
pub fn apply(lock: &mut Lockfile, kind: Kind, name: &str, resolved: &[Resolved], packages: Option<&[Package]>) {
    // what --eval measured stays with the entry, so `check` can say it's stale
    let evidence: BTreeMap<(Kind, String), crate::lockfile::DepEvidence> = locked_declarations(lock)
        .into_values()
        .flatten()
        .filter_map(|e| e.dep().evidence.clone().map(|ev| ((e.kind(), e.name().to_string()), ev)))
        .collect();
    let resolved: Vec<Resolved> = resolved
        .iter()
        .cloned()
        .map(|mut r| {
            let key = (r.entry.kind(), r.entry.name().to_string());
            if r.entry.dep().evidence.is_none() {
                r.entry.dep_mut().evidence = evidence.get(&key).cloned();
            }
            r
        })
        .collect();
    let resolved = &resolved[..];
    if kind == Kind::Package {
        // packages resolve as one graph: what's applied replaces every package's entries
        let roots: Vec<String> = lock.package.iter().filter(|p| p.depth == 1).map(|p| p.name.clone()).collect();
        for r in roots {
            forget(lock, Kind::Package, &r);
        }
        lock.package.clear();
    }
    forget(lock, kind, name);
    for r in resolved {
        match &r.entry {
            Entry::Skill(s) => {
                lock.skill.retain(|x| !(x.name == s.name && x.scope == s.scope));
                lock.skill.push(s.clone());
            }
            Entry::Tool(t) => {
                lock.tool.retain(|x| x.name != t.name);
                lock.tool.push(t.clone());
            }
            Entry::Instructions(i) => {
                lock.instructions.retain(|x| x.name != i.name);
                lock.instructions.push(i.clone());
            }
            Entry::Native(n) => {
                lock.native.retain(|x| !(x.name == n.name && x.kind == n.kind));
                lock.native.push(n.clone());
            }
        }
    }
    if let Some(ps) = packages {
        lock.package.extend(ps.iter().cloned());
        lock.provenance.openapm = Some(crate::apm::OPENAPM_VERSION.into());
    }
}

/// Every declared entry in the lock, by the declaration it belongs to.
pub fn locked_declarations(lock: &Lockfile) -> BTreeMap<(Kind, String), Vec<Entry>> {
    let mut out: BTreeMap<(Kind, String), Vec<Entry>> = BTreeMap::new();
    let key = |dep: &Declared, k: Kind, n: String| match dep.required_by.first() {
        Some(p) => (Kind::Package, p.clone()),
        None => (k, n),
    };
    for s in lock.skill.iter().filter(|s| s.dep.declared) {
        out.entry(key(&s.dep, Kind::Skill, s.name.clone())).or_default().push(Entry::Skill(s.clone()));
    }
    for t in lock.tool.iter().filter(|t| t.dep.declared) {
        out.entry(key(&t.dep, Kind::Mcp, t.name.clone())).or_default().push(Entry::Tool(t.clone()));
    }
    for i in lock.instructions.iter().filter(|i| i.dep.declared) {
        out.entry(key(&i.dep, Kind::Instructions, decl_name_of(i))).or_default().push(Entry::Instructions(i.clone()));
    }
    for n in lock.native.iter().filter(|n| n.dep.declared) {
        out.entry(key(&n.dep, Kind::Plugin, plugin_decl_name(n))).or_default().push(Entry::Native(n.clone()));
    }
    for p in lock.package.iter().filter(|p| p.depth == 1) {
        out.entry((Kind::Package, p.name.clone())).or_default();
    }
    out
}

/// Re-capture the config files `ids` and put them in the lock (their digests, and the
/// vendored copies `sync` restores from), so a change `hlock add` made isn't drift.
fn refresh_config(lock: &mut Lockfile, lock_path: &Path, profile: &HarnessProfile, reg: &crate::profiles::Registry, ctx: &Ctx, ids: &BTreeSet<String>) -> Result<()> {
    if ids.is_empty() || !lock.is_pin() {
        return Ok(());
    }
    let project = lock.stack.get("name").and_then(Value::as_str).map(str::to_string);
    let cap = crate::capture::capture(profile, reg, ctx, project.as_deref(), &crate::capture::UserScope::of(lock))?;
    let ns = crate::capture::vendor_namespace(lock_path);
    let relocate = |rel: &str| match &ns {
        Some(ns) => rel.replacen(&format!("{}/{}/", crate::capture::VENDOR_DIR, profile.id), &format!("{}/{ns}/{}/", crate::capture::VENDOR_DIR, profile.id), 1),
        None => rel.to_string(),
    };
    let dir = crate::sync::lock_dir_of(lock_path, ctx);
    for id in ids {
        let Some(mut c) = cap.lock.config.iter().find(|c| &c.id == id).cloned() else { continue };
        if let Some(rel) = c.vendored.clone() {
            if let Some(f) = cap.vendor.iter().find(|f| f.rel == rel) {
                let rel = relocate(&rel);
                crate::fsutil::write_atomic(&dir.join(&rel), &f.bytes)?;
                c.vendored = Some(rel);
            }
        }
        match lock.config.iter_mut().find(|x| &x.id == id) {
            Some(x) => *x = c,
            None => lock.config.push(c),
        }
    }
    for s in cap.lock.secret {
        if !lock.secret.iter().any(|x| x.name == s.name) {
            lock.secret.push(s);
        }
    }
    Ok(())
}

/// Re-record the project config files `ids` in the lock at `lock_path` after `sync` wrote
/// declared MCP servers into them, so the change isn't drift.
pub fn record_configs(lock_path: &Path, reg: &crate::profiles::Registry, ctx: &Ctx, ids: &BTreeSet<String>) -> Result<bool> {
    let mut lock = Lockfile::load(lock_path)?;
    let profile = reg.require_harness(&lock.harness.name)?;
    // only files the project owns: a user-scope file in an isolated root isn't the one pinned
    let ids: BTreeSet<String> = ids.iter().filter(|id| lock.config.iter().any(|c| &c.id == *id && c.restore == "record") || profile.file_path(id, ctx).is_some_and(|p| p.starts_with(&ctx.cwd))).cloned().collect();
    if ids.is_empty() || !lock.is_pin() {
        return Ok(false);
    }
    let before = lock.config.clone();
    refresh_config(&mut lock, lock_path, profile, reg, ctx, &ids)?;
    if lock.config == before {
        return Ok(false);
    }
    lock.save(lock_path)?;
    Ok(true)
}

// --------------------------------------------------------------------------- //
// harness.toml
// --------------------------------------------------------------------------- //

fn read_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.parse::<toml_edit::DocumentMut>().map_err(|e| Error::Manifest(format!("invalid {}: {e}", path.display())))
}

/// A new manifest for a project: its name, the harness, and nothing else yet.
fn new_manifest_text(name: &str, harness: &str) -> String {
    format!(
        "# The project's agent setup. `hlock add` declares dependencies here; harness.lock pins them.\n[project]\nname = {}\n\n[target]\nharness = {}\n",
        toml_edit::Value::from(name),
        toml_edit::Value::from(harness)
    )
}

/// Declare `name` in `harness.toml`, keeping the file's comments and layout. A project
/// scope, non-editable dependency is a plain string; otherwise an inline table.
pub fn manifest_set(path: &Path, d: &Decl) -> Result<()> {
    let mut doc = read_doc(path)?;
    // a server declared the v0 way stays there
    if d.kind == Kind::Mcp && doc.get("tools").and_then(|t| t.get(&d.name)).and_then(|e| e.get("mcp")).is_some() {
        doc["tools"][&d.name]["mcp"] = toml_edit::value(d.spec.clone());
    } else {
        let section = d.kind.section();
        if doc.get(section).is_none() {
            let mut t = toml_edit::Table::new();
            t.set_implicit(false);
            doc[section] = toml_edit::Item::Table(t);
        }
        let value: toml_edit::Value = if d.scope == "project" && !d.editable {
            d.spec.clone().into()
        } else {
            let mut t = toml_edit::InlineTable::new();
            t.insert("source", d.spec.clone().into());
            if d.scope != "project" {
                t.insert("scope", d.scope.clone().into());
            }
            if d.editable {
                t.insert("editable", true.into());
            }
            toml_edit::Value::InlineTable(t)
        };
        doc[section][&d.name] = toml_edit::Item::Value(value);
    }
    crate::fsutil::write_atomic(path, doc.to_string()).map_err(|e| crate::error::at(path, e))
}

/// Remove `name` from its section of `harness.toml`. Returns whether it was there.
pub fn manifest_remove(path: &Path, kind: Kind, name: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut doc = read_doc(path)?;
    let mut removed = doc.get_mut(kind.section()).and_then(|t| t.as_table_like_mut()).and_then(|t| t.remove(name)).is_some();
    if !removed && kind == Kind::Mcp {
        let in_tools = doc.get("tools").and_then(|t| t.get(name)).and_then(|e| e.get("mcp")).is_some();
        if in_tools {
            removed = doc.get_mut("tools").and_then(|t| t.as_table_like_mut()).and_then(|t| t.remove(name)).is_some();
        }
    }
    if removed {
        crate::fsutil::write_atomic(path, doc.to_string()).map_err(|e| crate::error::at(path, e))?;
    }
    Ok(removed)
}

// --------------------------------------------------------------------------- //
// hlock add
// --------------------------------------------------------------------------- //

/// What `hlock add` was asked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AddRequest {
    /// `skill`, `mcp`, `instructions`, `plugin`; none for an `apm:` source.
    pub kind: Option<String>,
    pub source: String,
    /// `--as`: the name to declare it under.
    pub name: Option<String>,
    /// `project` (the default) or `user`.
    pub scope: Option<String>,
    /// The harness to install into when there's no lock yet.
    pub harness: Option<String>,
    /// Files whose hidden characters are let through (`--allow-hidden FILE`).
    pub allow_hidden: Vec<String>,
}

/// A dependency resolved and fetched into the store, nothing else changed yet: what
/// `--dry-run` prints, and what `--eval` measures before [`commit`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prepared {
    pub decl: Decl,
    pub harness: String,
    pub resolved: Vec<Resolved>,
    pub packages: Option<Vec<Package>>,
    pub manifest: PathBuf,
    pub lock: PathBuf,
    /// The source the manifest declares under that name now, if any.
    pub replaces: Option<String>,
    pub warnings: Vec<String>,
}

/// The harness a project's dependencies install into: the lock's, else `[target]`,
/// else the one `hlock pin` would pick.
fn project_harness(lock_path: &Path, manifest: Option<&crate::manifest::Manifest>, explicit: Option<&str>, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<String> {
    if let Ok(l) = Lockfile::load(lock_path) {
        if !l.harness.name.is_empty() {
            return Ok(reg.require_harness(&l.harness.name)?.id.clone());
        }
    }
    let target = manifest.and_then(|m| m.target_harness());
    Ok(reg.select(explicit, target.as_deref(), ctx)?.id)
}

/// Resolve and fetch what `req` asks for. Nothing in the project changes.
pub fn prepare(req: &AddRequest, manifest_path: &Path, lock_path: &Path, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<Prepared> {
    let source_text = req.source.trim();
    let kind = match req.kind.as_deref() {
        Some(k) => Kind::parse(k)?,
        None if source_text.starts_with("apm:") => Kind::Package,
        None => return Err(err(format!("say what {source_text:?} is: hlock add skill|mcp|instructions|plugin SOURCE"))),
    };
    let source = parse(source_text, kind)?;
    let name = match &req.name {
        Some(n) => n.clone(),
        None => default_name(&source),
    };
    if !crate::fsutil::plain_name(&name) || name.starts_with('-') {
        return Err(err(format!("{name:?} can't name a dependency: use letters, digits, '.', '_' and '-'")));
    }
    // tables of their own in harness.toml (v0's [skills.builtin], [skills.trust], [instructions] files)
    if matches!((kind, name.as_str()), (Kind::Skill, "builtin" | "trust") | (Kind::Instructions, "files")) {
        return Err(err(format!("{name:?} is reserved in [{}]; pick another name with --as", kind.section())));
    }
    let scope = req.scope.clone().unwrap_or_else(|| "project".into());
    if !matches!(scope.as_str(), "project" | "user") {
        return Err(err(format!("scope {scope:?} isn't project or user")));
    }
    let manifest = if manifest_path.is_file() { Some(crate::manifest::Manifest::load(manifest_path)?) } else { None };
    let replaces = match &manifest {
        Some(m) => declared(m)?.into_iter().find(|d| d.kind == kind && d.name == name).map(|d| d.spec),
        None => None,
    };
    let harness = project_harness(lock_path, manifest.as_ref(), req.harness.as_deref(), reg, ctx)?;
    let profile = reg.require_harness(&harness)?;
    let editable = matches!(source, Source::Path { editable: true, .. });
    let decl = Decl { kind, name, spec: crate::pull::strip_credentials(source_text), source: Some(source), scope, editable, from: crate::MANIFEST_PATH.into() };
    let env = Env::new(ctx, profile).fresh();
    let (mut resolved, packages) = if kind == Kind::Package {
        // with the packages already declared, so they resolve as one graph
        let lock = Lockfile::load(lock_path).ok();
        let (all, _) = all_declared(manifest.as_ref(), &env.project)?;
        let mut roots: Vec<Decl> = all.into_iter().filter(|d| d.kind == Kind::Package && d.name != decl.name).collect();
        roots.push(decl.clone());
        let (r, p) = crate::apm::resolve_packages(&roots, &env, lock.as_ref())?;
        (r, Some(p))
    } else {
        resolve(&decl, &env, None)?
    };
    let mut warnings = Vec::new();
    // where each would go, without copying anything
    for r in resolved.iter_mut() {
        if let Err(e) = locate(r, &env) {
            warnings.push(e.to_string());
        }
    }
    // what it would place, scanned before anything is declared or installed
    let mut findings = Vec::new();
    for r in resolved.iter_mut() {
        let Some(content) = r.content.clone() else { continue };
        let label = match &r.entry {
            Entry::Skill(s) => s.path.clone().unwrap_or_else(|| s.name.clone()),
            Entry::Instructions(i) => i.name.clone(),
            _ => continue,
        };
        let f = crate::scan::scan_path(&content, &label);
        let allowed = f.iter().filter(|x| x.severity == crate::scan::Severity::Critical).all(|x| crate::audit::allows(&req.allow_hidden, x));
        let scan = Some(crate::scan::record(&f, allowed));
        match &mut r.entry {
            Entry::Skill(s) => s.scan = scan,
            Entry::Instructions(i) => i.scan = scan,
            _ => {}
        }
        let warn = f.iter().filter(|x| x.severity == crate::scan::Severity::Warning).count();
        if warn > 0 {
            warnings.push(format!("{} {}: {warn} invisible character{} (zero-width or bidi marks); see `hlock audit`", r.kind, r.name, if warn == 1 { "" } else { "s" }));
        }
        findings.extend(f);
    }
    crate::audit::blocking(&findings, &req.allow_hidden, &format!("{} {}", decl.kind, decl.name))?;
    // the project's policy, on what it declares and what that brought in
    let policy = crate::policy::Policy::for_lock(lock_path, ctx)?;
    let mut probe = Lockfile::default();
    apply(&mut probe, decl.kind, &decl.name, &resolved, packages.as_deref());
    let mut violations = policy.check_source(&decl.spec);
    violations.extend(policy.check_lock(&Lockfile { harness: Default::default(), ..probe }));
    violations.dedup();
    warnings.extend(policy.enforce(violations, &format!("{} {}", decl.kind, decl.name))?);
    Ok(Prepared { decl, harness, resolved, packages, manifest: manifest_path.to_path_buf(), lock: lock_path.to_path_buf(), replaces, warnings })
}

/// Fill in where a resolved dependency goes (`path` for a skill, `name` for an
/// instruction file) without copying anything.
pub fn locate(r: &mut Resolved, env: &Env) -> Result<()> {
    match &mut r.entry {
        Entry::Skill(s) => {
            let (_, portable) = skill_dest(env.profile, s.scope.as_deref().unwrap_or("project"), &s.name, env.ctx)?;
            s.path = Some(portable);
        }
        Entry::Instructions(i) => {
            let (_, portable) = instructions_dest(env.profile, &i.scope, &r.name, env.ctx)?;
            i.name = portable;
        }
        Entry::Tool(_) | Entry::Native(_) => {}
    }
    Ok(())
}

/// What [`commit`] did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddOutcome {
    pub kind: Kind,
    pub name: String,
    pub source: String,
    pub harness: String,
    pub entries: Vec<Entry>,
    pub packages: Vec<Package>,
    pub manifest: PathBuf,
    pub lock: PathBuf,
    pub created_manifest: bool,
    pub created_lock: bool,
    pub installed: Vec<String>,
    pub replaced: Option<String>,
    pub warnings: Vec<String>,
}

/// Declare a prepared dependency in `harness.toml`, pin it in the lock, and (with
/// `install`) put it in the harness. A project with no lock is pinned first, so the lock
/// says what else is there. `evidence` is what `--eval` measured.
pub fn commit(p: &Prepared, reg: &crate::profiles::Registry, ctx: &Ctx, install_it: bool, evidence: Option<crate::lockfile::DepEvidence>) -> Result<AddOutcome> {
    let profile = reg.require_harness(&p.harness)?;
    let mut warnings = p.warnings.clone();
    let created_manifest = !p.manifest.is_file();
    let created_lock = !p.lock.is_file();
    if created_lock {
        let project = std::fs::read_to_string(&p.manifest).ok().and_then(|t| crate::manifest::Manifest::from_str(&t).ok()).map(|m| m.name());
        let o = crate::capture::pin(reg, ctx, Some(&p.harness), None, project.as_deref(), &p.lock, true, false, crate::capture::UserScope::None)?;
        warnings.extend(o.warnings);
    }
    let mut lock = Lockfile::load(&p.lock)?;
    if lock.harness.name != profile.id {
        return Err(err(format!("{} is for {}, not {}", p.lock.display(), lock.harness.name, profile.name)));
    }
    let env = Env::for_lock(ctx, profile, &lock);
    let mut resolved = p.resolved.clone();
    let mut installed = Vec::new();
    let mut touched = BTreeSet::new();
    for r in resolved.iter_mut() {
        if install_it {
            let out = install(r, &env)?;
            installed.extend(out.messages);
            warnings.extend(out.warnings);
            touched.extend(out.files);
        } else {
            locate(r, &env)?;
        }
    }
    if let (Some(ev), Some(first)) = (evidence, resolved.first_mut()) {
        first.entry.dep_mut().evidence = Some(ev);
    }
    apply(&mut lock, p.decl.kind, &p.decl.name, &resolved, p.packages.as_deref());
    refresh_config(&mut lock, &p.lock, profile, reg, ctx, &touched)?;
    if created_manifest {
        let name = ctx.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".into());
        crate::fsutil::write_atomic(&p.manifest, new_manifest_text(&name, &profile.id))?;
    }
    manifest_set(&p.manifest, &p.decl)?;
    if lock.is_pin() {
        lock.manifest_hash = Some(crate::manifest::Manifest::load(&p.manifest)?.deps_hash());
    }
    lock.save(&p.lock)?;
    // you asked for it and saw what it is: what it runs is approved on this machine (one a
    // package brings still waits for `hlock trust`, unless it was trusted already)
    if install_it {
        let mut store = crate::trust::TrustStore::load(ctx)?;
        let mine = Lockfile { tool: resolved.iter().filter_map(|r| match &r.entry { Entry::Tool(t) if t.dep.required_by.is_empty() => Some(t.clone()), _ => None }).collect(), native: resolved.iter().filter_map(|r| match &r.entry { Entry::Native(n) => Some(n.clone()), _ => None }).collect(), skill: resolved.iter().filter_map(|r| match &r.entry { Entry::Skill(s) => Some(s.clone()), _ => None }).collect(), instructions: resolved.iter().filter_map(|r| match &r.entry { Entry::Instructions(i) => Some(i.clone()), _ => None }).collect(), ..Default::default() };
        let dir = crate::sync::lock_dir_of(&p.lock, ctx);
        if store.approve(&crate::trust::executables(&mine, &dir, Some(profile), ctx)) > 0 {
            store.save(ctx)?;
        }
    }
    Ok(AddOutcome {
        kind: p.decl.kind,
        name: p.decl.name.clone(),
        source: p.decl.spec.clone(),
        harness: profile.id.clone(),
        entries: resolved.into_iter().map(|r| r.entry).collect(),
        packages: p.packages.clone().unwrap_or_default(),
        manifest: p.manifest.clone(),
        lock: p.lock.clone(),
        created_manifest,
        created_lock,
        installed,
        replaced: p.replaces.clone().filter(|r| r != &p.decl.spec),
        warnings,
    })
}

/// `hlock add --adopt NAME`: declare something `pin` already recorded, from the source
/// the lock knows for it, pinned where it is now.
pub fn adopt(name: &str, manifest_path: &Path, lock_path: &Path, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<AddOutcome> {
    let mut lock = Lockfile::load(lock_path)?;
    let profile = reg.require_harness(&lock.harness.name)?;
    let no_source = |what: &str| err(format!("{what} {name} has no source to declare (pin vendored it); add it from where it came from: hlock add {what} SOURCE --as {name}"));
    let (decl, mut entry) = if let Some(s) = lock.skill.iter().find(|s| s.name == name && !matches!(s.origin.as_str(), "harness-builtin" | "bundled")) {
        let src = s.source.clone().ok_or_else(|| no_source("skill"))?;
        let (spec, commit) = match src.strip_prefix("path:") {
            Some(p) => (if p.starts_with("./") || p.starts_with("../") || p.starts_with('/') || p.starts_with("~/") { p.to_string() } else { format!("./{p}") }, None),
            None => match parse(&src, Kind::Skill)? {
                Source::Git(g) => {
                    let commit = g.reference.clone();
                    let mut frag = Vec::new();
                    if let Some(sp) = &g.subpath {
                        frag.push(format!("path={sp}"));
                    }
                    (format!("git+{}{}", g.url, if frag.is_empty() { String::new() } else { format!("#{}", frag.join("&")) }), commit)
                }
                _ => return Err(no_source("skill")),
            },
        };
        let mut s = s.clone();
        s.source = Some(spec.clone());
        s.dep = Declared { declared: true, commit, ..Default::default() };
        let scope = s.scope.clone().unwrap_or_else(|| "project".into());
        (Decl { kind: Kind::Skill, name: name.into(), spec, source: None, scope, editable: s.editable, from: crate::MANIFEST_PATH.into() }, Entry::Skill(s))
    } else if let Some(t) = lock.tool.iter().find(|t| t.name == name) {
        let spec = match (&t.url, t.source.as_deref()) {
            (Some(u), _) if !u.contains(crate::secrets::PREFIX) => u.clone(),
            (_, Some(src)) if src.starts_with("npm:") || src.starts_with("pypi:") => {
                // the version the server runs, from its arguments (`pkg@1.2.3`, `pkg==1.2.3`)
                let pkg = src.split_once(':').map(|(_, p)| p).unwrap_or(src);
                let pinned = t.args.iter().find(|a| a.starts_with(pkg) && (a.contains('@') || a.contains("=="))).cloned();
                match pinned {
                    Some(a) if src.starts_with("pypi:") => format!("pypi:{a}"),
                    Some(a) => format!("npm:{a}"),
                    None => src.to_string(),
                }
            }
            _ => return Err(err(format!("mcp {name} isn't from npm, PyPI or a URL, so it can't be declared; keep it pinned"))),
        };
        let mut t = t.clone();
        t.source = Some(spec.clone());
        t.dep = Declared { declared: true, ..Default::default() };
        let scope = t.scope.clone().unwrap_or_else(|| "project".into());
        (Decl { kind: Kind::Mcp, name: name.into(), spec, source: None, scope, editable: false, from: crate::MANIFEST_PATH.into() }, Entry::Tool(t))
    } else if let Some(n) = lock.native.iter().find(|n| plugin_decl_name(n) == name && n.kind.ends_with(".plugin")) {
        let market = n.source.as_deref().and_then(|s| s.strip_prefix("marketplace:")).or_else(|| n.name.split_once('@').map(|(_, m)| m)).ok_or_else(|| no_source("plugin"))?;
        let spec = format!("plugin:{market}/{}", plugin_decl_name(n));
        let mut n = n.clone();
        n.source = Some(spec.clone());
        n.dep = Declared { declared: true, commit: n.revision.clone().filter(|c| plain_token(c)), ..Default::default() };
        (Decl { kind: Kind::Plugin, name: name.into(), spec, source: None, scope: "project".into(), editable: false, from: crate::MANIFEST_PATH.into() }, Entry::Native(n))
    } else {
        return Err(err(format!("{} has no skill, MCP server or plugin named {name} to adopt", lock_path.display())));
    };
    parse(&decl.spec, decl.kind)?;
    entry.dep_mut().constraint = None;
    let created_manifest = !manifest_path.is_file();
    if created_manifest {
        let n = ctx.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".into());
        crate::fsutil::write_atomic(manifest_path, new_manifest_text(&n, &profile.id))?;
    }
    let r = Resolved { kind: decl.kind, name: decl.name.clone(), entry: entry.clone(), content: None, locked: true };
    apply(&mut lock, decl.kind, &decl.name, std::slice::from_ref(&r), None);
    manifest_set(manifest_path, &decl)?;
    if lock.is_pin() {
        lock.manifest_hash = Some(crate::manifest::Manifest::load(manifest_path)?.deps_hash());
    }
    lock.save(lock_path)?;
    Ok(AddOutcome {
        kind: decl.kind,
        name: decl.name,
        source: decl.spec,
        harness: profile.id.clone(),
        entries: vec![entry],
        packages: Vec::new(),
        manifest: manifest_path.to_path_buf(),
        lock: lock_path.to_path_buf(),
        created_manifest,
        created_lock: false,
        installed: Vec::new(),
        replaced: None,
        warnings: Vec::new(),
    })
}

// --------------------------------------------------------------------------- //
// hlock remove
// --------------------------------------------------------------------------- //

/// What [`remove`] did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveOutcome {
    pub kind: Kind,
    pub name: String,
    pub from_manifest: bool,
    pub from_lock: usize,
    pub uninstalled: Vec<String>,
    pub warnings: Vec<String>,
}

/// Undeclare a dependency, take it out of the lock, and take it out of the harness
/// (unless it was changed by hand since: then it's left, with a warning).
pub fn remove(kind: Kind, name: &str, manifest_path: &Path, lock_path: &Path, reg: &crate::profiles::Registry, ctx: &Ctx, uninstall: bool) -> Result<RemoveOutcome> {
    let mut lock = Lockfile::load(lock_path)?;
    let profile = reg.require_harness(&lock.harness.name)?;
    let env = Env::for_lock(ctx, profile, &lock);
    let entries = locked_declarations(&lock).remove(&(kind, name.to_string())).unwrap_or_default();
    let from_manifest = manifest_remove(manifest_path, kind, name)?;
    if entries.is_empty() && !from_manifest {
        return Err(err(format!("no {kind} named {name} is declared")));
    }
    let mut uninstalled = Vec::new();
    let mut warnings = Vec::new();
    let mut touched = BTreeSet::new();
    // packages are one graph: what the others still need stays, re-resolved without this one
    let mut regraph: Option<(Vec<Resolved>, Vec<Package>)> = None;
    let mut entries = entries;
    if kind == Kind::Package {
        let manifest = if manifest_path.is_file() { Some(crate::manifest::Manifest::load(manifest_path)?) } else { None };
        let (decls, _) = all_declared(manifest.as_ref(), &env.project)?;
        let rest: Vec<Decl> = decls.into_iter().filter(|d| d.kind == Kind::Package && d.name != name).collect();
        if !rest.is_empty() {
            let mut snapshot = lock.clone();
            for r in &rest {
                forget(&mut snapshot, Kind::Package, &r.name);
            }
            forget(&mut snapshot, Kind::Package, name);
            let (mut resolved, packages) = crate::apm::resolve_packages(&rest, &env, Some(&snapshot))?;
            for r in resolved.iter_mut() {
                locate(r, &env)?;
            }
            // only what the remaining graph doesn't have any more is uninstalled
            let kept: BTreeSet<(Kind, String)> = resolved.iter().map(|r| (r.entry.kind(), r.entry.name().to_string())).collect();
            let mut all = entries.clone();
            for r in &rest {
                all.extend(locked_declarations(&lock).remove(&(Kind::Package, r.name.clone())).unwrap_or_default());
            }
            entries = all.into_iter().filter(|e| !kept.contains(&(e.kind(), e.name().to_string()))).collect();
            regraph = Some((resolved, packages));
        }
    }
    if uninstall {
        for e in &entries {
            match uninstall_entry(e, &env) {
                Ok(Some((msg, file))) => {
                    uninstalled.push(msg);
                    touched.extend(file);
                }
                Ok(None) => {}
                Err(e) => warnings.push(e.to_string()),
            }
        }
    }
    let from_lock = forget(&mut lock, kind, name);
    if let Some((resolved, packages)) = regraph {
        let mut resolved = resolved;
        // what the remaining packages now own that wasn't installed (it belonged to this one)
        for r in resolved.iter_mut() {
            if installed_state(&r.entry, &env).1.is_none() {
                match install(r, &env) {
                    Ok(out) => touched.extend(out.files),
                    Err(e) => warnings.push(e.to_string()),
                }
            }
        }
        lock.package.retain(|p| p.name != name);
        apply(&mut lock, Kind::Package, name, &resolved, Some(&packages));
    } else if kind == Kind::Package {
        lock.package.retain(|p| p.name != name && p.dep.required_by.first().map(String::as_str) != Some(name));
    }
    refresh_config(&mut lock, lock_path, profile, reg, ctx, &touched)?;
    if lock.is_pin() && manifest_path.is_file() {
        lock.manifest_hash = Some(crate::manifest::Manifest::load(manifest_path)?.deps_hash());
    }
    lock.save(lock_path)?;
    Ok(RemoveOutcome { kind, name: name.to_string(), from_manifest, from_lock, uninstalled, warnings })
}

/// Whether `path` is somewhere `install` would put `e`: strictly under one of the
/// harness's skill directories, or in its rules directory or one of its instruction files.
fn in_harness_place(e: &Entry, path: &Path, env: &Env) -> bool {
    let root = env.profile.config_root(env.ctx);
    let real = crate::fsutil::resolve(path);
    let under = |d: &str| {
        let d = crate::fsutil::resolve(&env.ctx.expand(d, Some(&root)));
        real.starts_with(&d) && real != d
    };
    match e {
        Entry::Skill(_) => env.profile.skills.write.iter().chain(env.profile.skills.dirs.iter()).any(|d| under(d)),
        Entry::Instructions(_) => {
            env.profile.rules.as_ref().is_some_and(|r| real.parent() == Some(crate::fsutil::resolve(&env.ctx.cwd.join(&r.dir)).as_path()))
                || env.profile.instructions.iter().any(|t| crate::fsutil::resolve(&env.ctx.expand(t, Some(&root))) == real)
        }
        _ => false,
    }
}

/// Take one installed entry out of the harness, when it's still what the lock says.
fn uninstall_entry(e: &Entry, env: &Env) -> Result<Option<(String, Option<String>)>> {
    let ctx = env.ctx;
    match e {
        Entry::Skill(_) | Entry::Instructions(_) => {
            let (path, have) = installed_state(e, env);
            let Some(path) = path else { return Ok(None) };
            // the lock says where; only a place the harness reads dependencies from is removed
            if !in_harness_place(e, &path, env) {
                return Err(err(format!("{} {}: {} isn't where {} keeps {}s; left it alone", e.kind(), e.name(), crate::sync::pretty(&path, ctx), env.profile.name, e.kind())));
            }
            if have.is_none() {
                return Ok(None);
            }
            if have.as_deref() != e.digest() {
                return Err(err(format!("{} {} was changed by hand since it was added; left it at {}", e.kind(), e.name(), crate::sync::pretty(&path, ctx))));
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path).map_err(|x| crate::error::io_at(&path, x))?;
            } else {
                std::fs::remove_file(&path).map_err(|x| crate::error::io_at(&path, x))?;
            }
            Ok(Some((format!("removed {}", crate::sync::pretty(&path, ctx)), None)))
        }
        Entry::Tool(t) => {
            let dest = mcp_dest(env.profile, t.scope.as_deref().unwrap_or("project"), ctx)?;
            Ok(write_mcp(&dest, &t.name, None)?.then(|| (format!("removed MCP server {} from {}", t.name, crate::sync::pretty(&dest.path, ctx)), Some(dest.file.clone()))))
        }
        Entry::Native(n) => {
            let bin = env.profile.binaries.first().cloned().unwrap_or_default();
            let Some(exe) = ctx.which(&bin) else { return Err(err(format!("uninstalling plugin {} needs `{bin}` on PATH", n.name))) };
            let mut cmd = Command::new(exe);
            cmd.args(["plugin", "uninstall", &n.name]).current_dir(&ctx.cwd).env_clear().envs(&ctx.env).stdin(Stdio::null());
            let out = crate::process::run(cmd, &crate::process::Limits::timeout(Duration::from_secs(300))).map_err(|x| err(format!("can't run {bin}: {x}")))?;
            if !out.success {
                return Err(err(format!("`{bin} plugin uninstall {}` failed: {}", n.name, out.text().trim().lines().last().unwrap_or(""))));
            }
            Ok(Some((format!("uninstalled plugin {}", n.name), Some("settings".into()))))
        }
    }
}

// --------------------------------------------------------------------------- //
// Every declaration: harness.toml and apm.yml
// --------------------------------------------------------------------------- //

/// What the project declares: `harness.toml`'s dependencies, then `apm.yml`'s. When both
/// declare the same package, `harness.toml`'s counts, and a disagreement is a conflict.
pub fn all_declared(manifest: Option<&crate::manifest::Manifest>, project: &Path) -> Result<(Vec<Decl>, Vec<Issue>)> {
    let mut decls = match manifest {
        Some(m) => declared(m)?,
        None => Vec::new(),
    };
    let mut issues = Vec::new();
    for a in crate::apm::declared(project)? {
        let same_id = |d: &Decl| match (d.source().ok(), a.source().ok()) {
            (Some(Source::Git(x)), Some(Source::Git(y))) => crate::apm::source_identity(&x) == crate::apm::source_identity(&y),
            _ => false,
        };
        match decls.iter().find(|d| d.kind == Kind::Package && (d.name == a.name || same_id(d))) {
            Some(d) if d.spec != a.spec => issues.push(Issue {
                problem: "conflict".into(),
                kind: Kind::Package,
                name: d.name.clone(),
                detail: format!("harness.toml declares {}, apm.yml declares {}", d.spec, a.spec),
                fix: "make them agree".into(),
                fails: true,
            }),
            Some(_) => {}
            None => decls.push(a),
        }
    }
    Ok((decls, issues))
}

// --------------------------------------------------------------------------- //
// hlock check
// --------------------------------------------------------------------------- //

/// A way the declared dependencies differ from the lock or from what's installed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    /// `missing`, `changed`, `unlocked`, `removed`, `undeclared`, `stale-evidence`,
    /// `conflict`, `stale-export`.
    pub problem: String,
    pub kind: Kind,
    pub name: String,
    pub detail: String,
    /// The command that fixes it.
    pub fix: String,
    /// Whether `hlock check` fails for it (stale evidence and a stale export only warn).
    pub fails: bool,
}

/// Compare the declared dependencies with the lock and with what's installed: declared
/// but missing, declared but changed, declared but not locked (or locked from another
/// source), locked but no longer declared, and installed but neither declared nor locked.
pub fn check(manifest: Option<&crate::manifest::Manifest>, lock: &Lockfile, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<Vec<Issue>> {
    // nothing declared, nothing locked as declared: nothing to compare
    let (decls, _) = all_declared(manifest, &ctx.cwd)?;
    if decls.is_empty() && locked_declarations(lock).is_empty() {
        return Ok(Vec::new());
    }
    let profile = reg.require_harness(&lock.harness.name)?;
    let env = Env::new(ctx, profile);
    let (decls, mut issues) = all_declared(manifest, &env.project)?;
    let locked = locked_declarations(lock);
    let issue = |problem: &str, kind: Kind, name: &str, detail: String, fix: String, fails: bool| Issue { problem: problem.into(), kind, name: name.into(), detail, fix, fails };
    let mut seen: BTreeSet<(Kind, String)> = BTreeSet::new();
    for d in &decls {
        seen.insert((d.kind, d.name.clone()));
        let entries: Vec<Entry> = if d.kind == Kind::Package {
            match lock.package.iter().find(|p| p.name == d.name && p.depth == 1) {
                Some(p) if p.source == d.spec => locked.get(&(d.kind, d.name.clone())).cloned().unwrap_or_default(),
                Some(p) => {
                    issues.push(issue("unlocked", d.kind, &d.name, format!("{} now asks for {}; the lock has {}", d.from, d.spec, p.source), "hlock sync".into(), true));
                    continue;
                }
                None => {
                    issues.push(issue("unlocked", d.kind, &d.name, format!("declared in {}, not in the lock", d.from), "hlock sync".into(), true));
                    continue;
                }
            }
        } else {
            match locked_entry(lock, d) {
                Some(e) => vec![e],
                None => {
                    let other = locked.get(&(d.kind, d.name.clone())).and_then(|es| es.first()).and_then(|e| e.source().map(str::to_string));
                    let detail = match other {
                        Some(src) => format!("{} now asks for {}; the lock has {src}", d.from, d.spec),
                        None => format!("declared in {}, not in the lock", d.from),
                    };
                    issues.push(issue("unlocked", d.kind, &d.name, detail, "hlock sync".into(), true));
                    continue;
                }
            }
        };
        for e in &entries {
            let label = if d.kind == Kind::Package { format!("{} {} (from {})", e.kind(), e.name(), d.name) } else { format!("{} {}", e.kind(), e.name()) };
            let (path, have) = installed_state(e, &env);
            let shown = path.as_deref().map(|p| crate::sync::pretty(p, ctx)).unwrap_or_default();
            let want = match e {
                Entry::Tool(t) => mcp_dest(profile, t.scope.as_deref().unwrap_or("project"), ctx).map(|dest| crate::hash::content_hash(&mcp_entry(t, &dest.shape))).ok(),
                _ => e.digest().map(str::to_string),
            };
            let want = match e {
                Entry::Tool(_) => e.digest().map(str::to_string).or(want),
                _ => want,
            };
            // servers a package brings, and registry references, are recorded, not installed
            let recorded_only = matches!(e, Entry::Tool(t) if (t.command.is_none() && t.url.is_none()) || !t.dep.required_by.is_empty());
            match have {
                _ if recorded_only => {}
                None => {
                    issues.push(issue("missing", d.kind, &d.name, format!("{label} isn't installed{}", if shown.is_empty() { String::new() } else { format!(" at {shown}") }), "hlock sync".into(), true))
                }
                Some(h) if want.is_some() && Some(&h) != want.as_ref() && !matches!(e, Entry::Native(_)) => {
                    let editable = matches!(e, Entry::Skill(s) if s.editable);
                    let fix = "hlock sync (restores the locked copy; your edit is kept in .hlock/bak/)".to_string();
                    if editable {
                        issues.push(issue("changed", d.kind, &d.name, format!("{label} (editable) changed at {shown}"), fix, false));
                    } else {
                        issues.push(issue("changed", d.kind, &d.name, format!("{label} differs from the lock at {shown}"), fix, true));
                    }
                }
                _ => {}
            }
            if let Some(ev) = &e.dep().evidence {
                // the lock's digest, or for an editable path what's on disk now
                let now = if matches!(e, Entry::Skill(s) if s.editable) { installed_state(e, &env).1 } else { e.digest().map(str::to_string) };
                if ev.digest.is_some() && ev.digest != now {
                    issues.push(issue("stale-evidence", d.kind, &d.name, format!("{label} changed since --eval measured it"), format!("hlock upgrade dep:{} --eval", d.name), false));
                }
            }
        }
    }
    for (kind, name) in locked.keys() {
        if !seen.contains(&(*kind, name.clone())) {
            issues.push(issue("removed", *kind, name, "in the lock as declared, but no longer declared".into(), format!("hlock remove {kind} {name}"), true));
        }
    }
    issues.extend(undeclared(lock, &env));
    if let Some(i) = crate::apm::stale_export(lock, &env.project) {
        issues.push(i);
    }
    Ok(issues)
}

/// Skills in the harness's project skill directory that neither the lock nor the
/// manifest knows.
fn undeclared(lock: &Lockfile, env: &Env) -> Vec<Issue> {
    let Some(dir) = project_skills_dir(env.profile) else { return Vec::new() };
    let root = env.profile.config_root(env.ctx);
    let base = env.ctx.expand(dir, Some(&root));
    let mut out = Vec::new();
    for e in std::fs::read_dir(&base).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !e.path().join("SKILL.md").is_file() || name.ends_with(".hlock-bak") {
            continue;
        }
        if lock.skill.iter().any(|s| s.name == name || s.path.as_deref().is_some_and(|p| p.trim_end_matches('/').ends_with(&format!("/{name}")))) {
            continue;
        }
        out.push(Issue {
            problem: "undeclared".into(),
            kind: Kind::Skill,
            name: name.clone(),
            detail: format!("skill {name} is installed at {}/{name}, but isn't declared or pinned", dir.trim_end_matches('/')),
            fix: format!("hlock pin, then hlock add --adopt {name}"),
            fails: true,
        });
    }
    out
}

// --------------------------------------------------------------------------- //
// Keeping the lock current (sync)
// --------------------------------------------------------------------------- //

/// Bring the lock's declared entries in line with the declarations: resolve what's new
/// or whose source changed, drop what's no longer declared, and leave the rest (no
/// network when nothing changed). Returns what changed, as diff lines. With `frozen`, a
/// lock that would change is an error instead.
pub fn refresh(manifest: Option<&crate::manifest::Manifest>, lock: &mut Lockfile, reg: &crate::profiles::Registry, ctx: &Ctx, frozen: bool) -> Result<Vec<String>> {
    // `[environment]` and `[[secrets]]` are recorded as declared: nothing to resolve
    let mut changes = match manifest {
        Some(m) => {
            m.check_declared()?;
            let mut next = lock.clone();
            let changes = next.declare(&m.environment, &m.secrets);
            if !frozen {
                *lock = next;
            }
            changes
        }
        None => Vec::new(),
    };
    let (decls, _) = all_declared(manifest, &ctx.cwd)?;
    if decls.is_empty() && locked_declarations(lock).is_empty() {
        return finish_refresh(manifest, lock, frozen, changes);
    }
    let profile = reg.require_harness(&lock.harness.name)?;
    let env = Env::new(ctx, profile);
    let (decls, conflicts) = all_declared(manifest, &env.project)?;
    if let Some(c) = conflicts.first() {
        return Err(err(format!("package {}: {}", c.name, c.detail)));
    }
    let mut seen = BTreeSet::new();
    let pkgs: Vec<Decl> = decls.iter().filter(|d| d.kind == Kind::Package).cloned().collect();
    let pkg_names: BTreeSet<&str> = pkgs.iter().map(|d| d.name.as_str()).collect();
    let locked_roots: BTreeSet<String> = lock.package.iter().filter(|p| p.depth == 1).map(|p| p.name.clone()).collect();
    let stale = pkgs.iter().any(|d| crate::apm::locked_package(lock, d, &env).is_none()) || locked_roots.iter().any(|r| !pkg_names.contains(r.as_str()));
    if stale && !pkgs.is_empty() {
        for d in &pkgs {
            seen.insert((d.kind, d.name.clone()));
            changes.push(format!("+ package {} {}", d.name, d.spec));
        }
        if !frozen {
            let snapshot = lock.clone();
            let (mut resolved, packages) = crate::apm::resolve_packages(&pkgs, &env, Some(&snapshot))?;
            for r in resolved.iter_mut() {
                locate(r, &env)?;
            }
            apply(lock, Kind::Package, &pkgs[0].name, &resolved, Some(&packages));
        }
    }
    for d in &decls {
        seen.insert((d.kind, d.name.clone()));
        if d.kind == Kind::Package || locked_entry(lock, d).is_some() {
            continue;
        }
        if frozen {
            // nothing is looked up: the lock is wrong, and that's the answer
            changes.push(format!("+ {} {} {}", d.kind, d.name, d.spec));
            continue;
        }
        let (mut resolved, packages) = resolve(d, &env, Some(lock))?;
        for r in resolved.iter_mut() {
            locate(r, &env)?;
        }
        let pin = resolved.first().and_then(|r| r.entry.dep().resolved_tag.clone().or_else(|| r.entry.dep().commit.as_deref().map(|c| crate::fsutil::prefix_chars(c, 7).to_string())));
        changes.push(format!("+ {} {} {}{}", d.kind, d.name, d.spec, pin.map(|p| format!(" @ {p}")).unwrap_or_default()));
        apply(lock, d.kind, &d.name, &resolved, packages.as_deref());
    }
    for (kind, name) in locked_declarations(lock).into_keys() {
        if !seen.contains(&(kind, name.clone())) {
            changes.push(format!("- {kind} {name}"));
            if !frozen {
                forget(lock, kind, &name);
                lock.package.retain(|p| p.name != name || kind != Kind::Package);
            }
        }
    }
    finish_refresh(manifest, lock, frozen, changes)
}

fn finish_refresh(manifest: Option<&crate::manifest::Manifest>, lock: &mut Lockfile, frozen: bool, changes: Vec<String>) -> Result<Vec<String>> {
    if frozen && !changes.is_empty() {
        return Err(err(format!("the lock is out of date with the declared dependencies (--frozen):\n  {}\nrun `hlock sync` without --frozen to update it", changes.join("\n  "))));
    }
    if !changes.is_empty() && lock.is_pin() {
        if let Some(m) = manifest {
            lock.manifest_hash = Some(m.deps_hash());
        }
    }
    Ok(changes)
}

// --------------------------------------------------------------------------- //
// hlock outdated
// --------------------------------------------------------------------------- //

/// A declared dependency with a newer upstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outdated {
    pub kind: Kind,
    pub name: String,
    pub source: String,
    /// What the lock pins: a tag, a version, or a short commit.
    pub current: String,
    /// The newest the declaration allows (`hlock upgrade dep:NAME` moves to it).
    pub wanted: Option<String>,
    /// The newest there is, whatever the declaration allows.
    pub latest: Option<String>,
}

/// Look upstream for newer commits, tags and versions of the declared dependencies.
pub fn outdated(manifest: Option<&crate::manifest::Manifest>, lock: &Lockfile, reg: &crate::profiles::Registry, ctx: &Ctx, only: Option<Kind>) -> Result<Vec<Outdated>> {
    let profile = reg.require_harness(&lock.harness.name)?;
    let env = Env::new(ctx, profile);
    let (decls, _) = all_declared(manifest, &env.project)?;
    let decls: Vec<&Decl> = decls.iter().filter(|d| only.is_none_or(|k| k == d.kind)).collect();
    let gits: Vec<GitSource> = decls.iter().filter_map(|d| d.source().ok().and_then(|s| s.git().cloned())).collect();
    let listings = ls_remote_many(&gits.iter().collect::<Vec<_>>());
    let mut out = Vec::new();
    for d in decls {
        let (current, commit) = match d.kind {
            Kind::Package => match lock.package.iter().find(|p| p.name == d.name && p.depth == 1) {
                Some(p) => (p.dep.resolved_tag.clone().or_else(|| p.dep.commit.clone()), p.dep.commit.clone()),
                None => continue,
            },
            _ => match locked_entry(lock, d) {
                Some(Entry::Tool(t)) => (t.version.clone(), None),
                Some(e) => (e.dep().resolved_tag.clone().or_else(|| e.dep().commit.clone()), e.dep().commit.clone()),
                None => continue,
            },
        };
        let Some(current) = current else { continue };
        let short = |c: &str| if is_full_sha(c) { crate::fsutil::prefix_chars(c, 7).to_string() } else { c.to_string() };
        let (wanted, latest) = match d.source()? {
            Source::Git(g) => {
                let Some(Ok(refs)) = listings.get(&g.url) else { continue };
                let name = g.package_name();
                let versions = tag_versions(refs.tags.keys(), &name);
                let newest = VersionRange::parse("*")?.best_with(versions.iter().map(|(v, t)| (v.clone(), t.as_str())), false).map(|(_, t)| t.to_string());
                match (&g.constraint, &g.reference) {
                    (Some(_), _) => (pick(&g, refs).ok().and_then(|p| p.resolved_tag), newest),
                    (None, Some(r)) if refs.heads.contains_key(r) => {
                        let head = refs.heads.get(r).cloned();
                        (head.clone().filter(|h| Some(h) != commit.as_ref()).map(|h| short(&h)), head.filter(|h| Some(h) != commit.as_ref()).map(|h| short(&h)))
                    }
                    (None, Some(_)) => (None, newest.filter(|n| n != &current)),
                    (None, None) => {
                        let head = refs.head.clone().filter(|h| Some(h) != commit.as_ref()).map(|h| short(&h));
                        (head.clone(), head)
                    }
                }
            }
            Source::Npm { package, constraint } => {
                let wanted = npm_resolve(&package, constraint.as_deref()).ok().map(|x| x.0);
                (wanted, npm_resolve(&package, None).ok().map(|x| x.0))
            }
            Source::Pypi { package, constraint } => {
                let wanted = pypi_resolve(&package, constraint.as_deref()).ok().map(|x| x.0);
                (wanted, pypi_resolve(&package, None).ok().map(|x| x.0))
            }
            _ => continue,
        };
        let cur = short(&current);
        let wanted = wanted.filter(|w| *w != cur);
        let latest = latest.filter(|l| *l != cur);
        if wanted.is_some() || latest.is_some() {
            out.push(Outdated { kind: d.kind, name: d.name.clone(), source: d.spec.clone(), current: cur, wanted, latest });
        }
    }
    Ok(out)
}

/// `hlock upgrade dep:NAME`: the declaration resolved afresh, to the newest commit, tag or
/// version it allows. Nothing changes until [`commit`].
pub fn prepare_upgrade(name: &str, manifest_path: &Path, lock_path: &Path, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<Prepared> {
    let manifest = crate::manifest::Manifest::load(manifest_path)?;
    let (decls, _) = all_declared(Some(&manifest), &ctx.cwd)?;
    let d = decls.into_iter().find(|d| d.name == name).ok_or_else(|| err(format!("no declared dependency named {name}")))?;
    if d.from != crate::MANIFEST_PATH {
        return Err(err(format!("{name} is declared in {}; upgrade it with APM, or declare it in harness.toml", d.from)));
    }
    let req = AddRequest { kind: Some(d.kind.as_str().into()), source: d.spec.clone(), name: Some(d.name.clone()), scope: Some(d.scope.clone()), ..Default::default() };
    prepare(&req, manifest_path, lock_path, reg, ctx)
}

// --------------------------------------------------------------------------- //
// --eval: measure with and without
// --------------------------------------------------------------------------- //

/// One side of a comparison.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Side {
    pub trials: usize,
    pub success_rate: f64,
    pub cost_usd_per_task: f64,
    pub latency_s_p50: f64,
    /// Trials taken from the cache rather than run.
    pub cached: usize,
}

/// What `--eval` measured: the locked stack, and the locked stack plus the dependency.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Measurement {
    pub packs: Vec<String>,
    pub pack_digest: String,
    pub split: String,
    pub tasks: usize,
    pub seeds: u64,
    pub without: Side,
    pub with: Side,
    /// What ran the trials (`reference`, a code harness, the worker).
    pub adapter: String,
    /// The isolated root the candidate was installed into.
    pub staged: PathBuf,
}

impl Measurement {
    /// The lock's `[*.evidence]` for the measured dependency.
    pub fn evidence(&self, digest: Option<String>) -> crate::lockfile::DepEvidence {
        crate::lockfile::DepEvidence {
            eval_pack: Some(self.pack_digest.clone()),
            split: self.split.clone(),
            trials: self.with.trials,
            baseline_success_rate: self.without.success_rate,
            success_rate: self.with.success_rate,
            baseline_cost_usd_per_task: Some(self.without.cost_usd_per_task),
            cost_usd_per_task: Some(self.with.cost_usd_per_task),
            digest,
        }
    }
}

/// The stack a lock describes, for trials: its models, harness, skills, MCP servers and
/// instruction files.
pub(crate) fn lock_stack(lock: &Lockfile) -> crate::search::Stack {
    let mut s = crate::search::Stack::default();
    for l in &lock.llm {
        s.llm.insert(l.role.clone(), format!("{}:{}", l.provider, l.model));
    }
    if s.llm.is_empty() {
        s.llm.insert("planner".into(), "reference:qwen-7b".into());
    }
    if !lock.harness.name.is_empty() {
        s.harness = lock.harness.name.clone();
    }
    s.skills = lock.skill.iter().filter(|x| x.enabled).map(|x| x.name.clone()).collect();
    s.tools = lock.tool.iter().filter(|x| x.enabled != Some(false)).map(|x| x.name.clone()).collect();
    s.instructions = lock.instructions.iter().map(|x| x.name.clone()).collect();
    for d in &lock.decider {
        s.deciders.insert(d.use_.clone(), format!("{}:{}", d.provider, d.model));
    }
    s
}

/// Run the project's eval pack on the locked stack and on the locked stack plus `p`'s
/// dependency, on the search split, each task once per seed. The candidate is installed
/// into an isolated root (`.hlock/env/eval/`), so the project isn't touched; trials are
/// cached by stack, task, seed and pack digest, so the baseline usually costs nothing.
pub fn measure(p: &Prepared, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<Measurement> {
    if !p.manifest.is_file() {
        return Err(err("--eval measures on the project's eval pack, and there's no harness.toml; create one with `hlock evals init`"));
    }
    let manifest = crate::manifest::Manifest::load(&p.manifest)?;
    if manifest.eval_packs().is_empty() {
        return Err(err("--eval needs an eval pack: [evals] packs in harness.toml is empty; create one with `hlock evals init`"));
    }
    let lock = Lockfile::load(&p.lock).unwrap_or_else(|_| Lockfile { harness: crate::lockfile::Harness { name: p.harness.clone(), ..Default::default() }, ..Default::default() });
    // stage the candidate where nothing the project or the user owns is written
    let profile = reg.require_harness(&p.harness)?;
    let staged = ctx.cwd.join(crate::launch::ENV_DIR).join("eval").join(&p.decl.name);
    let _ = std::fs::remove_dir_all(&staged);
    let mut iso = ctx.clone();
    iso.cwd = staged.join("project");
    iso.home = staged.join("home");
    if let Some(var) = &profile.config_root.env {
        iso.env.insert(var.clone(), staged.join("config").to_string_lossy().into_owned());
    }
    std::fs::create_dir_all(&iso.cwd)?;
    crate::sync::ignore_env(&staged)?;
    // nothing is written to the project: a path dependency from outside it is vendored into the staged copy
    let env = Env { project: iso.cwd.clone(), ctx: &iso, profile, refs: std::sync::Mutex::new(BTreeMap::new()), apm_locked: BTreeMap::new(), owned: BTreeSet::new() };
    // the candidate lock: the project's, plus the dependency as installed in isolation
    let mut cand_lock = lock.clone();
    let mut staged_entries = Vec::new();
    for r in p.resolved.clone().iter_mut() {
        if matches!(r.entry, Entry::Skill(_) | Entry::Instructions(_)) {
            install(r, &env)?;
            // where it went in the isolated root, as an absolute path a trial can read
            if let Entry::Skill(s) = &mut r.entry {
                s.path = Some(skill_dest(profile, s.scope.as_deref().unwrap_or("project"), &s.name, &iso)?.0.to_string_lossy().into_owned());
            }
        }
        staged_entries.push(r.clone());
    }
    apply(&mut cand_lock, p.decl.kind, &p.decl.name, &staged_entries, p.packages.as_deref());
    let base = lock_stack(&lock);
    let mut cand = base.clone();
    for r in &p.resolved {
        match &r.entry {
            Entry::Skill(s) => cand.skills.push(s.name.clone()),
            Entry::Tool(t) => cand.tools.push(t.name.clone()),
            Entry::Instructions(i) => cand.instructions.push(i.name.clone()),
            Entry::Native(n) => cand.tools.push(n.name.clone()),
        }
    }
    let backend = crate::worker::Backend::resolve(&manifest, None)?;
    let workdir = ctx.cwd.join(".hlock");
    let mut resolver = crate::resolver::Resolver::with_backend(manifest.clone(), &workdir, backend.clone());
    resolver.harness_override = Some(p.harness.clone());
    resolver.prepare_trials()?;
    // a code harness gets each side's skills and MCP servers, the way `hlock launch` passes them
    let cand_path = staged.join("harness.lock");
    cand_lock.save(&cand_path)?;
    let adapter = resolver.adapter_for(&base);
    let tasks = resolver.load_packs();
    if tasks.is_empty() {
        return Err(err(format!("the eval packs ({}) have no tasks in the search split", manifest.eval_packs().join(", "))));
    }
    let pack_digest = crate::tasks::packs_digest(&manifest);
    let seeds = manifest.seeds();
    let cache_dir = workdir.join("trials");
    let ws_root = workdir.join("runs");
    std::fs::create_dir_all(&ws_root)?;
    // what a side's trials see, by content: a new commit of a skill isn't a cache hit
    let contents = |l: &Lockfile| {
        let parts: Vec<String> = l.skill.iter().map(|x| format!("{}={:?}", x.name, x.digest)).chain(l.instructions.iter().map(|x| format!("{}={:?}", x.name, x.digest))).chain(l.tool.iter().map(|x| format!("{}={:?}{:?}{:?}", x.name, x.command, x.args, x.url))).collect();
        crate::hash::sha256_text(&parts.join("\n"))
    };
    let (base_contents, cand_contents) = (contents(&lock), contents(&cand_lock));
    // each side's own lock: an upgrade's two sides can be the same stack by name
    let mut run = |stack: &crate::search::Stack, side: &str, side_lock: (Lockfile, PathBuf)| -> Result<Side> {
        resolver.trial_locks.clear();
        resolver.trial_locks.insert(stack.hash(), side_lock);
        let mut results = Vec::new();
        let mut cached = 0;
        for seed in 0..seeds {
            for t in &tasks {
                let key = crate::hash::sha256_hex(format!("{}\0{side}\0{}\0{seed}\0{pack_digest}", stack.hash(), t.id).as_bytes());
                let file = cache_dir.join(format!("{key}.json"));
                let hit = std::fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str::<crate::search::TrialResult>(&s).ok());
                let r = match hit {
                    Some(r) => {
                        cached += 1;
                        r
                    }
                    None => {
                        let r = resolver.run_trial(stack, t, seed, &ws_root);
                        if r.error.is_none() {
                            let _ = crate::fsutil::write_atomic(&file, serde_json::to_string(&r).unwrap_or_default());
                        }
                        r
                    }
                };
                results.push(r);
            }
        }
        let score = crate::search::StackScore { stack: stack.clone(), n: results.len(), trials: results };
        let round = |x: f64| (x * 1e4).round() / 1e4;
        Ok(Side { trials: score.n, success_rate: round(score.success_rate()), cost_usd_per_task: (score.mean_cost() * 1e6).round() / 1e6, latency_s_p50: round(score.mean_latency()), cached })
    };
    let without = run(&base, &base_contents, (lock.clone(), p.lock.clone()))?;
    let with = run(&cand, &cand_contents, (cand_lock.clone(), cand_path.clone()))?;
    Ok(Measurement {
        packs: manifest.eval_packs(),
        pack_digest,
        split: "search".into(),
        tasks: tasks.len(),
        seeds,
        without,
        with,
        adapter: if backend.needs_worker() && !adapter.starts_with("code harness") { format!("worker ({})", backend.as_str()) } else { adapter },
        staged,
    })
}

// --------------------------------------------------------------------------- //
// hlock pin: keep what's declared
// --------------------------------------------------------------------------- //

/// What `pin` captured, with the previous lock's declared dependencies carried over: a
/// declared entry that's installed as locked keeps its source and commit (and isn't
/// vendored); one that changed or went missing keeps its declared pin, with a warning
/// (`sync` restores it, `hlock add` accepts the new one). Files `apm.lock.yaml`
/// deployed become dependencies of their package, so `sync` fetches them.
pub fn carry_declared(prev: Option<&Lockfile>, cap: &mut crate::capture::Capture, project: &Path) {
    if let Some(prev) = prev.filter(|p| p.is_pin()) {
        carry_from(prev, cap);
    }
    if let Err(e) = adopt_apm_deployed(cap, project) {
        cap.warnings.push(format!("apm.lock.yaml: {e}"));
    }
}

fn drop_vendored(cap: &mut crate::capture::Capture, rel: Option<&str>) {
    if let Some(rel) = rel {
        let prefix = format!("{rel}/");
        cap.vendor.retain(|f| f.rel != rel && !f.rel.starts_with(&prefix));
    }
}

pub(crate) fn carry_from(prev: &Lockfile, cap: &mut crate::capture::Capture) {
    let lock = &mut cap.lock;
    let mut warnings = Vec::new();
    let mut unvendor = Vec::new();
    for p in prev.skill.iter().filter(|s| s.dep.declared) {
        match lock.skill.iter_mut().find(|s| s.name == p.name && s.scope == p.scope) {
            Some(s) if s.digest == p.digest => {
                unvendor.push(s.vendored.take());
                *s = Skill { path: s.path.clone().or(p.path.clone()), ..p.clone() };
            }
            Some(s) => {
                warnings.push(format!("skill {} differs from its declared pin; kept the pin (hlock sync restores it; hlock add accepts the new one)", p.name));
                unvendor.push(s.vendored.take());
                *s = p.clone();
            }
            None => {
                warnings.push(format!("skill {} is declared but not installed; kept its pin (hlock sync installs it)", p.name));
                lock.skill.push(p.clone());
            }
        }
    }
    for p in prev.tool.iter().filter(|t| t.dep.declared) {
        match lock.tool.iter_mut().find(|t| t.name == p.name) {
            Some(t) if t.command == p.command && t.args == p.args && t.url == p.url => {
                let (env, headers, enabled) = (t.env.clone(), t.headers.clone(), t.enabled);
                *t = Tool { env, headers, enabled, ..p.clone() };
            }
            Some(t) => {
                warnings.push(format!("MCP server {} differs from its declared pin; kept the pin", p.name));
                *t = p.clone();
            }
            None => lock.tool.push(p.clone()),
        }
    }
    for p in prev.instructions.iter().filter(|i| i.dep.declared) {
        match lock.instructions.iter_mut().find(|i| i.name == p.name) {
            Some(i) => {
                unvendor.push(i.vendored.take());
                *i = p.clone();
            }
            None => lock.instructions.push(p.clone()),
        }
    }
    for p in prev.native.iter().filter(|n| n.dep.declared) {
        match lock.native.iter_mut().find(|n| n.name == p.name && n.kind == p.kind) {
            Some(n) => {
                n.source = p.source.clone();
                n.dep = Declared { commit: n.revision.clone().filter(|c| plain_token(c)), ..p.dep.clone() };
            }
            None => lock.native.push(p.clone()),
        }
    }
    lock.package = prev.package.clone();
    if !lock.package.is_empty() {
        lock.provenance.openapm = prev.provenance.openapm.clone();
    }
    // the declared environment and secrets, merged with the placeholders this pin found
    let secrets: Vec<crate::manifest::DeclaredSecret> = prev
        .secret
        .iter()
        .filter(|s| s.declared)
        .map(|s| crate::manifest::DeclaredSecret { name: s.name.clone(), used_by: s.used_by.clone(), optional: s.optional, description: s.description.clone() })
        .collect();
    lock.declare(&prev.environment, &secrets);
    if prev.skill.iter().any(|s| s.dep.declared) || prev.tool.iter().any(|t| t.dep.declared) || prev.instructions.iter().any(|i| i.dep.declared) || prev.native.iter().any(|n| n.dep.declared) || !prev.package.is_empty() || !secrets.is_empty() || !prev.environment.is_empty() {
        lock.manifest_hash = prev.manifest_hash.clone();
    }
    for v in unvendor {
        drop_vendored(cap, v.as_deref());
    }
    cap.warnings.extend(warnings);
}

/// Skills that `apm install` deployed (they're in `apm.lock.yaml`'s `deployed_files`)
/// recorded as dependencies of their package, not as the project's own.
fn adopt_apm_deployed(cap: &mut crate::capture::Capture, project: &Path) -> Result<()> {
    let locked = crate::apm::read_project_lock(project)?;
    if locked.is_empty() {
        return Ok(());
    }
    let decls = crate::apm::declared(project).unwrap_or_default();
    for (id, l) in &locked {
        let Some(commit) = l.resolved_commit.clone().filter(|c| plain_token(c)) else { continue };
        // the declaration it came from, so the package's source is what apm.yml says
        let decl = decls.iter().find(|d| matches!(d.source(), Ok(Source::Git(g)) if crate::apm::source_identity(&g) == *id));
        let Some(Ok(Source::Git(g))) = decl.map(|d| d.source()) else { continue };
        let name = decl.map(|d| d.name.clone()).unwrap_or_else(|| g.package_name());
        let skills: Vec<usize> = cap
            .lock
            .skill
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.dep.declared && s.path.as_deref().is_some_and(|p| l.deployed_files.iter().any(|f| f.trim_end_matches('/') == p.trim_end_matches('/'))))
            .map(|(i, _)| i)
            .collect();
        if skills.is_empty() {
            continue;
        }
        // where each skill is in the package: fetch it (through the cache) to find out
        let content = fetch_git(project, &g, &commit, l.resolved_ref.as_deref(), None)?.1;
        let prims = crate::apm::primitives(&content, &name);
        for i in skills {
            let s = &mut cap.lock.skill[i];
            let Some(prim) = prims.iter().find(|p| p.kind == "skill" && p.name == s.name) else { continue };
            let sub = [g.subpath.clone().unwrap_or_default(), prim.rel.clone()].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("/");
            s.origin = "apm".into();
            s.source = Some(if sub.is_empty() { format!("git+{}", g.url) } else { format!("git+{}#path={sub}", g.url) });
            s.dep = Declared { declared: true, commit: Some(commit.clone()), resolved_ref: l.resolved_ref.clone(), resolved_tag: l.resolved_tag.clone(), required_by: vec![name.clone()], ..Default::default() };
            let v = s.vendored.take();
            drop_vendored(cap, v.as_deref());
        }
        if !cap.lock.package.iter().any(|p| p.name == name) {
            cap.lock.package.push(Package {
                name: name.clone(),
                source: decl.map(|d| d.spec.clone()).unwrap_or_default(),
                digest: l.content_hash.clone(),
                depth: l.depth.unwrap_or(1),
                dep: Declared { declared: true, constraint: l.constraint.clone(), resolved_tag: l.resolved_tag.clone(), resolved_ref: l.resolved_ref.clone(), commit: Some(commit.clone()), ..Default::default() },
                ..Default::default()
            });
            cap.lock.provenance.openapm = Some(crate::apm::OPENAPM_VERSION.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(spec: &str, kind: Kind) -> GitSource {
        match parse(spec, kind).unwrap() {
            Source::Git(g) => g,
            other => panic!("{spec}: {other:?}"),
        }
    }

    #[test]
    fn github_shorthand_with_subpaths_refs_and_ranges() {
        let s = g("github:anthropics/skills/skills/pdf", Kind::Skill);
        assert_eq!((s.url.as_str(), s.subpath.as_deref(), s.reference.as_deref(), s.constraint.as_deref()), ("https://github.com/anthropics/skills.git", Some("skills/pdf"), None, None));
        let s = g("github:acme/skills/skills/sql@^1.2", Kind::Skill);
        assert_eq!((s.subpath.as_deref(), s.constraint.as_deref()), (Some("skills/sql"), Some("^1.2")));
        let s = g("github:acme/skills/skills/sql@v1.3.0", Kind::Skill);
        assert_eq!((s.reference.as_deref(), s.constraint), (Some("v1.3.0"), None));
        let s = g("github:acme/rules/style.instructions.md", Kind::Instructions);
        assert!(s.is_file() && s.subpath.as_deref() == Some("style.instructions.md"));
        assert_eq!(g("github:acme/skills//skills/sql@main", Kind::Skill).subpath.as_deref(), Some("skills/sql"));
        assert!(parse("github:acme", Kind::Skill).is_err());
    }

    #[test]
    fn git_urls_apm_packages_and_registries() {
        let s = g("git+https://gitlab.com/acme/skills#path=sql&tag=v1.2.0", Kind::Skill);
        assert_eq!((s.url.as_str(), s.subpath.as_deref(), s.reference.as_deref()), ("https://gitlab.com/acme/skills", Some("sql"), Some("v1.2.0")));
        // what `pin` records for a skill checkout
        let s = g("git+https://github.com/anthropics/skills@9b1e2f0#path=skills/pdf", Kind::Skill);
        assert_eq!((s.url.as_str(), s.reference.as_deref(), s.subpath.as_deref()), ("https://github.com/anthropics/skills", Some("9b1e2f0"), Some("skills/pdf")));
        let s = g("git+https://h/a/b.git#version=^2", Kind::Skill);
        assert_eq!(s.constraint.as_deref(), Some("^2"));
        let s = g("apm:github/awesome-copilot/plugins/context-engineering#v2", Kind::Package);
        assert!(s.apm && s.url.ends_with("/github/awesome-copilot.git") && s.reference.as_deref() == Some("v2"));
        assert_eq!(s.subpath.as_deref(), Some("plugins/context-engineering"));
        let s = g("apm:gitlab.com/acme/pkgs/sub#^1.0", Kind::Package);
        assert_eq!((s.url.as_str(), s.subpath.as_deref(), s.constraint.as_deref()), ("https://gitlab.com/acme/pkgs.git", Some("sub"), Some("^1.0")));
        assert_eq!(parse("npm:@modelcontextprotocol/server-github@^1", Kind::Mcp).unwrap(), Source::Npm { package: "@modelcontextprotocol/server-github".into(), constraint: Some("^1".into()) });
        assert_eq!(parse("pypi:mcp-server-fetch==2025.1.1", Kind::Mcp).unwrap(), Source::Pypi { package: "mcp-server-fetch".into(), constraint: Some("=2025.1.1".into()) });
        assert_eq!(parse("https://mcp.linear.app/mcp", Kind::Mcp).unwrap(), Source::Remote { url: "https://mcp.linear.app/mcp".into() });
        assert_eq!(parse("plugin:acme-marketplace/acme-tools", Kind::Plugin).unwrap(), Source::Plugin { marketplace: "acme-marketplace".into(), name: "acme-tools".into() });
        assert_eq!(parse("./skills/sql", Kind::Skill).unwrap(), Source::Path { path: "./skills/sql".into(), editable: false });
        // a browser URL for a skill is a repository
        let s = g("https://github.com/acme/skills/tree/main/skills/sql", Kind::Skill);
        assert_eq!((s.reference.as_deref(), s.subpath.as_deref()), (Some("main"), Some("skills/sql")));
    }

    #[test]
    fn unsafe_sources_are_refused() {
        for spec in ["github:a/b@--upload-pack=x", "github:a/b//../../etc", "git+ext::sh -c x", "npm:-rf", "plugin:a/b/c", "git+https://h/a#path=../x", "github:a/b@^1.2.3.4.5"] {
            assert!(matches!(parse(spec, Kind::Skill), Err(Error::Dependency(_))), "{spec}");
        }
        let e = parse("https://u:ghp_secret@github.com/a/b", Kind::Skill).unwrap();
        assert!(!serde_json::to_string(&e).unwrap().contains("ghp_"));
        // a remote server's URL is committed as it is: one with a credential is refused
        for url in ["https://mcp.example.com/mcp?api_key=sk-ant-api03-0123456789abcdefABCDEF", "https://bob:pw@mcp.example.com/mcp"] {
            assert!(parse(url, Kind::Mcp).unwrap_err().to_string().contains("credential"), "{url}");
        }
        assert!(parse("https://mcp.example.com/mcp?region=eu", Kind::Mcp).is_ok());
    }

    #[test]
    fn names() {
        let n = |s: &str, k: Kind| default_name(&parse(s, k).unwrap());
        assert_eq!(n("github:anthropics/skills/skills/pdf", Kind::Skill), "pdf");
        assert_eq!(n("github:acme/rules/style.instructions.md", Kind::Instructions), "style");
        assert_eq!(n("github:acme/skills/sql/SKILL.md", Kind::Skill), "sql");
        assert_eq!(n("github:acme/one-skill", Kind::Skill), "one-skill");
        assert_eq!(n("npm:@modelcontextprotocol/server-github", Kind::Mcp), "github");
        assert_eq!(n("pypi:mcp-server-fetch", Kind::Mcp), "fetch");
        assert_eq!(n("https://mcp.linear.app/mcp", Kind::Mcp), "linear");
        assert_eq!(n("apm:github/awesome-copilot/plugins/context-engineering", Kind::Package), "context-engineering");
    }

    #[test]
    fn version_ranges_read_like_npm() {
        let v = |s: &str| semver::Version::parse(s).unwrap();
        let r = |s: &str| VersionRange::parse(s).unwrap();
        assert!(r("^1.2").matches(&v("1.9.0")) && !r("^1.2").matches(&v("2.0.0")) && !r("^1.2").matches(&v("1.1.0")));
        assert!(r("~1.2.3").matches(&v("1.2.9")) && !r("~1.2.3").matches(&v("1.3.0")));
        assert!(r(">=1.0 <2").matches(&v("1.5.0")) && !r(">=1.0 <2").matches(&v("2.0.0")));
        assert!(r(">= 1.0, < 2").matches(&v("1.5.0")));
        assert!(r("1.x").matches(&v("1.4.2")) && !r("1.x").matches(&v("2.0.0")));
        assert!(r("1.2.0").matches(&v("1.2.0")) && !r("1.2.0").matches(&v("1.2.1")), "a bare version is exact");
        assert!(r("1.2 - 1.4").matches(&v("1.3.0")) && !r("1.2 - 1.4").matches(&v("1.5.0")));
        assert!(r("^1 || ^3").matches(&v("3.1.0")) && !r("^1 || ^3").matches(&v("2.0.0")));
        assert!(r("*").matches(&v("0.1.0")));
        assert!(is_constraint("^1.2") && is_constraint("1.2.0") && is_constraint("1") && is_constraint(">=1 <2"));
        assert!(!is_constraint("main") && !is_constraint("v1.2.0") && !is_constraint("9b1e2f0") && !is_constraint("1abc"));
        let best = r("^1.2").best(vec![(v("1.2.0"), "a"), (v("1.4.1"), "b"), (v("2.0.0"), "c")]).unwrap();
        assert_eq!(best.1, "b");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-deps-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One `HLOCK_HOME` for every test in this process (the cache lives under it).
    pub(super) fn test_home() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| std::env::set_var("HLOCK_HOME", std::env::temp_dir().join(format!("hlock-deps-home-{}", std::process::id()))));
    }

    pub(super) fn sh_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn refs_resolve_from_one_listing_and_trees_come_from_the_cache() {
        test_home();
        let d = tmp("git");
        let r = d.join("skills");
        write(&r.join("skills/sql/SKILL.md"), "---\nname: sql\ndescription: d\n---\nv1\n");
        write(&r.join("style.md"), "be terse\n");
        sh_git(&r, &["init", "-q", "-b", "main"]);
        sh_git(&r, &["add", "-A"]);
        sh_git(&r, &["commit", "-q", "-m", "one"]);
        sh_git(&r, &["tag", "v1.2.0"]);
        let c1 = sh_git(&r, &["rev-parse", "HEAD"]);
        write(&r.join("skills/sql/SKILL.md"), "---\nname: sql\ndescription: d\n---\nv2\n");
        sh_git(&r, &["commit", "-q", "-am", "two"]);
        sh_git(&r, &["tag", "-a", "v1.3.0", "-m", "annotated"]);
        sh_git(&r, &["tag", "sql--v2.1.0"]);
        sh_git(&r, &["tag", "other--v2.5.0"]);
        sh_git(&r, &["tag", "v2.0.0"]);
        let c2 = sh_git(&r, &["rev-parse", "HEAD"]);
        let url = format!("git+file://{}", r.display());

        let src = |spec: &str| match parse(spec, Kind::Skill).unwrap() {
            Source::Git(g) => g,
            _ => unreachable!(),
        };
        let g = src(&format!("{url}#path=skills/sql&version=^1.2"));
        let refs = ls_remote(&g).unwrap();
        assert_eq!(refs.default_branch.as_deref(), Some("main"));
        let p = pick(&g, &refs).unwrap();
        assert_eq!((p.commit.as_str(), p.resolved_tag.as_deref()), (c2.as_str(), Some("v1.3.0")), "the annotated tag is peeled to its commit");
        // `{name}--v{version}` tags count for the package named by the subpath
        let p = pick(&src(&format!("{url}#path=skills/sql&version=^2")), &refs).unwrap();
        assert_eq!(p.resolved_tag.as_deref(), Some("sql--v2.1.0"), "another package's tags don't count");
        assert!(pick(&src(&format!("{url}#version=^9")), &refs).unwrap_err().to_string().contains("no tag"));
        assert_eq!(pick(&src(&format!("{url}#tag=v1.2.0")), &refs).unwrap().commit, c1);
        assert_eq!(pick(&src(&url), &refs).unwrap(), Pick { commit: c2.clone(), resolved_ref: Some("main".into()), resolved_tag: None });

        let project = d.join("project");
        let (d1, p1) = fetch_git(&project, &g, &c1, Some("v1.2.0"), None).unwrap();
        assert!(p1.join("SKILL.md").is_file() && std::fs::read_to_string(p1.join("SKILL.md")).unwrap().contains("v1"));
        assert_eq!(d1, crate::capture::tree_digest(&p1));
        let (d2, _) = fetch_git(&project, &g, &c2, None, None).unwrap();
        assert_ne!(d1, d2);
        // a single file is stored by its bytes' digest
        let f = src(&format!("{url}#path=style.md"));
        let (fd, fp) = fetch_git(&project, &f, &c2, None, None).unwrap();
        assert_eq!((fd, std::fs::read_to_string(&fp).unwrap()), (crate::hash::sha256_bytes(b"be terse\n"), "be terse\n".to_string()));
        // with the remote gone, a stored digest needs nothing, and a cached commit needs no fetch
        std::fs::rename(&r, d.join("moved")).unwrap();
        assert_eq!(fetch_git(&project, &g, &c1, None, Some(&d1)).unwrap().1, p1);
        std::fs::remove_dir_all(trees_dir(&project)).unwrap();
        assert_eq!(fetch_git(&project, &g, &c1, None, Some(&d1)).unwrap().0, d1);
        // a digest that doesn't match the lock is an error, not a silent install
        std::fs::remove_dir_all(trees_dir(&project)).unwrap();
        let e = fetch_git(&project, &g, &c1, None, Some("sha256:00")).unwrap_err().to_string();
        assert!(e.contains("but the lock says"), "{e}");
        assert!(fetch_git(&project, &src(&format!("{url}#path=nope")), &c1, None, None).unwrap_err().to_string().contains("has no nope"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tags_name_versions() {
        assert_eq!(tag_version("v1.3.0", "sql"), lenient_version("1.3.0"));
        assert_eq!(tag_version("sql--v1.3.0", "sql"), lenient_version("1.3.0"));
        assert_eq!(tag_version("sql-v1.3.0", "sql"), lenient_version("1.3.0"));
        assert_eq!(tag_version("sql--v1.3", "sql"), None, "tags spell all three parts");
        assert_eq!(tag_version("other--v1.3.0", "sql"), None);
        assert_eq!(tag_version("1.3.0", "sql"), None);
        let tags: Vec<String> = ["1.3.0", "1.4.0", "latest"].map(String::from).to_vec();
        assert_eq!(tag_versions(&tags, "sql").len(), 2, "bare versions count when nothing else does");
        let tags: Vec<String> = ["1.3.0", "v1.2.0"].map(String::from).to_vec();
        assert_eq!(tag_versions(&tags, "sql").iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["v1.2.0"]);
        assert_eq!(lenient_version("2025.1.1").unwrap().major, 2025);
    }
}
