//! `hlock meta`: the locked agent hlock uses for its own judgment work
//! (`specs/meta-harness.md`).
//!
//! The meta harness is pinned in `harness.meta.lock` (an ordinary pinned lock), and
//! optionally configured in `harness.meta.toml`. A *job* is a prompt rendered from
//! project state ([`render`]) plus a check hlock runs itself afterwards
//! ([`verify`]). [`prepare`] builds the command that runs a job in the meta harness,
//! isolated the same way `hlock launch` is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capture::{read_config, render_config, PinOutcome};
use crate::error::{Error, Result};
use crate::launch::{self, LaunchOptions};
use crate::localize::{choose_local, subst_value, LocalChoice};
use crate::lockfile::Lockfile;
use crate::manifest::Manifest;
use crate::profiles::{probe_harness, probe_runtime, Ctx, HarnessProfile, LocalProvider, Registry};
use crate::sync::{self, set_pointer, SyncOptions};
use crate::tasks::{EvalPack, PackFile, PLACEHOLDER};

/// The meta harness's optional manifest.
pub const META_MANIFEST_PATH: &str = "harness.meta.toml";
/// The pinned meta harness.
pub const META_LOCK_PATH: &str = "harness.meta.lock";
/// Where run records go, relative to the project.
pub const RUNS_DIR: &str = ".hlock/meta/runs";
/// Set in the meta harness's environment: the variables isolation changed, as a JSON
/// object of original values (`null` for unset). hlock processes put them back.
pub const RESTORE_ENV: &str = "HLOCK_META_RESTORE";

/// Stands in for the prompt while the command is built, before the prompt is rendered.
const PROMPT_TOKEN: &str = "\u{1}hlock-meta-prompt\u{1}";

/// The agent that ships inside hlock (see [`crate::agent`]): the default meta harness.
pub const BUILTIN: &str = "builtin";
/// Harnesses the meta harness can be, most preferred first: the built-in agent, then
/// open-source harnesses the user installs.
pub const META_HARNESSES: &[&str] = &[BUILTIN, "pi", "codex", "goose", "opencode", "hermes"];
/// The model `hlock meta init` picks for each API provider.
pub const DEFAULT_MODELS: &[(&str, &str)] = &[("anthropic", "claude-sonnet-5-5"), ("openai", "gpt-5.5"), ("google", "gemini-3.5-flash")];

/// Built-in jobs: name and description.
pub const BUILTIN_JOBS: [(&str, &str); 3] = [
    ("install", "Install what the project's lock still needs on this machine"),
    ("bootstrap-evals", "Fill in an eval pack with real tasks from this project"),
    ("update", "Update the lock's skills, MCP servers and plugins"),
];

// --------------------------------------------------------------------------- //
// harness.meta.toml
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JobSpec {
    pub description: Option<String>,
    /// The prompt of a custom job; `{lock}`, `{manifest}`, `{project}` and `{meta_lock}` are substituted.
    pub prompt: Option<String>,
    /// A shell command run in the project after the agent exits; exit 0 means verified.
    pub verify: Option<String>,
    /// Text appended to the job's prompt (built-in or custom).
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RunDefaults {
    /// `interactive` (default) or `headless`.
    pub mode: Option<String>,
}

/// `[model]`: what the meta harness talks to. An API provider (`provider`, with its key
/// in the `key-env` variable), or a local runtime (`runtime`) serving open weights.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct MetaModel {
    pub provider: Option<String>,
    pub name: Option<String>,
    pub key_env: Option<String>,
    pub runtime: Option<String>,
    pub base_url: Option<String>,
}

impl MetaModel {
    pub fn is_local(&self) -> bool {
        self.runtime.is_some()
    }
}

/// `[agent]`: what the built-in agent may do.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct AgentSpec {
    /// Command prefixes that run without asking (default: `hlock` and read-only git).
    pub allow: Option<Vec<String>>,
    pub max_steps: Option<u32>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MetaManifest {
    pub target: BTreeMap<String, Value>,
    pub model: MetaModel,
    pub agent: AgentSpec,
    pub run: RunDefaults,
    pub jobs: BTreeMap<String, JobSpec>,
}

impl MetaManifest {
    pub fn load(path: &Path) -> Result<MetaManifest> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Manifest(format!("cannot read {}: {e}", path.display())))?;
        MetaManifest::from_str(&text)
    }

    pub fn from_str(text: &str) -> Result<MetaManifest> {
        let m: MetaManifest = toml::from_str(text).map_err(|e| Error::Manifest(format!("invalid {META_MANIFEST_PATH}: {e}")))?;
        if let Some(mode) = &m.run.mode {
            if !matches!(mode.as_str(), "interactive" | "headless") {
                return Err(Error::Manifest(format!("{META_MANIFEST_PATH}: [run] mode must be \"interactive\" or \"headless\", not {mode:?}")));
            }
        }
        if m.model.name.is_some() && m.model.provider.is_none() && m.model.runtime.is_none() {
            return Err(Error::Manifest(format!("{META_MANIFEST_PATH}: [model] needs a `provider` (an API provider) or a `runtime` (a local one)")));
        }
        for (name, spec) in &m.jobs {
            let builtin = BUILTIN_JOBS.iter().any(|(b, _)| b == name);
            if builtin && (spec.prompt.is_some() || spec.verify.is_some()) {
                return Err(Error::Manifest(format!(
                    "{META_MANIFEST_PATH}: [jobs.{name}] is a built-in job; it takes `instructions` only, not `prompt` or `verify`"
                )));
            }
            if !builtin && spec.prompt.as_deref().map_or(true, |p| p.trim().is_empty()) {
                return Err(Error::Manifest(format!("{META_MANIFEST_PATH}: [jobs.{name}] needs a `prompt`")));
            }
        }
        Ok(m)
    }

    /// `[target] harness`: the meta harness.
    pub fn target_harness(&self) -> Option<String> {
        self.target.get("harness").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
    }

    pub fn headless(&self) -> bool {
        self.run.mode.as_deref() == Some("headless")
    }
}

// --------------------------------------------------------------------------- //
// Choosing and configuring the meta harness
// --------------------------------------------------------------------------- //

/// Whether `name` names the built-in agent.
pub fn is_builtin(name: &str) -> bool {
    matches!(name, BUILTIN | "hlock" | "hlock-agent")
}

/// A profile standing in for the built-in agent, so choosing a model works the same way.
pub fn builtin_profile() -> HarnessProfile {
    HarnessProfile {
        id: BUILTIN.into(),
        name: "built-in agent".into(),
        kind: "builtin".into(),
        open_source: Some(true),
        local_models: Some(true),
        api_keys: crate::agent::PROVIDERS.iter().map(|(p, _, k)| crate::profiles::ApiKey { provider: p.to_string(), env: k.to_string() }).collect(),
        origin: "builtin".into(),
        ..Default::default()
    }
}

