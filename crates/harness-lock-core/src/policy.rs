//! `hlock-policy.toml` (`specs/audit.md` §5): what a project or organization allows, for
//! sources, MCP servers, models, providers, harnesses and evidence.
//!
//! A policy comes from the project only: the file next to the lock (or in the project
//! directory), plus what it names with `extends`, pinned by hash. Inheritance only tightens:
//! every level's rules apply, so a child can deny more and require more, but can't allow what
//! a parent denies. Enforcement is the strictest any level asks for.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::deps::{Kind, Source};
use crate::error::{Error, Result};
use crate::lockfile::Lockfile;
use crate::profiles::Ctx;

/// The policy file's name.
pub const POLICY_FILE: &str = "hlock-policy.toml";
/// How many `extends` deep a policy may go.
const MAX_EXTENDS: usize = 5;

fn err(msg: impl Into<String>) -> Error {
    Error::Audit(msg.into())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Sources {
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
    /// Executables from these skip the trust prompt (§4.3).
    pub trusted: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Mcp {
    pub transports: Option<Vec<String>>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    pub providers: Option<Vec<String>>,
    /// `provider:model` patterns (`*:*-preview`).
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct HarnessRules {
    pub allow: Option<Vec<String>>,
    pub min_version: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct EvidenceRules {
    pub min_success_rate: Option<f64>,
    pub max_cost_usd_per_task: Option<f64>,
    pub max_age_days: Option<u64>,
}

/// One policy file, as written.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyFile {
    pub extends: Option<String>,
    /// `block`, `warn` or `off` (default `block`).
    pub enforcement: Option<String>,
    pub sources: Sources,
    pub mcp: Mcp,
    pub models: Models,
    pub harness: HarnessRules,
    pub evidence: EvidenceRules,
}

/// The policy in force: each level from the project's file up its `extends` chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Where each level came from, the project's first.
    pub files: Vec<String>,
    pub levels: Vec<PolicyFile>,
    /// The strictest enforcement any level sets: `block`, `warn` or `off`.
    pub enforcement: String,
}

/// A rule a lock breaks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// `sources`, `mcp`, `models`, `harness`, `evidence`.
    pub section: String,
    /// What breaks it: a source, a server, a model.
    pub subject: String,
    pub message: String,
    /// The policy file whose rule it breaks.
    pub file: String,
}

// --------------------------------------------------------------------------- //
// Patterns
// --------------------------------------------------------------------------- //

/// APM's pattern grammar: anchored at both ends; `*` matches any run without `/`, `**`
/// any run including it.
pub fn glob(pat: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => (0..=t.len()).any(|i| m(&p[2..], &t[i..])),
            Some(b'*') => (0..=t.len()).take_while(|&i| i == 0 || t[i - 1] != b'/').any(|i| m(&p[1..], &t[i..])),
            Some(c) => t.first() == Some(c) && m(&p[1..], &t[1..]),
        }
    }
    m(pat.as_bytes(), text.as_bytes())
}

/// Whether `pat` matches `subject`, or the repository or package `subject` is in (a
/// prefix at a `/`): `github:acme/*` matches `github:acme/skills/skills/sql`.
pub fn matches(pat: &str, subject: &str) -> bool {
    if glob(pat, subject) {
        return true;
    }
    subject.match_indices('/').any(|(i, _)| glob(pat, &subject[..i]))
}

/// What a source is called for policy: `github:OWNER/REPO[/PATH]`, `git:HOST/PATH`,
/// `npm:PKG`, `pypi:PKG`, `plugin:MARKET/NAME`, `path:…`, or the URL; never a ref.
/// Where an MCP server really comes from: its URL, or the package its command runs when that's
/// the source its lock entry names (`npx -y PKG` for `npm:PKG`). `None` for any other command.
pub fn mcp_source(t: &crate::lockfile::Tool) -> Option<String> {
    if let (Some(url), None) = (&t.url, &t.command) {
        return Some(url.clone());
    }
    let (cmd, src) = (t.command.as_deref()?, t.source.as_deref()?);
    crate::trust::runs_source(cmd, &t.args, src).then(|| src.to_string())
}

