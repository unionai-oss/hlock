//! Sandboxes: running the agent, a code harness's trials and `tests` graders inside an
//! isolation runtime (`specs/sandbox.md`).
//!
//! A lock names its sandbox in `[policy.sandbox]`: a runner (`docker`, or a command
//! runner the user or the project defines), a network mode, how secrets reach it, and
//! which of hlock's process kinds run inside. [`Sandbox::wrap`] turns a command into the
//! runner's argv, with the mounts and the environment the spec allows and nothing else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::hash::content_hash;
use crate::lockfile::Lockfile;
use crate::profiles::Ctx;

/// The process kinds a sandbox can cover.
pub const KINDS: [&str; 3] = ["agent", "trials", "graders"];
/// Where sandboxed runs keep their config roots and `HOME`, apart from `.hlock/env`.
pub const SANDBOX_ENV_DIR: &str = ".hlock/sandbox-env";
/// Set inside every sandbox, so a nested hlock ignores `HLOCK_NO_SANDBOX`.
pub const IN_SANDBOX: &str = "HLOCK_IN_SANDBOX";
/// Turns the lock's sandbox off, like `--no-sandbox` (not inside a sandbox).
pub const NO_SANDBOX: &str = "HLOCK_NO_SANDBOX";

fn err(msg: impl Into<String>) -> Error {
    Error::Manifest(msg.into())
}

/// The sandbox a lock runs in: `[policy.sandbox]` in the lock, `[sandbox]` in
/// `harness.toml` (where `profile` is a path, digested when it's locked).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct Spec {
    /// `docker`, or a command runner's name.
    pub runner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `open`, `none`, or `allowlist` (`hosts` only).
    pub network: String,
    /// The hosts an `allowlist` network reaches.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// `env` or `proxy`.
    pub secrets: String,
    pub applies_to: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grader_image: Option<String>,
}

/// A runtime-native policy file, relative to the project, and its digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProfileRef {
    pub path: String,
    pub digest: String,
}

const SPEC_KEYS: &[&str] = &["runner", "image", "network", "secrets", "applies-to", "profile", "grader-image", "runners"];

impl Spec {
    /// `[sandbox]` from `harness.toml` (its table, and the manifest's directory). `None`
    /// when there's no table or its runner is `none`.
    pub fn from_manifest(table: Option<&Value>, root: &Path) -> Result<Option<Spec>> {
        let Some(t) = table.and_then(Value::as_object) else { return Ok(None) };
        if let Some(k) = t.keys().find(|k| !SPEC_KEYS.contains(&k.as_str())) {
            return Err(err(format!("[sandbox] has an unknown key {k:?} (known: {})", SPEC_KEYS.join(", "))));
        }
        let s = |k: &str| -> Result<Option<String>> {
            match t.get(k) {
                None => Ok(None),
                Some(Value::String(v)) => Ok(Some(v.clone())),
                Some(_) => Err(err(format!("[sandbox] {k} must be a string"))),
            }
        };
        let runner = s("runner")?.unwrap_or_else(|| "none".into());
        if runner == "none" {
            return Ok(None);
        }
        let applies_to = match t.get("applies-to") {
            None => KINDS.iter().map(|k| k.to_string()).collect(),
            Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| err("[sandbox] applies-to must be a list of strings"))).collect::<Result<Vec<_>>>()?,
            Some(_) => return Err(err("[sandbox] applies-to must be a list of strings")),
        };
        let profile = match s("profile")? {
            None => None,
            Some(p) => Some(ProfileRef { digest: profile_digest(root, &p)?, path: p }),
        };
        // OpenShell denies by default and holds keys itself: its defaults say so
        let openshell = runner == "openshell";
        // `network = ["api.github.com", …]` is an allowlist of those hosts
        let (network, hosts) = match t.get("network") {
            Some(Value::Array(a)) => {
                let hosts = a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| err("[sandbox] network must be \"open\", \"none\" or a list of hosts"))).collect::<Result<Vec<_>>>()?;
                ("allowlist".to_string(), hosts)
            }
            Some(Value::String(n)) if n == "allowlist" => return Err(err("[sandbox] network = \"allowlist\": list the hosts instead (network = [\"api.github.com\"])")),
            Some(Value::String(_)) | None => (s("network")?.unwrap_or_else(|| if openshell { "none" } else { "open" }.into()), Vec::new()),
            Some(_) => return Err(err("[sandbox] network must be \"open\", \"none\" or a list of hosts")),
        };
        let spec = Spec {
            image: s("image")?,
            network,
            hosts,
            secrets: s("secrets")?.unwrap_or_else(|| if openshell { "proxy" } else { "env" }.into()),
            runner,
            applies_to,
            profile,
            grader_image: s("grader-image")?,
        };
        spec.validate()?;
        Ok(Some(spec))
    }

    /// The lock's `[policy.sandbox]`. `None` when it's empty or its runner is `none`.
    pub fn from_lock(lock: &Lockfile) -> Result<Option<Spec>> {
        let m = &lock.policy.sandbox;
        if m.is_empty() || m.get("runner").and_then(Value::as_str).is_none_or(|r| r == "none") {
            return Ok(None);
        }
        let spec: Spec = serde_json::from_value(json!(m)).map_err(|e| err(format!("the lock's [policy.sandbox] is malformed: {e}")))?;
        spec.validate()?;
        Ok(Some(spec))
    }

    /// The `[policy.sandbox]` map a lock records.
    pub fn to_map(&self) -> BTreeMap<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m.into_iter().collect(),
            _ => BTreeMap::new(),
        }
    }

    pub fn applies(&self, kind: &str) -> bool {
        self.applies_to.iter().any(|k| k == kind)
    }

    fn validate(&self) -> Result<()> {
        if self.runner.is_empty() || !valid_name(&self.runner) {
            return Err(err(format!("[sandbox] runner {:?} isn't a runner name (letters, digits, - and _)", self.runner)));
        }
        if !matches!(self.network.as_str(), "open" | "none" | "allowlist") {
            return Err(err(format!("[sandbox] network {:?} isn't open, none or a list of hosts", self.network)));
        }
        if self.network == "allowlist" {
            if self.hosts.is_empty() {
                return Err(err("[sandbox] network is an empty list of hosts; use network = \"none\" to reach none"));
            }
            if let Some(h) = self.hosts.iter().find(|h| !valid_host(h)) {
                return Err(err(format!("[sandbox] network host {h:?} isn't a hostname (lowercase letters, digits, '-' and '.', optionally starting with \"*.\"; no scheme, port or path)")));
            }
            let mut seen = std::collections::BTreeSet::new();
            if let Some(h) = self.hosts.iter().find(|h| !seen.insert(h.as_str())) {
                return Err(err(format!("[sandbox] network lists {h} twice")));
            }
        } else if !self.hosts.is_empty() {
            return Err(err(format!("[sandbox] hosts are for a network allowlist, not network = {:?}", self.network)));
        }
        if !matches!(self.secrets.as_str(), "env" | "proxy") {
            return Err(err(format!("[sandbox] secrets {:?} isn't env or proxy", self.secrets)));
        }
        if self.applies_to.is_empty() {
            return Err(err("[sandbox] applies-to is empty; list agent, trials or graders, or set runner = \"none\""));
        }
        if let Some(k) = self.applies_to.iter().find(|k| !KINDS.contains(&k.as_str())) {
            return Err(err(format!("[sandbox] applies-to has {k:?}; the kinds are agent, trials and graders")));
        }
        for img in self.image.iter().chain(&self.grader_image) {
            check_image(img)?;
        }
        if matches!(self.runner.as_str(), "docker" | "openshell") && self.image.is_none() {
            return Err(err(format!("[sandbox] runner = \"{}\" needs an image (hlock sync --docker --build makes one)", self.runner)));
        }
        if self.runner == "openshell" && self.network == "open" {
            return Err(err("[sandbox] OpenShell has no allow-all network rule: use network = \"none\" (the providers' hosts, plus what an OpenShell policy in profile allows)"));
        }
        if self.network == "allowlist" && matches!(self.runner.as_str(), "docker" | "openshell") {
            let how = if self.runner == "docker" { "Docker can't limit a container to some hosts" } else { "OpenShell's policy decides the hosts" };
            return Err(err(format!("[sandbox] runner = \"{}\" can't enforce a network allowlist ({how}): use network = \"none\" and allow the hosts in an OpenShell policy (profile), or a runner whose network-args has an allowlist entry", self.runner)));
        }
        if self.secrets == "proxy" && !matches!(self.runner.as_str(), "docker" | "openshell") {
            return Err(err("[sandbox] secrets = \"proxy\" works with runner = \"docker\" or \"openshell\" only; use secrets = \"env\""));
        }
        if self.secrets == "proxy" && self.network == "none" && self.runner == "docker" {
            return Err(err("[sandbox] secrets = \"proxy\" needs network = \"open\": with none, the agent can't reach the proxy"));
        }
        Ok(())
    }

    /// One line for the run: `docker (network open, secrets proxy)`.
    pub fn summary(&self) -> String {
        format!("{} (network {}, secrets {})", self.runner, self.network_label(), self.secrets)
    }

    /// `open`, `none`, or the allowlist's hosts.
    pub fn network_label(&self) -> String {
        if self.network == "allowlist" {
            format!("allowlist {}", self.hosts.join(", "))
        } else {
            self.network.clone()
        }
    }
}