/// Why a harness can't be the meta harness (`None` when it can).
pub fn ineligible(p: &HarnessProfile) -> Option<String> {
    if p.kind == "custom" || p.kind == "builtin" {
        return None;
    }
    if p.kind != "cli" {
        return Some(format!("{} is a framework; declare a project harness built on it under [harnesses.<id>] in harness.toml", p.name));
    }
    if p.open_source != Some(true) {
        return Some(format!("{} isn't open source; the meta harness must be one of: {}", p.name, META_HARNESSES.join(", ")));
    }
    if p.config_root.env.is_none() {
        return Some(format!("{}'s config root can't be relocated, so it can't have a config of its own", p.name));
    }
    if p.launch.prompt.is_empty() && p.launch.interactive.is_empty() {
        return Some(format!("the {} profile doesn't say how to give it a prompt ([launch] prompt or interactive)", p.name));
    }
    None
}

/// The meta harness's profile: the built-in agent's, or an eligible registry profile.
fn meta_profile<'a>(reg: &'a Registry, name: &str, builtin: &'a HarnessProfile) -> Result<&'a HarnessProfile> {
    if is_builtin(name) {
        Ok(builtin)
    } else {
        eligible_harness(reg, name)
    }
}

fn eligible_harness<'a>(reg: &'a Registry, name: &str) -> Result<&'a HarnessProfile> {
    let p = reg.require_harness(name)?;
    match ineligible(p) {
        Some(why) => Err(Error::Profile(why)),
        None => Ok(p),
    }
}

/// What `hlock meta init` asks for; everything is optional.
#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    pub harness: Option<String>,
    /// An API provider (`anthropic`, `openai`, …) or `local`.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// A local runtime (implies `local`).
    pub runtime: Option<String>,
    pub memory_gb: Option<f64>,
    /// Where the model's API lives, overriding the provider's or runtime's default.
    pub base_url: Option<String>,
}

/// The meta harness and model `hlock meta init` settled on, and what's still missing.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Choice {
    pub harness: String,
    pub harness_name: String,
    pub why: String,
    pub installed: bool,
    /// The command that installs the harness, when it isn't installed.
    pub install: Option<String>,
    pub model: MetaModel,
    pub model_why: String,
    /// The API key variable is set (always true for local models).
    pub key_set: bool,
    pub local: Option<LocalChoice>,
}

/// Choose the meta harness and its model from the options, the API keys in the
/// environment, and the local runtimes installed.
pub fn choose(reg: &Registry, ctx: &Ctx, opts: &InitOptions) -> Result<Choice> {
    let builtin = builtin_profile();
    let (profile, why) = match &opts.harness {
        Some(h) => (meta_profile(reg, h, &builtin)?, "chosen with --harness".to_string()),
        None => (&builtin, "ships with hlock; nothing to install".to_string()),
    };
    let installed = profile.kind != "cli" || probe_harness(profile, ctx).installed;
    let install = (!installed)
        .then(|| {
            profile.install.iter().find_map(|m| {
                let tool = match m.method.as_str() {
                    "npm" => "npm",
                    "brew" | "brew-cask" => "brew",
                    "uv-tool" => "uv",
                    "pip" => "python3",
                    "script" => "sh",
                    _ => return None,
                };
                ctx.which(tool)?;
                sync::install_command(&m.method, m.package.as_deref(), None, m.command.as_deref()).map(|argv| sync::shell_join(&argv))
            })
        })
        .flatten();
    let key_for = |provider: &str| profile.api_keys.iter().find(|k| k.provider == provider).map(|k| k.env.clone());
    let can_local = matches!(profile.kind.as_str(), "custom" | "builtin") || (profile.local_models == Some(true) && !profile.local_provider.is_empty());
    let want_local = opts.runtime.is_some() || opts.provider.as_deref() == Some("local");
    let local_installed = || crate::localize::DEFAULT_RUNTIMES.iter().filter_map(|r| reg.runtime(r)).any(|r| probe_runtime(r, ctx).installed);
    let api = |provider: &str, why: String| -> Result<(MetaModel, String, bool)> {
        let key = key_for(provider).ok_or_else(|| {
            let known: Vec<&str> = profile.api_keys.iter().map(|k| k.provider.as_str()).collect();
            Error::Profile(format!("{} doesn't support the API provider {provider:?} (it supports: {})", profile.name, if known.is_empty() { "none".to_string() } else { known.join(", ") }))
        })?;
        let name = opts.model.clone().or_else(|| DEFAULT_MODELS.iter().find(|(p, _)| *p == provider).map(|(_, m)| m.to_string())).ok_or_else(|| {
            Error::Profile(format!("there's no default {provider} model; pass --model"))
        })?;
        let set = ctx.var(&key).is_some();
        Ok((MetaModel { provider: Some(provider.into()), name: Some(name), key_env: Some(key), ..Default::default() }, why, set))
    };
    let (model, model_why, key_set, local) = if want_local || (opts.provider.is_none() && !profile.api_keys.iter().any(|k| ctx.var(&k.env).is_some()) && can_local && local_installed()) {
        if !can_local {
            return Err(Error::Profile(format!("{} can't use a local model endpoint", profile.name)));
        }
        let lc = choose_local(reg, ctx, opts.runtime.as_deref(), opts.model.as_deref(), opts.memory_gb)?;
        let why = if want_local { "chosen with --local".to_string() } else { format!("no API key is set and {} is installed", lc.runtime_name) };
        let m = MetaModel { runtime: Some(lc.runtime.clone()), name: Some(lc.model.clone()), ..Default::default() };
        (m, why, true, Some(lc))
    } else if let Some(p) = &opts.provider {
        let (m, w, k) = api(p, "chosen with --provider".into())?;
        (m, w, k, None)
    } else if let Some(k) = profile.api_keys.iter().find(|k| ctx.var(&k.env).is_some()) {
        let (m, w, set) = api(&k.provider, format!("{} is set", k.env))?;
        (m, w, set, None)
    } else if let Some(k) = profile.api_keys.first() {
        let (m, w, set) = api(&k.provider, format!("no API key or local runtime found; {} is {}'s first provider", k.provider, profile.name))?;
        (m, w, set, None)
    } else if can_local {
        let lc = choose_local(reg, ctx, None, opts.model.as_deref(), opts.memory_gb)?;
        let m = MetaModel { runtime: Some(lc.runtime.clone()), name: Some(lc.model.clone()), ..Default::default() };
        (m, format!("{} only talks to local models", profile.name), true, Some(lc))
    } else {
        (MetaModel::default(), format!("{} uses its own model settings", profile.name), true, None)
    };
    let mut model = model;
    if opts.base_url.is_some() {
        model.base_url = opts.base_url.clone();
    }
    Ok(Choice { harness: profile.id.clone(), harness_name: profile.name.clone(), why, installed, install, model, model_why, key_set, local })
}