pub fn subject(spec: &str) -> Option<String> {
    if let Some(m) = spec.strip_prefix("marketplace:") {
        return Some(format!("plugin:{m}"));
    }
    if spec.starts_with("mcp-registry:") || spec.starts_with("apm-mcp:") {
        return Some(spec.to_string());
    }
    let kind = if spec.starts_with("npm:") || spec.starts_with("pypi:") || spec.starts_with("http") { Kind::Mcp } else { Kind::Skill };
    let src = crate::deps::parse(spec, kind).ok()?;
    Some(match src {
        Source::Git(g) => {
            let id = crate::apm::source_identity(&g);
            match id.strip_prefix("github.com/") {
                Some(rest) => format!("github:{rest}"),
                None => format!("git:{id}"),
            }
        }
        Source::Npm { package, .. } => format!("npm:{package}"),
        Source::Pypi { package, .. } => format!("pypi:{package}"),
        Source::Remote { url } => url,
        Source::Plugin { marketplace, name } => format!("plugin:{marketplace}/{name}"),
        Source::Path { path, .. } => format!("path:{path}"),
        Source::Defined { name, .. } => format!("apm-mcp:{name}"),
    })
}

// --------------------------------------------------------------------------- //
// Loading
// --------------------------------------------------------------------------- //

fn rank(e: &str) -> u8 {
    match e {
        "off" => 0,
        "warn" => 1,
        _ => 2,
    }
}

/// Parse a policy file's text.
pub fn parse(text: &str, origin: &str) -> Result<PolicyFile> {
    let p: PolicyFile = toml::from_str(text).map_err(|e| err(format!("{origin}: {e}")))?;
    if let Some(e) = &p.enforcement {
        if !matches!(e.as_str(), "block" | "warn" | "off") {
            return Err(err(format!("{origin}: enforcement {e:?} isn't block, warn or off")));
        }
    }
    for t in p.mcp.transports.iter().flatten() {
        if !matches!(t.as_str(), "stdio" | "http" | "sse") {
            return Err(err(format!("{origin}: [mcp] transport {t:?} isn't stdio, http or sse")));
        }
    }
    Ok(p)
}

/// The text `extends` names, checked against the hash it's pinned to: a path (relative to
/// the file that extends it), or a git source (`github:OWNER/REPO/PATH`), at the default
/// branch's head, `#sha256:HEX` either way.
/// Where a parent policy fetched from git is kept, by its pinned hash: it's checked against
/// the pin when read, so a policy isn't fetched again on every command.
fn extends_cache(pin: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HLOCK_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hlock")))?;
    pin.chars().all(|c| c.is_ascii_hexdigit()).then(|| home.join("cache").join("policy").join(format!("{}.toml", pin.to_ascii_lowercase())))
}

/// A parent policy's text, checked against its pin. `base` is the directory relative paths
/// resolve in: `None` for a policy fetched from git, which can only extend another git one.
fn fetch_extends(spec: &str, base: Option<&Path>) -> Result<(String, String)> {
    let (what, pin) = spec.rsplit_once("#sha256:").ok_or_else(|| err(format!("extends = {spec:?} must be pinned by hash: …#sha256:HEX, so a change upstream doesn't change what this repo allows")))?;
    let git = what.starts_with("github:") || what.starts_with("git") || what.starts_with("https://");
    let cache = extends_cache(pin).filter(|_| git);
    if let Some(text) = cache.as_ref().and_then(|c| std::fs::read_to_string(c).ok()).filter(|t| crate::hash::sha256_hex(t.as_bytes()).eq_ignore_ascii_case(pin)) {
        return Ok((what.to_string(), text));
    }
    let text = if git {
        let Source::Git(g) = crate::deps::parse(what, Kind::Instructions).map_err(|e| err(format!("extends: {e}")))? else {
            return Err(err(format!("extends = {what:?} isn't a git file")));
        };
        let file = g.subpath.clone().ok_or_else(|| err(format!("extends = {what:?} names no file in the repository")))?;
        let refs = crate::deps::ls_remote(&g)?;
        let pick = crate::deps::pick(&g, &refs)?;
        crate::deps::read_git_file(&g, &pick.commit, pick.resolved_ref.as_deref(), &file)?.ok_or_else(|| err(format!("{what} has no {file}")))?
    } else {
        let base = base.ok_or_else(|| err(format!("extends = {what:?}: a policy fetched from git can only extend another git policy (github:…), not a path")))?;
        let p = base.join(what);
        std::fs::read_to_string(&p).map_err(|e| err(format!("extends: {}: {e}", p.display())))?
    };
    let got = crate::hash::sha256_hex(text.as_bytes());
    if !got.eq_ignore_ascii_case(pin) {
        return Err(err(format!("extends = {what:?} has sha256:{got}, not the sha256:{pin} it's pinned to; review the change, then update the pin")));
    }
    if let Some(c) = cache {
        let _ = c.parent().map(std::fs::create_dir_all);
        let _ = crate::fsutil::write_atomic(&c, text.as_bytes());
    }
    Ok((what.to_string(), text))
}