/// What a lock's sandbox change does to its protection: the ways `b` is weaker than `a`.
pub fn weakened(a: Option<&Spec>, b: Option<&Spec>) -> Vec<String> {
    let Some(a) = a else { return Vec::new() };
    let Some(b) = b else { return vec!["the sandbox is removed".into()] };
    let mut out = Vec::new();
    // none, then an allowlist, then open: each reaches more than the one before
    let reach = |s: &Spec| match s.network.as_str() {
        "none" => 0,
        "allowlist" => 1,
        _ => 2,
    };
    if reach(b) > reach(a) {
        out.push(format!("network {} → {}", a.network, b.network));
    } else if a.network == "allowlist" && b.network == "allowlist" {
        let more: Vec<&str> = b.hosts.iter().filter(|h| !a.hosts.contains(h)).map(String::as_str).collect();
        if !more.is_empty() {
            out.push(format!("network allows more hosts ({})", more.join(", ")));
        }
    }
    if a.secrets == "proxy" && b.secrets == "env" {
        out.push("secrets proxy → env".into());
    }
    for k in a.applies_to.iter().filter(|k| !b.applies(k)) {
        out.push(format!("{k} no longer sandboxed"));
    }
    let pinned = |i: &Option<String>| i.as_deref().is_some_and(|i| i.contains("@sha256:"));
    if pinned(&a.image) && !pinned(&b.image) || pinned(&a.grader_image) && !pinned(&b.grader_image) {
        out.push("an image is no longer pinned by digest".into());
    }
    match (&a.profile, &b.profile) {
        (Some(_), None) => out.push("the profile is removed".into()),
        (Some(x), Some(y)) if x != y => out.push(format!("the profile changed ({})", y.path)),
        _ => {}
    }
    out
}

/// A hostname for a network allowlist: lowercase labels of letters, digits and `-`,
/// optionally after `*.` (its subdomains).
pub fn valid_host(h: &str) -> bool {
    let rest = h.strip_prefix("*.").unwrap_or(h);
    !rest.is_empty()
        && rest.len() <= 253
        && rest.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An environment variable name hlock passes into a sandbox.
pub fn valid_env_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// A Docker image reference: never a flag, nothing a shell or `docker` reads as syntax.
fn check_image(img: &str) -> Result<()> {
    let ok = !img.is_empty()
        && !img.starts_with(['-', '/', '.', ':', '@'])
        && img.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:@".contains(c))
        && img.len() <= 512;
    if ok {
        Ok(())
    } else {
        Err(err(format!("[sandbox] image {img:?} isn't an image reference (like ghcr.io/acme/agent@sha256:…)")))
    }
}

/// The digest of a profile file, which must be a regular file inside the project.
pub fn profile_digest(root: &Path, rel: &str) -> Result<String> {
    let path = profile_path(root, rel)?;
    let bytes = std::fs::read(&path).map_err(|e| err(format!("[sandbox] profile {rel}: {e}")))?;
    Ok(format!("sha256:{}", crate::hash::sha256_hex(&bytes)))
}

fn profile_path(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(err(format!("[sandbox] profile {rel:?} must be a path inside the project")));
    }
    let full = root.join(p);
    let meta = std::fs::symlink_metadata(&full).map_err(|e| err(format!("[sandbox] profile {rel}: {e}")))?;
    if !meta.is_file() {
        return Err(err(format!("[sandbox] profile {rel} isn't a regular file")));
    }
    Ok(full)
}

// --------------------------------------------------------------------------- //
// Runners
// --------------------------------------------------------------------------- //

/// How to wrap a command: the built-in `docker`, or an argv template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct Runner {
    #[serde(skip)]
    pub name: String,
    /// `builtin`, `user` (`~/.config/hlock/sandbox.toml`) or `project` (`harness.toml`).
    #[serde(skip)]
    pub origin: String,
    pub command: Vec<String>,
    pub mount_args: Vec<String>,
    pub mount_ro_args: Vec<String>,
    pub network_args: BTreeMap<String, Vec<String>>,
    pub networks: Vec<String>,
}

const RUNNER_KEYS: &[&str] = &["command", "mount-args", "mount-ro-args", "network-args", "networks"];

impl Runner {
    fn docker() -> Runner {
        Runner { name: "docker".into(), origin: "builtin".into(), networks: vec!["open".into(), "none".into()], ..Default::default() }
    }

    /// NVIDIA OpenShell (`crate::openshell`): network is its policy's, deny by default.
    fn openshell() -> Runner {
        Runner { name: "openshell".into(), origin: "builtin".into(), networks: vec!["none".into()], ..Default::default() }
    }

    /// One of hlock's own runners (`docker`, `openshell`), not an argv template.
    pub fn builtin(&self) -> bool {
        self.origin == "builtin"
    }

    /// The network modes it claims to enforce.
    pub fn supports(&self, network: &str) -> bool {
        let listed = if !self.networks.is_empty() {
            self.networks.clone()
        } else if !self.network_args.is_empty() {
            self.network_args.keys().cloned().collect()
        } else {
            vec!["open".into()]
        };
        listed.iter().any(|n| n == network)
    }

    /// The digest `hlock trust` approves: what it runs, and how.
    pub fn digest(&self) -> String {
        content_hash(&json!({"kind": "runner", "name": self.name, "command": self.command, "mount-args": self.mount_args,
            "mount-ro-args": self.mount_ro_args, "network-args": self.network_args, "networks": self.networks}))
    }

    fn parse(name: &str, origin: &str, v: &Value) -> Result<Runner> {
        let t = v.as_object().ok_or_else(|| err(format!("sandbox runner {name} must be a table")))?;
        if let Some(k) = t.keys().find(|k| !RUNNER_KEYS.contains(&k.as_str())) {
            return Err(err(format!("sandbox runner {name} has an unknown key {k:?} (known: {})", RUNNER_KEYS.join(", "))));
        }
        let mut r: Runner = serde_json::from_value(v.clone()).map_err(|e| err(format!("sandbox runner {name}: {e}")))?;
        if r.command.is_empty() || r.command[0].starts_with('{') {
            return Err(err(format!("sandbox runner {name} needs a command, starting with a program")));
        }
        r.name = name.into();
        r.origin = origin.into();
        Ok(r)
    }
}

/// The user's runners file: `~/.config/hlock/sandbox.toml` (or `HLOCK_SANDBOX_FILE`).
pub fn user_file(ctx: &Ctx) -> PathBuf {
    match ctx.var("HLOCK_SANDBOX_FILE") {
        Some(p) => PathBuf::from(p),
        None => ctx.expand("~/.config/hlock/sandbox.toml", None),
    }
}

/// Names a runners file can't define: hlock's own runners and `none`.
const RESERVED: &[&str] = &["docker", "openshell", "none"];

/// Every runner defined here: `docker` and `openshell`, the user's, then the project's
/// (`harness.toml`'s `[sandbox.runners]`, which can't shadow a built-in, `none`, or a user
/// runner).
pub fn runners(ctx: &Ctx, project: Option<&Value>) -> Result<BTreeMap<String, Runner>> {
    let mut out = BTreeMap::from([("docker".to_string(), Runner::docker()), ("openshell".to_string(), Runner::openshell())]);
    let file = user_file(ctx);
    if file.is_file() {
        let text = std::fs::read_to_string(&file)?;
        let doc: Value = toml::from_str::<toml::Value>(&text)
            .map_err(|e| err(format!("{}: {e}", file.display())))
            .and_then(|v| serde_json::to_value(v).map_err(|e| err(e.to_string())))?;
        for (name, def) in doc.get("runners").and_then(Value::as_object).into_iter().flatten() {
            if !valid_name(name) || RESERVED.contains(&name.as_str()) {
                return Err(err(format!("{}: {name:?} can't be a runner name", file.display())));
            }
            out.insert(name.clone(), Runner::parse(name, "user", def)?);
        }
    }
    for (name, def) in project.and_then(|v| v.get("runners")).and_then(Value::as_object).into_iter().flatten() {
        if !valid_name(name) || RESERVED.contains(&name.as_str()) {
            return Err(err(format!("[sandbox.runners] can't define {name:?}")));
        }
        if out.contains_key(name) {
            continue; // the user's own definition wins
        }
        out.insert(name.clone(), Runner::parse(name, "project", def)?);
    }
    Ok(out)
}

/// The `[sandbox]` table of the `harness.toml` that goes with a lock (at or above its
/// directory), if any.
pub fn project_table(lock_dir: &Path) -> Option<Value> {
    let path = crate::manifest::find_manifest(lock_dir)?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    serde_json::to_value(v.get("sandbox")?).ok()
}

