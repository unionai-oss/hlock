//! The execution contract for code harnesses (SPEC §7.8.3): how hlock runs a
//! framework agent or a custom script declared under `[harnesses.<id>]`.
//!
//! hlock passes everything through environment variables (`HLOCK_PROMPT`,
//! `HLOCK_MODEL`, `HLOCK_LLM_BASE_URL`, `HLOCK_WORKSPACE`, `HLOCK_MCP_CONFIG`,
//! `HLOCK_SKILLS_DIR`, `HLOCK_LOCK`, `HLOCK_OUTPUT`, `HLOCK_SEED`) and reads an
//! optional JSON result from `HLOCK_OUTPUT`; without one, exit code 0 means
//! success. A Python `entrypoint` runs through a small shim (`entry_shim.py`,
//! passed with `python -c`), so both kinds share one path and the agent's
//! interpreter needs nothing from hlock installed. [`CommandAdapter`] plugs this into the resolver,
//! so `resolve` evaluates code harnesses for real.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::adapters::{HarnessAdapter, RunOutcome};
use crate::error::{Error, Result};
use crate::fsutil;
use crate::lockfile::{Lockfile, Tool};
use crate::process::{self, Limits};
use crate::profiles::{python_for, Ctx, HarnessProfile};
use crate::search::Stack;
use crate::tasks::Task;

/// The Python shim that calls `module:callable(prompt, ctx)` and writes the result.
pub const ENTRY_SHIM: &str = include_str!("entry_shim.py");

/// A ready-to-run code-harness invocation.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Where the harness may write its JSON result.
    pub output: PathBuf,
}

/// The result of one run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RunResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub output: Option<Value>,
    pub cost_usd: f64,
    pub steps: u64,
    pub latency_s: f64,
    pub trace: Value,
    pub log: String,
}

fn subst(s: &str, vars: &[(&str, &str)]) -> String {
    vars.iter().fold(s.to_string(), |acc, (k, v)| acc.replace(&format!("{{{k}}}"), v))
}

/// Replace `${HLOCK_SECRET_…}` placeholders in a string with their values, from the
/// environment or the user's secret sources ([`crate::secrets`]). Any other `${…}` is
/// left as it is, so a lock can't copy arbitrary variables into a config file.
fn fill_env(s: &str, ctx: &Ctx) -> String {
    let mut out = s.to_string();
    let mut start = 0;
    while let Some(i) = out[start..].find("${") {
        let i = start + i;
        let Some(j) = out[i..].find('}') else { break };
        let name = out[i + 2..i + j].to_string();
        let found = if name.starts_with(crate::secrets::PREFIX) { crate::secrets::lookup(&name, ctx).map(|f| f.value) } else { None };
        match found {
            Some(v) => {
                out.replace_range(i..i + j + 1, &v);
                start = i + v.len();
            }
            None => start = i + j + 1,
        }
    }
    out
}

/// A standard `mcpServers` document for locked MCP servers (secrets filled from the environment).
pub fn mcp_config(tools: &[Tool], ctx: &Ctx) -> Value {
    let mut servers = Map::new();
    for t in tools.iter().filter(|t| t.kind == "mcp" && t.enabled != Some(false)) {
        let fill_map = |m: &BTreeMap<String, String>| -> Value {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), json!(fill_env(v, ctx)))).collect())
        };
        let entry = match (t.transport.as_deref(), &t.command, &t.url) {
            (Some("http") | Some("sse"), _, Some(url)) => {
                let mut e = json!({"type": t.transport, "url": fill_env(url, ctx)});
                if !t.headers.is_empty() {
                    e["headers"] = fill_map(&t.headers);
                }
                e
            }
            (_, Some(cmd), _) => {
                let mut e = json!({"command": cmd, "args": t.args.iter().map(|a| fill_env(a, ctx)).collect::<Vec<_>>()});
                if !t.env.is_empty() {
                    e["env"] = fill_map(&t.env);
                }
                e
            }
            _ => continue,
        };
        servers.insert(t.name.clone(), entry);
    }
    json!({ "mcpServers": servers })
}

