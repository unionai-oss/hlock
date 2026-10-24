//! The manifest: `harness.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Error, Result};
use crate::hash::sha256_bytes;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct Objective {
    pub primary: String,
    pub constraints: BTreeMap<String, String>,
    pub select: String,
}

/// `[budget]`. Keys are kebab-case like the rest of the manifest; the snake_case
/// spellings older docs and `hlock init` wrote still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(default)]
pub struct Budget {
    #[serde(alias = "max_spend_usd")]
    pub max_spend_usd: Option<f64>,
    #[serde(alias = "max_trials")]
    pub max_trials: Option<usize>,
    pub strategy: String,
    #[serde(alias = "random_seed")]
    pub random_seed: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_spend_usd: None,
            max_trials: None,
            strategy: "grid".into(),
            random_seed: 0,
        }
    }
}

/// The search strategies, objectives and select rules `resolve` supports.
pub const STRATEGIES: &[&str] = &["grid"];
pub const PRIMARIES: &[&str] = &["success-rate"];
pub const SELECTS: &[&str] = &["min-cost", "max-quality"];

/// The keys of the manifest's fixed-shape tables; anything else is reported by
/// [`Manifest::problems`]. Tables holding names (`[skills]`, `[harnesses]`, …) aren't
/// checked, and neither are the reserved `[context]` and `[enforce]`.
const TOP_KEYS: &[&str] = &["project", "evals", "tasks", "candidates", "tools", "skills", "instructions", "mcp", "plugins", "packages", "context", "objective", "budget", "enforce", "backend", "backends", "optimizers", "target", "harnesses", "deciders", "tune", "sandbox", "environment", "secrets", "workspace", "commands"];
const BUDGET_KEYS: &[&str] = &["max-spend-usd", "max-trials", "strategy", "random-seed", "max_spend_usd", "max_trials", "random_seed"];
const OBJECTIVE_KEYS: &[&str] = &["primary", "constraints", "select"];
const EVALS_KEYS: &[&str] = &["packs", "seeds", "splits", "judge"];
const SPLITS_KEYS: &[&str] = &["search"];
const CANDIDATES_KEYS: &[&str] = &["llm", "decision", "harness", "policy"];
const BACKEND_KEYS: &[&str] = &["target"];
const TARGET_KEYS: &[&str] = &["harness", "also"];
const TUNE_KEYS: &[&str] = &["optimizer", "edit", "proposer", "budget", "trials", "alpha", "max-cost-increase"];
const TUNE_BUDGET_KEYS: &[&str] = &["rollouts", "usd"];
const ENVIRONMENT_KEYS: &[&str] = &["apt", "brew", "pip", "npm", "cargo", "go", "binaries", "ignore"];
const BINARY_KEYS: &[&str] = &["url", "sha256", "path"];
const SECRET_KEYS: &[&str] = &["name", "used-by", "optional", "description"];
const WORKSPACE_KEYS: &[&str] = &["repos", "files"];
const REPO_KEYS: &[&str] = &["url", "ref", "path"];
const SEED_FILE_KEYS: &[&str] = &["path", "target"];

/// `[environment]`: what the agent needs installed, as package lists for each installer
/// and binaries pinned by URL and SHA-256. The system installers say the platform (`apt`
/// for Debian and Ubuntu, `brew` for macOS); the language ones apply everywhere. The lock
/// records it as declared; nothing installs it yet.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Environment {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub apt: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub brew: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pip: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub npm: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cargo: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub go: Vec<String>,
    /// `[[environment.binaries]]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub binaries: Vec<Binary>,
    /// Programs the stack names that it doesn't need installed (`hlock env suggest`
    /// stops suggesting them): `docker`, when a server that runs it is turned off.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

/// A file downloaded from `url`, checked against `sha256`, and placed at `path`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Binary {
    pub url: String,
    /// 64 lowercase hex digits.
    pub sha256: String,
    /// Where it's installed: an absolute path (`/usr/local/bin/tool`).
    pub path: String,
}

impl Environment {
    pub fn is_empty(&self) -> bool {
        *self == Environment::default()
    }

    /// The package lists, by installer.
    pub fn lists(&self) -> [(&'static str, &Vec<String>); 6] {
        [("apt", &self.apt), ("brew", &self.brew), ("pip", &self.pip), ("npm", &self.npm), ("cargo", &self.cargo), ("go", &self.go)]
    }

    /// The list for an installer, to add to.
    pub fn list_mut(&mut self, installer: &str) -> Option<&mut Vec<String>> {
        Some(match installer {
            "apt" => &mut self.apt,
            "brew" => &mut self.brew,
            "pip" => &mut self.pip,
            "npm" => &mut self.npm,
            "cargo" => &mut self.cargo,
            "go" => &mut self.go,
            _ => return None,
        })
    }

    /// Values that would reach an installer as something other than what they say: a
    /// package that's an option or several words, a binary without a URL, SHA-256 or
    /// absolute path. The manifest and the lock both check it.
    pub fn check(&self) -> std::result::Result<(), String> {
        for (list, items) in self.lists() {
            for p in items {
                if p.is_empty() || p.starts_with('-') || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!("[environment] {list} entry {p:?} must be one package: no spaces, not starting with '-'"));
                }
            }
        }
        if let Some(p) = self.ignore.iter().find(|p| !crate::fsutil::plain_name(p) || p.starts_with('-') || p.chars().any(char::is_whitespace)) {
            return Err(format!("[environment] ignore entry {p:?} must be a program name (no '/', spaces, or leading '-')"));
        }
        let mut paths = std::collections::BTreeSet::new();
        for (i, b) in self.binaries.iter().enumerate() {
            let what = if b.path.is_empty() { format!("[[environment.binaries]] #{}", i + 1) } else { format!("[[environment.binaries]] {}", b.path) };
            for (k, v) in [("url", &b.url), ("sha256", &b.sha256), ("path", &b.path)] {
                if v.is_empty() {
                    return Err(format!("{what} needs `{k}` (each binary has `url`, `sha256` and `path`)"));
                }
            }
            if !(b.url.starts_with("https://") || b.url.starts_with("http://")) || b.url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("{what} url {:?} must be an http(s) URL", b.url));
            }
            if !is_sha256_hex(&b.sha256) {
                return Err(format!("{what} sha256 {:?} must be 64 lowercase hex digits", b.sha256));
            }
            let p = Path::new(&b.path);
            if !b.path.starts_with('/') || b.path.chars().any(char::is_control) || !p.components().all(|c| matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_))) || p.file_name().is_none() {
                return Err(format!("{what} path {:?} must be an absolute file path, with no '..'", b.path));
            }
            if !paths.insert(&b.path) {
                return Err(format!("{what} is declared twice"));
            }
        }
        Ok(())
    }
}