// --------------------------------------------------------------------------- //
// Trust
// --------------------------------------------------------------------------- //

/// The `sandbox` executable `hlock trust` approves for a lock: the runner's template (when
/// the project defines it), the images, and the profile. A change to any asks again.
pub fn executable(spec: &Spec, runner: Option<&Runner>) -> crate::trust::Executable {
    let template = runner.filter(|r| r.origin == "project").map(|r| r.digest());
    let detail = match (&spec.image, runner.filter(|r| r.origin != "builtin")) {
        (Some(i), _) => i.clone(),
        (None, Some(r)) => r.command.join(" "),
        (None, None) => spec.runner.clone(),
    };
    crate::trust::Executable {
        kind: "sandbox".into(),
        name: spec.runner.clone(),
        detail,
        digest: content_hash(&json!({"kind": "sandbox", "runner": spec.runner, "template": template, "image": spec.image,
            "grader-image": spec.grader_image, "profile": spec.profile.as_ref().map(|p| &p.digest)})),
        via: None,
        source: None,
        steps: vec![],
        status: String::new(),
    }
}

// --------------------------------------------------------------------------- //
// A sandbox ready to run things
// --------------------------------------------------------------------------- //

/// A checked sandbox: the spec, its runner, and hlock's copy of the profile.
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub spec: Spec,
    pub runner: Runner,
    /// The project directory the lock belongs to.
    pub project: PathBuf,
    /// The profile, copied where hlock owns it after its digest was checked.
    pub profile_copy: Option<PathBuf>,
}

/// One path the sandbox sees: `src` on the host at `dst` inside.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mount {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub readonly: bool,
}

impl Mount {
    pub fn rw(p: &Path) -> Mount {
        Mount { src: p.to_path_buf(), dst: p.to_path_buf(), readonly: false }
    }
    pub fn ro(p: &Path) -> Mount {
        Mount { src: p.to_path_buf(), dst: p.to_path_buf(), readonly: true }
    }
}

/// One command to run sandboxed.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// `agent`, `trials` or `graders`.
    pub kind: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The sandbox's `HOME`.
    pub home: PathBuf,
    /// The harness's config root, when it has one.
    pub config: Option<PathBuf>,
    pub mounts: Vec<Mount>,
    /// The variables the command gets inside (beyond `HOME` and [`IN_SANDBOX`]).
    pub env: BTreeMap<String, String>,
    /// Allocate a terminal (`docker run -t`).
    pub tty: bool,
    /// Make the host reachable as `host.docker.internal` (the proxy).
    pub host_gateway: bool,
}

/// A wrapped command: the runner's argv, its process environment, and how to stop it.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Wrapped {
    pub argv: Vec<String>,
    /// The runner process's whole environment. It holds the values `-e NAME` reads.
    #[serde(skip)]
    pub env: BTreeMap<String, String>,
    /// The names that reach the command inside.
    pub env_names: Vec<String>,
    pub id: String,
    /// Run this when a timeout or an interrupt stops the runner (`docker kill …`).
    pub kill: Vec<String>,
    pub mounts: Vec<Mount>,
    /// What the runner can't do as asked (a read-only path it mounts read-write).
    pub notes: Vec<String>,
    /// For `openshell`: the session to begin before `argv` runs and end after it
    /// ([`crate::openshell::begin`]); `argv` is then only what it will look like.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::openshell::Session>,
    /// For `openshell`: the command's variables, for the file [`crate::openshell::begin`]
    /// writes inside. They hold secret values, so they're never printed.
    #[serde(skip)]
    pub session_env: BTreeMap<String, String>,
}

/// The host variables a runner process keeps for itself.
pub fn runner_keeps(k: &str) -> bool {
    matches!(k, "PATH" | "USER" | "LOGNAME" | "TMPDIR" | "XDG_RUNTIME_DIR" | "SYSTEMROOT") || k.starts_with("DOCKER_") || k.starts_with("OPENSHELL_")
}

/// Variables that change how the runner process itself behaves: never set from the lock.
fn steers_runner(k: &str) -> bool {
    runner_keeps(k) || k.starts_with("LD_") || k.starts_with("DYLD_") || k.starts_with("DOCKER_") || matches!(k, "PATH" | "BASH_ENV" | "ENV")
}

/// The host variables that pass into every sandbox when set.
pub fn passes_through(k: &str) -> bool {
    matches!(k, "TERM" | "COLORTERM" | "LANG" | "TZ") || k.starts_with("LC_")
}

impl Sandbox {
    /// Check a spec against this machine: its runner is defined and installed, can enforce
    /// the network mode, and the profile still matches the lock. Doesn't check trust.
    pub fn new(spec: &Spec, project: &Path, project_table: Option<&Value>, ctx: &Ctx) -> Result<Sandbox> {
        let all = runners(ctx, project_table)?;
        let runner = all.get(&spec.runner).cloned().ok_or_else(|| {
            err(format!("the lock's sandbox runner {:?} isn't defined on this machine; define it in {} (or harness.toml [sandbox.runners]), or run with --no-sandbox", spec.runner, user_file(ctx).display()))
        })?;
        let program = if runner.builtin() { runner.name.clone() } else { runner.command[0].clone() };
        if ctx.which(&program).is_none() {
            return Err(err(format!("{program} isn't installed (the {} sandbox runs it); install it, or run with --no-sandbox", spec.runner)));
        }
        if runner.name == "openshell" && spec.network != "none" {
            return Err(err("OpenShell has no allow-all network rule: use network = \"none\" (the providers' hosts, plus what an OpenShell policy in profile allows)"));
        }
        if !runner.supports(&spec.network) {
            return Err(err(format!("the {} runner can't enforce network = {:?}", runner.name, spec.network)));
        }
        let uses_profile = runner.command.iter().chain(&runner.mount_args).any(|a| a.contains("{profile}"));
        if uses_profile && spec.profile.is_none() {
            return Err(err(format!("the {} runner needs a profile; set [sandbox] profile", runner.name)));
        }
        let profile_copy = match &spec.profile {
            None => None,
            Some(p) => {
                let have = profile_digest(project, &p.path)?;
                if have != p.digest {
                    return Err(err(format!("the sandbox profile {} changed since the lock (re-pin to lock the new one)", p.path)));
                }
                // a copy hlock owns, so it can't change between the check and the use
                let dir = project.join(SANDBOX_ENV_DIR).join("profiles");
                std::fs::create_dir_all(&dir)?;
                let name = Path::new(&p.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "profile".into());
                let copy = dir.join(format!("{}-{name}", &p.digest.trim_start_matches("sha256:")[..12]));
                let bytes = std::fs::read(profile_path(project, &p.path)?)?;
                crate::fsutil::write_private(&copy, &bytes)?;
                Some(copy)
            }
        };
        Ok(Sandbox { spec: spec.clone(), runner, project: project.to_path_buf(), profile_copy })
    }