/// Build the invocation for a code harness. `model` is `provider:model`; when
/// `None` it comes from the lock's first LLM role.
pub fn invocation(
    profile: &HarnessProfile,
    lock: Option<&Lockfile>,
    lock_path: Option<&Path>,
    model: Option<&str>,
    prompt: &str,
    workspace: &Path,
    seed: u64,
    ctx: &Ctx,
) -> Result<Invocation> {
    // the harness runs inside the workspace, so every path it gets must be absolute
    let workspace = &if workspace.is_absolute() { workspace.to_path_buf() } else { ctx.cwd.join(workspace) };
    let hl = workspace.join(".hlock");
    fsutil::clear_symlink(&hl)?;
    std::fs::create_dir_all(&hl)?;
    let output = hl.join("result.json");
    let _ = std::fs::remove_file(&output);

    let llm = lock.and_then(|l| l.llm.iter().find(|x| x.role == "default").or_else(|| l.llm.first()));
    let model = model.map(str::to_string).or_else(|| llm.map(|l| format!("{}:{}", l.provider, l.model))).unwrap_or_default();
    let base_url = llm.and_then(|l| l.endpoint.clone()).or_else(|| ctx.var("HL_LLM_BASE_URL").map(str::to_string));

    let mut env: BTreeMap<String, String> = profile.run.env.clone();
    env.insert("HLOCK_PROMPT".into(), prompt.into());
    env.insert("HLOCK_MODEL".into(), model.clone());
    env.insert("HLOCK_WORKSPACE".into(), workspace.to_string_lossy().into_owned());
    env.insert("HLOCK_OUTPUT".into(), output.to_string_lossy().into_owned());
    env.insert("HLOCK_SEED".into(), seed.to_string());
    env.insert("HLOCK_HARNESS".into(), profile.id.clone());
    if let Some(b) = &base_url {
        env.insert("HLOCK_LLM_BASE_URL".into(), b.clone());
    }
    if let Some(l) = lock {
        let deciders: Vec<(String, crate::decide::DeciderConfig)> = l.decider.iter().map(|d| (d.use_.clone(), crate::decide::entry_config_for(d, crate::decide::own_spec(&ctx.cwd, &d.use_).as_ref()))).collect();
        let var = |k: &str| ctx.var(k).map(str::to_string);
        env.extend(crate::decide::decider_env(&deciders, &profile.decider_env, &var));
        if let (false, Some(p)) = (l.decider.is_empty(), lock_path) {
            // `hlock decide hook <use> --format plugin --lock $HLOCK_DECIDE_LOCK` asks them
            let abs = if p.is_absolute() { p.to_path_buf() } else { ctx.cwd.join(p) };
            env.insert("HLOCK_DECIDE_LOCK".into(), abs.to_string_lossy().into_owned());
            env.insert("HLOCK_BIN".into(), ctx.var("HLOCK_BIN").unwrap_or("hlock").to_string());
        }
    }
    if let Some(p) = lock_path {
        let abs = if p.is_absolute() { p.to_path_buf() } else { ctx.cwd.join(p) };
        env.insert("HLOCK_LOCK".into(), abs.to_string_lossy().into_owned());
    }
    if let Some(l) = lock {
        if l.tool.iter().any(|t| t.kind == "mcp") {
            let path = hl.join("mcp.json");
            // it holds filled-in secrets
            fsutil::write_private(&path, serde_json::to_string_pretty(&mcp_config(&l.tool, ctx))?)?;
            env.insert("HLOCK_MCP_CONFIG".into(), path.to_string_lossy().into_owned());
        }
        let lock_dir = lock_path.and_then(Path::parent).map(|d| if d.is_absolute() { d.to_path_buf() } else { ctx.cwd.join(d) }).unwrap_or_else(|| ctx.cwd.clone());
        let mut skills: Vec<(String, PathBuf)> = Vec::new();
        for s in &l.skill {
            let Some(dir) = s.vendored.as_ref().map(|v| lock_dir.join(v)).or_else(|| s.path.as_ref().map(|p| ctx.expand(p, None))) else { continue };
            if dir.join("SKILL.md").is_file() {
                fsutil::check_plain_name("skill name", &s.name)?;
                skills.push((s.name.clone(), dir));
            }
        }
        // the lock's instruction files that have a copy next to it (a tune candidate's)
        let instructions: Vec<(&str, PathBuf)> = l.instructions.iter().filter_map(|i| i.vendored.as_ref().filter(|v| fsutil::safe_rel(v) && fsutil::safe_rel(&i.name)).map(|v| (i.name.as_str(), lock_dir.join(v)))).filter(|(_, p)| p.is_file()).collect();
        if !instructions.is_empty() {
            let dir = hl.join("instructions");
            let _ = std::fs::remove_dir_all(&dir);
            for (name, src) in instructions {
                let dest = dir.join(name);
                std::fs::create_dir_all(dest.parent().unwrap_or(&dir))?;
                std::fs::copy(&src, &dest)?;
            }
            env.insert("HLOCK_INSTRUCTIONS_DIR".into(), dir.to_string_lossy().into_owned());
        }
        if !skills.is_empty() {
            let dir = hl.join("skills");
            let _ = std::fs::remove_dir_all(&dir);
            for (name, src) in skills {
                fsutil::copy_tree(&src, &dir.join(name))?;
            }
            env.insert("HLOCK_SKILLS_DIR".into(), dir.to_string_lossy().into_owned());
        }
    }

    let vars = [("prompt", prompt), ("model", model.as_str()), ("workspace", &*workspace.to_string_lossy())];
    let argv: Vec<String> = if let Some(ep) = &profile.run.entrypoint {
        let py = python_for(profile.run.python.as_deref(), ctx).ok_or_else(|| Error::Profile("no Python interpreter found (set HL_PYTHON)".into()))?;
        // the project directory must be importable
        let pp = match ctx.var("PYTHONPATH") {
            Some(old) => format!("{}{}{old}", ctx.cwd.display(), if cfg!(windows) { ";" } else { ":" }),
            None => ctx.cwd.to_string_lossy().into_owned(),
        };
        env.insert("PYTHONPATH".into(), pp);
        vec![py.to_string_lossy().into_owned(), "-c".into(), ENTRY_SHIM.into(), ep.clone()]
    } else if !profile.run.command.is_empty() {
        let mut a: Vec<String> = profile.run.command.iter().map(|x| subst(x, &vars)).collect();
        if a[0].contains('/') && !Path::new(&a[0]).is_absolute() {
            a[0] = ctx.cwd.join(&a[0]).to_string_lossy().into_owned();
        }
        a
    } else {
        return Err(Error::Profile(format!("harness {:?} has no entrypoint or command", profile.id)));
    };
    Ok(Invocation { argv, env, cwd: workspace.to_path_buf(), output })
}

