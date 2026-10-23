//! `hlock launch`: start a harness session that matches a lock (SPEC §7.8.6).
//!
//! [`prepare`] verifies the installed harness, materializes the locked setup
//! into an isolated config root (the same path as `hlock sync --prefix`), and
//! returns the command and environment to `exec`. Launching a different harness
//! than the lock's translates the portable components (MCP servers, skills,
//! instruction files, and the model where the target can reach it) into the
//! target's formats, and reports what doesn't carry over.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::capture::{live_mcp_servers, read_config, render_config, tree_digest};
use crate::error::{Error, Result};
use crate::fsutil;
use crate::lockfile::{Lockfile, Tool};
use crate::profiles::{probe_harness, Ctx, HarnessProfile, Registry};
use crate::secrets;
use crate::sync::{self, lock_dir_of, placeholders, pretty, set_pointer, SyncOptions};

/// Default directory for isolated launch environments.
pub const ENV_DIR: &str = ".hlock/env";

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// Harness to launch (default: the lock's).
    pub target: Option<String>,
    /// Where isolated config roots live (default [`ENV_DIR`]).
    pub prefix: Option<PathBuf>,
    /// Launch even if the installed version differs from the lock.
    pub allow_version_mismatch: bool,
    /// Use the user's own config instead of an isolated root (same harness only).
    pub in_place: bool,
    /// Where a code harness works (default: the project directory).
    pub workspace: Option<PathBuf>,
    /// Run this prompt non-interactively with the profile's `[launch] prompt` arguments.
    pub prompt: Option<String>,
    /// Treat the lock's executables as approved (`--trust`).
    pub trust: bool,
    /// Files whose hidden characters are let through (`--allow-hidden FILE`).
    pub allow_hidden: Vec<String>,
    /// For an agent CLI: the directory that stands in for the project, where the lock's
    /// project-scoped entries are set up and the agent runs (an `hlock evals run`
    /// workspace). Default: the project. The isolated config root stays the project's.
    pub project: Option<PathBuf>,
    /// Run without the lock's sandbox (`--no-sandbox`).
    pub no_sandbox: bool,
    /// Variables the sandboxed agent gets from hlock (the proxy's base URLs and token).
    /// With them, provider credentials stay out of the sandbox.
    pub proxy_env: BTreeMap<String, String>,
    /// Give a sandboxed agent a terminal (`docker run -t`).
    pub tty: bool,
}

/// How a sandboxed launch runs: the runner's argv and environment, in place of `argv`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SandboxPrep {
    pub spec: crate::sandbox::Spec,
    /// `docker (network open, secrets env)`.
    pub summary: String,
    pub argv: Vec<String>,
    /// The runner process's environment; it holds secret values, so it's never printed.
    pub env: BTreeMap<String, String>,
    pub env_names: Vec<String>,
    pub kill: Vec<String>,
    /// The project whose control files are checked after the run.
    pub project: PathBuf,
    /// For docker and openshell: the image, and the harness and version the lock pins, to
    /// compare with the image's labels.
    pub image: Option<String>,
    pub harness_version: Option<String>,
    /// For openshell: the session `hlock.sandbox.run` begins and ends around `argv`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::openshell::Session>,
    /// For openshell: the command's variables, written into the sandbox by `begin`. Like
    /// `env`, it holds secret values and is never printed.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub session_env: BTreeMap<String, String>,
}

/// What to `exec`, and what happened while preparing it.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LaunchPrep {
    pub harness: String,
    pub harness_name: String,
    /// `cli` or `custom` (a code harness).
    pub kind: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub root: Option<PathBuf>,
    /// True when the lock is for another harness and its portable parts were translated.
    pub translated: bool,
    /// Things done while preparing (files written, plugins installed).
    pub done: Vec<String>,
    /// Things to know: what didn't carry over, version notes.
    pub notes: Vec<String>,
    pub missing_secrets: Vec<String>,
    /// Secret values the harness reads from its environment through the references
    /// written in its config. Added to the environment when it starts; never printed.
    pub secret_env: BTreeMap<String, String>,
    /// Set when the run is sandboxed: start this instead of `argv`.
    pub sandbox: Option<SandboxPrep>,
}

fn launch_sandbox(lock: &Lockfile, lock_path: &Path, project: &Path, ctx: &Ctx, opts: &LaunchOptions, prep: &mut LaunchPrep) -> Result<Option<crate::sandbox::Sandbox>> {
    crate::sandbox::for_lock(lock, lock_path, project, ctx, opts.no_sandbox, opts.trust, &mut prep.notes)
}

/// The locked model to pass on the command line, if the target can use it.
fn model_arg(lock: &Lockfile, target: &HarnessProfile, notes: &mut Vec<String>) -> Option<String> {
    let llm = lock.llm.iter().find(|l| l.role == "default").or_else(|| lock.llm.first())?;
    if matches!(llm.provider.as_str(), "reference" | "unknown") || llm.model == "harness-default" {
        return None;
    }
    if target.model.format.as_deref() == Some("provider/model") || target.providers.is_some() {
        if target.providers.is_some() && !matches!(llm.provider.as_str(), "anthropic" | "openai" | "google") {
            notes.push(format!("{} must have a provider named {:?} configured for {}", target.name, llm.provider, llm.model));
        }
        return Some(format!("{}/{}", llm.provider, llm.model));
    }
    match target.model.default_provider.as_deref() {
        Some(p) if p == llm.provider => Some(llm.model.clone()),
        Some(p) => {
            notes.push(format!("{} talks to {p}, so the locked model {}:{} isn't passed; it uses its own default", target.name, llm.provider, llm.model));
            None
        }
        None => Some(llm.model.clone()),
    }
}