/// A starter `harness.meta.toml` for a choice.
pub fn manifest_text(c: &Choice) -> String {
    // `key = value` lines with their comments lined up
    let line = |kv: String, comment: &str| if comment.is_empty() { format!("{kv}\n") } else { format!("{kv:<30}# {comment}\n") };
    let mut s = String::from("# hlock meta manifest: the agent hlock uses for its own work (hlock meta --help).\n");
    s.push_str("[target]\n");
    s.push_str(&line(format!("harness = \"{}\"", c.harness), &format!("builtin (ships with hlock), or one you install: {}", META_HARNESSES[1..].join(", "))));
    let m = &c.model;
    if m.name.is_some() || m.runtime.is_some() {
        s.push_str("\n[model]\n");
        if let Some(r) = &m.runtime {
            s.push_str(&line(format!("runtime = \"{r}\""), "a local runtime serving open weights (hlock runtime list)"));
        }
        if let Some(p) = &m.provider {
            s.push_str(&line(format!("provider = \"{p}\""), ""));
        }
        if let Some(n) = &m.name {
            s.push_str(&line(format!("name = \"{n}\""), ""));
        }
        if let Some(k) = &m.key_env {
            s.push_str(&line(format!("key-env = \"{k}\""), "the variable holding the API key; the key is never written"));
        }
        if let Some(u) = &m.base_url {
            s.push_str(&line(format!("base-url = \"{u}\""), ""));
        }
    }
    if is_builtin(&c.harness) {
        s.push_str("\n[agent]\n# commands that run without asking; others need your OK, and unattended runs refuse them\n");
        s.push_str(&line(format!("allow = [{}]", crate::agent::DEFAULT_ALLOW.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", ")), ""));
    }
    s.push_str("\n[run]\n");
    s.push_str(&line("mode = \"interactive\"".into(), "or \"headless\""));
    s.push_str("\n# Extra instructions for a built-in job, or a custom job:\n# [jobs.install]\n# instructions = \"Prefer Homebrew. Never use sudo.\"\n");
    s
}

/// The meta harness's own environment: a context whose config root is its dedicated
/// directory (the isolated root `hlock launch` uses for the meta lock), the variables
/// that point there, and the root.
pub fn meta_env(profile: &HarnessProfile, ctx: &Ctx, meta_lock_path: &Path) -> (Ctx, BTreeMap<String, String>, PathBuf) {
    let sopts = SyncOptions { prefix: Some(launch::env_prefix(meta_lock_path, ctx)), ..Default::default() };
    let (t, _, iso) = sync::target_ctx(profile, ctx, &sopts);
    let root = profile.config_root(&t);
    (t, iso, root)
}

/// Write `[model]` into the meta harness's own config (never the user's).
pub fn configure(profile: &HarnessProfile, model: &MetaModel, tctx: &Ctx) -> Result<Vec<PathBuf>> {
    if profile.kind != "cli" || (model.name.is_none() && model.runtime.is_none()) {
        return Ok(Vec::new());
    }
    let name = model.name.clone().unwrap_or_default();
    let (templates, vars): (&[LocalProvider], Vec<(&str, String)>) = if let Some(rt) = &model.runtime {
        let base = model.base_url.clone().unwrap_or_else(|| "http://localhost:11434/v1".into());
        let host = base.trim_end_matches('/').trim_end_matches("/v1").to_string();
        (&profile.local_provider, vec![("provider", rt.clone()), ("model", name), ("base_url", base), ("host", host)])
    } else {
        (&profile.api_provider, vec![("provider", model.provider.clone().unwrap_or_default()), ("model", name)])
    };
    if templates.is_empty() {
        return Err(Error::Profile(format!(
            "the {} profile doesn't say how to set {} model; configure it in `hlock meta launch`, then run `hlock meta pin`",
            profile.name,
            if model.is_local() { "a local" } else { "an API" }
        )));
    }
    let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut per_file: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    for lp in templates {
        for (ptr, v) in &lp.set {
            let ptr = subst_value(&Value::String(ptr.clone()), &vars).as_str().unwrap_or_default().to_string();
            per_file.entry(lp.file.clone()).or_default().push((ptr, subst_value(v, &vars)));
        }
    }
    let mut written = Vec::new();
    for (file, sets) in per_file {
        let spec = profile.files.get(&file).ok_or_else(|| Error::Profile(format!("the {} profile has no file {file:?}", profile.id)))?;
        let path = profile.file_path(&file, tctx).ok_or_else(|| Error::Profile(format!("the {} profile has no file {file:?}", profile.id)))?;
        let mut body = read_config(&path, &spec.format)?.unwrap_or_else(|| Value::Object(Default::default()));
        for (ptr, v) in sets {
            set_pointer(&mut body, &ptr, v);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, render_config(&body, &spec.format).map_err(Error::Profile)?)?;
        written.push(path);
    }
    Ok(written)
}

/// The base URL `[model]` points at for a local runtime.
fn local_base_url(reg: &Registry, model: &MetaModel) -> Option<String> {
    let rt = reg.runtime(model.runtime.as_deref()?)?;
    Some(model.base_url.clone().unwrap_or_else(|| format!("http://localhost:{}/v1", rt.ports.first().copied().unwrap_or(8080))))
}

/// `hlock meta pin`: write `[model]` into the meta harness's own config, then pin that
/// config into `out`.
pub fn pin(reg: &Registry, ctx: &Ctx, meta: &MetaManifest, out: &Path, project: Option<&str>, vendor: bool, force: bool) -> Result<PinOutcome> {
    let harness = meta.target_harness().ok_or_else(|| Error::Manifest(format!("{META_MANIFEST_PATH} has no [target] harness; run `hlock meta init`")))?;
    if is_builtin(&harness) {
        return pin_builtin(reg, meta, out, project);
    }
    let profile = eligible_harness(reg, &harness)?;
    let (tctx, _, root) = meta_env(profile, ctx, out);
    std::fs::create_dir_all(&root)?;
    let mut model = meta.model.clone();
    if model.is_local() {
        model.base_url = local_base_url(reg, &model);
    }
    configure(profile, &model, &tctx)?;
    crate::capture::pin(reg, &tctx, Some(&profile.id), None, project, out, vendor, force)
}