/// Run an invocation to completion, capturing output, with a timeout.
pub fn run(inv: &Invocation, ctx: &Ctx, timeout: Duration) -> Result<RunResult> {
    let start = Instant::now();
    let Some((prog, args)) = inv.argv.split_first() else {
        return Err(Error::Profile("the harness invocation has no command".into()));
    };
    let mut cmd = Command::new(prog);
    cmd.args(args).current_dir(&inv.cwd).env_clear().envs(&ctx.env).envs(&inv.env).stdin(Stdio::null());
    let out = process::run(cmd, &Limits::timeout(timeout)).map_err(|e| Error::Profile(format!("cannot run {prog}: {e}")))?;
    let timed_out = out.timed_out;
    let mut log = out.text();
    // the harness wrote it into its workspace: read it as a plain file, never through a symlink
    let parsed: Option<Value> = fsutil::read_regular_text(&inv.output, MAX_RESULT_FILE).ok().and_then(|t| serde_json::from_str(&t).ok());
    let exit_ok = out.success && !timed_out;
    let success = parsed.as_ref().and_then(|v| v.get("success")).and_then(Value::as_bool).unwrap_or(exit_ok) && !timed_out;
    if timed_out {
        log.push_str(&format!("\n[hlock] timed out after {}s", timeout.as_secs()));
    }
    Ok(RunResult {
        success,
        exit_code: out.code,
        cost_usd: parsed.as_ref().and_then(|v| v["cost_usd"].as_f64()).unwrap_or(0.0),
        steps: parsed.as_ref().and_then(|v| v["steps"].as_u64()).unwrap_or(1),
        trace: parsed.as_ref().and_then(|v| v.get("trace").cloned()).unwrap_or_else(|| json!([])),
        latency_s: (start.elapsed().as_secs_f64() * 1e3).round() / 1e3,
        output: parsed,
        log,
    })
}

/// The most of a harness's JSON result hlock reads.
const MAX_RESULT_FILE: usize = 16 << 20;