/// One MCP server in a target harness's config shape.
pub fn mcp_entry(t: &Tool, shape: &str) -> Value {
    let remote = crate::trust::is_remote(t);
    let env = json!(t.env);
    let headers = json!(t.headers);
    let enabled = t.enabled != Some(false);
    match (shape, remote) {
        ("opencode", false) => {
            let mut cmd = vec![t.command.clone().unwrap_or_default()];
            cmd.extend(t.args.iter().cloned());
            let mut e = json!({"type": "local", "command": cmd, "enabled": enabled});
            if !t.env.is_empty() {
                e["environment"] = env;
            }
            e
        }
        ("opencode", true) => {
            let mut e = json!({"type": "remote", "url": t.url, "enabled": enabled});
            if !t.headers.is_empty() {
                e["headers"] = headers;
            }
            e
        }
        ("goose", false) => json!({"type": "stdio", "name": t.name, "cmd": t.command, "args": t.args, "envs": env, "enabled": enabled, "timeout": 300}),
        ("goose", true) => json!({"type": "streamable_http", "name": t.name, "uri": t.url, "headers": headers, "enabled": enabled, "timeout": 300}),
        (s @ ("openclaw" | "nanobot"), true) => {
            // the key and the name each uses for streamable HTTP
            let (key, http) = if s == "openclaw" { ("transport", "streamable-http") } else { ("type", "streamableHttp") };
            let mut e = json!({key: if t.transport.as_deref() == Some("sse") { "sse" } else { http }, "url": t.url});
            if !t.headers.is_empty() {
                e["headers"] = headers;
            }
            e
        }
        ("codex", true) => {
            let mut e = json!({"url": t.url});
            if !t.headers.is_empty() {
                e["http_headers"] = headers;
            }
            e
        }
        (_, true) => {
            let mut e = json!({"type": t.transport, "url": t.url});
            if !t.headers.is_empty() {
                e["headers"] = headers;
            }
            e
        }
        (_, false) => {
            let mut e = json!({"command": t.command, "args": t.args});
            if !t.env.is_empty() {
                e["env"] = env;
            }
            e
        }
    }
}

/// Fill `${HLOCK_SECRET_*}` in a tool from [`secrets::lookup`], else from the same
/// server's entry in the source harness's live config. With `env_ref`, write the
/// target harness's environment reference instead, and collect the value in `values`.
fn fill_tool(t: &Tool, live: Option<&Value>, ctx: &Ctx, env_ref: Option<&str>, values: &mut BTreeMap<String, String>, missing: &mut BTreeSet<String>) -> Tool {
    let mut t = t.clone();
    let lookup = |section: &[&str], key: &str| -> Option<String> {
        let entry = live?;
        section.iter().find_map(|s| entry.get(*s).and_then(|m| m.get(key)).and_then(Value::as_str).map(str::to_string))
    };
    let mut fill = |v: &str, live_value: Option<String>| -> String {
        let names = placeholders(v);
        if names.is_empty() {
            return v.to_string();
        }
        let found: BTreeMap<String, String> = names.iter().filter_map(|n| secrets::lookup(n, ctx).map(|f| (n.clone(), f.value))).collect();
        if let Some(tpl) = env_ref {
            // the live config's value for a lone placeholder (`Bearer ${X}` → `Bearer abc`)
            let from_live = |n: &str| -> Option<String> {
                let lv = live_value.as_deref().filter(|x| placeholders(x).is_empty())?;
                let (pre, post) = v.split_once(&format!("${{{n}}}"))?;
                (names.len() == 1 && lv.len() > pre.len() + post.len() && lv.starts_with(pre) && lv.ends_with(post)).then(|| lv[pre.len()..lv.len() - post.len()].to_string())
            };
            let mut out = v.to_string();
            for n in &names {
                match found.get(n).cloned().or_else(|| from_live(n)) {
                    Some(val) => {
                        values.insert(n.clone(), val);
                    }
                    None => {
                        missing.insert(n.clone());
                    }
                }
                out = out.replace(&format!("${{{n}}}"), &secrets::env_ref(tpl, n));
            }
            return out;
        }
        let mut out = v.to_string();
        let mut unfilled = false;
        for n in &names {
            match found.get(n) {
                Some(val) => out = out.replace(&format!("${{{n}}}"), val),
                None => unfilled = true,
            }
        }
        if unfilled {
            if let Some(lv) = live_value.filter(|x| placeholders(x).is_empty()) {
                return lv;
            }
            missing.extend(names.into_iter().filter(|n| !found.contains_key(n)));
        }
        out
    };
    t.env = t.env.iter().map(|(k, v)| (k.clone(), fill(v, lookup(&["env", "environment", "envs"], k)))).collect();
    t.headers = t.headers.iter().map(|(k, v)| (k.clone(), fill(v, lookup(&["headers", "http_headers"], k)))).collect();
    if let Some(u) = &t.url {
        let live_url = live.and_then(|e| ["url", "serverUrl", "httpUrl", "uri"].iter().find_map(|k| e.get(*k).and_then(Value::as_str)).map(str::to_string));
        t.url = Some(fill(u, live_url));
    }
    t
}