/// The lock for the built-in agent: hlock's version, the model, and what it may do.
fn builtin_lock(reg: &Registry, meta: &MetaManifest, project: Option<&str>) -> Result<Lockfile> {
    let m = &meta.model;
    let model = m.name.clone().ok_or_else(|| Error::Manifest(format!("{META_MANIFEST_PATH}: the built-in agent needs a [model] name")))?;
    let mut llm = crate::lockfile::LLM { role: "default".into(), model, resolved_from: Some(format!("{META_MANIFEST_PATH}#/model")), ..Default::default() };
    if let Some(rt) = &m.runtime {
        llm.provider = rt.clone();
        llm.runtime = Some(rt.clone());
        llm.endpoint = Some(local_base_url(reg, m).ok_or_else(|| Error::Profile(format!("unknown runtime {rt:?} (see hlock runtime list)")))?);
    } else {
        let provider = m.provider.clone().ok_or_else(|| Error::Manifest(format!("{META_MANIFEST_PATH}: [model] needs a provider or a runtime")))?;
        if m.base_url.is_none() && crate::agent::provider_base_url(&provider).is_none() {
            return Err(Error::Manifest(format!(
                "the built-in agent doesn't know where {provider:?} is; set [model] base-url to its OpenAI-compatible endpoint"
            )));
        }
        llm.endpoint = m.base_url.clone();
        if let Some(k) = m.key_env.clone().or_else(|| crate::agent::provider_key_env(&provider).map(str::to_string)) {
            llm.params.insert("key-env".into(), Value::String(k));
        }
        llm.provider = provider;
    }
    let version = env!("CARGO_PKG_VERSION").to_string();
    let mut lf = Lockfile { kind: Some("pin".into()), llm: vec![llm], ..Default::default() };
    lf.stack.insert("name".into(), Value::String(project.unwrap_or(BUILTIN).to_string()));
    lf.stack.insert("harness".into(), Value::String(BUILTIN.into()));
    lf.stack.insert("selection".into(), Value::String("pin".into()));
    lf.harness = crate::lockfile::Harness { name: BUILTIN.into(), version: Some(version.clone()), source: Some(format!("hlock@{version}")), ..Default::default() };
    lf.policy.tool_allowlist = meta.agent.allow.clone().unwrap_or_else(|| crate::agent::DEFAULT_ALLOW.iter().map(|s| s.to_string()).collect());
    lf.policy.max_steps = meta.agent.max_steps.unwrap_or(crate::agent::DEFAULT_MAX_STEPS);
    lf.policy.tool_timeout_seconds = Some(meta.agent.timeout_seconds.unwrap_or(crate::agent::DEFAULT_TIMEOUT_SECONDS));
    lf.provenance.backend = "pin".into();
    Ok(lf)
}

fn pin_builtin(reg: &Registry, meta: &MetaManifest, out: &Path, project: Option<&str>) -> Result<PinOutcome> {
    let lock = builtin_lock(reg, meta, project)?;
    let previous = Lockfile::load(out).ok().filter(Lockfile::is_pin);
    let mut diff = previous.as_ref().map(|p| crate::capture::diff_locks(p, &lock)).unwrap_or_default();
    if let Some(p) = &previous {
        diff.extend(policy_diff(&p.policy, &lock.policy));
    }
    lock.save(out)?;
    Ok(PinOutcome {
        lock,
        path: out.to_path_buf(),
        selection: crate::profiles::Selection { id: BUILTIN.into(), reason: "ships with hlock".into() },
        diff,
        previous: previous.is_some(),
        vendored: Vec::new(),
        warnings: Vec::new(),
    })
}

/// What the built-in agent may do, as drift lines.
fn policy_diff(old: &crate::lockfile::Policy, new: &crate::lockfile::Policy) -> Vec<String> {
    let mut out = Vec::new();
    if old.tool_allowlist != new.tool_allowlist {
        out.push(format!("~ agent allow: {} → {}", old.tool_allowlist.join(", "), new.tool_allowlist.join(", ")));
    }
    if old.max_steps != new.max_steps {
        out.push(format!("~ agent max-steps: {} → {}", old.max_steps, new.max_steps));
    }
    if old.tool_timeout_seconds != new.tool_timeout_seconds {
        out.push(format!("~ agent timeout-seconds: {:?} → {:?}", old.tool_timeout_seconds, new.tool_timeout_seconds));
    }
    out
}

/// `hlock meta check`: what drifted between `harness.meta.toml`, the meta lock, and the
/// meta harness's own config. Empty when everything matches.
pub fn check(reg: &Registry, ctx: &Ctx, meta: Option<&MetaManifest>, lock: &Lockfile, lock_path: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if is_builtin(&lock.harness.name) {
        let want = env!("CARGO_PKG_VERSION");
        if lock.harness.version.as_deref() != Some(want) {
            out.push(format!("~ harness builtin: the lock pins hlock {}, this is hlock {want}", lock.harness.version.as_deref().unwrap_or("?")));
        }
        if let Some(m) = meta {
            if m.target_harness().is_some_and(|t| !is_builtin(&t)) {
                out.push(format!("~ harness: {META_MANIFEST_PATH} targets {}, the lock pins the built-in agent", m.target_harness().unwrap_or_default()));
            } else {
                let fresh = builtin_lock(reg, m, lock.stack.get("name").and_then(Value::as_str))?;
                out.extend(crate::capture::diff_locks(lock, &fresh));
                out.extend(policy_diff(&lock.policy, &fresh.policy));
            }
        }
        return Ok(out);
    }
    let profile = reg.require_harness(&lock.harness.name)?;
    if let Some(why) = ineligible(profile) {
        out.push(format!("! {why}"));
    }
    if let Some(m) = meta {
        if let Some(t) = m.target_harness() {
            if reg.harness(&t).map(|p| p.id.clone()).unwrap_or(t.clone()) != profile.id {
                out.push(format!("~ harness: {META_MANIFEST_PATH} targets {t}, the lock pins {}", profile.id));
            }
        }
        if let Some(want) = &m.model.name {
            let have = lock.llm.iter().find(|l| l.role == "default").or_else(|| lock.llm.first()).map(|l| l.model.clone());
            if have.as_deref() != Some(want.as_str()) {
                out.push(format!("~ model: {META_MANIFEST_PATH} sets {want}, the lock pins {}", have.unwrap_or_else(|| "none".into())));
            }
        }
    }
    let (tctx, _, _) = meta_env(profile, ctx, lock_path);
    out.extend(crate::capture::check_pin(lock, reg, &tctx)?);
    Ok(out)
}