impl Policy {
    /// The policy at `path` and up its `extends` chain.
    pub fn load(path: &Path) -> Result<Policy> {
        let text = std::fs::read_to_string(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
        let mut base = path.parent().map(Path::to_path_buf);
        let mut levels = vec![parse(&text, &path.display().to_string())?];
        // the project's own file, by name: it's next to the lock
        let mut files = vec![path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())];
        while let Some(ext) = levels.last().and_then(|l| l.extends.clone()) {
            if levels.len() > MAX_EXTENDS {
                return Err(err(format!("{} extends more than {MAX_EXTENDS} policies deep", path.display())));
            }
            let (what, text) = fetch_extends(&ext, base.as_deref())?;
            // a path a fetched policy extends is relative to that policy, which isn't on disk
            let git = what.starts_with("github:") || what.starts_with("git") || what.starts_with("https://");
            base = if git { None } else { base.map(|b| b.join(&what).parent().map(Path::to_path_buf).unwrap_or(b)) };
            if files.contains(&what) {
                return Err(err(format!("{} extends itself through {what}", path.display())));
            }
            levels.push(parse(&text, &what)?);
            files.push(what);
        }
        let enforcement = levels.iter().map(|l| l.enforcement.clone().unwrap_or_else(|| "block".into())).max_by_key(|e| rank(e)).unwrap_or_else(|| "block".into());
        Ok(Policy { files, levels, enforcement })
    }

    /// The project's policy: `hlock-policy.toml` next to the lock, else in the project
    /// directory. `None` when there's none.
    pub fn find(lock_path: &Path, ctx: &Ctx) -> Option<PathBuf> {
        let dir = crate::sync::lock_dir_of(lock_path, ctx);
        [dir.join(POLICY_FILE), ctx.cwd.join(POLICY_FILE)].into_iter().find(|p| p.is_file())
    }

    /// The policy for a lock (an empty one when the project has none).
    pub fn for_lock(lock_path: &Path, ctx: &Ctx) -> Result<Policy> {
        match Policy::find(lock_path, ctx) {
            Some(p) => Policy::load(&p),
            None => Ok(Policy::default()),
        }
    }