/// `s` is a SHA-256 digest as 64 lowercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// A `[[secrets]]` entry: a secret the agent needs, declared up front. Values are never
/// stored; the lock's `[[secret]]` records the name, what uses it, and whether it's
/// optional.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct DeclaredSecret {
    /// `GITHUB_TOKEN`, supplied as `HLOCK_SECRET_GITHUB_TOKEN` (either spelling works).
    pub name: String,
    /// What needs it: `mcp:github`, `skill:release`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl DeclaredSecret {
    /// The variable that supplies it, as the lock names it (`HLOCK_SECRET_…`).
    pub fn full_name(&self) -> String {
        crate::secrets::full_name(&self.name)
    }
}

/// `[workspace]`: the state the agent's workspace starts from, as repositories to check
/// out and files from the package to copy in. The lock records it, with each file's
/// digest; the live state that grows from it is never locked.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Workspace {
    /// `[[workspace.repos]]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<SeedRepo>,
    /// `[[workspace.files]]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<SeedFile>,
}

/// A repository checked out at `path` in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct SeedRepo {
    pub url: String,
    /// A branch, tag or commit; the default branch when there's none.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Where it's checked out, relative to the workspace.
    pub path: String,
}

/// A file or directory in the package (`path`), copied to `target` in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct SeedFile {
    /// Relative to `harness.toml`.
    pub path: String,
    /// Relative to the workspace.
    pub target: String,
    /// The content's digest: the lock records it, the manifest leaves it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl Workspace {
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty() && self.files.is_empty()
    }

    /// Values that would reach `git` or the filesystem as something other than what they
    /// say: a URL that's an option, a ref with spaces, a path outside its directory, two
    /// seeds at one place. The manifest and the lock both check it.
    pub fn check(&self) -> std::result::Result<(), String> {
        use crate::fsutil::{plain_token, safe_rel};
        let mut targets = std::collections::BTreeSet::new();
        for (i, r) in self.repos.iter().enumerate() {
            let what = if r.url.is_empty() { format!("[[workspace.repos]] #{}", i + 1) } else { format!("[[workspace.repos]] {}", r.url) };
            for (k, v) in [("url", &r.url), ("path", &r.path)] {
                if v.is_empty() {
                    return Err(format!("{what} needs `{k}` (each repo has `url` and `path`, and may have `ref`)"));
                }
            }
            let scheme = ["https://", "http://", "ssh://", "git@"].iter().any(|s| r.url.starts_with(s));
            if !scheme || r.url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("{what} url {:?} must be an https, http, ssh or git@ URL", r.url));
            }
            if let Some(rf) = r.reference.as_deref().filter(|rf| !plain_token(rf)) {
                return Err(format!("{what} ref {rf:?} must be a branch, tag or commit (letters, digits and ._:/@+- only, not starting with '-')"));
            }
            if !safe_rel(&r.path) {
                return Err(format!("{what} path {:?} must be a relative path inside the workspace (no leading '/', no '..')", r.path));
            }
            if !targets.insert(crate::fsutil::normalize(Path::new(&r.path))) {
                return Err(format!("{what}: {} is seeded twice", r.path));
            }
        }
        for (i, f) in self.files.iter().enumerate() {
            let what = if f.path.is_empty() { format!("[[workspace.files]] #{}", i + 1) } else { format!("[[workspace.files]] {}", f.path) };
            for (k, v) in [("path", &f.path), ("target", &f.target)] {
                if v.is_empty() {
                    return Err(format!("{what} needs `{k}` (each file has `path`, in the package, and `target`, in the workspace)"));
                }
            }
            if !safe_rel(&f.path) {
                return Err(format!("{what} path {:?} must be a relative path inside the package (no leading '/', no '..')", f.path));
            }
            if !safe_rel(&f.target) {
                return Err(format!("{what} target {:?} must be a relative path inside the workspace (no leading '/', no '..')", f.target));
            }
            if !targets.insert(crate::fsutil::normalize(Path::new(&f.target))) {
                return Err(format!("{what}: {} is seeded twice", f.target));
            }
            if let Some(d) = f.digest.as_deref().filter(|d| !d.strip_prefix("sha256:").is_some_and(is_sha256_hex)) {
                return Err(format!("{what} digest {d:?} must be sha256: and 64 lowercase hex digits"));
            }
        }
        Ok(())
    }
}

/// The user-profile modes `[context] profile` takes: the user's own memory and personal
/// config (`shared`), a copy of them that writes back nowhere (`forked`), or neither (`none`).
pub const PROFILE_MODES: &[&str] = &["shared", "forked", "none"];

/// A slash command the agent offers: `[commands]` names a file in the package; the lock's
/// `[[command]]` records it with its digest.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Command {
    pub name: String,
    /// Relative to `harness.toml`.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// `[project]` `display-name` and `avatar`: how registry listings and launch UIs show
/// the agent. The lock records it, with the avatar's digest.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct Persona {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// An image in the package, relative to `harness.toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_digest: Option<String>,
}

impl Persona {
    pub fn is_empty(&self) -> bool {
        *self == Persona::default()
    }
}

/// A command name: letters, digits, `-`, `_` and `.`, not starting with `-` or `.`.
fn command_name(s: &str) -> bool {
    !s.is_empty() && !s.starts_with(['-', '.']) && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// What a manifest declares that isn't searched or resolved, only recorded: the
/// environment, secrets, workspace seed, commands, profile mode and persona, with the
/// digests of the files they name. The lock's copy is [`Declarations::from_lock`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Declarations {
    pub environment: Environment,
    pub secrets: Vec<DeclaredSecret>,
    pub workspace: Workspace,
    pub commands: Vec<Command>,
    pub profile: Option<String>,
    pub persona: Persona,
}