/// Before a run: what would stop the meta harness from reaching its model.
fn preflight(reg: &Registry, ctx: &Ctx, meta: Option<&MetaManifest>, profile: &HarnessProfile, notes: &mut Vec<String>) -> Result<()> {
    let Some(m) = meta.map(|m| &m.model) else { return Ok(()) };
    if let Some(key) = &m.key_env {
        if ctx.var(key).is_none() {
            return Err(Error::Profile(format!(
                "{key} isn't set; {} reads its {} API key from it. Export it and run the job again",
                profile.name,
                m.provider.as_deref().unwrap_or("provider")
            )));
        }
    }
    if let Some(rt) = m.runtime.as_deref().and_then(|r| reg.runtime(r)) {
        if !probe_runtime(rt, ctx).installed {
            return Err(Error::Profile(format!("{} isn't installed, so the meta harness has no model to talk to; run `hlock meta sync --install`", rt.name)));
        }
        if let Some(name) = &m.name {
            if !crate::capture::locate_model(rt, name, ctx).present {
                let pull = rt.pull.as_ref().map(|p| format!(": `{}`", p.replace("{model}", name))).unwrap_or_default();
                notes.push(format!("{name} isn't on disk yet{pull}"));
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// Jobs
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub name: String,
    pub description: String,
    pub builtin: bool,
    /// Whether hlock can check the result (always for built-ins; custom jobs with `verify`).
    pub verifies: bool,
}

/// The built-in jobs, then the custom ones from `harness.meta.toml`.
pub fn jobs(meta: Option<&MetaManifest>) -> Vec<JobInfo> {
    let mut out: Vec<JobInfo> = BUILTIN_JOBS
        .iter()
        .map(|(n, d)| JobInfo { name: n.to_string(), description: d.to_string(), builtin: true, verifies: true })
        .collect();
    if let Some(m) = meta {
        for (name, spec) in &m.jobs {
            if BUILTIN_JOBS.iter().any(|(b, _)| b == name) {
                continue;
            }
            out.push(JobInfo {
                name: name.clone(),
                description: spec.description.clone().unwrap_or_default(),
                builtin: false,
                verifies: spec.verify.is_some(),
            });
        }
    }
    out
}

fn job_spec<'a>(name: &str, meta: Option<&'a MetaManifest>) -> Result<Option<&'a JobSpec>> {
    let builtin = BUILTIN_JOBS.iter().any(|(b, _)| *b == name);
    let spec = meta.and_then(|m| m.jobs.get(name));
    if !builtin && spec.is_none() {
        let known: Vec<String> = jobs(meta).into_iter().map(|j| j.name).collect();
        return Err(Error::Profile(format!("unknown meta job {name:?}; known jobs: {}", known.join(", "))));
    }
    Ok(spec)
}

/// Everything a job reads from the project.
#[derive(Debug, Clone, Default)]
pub struct JobInput {
    /// The project directory (where the project's lock lives).
    pub project: PathBuf,
    /// The project's lock.
    pub lock_path: PathBuf,
    pub manifest: Option<Manifest>,
    pub manifest_path: Option<PathBuf>,
    pub meta_lock_path: PathBuf,
    /// `bootstrap-evals`: the pack directory (default: the manifest's first eval pack, else `./evals`).
    pub out: Option<PathBuf>,
    /// `bootstrap-evals`: how many tasks to aim for.
    pub count: usize,
    /// `bootstrap-evals`: the Agent Skill that says how to fill in the pack's template, if one was written.
    pub guide: Option<PathBuf>,
    /// The user's extra instructions.
    pub instructions: String,
}

impl JobInput {
    /// A path for prompts and messages: relative to the project when it's inside it.
    fn show(&self, p: &Path) -> String {
        p.strip_prefix(&self.project).map(|r| r.to_string_lossy().into_owned()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| p.to_string_lossy().into_owned())
    }

    /// The `bootstrap-evals` directory.
    pub fn pack_dir(&self) -> PathBuf {
        if let Some(o) = &self.out {
            return if o.is_absolute() { o.clone() } else { self.project.join(o) };
        }
        if let Some(m) = &self.manifest {
            if let Some(first) = m.eval_packs().first() {
                let p = PathBuf::from(first);
                return if p.is_absolute() { p } else { m.root().join(p) };
            }
        }
        self.project.join("evals")
    }

    fn load_lock(&self) -> Result<Lockfile> {
        if !self.lock_path.exists() {
            return Err(Error::Lockfile(format!(
                "cannot read {}: no such file; write one with `hlock pin` or `hlock resolve`",
                self.show(&self.lock_path)
            )));
        }
        Lockfile::load(&self.lock_path)
    }
}

// --------------------------------------------------------------------------- //
// Prompts
// --------------------------------------------------------------------------- //

/// The job's prompt. `env_note` describes the meta harness's isolation (empty when it
/// runs in place).
pub fn render(job: &str, input: &JobInput, meta: Option<&MetaManifest>, reg: &Registry, ctx: &Ctx, env_note: &str) -> Result<String> {
    let spec = job_spec(job, meta)?;
    let (goal, check) = match job {
        "install" => install_goal(input, reg, ctx)?,
        "bootstrap-evals" => pack_goal(input),
        "update" => update_goal(input, reg, ctx)?,
        _ => {
            let spec = spec.expect("custom jobs have a spec");
            let body = substitute(spec.prompt.as_deref().unwrap_or_default(), input);
            let check = spec.verify.is_some().then(|| format!("hlock meta verify {job}"));
            (body.trim_end().to_string(), check)
        }
    };
    let mut s = String::from(
        "You are hlock's meta harness: the agent hlock (harness-lock) calls for work that needs judgment. \
         When you finish, hlock checks the result itself, so do the job rather than describe it.\n\n",
    );
    s.push_str(&format!("Project: {}\n\n## Job: {job}\n\n{goal}\n\n", input.project.display()));
    s.push_str("## Rules\n\n");
    s.push_str("- Stay inside the job. Don't change files or settings it doesn't need.\n");
    s.push_str("- Ask the user before installing system software or downloading large files, and say how big they are.\n");
    s.push_str("- Never print, log, or commit secret values. Ask the user to set secrets in their shell; don't set them yourself.\n");
    if !env_note.is_empty() {
        s.push_str(&format!("- {env_note}\n"));
    }
    if let Some(c) = check {
        s.push_str(&format!("\n## Check your work\n\nRun `{c}` and fix what it reports before you finish.\n"));
    }
    let extra: Vec<&str> = [spec.and_then(|j| j.instructions.as_deref()), Some(input.instructions.as_str())]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if !extra.is_empty() {
        s.push_str(&format!("\n## Instructions from the user\n\n{}\n", extra.join("\n\n")));
    }
    Ok(s)
}

fn substitute(template: &str, input: &JobInput) -> String {
    let manifest = input.manifest_path.as_deref().map(|p| input.show(p)).unwrap_or_else(|| crate::MANIFEST_PATH.into());
    template
        .replace("{lock}", &input.show(&input.lock_path))
        .replace("{meta_lock}", &input.show(&input.meta_lock_path))
        .replace("{manifest}", &manifest)
        .replace("{project}", &input.project.to_string_lossy())
}

fn install_goal(input: &JobInput, reg: &Registry, ctx: &Ctx) -> Result<(String, Option<String>)> {
    let lock = input.load_lock()?;
    let plan = sync::plan(&lock, &input.lock_path, reg, ctx, &SyncOptions::default())?;
    let lock_name = input.show(&input.lock_path);
    let mut s = format!(
        "Make this machine match `{lock_name}` for {} {}: install what's missing, so `hlock sync` has nothing left that needs a person.\n\n",
        plan.harness_name,
        plan.version.as_deref().unwrap_or("")
    );
    let pending: Vec<&sync::Step> = plan.steps.iter().filter(|st| st.status != "satisfied" && !st.optional).collect();
    let (secrets, steps): (Vec<&sync::Step>, Vec<&sync::Step>) = pending.into_iter().partition(|st| st.kind == "secret");
    if steps.is_empty() {
        s.push_str("Nothing is missing that you can install. ");
    } else {
        s.push_str("Steps still missing, in order:\n\n");
        for (i, st) in steps.iter().enumerate() {
            s.push_str(&format!("{}. **{}**", i + 1, st.title));
            if st.status == "auto" {
                s.push_str(" (`hlock sync --yes` does this)");
            }
            s.push('\n');
            if !st.reason.is_empty() {
                s.push_str(&format!("   - Why: {}\n", st.reason));
            }
            if let Some(c) = &st.command {
                s.push_str(&format!("   - Suggested: `{c}`\n"));
            }
            if let Some(v) = &st.verify {
                s.push_str(&format!("   - Check: `{v}`\n"));
            }
        }
        s.push_str("\nThe suggested commands are a starting point: use what works on this machine, and pin the exact versions listed.\n\n");
    }
    if !secrets.is_empty() {
        let names: Vec<String> = secrets.iter().map(|st| st.id.trim_start_matches("secret:").to_string()).collect();
        s.push_str(&format!(
            "These secrets are the user's to set; tell them which ones are missing at the end, and don't set them: {}.\n\n",
            names.join(", ")
        ));
    }
    s.push_str("Finish by running `hlock sync --yes`, which performs the remaining automatic steps.");
    Ok((s, Some("hlock meta verify install".into())))
}

fn pack_goal(input: &JobInput) -> (String, Option<String>) {
    let dir = input.pack_dir();
    let shown = input.show(&dir);
    let check = Some(format!("hlock meta verify bootstrap-evals --out {shown}"));
    let name = input.manifest.as_ref().map(Manifest::name).unwrap_or_else(|| {
        input.project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".into())
    });
    let text = std::fs::read_to_string(dir.join("pack.toml")).unwrap_or_default();
    if text.is_empty() {
        return (
            format!("There's no eval pack in `{shown}` yet. Write one with `hlock evals init {shown} --template <template>` (`hlock evals templates` lists them), then fill it in with real tasks from this project ({name})."),
            check,
        );
    }
    let ids: Vec<String> = toml::from_str::<PackFile>(&text).map(|p| p.task.into_iter().map(|t| t.id).collect()).unwrap_or_default();
    let open_slots = text.contains(PLACEHOLDER)
        || EvalPack::load(&dir.to_string_lossy(), None).is_ok_and(|p| p.tasks.iter().any(|t| t.prompt_text.contains(PLACEHOLDER)));
    let mut s = if open_slots {
        format!(
            "Fill in the eval pack template in `{shown}`: replace every `{PLACEHOLDER}` slot with a real task from this project ({name}), \
             so the tasks look like the work its users actually hand to an agent. Aim for at least {} tasks.\n\n",
            input.count
        )
    } else {
        format!(
            "Add tasks to the eval pack in `{shown}` (it has {}), drawn from real work in this project ({name}), until it has at least {}. \
             Don't remove or rewrite existing tasks, and bump the pack's `version`.\n\n",
            if ids.is_empty() { "no tasks".to_string() } else { ids.join(", ") },
            input.count
        )
    };
    match &input.guide {
        Some(g) => s.push_str(&format!("Follow the guide in `{}`: where to look for tasks, the task kinds, and the rules for writing them.", input.show(g))),
        None => s.push_str("Each task needs a prompt file, the fixtures it needs in `workspace/`, and an expected artifact a program can check. Never copy secrets or personal data into a task."),
    }
    if let Some(t) = input.manifest.as_ref().and_then(Manifest::target_harness) {
        s.push_str(&format!("\n\nThe project's agent runs in {t}."));
    }
    (s, check)
}

fn update_goal(input: &JobInput, reg: &Registry, ctx: &Ctx) -> Result<(String, Option<String>)> {
    let lock = input.load_lock()?;
    let lock_name = input.show(&input.lock_path);
    if !lock.is_pin() {
        return Err(Error::Lockfile(format!(
            "{lock_name} is a resolved lock; its components come from harness.toml, so update them there (or plan it with `hlock upgrade`). The update job works on pinned locks."
        )));
    }
    let profile = reg.require_harness(&lock.harness.name)?;
    let mut s = format!(
        "Update the skills, MCP servers and plugins pinned in `{lock_name}` for {} to their newest upstream releases. \
         Apply each update through {}'s own mechanism (its plugin commands, the package version in an MCP server's arguments, a skill's git source), \
         skip anything that looks like a breaking change unless the user says otherwise, and finish with a short summary of what you changed and what you skipped.\n\n",
        profile.name, profile.name
    );
    s.push_str(&format!("{}'s config root on this machine: `{}`\n\n", profile.name, profile.config_root(ctx).display()));
    let mut any = false;
    if !lock.skill.is_empty() {
        any = true;
        s.push_str("Skills:\n");
        for sk in &lock.skill {
            s.push_str(&format!("- {} ({}{})\n", sk.name, sk.origin, sk.source.as_deref().map(|x| format!(", {x}")).unwrap_or_default()));
        }
        s.push('\n');
    }
    if !lock.tool.is_empty() {
        any = true;
        s.push_str("MCP servers:\n");
        for t in &lock.tool {
            let how = match (&t.url, &t.command) {
                (Some(u), _) => u.clone(),
                (None, Some(c)) => std::iter::once(c.clone()).chain(t.args.iter().cloned()).collect::<Vec<_>>().join(" "),
                _ => String::new(),
            };
            s.push_str(&format!("- {} ({}) {how}\n", t.name, t.transport.as_deref().unwrap_or("?")));
        }
        s.push('\n');
    }
    if !lock.native.is_empty() {
        any = true;
        s.push_str("Plugins:\n");
        for n in &lock.native {
            s.push_str(&format!("- {} {} ({}{})\n", n.name, n.version.as_deref().unwrap_or(""), n.kind, n.source.as_deref().map(|x| format!(", {x}")).unwrap_or_default()));
        }
        s.push('\n');
    }
    if !any {
        s.push_str("The lock pins no skills, MCP servers or plugins, so there may be nothing to update.\n\n");
    }
    s.push_str(&format!("Don't edit `{lock_name}`; hlock re-pins it after you finish."));
    Ok((s.trim_end().to_string(), Some("hlock meta verify update".into())))
}

// --------------------------------------------------------------------------- //
// Verification
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize, Default)]
pub struct Verdict {
    pub job: String,
    /// `verified`, `failed`, `needs-you` (only the user can finish it), or `unverified`.
    pub status: String,
    pub summary: String,
    pub details: Vec<String>,
    /// `install`: the secrets the user still has to set.
    pub secrets: Vec<String>,
    /// `update`: changes since the lock.
    pub diff: Vec<String>,
}