    /// Wrap `run` in the runner: the argv to start, and the environment to start it with.
    pub fn wrap(&self, run: &Run, ctx: &Ctx) -> Result<Wrapped> {
        let id = run_id();
        let mut inside: BTreeMap<String, String> = ctx.env.iter().filter(|(k, _)| passes_through(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        inside.extend(run.env.clone());
        inside.remove("PATH");
        inside.insert("HOME".into(), run.home.to_string_lossy().into_owned());
        inside.insert(IN_SANDBOX.into(), "1".into());
        if let Some(bad) = inside.keys().find(|k| !valid_env_name(k)) {
            return Err(err(format!("{bad:?} can't be passed into a sandbox: not a variable name")));
        }
        // a variable that would steer the runner itself (its loader, its Docker daemon)
        // never reaches it from the lock or the project
        let steering: Vec<String> = inside.keys().filter(|k| steers_runner(k)).cloned().collect();
        for k in &steering {
            inside.remove(k);
        }
        std::fs::create_dir_all(&run.home)?;
        let mut mounts = vec![Mount::rw(&run.cwd), Mount::rw(&run.home)];
        if let Some(c) = &run.config {
            std::fs::create_dir_all(c)?;
            mounts.push(Mount::rw(c));
        }
        mounts.extend(run.mounts.iter().cloned());
        let mounts = normalize(mounts)?;
        let runner_env: BTreeMap<String, String> = ctx.env.iter().filter(|(k, _)| runner_keeps(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        let names: Vec<String> = inside.keys().cloned().collect();
        let mut notes = Vec::new();
        if !steering.is_empty() {
            notes.push(format!("left out of the sandbox's environment, since they'd steer the runner: {}", steering.join(", ")));
        }
        if self.runner.name == "openshell" {
            if self.spec.secrets == "proxy" {
                // a provider sends the key to the provider's own host: a key meant for
                // another endpoint would go to the wrong place
                for (_, key, host, _) in crate::openshell::KEY_ROUTES {
                    let base = key.replace("_API_KEY", "_BASE_URL");
                    if let Some(url) = ctx.var(&base).filter(|u| !u.trim_end_matches('/').starts_with(&format!("https://{host}"))) {
                        return Err(err(format!("{base} is {url}, but an OpenShell provider sends {key} to {host} only; unset {base}, or use secrets = \"env\" with an OpenShell policy in profile that allows its host")));
                    }
                }
            }
            let session = crate::openshell::plan(self, run, &mounts, &id, names.clone())?;
            // the openshell CLI finds its gateway under the real HOME (and XDG_CONFIG_HOME); a
            // provider reads the keys from its environment. The command's values go in a
            // file inside (begin), never in the CLI's environment.
            let mut env = runner_env;
            for k in ["HOME", "XDG_CONFIG_HOME"] {
                if let Some(v) = ctx.var(k) {
                    env.insert(k.into(), v.to_string());
                }
            }
            if self.spec.secrets == "proxy" {
                env.extend(crate::openshell::KEY_ROUTES.iter().filter_map(|(_, k, _, _)| ctx.var(k).map(|v| (k.to_string(), v.to_string()))));
            } else if run.kind != "graders" {
                notes.push("secrets = \"env\": the keys are in the sandbox's environment, and OpenShell's policy (profile) has to allow their hosts".into());
            }
            let session_env: BTreeMap<String, String> = inside.into_iter().filter(|(k, _)| k != "HOME" && k != IN_SANDBOX).collect();
            let argv = crate::openshell::preview(&session);
            return Ok(Wrapped { argv, env, env_names: names, id, kill: vec![], mounts, notes, session: Some(session), session_env });
        }
        let (argv, env, kill) = if self.runner.name == "docker" {
            let image = if run.kind == "graders" { self.spec.grader_image.as_ref().or(self.spec.image.as_ref()) } else { self.spec.image.as_ref() };
            let image = image.ok_or_else(|| err("the docker sandbox has no image"))?;
            let mut env = runner_env;
            // the docker client finds its context under the real HOME
            if let Some(h) = ctx.env.get("HOME") {
                env.insert("HOME".into(), h.clone());
            }
            let mut a = docker_argv(self, run, &mounts, &inside, image, &id)?;
            a.extend(run.argv.iter().skip(1).cloned());
            for (k, v) in &inside {
                if k != "HOME" && k != IN_SANDBOX {
                    env.insert(k.clone(), v.clone());
                }
            }
            (a, env, vec!["docker".into(), "kill".into(), format!("hlock-{id}")])
        } else {
            let mut a = self.expand(run, &mounts, &id)?;
            a.extend(run.argv.iter().cloned());
            let r = &self.runner;
            if r.command.iter().any(|a| a == "{mounts}") {
                if r.mount_ro_args.is_empty() && mounts.iter().any(|m| m.readonly) {
                    notes.push(format!("the {} runner has no mount-ro-args, so read-only paths are mounted read-write", r.name));
                }
                if mounts.iter().any(|m| m.src != m.dst) && !r.mount_args.iter().chain(&r.mount_ro_args).any(|a| a.contains("{src}")) {
                    notes.push(format!("the {} runner's mount templates don't use {{src}}, so .env files aren't hidden", r.name));
                }
            }
            let mut env = runner_env;
            env.extend(inside.clone());
            (a, env, vec![])
        };
        Ok(Wrapped { argv, env, env_names: names, id, kill, mounts, notes, session: None, session_env: BTreeMap::new() })
    }

    /// Run a command sandboxed to completion, within `limits`: `trials` and `graders`.
    /// The command sees `cwd` read-write, the project read-only, and `mounts`. A run that
    /// times out also gets the runner's kill command (`docker kill`).
    #[allow(clippy::too_many_arguments)]
    pub fn run_command(&self, kind: &str, argv: &[String], cwd: &Path, env: BTreeMap<String, String>, mounts: Vec<Mount>, ctx: &Ctx, limits: &crate::process::Limits) -> Result<crate::process::Output> {
        let at_project = std::fs::canonicalize(cwd).ok() == std::fs::canonicalize(&self.project).ok();
        let mut all = if at_project { project_mounts(&self.project)? } else { project_ro_mounts(&self.project)? };
        all.extend(mounts);
        let run = Run {
            kind: kind.into(),
            argv: argv.to_vec(),
            cwd: cwd.to_path_buf(),
            home: self.project.join(SANDBOX_ENV_DIR).join(format!("{kind}-home")),
            mounts: all,
            env,
            ..Default::default()
        };
        let w = self.wrap(&run, ctx)?;
        let started = match &w.session {
            Some(s) => Some(crate::openshell::begin(s.clone(), &w.env, &w.session_env, limits.timeout.map(|t| t.as_secs()))?),
            None => None,
        };
        let (argv, run_env) = match &started {
            Some(st) => (st.argv.clone(), st.env.clone()),
            None => (w.argv.clone(), w.env.clone()),
        };
        let (prog, args) = argv.split_first().ok_or_else(|| err("nothing to run"))?;
        let mut cmd = std::process::Command::new(prog);
        cmd.args(args).current_dir(cwd).env_clear().envs(&run_env).stdin(std::process::Stdio::null());
        let t0 = std::time::Instant::now();
        let mut ran = crate::process::run(cmd, limits).map_err(|e| err(format!("cannot run {prog}: {e}")));
        // the deadline inside the sandbox stops it just before ours: that's a timeout too
        if let (Ok(o), Some(st)) = (ran.as_mut(), &started) {
            if st.stopped_inside(o.code, t0.elapsed()) {
                o.timed_out = true;
                o.success = false;
            }
        }
        let timed_out = ran.as_ref().is_ok_and(|o| o.timed_out);
        if timed_out {
            self.kill(&w);
        }
        // killed without a timeout: a Ctrl-C (stop_all), and the command may still run inside
        let stopped = match &ran {
            Ok(o) if o.timed_out => Some("timeout"),
            Ok(o) if o.code.is_none() => Some("interrupt"),
            Err(_) => Some("interrupt"),
            Ok(_) => None,
        };
        // copied back (and the sandbox gone) before anyone reads the command's files
        let ended = started.as_ref().map(|st| crate::openshell::end(&st.session, &w.env, stopped)).transpose();
        let mut out = ran?;
        for n in ended?.unwrap_or_default() {
            out.stderr.extend(format!("\n[hlock] {n}").into_bytes());
        }
        Ok(out)
    }

    /// Stop what a wrapped run left (a container), if the runner says how.
    pub fn kill(&self, w: &Wrapped) {
        if let Some((prog, args)) = w.kill.split_first() {
            let mut c = std::process::Command::new(prog);
            c.args(args).env_clear().envs(&w.env);
            let _ = crate::process::run(c, &crate::process::Limits::timeout(std::time::Duration::from_secs(30)));
        }
    }

    /// A command runner's template, expanded.
    fn expand(&self, run: &Run, mounts: &[Mount], id: &str) -> Result<Vec<String>> {
        let r = &self.runner;
        let profile = self.profile_copy.as_ref().map(|p| p.to_string_lossy().into_owned());
        let image = self.spec.image.clone().unwrap_or_default();
        let config = run.config.as_ref().unwrap_or(&run.home).to_string_lossy().into_owned();
        let sub = |a: &str, m: Option<&Mount>| -> Result<String> {
            if a.contains("{profile}") && profile.is_none() {
                return Err(err(format!("the {} runner needs a profile; set [sandbox] profile", r.name)));
            }
            let mut s = a
                .replace("{cwd}", &run.cwd.to_string_lossy())
                .replace("{config}", &config)
                .replace("{home}", &run.home.to_string_lossy())
                .replace("{profile}", profile.as_deref().unwrap_or(""))
                .replace("{image}", &image)
                .replace("{id}", id)
                .replace("{network}", &self.spec.network);
            if let Some(m) = m {
                s = s.replace("{src}", &m.src.to_string_lossy()).replace("{path}", &m.dst.to_string_lossy());
            }
            Ok(s)
        };
        let mut out = Vec::new();
        for a in &r.command {
            match a.as_str() {
                "{mounts}" => {
                    for m in mounts {
                        let tpl = if m.readonly && !r.mount_ro_args.is_empty() { &r.mount_ro_args } else { &r.mount_args };
                        for t in tpl {
                            out.push(sub(t, Some(m))?);
                        }
                    }
                }
                "{network-args}" => {
                    // for an allowlist, an argument with {host} repeats once per host, and {hosts} is them all
                    for t in r.network_args.get(&self.spec.network).cloned().unwrap_or_default() {
                        if t.contains("{host}") {
                            for h in &self.spec.hosts {
                                out.push(sub(&t.replace("{host}", h), None)?);
                            }
                        } else {
                            out.push(sub(&t.replace("{hosts}", &self.spec.hosts.join(",")), None)?);
                        }
                    }
                }
                a => out.push(sub(a, None)?),
            }
        }
        Ok(out)
    }
}

fn docker_argv(sb: &Sandbox, run: &Run, mounts: &[Mount], inside: &BTreeMap<String, String>, image: &str, id: &str) -> Result<Vec<String>> {
    let mut a: Vec<String> = ["docker", "run", "--rm", "-i"].map(String::from).to_vec();
    if run.tty {
        a.push("-t".into());
    }
    a.extend([format!("--name=hlock-{id}"), "--init".into()]);
    a.push(format!("--network={}", if sb.spec.network == "none" { "none" } else { "bridge" }));
    a.extend(["--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit=4096"].map(String::from));
    if let Some((uid, gid)) = owner() {
        a.push(format!("--user={uid}:{gid}"));
    }
    a.push(format!("--workdir={}", run.cwd.display()));
    let program = run.argv.first().ok_or_else(|| err("nothing to run"))?;
    a.push(format!("--entrypoint={}", docker_program(program, mounts)));
    for m in mounts {
        a.push("--mount".into());
        a.push(format!("type=bind,src={},dst={}{}", m.src.display(), m.dst.display(), if m.readonly { ",readonly" } else { "" }));
    }
    for k in inside.keys() {
        a.push("-e".into());
        // not secret, and the docker client's own HOME differs
        a.push(match k.as_str() {
            "HOME" | IN_SANDBOX => format!("{k}={}", inside[k]),
            _ => k.clone(),
        });
    }
    if run.host_gateway {
        a.push("--add-host=host.docker.internal:host-gateway".into());
    }
    a.push(image.to_string());
    Ok(a)
}

/// The program inside an image: its path when the sandbox mounts it (a pack's script, a
/// project's `./run.sh`), else its name on the image's `PATH`, and `python3` for any Python.
pub fn docker_program(argv0: &str, mounts: &[Mount]) -> String {
    let p = Path::new(argv0);
    if p.is_absolute() {
        if let Ok(c) = std::fs::canonicalize(p) {
            if mounts.iter().any(|m| c.starts_with(&m.dst)) {
                return c.to_string_lossy().into_owned();
            }
        }
    }
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| argv0.to_string());
    if name.starts_with("python") {
        "python3".into()
    } else {
        name
    }
}

/// This process's user and group: what the agent runs as inside, so its files are yours.
#[cfg(unix)]
fn owner() -> Option<(u32, u32)> {
    // SAFETY: getuid and getgid can't fail and touch no memory
    Some(unsafe { (libc::getuid(), libc::getgid()) })
}

#[cfg(not(unix))]
fn owner() -> Option<(u32, u32)> {
    None
}

fn run_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    crate::hash::sha256_hex(format!("{}-{nanos}", std::process::id()).as_bytes())[..12].to_string()
}

/// Canonical paths (so `/var` and `/private/var` agree), no duplicates, parents before
/// what's mounted inside them, and nothing `docker --mount` would misread.
fn normalize(mounts: Vec<Mount>) -> Result<Vec<Mount>> {
    let mut out: Vec<Mount> = Vec::new();
    for m in mounts {
        let src = std::fs::canonicalize(&m.src).map_err(|e| err(format!("can't mount {} into the sandbox: {e}", m.src.display())))?;
        let dst = if m.src == m.dst {
            src.clone()
        } else {
            // a hidden file's place: its directory is canonical, the name stays
            let parent = m.dst.parent().map(std::fs::canonicalize).transpose()?.unwrap_or_default();
            parent.join(m.dst.file_name().unwrap_or_default())
        };
        for p in [&src, &dst] {
            let s = p.to_string_lossy();
            if s.contains(',') || s.starts_with('-') || s.contains('\n') {
                return Err(err(format!("can't mount {s} into the sandbox: its path has a character the runner would misread")));
            }
        }
        // the same place twice (a workspace that's the project): writable wins, since a
        // command's own working directory is listed first and is meant to be written
        if let Some(prev) = out.iter_mut().find(|x| x.dst == dst) {
            prev.readonly &= m.readonly;
            continue;
        }
        out.push(Mount { src, dst, readonly: m.readonly });
    }
    out.sort_by_key(|m| m.dst.components().count());
    Ok(out)
}

// --------------------------------------------------------------------------- //
// The project: control paths and hidden files
// --------------------------------------------------------------------------- //

/// The files at the project's top that decide what hlock (or direnv, or git) runs next on
/// the host.
const CONTROL_FILES: &[&str] = &["harness.toml", "hlock-policy.toml", ".envrc"];
/// Directories skipped when looking for `.env` files.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "venv", ".hlock", "__pycache__"];
const MAX_WALK: usize = 100_000;

/// Whether a file at the project's top is a control file (or a harness lock), existing or
/// not: what a sandbox that copies files back never brings back.
pub fn is_control_name(name: &str) -> bool {
    CONTROL_FILES.contains(&name) || name.starts_with("harness") && (name.ends_with(".lock") || name.ends_with(".lock.prev"))
}

/// The control paths that exist: mounted read-only over the read-write project.
pub fn control_paths(project: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = CONTROL_FILES.iter().map(|f| project.join(f)).filter(|p| p.exists()).collect();
    if let Ok(rd) = std::fs::read_dir(project) {
        let mut locks: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("harness") && (n.ends_with(".lock") || n.ends_with(".lock.prev"))))
            .collect();
        locks.sort();
        out.extend(locks);
    }
    for p in [".hlock", ".git/hooks", ".git/config"] {
        let p = project.join(p);
        if p.exists() {
            out.push(p);
        }
    }
    out
}