impl Declarations {
    /// What a lock records as declared, to carry into another lock (`pin`).
    pub fn from_lock(l: &crate::lockfile::Lockfile) -> Declarations {
        Declarations {
            environment: l.environment.clone(),
            secrets: l
                .secret
                .iter()
                .filter(|s| s.declared)
                .map(|s| DeclaredSecret { name: s.name.clone(), used_by: s.used_by.clone(), optional: s.optional, description: s.description.clone() })
                .collect(),
            workspace: l.workspace.clone(),
            commands: l.command.clone(),
            profile: l.context.profile.clone(),
            persona: l.project.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Declarations::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Candidates {
    #[serde(default)]
    pub llm: BTreeMap<String, LLMCandidate>,
    #[serde(default)]
    pub decision: Vec<String>,
    #[serde(default, deserialize_with = "de_harness")]
    pub harness: Vec<HarnessCandidate>,
    #[serde(default)]
    pub policy: BTreeMap<String, Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LLMCandidate {
    /// Manifest candidates may spell this `any_of` (underscore) or `any-of`.
    #[serde(alias = "any-of")]
    pub any_of: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessCandidate {
    pub name: String,
    #[serde(default)]
    pub pattern: Vec<String>,
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl Default for HarnessCandidate {
    fn default() -> Self {
        HarnessCandidate {
            name: String::new(),
            pattern: Vec::new(),
            config: BTreeMap::new(),
        }
    }
}

/// Accept both `harness = ["reference"]` and `[[candidates.harness]]` tables.
fn de_harness<'de, D>(d: D) -> std::result::Result<Vec<HarnessCandidate>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(|item| match item {
                serde_json::Value::String(s) => Ok(HarnessCandidate {
                    name: s.clone(),
                    ..Default::default()
                }),
                other => serde_json::from_value(other.clone()).map_err(D::Error::custom),
            })
            .collect(),
        serde_json::Value::Null => Ok(vec![]),
        other => Err(D::Error::custom(format!("`harness` must be a list of names or [[candidates.harness]] tables, not {other}"))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Manifest {
    #[serde(default)]
    pub project: BTreeMap<String, serde_json::Value>,
    /// `[evals]`: the eval packs to measure against. `[tasks]` is the old name.
    #[serde(default, alias = "tasks")]
    pub evals: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub candidates: Candidates,
    #[serde(default)]
    pub tools: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub skills: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub instructions: BTreeMap<String, serde_json::Value>,
    /// MCP servers the project declares (`[mcp]`): a source each, or a table with one.
    #[serde(default)]
    pub mcp: BTreeMap<String, serde_json::Value>,
    /// Harness plugins the project declares (`[plugins]`, `plugin:MARKETPLACE/NAME`).
    #[serde(default)]
    pub plugins: BTreeMap<String, serde_json::Value>,
    /// APM packages (`[packages]`, `apm:OWNER/REPO`): each brings its own skills,
    /// instructions and MCP servers, and the packages it depends on.
    #[serde(default)]
    pub packages: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub context: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub objective: Objective,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default, rename = "enforce")]
    pub enforce: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub backend: BTreeMap<String, serde_json::Value>,
    /// Backends the project defines or configures (`[backends.<name>]`): a `command` or
    /// a `python` object, and `options`. See [`crate::worker::Backend::resolve`].
    #[serde(default)]
    pub backends: BTreeMap<String, serde_json::Value>,
    /// Optimizers the project defines or configures (`[optimizers.<name>]`): a `command`
    /// or a `python` object, and `options`. See [`crate::optimize::Optimizer::resolve`].
    #[serde(default)]
    pub optimizers: BTreeMap<String, serde_json::Value>,
    /// The harness this lock is for (`[target] harness = "claude-code"`).
    #[serde(default)]
    pub target: BTreeMap<String, serde_json::Value>,
    /// Project-defined harnesses (`[harnesses.<id>]`): framework agents and custom scripts.
    #[serde(default)]
    pub harnesses: BTreeMap<String, serde_json::Value>,
    /// Decision models by what they decide (`[deciders.<use>]`).
    #[serde(default)]
    pub deciders: BTreeMap<String, serde_json::Value>,
    /// How `tune` edits the harness (`[tune]`): the optimizer, what it may edit, its budget.
    #[serde(default)]
    pub tune: BTreeMap<String, serde_json::Value>,
    /// The sandbox the lock runs in (`[sandbox]`), and project-defined runners.
    #[serde(default)]
    pub sandbox: BTreeMap<String, serde_json::Value>,
    /// What the agent needs installed (`[environment]`).
    #[serde(default, skip_serializing_if = "Environment::is_empty")]
    pub environment: Environment,
    /// Secrets the agent needs (`[[secrets]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<DeclaredSecret>,
    /// What the agent's workspace starts from (`[workspace]`).
    #[serde(default, skip_serializing_if = "Workspace::is_empty")]
    pub workspace: Workspace,
    /// Slash commands, by name, each a file in the package (`[commands]`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, String>,
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// The text this manifest was parsed from, which [`Manifest::manifest_hash`] hashes.
    #[serde(skip)]
    pub source_text: Option<String>,
}

/// The nearest `harness.toml` at or above `start`, like Cargo finds `Cargo.toml`.
pub fn find_manifest(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(crate::MANIFEST_PATH);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Manifest(format!("cannot read {}: {e}", path.display())))?;
        let mut m = Manifest::from_str(&text)?;
        // a bad [deciders] table is an error here, not a silent fallback later
        m.deciders()?;
        m.check_declared()?;
        m.source_path = Some(path.to_path_buf());
        Ok(m)
    }

    /// The project directory: where the manifest lives (else the working directory).
    /// Relative paths in the manifest (eval packs, instructions) resolve against it.
    pub fn root(&self) -> PathBuf {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match self.source_path.as_ref().and_then(|p| p.parent()) {
            Some(d) if d.as_os_str().is_empty() => cwd,
            Some(d) if d.is_absolute() => d.to_path_buf(),
            Some(d) => cwd.join(d),
            None => cwd,
        }
    }

    /// Parse from an in-memory TOML string.
    pub fn from_str(text: &str) -> Result<Manifest> {
        let mut m: Manifest = toml::from_str(text).map_err(|e| Error::Manifest(format!("invalid harness.toml: {e}")))?;
        m.source_text = Some(text.to_string());
        Ok(m)
    }

    /// Problems that would make `resolve` or `tune` quietly do something other than
    /// what the manifest says: a key a fixed-shape table doesn't know (naming the closest
    /// one), and a `strategy`, `primary` or `select` that isn't supported.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(doc) = self.source_text.as_deref().and_then(|t| t.parse::<toml::Table>().ok()) {
            let mut report = |table: Option<&toml::Table>, known: &[&str], name: &str| {
                for key in table.into_iter().flat_map(|t| t.keys()).filter(|k| !known.contains(&k.as_str())) {
                    let hint = crate::tasks::closest(key, known).map(|k| format!(" (did you mean `{k}`?)")).unwrap_or_default();
                    out.push(format!("unknown key `{key}` in {name}{hint}"));
                }
            };
            let table = |path: &[&str]| path.iter().try_fold(&doc, |t, k| t.get(*k).and_then(toml::Value::as_table));
            report(Some(&doc), TOP_KEYS, "harness.toml");
            report(table(&["budget"]), BUDGET_KEYS, "[budget]");
            report(table(&["objective"]), OBJECTIVE_KEYS, "[objective]");
            report(table(&["evals"]).or(table(&["tasks"])), EVALS_KEYS, "[evals]");
            report(table(&["evals", "splits"]), SPLITS_KEYS, "[evals.splits]");
            report(table(&["candidates"]), CANDIDATES_KEYS, "[candidates]");
            report(table(&["backend"]), BACKEND_KEYS, "[backend]");
            for name in table(&["backends"]).into_iter().flat_map(|t| t.keys()) {
                report(table(&["backends", name]), crate::worker::BACKEND_DEF_KEYS, &format!("[backends.{name}]"));
            }
            for name in table(&["optimizers"]).into_iter().flat_map(|t| t.keys()) {
                report(table(&["optimizers", name]), crate::worker::BACKEND_DEF_KEYS, &format!("[optimizers.{name}]"));
            }
            report(table(&["target"]), TARGET_KEYS, "[target]");
            report(table(&["tune"]), TUNE_KEYS, "[tune]");
            report(table(&["tune", "budget"]), TUNE_BUDGET_KEYS, "[tune.budget]");
            report(table(&["environment"]), ENVIRONMENT_KEYS, "[environment]");
            let items = |path: &[&str], last: &str| -> Vec<&toml::Table> {
                let arr = match path {
                    [] => doc.get(last),
                    _ => table(path).and_then(|t| t.get(last)),
                };
                arr.and_then(toml::Value::as_array).into_iter().flatten().filter_map(toml::Value::as_table).collect()
            };
            for t in items(&["environment"], "binaries") {
                report(Some(t), BINARY_KEYS, "[[environment.binaries]]");
            }
            for t in items(&[], "secrets") {
                report(Some(t), SECRET_KEYS, "[[secrets]]");
            }
            report(table(&["workspace"]), WORKSPACE_KEYS, "[workspace]");
            for t in items(&["workspace"], "repos") {
                report(Some(t), REPO_KEYS, "[[workspace.repos]]");
            }
            for t in items(&["workspace"], "files") {
                report(Some(t), SEED_FILE_KEYS, "[[workspace.files]]");
            }
        }
        let mut supported = |key: &str, value: &str, allowed: &[&str]| {
            if !value.is_empty() && !allowed.contains(&value) {
                out.push(format!("{key} = {value:?} isn't supported (use {})", allowed.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>().join(" or ")));
            }
        };
        supported("[budget] strategy", &self.budget.strategy, STRATEGIES);
        supported("[objective] primary", &self.objective.primary, PRIMARIES);
        supported("[objective] select", &self.objective.select, SELECTS);
        out
    }

    /// The values in `[environment]`, `[[secrets]]`, `[workspace]`, `[commands]`,
    /// `[context] profile` and `[project]` `display-name` and `avatar` that can't be used:
    /// checked when the manifest is loaded, so a bad digest, name or path is an error
    /// rather than ignored.
    pub fn check_declared(&self) -> Result<()> {
        self.environment.check().map_err(Error::Manifest)?;
        check_secrets(&self.secrets, "[[secrets]]").map_err(Error::Manifest)?;
        if self.workspace.files.iter().any(|f| f.digest.is_some()) {
            return Err(Error::Manifest("[[workspace.files]] digest is what the lock records; leave it out of harness.toml".into()));
        }
        self.workspace.check().map_err(Error::Manifest)?;
        for (name, path) in &self.commands {
            check_command(name, path).map_err(Error::Manifest)?;
        }
        self.profile_mode()?;
        self.persona_decl()?;
        Ok(())
    }

    /// `[context] profile`: whose memory and personal config the agent starts from.
    pub fn profile_mode(&self) -> Result<Option<String>> {
        match self.context.get("profile") {
            None => Ok(None),
            Some(serde_json::Value::String(p)) if PROFILE_MODES.contains(&p.as_str()) => Ok(Some(p.clone())),
            Some(v) => Err(Error::Manifest(format!("[context] profile = {v} isn't one of {}", PROFILE_MODES.join(", ")))),
        }
    }

    /// `[project]` `display-name` and `avatar`, as written (no digest).
    fn persona_decl(&self) -> Result<Persona> {
        let s = |k: &str| -> Result<Option<String>> {
            match self.project.get(k) {
                None => Ok(None),
                Some(serde_json::Value::String(v)) => Ok(Some(v.clone())),
                Some(_) => Err(Error::Manifest(format!("[project] {k} must be a string"))),
            }
        };
        let p = Persona { display_name: s("display-name")?, avatar: s("avatar")?, avatar_digest: None };
        check_persona(&p).map_err(Error::Manifest)?;
        Ok(p)
    }

    /// What this manifest declares to record in a lock, with the digests of the workspace
    /// files, commands and avatar it names, read from [`Manifest::root`]. A file that's
    /// missing is an error.
    pub fn declarations(&self) -> Result<Declarations> {
        self.check_declared()?;
        let root = self.root();
        let digest = |what: &str, rel: &str| -> Result<String> {
            crate::deps::content_digest(&root.join(rel)).map_err(|e| Error::Manifest(format!("{what} {rel}: {e}")))
        };
        let mut workspace = self.workspace.clone();
        for f in &mut workspace.files {
            f.digest = Some(digest("[[workspace.files]]", &f.path)?);
        }
        let commands = self
            .commands
            .iter()
            .map(|(name, path)| Ok(Command { name: name.clone(), path: path.clone(), digest: Some(digest(&format!("[commands] {name}"), path)?) }))
            .collect::<Result<Vec<_>>>()?;
        let mut persona = self.persona_decl()?;
        if let Some(a) = &persona.avatar {
            persona.avatar_digest = Some(digest("[project] avatar", a)?);
        }
        Ok(Declarations { environment: self.environment.clone(), secrets: self.secrets.clone(), workspace, commands, profile: self.profile_mode()?, persona })
    }

    /// [`Manifest::problems`] as an error, for the commands that search.
    pub fn check_search(&self) -> Result<()> {
        let p = self.problems();
        if p.is_empty() {
            return Ok(());
        }
        Err(Error::Manifest(format!("harness.toml: {}", p.join("; "))))
    }

    pub fn name(&self) -> String {
        self.project
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "agent".to_string())
    }

    pub fn eval_packs(&self) -> Vec<String> {
        self.evals
            .get("packs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[deprecated(note = "renamed to `eval_packs`")]
    pub fn task_packs(&self) -> Vec<String> {
        self.eval_packs()
    }

    pub fn seeds(&self) -> u64 {
        self.evals
            .get("seeds")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
    }

    /// `[evals.judge]`: the model that scores `judge` graders, over any pack's own.
    /// An invalid table reads as unset; [`Manifest::judge_checked`] reports it.
    pub fn judge(&self) -> Option<crate::graders::JudgeConfig> {
        self.judge_checked().ok().flatten()
    }

    /// [`Manifest::judge`], with an error for an invalid `[evals.judge]` table.
    pub fn judge_checked(&self) -> Result<Option<crate::graders::JudgeConfig>> {
        match self.evals.get("judge") {
            None => Ok(None),
            Some(v) => {
                let j: crate::graders::JudgeConfig = serde_json::from_value(v.clone()).map_err(|e| Error::Manifest(format!("[evals.judge]: {e}")))?;
                Ok(j.is_set().then_some(j))
            }
        }
    }

    /// `[deciders.<use>]`, checked, with defaults filled in.
    /// The `[sandbox]` table as a spec (`None` without one, or with `runner = "none"`).
    pub fn sandbox_spec(&self) -> Result<Option<crate::sandbox::Spec>> {
        if self.sandbox.is_empty() {
            return Ok(None);
        }
        crate::sandbox::Spec::from_manifest(Some(&serde_json::json!(self.sandbox)), &self.root())
    }

    /// The `[sandbox]` table, for runner lookup.
    pub fn sandbox_table(&self) -> Option<serde_json::Value> {
        (!self.sandbox.is_empty()).then(|| serde_json::json!(self.sandbox))
    }

    pub fn deciders(&self) -> Result<BTreeMap<String, crate::decide::DeciderSpec>> {
        self.deciders
            .iter()
            .map(|(use_, v)| {
                let spec: crate::decide::DeciderSpec = serde_json::from_value(v.clone()).map_err(|e| Error::Manifest(format!("[deciders.{use_}]: {e}")))?;
                Ok((use_.clone(), spec.validated(use_).map_err(Error::Manifest)?))
            })
            .collect()
    }

    /// The judge `[deciders.judge]`, else `[deciders.default]`, names: used when
    /// neither `[evals.judge]` nor a pack's `[pack.judge]` is set.
    /// An invalid `[deciders]` table reads as unset; [`Manifest::default_judge_checked`]
    /// reports it.
    pub fn default_judge(&self) -> Option<crate::graders::JudgeConfig> {
        self.default_judge_checked().ok().flatten()
    }

    /// [`Manifest::default_judge`], with an error for an invalid `[deciders]` table. A
    /// `[deciders.judge]` with only `any-of` falls back to `[deciders.default]`'s model.
    pub fn default_judge_checked(&self) -> Result<Option<crate::graders::JudgeConfig>> {
        let specs = self.deciders()?;
        let Some(spec) = ["judge", "default"].iter().filter_map(|u| specs.get(*u)).find(|s| s.model.is_some()) else { return Ok(None) };
        Ok(Some(crate::graders::JudgeConfig {
            model: spec.model.clone().unwrap_or_default(),
            protocol: spec.protocol.clone(),
            base_url: spec.base_url.clone(),
            key_env: spec.key_env.clone(),
            long_context: spec.long_context,
            ..Default::default()
        }))
    }

    pub fn backend_target(&self) -> String {
        self.backend
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "offline".to_string())
    }

    /// `[target] harness`, the harness the lock is meant to run in.
    pub fn target_harness(&self) -> Option<String> {
        self.target
            .get("harness")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    pub fn search_frac(&self) -> f64 {
        self.evals
            .get("splits")
            .and_then(|v| v.get("search"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.7)
    }

    /// Hash of the dependency sections only (`[skills]`, `[mcp]`, `[instructions]`,
    /// `[plugins]`, `[packages]`, `[tools]`, and `[environment]`, `[[secrets]]`,
    /// `[workspace]`, `[commands]`, `[context] profile` and `[project]` `display-name` and
    /// `avatar` when present): what a pinned lock's `manifest-hash`
    /// records, so editing `[candidates]` in the same file doesn't invalidate a pin.
    pub fn deps_hash(&self) -> String {
        let mut sections = serde_json::json!({
            "skills": self.skills,
            "mcp": self.mcp,
            "instructions": self.instructions,
            "plugins": self.plugins,
            "packages": self.packages,
            "tools": self.tools,
        });
        // only when present, so a manifest without them hashes as it did before they existed
        if !self.environment.is_empty() {
            sections["environment"] = serde_json::json!(self.environment);
        }
        if !self.secrets.is_empty() {
            sections["secrets"] = serde_json::json!(self.secrets);
        }
        if !self.workspace.is_empty() {
            sections["workspace"] = serde_json::json!(self.workspace);
        }
        if !self.commands.is_empty() {
            sections["commands"] = serde_json::json!(self.commands);
        }
        // the profile mode and persona as declared (an unusable value hashes as absent:
        // loading the manifest refuses it anyway)
        if let Some(p) = self.profile_mode().ok().flatten() {
            sections["profile"] = serde_json::json!(p);
        }
        if let Some(p) = self.persona_decl().ok().filter(|p| !p.is_empty()) {
            sections["persona"] = serde_json::json!(p);
        }
        crate::hash::content_hash(&sections)
    }

    /// Hash of the manifest: of the text it was parsed from (so it matches what the run
    /// used, whether loaded or parsed), else of its TOML (built in code).
    pub fn manifest_hash(&self) -> String {
        if let Some(text) = &self.source_text {
            return sha256_bytes(text.as_bytes());
        }
        match toml::to_string(self) {
            Ok(s) => sha256_bytes(s.as_bytes()),
            Err(_) => String::new(),
        }
    }
}

/// A command's name and file: a plain name, and a path inside the package.
pub(crate) fn check_command(name: &str, path: &str) -> std::result::Result<(), String> {
    if !command_name(name) {
        return Err(format!("[commands] name {name:?} must be letters, digits, '-', '_' and '.', not starting with '-' or '.'"));
    }
    if !crate::fsutil::safe_rel(path) {
        return Err(format!("[commands] {name} path {path:?} must be a relative path inside the package (no leading '/', no '..')"));
    }
    Ok(())
}

/// A display name is one line of text; an avatar is a path inside the package.
pub(crate) fn check_persona(p: &Persona) -> std::result::Result<(), String> {
    if let Some(n) = p.display_name.as_deref().filter(|n| n.trim().is_empty() || n.chars().any(char::is_control)) {
        return Err(format!("[project] display-name {n:?} must be non-empty text on one line"));
    }
    if let Some(a) = p.avatar.as_deref().filter(|a| !crate::fsutil::safe_rel(a)) {
        return Err(format!("[project] avatar {a:?} must be a relative path inside the package (no leading '/', no '..')"));
    }
    if let Some(d) = p.avatar_digest.as_deref().filter(|d| !d.strip_prefix("sha256:").is_some_and(is_sha256_hex)) {
        return Err(format!("[project] avatar-digest {d:?} must be sha256: and 64 lowercase hex digits"));
    }
    Ok(())
}

/// Secret names are variable names (letters, digits and `_`), declared once each, and
/// what uses them is text on one line. `what` names the section in errors.
pub(crate) fn check_secrets(secrets: &[DeclaredSecret], what: &str) -> std::result::Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for s in secrets {
        if s.name.is_empty() {
            return Err(format!("{what} entry needs a `name`"));
        }
        if !s.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || crate::secrets::check_name(&s.full_name()).is_err() {
            return Err(format!("{what} name {:?} isn't a valid secret name (letters, digits and _ only)", s.name));
        }
        if !seen.insert(s.full_name()) {
            return Err(format!("{what} {} is declared twice", s.name));
        }
        if let Some(u) = s.used_by.iter().find(|u| u.trim().is_empty() || u.chars().any(char::is_control)) {
            return Err(format!("{what} {} used-by entry {u:?} must be non-empty text on one line", s.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_tables_are_errors_not_defaults() {
        let e = Manifest::from_str("[candidates]\nharness = \"reference\"\n").unwrap_err();
        assert!(e.to_string().contains("must be a list"), "{e}");
        let m = Manifest::from_str("[evals.judge]\nmodel = 3\n").unwrap();
        assert!(m.judge_checked().is_err() && m.judge().is_none());
        let m = Manifest::from_str("[deciders.judge]\nmodel = \"anthropic:x\"\n").unwrap();
        assert!(m.default_judge_checked().is_err() && m.default_judge().is_none());
        // a use with only any-of candidates falls back to [deciders.default]'s model
        let m = Manifest::from_str("[deciders.judge]\nany-of = [\"ollama:kev:4b\"]\n[deciders.default]\nmodel = \"typesafe:jev-1.13.0\"\n").unwrap();
        assert_eq!(m.default_judge_checked().unwrap().unwrap().model, "typesafe:jev-1.13.0");
    }

    const DECLARED: &str = r#"
[environment]
apt = ["protobuf-compiler", "ripgrep"]
pip = ["ruff==0.6.9"]
npm = ["pnpm@9"]
cargo = ["cargo-nextest"]
go = ["golang.org/x/tools/gopls@v0.16.2"]

[[environment.binaries]]
url = "https://example.com/tool-1.2.3-linux-x64"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
path = "/usr/local/bin/tool"

[[secrets]]
name = "GITHUB_TOKEN"
used-by = ["mcp:github", "skill:release"]
optional = false
description = "Opens PRs on the user's behalf."

[[secrets]]
name = "HLOCK_SECRET_SENTRY_DSN"
optional = true
"#;

    #[test]
    fn environment_and_secrets_parse_and_check() {
        let m = Manifest::from_str(DECLARED).unwrap();
        m.check_declared().unwrap();
        assert!(m.problems().is_empty(), "{:?}", m.problems());
        assert_eq!(m.environment.pip, ["ruff==0.6.9"]);
        assert_eq!(m.environment.binaries[0].path, "/usr/local/bin/tool");
        assert_eq!(m.secrets[0].used_by, ["mcp:github", "skill:release"]);
        assert_eq!(m.secrets[0].description.as_deref(), Some("Opens PRs on the user's behalf."));
        assert_eq!((m.secrets[0].full_name(), m.secrets[1].full_name()), ("HLOCK_SECRET_GITHUB_TOKEN".to_string(), "HLOCK_SECRET_SENTRY_DSN".to_string()));
        assert!(m.secrets[1].optional && m.secrets[1].used_by.is_empty());
        // an empty manifest has neither
        let e = Manifest::from_str("").unwrap();
        assert!(e.environment.is_empty() && e.secrets.is_empty());
        e.check_declared().unwrap();
    }

    #[test]
    fn unknown_environment_and_secret_keys_are_problems() {
        let m = Manifest::from_str(
            "[environment]\nyum = [\"x\"]\n[[environment.binaries]]\nurl = \"https://x/t\"\nsha265 = \"\"\npath = \"/t\"\n[[secrets]]\nname = \"A\"\nused_by = [\"x\"]\n",
        )
        .unwrap();
        let p = m.problems();
        for want in [
            "unknown key `yum` in [environment]",
            "unknown key `sha265` in [[environment.binaries]] (did you mean `sha256`?)",
            "unknown key `used_by` in [[secrets]] (did you mean `used-by`?)",
        ] {
            assert!(p.iter().any(|x| x == want), "{want:?} not in {p:?}");
        }
        // a value of the wrong type doesn't parse at all
        assert!(Manifest::from_str("[[secrets]]\nname = \"A\"\nused-by = \"mcp:x\"\n").is_err());
        assert!(Manifest::from_str("[environment]\napt = \"ripgrep\"\n").is_err());
    }

    #[test]
    fn bad_environment_and_secret_values_are_errors() {
        let bin = |url: &str, sha: &str, path: &str| format!("[[environment.binaries]]\nurl = {url:?}\nsha256 = {sha:?}\npath = {path:?}\n");
        let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for (text, want) in [
            (bin("https://x/t", &sha.to_uppercase(), "/t"), "must be 64 lowercase hex digits"),
            (bin("https://x/t", &sha[..63], "/t"), "must be 64 lowercase hex digits"),
            (bin("https://x/t", "", "/t"), "needs `sha256`"),
            (bin("", sha, "/t"), "needs `url`"),
            (bin("https://x/t", sha, ""), "[[environment.binaries]] #1 needs `path`"),
            (bin("ftp://x/t", sha, "/t"), "must be an http(s) URL"),
            (bin("https://x/t", sha, "bin/t"), "must be an absolute file path"),
            (bin("https://x/t", sha, "/usr/../etc/passwd"), "must be an absolute file path"),
            (format!("{}{}", bin("https://x/a", sha, "/t"), bin("https://x/b", sha, "/t")), "[[environment.binaries]] /t is declared twice"),
            ("[environment]\npip = [\"-e git+https://x\"]\n".to_string(), "[environment] pip entry"),
            ("[environment]\napt = [\"a b\"]\n".to_string(), "must be one package"),
            ("[[secrets]]\nused-by = [\"x\"]\n".to_string(), "needs a `name`"),
            ("[[secrets]]\nname = \"GITHUB-TOKEN\"\n".to_string(), "isn't a valid secret name"),
            ("[[secrets]]\nname = \"${GH}\"\n".to_string(), "isn't a valid secret name"),
            ("[[secrets]]\nname = \"HLOCK_SECRET_\"\n".to_string(), "isn't a valid secret name"),
            ("[[secrets]]\nname = \"GH\"\n[[secrets]]\nname = \"HLOCK_SECRET_GH\"\n".to_string(), "declared twice"),
            ("[[secrets]]\nname = \"GH\"\nused-by = [\"\"]\n".to_string(), "used-by entry"),
        ] {
            let e = Manifest::from_str(&text).unwrap().check_declared().unwrap_err().to_string();
            assert!(e.contains(want), "{text}: {e}");
        }
        // and `load` refuses them
        let d = std::env::temp_dir().join(format!("hlock-manifest-declared-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("harness.toml");
        std::fs::write(&p, bin("https://x/t", "abc", "/t")).unwrap();
        assert!(Manifest::load(&p).unwrap_err().to_string().contains("sha256"));
        let _ = std::fs::remove_dir_all(&d);
    }

    const SEEDED: &str = r#"
[project]
name = "rbac"
display-name = "RBAC reviewer"
avatar = "./avatar.png"

[context]
profile = "forked"

[[workspace.repos]]
url = "https://github.com/org/repo"
ref = "main"
path = "src/repo"

[[workspace.files]]
path = "./seed/NOTES.md"
target = "NOTES.md"

[[workspace.files]]
path = "seed/fixtures"
target = "fixtures"

[commands]
review = "./commands/review.md"
"#;

    #[test]
    fn workspace_commands_profile_and_persona_parse_and_digest() {
        let d = std::env::temp_dir().join(format!("hlock-manifest-seeded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("seed/fixtures")).unwrap();
        std::fs::create_dir_all(d.join("commands")).unwrap();
        std::fs::write(d.join("seed/NOTES.md"), "notes").unwrap();
        std::fs::write(d.join("seed/fixtures/a.json"), "{}").unwrap();
        std::fs::write(d.join("commands/review.md"), "Review the diff.").unwrap();
        std::fs::write(d.join("avatar.png"), [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(d.join("harness.toml"), SEEDED).unwrap();
        let m = Manifest::load(&d.join("harness.toml")).unwrap();
        assert!(m.problems().is_empty(), "{:?}", m.problems());
        assert_eq!(m.workspace.repos[0].reference.as_deref(), Some("main"));
        assert_eq!(m.profile_mode().unwrap().as_deref(), Some("forked"));
        let decl = m.declarations().unwrap();
        assert_eq!(decl.workspace.files[0].digest.as_deref(), Some(crate::hash::sha256_text("notes").as_str()));
        assert_eq!(decl.workspace.files[1].digest.as_deref(), Some(crate::capture::tree_digest(&d.join("seed/fixtures")).as_str()), "a directory: its tree digest");
        assert_eq!(decl.commands, [Command { name: "review".into(), path: "./commands/review.md".into(), digest: Some(crate::hash::sha256_text("Review the diff.")) }]);
        assert_eq!(decl.profile.as_deref(), Some("forked"));
        assert_eq!(decl.persona.display_name.as_deref(), Some("RBAC reviewer"));
        assert!(decl.persona.avatar_digest.as_deref().is_some_and(|x| x.starts_with("sha256:")));

        // a file that isn't there is an error, naming it
        std::fs::remove_file(d.join("commands/review.md")).unwrap();
        let e = m.declarations().unwrap_err().to_string();
        assert!(e.contains("[commands] review ./commands/review.md"), "{e}");
        let _ = std::fs::remove_dir_all(&d);

        // none of it declared: nothing to record
        assert!(Manifest::from_str("").unwrap().declarations().unwrap().is_empty());
    }

    #[test]
    fn unknown_workspace_keys_are_problems() {
        let m = Manifest::from_str("[workspace]\nrepo = []\n[[workspace.repos]]\nurl = \"https://x/r\"\nbranch = \"main\"\npath = \"r\"\n[[workspace.files]]\npath = \"a\"\ntraget = \"a\"\n[command]\nx = \"y\"\n").unwrap();
        let p = m.problems();
        for want in [
            "unknown key `repo` in [workspace] (did you mean `repos`?)",
            "unknown key `branch` in [[workspace.repos]]",
            "unknown key `traget` in [[workspace.files]] (did you mean `target`?)",
            "unknown key `command` in harness.toml (did you mean `commands`?)",
        ] {
            assert!(p.iter().any(|x| x == want), "{want:?} not in {p:?}");
        }
    }

    #[test]
    fn bad_workspace_commands_profile_and_persona_values_are_errors() {
        let repo = |url: &str, rf: &str, path: &str| format!("[[workspace.repos]]\nurl = {url:?}\nref = {rf:?}\npath = {path:?}\n");
        let file = |path: &str, target: &str| format!("[[workspace.files]]\npath = {path:?}\ntarget = {target:?}\n");
        for (text, want) in [
            (repo("", "main", "r"), "[[workspace.repos]] #1 needs `url`"),
            (repo("https://x/r", "main", ""), "needs `path`"),
            (repo("--upload-pack=touch /tmp/x", "main", "r"), "must be an https, http, ssh or git@ URL"),
            (repo("file:///etc", "main", "r"), "must be an https, http, ssh or git@ URL"),
            (repo("https://x/r", "-b", "r"), "must be a branch, tag or commit"),
            (repo("https://x/r", "main; rm", "r"), "must be a branch, tag or commit"),
            (repo("https://x/r", "main", "/abs"), "inside the workspace"),
            (repo("https://x/r", "main", "../up"), "inside the workspace"),
            (format!("{}{}", repo("https://x/a", "main", "r"), file("a", "./r")), "r is seeded twice"),
            (file("../outside", "a"), "inside the package"),
            (file("a", ""), "needs `target`"),
            (file("a", "/etc/passwd"), "inside the workspace"),
            ("[[workspace.files]]\npath = \"a\"\ntarget = \"a\"\ndigest = \"sha256:x\"\n".to_string(), "digest is what the lock records"),
            ("[commands]\n\"../x\" = \"c.md\"\n".to_string(), "[commands] name"),
            ("[commands]\n\"-x\" = \"c.md\"\n".to_string(), "[commands] name"),
            ("[commands]\nx = \"/etc/passwd\"\n".to_string(), "[commands] x path"),
            ("[context]\nprofile = \"isolated\"\n".to_string(), "isn't one of shared, forked, none"),
            ("[context]\nprofile = true\n".to_string(), "[context] profile"),
            ("[project]\ndisplay-name = \"a\\nb\"\n".to_string(), "[project] display-name"),
            ("[project]\ndisplay-name = 3\n".to_string(), "[project] display-name must be a string"),
            ("[project]\navatar = \"/me.png\"\n".to_string(), "[project] avatar"),
        ] {
            let e = Manifest::from_str(&text).unwrap().check_declared().unwrap_err().to_string();
            assert!(e.contains(want), "{text}: {e}");
        }
        // a [commands] value that isn't a path doesn't parse at all
        assert!(Manifest::from_str("[commands]\nreview = { path = \"x\" }\n").is_err());
    }

    #[test]
    fn deps_hash_counts_workspace_commands_profile_and_persona_only_when_present() {
        let base = "[skills]\nsql = \"github:acme/skills/skills/sql\"\n";
        let before = Manifest::from_str(base).unwrap().deps_hash();
        // nor does the rest of [context] or [project]
        assert_eq!(Manifest::from_str(&format!("[project]\nname = \"x\"\n[context]\nisolation = \"locked-only\"\n{base}")).unwrap().deps_hash(), before);
        assert_ne!(Manifest::from_str(&format!("{base}[context]\nprofile = \"none\"\n")).unwrap().deps_hash(), before);
        assert_ne!(Manifest::from_str(&format!("[project]\ndisplay-name = \"X\"\n{base}")).unwrap().deps_hash(), before);
        assert_ne!(Manifest::from_str(&format!("{base}[commands]\nreview = \"c.md\"\n")).unwrap().deps_hash(), before);
        assert_ne!(Manifest::from_str(&format!("{base}[[workspace.files]]\npath = \"a\"\ntarget = \"a\"\n")).unwrap().deps_hash(), before);
    }

    #[test]
    fn deps_hash_counts_environment_and_secrets_only_when_present() {
        let base = "[skills]\nsql = \"github:acme/skills/skills/sql\"\n";
        let m = Manifest::from_str(base).unwrap();
        // what it was before these sections existed
        let before = crate::hash::content_hash(&serde_json::json!({"skills": m.skills, "mcp": {}, "instructions": {}, "plugins": {}, "packages": {}, "tools": {}}));
        assert_eq!(m.deps_hash(), before);
        let with_env = Manifest::from_str(&format!("{base}[environment]\napt = [\"ripgrep\"]\n")).unwrap();
        let with_secret = Manifest::from_str(&format!("{base}[[secrets]]\nname = \"GH\"\n")).unwrap();
        assert_ne!(with_env.deps_hash(), before);
        assert_ne!(with_secret.deps_hash(), before);
    }

    #[test]
    fn load_checks_deciders_and_hashes_what_it_parsed() {
        let d = std::env::temp_dir().join(format!("hlock-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("harness.toml");
        std::fs::write(&p, "[deciders.stop]\nmodel = \"anthropic:x\"\n").unwrap();
        assert!(Manifest::load(&p).unwrap_err().to_string().contains("[deciders.stop]"));
        let text = "[project]\nname = \"a\"\n";
        std::fs::write(&p, text).unwrap();
        let m = Manifest::load(&p).unwrap();
        assert_eq!(m.manifest_hash(), Manifest::from_str(text).unwrap().manifest_hash());
        std::fs::write(&p, "[project]\nname = \"changed\"\n").unwrap();
        assert_eq!(m.manifest_hash(), sha256_bytes(text.as_bytes()), "the hash is of what was parsed, not the file now");
        let _ = std::fs::remove_dir_all(&d);
    }
}