impl Verdict {
    fn new(job: &str, status: &str, summary: impl Into<String>) -> Verdict {
        Verdict { job: job.into(), status: status.into(), summary: summary.into(), ..Default::default() }
    }
}

/// hlock's own check of a job's result.
pub fn verify(job: &str, input: &JobInput, meta: Option<&MetaManifest>, reg: &Registry, ctx: &Ctx) -> Result<Verdict> {
    let spec = job_spec(job, meta)?;
    match job {
        "install" => {
            let lock = input.load_lock()?;
            let plan = sync::plan(&lock, &input.lock_path, reg, ctx, &SyncOptions::default())?;
            let pending: Vec<&sync::Step> = plan.steps.iter().filter(|st| st.status != "satisfied" && !st.optional).collect();
            let (secrets, steps): (Vec<&sync::Step>, Vec<&sync::Step>) = pending.into_iter().partition(|st| st.kind == "secret");
            let secret_names: Vec<String> = secrets.iter().map(|st| st.id.trim_start_matches("secret:").to_string()).collect();
            let mut v = if !steps.is_empty() {
                let mut v = Verdict::new(job, "failed", format!("{} step{} still missing", steps.len(), if steps.len() == 1 { "" } else { "s" }));
                v.details = steps.iter().map(|st| format!("{}{}", st.title, if st.reason.is_empty() { String::new() } else { format!(" ({})", st.reason) })).collect();
                v
            } else if !secrets.is_empty() {
                Verdict::new(job, "needs-you", format!("installed; {} secret{} left for you to set", secrets.len(), if secrets.len() == 1 { "" } else { "s" }))
            } else {
                Verdict::new(job, "verified", format!("this machine matches {}", input.show(&input.lock_path)))
            };
            v.secrets = secret_names;
            Ok(v)
        }
        "bootstrap-evals" => {
            let dir = input.pack_dir();
            let shown = input.show(&dir);
            let pack = match EvalPack::load(&dir.to_string_lossy(), None) {
                Ok(p) => p,
                Err(e) => {
                    let mut v = Verdict::new(job, "failed", format!("{shown} isn't an eval pack"));
                    v.details.push(e.to_string());
                    return Ok(v);
                }
            };
            let issues = pack.check();
            let line = |i: &crate::tasks::PackIssue| format!("{}{}{}", if i.level == "error" { "" } else { "warning: " }, i.task.as_deref().map(|t| format!("{t}: ")).unwrap_or_default(), i.message);
            let errors: Vec<String> = issues.iter().filter(|i| i.level == "error").map(line).collect();
            let n = pack.tasks.len();
            let mut v = if errors.is_empty() {
                Verdict::new(job, "verified", format!("{n} task{} in {shown}", if n == 1 { "" } else { "s" }))
            } else {
                let mut v = Verdict::new(job, "failed", format!("{shown} isn't ready: {} error{}", errors.len(), if errors.len() == 1 { "" } else { "s" }));
                v.details = errors;
                v
            };
            v.details.extend(issues.iter().filter(|i| i.level != "error").map(line));
            if input.count > 0 && n < input.count && v.status == "verified" {
                v.details.push(format!("asked for {}, got {n}", input.count));
            }
            Ok(v)
        }
        "update" => {
            let lock = input.load_lock()?;
            if !lock.is_pin() {
                return Err(Error::Lockfile(format!("{} is a resolved lock; the update job works on pinned locks", input.show(&input.lock_path))));
            }
            let diff = crate::capture::check_pin(&lock, reg, ctx)?;
            let mut v = if diff.is_empty() {
                Verdict::new(job, "verified", "no changes: everything matches the lock")
            } else {
                Verdict::new(job, "verified", format!("{} change{} since {}", diff.len(), if diff.len() == 1 { "" } else { "s" }, input.show(&input.lock_path)))
            };
            v.diff = diff;
            Ok(v)
        }
        _ => {
            let spec = spec.expect("custom jobs have a spec");
            let Some(cmd) = &spec.verify else {
                return Ok(Verdict::new(job, "unverified", "this job has no `verify` command"));
            };
            let cmd = substitute(cmd, input);
            let out = std::process::Command::new("sh")
                .args(["-c", &cmd])
                .current_dir(&input.project)
                .env_clear()
                .envs(&ctx.env)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| Error::Profile(format!("cannot run `{cmd}`: {e}")))?;
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let tail: Vec<String> = text.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().map(str::to_string).collect();
            let mut v = if out.status.success() {
                Verdict::new(job, "verified", format!("`{cmd}` passed"))
            } else {
                Verdict::new(job, "failed", format!("`{cmd}` exited with {}", out.status.code().unwrap_or(-1)))
            };
            v.details = tail;
            Ok(v)
        }
    }
}