    /// The policy in a directory (an empty one when there's none).
    pub fn for_dir(dir: &Path) -> Result<Policy> {
        let p = dir.join(POLICY_FILE);
        if p.is_file() { Policy::load(&p) } else { Ok(Policy::default()) }
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// Whether violations stop a step (`enforcement = "block"`).
    pub fn blocks(&self) -> bool {
        !self.is_empty() && self.enforcement == "block"
    }

    /// Whether executables from `source` skip the trust prompt: every level that lists
    /// trusted sources lists one it matches, and none denies it.
    pub fn trusts(&self, source: &str) -> bool {
        let Some(sub) = subject(source) else { return false };
        let listing: Vec<&Vec<String>> = self.levels.iter().filter_map(|l| l.sources.trusted.as_ref()).collect();
        !listing.is_empty() && listing.iter().all(|t| t.iter().any(|p| matches(p, &sub))) && self.check_source(source).is_empty()
    }

    /// The rules a source breaks: a deny that matches it, or an allow list it isn't on.
    pub fn check_source(&self, source: &str) -> Vec<Violation> {
        let Some(sub) = subject(source) else { return Vec::new() };
        let mut out = Vec::new();
        for (l, file) in self.levels.iter().zip(&self.files) {
            if let Some(p) = l.sources.deny.iter().find(|p| matches(p, &sub)) {
                out.push(Violation { section: "sources".into(), subject: sub.clone(), message: format!("{sub} is denied ({p})"), file: file.clone() });
            } else if let Some(allow) = &l.sources.allow {
                if !allow.iter().any(|p| matches(p, &sub)) {
                    out.push(Violation { section: "sources".into(), subject: sub.clone(), message: format!("{sub} isn't in [sources] allow"), file: file.clone() });
                }
            }
        }
        out
    }

    /// The rules an MCP server breaks: its transport, or a deny on its source.
    pub fn check_mcp(&self, t: &crate::lockfile::Tool) -> Vec<Violation> {
        let mut out = Vec::new();
        let sub = mcp_source(t).as_deref().and_then(subject).unwrap_or_else(|| t.name.clone());
        // a deny matches what it runs or where it connects, and what the lock says it is
        let claimed = t.source.as_deref().and_then(subject);
        let transport = t.transport.clone().or_else(|| if t.url.is_some() { Some("http".into()) } else { t.command.as_ref().map(|_| "stdio".into()) });
        for (l, file) in self.levels.iter().zip(&self.files) {
            if let (Some(ts), Some(tr)) = (&l.mcp.transports, &transport) {
                if !ts.contains(tr) {
                    out.push(Violation { section: "mcp".into(), subject: t.name.clone(), message: format!("MCP server {} uses {tr}, which [mcp] transports doesn't allow", t.name), file: file.clone() });
                }
            }
            if let Some(p) = l.mcp.deny.iter().find(|p| matches(p, &sub) || claimed.as_deref().is_some_and(|c| matches(p, c))) {
                out.push(Violation { section: "mcp".into(), subject: t.name.clone(), message: format!("MCP server {} ({sub}) is denied ({p})", t.name), file: file.clone() });
            }
        }
        out
    }

    /// The rules a model breaks: its provider, or a deny on `provider:model`.
    pub fn check_model(&self, provider: &str, model: &str) -> Vec<Violation> {
        let mut out = Vec::new();
        let sub = format!("{provider}:{model}");
        for (l, file) in self.levels.iter().zip(&self.files) {
            if let Some(ps) = &l.models.providers {
                if !ps.iter().any(|p| p == provider) {
                    out.push(Violation { section: "models".into(), subject: sub.clone(), message: format!("{sub}: provider {provider} isn't in [models] providers"), file: file.clone() });
                }
            }
            if let Some(p) = l.models.deny.iter().find(|p| glob(p, &sub)) {
                out.push(Violation { section: "models".into(), subject: sub.clone(), message: format!("{sub} is denied ({p})"), file: file.clone() });
            }
        }
        out
    }

    /// The rules a harness breaks: not allowed, or older than a level's `min-version`.
    pub fn check_harness(&self, name: &str, version: Option<&str>) -> Vec<Violation> {
        let mut out = Vec::new();
        for (l, file) in self.levels.iter().zip(&self.files) {
            if let Some(a) = &l.harness.allow {
                if !a.iter().any(|h| h == name) {
                    out.push(Violation { section: "harness".into(), subject: name.into(), message: format!("harness {name} isn't in [harness] allow"), file: file.clone() });
                }
            }
            if let Some(min) = l.harness.min_version.get(name) {
                let (have, want) = (version.and_then(crate::deps::lenient_version), crate::deps::lenient_version(min));
                match (have, want) {
                    (Some(h), Some(w)) if h < w => out.push(Violation { section: "harness".into(), subject: name.into(), message: format!("{name} {} is older than [harness] min-version {min}", version.unwrap_or("")), file: file.clone() }),
                    (None, Some(_)) => out.push(Violation { section: "harness".into(), subject: name.into(), message: format!("{name}'s version isn't known, and [harness] min-version is {min}"), file: file.clone() }),
                    _ => {}
                }
            }
        }
        out
    }

    /// The rules a resolved lock's evidence breaks (a pinned lock has none to check).
    pub fn check_evidence(&self, lock: &Lockfile, today_days: i64) -> Vec<Violation> {
        let mut out = Vec::new();
        if lock.evidence.trials == 0 {
            return out;
        }
        let sr = lock.evidence.success_rate.get("mean").copied();
        let cost = lock.evidence.cost_usd_per_task.get("mean").copied();
        for (l, file) in self.levels.iter().zip(&self.files) {
            let e = &l.evidence;
            if let (Some(min), Some(sr)) = (e.min_success_rate, sr) {
                if sr < min {
                    out.push(Violation { section: "evidence".into(), subject: "success-rate".into(), message: format!("success rate {sr:.2} is below [evidence] min-success-rate {min}"), file: file.clone() });
                }
            }
            if let (Some(max), Some(c)) = (e.max_cost_usd_per_task, cost) {
                if c > max {
                    out.push(Violation { section: "evidence".into(), subject: "cost".into(), message: format!("cost ${c:.4}/task is above [evidence] max-cost-usd-per-task {max}"), file: file.clone() });
                }
            }
            if let Some(max) = e.max_age_days {
                match days_of(&lock.generated_at) {
                    Some(d) if today_days - d > max as i64 => out.push(Violation { section: "evidence".into(), subject: "age".into(), message: format!("the evidence is {} days old, more than [evidence] max-age-days {max}; re-measure with hlock resolve", today_days - d), file: file.clone() }),
                    None => out.push(Violation { section: "evidence".into(), subject: "age".into(), message: format!("the lock doesn't say when it was measured, and [evidence] max-age-days is {max}"), file: file.clone() }),
                    _ => {}
                }
            }
        }
        out
    }

    /// Every rule `lock` breaks: its sources, MCP servers, models and deciders, harness,
    /// and evidence.
    pub fn check_lock(&self, lock: &Lockfile) -> Vec<Violation> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for s in &lock.skill {
            if let Some(src) = &s.source {
                out.extend(self.check_source(src));
            }
        }
        for i in &lock.instructions {
            if let Some(src) = &i.source {
                out.extend(self.check_source(src));
            }
        }
        for t in lock.tool.iter().filter(|t| t.kind == "mcp") {
            // what it really runs or connects to; a claimed source is checked against a deny too
            match mcp_source(t) {
                Some(src) => out.extend(self.check_source(&src)),
                None => {
                    for (_, file) in self.levels.iter().zip(&self.files).filter(|(l, _)| l.sources.allow.is_some()) {
                        out.push(Violation { section: "sources".into(), subject: t.name.clone(), message: format!("MCP server {} runs {:?}, which isn't a package [sources] allow can check (npx PKG, uvx PKG, or a URL)", t.name, t.command.as_deref().unwrap_or("")), file: file.clone() });
                    }
                }
            }
            if let Some(src) = &t.source {
                out.extend(self.check_source(src).into_iter().filter(|v| v.message.contains("denied")));
            }
            out.extend(self.check_mcp(t));
        }
        for n in &lock.native {
            if let Some(src) = &n.source {
                out.extend(self.check_source(src));
            }
        }
        for p in &lock.package {
            out.extend(self.check_source(&p.source));
        }
        for l in &lock.llm {
            out.extend(self.check_model(&l.provider, &l.model));
        }
        for d in &lock.decider {
            out.extend(self.check_model(&d.provider, &d.model));
        }
        if !lock.harness.name.is_empty() {
            out.extend(self.check_harness(&lock.harness.name, lock.harness.version.as_deref()));
        }
        out.extend(self.check_evidence(lock, today()));
        out.dedup();
        out
    }