/// Runs trials through a code harness's entrypoint, for `resolve` and `tune`.
pub struct CommandAdapter {
    profile: HarnessProfile,
    ctx: Ctx,
    deciders: BTreeMap<String, crate::decide::DeciderSpec>,
    /// A lock whose skills and MCP servers each trial gets (`hlock add --eval`).
    lock: Option<(crate::lockfile::Lockfile, PathBuf)>,
}

impl CommandAdapter {
    pub fn new(profile: HarnessProfile, ctx: Ctx) -> Self {
        CommandAdapter { profile, ctx, deciders: BTreeMap::new(), lock: None }
    }

    /// Give each trial a lock's skills and MCP servers, as `hlock launch` would.
    pub fn with_lock(mut self, lock: Option<(crate::lockfile::Lockfile, PathBuf)>) -> Self {
        self.lock = lock;
        self
    }

    /// The manifest's `[deciders.*]`, for the endpoint settings of a stack's deciders.
    pub fn with_deciders(mut self, deciders: BTreeMap<String, crate::decide::DeciderSpec>) -> Self {
        self.deciders = deciders;
        self
    }
}

impl HarnessAdapter for CommandAdapter {
    fn name(&self) -> &str {
        &self.profile.id
    }

    fn run(&self, stack: &Stack, task: &Task, workspace: &Path, seed: u64) -> Result<RunOutcome> {
        if let (Some(fixtures), Some(pack_dir)) = (&task.fixtures, &task.pack_dir) {
            let src = pack_dir.join(fixtures);
            if src.exists() {
                fsutil::copy_tree(&src, workspace)?;
            }
        }
        let model = stack.llm.get("planner").or_else(|| stack.llm.values().next()).cloned();
        let (lock, lock_path) = match &self.lock {
            Some((l, p)) => (Some(l), Some(p.as_path())),
            None => (None, None),
        };
        let mut inv = invocation(&self.profile, lock, lock_path, model.as_deref(), &task.prompt_text, workspace, seed, &self.ctx)?;
        let deciders: Vec<(String, crate::decide::DeciderConfig)> = stack
            .deciders
            .iter()
            .map(|(u, m)| (u.clone(), self.deciders.get(u).map(|s| s.config(m)).unwrap_or_else(|| crate::decide::DeciderConfig::from_ref(crate::decide::split_threshold(m).0))))
            .collect();
        let var = |k: &str| self.ctx.var(k).map(str::to_string);
        inv.env.extend(crate::decide::decider_env(&deciders, &self.profile.decider_env, &var));
        if !stack.deciders.is_empty() {
            // a lock holding just this trial's deciders, for `hlock decide hook --format plugin`
            // a decider that can't be locked fails the trial: running without it would
            // measure a different stack than the one being scored
            let entries: Vec<crate::lockfile::Decider> = stack
                .deciders
                .iter()
                .map(|(u, m)| {
                    let spec = self.deciders.get(u).cloned().unwrap_or_default();
                    crate::decide::lock_entry(u, m, &spec, "env", vec![], &self.ctx.cwd).map_err(|e| Error::Profile(format!("decider {u}: {e}")))
                })
                .collect::<Result<_>>()?;
            // absolute: the harness runs in the workspace, so a relative path would miss
            let ws = if inv.cwd.is_absolute() { inv.cwd.clone() } else { self.ctx.cwd.join(&inv.cwd) };
            let path = ws.join(".hlock").join("deciders.lock");
            let lock = Lockfile { decider: entries, ..Default::default() };
            lock.save(&path)?;
            inv.env.insert("HLOCK_DECIDE_LOCK".into(), path.to_string_lossy().into_owned());
            inv.env.insert("HLOCK_BIN".into(), self.ctx.var("HLOCK_BIN").unwrap_or("hlock").to_string());
        }
        let r = run(&inv, &self.ctx, Duration::from_secs(task.timeout_seconds.max(1)))?;
        let mut trace = r.trace.clone();
        if let Value::Array(a) = &mut trace {
            let tail: Vec<&str> = r.log.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
            a.push(json!({"type": "harness.exit", "harness": self.profile.id, "code": r.exit_code, "latency_s": r.latency_s, "log_tail": tail.join("\n")}));
        }
        Ok(RunOutcome { success: r.success, cost_usd: r.cost_usd, latency_s: r.latency_s, steps: r.steps, trace })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_and_env_fill() {
        assert_eq!(subst("--model={model} {prompt}", &[("model", "a:b"), ("prompt", "hi")]), "--model=a:b hi");
        let env = [("HLOCK_SECRET_TOK", "x"), ("HOME_TOKEN", "leak"), ("HLOCK_KEYCHAIN", "0")].map(|(k, v)| (k.to_string(), v.to_string()));
        let ctx = Ctx { home: "/h".into(), cwd: "/w".into(), env: env.into(), path: vec![] };
        // only secret placeholders are filled; other variables stay as written
        assert_eq!(fill_env("Bearer ${HLOCK_SECRET_TOK} ${HOME_TOKEN} ${HLOCK_SECRET_MISSING}", &ctx), "Bearer x ${HOME_TOKEN} ${HLOCK_SECRET_MISSING}");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-contract-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn inv(argv: &[&str], cwd: &Path) -> Invocation {
        Invocation { argv: argv.iter().map(|s| s.to_string()).collect(), env: BTreeMap::new(), cwd: cwd.into(), output: cwd.join("result.json") }
    }

    #[test]
    fn an_empty_command_is_an_error() {
        let d = tmp("empty");
        assert!(run(&inv(&[], &d), &Ctx::at(&d), Duration::from_secs(5)).unwrap_err().to_string().contains("no command"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn big_output_doesnt_hang() {
        let d = tmp("run");
        let ctx = Ctx::at(&d);
        let r = run(&inv(&["sh", "-c", "head -c 300000 /dev/zero | tr '\\0' x; exit 2"], &d), &ctx, Duration::from_secs(20)).unwrap();
        assert_eq!((r.success, r.exit_code), (false, Some(2)));
        assert!(r.log.len() >= 300_000);
        // the timeout is tested in tests/bounded.rs, away from process::stop_all's test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn mcp_config_is_private_and_skill_names_are_checked() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("inv");
        std::fs::create_dir_all(d.join("skill")).unwrap();
        std::fs::write(d.join("skill/SKILL.md"), "x").unwrap();
        let ctx = Ctx { env: [("HLOCK_SECRET_T".to_string(), "s3".to_string())].into(), ..Ctx::at(&d) };
        let profile = HarnessProfile { id: "h".into(), run: crate::profiles::RunSpec { command: vec!["true".into()], ..Default::default() }, ..Default::default() };
        let tool = Tool { name: "gh".into(), kind: "mcp".into(), command: Some("npx".into()), env: [("T".to_string(), "${HLOCK_SECRET_T}".to_string())].into(), ..Default::default() };
        let skill = |name: &str| crate::lockfile::Skill { name: name.into(), path: Some(d.join("skill").to_string_lossy().into_owned()), ..Default::default() };
        let mut lock = Lockfile { tool: vec![tool], skill: vec![skill("ok")], ..Default::default() };
        let ws = d.join("ws");
        let i = invocation(&profile, Some(&lock), None, None, "hi", &ws, 0, &ctx).unwrap();
        let mcp = PathBuf::from(&i.env["HLOCK_MCP_CONFIG"]);
        assert_eq!(std::fs::metadata(&mcp).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(ws.join(".hlock/skills/ok/SKILL.md").is_file());
        lock.skill = vec![skill("../../escape")];
        assert!(invocation(&profile, Some(&lock), None, None, "hi", &ws, 0, &ctx).unwrap_err().to_string().contains("plain name"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mcp_document() {
        let ctx = Ctx { home: "/h".into(), cwd: "/w".into(), env: [("HLOCK_SECRET_T".to_string(), "s3".to_string())].into(), path: vec![] };
        let tools = vec![
            Tool { name: "gh".into(), kind: "mcp".into(), transport: Some("stdio".into()), command: Some("npx".into()), args: vec!["pkg".into()], env: [("T".to_string(), "${HLOCK_SECRET_T}".to_string())].into(), ..Default::default() },
            Tool { name: "remote".into(), kind: "mcp".into(), transport: Some("http".into()), url: Some("https://x/mcp".into()), ..Default::default() },
            Tool { name: "off".into(), kind: "mcp".into(), command: Some("x".into()), enabled: Some(false), ..Default::default() },
        ];
        let v = mcp_config(&tools, &ctx);
        assert_eq!(v["mcpServers"]["gh"]["env"]["T"], "s3");
        assert_eq!(v["mcpServers"]["remote"]["type"], "http");
        assert!(v["mcpServers"].get("off").is_none());
    }
}