// --------------------------------------------------------------------------- //
// Running a job
// --------------------------------------------------------------------------- //

/// Shell prefix that undoes isolation for one command: `env -u VAR` or `env VAR=value`.
pub fn restore_prefix(changed: &BTreeMap<String, Option<String>>) -> String {
    let mut parts = vec!["env".to_string()];
    for (k, v) in changed {
        match v {
            Some(v) => parts.push(sync::shell_join(&[format!("{k}={v}")])),
            None => parts.push(format!("-u {k}")),
        }
    }
    parts.join(" ")
}

/// Put back the variables a meta session's isolation changed (see [`RESTORE_ENV`]),
/// so an `hlock` command run by the meta harness acts on the user's real setup.
pub fn restore_env(env: &mut BTreeMap<String, String>) {
    let Some(marker) = env.remove(RESTORE_ENV) else { return };
    let Ok(Value::Object(changed)) = serde_json::from_str::<Value>(&marker) else { return };
    for (k, v) in changed {
        match v.as_str() {
            Some(orig) => {
                env.insert(k, orig.to_string());
            }
            None => {
                env.remove(&k);
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Run non-interactively (default: `harness.meta.toml`'s `[run] mode`, else interactive).
    pub headless: Option<bool>,
    /// Only build the command: what would stop the run becomes a note.
    pub dry_run: bool,
    pub allow_version_mismatch: bool,
}

/// A job ready to run: the command, its environment, and the rendered prompt.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Prepared {
    pub job: String,
    pub harness: String,
    pub harness_name: String,
    pub harness_version: Option<String>,
    /// `interactive` or `headless`.
    pub mode: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub root: Option<PathBuf>,
    pub prompt: String,
    /// True when the harness can't take a first message: the user points it at the prompt file.
    pub paste: bool,
    pub done: Vec<String>,
    pub notes: Vec<String>,
    pub missing_secrets: Vec<String>,
}

/// Build the command that runs `job` in the meta harness pinned in `meta_lock`.
pub fn prepare(job: &str, input: &JobInput, meta: Option<&MetaManifest>, meta_lock: &Lockfile, reg: &Registry, ctx: &Ctx, opts: &RunOptions) -> Result<Prepared> {
    job_spec(job, meta)?;
    let builtin = builtin_profile();
    let profile = meta_profile(reg, &meta_lock.harness.name, &builtin)?;
    if profile.kind == "builtin" {
        return prepare_builtin(job, input, meta, meta_lock, reg, ctx, opts);
    }
    let code = profile.kind != "cli";
    let mut headless = opts.headless.unwrap_or_else(|| meta.is_some_and(MetaManifest::headless));
    let mut notes = Vec::new();
    if let Err(e) = preflight(reg, ctx, meta, profile, &mut notes) {
        if !opts.dry_run {
            return Err(e);
        }
        notes.push(e.to_string().trim_start_matches("profile error: ").to_string());
    }
    if code && !headless {
        notes.push(format!("{} is a code harness, so the job runs headless", profile.name));
        headless = true;
    }
    // headless runs use the profile's `[launch] prompt`; interactive ones its `interactive` arguments
    let (args, prompt, paste) = if code {
        (vec![PROMPT_TOKEN.to_string()], None, false)
    } else if headless {
        if profile.launch.prompt.is_empty() {
            return Err(Error::Profile(format!(
                "{} has no non-interactive mode in its profile ([launch] prompt); run the job interactively",
                profile.name
            )));
        }
        (Vec::new(), Some(PROMPT_TOKEN.to_string()), false)
    } else if profile.launch.interactive.is_empty() {
        (Vec::new(), None, true)
    } else {
        (profile.launch.interactive.iter().map(|a| a.replace("{prompt}", PROMPT_TOKEN)).collect(), None, false)
    };
    let lopts = LaunchOptions {
        allow_version_mismatch: opts.allow_version_mismatch,
        workspace: Some(input.project.clone()),
        prompt,
        ..Default::default()
    };
    let prep = launch::prepare(meta_lock, &input.meta_lock_path, reg, ctx, &lopts, &args)?;
    notes.extend(prep.notes.iter().cloned());

    // what isolation changed, so hlock (and the agent) can undo it for the real setup
    let changed: BTreeMap<String, Option<String>> = if code {
        BTreeMap::new()
    } else {
        prep.env.keys().map(|k| (k.clone(), ctx.env.get(k).cloned())).collect()
    };
    let env_note = if changed.is_empty() {
        String::new()
    } else {
        let vars: Vec<String> = prep.env.iter().map(|(k, v)| format!("{k}={}", input.show(Path::new(v)))).collect();
        format!(
            "Your own configuration is isolated: this session runs with {}, and commands you run inherit it. \
             `hlock` commands put the user's real values back on their own. For any other command that must act on the user's real setup \
             (a harness's own CLI, installing plugins or skills), prefix it with `{}`.",
            vars.join(" "),
            restore_prefix(&changed)
        )
    };
    let prompt = render(job, input, meta, reg, ctx, &env_note)?;
    let fill = |s: &String| s.replace(PROMPT_TOKEN, &prompt);
    let mut env: BTreeMap<String, String> = prep.env.iter().map(|(k, v)| (k.clone(), fill(v))).collect();
    if !changed.is_empty() {
        let marker: serde_json::Map<String, Value> = changed.iter().map(|(k, v)| (k.clone(), v.clone().map(Value::String).unwrap_or(Value::Null))).collect();
        env.insert(RESTORE_ENV.into(), Value::Object(marker).to_string());
    }
    Ok(Prepared {
        job: job.into(),
        harness: profile.id.clone(),
        harness_name: profile.name.clone(),
        harness_version: meta_lock.harness.version.clone(),
        mode: if headless { "headless" } else { "interactive" }.into(),
        argv: prep.argv.iter().map(fill).collect(),
        env,
        root: prep.root.clone(),
        prompt,
        paste,
        done: prep.done,
        notes,
        missing_secrets: prep.missing_secrets,
    })
}

/// The built-in agent runs as `hlock meta agent` (hlock's own interpreter), so a job runs,
/// streams and records the same way an external harness's does.
fn prepare_builtin(job: &str, input: &JobInput, meta: Option<&MetaManifest>, meta_lock: &Lockfile, reg: &Registry, ctx: &Ctx, opts: &RunOptions) -> Result<Prepared> {
    let headless = opts.headless.unwrap_or_else(|| meta.is_some_and(MetaManifest::headless));
    let mut notes = Vec::new();
    let builtin = builtin_profile();
    if let Err(e) = preflight(reg, ctx, meta, &builtin, &mut notes) {
        if !opts.dry_run {
            return Err(e);
        }
        notes.push(e.to_string());
    }
    let want = env!("CARGO_PKG_VERSION");
    if meta_lock.harness.version.as_deref() != Some(want) {
        let msg = format!("the meta lock pins hlock {}, this is hlock {want}", meta_lock.harness.version.as_deref().unwrap_or("?"));
        if !opts.allow_version_mismatch {
            return Err(Error::Profile(format!("{msg}; re-pin with `hlock meta pin`, or pass --allow-version-mismatch")));
        }
        notes.push(msg);
    }
    let prompt = render(job, input, meta, reg, ctx, "")?;
    let python = crate::profiles::python_for(None, ctx).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "python3".into());
    let argv = vec![
        python,
        "-m".into(),
        "harness_lock.cli".into(),
        "meta".into(),
        "agent".into(),
        "--meta-lock".into(),
        input.meta_lock_path.to_string_lossy().into_owned(),
        if headless { "--headless" } else { "--interactive" }.into(),
        prompt.clone(),
    ];
    Ok(Prepared {
        job: job.into(),
        harness: BUILTIN.into(),
        harness_name: builtin.name,
        harness_version: meta_lock.harness.version.clone(),
        mode: if headless { "headless" } else { "interactive" }.into(),
        argv,
        env: BTreeMap::new(),
        root: None,
        prompt,
        paste: false,
        done: Vec::new(),
        notes,
        missing_secrets: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_env_round_trip() {
        let mut env: BTreeMap<String, String> = [
            ("CLAUDE_CONFIG_DIR".to_string(), "/p/.hlock/env/x".to_string()),
            ("XDG_CONFIG_HOME".to_string(), "/p/.hlock/env/y".to_string()),
            (RESTORE_ENV.to_string(), r#"{"CLAUDE_CONFIG_DIR": null, "XDG_CONFIG_HOME": "/home/u/.config"}"#.to_string()),
        ]
        .into();
        restore_env(&mut env);
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR") && !env.contains_key(RESTORE_ENV));
        assert_eq!(env["XDG_CONFIG_HOME"], "/home/u/.config");
        let changed: BTreeMap<String, Option<String>> = [("A".to_string(), None), ("B".to_string(), Some("/x y".to_string()))].into();
        assert_eq!(restore_prefix(&changed), "env -u A 'B=/x y'");
    }

    #[test]
    fn meta_manifest_rules() {
        let m = MetaManifest::from_str("[target]\nharness = \"pi\"\n[run]\nmode = \"headless\"\n[jobs.install]\ninstructions = \"use brew\"\n[jobs.notes]\nprompt = \"write notes\"\nverify = \"true\"\n").unwrap();
        assert_eq!(m.target_harness().as_deref(), Some("pi"));
        assert!(m.headless());
        let names: Vec<String> = jobs(Some(&m)).into_iter().map(|j| j.name).collect();
        assert_eq!(names, ["install", "bootstrap-evals", "update", "notes"]);
        assert!(MetaManifest::from_str("[jobs.install]\nprompt = \"x\"\n").is_err());
        assert!(MetaManifest::from_str("[jobs.notes]\nverify = \"true\"\n").is_err());
        assert!(MetaManifest::from_str("[run]\nmode = \"yolo\"\n").is_err());
        assert!(job_spec("nope", Some(&m)).is_err());
    }
}