/// What a sandboxed run sees of the project: the project read-write, control paths
/// read-only, `.hlock/sandbox-env` read-write again, `.env` files covered by an empty file.
pub fn project_mounts(project: &Path) -> Result<Vec<Mount>> {
    let mut m = vec![Mount::rw(project)];
    m.extend(control_paths(project).iter().map(|p| Mount::ro(p)));
    m.extend(covered(project)?);
    Ok(m)
}

/// The project read-only, with what's secret covered: what trials and graders see of it.
pub fn project_ro_mounts(project: &Path) -> Result<Vec<Mount>> {
    let mut m = vec![Mount::ro(project)];
    m.extend(covered(project)?);
    Ok(m)
}

/// Directories under `.hlock` a sandbox never sees: the unsandboxed config roots (they hold
/// filled-in secrets and logins) and Docker build contexts.
const COVERED_DIRS: &[&str] = &[".hlock/env", ".hlock/docker"];

/// hlock's empty file and directory, mounted read-only over what a sandbox doesn't see:
/// `.env` files and [`COVERED_DIRS`].
fn covered(project: &Path) -> Result<Vec<Mount>> {
    let env_dir = project.join(SANDBOX_ENV_DIR);
    crate::sync::ignore_env(&env_dir)?;
    let (empty, empty_dir) = (env_dir.join(".empty"), env_dir.join(".empty-dir"));
    if !empty.is_file() {
        std::fs::write(&empty, b"")?;
    }
    std::fs::create_dir_all(&empty_dir)?;
    let mut m: Vec<Mount> = hidden_files(project)?.into_iter().map(|f| Mount { src: empty.clone(), dst: f, readonly: true }).collect();
    for d in COVERED_DIRS.iter().map(|d| project.join(d)).filter(|d| d.is_dir()) {
        m.push(Mount { src: empty_dir.clone(), dst: d, readonly: true });
    }
    Ok(m)
}

/// Model-provider credentials and endpoints a harness reads from its environment: what
/// a sandbox gets from the host under `secrets = "env"`.
pub const PROVIDER_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_ORG_ID", "GEMINI_API_KEY", "GOOGLE_API_KEY",
    "GOOGLE_GENERATIVE_AI_API_KEY", "OPENROUTER_API_KEY", "NEARAI_API_KEY", "HL_LLM_BASE_URL", "HL_LLM_API_KEY",
];

/// The host's values for [`PROVIDER_VARS`] and `extra` (a profile's `api-keys`).
pub fn provider_env(ctx: &Ctx, extra: &[String]) -> BTreeMap<String, String> {
    PROVIDER_VARS.iter().map(|s| s.to_string()).chain(extra.iter().cloned()).filter_map(|k| ctx.var(&k).map(|v| (k.clone(), v.to_string()))).collect()
}

