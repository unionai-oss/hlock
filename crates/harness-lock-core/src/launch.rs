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
use crate::lockfile::{Lockfile, Tool};
use crate::profiles::{probe_harness, Ctx, HarnessProfile, Registry};
use crate::sync::{self, copy_tree, lock_dir_of, placeholders, pretty, set_pointer, SyncOptions};

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
    let remote = matches!(t.transport.as_deref(), Some("http") | Some("sse")) && t.url.is_some();
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

/// Fill `${HLOCK_SECRET_*}` in a tool from the environment, else from the same
/// server's entry in the source harness's live config.
fn fill_tool(t: &Tool, live: Option<&Value>, ctx: &Ctx, missing: &mut BTreeSet<String>) -> Tool {
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
        let mut out = v.to_string();
        let mut unfilled = false;
        for n in &names {
            match ctx.var(n) {
                Some(val) => out = out.replace(&format!("${{{n}}}"), val),
                None => unfilled = true,
            }
        }
        if unfilled {
            if let Some(lv) = live_value.filter(|x| placeholders(x).is_empty()) {
                return lv;
            }
            missing.extend(names.into_iter().filter(|n| ctx.var(n).is_none()));
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

fn write_pointer(path: &Path, format: &str, ptr: &str, value: Value) -> Result<()> {
    let mut doc = read_config(path, format)?.filter(Value::is_object).unwrap_or_else(|| json!({}));
    set_pointer(&mut doc, ptr, value);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fmt = if format == "jsonc" { "json" } else { format };
    std::fs::write(path, render_config(&doc, fmt).map_err(Error::Profile)?)?;
    Ok(())
}

/// Translate a lock's portable components into `target`'s isolated root.
fn translate(lock: &Lockfile, lock_path: &Path, source: Option<&HarnessProfile>, target: &HarnessProfile, t: &Ctx, root: &Path, ctx: &Ctx, prep: &mut LaunchPrep) -> Result<()> {
    let lock_dir = lock_dir_of(lock_path, ctx);
    let source_root = source.map(|s| s.config_root(ctx));

    // MCP servers
    let tools: Vec<&Tool> = lock.tool.iter().filter(|x| x.kind == "mcp").collect();
    if !tools.is_empty() {
        match target.mcp.write.as_deref().and_then(|w| w.split_once('#')) {
            Some((file, ptr)) => {
                let spec = target.files.get(file).ok_or_else(|| Error::Profile(format!("{} profile: mcp.write names unknown file {file:?}", target.id)))?;
                let path = target.file_path(file, t).unwrap_or_default();
                let live = source.map(|s| live_mcp_servers(s, ctx)).unwrap_or_default();
                let shape = target.mcp.shape.as_deref().unwrap_or("standard");
                let mut missing = BTreeSet::new();
                let servers: Map<String, Value> = tools
                    .iter()
                    .filter(|x| x.enabled != Some(false) || matches!(shape, "opencode" | "goose"))
                    .map(|x| (x.name.clone(), mcp_entry(&fill_tool(x, live.get(&x.name), ctx, &mut missing), shape)))
                    .collect();
                let n = servers.len();
                write_pointer(&path, &spec.format, ptr, Value::Object(servers))?;
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
            let dest = dir.join(&s.name);
            if dest.join("SKILL.md").is_file() && Some(tree_digest(&dest)) == s.digest {
                continue;
            }
            let _ = std::fs::remove_dir_all(&dest);
            copy_tree(&src, &dest)?;
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

    // user-level instruction files → the target's user-level instruction file
    let user_ins: Vec<_> = lock.instructions.iter().filter(|i| i.scope == "user").collect();
    if let Some(first) = user_ins.first() {
        let dest_tpl = target.instructions.iter().find(|i| i.starts_with("{root}/"));
        let src = first.vendored.as_ref().map(|v| lock_dir.join(v)).or_else(|| Some(ctx.expand(&first.name, source_root.as_deref()))).filter(|p| p.is_file());
        match (dest_tpl, src) {
            (Some(tpl), Some(src)) => {
                let dest = t.expand(tpl, Some(root));
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src, &dest)?;
                prep.done.push(format!("wrote {} from {}", pretty(&dest, ctx), first.name));
            }
            _ => prep.notes.push(format!("user instructions {} weren't carried over", first.name)),
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

/// Where a lock's isolated environments live: [`ENV_DIR`], or `ENV_DIR/<stem>` for a
/// lock not named `harness.lock`, so two locks never share a root.
pub fn env_prefix(lock_path: &Path, ctx: &Ctx) -> PathBuf {
    let dir = match lock_path.file_stem().and_then(|s| s.to_str()) {
        Some("harness") | None => PathBuf::from(ENV_DIR),
        Some(stem) => PathBuf::from(ENV_DIR).join(stem),
    };
    ctx.cwd.join(dir)
}

/// Prepare a launch: verify, materialize, and build the command to `exec`.
/// `args` are passed through to the harness (or, for a code harness, joined into the prompt).
pub fn prepare(lock: &Lockfile, lock_path: &Path, reg: &Registry, ctx: &Ctx, opts: &LaunchOptions, args: &[String]) -> Result<LaunchPrep> {
    let source = reg.harness(&lock.harness.name);
    let target_name = opts.target.clone().unwrap_or_else(|| lock.harness.name.clone());
    let target = reg.require_harness(&target_name)?;
    let same = source.is_some_and(|s| s.id == target.id);
    let mut prep = LaunchPrep { harness: target.id.clone(), harness_name: target.name.clone(), kind: target.kind.clone(), translated: !same, ..Default::default() };

    if target.kind != "cli" {
        let workspace = opts.workspace.clone().unwrap_or_else(|| ctx.cwd.clone());
        let prompt = args.join(" ");
        let inv = crate::contract::invocation(target, Some(lock), Some(lock_path), None, &prompt, &workspace, 0, ctx)?;
        prep.argv = inv.argv;
        prep.env = inv.env;
        return Ok(prep);
    }

    let probe = probe_harness(target, ctx);
    let bin = probe.binary.clone().ok_or_else(|| {
        let hint = if same { "run `hlock sync --install`" } else { "install it first" };
        Error::Profile(format!("{} isn't installed; {hint}", target.name))
    })?;
    if same {
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
    } else {
        prep.notes.push(format!("translating a {} lock into {}", source.map(|s| s.name.as_str()).unwrap_or(&lock.harness.name), target.name));
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
            Some(p) => ctx.cwd.join(p),
            None => env_prefix(lock_path, ctx),
        };
        sync::ignore_env(&sync::env_dir(target, &prefix, ctx))?;
        let sopts = SyncOptions { prefix: Some(prefix), ..Default::default() };
        let (t, _, iso) = sync::target_ctx(target, ctx, &sopts);
        let root = target.config_root(&t);
        std::fs::create_dir_all(&root)?;
        if same {
            let plan = sync::plan(lock, lock_path, reg, ctx, &sopts)?;
            for st in plan.steps.iter().filter(|s| s.status == "auto" && matches!(s.kind.as_str(), "config" | "skill" | "instructions" | "plugin")) {
                match sync::run_step(st, ctx) {
                    Ok(_) => prep.done.push(st.title.clone()),
                    Err(e) => prep.notes.push(format!("{}: {e}", st.title)),
                }
            }
            for st in plan.steps.iter().filter(|s| s.kind == "secret" && s.status == "manual") {
                prep.missing_secrets.push(st.id.trim_start_matches("secret:").to_string());
            }
        } else {
            translate(lock, lock_path, source, target, &t, &root, ctx, &mut prep)?;
        }
        prep.env = iso;
        prep.root = Some(root);
    }
    // pinned locks restore the model through config; otherwise pass it explicitly
    if !same || !has_config {
        if let (Some(flag), Some(m)) = (target.launch.model_flag.clone(), model_arg(lock, target, &mut prep.notes)) {
            argv.push(flag);
            argv.push(m);
        }
    }
    argv.extend(args.iter().cloned());
    prep.argv = argv;
    prep.missing_secrets.sort();
    prep.missing_secrets.dedup();
    Ok(prep)
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
    }

    #[test]
    fn secrets_from_live_config() {
        let ctx = Ctx { home: "/h".into(), cwd: "/w".into(), env: BTreeMap::new(), path: vec![] };
        let t = Tool { name: "gh".into(), kind: "mcp".into(), env: [("TOKEN".to_string(), "${HLOCK_SECRET_GH_TOKEN}".to_string())].into(), ..Default::default() };
        let live = json!({"command": "npx", "env": {"TOKEN": "real"}});
        let mut missing = BTreeSet::new();
        assert_eq!(fill_tool(&t, Some(&live), &ctx, &mut missing).env["TOKEN"], "real");
        assert!(missing.is_empty());
        let filled = fill_tool(&t, None, &ctx, &mut missing);
        assert_eq!(filled.env["TOKEN"], "${HLOCK_SECRET_GH_TOKEN}");
        assert!(missing.contains("HLOCK_SECRET_GH_TOKEN"));
    }
}