/// Set `ptr` in a config file. `private` (it will hold secret values) writes it 0600
/// from the start, so the secrets are never readable by others, even briefly.
fn write_pointer(path: &Path, format: &str, ptr: &str, value: Value, private: bool) -> Result<()> {
    let mut doc = read_config(path, format)?.filter(Value::is_object).unwrap_or_else(|| json!({}));
    set_pointer(&mut doc, ptr, value);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fmt = if format == "jsonc" { "json" } else { format };
    let text = render_config(&doc, fmt).map_err(Error::Profile)?;
    if private {
        fsutil::write_private(path, text)?;
    } else {
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// Translate a lock's portable components into `target`'s isolated root.
#[allow(clippy::too_many_arguments)]
fn translate(lock: &Lockfile, lock_path: &Path, source: Option<&HarnessProfile>, target: &HarnessProfile, t: &Ctx, root: &Path, ctx: &Ctx, prep: &mut LaunchPrep, gate: &Gate) -> Result<()> {
    let lock_dir = lock_dir_of(lock_path, ctx);
    let source_root = source.map(|s| s.config_root(ctx));

    // MCP servers (what isn't trusted here is left out)
    let tools: Vec<&Tool> = lock.tool.iter().filter(|x| x.kind == "mcp").filter(|x| gate.lets("mcp", &x.name, prep)).collect();
    if !tools.is_empty() {
        match target.mcp.write.as_deref().and_then(|w| w.split_once('#')) {
            Some((file, ptr)) => {
                let spec = target.files.get(file).ok_or_else(|| Error::Profile(format!("{} profile: mcp.write names unknown file {file:?}", target.id)))?;
                let path = target.file_path(file, t).unwrap_or_default();
                let live = source.map(|s| live_mcp_servers(s, ctx)).unwrap_or_default();
                let shape = target.mcp.shape.as_deref().unwrap_or("standard");
                let mut missing = BTreeSet::new();
                let mut values = BTreeMap::new();
                let env_ref = spec.env_ref.as_deref();
                let servers: Map<String, Value> = tools
                    .iter()
                    .filter(|x| x.enabled != Some(false) || matches!(shape, "opencode" | "goose"))
                    .map(|x| (x.name.clone(), mcp_entry(&fill_tool(x, live.get(&x.name), ctx, env_ref, &mut values, &mut missing), shape)))
                    .collect();
                let n = servers.len();
                // secret values are written into it, unless the target reads them from its environment
                let private = env_ref.is_none() && tools.iter().any(|x| x.env.values().chain(x.headers.values()).chain(x.url.iter()).any(|v| !placeholders(v).is_empty()));
                write_pointer(&path, &spec.format, ptr, Value::Object(servers), private)?;
                prep.secret_env.extend(values);
                prep.done.push(format!("wrote {n} MCP server{} to {}", if n == 1 { "" } else { "s" }, pretty(&path, ctx)));
                prep.missing_secrets.extend(missing);
            }
            None => prep.notes.push(format!("{} has no MCP config hlock can write; MCP servers weren't carried over", target.name)),
        }
    }

    // skills
    if let Some(write) = &target.skills.write {
        let dir = t.expand(write, Some(root));
        let mut copied = 0;
        for s in lock.skill.iter().filter(|s| !matches!(s.origin.as_str(), "harness-builtin" | "bundled")) {
            // the name becomes a directory that's replaced: never `..` or a path
            if !fsutil::plain_name(&s.name) {
                prep.notes.push(format!("skill {:?} wasn't copied: its name must be a plain name (no '/', '\\', or '..')", s.name));
                continue;
            }
            let src = s
                .vendored
                .as_ref()
                .map(|v| lock_dir.join(v))
                .filter(|p| p.join("SKILL.md").is_file())
                .or_else(|| s.path.as_ref().map(|p| ctx.expand(p, source_root.as_deref())).filter(|p| p.join("SKILL.md").is_file()));
            let Some(src) = src else {
                prep.notes.push(format!("skill {} has no local copy; run hlock sync first", s.name));
                continue;
            };
            if !gate.lets("script", &s.name, prep) || !gate.lets("hidden", &format!("skill {}", s.name), prep) || !gate.clean(&src, &format!("skill {}", s.name), s.scan.as_deref(), prep) {
                continue;
            }
            if let Some(why) = sync::tampered(&src, s.digest.as_ref()).filter(|_| s.vendored.as_ref().is_some_and(|v| src == lock_dir.join(v))) {
                prep.notes.push(format!("left out skill {}: {why}", s.name));
                continue;
            }
            let dest = dir.join(&s.name);
            if dest.join("SKILL.md").is_file() && Some(tree_digest(&dest)) == s.digest {
                continue;
            }
            replace_dir(&src, &dest)?;
            copied += 1;
        }
        if copied > 0 {
            prep.done.push(format!("copied {copied} skill{} to {}", if copied == 1 { "" } else { "s" }, pretty(&dir, ctx)));
        }
        let skipped = lock.skill.iter().filter(|s| matches!(s.origin.as_str(), "harness-builtin" | "bundled")).count();
        if skipped > 0 {
            prep.notes.push(format!("{skipped} skills that ship with the source harness or its plugins don't carry over"));
        }
    } else if !lock.skill.is_empty() {
        prep.notes.push(format!("{} has no skills directory; skills weren't carried over", target.name));
    }

    // user-level instruction files → the target's user-level instruction files
    let user_ins: Vec<_> = lock.instructions.iter().filter(|i| i.scope == "user").collect();
    let names: Vec<String> = user_ins.iter().map(|i| i.name.clone()).collect();
    for (ins, dest_tpl) in user_ins.iter().zip(user_instruction_targets(target, &names)) {
        let src = ins.vendored.as_ref().map(|v| lock_dir.join(v)).or_else(|| Some(ctx.expand(&ins.name, source_root.as_deref()))).filter(|p| p.is_file());
        let what = format!("instructions {}", ins.name);
        if src.as_ref().is_some_and(|p| !gate.lets("hidden", &what, prep) || !gate.clean(p, &what, ins.scan.as_deref(), prep)) {
            continue;
        }
        if let Some(why) = src.as_ref().filter(|p| ins.vendored.as_ref().is_some_and(|v| **p == lock_dir.join(v))).and_then(|p| sync::tampered(p, ins.digest.as_ref())) {
            prep.notes.push(format!("left out {what}: {why}"));
            continue;
        }
        match (dest_tpl, src) {
            (Some(tpl), Some(src)) => {
                let dest = t.expand(tpl, Some(root));
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src, &dest)?;
                prep.done.push(format!("wrote {} from {}", pretty(&dest, ctx), ins.name));
            }
            _ => prep.notes.push(format!("user instructions {} weren't carried over", ins.name)),
        }
    }

    // what doesn't carry
    if !lock.native.is_empty() {
        let names: Vec<&str> = lock.native.iter().filter(|n| n.kind != "package").map(|n| n.name.as_str()).collect();
        if !names.is_empty() {
            prep.notes.push(format!("plugins don't carry across harnesses: {}", names.join(", ")));
        }
    }
    if lock.config.iter().any(|c| c.restore != "record") {
        prep.notes.push(format!("{}-specific settings (permissions, hooks, theme, providers) don't carry over", source.map(|s| s.name.as_str()).unwrap_or("source")));
    }
    Ok(())
}

/// Replace the directory `dest` with a copy of `src`: copy into a temporary sibling
/// first, so a failed copy leaves the old one in place.
fn replace_dir(src: &Path, dest: &Path) -> Result<()> {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dest.with_file_name(format!(".{name}.hlock-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = fsutil::copy_tree(src, &tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    // a symlink in the way is removed, not followed
    if std::fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir()) {
        std::fs::remove_file(dest)?;
    } else if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Where each of a lock's user-level instruction files (`names`) goes in `target`:
/// the target's file of the same name (an autonomous agent's `SOUL.md`), else the
/// target's first user-level file, for the first file nothing else took. `None`: it
/// doesn't carry over, so two files never land on one path.
pub(crate) fn user_instruction_targets<'a>(target: &'a HarnessProfile, names: &[String]) -> Vec<Option<&'a str>> {
    let file = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let tpls: Vec<&str> = target.instructions.iter().map(String::as_str).filter(|i| i.starts_with("{root}/")).collect();
    let mut used = BTreeSet::new();
    let mut out: Vec<Option<&str>> = names
        .iter()
        .map(|n| tpls.iter().copied().find(|t| file(t) == file(n)).filter(|t| used.insert(*t)))
        .collect();
    if let Some(first) = tpls.first().filter(|t| !used.contains(*t)) {
        if let Some(slot) = out.iter_mut().find(|o| o.is_none()) {
            *slot = Some(first);
        }
    }
    out
}

/// Where a lock's isolated environments live: [`ENV_DIR`], or `ENV_DIR/<stem>` for a
/// lock not named `harness.lock`, so two locks never share a root.
pub fn env_prefix(lock_path: &Path, ctx: &Ctx) -> PathBuf {
    let dir = match lock_path.file_stem().and_then(|s| s.to_str()) {
        Some("harness") | None => PathBuf::from(ENV_DIR),
        Some(stem) => PathBuf::from(ENV_DIR).join(stem),
    };
    ctx.cwd.join(dir)
}

/// Set up the lock's project-scoped skills and instruction files in `to`, a stand-in for
/// the project at `from` (`LaunchOptions::project`). The plan leaves them to the repo, so
/// each is copied from the project, and only while it still matches the lock: a file
/// changed since it was pinned is left out, with a note.
fn stand_in_project(lock: &Lockfile, plan: &sync::Plan, from: &Path, to: &Path, prep: &mut LaunchPrep) -> Result<()> {
    let from_repo = |id: &str| plan.steps.iter().any(|st| st.id == id && st.status == "manual" && st.reason.contains("comes from the repo"));
    for s in &lock.skill {
        let Some(path) = s.path.as_deref().filter(|p| crate::fsutil::safe_rel(p)) else { continue };
        if !from_repo(&format!("skill:{}", s.name)) {
            continue;
        }
        let src = from.join(path);
        if src.is_dir() && Some(crate::capture::tree_digest(&src)) == s.digest {
            crate::fsutil::copy_tree(&src, &to.join(path))?;
            prep.done.push(format!("Copy skill {} → {path}", s.name));
        } else {
            prep.notes.push(format!("left out skill {}: {path} changed in the project since it was locked", s.name));
        }
    }
    for i in &lock.instructions {
        if !crate::fsutil::safe_rel(&i.name) || !from_repo(&format!("instructions:{}", i.name)) {
            continue;
        }
        let src = from.join(&i.name);
        match std::fs::read(&src) {
            Ok(bytes) if Some(crate::hash::sha256_bytes(&bytes)) == i.digest => {
                let dest = to.join(&i.name);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::fsutil::clear_symlink(&dest)?;
                std::fs::write(&dest, bytes)?;
                prep.done.push(format!("Write {}", i.name));
            }
            _ => prep.notes.push(format!("left out {}: it changed in the project since it was locked", i.name)),
        }
    }
    Ok(())
}

/// Prepare a launch: verify, materialize, and build the command to `exec`.
/// `args` are passed through to the harness (or, for a code harness, joined into the prompt).
/// What a launch may place outside `sync`'s plan (translating into another harness, or a
/// code harness's MCP config and skills): the lock's executables that aren't trusted here
/// are left out, and content with hidden characters isn't copied.
struct Gate {
    blocked: Vec<crate::trust::Executable>,
    allow_hidden: Vec<String>,
}

impl Gate {
    fn new(lock: &Lockfile, lock_path: &Path, source: Option<&HarnessProfile>, ctx: &Ctx, opts: &LaunchOptions) -> Result<Gate> {
        let policy = crate::policy::Policy::for_lock(lock_path, ctx)?;
        policy.enforce(policy.check_lock(lock), &pretty(lock_path, ctx))?;
        let store = crate::trust::TrustStore::load(ctx)?;
        let es = crate::trust::assess(lock, &lock_dir_of(lock_path, ctx), source, ctx, &policy, &store);
        let blocked = if opts.trust { Vec::new() } else { es.into_iter().filter(|e| e.kind != "policy" && matches!(e.status.as_str(), "untrusted" | "denied")).collect() };
        Ok(Gate { blocked, allow_hidden: opts.allow_hidden.clone() })
    }

    /// Whether the `kind` executable `name` may be placed; a note says why not.
    fn lets(&self, kind: &str, name: &str, prep: &mut LaunchPrep) -> bool {
        // names are listed with hidden characters made visible
        let shown = crate::scan::visible(name);
        let Some(e) = self.blocked.iter().find(|e| e.kind == kind && e.name == shown) else { return true };
        let why = if e.status == "denied" { "you denied it on this machine" } else { "not yet trusted: approve with hlock trust, or launch --trust" };
        prep.notes.push(format!("left out {kind} {name}: {why}"));
        false
    }

    /// Whether the content at `path` is free of critical hidden characters (or allowed).
    fn clean(&self, path: &Path, what: &str, scan: Option<&str>, prep: &mut LaunchPrep) -> bool {
        if crate::audit::allowed(scan) {
            return true; // and approved: `lets("hidden", …)` checked it
        }
        let f = crate::scan::scan_path(path, what);
        match f.iter().find(|x| x.severity == crate::scan::Severity::Critical && !crate::audit::allows(&self.allow_hidden, x)) {
            Some(x) => {
                prep.notes.push(format!("left out {what}: it has characters that hide text from a reviewer ({})", x.label()));
                false
            }
            None => true,
        }
    }

    /// An error naming what isn't trusted, if anything.
    fn require_all(&self, lock_path: &Path, ctx: &Ctx) -> Result<()> {
        if self.blocked.is_empty() {
            return Ok(());
        }
        Err(Error::Audit(format!("{} runs {}, not yet trusted on this machine: approve with hlock trust, or pass --trust", pretty(lock_path, ctx), self.names())))
    }

    fn names(&self) -> String {
        self.blocked.iter().map(|e| format!("{} {}", e.kind, e.name)).collect::<Vec<_>>().join(", ")
    }
}

/// Fails unless everything `lock` runs is trusted here (or `trust`): for running a code
/// harness, which gets the lock's MCP servers and skills as they are.
pub fn require_trusted(lock: &Lockfile, lock_path: &Path, reg: &Registry, ctx: &Ctx, trust: bool) -> Result<()> {
    let opts = LaunchOptions { trust, ..Default::default() };
    Gate::new(lock, lock_path, reg.harness(&lock.harness.name), ctx, &opts)?.require_all(lock_path, ctx)
}

pub fn prepare(lock: &Lockfile, lock_path: &Path, reg: &Registry, ctx: &Ctx, opts: &LaunchOptions, args: &[String]) -> Result<LaunchPrep> {
    let source = reg.harness(&lock.harness.name);
    let target_name = opts.target.clone().unwrap_or_else(|| lock.harness.name.clone());
    let target = reg.require_harness(&target_name)?;
    let same = source.is_some_and(|s| s.id == target.id);
    let mut prep = LaunchPrep { harness: target.id.clone(), harness_name: target.name.clone(), kind: target.kind.clone(), translated: !same, ..Default::default() };

    if target.kind != "cli" {
        // a code harness gets the lock's MCP servers and skills as they are: all of them
        // must be trusted here
        Gate::new(lock, lock_path, source, ctx, opts)?.require_all(lock_path, ctx)?;
        let workspace = opts.workspace.clone().unwrap_or_else(|| ctx.cwd.clone());
        let prompt = args.join(" ");
        let inv = crate::contract::invocation(target, Some(lock), Some(lock_path), None, &prompt, &workspace, 0, ctx)?;
        let sandbox = launch_sandbox(lock, lock_path, &ctx.cwd, ctx, opts, &mut prep)?;
        if sandbox.as_ref().is_some_and(|s| !s.spec.applies("trials")) {
            prep.notes.push("the lock's sandbox doesn't cover trials (applies-to), so the code harness runs on this machine".into());
        }
        if let Some(sb) = sandbox.filter(|s| s.spec.applies("trials")) {
            let mut env = crate::sandbox::provider_env(ctx, &[]);
            env.extend(inv.env.clone());
            let run = crate::sandbox::Run {
                kind: "trials".into(),
                argv: inv.argv.clone(),
                cwd: workspace.clone(),
                home: ctx.cwd.join(crate::sandbox::SANDBOX_ENV_DIR).join("trials-home"),
                mounts: if std::fs::canonicalize(&workspace).ok() == std::fs::canonicalize(&ctx.cwd).ok() {
                    crate::sandbox::project_mounts(&ctx.cwd)?
                } else {
                    crate::sandbox::project_ro_mounts(&ctx.cwd)?
                },
                env,
                tty: opts.tty,
                ..Default::default()
            };
            prep.sandbox = Some(sandbox_prep(&sb, &run, ctx, &mut prep.notes, None)?);
        }
        prep.argv = inv.argv;
        prep.env = inv.env;
        return Ok(prep);
    }

    // the isolated root is the real project's; project-scoped entries go to `opts.project`
    let (project_dir, default_prefix) = (ctx.cwd.clone(), env_prefix(lock_path, ctx));
    let moved;
    let ctx = match &opts.project {
        Some(p) => {
            moved = Ctx { cwd: p.clone(), ..ctx.clone() };
            &moved
        }
        None => ctx,
    };
    let sandbox = launch_sandbox(lock, lock_path, &project_dir, ctx, opts, &mut prep)?.filter(|s| s.spec.applies("agent"));
    if sandbox.is_some() && opts.in_place {
        return Err(Error::Profile("--in-place can't be sandboxed: it's your own config; drop --in-place, or run with --no-sandbox".into()));
    }
    if sandbox.is_some() && !same {
        return Err(Error::Profile(format!("launching {} from a {} lock can't be sandboxed yet (the sandbox is for the lock's harness); run with --no-sandbox", target.name, lock.harness.name)));
    }
    let docker = sandbox.as_ref().is_some_and(|s| matches!(s.runner.name.as_str(), "docker" | "openshell"));
    let probe = probe_harness(target, ctx);
    // in a docker or openshell sandbox the image runs the harness; the host needn't have it
    let bin = match (&probe.binary, docker) {
        (Some(b), false) => b.clone(),
        (_, true) => target.binaries.first().cloned().unwrap_or_else(|| target.id.clone()),
        (None, false) => {
            let hint = if same { "run `hlock sync --install`" } else { "install it first" };
            return Err(Error::Profile(format!("{} isn't installed; {hint}", target.name)));
        }
    };
    if !same {
        prep.notes.push(format!("translating a {} lock into {}", source.map(|s| s.name.as_str()).unwrap_or(&lock.harness.name), target.name));
    } else if !docker {
        // (in an image's sandbox, the caller checks the image's labels instead)
        if let (Some(want), Some(have)) = (&lock.harness.version, &probe.version) {
            if want != have {
                if !opts.allow_version_mismatch {
                    return Err(Error::Profile(format!(
                        "{} {have} is installed but the lock pins {want}; install {want} (hlock sync --install) or pass --allow-version-mismatch",
                        target.name
                    )));
                }
                prep.notes.push(format!("running {} {have}; the lock pins {want}", target.name));
            }
        }
    }

    let mut argv = vec![bin];
    argv.extend(target.launch.args.iter().cloned());
    // the prompt arguments come first: `opencode run`, `codex exec` take flags after them
    if let Some(prompt) = &opts.prompt {
        if target.launch.prompt.is_empty() {
            return Err(Error::Profile(format!(
                "the {} profile doesn't say how to run a prompt non-interactively; add `prompt` to its [launch] table, or use hlock launch",
                target.name
            )));
        }
        argv.extend(target.launch.prompt.iter().map(|a| a.replace("{prompt}", prompt)));
    }
    let has_config = lock.config.iter().any(|c| c.restore != "record");

    if same && opts.in_place {
        prep.notes.push("using your own config (no isolation)".into());
    } else {
        let prefix = match opts.prefix.clone() {
            Some(p) if p.is_absolute() => p,
            Some(p) => project_dir.join(p),
            // a sandboxed session gets a root of its own: nothing it writes reaches an
            // unsandboxed launch
            None if sandbox.is_some() => project_dir.join(crate::sandbox::SANDBOX_ENV_DIR),
            None => default_prefix,
        };
        sync::ignore_env(&sync::env_dir(target, &prefix, ctx))?;
        // secrets go in the harness's environment where its config can reference them
        let sopts = SyncOptions { prefix: Some(prefix), inject: true, trust: opts.trust, allow_hidden: opts.allow_hidden.clone(), ..Default::default() };
        let (t, _, iso) = sync::target_ctx(target, ctx, &sopts);
        let root = target.config_root(&t);
        std::fs::create_dir_all(&root)?;
        if same {
            let plan = sync::plan(lock, lock_path, reg, ctx, &sopts)?;
            // plugins install through the harness's own CLI, on this machine
            let host_cli = probe.binary.is_some();
            if !host_cli && plan.steps.iter().any(|s| s.status == "auto" && s.kind == "plugin") {
                prep.notes.push(format!("{} isn't installed on this machine, so the lock's plugins aren't installed for the sandbox", target.name));
            }
            for st in plan.steps.iter().filter(|s| s.status == "auto" && matches!(s.kind.as_str(), "config" | "skill" | "instructions" | "plugin" | "mcp") && (host_cli || s.kind != "plugin")) {
                match sync::run_step(st, ctx) {
                    Ok(_) => prep.done.push(st.title.clone()),
                    Err(e) => prep.notes.push(format!("{}: {e}", st.title)),
                }
            }
            // what isn't trusted (or has hidden characters) was left out; say so rather than start without it silently
            for st in plan.steps.iter().filter(|s| s.status == "manual" && (s.reason.starts_with('⚑') || s.reason.starts_with("you denied") || s.reason.contains("hide text"))) {
                prep.notes.push(format!("left out {}: {}", st.title, st.reason));
            }
            if opts.project.is_some() {
                stand_in_project(lock, &plan, &project_dir, &ctx.cwd, &mut prep)?;
            }
            for st in plan.steps.iter().filter(|s| s.kind == "secret" && s.status == "manual") {
                prep.missing_secrets.push(st.id.trim_start_matches("secret:").to_string());
            }
            let (values, missing) = sync::secret_env(lock, lock_path, reg, ctx, &sopts)?;
            prep.secret_env.extend(values);
            prep.missing_secrets.extend(missing);
        } else {
            let gate = Gate::new(lock, lock_path, source, ctx, opts)?;
            translate(lock, lock_path, source, target, &t, &root, ctx, &mut prep, &gate)?;
        }
        prep.env = iso;
        prep.root = Some(root);
    }
    // pinned locks restore the model through config; otherwise pass it explicitly
    if !same || !has_config {
        match (target.launch.model_flag.clone(), model_arg(lock, target, &mut prep.notes)) {
            (Some(flag), Some(m)) => {
                argv.push(flag);
                argv.push(m);
            }
            (None, Some(m)) => prep.notes.push(format!("{} takes no model flag, so it uses its own configured model, not the locked {m}", target.name)),
            _ => {}
        }
    }
    argv.extend(args.iter().cloned());
    prep.argv = argv;
    prep.missing_secrets.sort();
    prep.missing_secrets.dedup();
    if let Some(sb) = &sandbox {
        let root = prep.root.clone().ok_or_else(|| Error::Profile("a sandboxed launch needs an isolated config root".into()))?;
        let mut env = prep.env.clone();
        if sb.spec.secrets == "proxy" {
            // the proxy holds the keys; a lock secret holding one of them stays out too
            let real: Vec<String> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"].iter().filter_map(|k| ctx.var(k).map(str::to_string)).collect();
            let kept: Vec<String> = prep.secret_env.iter().filter(|(_, v)| !real.contains(v)).map(|(k, _)| k.clone()).collect();
            env.extend(prep.secret_env.iter().filter(|(k, _)| kept.contains(k)).map(|(k, v)| (k.clone(), v.clone())));
            if !kept.is_empty() {
                prep.notes.push(format!("{} lock secret{} reach{} the sandbox by environment: {}", kept.len(), if kept.len() == 1 { "" } else { "s" }, if kept.len() == 1 { "es" } else { "" }, kept.join(", ")));
            }
            env.extend(opts.proxy_env.clone());
        } else {
            let keys: Vec<String> = target.api_keys.iter().map(|k| k.env.clone()).collect();
            env.extend(crate::sandbox::provider_env(ctx, &keys));
            env.extend(prep.secret_env.clone());
        }
        let home = sync::env_dir(target, &project_dir.join(crate::sandbox::SANDBOX_ENV_DIR), ctx).join("home");
        let mounts = match &opts.project {
            None => crate::sandbox::project_mounts(&project_dir)?,
            Some(_) => vec![],
        };
        let run = crate::sandbox::Run {
            kind: "agent".into(),
            argv: prep.argv.clone(),
            cwd: ctx.cwd.clone(),
            home,
            config: Some(root),
            mounts,
            env,
            tty: opts.tty,
            host_gateway: !opts.proxy_env.is_empty(),
        };
        let version = lock.harness.version.clone();
        prep.sandbox = Some(sandbox_prep(sb, &run, ctx, &mut prep.notes, version)?);
    }
    Ok(prep)
}

fn sandbox_prep(sb: &crate::sandbox::Sandbox, run: &crate::sandbox::Run, ctx: &Ctx, notes: &mut Vec<String>, version: Option<String>) -> Result<SandboxPrep> {
    let w = sb.wrap(run, ctx)?;
    notes.extend(w.notes.iter().cloned());
    Ok(SandboxPrep {
        spec: sb.spec.clone(),
        summary: sb.spec.summary(),
        argv: w.argv,
        env: w.env,
        env_names: w.env_names,
        kill: w.kill,
        project: sb.project.clone(),
        image: if matches!(sb.runner.name.as_str(), "docker" | "openshell") { if run.kind == "graders" { sb.spec.grader_image.clone().or(sb.spec.image.clone()) } else { sb.spec.image.clone() } } else { None },
        harness_version: version,
        session: w.session,
        session_env: w.session_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_shapes() {
        let stdio = Tool { name: "gh".into(), kind: "mcp".into(), transport: Some("stdio".into()), command: Some("npx".into()), args: vec!["pkg".into()], env: [("T".to_string(), "v".to_string())].into(), ..Default::default() };
        let http = Tool { name: "r".into(), kind: "mcp".into(), transport: Some("http".into()), url: Some("https://x/mcp".into()), headers: [("Authorization".to_string(), "Bearer t".to_string())].into(), ..Default::default() };
        assert_eq!(mcp_entry(&stdio, "standard"), json!({"command": "npx", "args": ["pkg"], "env": {"T": "v"}}));
        assert_eq!(mcp_entry(&stdio, "opencode"), json!({"type": "local", "command": ["npx", "pkg"], "environment": {"T": "v"}, "enabled": true}));
        assert_eq!(mcp_entry(&http, "opencode")["type"], "remote");
        assert_eq!(mcp_entry(&http, "codex"), json!({"url": "https://x/mcp", "http_headers": {"Authorization": "Bearer t"}}));
        assert_eq!(mcp_entry(&stdio, "goose")["cmd"], "npx");
        assert_eq!(mcp_entry(&http, "standard")["type"], "http");
        assert_eq!(mcp_entry(&http, "openclaw"), json!({"transport": "streamable-http", "url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}}));
        assert_eq!(mcp_entry(&stdio, "openclaw"), mcp_entry(&stdio, "standard"));
        assert_eq!(mcp_entry(&http, "nanobot")["type"], "streamableHttp");
    }

    #[test]
    fn user_instructions_land_on_their_own_files() {
        let r = Registry::builtin();
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let oc = names(&["{root}/workspace/AGENTS.md", "{root}/workspace/SOUL.md", "{root}/workspace/TOOLS.md"]);
        // same names match; nanobot has no TOOLS.md, so it doesn't carry over
        assert_eq!(
            user_instruction_targets(r.harness("nanobot").unwrap(), &oc),
            [Some("{root}/workspace/AGENTS.md"), Some("{root}/workspace/SOUL.md"), None]
        );
        // a harness with one file takes the first, and never two onto one path
        assert_eq!(user_instruction_targets(r.harness("claude-code").unwrap(), &oc), [Some("{root}/CLAUDE.md"), None, None]);
        assert_eq!(user_instruction_targets(r.harness("hermes").unwrap(), &oc), [None, Some("{root}/SOUL.md"), None]);
        assert_eq!(user_instruction_targets(r.harness("openclaw").unwrap(), &names(&["{root}/CLAUDE.md"])), [Some("{root}/workspace/AGENTS.md")]);
    }

    #[test]
    fn secrets_from_live_config() {
        let ctx = Ctx { home: "/h".into(), cwd: "/w".into(), env: [("HLOCK_KEYCHAIN".to_string(), "0".to_string())].into(), path: vec![] };
        let t = Tool { name: "gh".into(), kind: "mcp".into(), env: [("TOKEN".to_string(), "${HLOCK_SECRET_GH_TOKEN}".to_string())].into(), ..Default::default() };
        let live = json!({"command": "npx", "env": {"TOKEN": "real"}});
        let (mut missing, mut values) = (BTreeSet::new(), BTreeMap::new());
        assert_eq!(fill_tool(&t, Some(&live), &ctx, None, &mut values, &mut missing).env["TOKEN"], "real");
        assert!(missing.is_empty() && values.is_empty());
        let filled = fill_tool(&t, None, &ctx, None, &mut values, &mut missing);
        assert_eq!(filled.env["TOKEN"], "${HLOCK_SECRET_GH_TOKEN}");
        assert!(missing.contains("HLOCK_SECRET_GH_TOKEN"));
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-launch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_failed_skill_copy_keeps_the_old_one() {
        let d = tmp("replace");
        std::fs::create_dir_all(d.join("dest")).unwrap();
        std::fs::write(d.join("dest/SKILL.md"), "old").unwrap();
        assert!(replace_dir(&d.join("missing"), &d.join("dest")).is_err());
        assert_eq!(std::fs::read_to_string(d.join("dest/SKILL.md")).unwrap(), "old");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/SKILL.md"), "new").unwrap();
        replace_dir(&d.join("src"), &d.join("dest")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("dest/SKILL.md")).unwrap(), "new");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2); // no temp dir left
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn skills_with_path_names_are_skipped() {
        let d = tmp("skillname");
        std::fs::create_dir_all(d.join("skill")).unwrap();
        std::fs::write(d.join("skill/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(d.join("keep")).unwrap();
        let ctx = Ctx { home: d.join("home"), cwd: d.clone(), env: [("HLOCK_KEYCHAIN".to_string(), "0".to_string())].into(), path: vec![] };
        let reg = Registry::builtin();
        let target = reg.harness("claude-code").unwrap();
        let skill = |name: &str| crate::lockfile::Skill { name: name.into(), origin: "user".into(), path: Some(d.join("skill").to_string_lossy().into_owned()), ..Default::default() };
        let lock = Lockfile { skill: vec![skill("../../keep"), skill("ok")], ..Default::default() };
        let root = d.join("root");
        let mut prep = LaunchPrep::default();
        translate(&lock, &d.join("harness.lock"), None, target, &ctx, &root, &ctx, &mut prep, &Gate { blocked: Vec::new(), allow_hidden: Vec::new() }).unwrap();
        assert!(d.join("keep").is_dir(), "a path-named skill must not replace anything");
        assert!(prep.notes.iter().any(|n| n.contains("\"../../keep\" wasn't copied")), "{:?}", prep.notes);
        assert!(prep.done.iter().any(|n| n.starts_with("copied 1 skill")), "{:?}", prep.done);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn configs_with_secrets_are_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("private");
        write_pointer(&d.join("a.json"), "json", "/mcpServers", json!({"x": {"env": {"T": "s3"}}}), true).unwrap();
        assert_eq!(std::fs::metadata(d.join("a.json")).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn secrets_as_environment_references() {
        let ctx = Ctx { home: "/h".into(), cwd: "/w".into(), env: [("HLOCK_KEYCHAIN".to_string(), "0".to_string())].into(), path: vec![] };
        let t = Tool { name: "api".into(), kind: "mcp".into(), headers: [("Authorization".to_string(), "Bearer ${HLOCK_SECRET_API}".to_string())].into(), ..Default::default() };
        let live = json!({"headers": {"Authorization": "Bearer abc"}});
        let (mut missing, mut values) = (BTreeSet::new(), BTreeMap::new());
        let filled = fill_tool(&t, Some(&live), &ctx, Some("{env:{name}}"), &mut values, &mut missing);
        assert_eq!(filled.headers["Authorization"], "Bearer {env:HLOCK_SECRET_API}"); // no value on disk
        assert_eq!(values["HLOCK_SECRET_API"], "abc"); // passed in the environment instead
        assert!(missing.is_empty());
    }
}