/// `.env` and `.env.*` files (or links) under the project. A project too big to search
/// is an error: a `.env` past the limit would be left showing.
pub fn hidden_files(project: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![project.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > MAX_WALK {
                return Err(err(format!("{} has more than {MAX_WALK} files to search for .env files; a sandbox can't promise to hide them all", project.display())));
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(e.path());
                }
            } else if (ft.is_file() || ft.is_symlink()) && (name == ".env" || name.starts_with(".env.")) {
                out.push(e.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A digest of each control file (and each file in `.git/hooks`), or `None` when it
/// doesn't exist: compared before and after a sandboxed run.
pub fn control_snapshot(project: &Path) -> BTreeMap<String, Option<String>> {
    let mut names: Vec<String> = CONTROL_FILES.iter().map(|s| s.to_string()).collect();
    names.extend(["harness.lock".into(), "harness.tune.lock".into()]);
    if let Ok(rd) = std::fs::read_dir(project) {
        names.extend(rd.flatten().filter_map(|e| e.file_name().to_str().map(str::to_string)).filter(|n| n.starts_with("harness") && n.ends_with(".lock")));
    }
    if let Ok(rd) = std::fs::read_dir(project.join(".git/hooks")) {
        names.extend(rd.flatten().filter_map(|e| e.file_name().to_str().map(|n| format!(".git/hooks/{n}"))));
    }
    names.push(".git/config".into());
    names.into_iter().map(|n| {
        let d = std::fs::read(project.join(&n)).ok().map(|b| crate::hash::sha256_hex(&b));
        (n, d)
    }).collect()
}

/// The control files a run created or changed.
pub fn control_changes(before: &BTreeMap<String, Option<String>>, after: &BTreeMap<String, Option<String>>) -> Vec<String> {
    after
        .iter()
        .filter_map(|(k, v)| match (before.get(k).cloned().flatten(), v) {
            (None, Some(_)) => Some(format!("created {k}")),
            (Some(a), Some(b)) if &a != b => Some(format!("changed {k}")),
            (Some(_), None) => Some(format!("removed {k}")),
            _ => None,
        })
        .collect()
}

// --------------------------------------------------------------------------- //
// Choosing the sandbox for a command
// --------------------------------------------------------------------------- //

/// Whether `--no-sandbox` or `HLOCK_NO_SANDBOX` turns the sandbox off. Inside a sandbox
/// the variable is ignored (a nested `hlock run` can't turn it off); returns a note then.
pub fn disabled(flag: bool, ctx: &Ctx) -> (bool, Option<String>) {
    if flag {
        return (true, None);
    }
    let var = ctx.var(NO_SANDBOX).is_some_and(|v| !matches!(v, "0" | "false"));
    match (var, ctx.var(IN_SANDBOX).is_some()) {
        (true, true) => (false, Some(format!("ignoring {NO_SANDBOX}: {IN_SANDBOX} is set"))),
        (v, _) => (v, None),
    }
}

/// The sandbox a lock's runs use: checked against the project's policy, approved on this
/// machine (unless `trust`), and ready. `None` when the lock has none, or it's turned off
/// (`no_sandbox`, or `HLOCK_NO_SANDBOX`); `notes` says so.
pub fn for_lock(lock: &Lockfile, lock_path: &Path, project: &Path, ctx: &Ctx, no_sandbox: bool, trust: bool, notes: &mut Vec<String>) -> Result<Option<Sandbox>> {
    let (off, note) = disabled(no_sandbox, ctx);
    notes.extend(note);
    let shown = crate::sync::pretty(lock_path, ctx);
    let Some(spec) = Spec::from_lock(lock)? else {
        let policy = crate::policy::Policy::for_lock(lock_path, ctx)?;
        notes.extend(policy.enforce(policy.check_sandbox(false, false), &shown)?);
        return Ok(None);
    };
    if off {
        let policy = crate::policy::Policy::for_lock(lock_path, ctx)?;
        notes.extend(policy.enforce(policy.check_sandbox(true, true), &shown)?);
        notes.push(format!("sandbox off ({}): running on this machine", if no_sandbox { "--no-sandbox" } else { NO_SANDBOX }));
        return Ok(None);
    }
    let lock_dir = crate::sync::lock_dir_of(lock_path, ctx);
    if !trust {
        let store = crate::trust::TrustStore::load(ctx)?;
        if let Some(e) = crate::trust::sandbox_executable(lock, &lock_dir, ctx) {
            match store.status(&e.digest) {
                Some("trusted") => {}
                Some(_) => return Err(Error::Audit(format!("you denied the {} sandbox on this machine (hlock trust)", spec.runner))),
                None => return Err(Error::Audit(format!("the {} sandbox ({}) isn't trusted on this machine yet: approve it with hlock trust, or pass --trust", spec.runner, e.detail))),
            }
        }
    }
    let table = project_table(&lock_dir);
    Ok(Some(Sandbox::new(&spec, project, table.as_ref(), ctx)?))
}

/// What a lock's evidence was measured under, compared with the sandbox it runs in: a
/// note when they differ for a code-harness lock (an agent CLI's trials were simulated).
pub fn evidence_note(lock: &Lockfile) -> Option<String> {
    let ev = lock.evidence.sandbox.as_ref()?;
    let spec = Spec::from_lock(lock).ok()??;
    if lock.evidence.simulated || !spec.applies("trials") {
        return None;
    }
    let ran = ev.applies_to.iter().any(|k| k == "trials");
    (!ran || ev.runner != spec.runner || ev.network != spec.network).then(|| {
        let was = if ran { format!("runner {}, network {}", ev.runner, ev.network) } else { "trials unsandboxed".into() };
        format!("the evidence was measured without this sandbox ({was}); this lock runs under {}", spec.summary())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-sbx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    fn ctx(home: &Path) -> Ctx {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for b in ["docker", "jail", "openshell"] {
            std::fs::write(bin.join(b), "#!/bin/sh\n").unwrap();
        }
        Ctx {
            home: home.to_path_buf(),
            cwd: home.to_path_buf(),
            env: [("PATH", bin.to_str().unwrap()), ("HOME", home.to_str().unwrap()), ("ANTHROPIC_API_KEY", "sk-real"), ("TERM", "xterm"), ("AWS_SECRET_ACCESS_KEY", "aws")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            path: vec![bin],
        }
    }

    fn table(s: &str) -> Value {
        serde_json::to_value(toml::from_str::<toml::Value>(s).unwrap()).unwrap()
    }

    #[test]
    fn manifest_defaults_and_checks() {
        let d = tmp("spec");
        let t = table("runner = \"docker\"\nimage = \"ghcr.io/a/b@sha256:abc\"");
        let s = Spec::from_manifest(Some(&t), &d).unwrap().unwrap();
        assert_eq!((s.network.as_str(), s.secrets.as_str()), ("open", "env"));
        assert_eq!(s.applies_to, vec!["agent", "trials", "graders"]);
        assert!(Spec::from_manifest(Some(&table("runner = \"none\"")), &d).unwrap().is_none());
        // OpenShell denies by default and holds the keys: its defaults
        let o = Spec::from_manifest(Some(&table("runner = \"openshell\"\nimage = \"hlock-pi:1\"")), &d).unwrap().unwrap();
        assert_eq!((o.network.as_str(), o.secrets.as_str()), ("none", "proxy"));
        for (bad, why) in [
            ("runner = \"docker\"", "needs an image"),
            ("runner = \"docker\"\nimage = \"--privileged\"", "isn't an image reference"),
            ("runner = \"docker\"\nimage = \"x\"\nnetwork = \"allowlist\"", "list the hosts instead"),
            ("runner = \"docker\"\nimage = \"x\"\nnetwork = \"some\"", "isn't open, none or a list of hosts"),
            ("runner = \"jail\"\nsecrets = \"proxy\"", "\"openshell\" only"),
            ("runner = \"openshell\"", "needs an image"),
            ("runner = \"openshell\"\nimage = \"x\"\nnetwork = \"open\"", "no allow-all network rule"),
            ("runner = \"docker\"\nimage = \"x\"\nsecrets = \"proxy\"\nnetwork = \"none\"", "needs network"),
            ("runner = \"docker\"\nimage = \"x\"\napplies-to = [\"everything\"]", "applies-to has"),
            ("runner = \"docker\"\nimage = \"x\"\nprofile = \"../etc/passwd\"", "inside the project"),
            ("runner = \"docker\"\nimage = \"x\"\nbogus = 1", "unknown key"),
        ] {
            let e = Spec::from_manifest(Some(&table(bad)), &d).unwrap_err().to_string();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn lock_round_trip_and_weakening() {
        let d = tmp("lock");
        std::fs::write(d.join("p.sb"), "(version 1)").unwrap();
        let s = Spec::from_manifest(Some(&table("runner = \"docker\"\nimage = \"i@sha256:1\"\nnetwork = \"none\"\nprofile = \"p.sb\"")), &d).unwrap().unwrap();
        assert!(s.profile.as_ref().unwrap().digest.starts_with("sha256:"));
        let mut lock = Lockfile::default();
        lock.policy.sandbox = s.to_map();
        assert_eq!(Spec::from_lock(&lock).unwrap().unwrap(), s);
        assert!(Spec::from_lock(&Lockfile::default()).unwrap().is_none());
        let mut weaker = s.clone();
        weaker.network = "open".into();
        weaker.image = Some("i:latest".into());
        weaker.applies_to.retain(|k| k != "graders");
        weaker.profile = None;
        let w = weakened(Some(&s), Some(&weaker));
        assert_eq!(w.len(), 4, "{w:?}");
        assert_eq!(weakened(Some(&s), None), vec!["the sandbox is removed"]);
        assert!(weakened(None, Some(&s)).is_empty() && weakened(Some(&s), Some(&s)).is_empty());
    }

    #[test]
    fn network_allowlist() {
        let d = tmp("allow");
        let c = ctx(&d);
        let runner = "[runners.jail]\ncommand = [\"jail\", \"{network-args}\", \"--\"]\nnetwork-args = { none = [\"--unshare-net\"], allowlist = [\"--allow={host}\", \"--hosts={hosts}\"] }\n";
        let project_table = table(&format!("runner = \"jail\"\nnetwork = [\"api.github.com\", \"*.pypi.org\"]\n{runner}"));
        let spec = Spec::from_manifest(Some(&project_table), &d).unwrap().unwrap();
        assert_eq!((spec.network.as_str(), spec.hosts.as_slice()), ("allowlist", &["api.github.com".to_string(), "*.pypi.org".to_string()][..]));
        assert_eq!(spec.summary(), "jail (network allowlist api.github.com, *.pypi.org, secrets env)");
        // the lock records the hosts, and reads them back
        let mut lock = Lockfile::default();
        lock.policy.sandbox = spec.to_map();
        assert_eq!(lock.policy.sandbox["hosts"], json!(["api.github.com", "*.pypi.org"]));
        assert_eq!(Spec::from_lock(&lock).unwrap().unwrap(), spec);
        // {host} repeats per host; {hosts} is them all
        let sb = Sandbox::new(&spec, &d, Some(&project_table), &c).unwrap();
        let run = Run { kind: "agent".into(), argv: vec!["true".into()], cwd: d.clone(), home: d.join("h"), ..Default::default() };
        let a = sb.wrap(&run, &c).unwrap().argv.join(" ");
        assert!(a.starts_with("jail --allow=api.github.com --allow=*.pypi.org --hosts=api.github.com,*.pypi.org -- true"), "{a}");
        // a runner without an allowlist entry can't enforce it
        let plain = table("runner = \"jail\"\nnetwork = [\"api.github.com\"]\n[runners.jail]\ncommand = [\"jail\", \"{network-args}\"]\nnetwork-args = { none = [\"--unshare-net\"] }\n");
        let e = Sandbox::new(&Spec::from_manifest(Some(&plain), &d).unwrap().unwrap(), &d, Some(&plain), &c).unwrap_err().to_string();
        assert!(e.contains("can't enforce network = \"allowlist\""), "{e}");
        for (bad, why) in [
            ("runner = \"docker\"\nimage = \"x\"\nnetwork = [\"api.github.com\"]", "Docker can't limit"),
            ("runner = \"openshell\"\nimage = \"x\"\nnetwork = [\"api.github.com\"]", "OpenShell's policy decides"),
            ("runner = \"jail\"\nnetwork = []", "empty list of hosts"),
            ("runner = \"jail\"\nnetwork = [\"https://api.github.com\"]", "isn't a hostname"),
            ("runner = \"jail\"\nnetwork = [\"api.github.com:443\"]", "isn't a hostname"),
            ("runner = \"jail\"\nnetwork = [\"API.github.com\"]", "isn't a hostname"),
            ("runner = \"jail\"\nnetwork = [\"a.b\", \"a.b\"]", "twice"),
            ("runner = \"jail\"\nnetwork = [1]", "a list of hosts"),
        ] {
            let e = Spec::from_manifest(Some(&table(bad)), &d).unwrap_err().to_string();
            assert!(e.contains(why), "{bad}: {e}");
        }
        // reaching more is a weakening: none → allowlist → open, and hosts added
        let none = Spec { network: "none".into(), hosts: Vec::new(), ..spec.clone() };
        let open = Spec { network: "open".into(), hosts: Vec::new(), ..spec.clone() };
        let more = Spec { hosts: vec!["api.github.com".into(), "*.pypi.org".into(), "evil.example".into()], ..spec.clone() };
        assert_eq!(weakened(Some(&none), Some(&spec)), ["network none → allowlist"]);
        assert_eq!(weakened(Some(&spec), Some(&open)), ["network allowlist → open"]);
        assert_eq!(weakened(Some(&spec), Some(&more)), ["network allows more hosts (evil.example)"]);
        assert!(weakened(Some(&more), Some(&spec)).is_empty() && weakened(Some(&spec), Some(&none)).is_empty());
    }

    #[test]
    fn runners_user_wins_and_project_cant_shadow_docker() {
        let d = tmp("runners");
        let c = ctx(&d);
        std::fs::create_dir_all(d.join(".config/hlock")).unwrap();
        std::fs::write(d.join(".config/hlock/sandbox.toml"), "[runners.jail]\ncommand = [\"jail\", \"--user\"]\n").unwrap();
        let project = table("[runners.jail]\ncommand = [\"jail\", \"--project\"]\n[runners.other]\ncommand = [\"other\"]\n");
        let r = runners(&c, Some(&project)).unwrap();
        assert_eq!(r["jail"].command[1], "--user");
        assert_eq!((r["jail"].origin.as_str(), r["other"].origin.as_str(), r["docker"].origin.as_str()), ("user", "project", "builtin"));
        let e = runners(&c, Some(&table("[runners.docker]\ncommand = [\"evil\"]\n"))).unwrap_err().to_string();
        assert!(e.contains("can't define"), "{e}");
        let e = runners(&c, Some(&table("[runners.openshell]\ncommand = [\"evil\"]\n"))).unwrap_err().to_string();
        assert!(e.contains("can't define"), "{e}");
        assert_eq!(r["openshell"].origin, "builtin");
    }

    #[test]
    fn docker_wrap() {
        let d = tmp("docker");
        let c = ctx(&d);
        let project = d.join("proj");
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        std::fs::write(project.join("harness.toml"), "").unwrap();
        std::fs::write(project.join("harness.lock"), "").unwrap();
        std::fs::write(project.join("Cargo.lock"), "").unwrap();
        std::fs::create_dir_all(project.join("app")).unwrap();
        std::fs::write(project.join("app/.env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(project.join(".hlock/env/claude-code")).unwrap();
        let spec = Spec::from_manifest(Some(&table("runner = \"docker\"\nimage = \"img@sha256:1\"")), &project).unwrap().unwrap();
        let sb = Sandbox::new(&spec, &project, None, &c).unwrap();
        let run = Run {
            kind: "agent".into(),
            argv: vec!["/opt/homebrew/bin/claude".into(), "-p".into(), "hi".into()],
            cwd: project.clone(),
            home: project.join(SANDBOX_ENV_DIR).join("claude-code/home"),
            config: Some(project.join(SANDBOX_ENV_DIR).join("claude-code/config")),
            mounts: project_mounts(&project).unwrap(),
            env: [("CLAUDE_CONFIG_DIR", "/x"), ("ANTHROPIC_API_KEY", "sk-real"), ("DOCKER_HOST", "tcp://evil:2375"), ("LD_PRELOAD", "/tmp/x.so")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };
        let w = sb.wrap(&run, &c).unwrap();
        let a = w.argv.join(" ");
        assert!(a.starts_with("docker run --rm -i --name=hlock-"), "{a}");
        assert!(a.contains("--entrypoint=claude") && a.contains("--network=bridge") && a.contains("--cap-drop=ALL"), "{a}");
        assert!(a.ends_with("img@sha256:1 -p hi"), "{a}");
        let p = project.display();
        assert!(a.contains(&format!("src={p}/harness.toml,dst={p}/harness.toml,readonly")), "{a}");
        assert!(a.contains(&format!("dst={p}/.git/hooks,readonly")) && a.contains(&format!("dst={p}/.hlock,readonly")), "{a}");
        assert!(!a.contains("Cargo.lock"), "only harness locks are control paths: {a}");
        assert!(!a.contains(&format!("dst={p}/.hlock/sandbox-env,")), "the agent writes only its own root: {a}");
        assert!(a.contains(&format!("src={p}/.hlock/sandbox-env/.empty-dir,dst={p}/.hlock/env,readonly")), "unsandboxed roots are covered: {a}");
        // what would steer docker itself never comes from the lock or the project
        assert!(!w.env.contains_key("LD_PRELOAD") && !a.contains("DOCKER_HOST") && w.notes.iter().any(|n| n.contains("DOCKER_HOST")), "{:?}", w.notes);
        assert!(a.contains(&format!("src={p}/.hlock/sandbox-env/.empty,dst={p}/app/.env,readonly")), "{a}");
        // values never in argv, except HOME and the marker
        assert!(!a.contains("sk-real") && a.contains("-e ANTHROPIC_API_KEY ") && a.contains("-e HLOCK_IN_SANDBOX=1"), "{a}");
        assert!(!w.env_names.contains(&"AWS_SECRET_ACCESS_KEY".to_string()) && !w.env.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert_eq!(w.env["ANTHROPIC_API_KEY"], "sk-real");
        assert!(w.env_names.contains(&"TERM".to_string()) && !w.env_names.contains(&"PATH".to_string()));
        assert_eq!(w.kill[..2], ["docker".to_string(), "kill".to_string()]);
        // parents before what's inside them
        let order: Vec<&Mount> = w.mounts.iter().collect();
        assert!(order.iter().position(|m| m.dst == project).unwrap() < order.iter().position(|m| m.dst == project.join(".hlock")).unwrap());
    }

    #[test]
    fn openshell_wrap_plans_a_session() {
        let d = tmp("openshell");
        let c = ctx(&d);
        let project = d.join("proj");
        std::fs::create_dir_all(project.join("app")).unwrap();
        std::fs::write(project.join("harness.toml"), "").unwrap();
        std::fs::write(project.join("app/.env"), "SECRET=1").unwrap();
        let spec = Spec::from_manifest(Some(&table("runner = \"openshell\"\nimage = \"hlock-claude-code:1\"")), &project).unwrap().unwrap();
        let sb = Sandbox::new(&spec, &project, None, &c).unwrap();
        let home = project.join(SANDBOX_ENV_DIR).join("claude-code/home");
        let run = Run {
            kind: "agent".into(),
            argv: vec!["claude".into()],
            cwd: project.clone(),
            home: home.clone(),
            mounts: project_mounts(&project).unwrap(),
            env: [("CLAUDE_CONFIG_DIR", "/x"), ("XDG_CONFIG_HOME", "/root-of-its-own")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let w = sb.wrap(&run, &c).unwrap();
        let s = w.session.as_ref().unwrap();
        // the command's variables are kept apart from the CLI's (one of them would move the
        // CLI's gateway config)
        assert!(!w.env.contains_key("XDG_CONFIG_HOME") && !w.env.contains_key("CLAUDE_CONFIG_DIR"));
        assert_eq!(w.session_env["XDG_CONFIG_HOME"], "/root-of-its-own");
        assert_eq!(w.argv[..3], ["openshell".to_string(), "sandbox".into(), "exec".into()]);
        assert!(w.kill.is_empty() && s.name.starts_with("hlock-") && s.image == "hlock-claude-code:1");
        // the project comes back; its control files and .env don't; HOME is a place of its own
        let p = &s.places[0];
        assert!(p.host == project && p.copy_back);
        assert!(p.keep.contains(&project.join("harness.toml")) && p.skip.contains(&project.join("app/.env")));
        assert!(s.places.iter().any(|x| x.host == home && x.rewrite && !x.copy_back));
        // the provider reads the real key from the runner's environment; the command doesn't get it
        assert_eq!(w.env["ANTHROPIC_API_KEY"], "sk-real");
        assert!(!s.env_names.contains(&"ANTHROPIC_API_KEY".to_string()) && s.env_names.contains(&"CLAUDE_CONFIG_DIR".to_string()));
        assert_eq!(w.env["HOME"], d.to_string_lossy());
    }

    #[test]
    fn command_runner_template() {
        let d = tmp("cmd");
        let c = ctx(&d);
        std::fs::write(d.join("agent.sb"), "(version 1)").unwrap();
        let project_table = table(
            "runner = \"jail\"\nnetwork = \"none\"\nprofile = \"agent.sb\"\n[runners.jail]\ncommand = [\"jail\", \"-f\", \"{profile}\", \"-D\", \"CWD={cwd}\", \"{network-args}\", \"{mounts}\", \"--\"]\nmount-args = [\"--bind\", \"{path}\", \"{path}\"]\nmount-ro-args = [\"--ro-bind\", \"{src}\", \"{path}\"]\nnetwork-args = { none = [\"--unshare-net\"], open = [] }\n",
        );
        let spec = Spec::from_manifest(Some(&project_table), &d).unwrap().unwrap();
        let sb = Sandbox::new(&spec, &d, Some(&project_table), &c).unwrap();
        let run = Run { kind: "graders".into(), argv: vec!["sh".into(), "-c".into(), "true".into()], cwd: d.join("ws"), home: d.join("h"), mounts: vec![Mount::ro(&d.join("agent.sb"))], ..Default::default() };
        std::fs::create_dir_all(d.join("ws")).unwrap();
        let w = sb.wrap(&run, &c).unwrap();
        let a = w.argv.join(" ");
        assert!(a.starts_with(&format!("jail -f {}/.hlock/sandbox-env/profiles/", d.display())), "{a}");
        assert!(a.contains(&format!("CWD={}/ws --unshare-net --bind", d.display())), "{a}");
        assert!(a.contains(&format!("--ro-bind {0}/agent.sb {0}/agent.sb", d.display())), "{a}");
        assert!(a.ends_with("-- sh -c true"), "{a}");
        assert!(w.kill.is_empty() && w.env["HLOCK_IN_SANDBOX"] == "1" && !w.env.contains_key("ANTHROPIC_API_KEY"));
        // a changed profile is refused
        std::fs::write(d.join("agent.sb"), "(version 1)(deny default)").unwrap();
        let e = Sandbox::new(&spec, &d, Some(&project_table), &c).unwrap_err().to_string();
        assert!(e.contains("changed since the lock"), "{e}");
        // and a runner whose template needs a profile the lock doesn't have
        assert!(Sandbox::new(&Spec { runner: "jail".into(), network: "open".into(), profile: None, ..spec.clone() }, &d, Some(&project_table), &c).unwrap_err().to_string().contains("needs a profile"));
    }

    #[test]
    fn undefined_runner_and_bad_paths() {
        let d = tmp("undef");
        let c = ctx(&d);
        let spec = Spec { runner: "ghost".into(), network: "open".into(), secrets: "env".into(), applies_to: vec!["agent".into()], ..Default::default() };
        assert!(Sandbox::new(&spec, &d, None, &c).unwrap_err().to_string().contains("isn't defined on this machine"));
        let bad = d.join("a,b");
        std::fs::create_dir_all(&bad).unwrap();
        assert!(normalize(vec![Mount::rw(&bad)]).unwrap_err().to_string().contains("misread"));
        assert!(valid_env_name("A_1") && !valid_env_name("1A") && !valid_env_name("A=B") && !valid_env_name(""));
        assert_eq!(docker_program("/Users/x/.venv/bin/python3.12", &[]), "python3");
        let script = d.join("check.sh");
        std::fs::write(&script, "").unwrap();
        assert_eq!(docker_program(script.to_str().unwrap(), &[Mount::ro(&d)]), script.to_str().unwrap());
    }

    #[test]
    fn trust_digest_tracks_inputs() {
        let s = Spec { runner: "docker".into(), image: Some("a".into()), network: "open".into(), secrets: "env".into(), applies_to: vec!["agent".into()], ..Default::default() };
        let base = executable(&s, Some(&Runner::docker())).digest;
        let mut t = s.clone();
        t.image = Some("b".into());
        assert_ne!(base, executable(&t, Some(&Runner::docker())).digest);
        let r = Runner { name: "j".into(), origin: "project".into(), command: vec!["j".into()], ..Default::default() };
        let mut r2 = r.clone();
        r2.command.push("--evil".into());
        let s2 = Spec { runner: "j".into(), image: None, ..s.clone() };
        assert_ne!(executable(&s2, Some(&r)).digest, executable(&s2, Some(&r2)).digest);
        assert_eq!(executable(&s, None).kind, "sandbox");
    }

    #[test]
    fn lock_diff_marks_a_weaker_sandbox() {
        let spec = Spec { runner: "docker".into(), image: Some("i@sha256:1".into()), network: "none".into(), secrets: "env".into(), applies_to: KINDS.iter().map(|k| k.to_string()).collect(), ..Default::default() };
        let (mut a, mut b) = (Lockfile::default(), Lockfile::default());
        a.policy.sandbox = spec.to_map();
        b.policy.sandbox = Spec { network: "open".into(), ..spec.clone() }.to_map();
        let d = crate::capture::diff_locks(&a, &b);
        assert!(d.iter().any(|l| l.starts_with("~ sandbox: docker (network none") && l.ends_with("weakened: network none → open")), "{d:?}");
        let d = crate::capture::diff_locks(&a, &Lockfile::default());
        assert!(d.iter().any(|l| l.starts_with("- sandbox docker") && l.contains("weakened: the sandbox is removed")), "{d:?}");
        assert!(crate::capture::diff_locks(&Lockfile::default(), &a).iter().any(|l| l.starts_with("+ sandbox docker") && !l.contains("weakened")));
    }

    #[test]
    fn control_files_and_disable() {
        let d = tmp("control");
        std::fs::write(d.join("harness.toml"), "a").unwrap();
        let before = control_snapshot(&d);
        std::fs::write(d.join("harness.toml"), "b").unwrap();
        std::fs::write(d.join(".envrc"), "curl x|sh").unwrap();
        let ch = control_changes(&before, &control_snapshot(&d));
        assert_eq!(ch, vec!["created .envrc", "changed harness.toml"]);
        let mut c = ctx(&d);
        assert_eq!(disabled(false, &c), (false, None));
        c.env.insert(NO_SANDBOX.into(), "1".into());
        assert_eq!(disabled(false, &c).0, true);
        c.env.insert(IN_SANDBOX.into(), "1".into());
        let (off, note) = disabled(false, &c);
        assert!(!off && note.is_some());
        assert!(disabled(true, &c).0);
    }
}