    /// Stop with the violations when the policy blocks; otherwise return them as warnings.
    pub fn enforce(&self, violations: Vec<Violation>, what: &str) -> Result<Vec<String>> {
        if violations.is_empty() {
            return Ok(Vec::new());
        }
        let lines: Vec<String> = violations.iter().map(|v| format!("  {} ({})", v.message, v.file)).collect();
        if self.blocks() {
            // the strictest level wins: `warn` here can't relax a parent that blocks
            let hint = match self.files.iter().zip(&self.levels).skip(1).find(|(_, l)| l.enforcement.as_deref().unwrap_or("block") == "block") {
                Some((parent, _)) => format!("change the lock (the policy it extends, {parent}, blocks, so it can't be relaxed here)"),
                None => "change the lock, or the policy (enforcement = \"warn\" to only warn)".into(),
            };
            return Err(err(format!("{what} breaks the project's policy:\n{}\n{hint}", lines.join("\n"))));
        }
        Ok(violations.iter().map(|v| format!("policy: {}", v.message)).collect())
    }
}

/// Days since 1970-01-01 of an RFC 3339 date (`2026-10-20T…`).
fn days_of(ts: &str) -> Option<i64> {
    let d = ts.get(..10)?;
    let mut it = d.split('-');
    let (y, m, day): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    // days from civil (Howard Hinnant's algorithm)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn today() -> i64 {
    days_of(&crate::pull::now_rfc3339()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_read_like_apm() {
        assert!(glob("github:acme/*", "github:acme/skills") && !glob("github:acme/*", "github:acme/skills/sql"));
        assert!(glob("github:acme/**", "github:acme/skills/sql"));
        assert!(matches("github:acme/*", "github:acme/skills/skills/sql"), "a repository's paths count as it");
        assert!(matches("github:*/awesome-*", "github:github/awesome-copilot/plugins/x"));
        assert!(!matches("github:acme/*", "github:acmecorp/skills"));
        assert!(glob("*:*-preview", "openai:gpt-6-preview") && !glob("*:*-preview", "openai:gpt-6"));
        assert!(matches("npm:@modelcontextprotocol/*", "npm:@modelcontextprotocol/server-github"));
    }

    #[test]
    fn subjects_drop_refs() {
        assert_eq!(subject("github:acme/skills/skills/sql@^1.2").as_deref(), Some("github:acme/skills/skills/sql"));
        assert_eq!(subject("git+https://github.com/anthropics/skills@9b1e2f0#path=skills/pdf").as_deref(), Some("github:anthropics/skills/skills/pdf"));
        assert_eq!(subject("npm:@modelcontextprotocol/server-github@^1").as_deref(), Some("npm:@modelcontextprotocol/server-github"));
        assert_eq!(subject("marketplace:acme-market").as_deref(), Some("plugin:acme-market"));
        assert_eq!(subject("apm:github/awesome-copilot#v2").as_deref(), Some("github:github/awesome-copilot"));
    }

    fn policy(texts: &[&str]) -> Policy {
        let levels: Vec<PolicyFile> = texts.iter().map(|t| parse(t, "p").unwrap()).collect();
        let enforcement = levels.iter().map(|l| l.enforcement.clone().unwrap_or_else(|| "block".into())).max_by_key(|e| rank(e)).unwrap();
        Policy { files: (0..texts.len()).map(|i| format!("level{i}")).collect(), levels, enforcement }
    }

    #[test]
    fn rules_tighten_down_the_chain() {
        let child = "enforcement = \"warn\"\n[sources]\nallow = [\"github:acme/*\", \"github:evil/*\"]\n";
        let parent = "enforcement = \"block\"\n[sources]\nallow = [\"github:acme/*\", \"npm:@modelcontextprotocol/*\"]\ndeny = [\"github:*/awesome-*\"]\ntrusted = [\"github:acme/*\"]\n";
        let p = policy(&[child, parent]);
        assert_eq!(p.enforcement, "block", "the stricter enforcement wins");
        assert!(p.check_source("github:acme/skills/skills/sql").is_empty());
        assert!(!p.check_source("github:evil/x").is_empty(), "a child can't allow what its parent doesn't");
        assert!(!p.check_source("github:acme/awesome-list").is_empty(), "the parent's deny holds");
        assert!(p.trusts("github:acme/skills") && !p.trusts("github:evil/x"));
        assert!(Policy::default().check_source("github:anyone/x").is_empty() && !Policy::default().trusts("github:acme/x"));
    }

    #[test]
    fn models_harnesses_mcp_and_evidence() {
        let p = policy(&["[mcp]\ntransports = [\"stdio\", \"http\"]\ndeny = [\"npm:*-filesystem-*\"]\n[models]\nproviders = [\"anthropic\", \"ollama\"]\ndeny = [\"*:*-preview\"]\n[harness]\nallow = [\"claude-code\", \"codex\"]\nmin-version = { claude-code = \"2.1\" }\n[evidence]\nmin-success-rate = 0.8\nmax-cost-usd-per-task = 0.5\nmax-age-days = 90\n"]);
        assert!(p.check_model("anthropic", "claude-opus").is_empty());
        assert_eq!(p.check_model("openai", "gpt-6").len(), 1);
        assert_eq!(p.check_model("anthropic", "x-preview").len(), 1);
        assert!(p.check_harness("claude-code", Some("2.1.3")).is_empty());
        assert_eq!(p.check_harness("claude-code", Some("2.0.9")).len(), 1);
        assert_eq!(p.check_harness("pi", Some("1.0.0")).len(), 1);
        let sse = crate::lockfile::Tool { name: "s".into(), kind: "mcp".into(), transport: Some("sse".into()), source: Some("npm:@x/server-filesystem-plus".into()), ..Default::default() };
        assert_eq!(p.check_mcp(&sse).len(), 1, "the transport; the deny pattern needs a match on a full segment");
        let fs = crate::lockfile::Tool { name: "f".into(), kind: "mcp".into(), transport: Some("stdio".into()), source: Some("npm:mcp-filesystem-x".into()), ..Default::default() };
        assert_eq!(p.check_mcp(&fs).len(), 1);
        let mut lock = Lockfile { generated_at: "2026-01-01T00:00:00Z".into(), ..Default::default() };
        lock.evidence.trials = 10;
        lock.evidence.success_rate.insert("mean".into(), 0.7);
        lock.evidence.cost_usd_per_task.insert("mean".into(), 0.6);
        let v = p.check_evidence(&lock, days_of("2026-10-20").unwrap());
        assert_eq!(v.iter().map(|x| x.subject.as_str()).collect::<Vec<_>>(), ["success-rate", "cost", "age"]);
        assert!(parse("[sources]\nalow = []\n", "p").is_err(), "a typo is an error, not an unenforced rule");
        assert_eq!(days_of("1970-01-02T00:00:00Z"), Some(1));
    }

    #[test]
    fn extends_is_pinned_by_hash() {
        let d = std::env::temp_dir().join(format!("hlock-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let parent = "[sources]\ndeny = [\"github:evil/*\"]\n";
        std::fs::write(d.join("org.toml"), parent).unwrap();
        let hex = crate::hash::sha256_hex(parent.as_bytes());
        std::fs::write(d.join(POLICY_FILE), format!("extends = \"org.toml#sha256:{hex}\"\nenforcement = \"warn\"\n")).unwrap();
        let p = Policy::load(&d.join(POLICY_FILE)).unwrap();
        assert_eq!((p.levels.len(), p.enforcement.as_str()), (2, "block"));
        assert!(!p.check_source("github:evil/x").is_empty());
        std::fs::write(d.join("org.toml"), "[sources]\n").unwrap();
        assert!(Policy::load(&d.join(POLICY_FILE)).unwrap_err().to_string().contains("not the sha256"), "a change upstream needs a new pin");
        std::fs::write(d.join(POLICY_FILE), "extends = \"org.toml\"\n").unwrap();
        assert!(Policy::load(&d.join(POLICY_FILE)).unwrap_err().to_string().contains("pinned by hash"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
