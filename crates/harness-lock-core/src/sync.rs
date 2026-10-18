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
use crate::lockfile::{Lockfile, Native, Runtime};
use crate::profiles::{probe_harness, probe_runtime, python_package_versions, run_capture, Ctx, HarnessProfile, Registry};

/// Where a sync materializes the setup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncOptions {
    /// Materialize into an isolated config root under this directory instead of
    /// the user's real one (the "virtual environment" `launch` uses).
    pub prefix: Option<PathBuf>,
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
    },
    /// Copy a vendored directory (a skill) into place.
    CopyDir { from: PathBuf, to: PathBuf },
    /// Copy a file (an instruction file) into place.
    CopyFile { from: PathBuf, to: PathBuf },
    /// Fetch a skill from git at a commit.
    GitSkill { url: String, commit: String, subpath: String, to: PathBuf },
    /// Run a command.
    Run { argv: Vec<String>, env: BTreeMap<String, String> },
    /// Move a path aside to `<path>.hlock-bak`.
    Remove { path: PathBuf },
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
    pub steps: Vec<Step>,
}

impl Plan {
    pub fn count(&self, status: &str) -> usize {
        self.steps.iter().filter(|s| s.status == status && !s.optional).count()
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

/// Fill placeholders from the environment, else keep the target's existing value
/// at the same place (so a sync never overwrites a real secret with a
/// placeholder). Returns the names still missing.
fn fill(desired: &Value, actual: Option<&Value>, env: &BTreeMap<String, String>, missing: &mut BTreeSet<String>) -> Value {
    match desired {
        Value::String(s) if s.contains(PH_OPEN) => {
            let mut out = s.clone();
            let mut unfilled = Vec::new();
            for name in placeholders(s) {
                match env.get(&name).filter(|v| !v.is_empty()) {
                    Some(v) => out = out.replace(&format!("${{{name}}}"), v),
                    None => unfilled.push(name),
                }
            }
            if unfilled.is_empty() {
                return Value::String(out);
            }
            match actual.and_then(Value::as_str) {
                Some(a) if !a.contains(PH_OPEN) && !a.is_empty() => Value::String(a.to_string()),
                _ => {
                    missing.extend(unfilled);
                    Value::String(out)
                }
            }
        }
        Value::Object(m) => {
            let am = actual.and_then(Value::as_object);
            Value::Object(m.iter().map(|(k, v)| (k.clone(), fill(v, am.and_then(|a| a.get(k)), env, missing))).collect())
        }
        Value::Array(items) => {
            let aa = actual.and_then(Value::as_array);
            Value::Array(items.iter().enumerate().map(|(i, v)| fill(v, aa.and_then(|a| a.get(i)), env, missing)).collect())
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
        (Some(Value::String(s)), b) if s.contains(PH_OPEN) => b.or(Some(Value::String(s))),
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
}

/// The file contents a `WriteConfig` would produce, and the secrets it lacks.
fn desired_config(vendored: &Path, format: &str, pointers: &[String], target: &Path, fallback: Option<&Path>, env: &BTreeMap<String, String>) -> Result<(Value, BTreeSet<String>)> {
    let src_fmt = if pointers.is_empty() { if format == "jsonc" { "json" } else { format } } else { "json" };
    let desired = read_config(vendored, src_fmt)?.ok_or_else(|| Error::Lockfile(format!("vendored file missing: {}", vendored.display())))?;
    let actual = read_config(target, format).ok().flatten();
    let live = fallback.and_then(|f| read_config(f, format).ok().flatten());
    let secrets_from = overlay(actual.clone(), live);
    let mut missing = BTreeSet::new();
    if pointers.is_empty() {
        return Ok((fill(&desired, secrets_from.as_ref(), env, &mut missing), missing));
    }
    let mut out = actual.clone().filter(Value::is_object).unwrap_or_else(|| json!({}));
    for p in pointers {
        if let Some(sub) = desired.get(p) {
            let cur = secrets_from.as_ref().and_then(|a| a.pointer(p));
            set_pointer(&mut out, p, fill(sub, cur, env, &mut missing));
        }
    }
    Ok((out, missing))
}

// --------------------------------------------------------------------------- //
// Targets
// --------------------------------------------------------------------------- //

/// The context a plan writes into, and the environment that selects it.
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
        let base = if prefix.is_absolute() { prefix.clone() } else { ctx.cwd.join(prefix) };
        let base = base.join(&profile.id);
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

/// Expand a lock's portable path (`{root}/…`, `~/…`, relative) for a target.
fn expand(p: &str, root: &Path, t: &Ctx) -> PathBuf {
    t.expand(p, Some(root))
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
    argv.iter()
        .map(|a| if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=+".contains(c)) { a.clone() } else { format!("'{}'", a.replace('\'', "'\\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command that installs a package by method, or `None` for app/manual installs.
pub(crate) fn install_command(method: &str, package: Option<&str>, version: Option<&str>, script: Option<&str>) -> Option<Vec<String>> {
    let v = |sep: &str| version.map(|v| format!("{sep}{v}")).unwrap_or_default();
    match (method, package) {
        ("npm", Some(p)) => Some(vec!["npm".into(), "install".into(), "-g".into(), format!("{p}{}", v("@"))]),
        ("brew", Some(p)) => Some(vec!["brew".into(), "install".into(), p.into()]),
        ("brew-cask", Some(p)) => Some(vec!["brew".into(), "install".into(), "--cask".into(), p.into()]),
        ("uv-tool", Some(p)) => Some(vec!["uv".into(), "tool".into(), "install".into(), format!("{p}{}", v("=="))]),
        ("pip", Some(p)) => Some(vec!["python3".into(), "-m".into(), "pip".into(), "install".into(), format!("{p}{}", v("=="))]),
        ("script", _) => script.map(|s| vec!["sh".into(), "-c".into(), s.replace("{version}", version.unwrap_or(""))]),
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
    let script = profile_install.iter().find(|m| m.method == "script").and_then(|m| m.command.clone());
    let manual_hint = profile_install.iter().find(|m| m.method == method || m.method == "app" || m.method == "manual").and_then(|m| m.command.clone());
    let mut argv = install_command(&method, package.as_deref(), version, script.as_deref());
    if argv.is_none() {
        // the recorded method can't be scripted here; fall back to the profile's first scriptable one
        argv = profile_install.iter().find_map(|m| install_command(&m.method, m.package.as_deref(), version, m.command.as_deref()).filter(|a| ctx.which(&a[0]).is_some()));
    }
    let vtxt = version.map(|v| format!(" {v}")).unwrap_or_default();
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
    let (t, mode, isolation) = target_ctx(profile, ctx, opts);
    let root = profile.config_root(&t);
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
        if let Some(s) = native_step(n, profile, &t, &root, harness_env.clone(), opts, ctx) {
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
        let vendored = lock_dir.join(vend);
        let fallback = (mode == "prefix").then(|| profile.file_path(&c.id, ctx)).flatten();
        let (desired, missing) = desired_config(&vendored, &c.format, &c.pointers, &target, fallback.as_deref(), &t.env)?;
        collect_placeholders(&read_config(&vendored, if c.pointers.is_empty() && c.format != "jsonc" { &c.format } else { "json" })?.unwrap_or_default(), &mut all_secrets);
        missing_secrets.extend(missing.iter().cloned());
        let actual = read_config(&target, &c.format).ok().flatten();
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
            action: (!same).then(|| Action::WriteConfig { path: target.clone(), vendored, format: c.format.clone(), pointers: c.pointers.clone(), fallback }),
            ..Default::default()
        });
    }

    // ---- skills ----
    let write_dir = profile.skills.write.as_ref().map(|w| expand(w, &root, &t));
    let mut locked_skill_dirs = BTreeSet::new();
    for s in &lock.skill {
        if matches!(s.origin.as_str(), "harness-builtin" | "bundled") {
            continue; // provided by the harness / its plugins
        }
        let Some(path) = &s.path else { continue };
        let mut dest = expand(path, &root, &t);
        let outside_root = !path.starts_with("{root}") && (path.starts_with('~') || Path::new(path).is_absolute());
        if mode != "home" && outside_root {
            if let Some(w) = &write_dir {
                dest = w.join(&s.name);
            }
        }
        locked_skill_dirs.insert(dest.clone());
        let ok = dest.join("SKILL.md").is_file() && Some(tree_digest(&dest)) == s.digest;
        let shown = pretty(&dest, ctx);
        let mut step = Step { id: format!("skill:{}", s.name), kind: "skill".into(), title: format!("{} → {shown}", s.name), ..Default::default() };
        if ok {
            step.reason = "matches the lock".into();
        } else if let Some(v) = &s.vendored {
            step.status = "auto".into();
            step.title = format!("Copy skill {} → {shown}", s.name);
            step.action = Some(Action::CopyDir { from: lock_dir.join(v), to: dest });
        } else if let Some(src) = s.source.as_deref().and_then(|x| x.strip_prefix("git+")) {
            let (url_commit, sub) = src.split_once("#path=").unwrap_or((src, ""));
            let (url, commit) = url_commit.rsplit_once('@').unwrap_or((url_commit, "HEAD"));
            let has_git = ctx.which("git").is_some();
            step.status = if has_git { "auto" } else { "manual" }.into();
            step.title = format!("Fetch skill {} from {url}@{}", s.name, &commit[..commit.len().min(10)]);
            step.reason = if has_git { String::new() } else { "needs git".into() };
            step.command = Some(format!("git clone {url} && git -C <clone> checkout {commit}  # then copy {sub}"));
            step.action = has_git.then(|| Action::GitSkill { url: url.into(), commit: commit.into(), subpath: sub.into(), to: dest });
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
    for i in &lock.instructions {
        let dest = expand(&i.name, &root, &t);
        let ok = std::fs::read(&dest).map(|b| Some(crate::hash::sha256_bytes(&b)) == i.digest).unwrap_or(false);
        let mut step = Step { id: format!("instructions:{}", i.name), kind: "instructions".into(), title: pretty(&dest, ctx), ..Default::default() };
        if ok {
            step.reason = "matches the lock".into();
        } else if let Some(v) = &i.vendored {
            step.status = "auto".into();
            step.title = format!("Write {}", pretty(&dest, ctx));
            step.action = Some(Action::CopyFile { from: lock_dir.join(v), to: dest });
        } else {
            step.status = "manual".into();
            step.optional = true;
            step.reason = "project instruction file differs from the lock; it comes from the repo".into();
        }
        steps.push(step);
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

    // ---- runtimes and models ----
    let used: BTreeSet<(String, String)> = lock.llm.iter().filter_map(|l| l.runtime.clone().map(|r| (r, l.model.clone()))).collect();
    for rt in &lock.runtime {
        steps.extend(runtime_steps(rt, &used, reg, ctx, opts, docker));
    }

    // ---- secrets ----
    let env_has = |n: &str| ctx.var(n).is_some();
    for r in &lock.secret {
        if !all_secrets.contains(&r.name) {
            continue;
        }
        let missing = missing_secrets.contains(&r.name);
        steps.push(Step {
            id: format!("secret:{}", r.name),
            kind: "secret".into(),
            title: format!("{} ({})", r.name, r.used_by),
            status: if missing && !docker { "manual" } else { "satisfied" }.into(),
            reason: if docker {
                "supplied at `docker run -e` time".into()
            } else if !missing && env_has(&r.name) {
                "set in the environment".into()
            } else if !missing {
                "kept the value already configured here".into()
            } else {
                "not set".into()
            },
            command: (missing && !docker).then(|| format!("export {}=…   # then re-run hlock sync", r.name)),
            ..Default::default()
        });
    }

    Ok(Plan {
        harness: profile.id.clone(),
        harness_name: profile.name.clone(),
        binary: profile.binaries.first().cloned(),
        version: lock.harness.version.clone(),
        mode,
        root,
        isolation,
        steps,
    })
}

fn native_step(n: &Native, profile: &HarnessProfile, t: &Ctx, root: &Path, env: BTreeMap<String, String>, opts: &SyncOptions, ctx: &Ctx) -> Option<Step> {
    let kind = n.kind.rsplit('.').next().unwrap_or(&n.kind);
    let bin = profile.binaries.first().cloned().unwrap_or_default();
    let have_bin = ctx.which(&bin).is_some();
    let mut s = Step { id: format!("{}:{}", n.kind, n.name), kind: "plugin".into(), ..Default::default() };
    let vtxt = n.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
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
            verify: (!endpoint.is_empty()).then(|| format!("curl -s {}/models", endpoint.trim_end_matches('/'))),
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
            let cmd = profile.pull.as_ref().map(|p| {
                p.replace("{model}", &m.id).replace("{repo}", repo).replace("{revision}", m.revision.as_deref().unwrap_or("main")).replace(" {file}", "")
            });
            s.title = format!("Pull {} model {}{size}", profile.name, m.id);
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

pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Perform one `auto` step and describe what happened.
pub fn run_step(step: &Step, ctx: &Ctx) -> Result<String> {
    let Some(action) = &step.action else { return Ok("nothing to do".into()) };
    match action {
        Action::WriteConfig { path, vendored, format, pointers, fallback } => {
            let (desired, missing) = desired_config(vendored, format, pointers, path, fallback.as_deref(), &ctx.env)?;
            let fmt = if format == "jsonc" { "json" } else { format.as_str() };
            let text = render_config(&desired, fmt).map_err(Error::Profile)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bak = if path.exists() {
                let b = PathBuf::from(format!("{}.hlock-bak", path.display()));
                std::fs::copy(path, &b)?;
                Some(b)
            } else {
                None
            };
            std::fs::write(path, text)?;
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
            if !from.is_dir() {
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
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if to.exists() {
                std::fs::copy(to, format!("{}.hlock-bak", to.display()))?;
            }
            std::fs::copy(from, to)?;
            Ok(format!("wrote {}", to.display()))
        }
        Action::GitSkill { url, commit, subpath, to } => {
            let git = ctx.which("git").ok_or_else(|| Error::Profile("git isn't on PATH".into()))?;
            let tmp = std::env::temp_dir().join(format!("hlock-skill-{}-{}", std::process::id(), crate::hash::sha256_hex(url.as_bytes()).get(..8).unwrap_or("x")));
            let _ = std::fs::remove_dir_all(&tmp);
            let t = Duration::from_secs(600);
            let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
            run_capture(&git, &s(&["clone", "--quiet", url, &tmp.to_string_lossy()]), ctx, t).ok_or_else(|| Error::Profile(format!("git clone {url} failed")))?;
            run_capture(&git, &s(&["-C", &tmp.to_string_lossy(), "checkout", "--quiet", commit]), ctx, t).ok_or_else(|| Error::Profile(format!("git checkout {commit} failed")))?;
            let src = tmp.join(subpath);
            if !src.join("SKILL.md").is_file() {
                return Err(Error::Profile(format!("{url}@{commit} has no skill at {subpath}")));
            }
            backup(to)?;
            copy_tree(&src, to)?;
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(format!("fetched into {}", to.display()))
        }
        Action::Run { argv, env } => {
            let mut cmd = std::process::Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(&ctx.cwd).envs(env).stdin(std::process::Stdio::null());
            let out = cmd.output().map_err(|e| Error::Profile(format!("cannot run {}: {e}", argv[0])))?;
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            if out.status.success() {
                Ok(format!("ran {}", shell_join(argv)))
            } else {
                let tail: Vec<&str> = text.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
                Err(Error::Profile(format!("`{}` exited with {}:\n{}", shell_join(argv), out.status, tail.join("\n"))))
            }
        }
        Action::Remove { path } => {
            let bak = backup(path)?;
            Ok(format!("moved {} aside to {}", path.display(), bak.map(|b| b.display().to_string()).unwrap_or_default()))
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
    s.push_str("## Rules\n\n- Ask the user before installing system software or downloading large model weights, and say how big the download is.\n- Never print, log, or commit secret values. Ask the user to set secrets in their shell; don't type them yourself.\n- Run each step's check before moving on. If a step fails, stop and report the error.\n- Finish by running `hlock sync`, which performs the remaining automatic steps, then `hlock check`.\n\n");
    let pending: Vec<&Step> = plan.steps.iter().filter(|st| st.status != "satisfied" && !st.optional).collect();
    if pending.is_empty() {
        s.push_str("## Steps\n\nNothing is missing. Run `hlock check` to confirm.\n");
        return s;
    }
    s.push_str("## Steps\n\n");
    for (i, st) in pending.iter().enumerate() {
        s.push_str(&format!("{}. **{}**", i + 1, st.title));
        if st.status == "auto" {
            s.push_str(" (`hlock sync` does this)");
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
    s.push_str("\n## Finish\n\n1. Run `hlock sync` and confirm it reports no remaining steps.\n2. Run `hlock check`.\n");
    s
}

/// A Dockerfile and entrypoint for a plan built with `SyncOptions::docker`.
/// Files the plan writes under `<context>/home` are copied to the image's home;
/// secrets stay placeholders in the image and are filled from `-e` at start-up.
pub fn dockerfile(plan: &Plan, lock: &Lockfile, profile: &HarnessProfile) -> (String, String) {
    let mut d = String::new();
    d.push_str(&format!("# Generated by `hlock sync --docker` for {} {}. Rebuild after re-pinning.\n", plan.harness_name, plan.version.as_deref().unwrap_or("")));
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
    match install {
        Some(a) if a[0] == "sh" => d.push_str(&format!("RUN {}\n", a[2])),
        Some(a) => d.push_str(&format!("RUN {}\n", a.iter().map(|x| if x == "python3" { "uv".to_string() } else { x.clone() }).collect::<Vec<_>>().join(" ").replace("uv -m pip install", "uv pip install --system"))),
        None => d.push_str(&format!("# TODO: install {} here; hlock has no scriptable Linux install for it\n", plan.harness_name)),
    }
    for st in plan.steps.iter().filter(|s| s.kind == "plugin" && s.status == "auto") {
        if let Some(Action::Run { argv, .. }) = &st.action {
            let cmd = if argv[0] == "sh" { argv[2].clone() } else { shell_join(argv) };
            d.push_str(&format!("RUN {cmd} || echo 'warning: {} failed'\n", st.id.replace('\'', "")));
        }
    }
    for st in plan.steps.iter().filter(|s| s.kind == "package") {
        if let Some(c) = &st.command {
            d.push_str(&format!("RUN {}\n", c.replace("uv pip install", "uv pip install --system").replace("python3 -m pip install", "uv pip install --system")));
        }
    }
    d.push_str("COPY home/ /root/\n");
    d.push_str("COPY hlock-entrypoint /usr/local/bin/hlock-entrypoint\n");
    for rt in lock.runtime.iter().filter(|r| r.location == "local") {
        if let Some((_, Some(port))) = rt.endpoint.as_deref().and_then(host_port) {
            d.push_str(&format!("# {} runs on the host: point the harness at http://host.docker.internal:{port}\n", rt.name));
        }
    }
    let secrets: Vec<&str> = plan.steps.iter().filter(|s| s.kind == "secret").map(|s| s.id.trim_start_matches("secret:")).collect();
    if !secrets.is_empty() {
        d.push_str(&format!("# Secrets (pass with -e): {}\n", secrets.join(" ")));
    }
    d.push_str("WORKDIR /workspace\nENTRYPOINT [\"/usr/local/bin/hlock-entrypoint\"]\n");
    let bin = profile.binaries.first().cloned().unwrap_or_else(|| "bash".into());
    d.push_str(&format!("CMD [\"{bin}\"]\n"));

    let entry = r#"#!/bin/sh
# Fill ${HLOCK_SECRET_*} placeholders in the harness config from the environment, then run.
python3 - <<'PY'
import os, pathlib, re
pat = re.compile(r"\$\{(HLOCK_SECRET_[A-Z0-9_]+)\}")
for p in pathlib.Path.home().rglob("*"):
    if p.is_file() and p.suffix in {".json", ".jsonc", ".toml", ".yaml", ".yml"} and p.stat().st_size < 5_000_000:
        try:
            text = p.read_text()
        except Exception:
            continue
        new = pat.sub(lambda m: os.environ.get(m.group(1), m.group(0)), text)
        if new != text:
            p.write_text(new)
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
    fn placeholder_fill_keeps_existing_secrets() {
        let desired = json!({"a": {"apiKey": "${HLOCK_SECRET_A}"}, "b": "Bearer ${HLOCK_SECRET_B}", "c": "${HLOCK_SECRET_C}"});
        let actual = json!({"a": {"apiKey": "real-key"}});
        let env: BTreeMap<String, String> = [("HLOCK_SECRET_B".to_string(), "tok".to_string())].into();
        let mut missing = BTreeSet::new();
        let v = fill(&desired, Some(&actual), &env, &mut missing);
        assert_eq!(v, json!({"a": {"apiKey": "real-key"}, "b": "Bearer tok", "c": "${HLOCK_SECRET_C}"}));
        assert_eq!(missing.into_iter().collect::<Vec<_>>(), ["HLOCK_SECRET_C"]);
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
}
