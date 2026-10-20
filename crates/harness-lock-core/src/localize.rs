//! `hlock localize`: the best all-local equivalent of a lock (SPEC §7.8.7).
//!
//! Takes any lock (pinned, resolved, tuned) and replaces what isn't local:
//! a proprietary harness, or one that can't use a local model, becomes a
//! local-capable one; LLM roles and the decision model on API providers move
//! to a local runtime, picked from the runtime's model catalog and the weights
//! already on disk, filtered to what fits this machine's memory. Portable
//! components carry over. With eval packs it evaluates the candidates through
//! the resolver; otherwise it ranks them and marks the result unevaluated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::capture::{drop_nulls, locate_model, render_config, VendorFile, VENDOR_DIR};
use crate::error::{Error, Result};
use crate::launch::mcp_entry;
use crate::lockfile::{ConfigFile, Evidence, Harness, Lockfile, Runtime, LLM};
use crate::manifest::{HarnessCandidate, LLMCandidate, Manifest};
use crate::profiles::{probe_harness, probe_runtime, run_capture, CatalogModel, Ctx, HarnessProfile, Registry, RuntimeProfile};
use crate::sync::set_pointer;

/// Local-capable harnesses `localize` tries, most preferred first.
pub const DEFAULT_HARNESSES: &[&str] = &["opencode", "pi", "codex", "hermes", "goose"];
/// Local runtimes, most preferred first.
pub const DEFAULT_RUNTIMES: &[&str] = &["ollama", "lmstudio", "llama-cpp", "mlx-lm", "vllm", "sglang", "localai"];

#[derive(Debug, Clone, Default)]
pub struct LocalizeOptions {
    pub harness: Option<String>,
    pub runtime: Option<String>,
    /// Exact model ids to consider (default: the runtime's catalog and weights on disk).
    pub models: Vec<String>,
    /// Memory budget for model weights, overriding the machine probe.
    pub memory_gb: Option<f64>,
    /// A manifest whose eval packs evaluate the candidates.
    pub manifest: Option<Manifest>,
    /// Drop remote MCP servers instead of flagging them.
    pub strict: bool,
    /// Keep self-hosted (network) runtimes instead of replacing them.
    pub allow_network: bool,
    /// Where trial artifacts go.
    pub workdir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LocalizeOutcome {
    pub lock: Lockfile,
    pub vendor: Vec<VendorFile>,
    pub report: Value,
}

/// Total memory, the share usable for model weights, and how it was measured.
pub fn machine_memory(ctx: &Ctx) -> Option<(f64, f64, String)> {
    let t = Duration::from_secs(5);
    let gb = |b: f64| (b / 1073741824.0 * 10.0).round() / 10.0; // GiB, as memory is sold
    if let Some(smi) = ctx.which("nvidia-smi") {
        let args = ["--query-gpu=memory.total", "--format=csv,noheader,nounits"].map(String::from);
        if let Some(out) = run_capture(&smi, &args, ctx, t) {
            let mib: f64 = out.lines().filter_map(|l| l.trim().parse::<f64>().ok()).sum();
            if mib > 0.0 {
                return Some((gb(mib * 1048576.0), gb(mib * 1048576.0 * 0.9), "NVIDIA GPU memory".into()));
            }
        }
    }
    if cfg!(target_os = "macos") {
        let sysctl = ctx.which("sysctl").unwrap_or_else(|| PathBuf::from("/usr/sbin/sysctl"));
        let bytes: f64 = run_capture(&sysctl, &["-n".into(), "hw.memsize".into()], ctx, t)?.lines().next()?.trim().parse().ok()?;
        let unified = cfg!(target_arch = "aarch64");
        let share = if unified { 0.75 } else { 0.5 };
        return Some((gb(bytes), gb(bytes * share), if unified { "unified memory (75%)" } else { "RAM (50%)" }.into()));
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: f64 = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some((gb(kb * 1024.0), gb(kb * 1024.0 * 0.5), "RAM (50%)".into()))
}

/// Parameter count (billions) from a model id like `qwen3:8b` or `Qwen3-30B-A3B`.
fn params_from_id(id: &str) -> Option<f64> {
    let lower = id.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut best = None;
    for (i, &c) in b.iter().enumerate() {
        if c != b'b' || i == 0 || !b[i - 1].is_ascii_digit() {
            continue;
        }
        if b.get(i + 1).is_some_and(|x| x.is_ascii_alphanumeric()) {
            continue;
        }
        let mut j = i;
        while j > 0 && (b[j - 1].is_ascii_digit() || b[j - 1] == b'.') {
            j -= 1;
        }
        if j > 0 && b[j - 1].is_ascii_alphabetic() && b[j - 1] != b'-' && b[j - 1] != b':' && b[j - 1] != b'_' {
            // e.g. "a3b" (active params) — skip
            if b[j - 1] == b'a' {
                continue;
            }
        }
        if let Ok(v) = lower[j..i].parse::<f64>() {
            best = Some(best.map_or(v, |x: f64| x.max(v)));
        }
    }
    best
}

fn tier_for(params: Option<f64>) -> u8 {
    match params {
        Some(p) if p >= 25.0 => 3,
        Some(p) if p >= 12.0 => 2,
        _ => 1,
    }
}

/// A model candidate with what's known about it here.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
struct Candidate {
    id: String,
    tier: u8,
    params_b: Option<f64>,
    min_memory_gb: Option<f64>,
    present: bool,
    tools: bool,
    roles: Vec<String>,
    from: &'static str,
}

/// Models already in an Ollama store, with their sizes.
fn ollama_models_on_disk(rt: &RuntimeProfile, ctx: &Ctx) -> Vec<(String, u64)> {
    let Some(dir) = rt.models_dir(ctx) else { return vec![] };
    let lib = dir.join("manifests/registry.ollama.ai/library");
    let mut out = Vec::new();
    for name in std::fs::read_dir(&lib).into_iter().flatten().flatten() {
        for tag in std::fs::read_dir(name.path()).into_iter().flatten().flatten() {
            let id = format!("{}:{}", name.file_name().to_string_lossy(), tag.file_name().to_string_lossy());
            let m = locate_model(rt, &id, ctx);
            out.push((id, m.size_bytes.unwrap_or(0)));
        }
    }
    out.sort();
    out
}

fn candidates(rt: &RuntimeProfile, opts: &LocalizeOptions, usable_gb: Option<f64>, ctx: &Ctx) -> Vec<Candidate> {
    let fits = |need: Option<f64>| match (need, usable_gb) {
        (Some(n), Some(u)) => n <= u,
        _ => true,
    };
    let mut out: Vec<Candidate> = Vec::new();
    if !opts.models.is_empty() {
        for id in &opts.models {
            let cat = rt.catalog.iter().find(|c| &c.id == id);
            let params = cat.and_then(|c| c.params_b).or_else(|| params_from_id(id));
            out.push(Candidate {
                id: id.clone(),
                tier: cat.map(|c| c.tier).unwrap_or_else(|| tier_for(params)),
                params_b: params,
                min_memory_gb: cat.and_then(|c| c.min_memory_gb),
                present: locate_model(rt, id, ctx).present,
                tools: cat.map(|c| c.tools).unwrap_or(true),
                roles: cat.map(|c| c.roles.clone()).unwrap_or_default(),
                from: "--models",
            });
        }
        return out;
    }
    // decision models (Kev, …) answer typed questions; they're never a chat model
    for c in rt.catalog.iter().filter(|c| fits(c.min_memory_gb) && !c.roles.iter().any(|r| r == "decider")) {
        let CatalogModel { id, params_b, min_memory_gb, tier, tools, roles } = c.clone();
        let present = locate_model(rt, &id, ctx).present;
        out.push(Candidate { id, tier, params_b, min_memory_gb, present, tools, roles, from: "catalog" });
    }
    if rt.models.layout == "ollama" {
        for (id, size) in ollama_models_on_disk(rt, ctx) {
            if out.iter().any(|c| c.id == id) {
                continue;
            }
            let need = (size > 0).then(|| size as f64 * 1.25 / 1e9);
            if !fits(need) {
                continue;
            }
            let params = params_from_id(&id);
            out.push(Candidate { id, tier: tier_for(params), params_b: params, min_memory_gb: need, present: true, tools: true, roles: vec![], from: "on disk" });
        }
    }
    out
}

/// Best first: higher tier, already on disk, larger.
fn rank_default(c: &[Candidate]) -> Vec<Candidate> {
    let mut v: Vec<Candidate> = c.iter().filter(|x| x.tools && (x.roles.is_empty() || x.roles.iter().any(|r| r == "default"))).cloned().collect();
    if v.is_empty() {
        v = c.to_vec();
    }
    v.sort_by(|a, b| {
        b.tier.cmp(&a.tier).then(b.present.cmp(&a.present)).then(b.params_b.unwrap_or(0.0).partial_cmp(&a.params_b.unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal))
    });
    v
}

/// The Kev model a runtime serves for deciders: the 9B with 16 GB or more to spare,
/// else the 4B. `None` when its catalog has no decider models.
fn local_decider_pick(rt: &RuntimeProfile, usable_gb: Option<f64>) -> Option<String> {
    let deciders: Vec<&CatalogModel> = rt.catalog.iter().filter(|c| c.roles.iter().any(|r| r == "decider")).collect();
    let want = if usable_gb.is_some_and(|u| u >= 16.0) { "kev:9b" } else { "kev:4b" };
    deciders.iter().find(|c| c.id == want).or_else(|| deciders.iter().find(|c| c.id.starts_with("kev"))).map(|c| c.id.clone())
}

/// Smallest capable model for the small and decision roles.
fn pick_small(c: &[Candidate]) -> Option<Candidate> {
    let mut v: Vec<Candidate> = c.iter().filter(|x| x.tools && x.tier == 1).cloned().collect();
    v.sort_by(|a, b| {
        let ra = a.roles.iter().any(|r| r == "small" || r == "decision");
        let rb = b.roles.iter().any(|r| r == "small" || r == "decision");
        rb.cmp(&ra).then(b.present.cmp(&a.present)).then(a.params_b.unwrap_or(99.0).partial_cmp(&b.params_b.unwrap_or(99.0)).unwrap_or(std::cmp::Ordering::Equal))
    });
    v.into_iter().next()
}

fn is_local_harness(p: &HarnessProfile) -> bool {
    p.kind == "custom" || (p.open_source == Some(true) && p.local_models == Some(true))
}

pub(crate) fn subst_value(v: &Value, vars: &[(&str, &str)]) -> Value {
    let s = |x: &str| vars.iter().fold(x.to_string(), |acc, (k, val)| acc.replace(&format!("{{{k}}}"), val));
    match v {
        Value::String(x) => Value::String(s(x)),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst_value(x, vars)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (s(k), subst_value(x, vars))).collect()),
        other => other.clone(),
    }
}

/// A local runtime and the model to serve on it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalChoice {
    pub runtime: String,
    pub runtime_name: String,
    pub installed: bool,
    pub base_url: String,
    pub host: String,
    pub model: String,
    /// The weights are already on disk.
    pub present: bool,
    /// The command that downloads the weights, when they aren't.
    pub pull: Option<String>,
    pub usable_gb: Option<f64>,
}

/// Pick a local runtime (`runtime`, else the most preferred installed one, else Ollama)
/// and the best model for it that fits this machine (`model` to name one).
pub fn choose_local(reg: &Registry, ctx: &Ctx, runtime: Option<&str>, model: Option<&str>, memory_gb: Option<f64>) -> Result<LocalChoice> {
    let rt = match runtime {
        Some(r) => reg.runtime(r).ok_or_else(|| Error::Profile(format!("unknown runtime {r:?} (see hlock runtime list)")))?,
        None => DEFAULT_RUNTIMES
            .iter()
            .filter_map(|r| reg.runtime(r))
            .find(|r| probe_runtime(r, ctx).installed)
            .or_else(|| reg.runtime("ollama"))
            .ok_or_else(|| Error::Profile("no runtime profiles".into()))?,
    };
    let usable = memory_gb.or_else(|| machine_memory(ctx).map(|m| m.1));
    let opts = LocalizeOptions { models: model.map(|m| vec![m.to_string()]).unwrap_or_default(), ..Default::default() };
    let ranked = rank_default(&candidates(rt, &opts, usable, ctx));
    let top = ranked.first().ok_or_else(|| {
        Error::Profile(format!("no {} model fits {} GB; pass --model", rt.name, usable.map(|u| u.to_string()).unwrap_or_else(|| "?".into())))
    })?;
    // a catalog model (known to call tools) already on disk, else the smallest of the best tier (the least to download)
    let best = ranked.iter().find(|c| c.present && c.tier >= 2 && c.from == "catalog").unwrap_or_else(|| {
        ranked
            .iter()
            .filter(|c| c.tier == top.tier)
            .min_by(|a, b| a.min_memory_gb.or(a.params_b).unwrap_or(f64::MAX).partial_cmp(&b.min_memory_gb.or(b.params_b).unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(top)
    });
    let port = rt.ports.first().copied().unwrap_or(8080);
    Ok(LocalChoice {
        runtime: rt.id.clone(),
        runtime_name: rt.name.clone(),
        installed: probe_runtime(rt, ctx).installed,
        base_url: format!("http://localhost:{port}/v1"),
        host: format!("http://localhost:{port}"),
        model: best.id.clone(),
        present: best.present,
        pull: (!best.present).then(|| rt.pull.as_ref().map(|p| p.replace("{model}", &crate::fsutil::shell_quote(&best.id)))).flatten(),
        usable_gb: usable,
    })
}

/// Produce a localized lock. Nothing is written; `vendor` holds files to place next to `out`.
pub fn localize(source: &Lockfile, source_path: &Path, reg: &Registry, ctx: &Ctx, opts: &LocalizeOptions) -> Result<LocalizeOutcome> {
    let mut notes: Vec<String> = Vec::new();
    let mut replaced: Vec<String> = Vec::new();
    let mem = machine_memory(ctx);
    let usable = opts.memory_gb.or(mem.as_ref().map(|m| m.1));

    // ---- harness ----
    let src_profile = reg.harness(&source.harness.name);
    let (target, why) = if let Some(h) = &opts.harness {
        let p = reg.require_harness(h)?;
        if !(p.kind == "custom" || p.local_models == Some(true)) {
            return Err(Error::Profile(format!("{} can't use a local model endpoint", p.name)));
        }
        if p.open_source == Some(false) {
            notes.push(format!("{} is proprietary; it runs locally but isn't open source", p.name));
        }
        (p, "--harness".to_string())
    } else if let Some(p) = src_profile.filter(|p| is_local_harness(p)) {
        (p, "already local-capable".to_string())
    } else {
        let options: Vec<&HarnessProfile> = DEFAULT_HARNESSES.iter().filter_map(|h| reg.harness(h)).filter(|p| is_local_harness(p)).collect();
        let installed = options.iter().find(|p| probe_harness(p, ctx).installed).copied();
        match installed {
            Some(p) => (p, "the most preferred local-capable harness installed".to_string()),
            None => {
                let p = options.first().copied().ok_or_else(|| Error::Profile("no local-capable harness profile".into()))?;
                notes.push(format!("{} isn't installed; `hlock sync --install` installs it", p.name));
                (p, "the default local-capable harness".to_string())
            }
        }
    };
    let harness_changed = src_profile.map(|p| p.id != target.id).unwrap_or(true);
    if harness_changed {
        replaced.push(format!("harness {} → {}", source.harness.name, target.id));
    }

    // ---- runtime ----
    let runtime = match &opts.runtime {
        Some(r) => reg.runtime(r).ok_or_else(|| Error::Profile(format!("unknown runtime {r:?} (see hlock runtime list)")))?,
        None => DEFAULT_RUNTIMES
            .iter()
            .filter_map(|r| reg.runtime(r))
            .find(|r| probe_runtime(r, ctx).installed)
            .or_else(|| reg.runtime("ollama"))
            .ok_or_else(|| Error::Profile("no runtime profiles".into()))?,
    };
    let rprobe = probe_runtime(runtime, ctx);
    if !rprobe.installed {
        notes.push(format!("{} isn't installed; `hlock sync` will show how to install it", runtime.name));
    }
    let port = runtime.ports.first().copied().unwrap_or(8080);
    let base_url = format!("http://localhost:{port}/v1");

    // ---- models ----
    let cands = candidates(runtime, opts, usable, ctx);
    let ranked = rank_default(&cands);
    if ranked.is_empty() {
        return Err(Error::Profile(format!(
            "no {} model fits {} GB; pass --models or --memory-gb",
            runtime.name,
            usable.map(|u| u.to_string()).unwrap_or_else(|| "?".into())
        )));
    }
    let small = pick_small(&cands);

    // which source roles need replacing
    let is_local_llm = |l: &LLM| -> bool {
        match l.runtime.as_deref().and_then(|r| source.runtime.iter().find(|x| x.name == r)) {
            Some(rt) => rt.location == "local" || (opts.allow_network && rt.location == "network"),
            None => l.provider == "reference",
        }
    };

    // ---- evaluate or rank ----
    let mut default_pick = ranked[0].clone();
    let mut evaluated = false;
    let mut evidence = Evidence::default();
    let mut eval_harness = target.id.clone();
    let tasks = opts.manifest.as_ref().filter(|m| !m.eval_packs().is_empty());
    if let Some(m) = tasks {
        let mut man = m.clone();
        man.target.clear();
        let top: Vec<String> = ranked.iter().take(4).map(|c| format!("{}:{}", runtime.id, c.id)).collect();
        man.candidates.llm = BTreeMap::from([("planner".to_string(), LLMCandidate { any_of: top })]);
        man.candidates.harness = vec![HarnessCandidate { name: target.id.clone(), ..Default::default() }];
        let mut resolver = crate::Resolver::new(man, &opts.workdir);
        let lock = resolver.resolve_lock()?;
        if let Some(win) = lock.llm.first() {
            if let Some(c) = ranked.iter().find(|c| c.id == win.model) {
                default_pick = c.clone();
            }
        }
        evaluated = true;
        evidence = lock.evidence.clone();
        eval_harness = lock.harness.adapter.clone().unwrap_or_else(|| target.id.clone());
        if eval_harness != target.id {
            notes.push(format!("trials ran on the {eval_harness} adapter, so the evidence is a stand-in"));
        }
    } else {
        notes.push("no eval packs to evaluate with; candidates were ranked by fit, tier, and what's on disk".into());
    }

    // ---- build the lock ----
    let mut lock = source.clone();
    lock.kind = Some("localized".into());
    lock.base_lock = Some(source_path.to_string_lossy().into_owned());
    lock.stack.insert("selection".into(), json!("localize"));
    lock.stack.insert("harness".into(), json!(target.id));
    lock.provenance.backend = "localize".into();
    let hp = probe_harness(target, ctx);
    if harness_changed {
        lock.harness = Harness { name: target.id.clone(), version: hp.version.clone(), source: hp.source.clone(), pattern: source.harness.pattern.clone(), ..Default::default() };
    }

    let mut used_models: BTreeSet<String> = BTreeSet::new();
    let mut new_llms = Vec::new();
    for l in &source.llm {
        if is_local_llm(l) {
            new_llms.push(l.clone());
            continue;
        }
        let pick = if l.role == "small" { small.clone().unwrap_or_else(|| default_pick.clone()) } else { default_pick.clone() };
        replaced.push(format!("llm {}: {}:{} → {}:{}", l.role, l.provider, l.model, runtime.id, pick.id));
        used_models.insert(pick.id.clone());
        new_llms.push(LLM {
            role: l.role.clone(),
            provider: runtime.id.clone(),
            model: pick.id.clone(),
            resolved_from: Some(format!("localize:{}:{}", l.provider, l.model)),
            params: l.params.clone(),
            endpoint: Some(base_url.clone()),
            runtime: Some(runtime.id.clone()),
            ..Default::default()
        });
    }
    if new_llms.is_empty() {
        used_models.insert(default_pick.id.clone());
        new_llms.push(LLM { role: "default".into(), provider: runtime.id.clone(), model: default_pick.id.clone(), endpoint: Some(base_url.clone()), runtime: Some(runtime.id.clone()), ..Default::default() });
    }
    lock.llm = new_llms;

    // decision model on an API provider → the small local model
    if source.decision.kind.starts_with("llm") {
        let api = source.decision.model.as_deref().map(|m| !m.starts_with(&runtime.id)).unwrap_or(true);
        if api {
            if let Some(s) = &small {
                replaced.push(format!("decision {} → llm:{}:{}", source.decision.model.as_deref().unwrap_or(&source.decision.kind), runtime.id, s.id));
                lock.decision.kind = "llm".into();
                lock.decision.model = Some(format!("{}:{}", runtime.id, s.id));
                used_models.insert(s.id.clone());
            }
        }
    }

    // deciders on an API provider → Kev on the local runtime, when it serves Kev
    if let Some(kev) = local_decider_pick(runtime, usable) {
        for d in lock.decider.iter_mut().filter(|d| d.runtime.is_none()) {
            replaced.push(format!("decider {} {}:{} → {}:{} (scores may move; re-resolve to measure)", d.use_, d.provider, d.model, runtime.id, kev));
            d.provider = runtime.id.clone();
            d.model = kev.clone();
            d.protocol = "systemone".into();
            d.endpoint = None;
            d.key_env = None;
            d.runtime = Some(runtime.id.clone());
        }
    }

    // runtimes: keep local ones still in use, add the chosen one
    let still_used: BTreeSet<String> = lock.llm.iter().filter_map(|l| l.runtime.clone()).collect();
    lock.runtime.retain(|r| still_used.contains(&r.name) && r.name != runtime.id);
    if !used_models.is_empty() {
        lock.runtime.push(Runtime {
            name: runtime.id.clone(),
            location: "local".into(),
            version: rprobe.version.clone(),
            source: rprobe.source.clone(),
            endpoint: Some(base_url.clone()),
            model: used_models.iter().map(|m| locate_model(runtime, m, ctx)).collect(),
        });
    }

    // MCP: remote servers are network dependencies
    let remote: Vec<String> = lock.tool.iter().filter(|t| t.kind == "mcp" && t.url.is_some()).map(|t| t.name.clone()).collect();
    if !remote.is_empty() {
        if opts.strict {
            lock.tool.retain(|t| !remote.contains(&t.name));
            replaced.push(format!("dropped remote MCP servers: {}", remote.join(", ")));
        } else {
            notes.push(format!("remote MCP servers still need the network: {} (--strict drops them)", remote.join(", ")));
        }
    }

    // harness-specific parts that don't carry over
    let mut vendor = Vec::new();
    if harness_changed {
        let plugins: Vec<String> = lock.native.iter().filter(|n| n.kind != "package").map(|n| n.name.clone()).collect();
        if !plugins.is_empty() {
            let shown: Vec<&str> = plugins.iter().take(4).map(String::as_str).collect();
            let more = if plugins.len() > 4 { format!(" and {} more", plugins.len() - 4) } else { String::new() };
            notes.push(format!("{}'s {} plugins don't carry over: {}{more}", source.harness.name, plugins.len(), shown.join(", ")));
        }
        lock.native.retain(|n| n.kind == "package");
        lock.config.clear();
        lock.skill.retain(|s| !matches!(s.origin.as_str(), "harness-builtin" | "bundled"));
        // each user-level file goes to its own file in the target, so two never share a path
        let user: Vec<String> = lock.instructions.iter().filter(|i| i.scope == "user").map(|i| i.name.clone()).collect();
        let mut dests = crate::launch::user_instruction_targets(target, &user).into_iter();
        let mut dropped = Vec::new();
        let has_dest = target.instructions.iter().any(|i| i.starts_with("{root}/"));
        lock.instructions.retain_mut(|i| {
            if i.scope != "user" || !has_dest {
                return true;
            }
            match dests.next().flatten() {
                Some(tpl) => {
                    i.name = tpl.to_string();
                    true
                }
                None => {
                    dropped.push(i.name.clone());
                    false
                }
            }
        });
        if !dropped.is_empty() {
            notes.push(format!("{} has no place for user instructions {}", target.name, dropped.join(", ")));
        }
    }

    // configure the local endpoint (and, for a new harness, its MCP servers)
    let default_model = lock.llm.iter().find(|l| l.role == "default").or_else(|| lock.llm.first()).map(|l| l.model.clone()).unwrap_or_default();
    let host = format!("http://localhost:{port}");
    let vars = [("provider", runtime.id.as_str()), ("model", default_model.as_str()), ("base_url", base_url.as_str()), ("host", host.as_str())];
    let mut per_file: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let local_provider: Vec<_> = target.local_provider.iter().filter(|lp| lp.applies_to(Some(&runtime.id))).collect();
    if target.kind == "cli" && !local_provider.is_empty() {
        for lp in local_provider {
            let entry = per_file.entry(lp.file.clone()).or_default();
            for (ptr, v) in &lp.set {
                let ptr = subst_value(&json!(ptr), &vars).as_str().unwrap_or_default().to_string();
                entry.insert(ptr, subst_value(v, &vars));
            }
        }
    } else if target.kind == "cli" {
        notes.push(format!("the {} profile doesn't say how to configure a local model; point it at {base_url} yourself", target.name));
    }
    if harness_changed {
        if let Some((file, ptr)) = target.mcp.write.as_deref().and_then(|w| w.split_once('#')) {
            let shape = target.mcp.shape.as_deref().unwrap_or("standard");
            let servers: Map<String, Value> = lock.tool.iter().filter(|t| t.kind == "mcp").map(|t| (t.name.clone(), mcp_entry(t, shape))).collect();
            if !servers.is_empty() {
                per_file.entry(file.to_string()).or_default().insert(ptr.to_string(), Value::Object(servers));
            }
        }
    }
    let vroot = format!("{VENDOR_DIR}/{}/localize", target.id);
    let source_dir = match source_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => ctx.cwd.join(p),
        None => ctx.cwd.clone(),
    };
    for (file, mut sets) in per_file {
        let Some(spec) = target.files.get(&file) else {
            notes.push(format!("the {} profile has no file {file:?}", target.id));
            continue;
        };
        // the source lock's entry for the same file (the harness didn't change, or this
        // lock was localized before) is merged into, never duplicated
        if let Some(i) = lock.config.iter().position(|c| c.id == file) {
            let prev = lock.config.remove(i);
            let fmt = if prev.restore == "pointers" || prev.format == "jsonc" { "json" } else { prev.format.as_str() };
            let doc = prev.vendored.as_deref().and_then(|v| read_vendored(&source_dir, v)).and_then(|t| crate::capture::parse_config(&t, fmt).ok());
            match (prev.restore.as_str(), doc) {
                // earlier pointers stay, under ours
                ("pointers", Some(Value::Object(m))) => {
                    for (p, v) in m {
                        sets.entry(p).or_insert(v);
                    }
                }
                // a whole file: ours are set inside it
                ("record", _) => {}
                (_, Some(mut doc)) => {
                    for (p, v) in &sets {
                        set_pointer(&mut doc, p, v.clone());
                    }
                    let rel = format!("{vroot}/{file}.{fmt}");
                    vendor.push(VendorFile { rel: rel.clone(), bytes: render_config(&doc, fmt).map_err(Error::Profile)?.into_bytes(), executable: false });
                    lock.config.insert(i, ConfigFile { digest: Some(crate::hash::content_hash(&doc)), vendored: Some(rel), ..prev });
                    continue;
                }
                (_, None) => notes.push(format!("can't read the source lock's vendored {file}; only the local model settings are restored")),
            }
        }
        let body = Value::Object(sets.clone());
        let rel = format!("{vroot}/{file}.pointers.json");
        vendor.push(VendorFile { rel: rel.clone(), bytes: render_config(&body, "json").map_err(Error::Profile)?.into_bytes(), executable: false });
        let mut check = json!({});
        for (p, v) in &sets {
            set_pointer(&mut check, p, v.clone());
        }
        lock.config.push(ConfigFile {
            id: file.clone(),
            path: spec.path.clone(),
            format: spec.format.clone(),
            restore: "pointers".into(),
            pointers: sets.keys().cloned().collect(),
            digest: Some(crate::hash::content_hash(&check)),
            vendored: Some(rel),
        });
    }

    lock.evidence = evidence;
    let report = json!({
        "source": source_path,
        "evaluated": evaluated,
        "evaluated-with": evaluated.then_some(eval_harness),
        "memory": mem.as_ref().map(|(total, use_, how)| json!({"total-gb": total, "usable-gb": opts.memory_gb.unwrap_or(*use_), "how": how})),
        "harness": {"from": source.harness.name, "to": target.id, "why": why, "installed": hp.installed},
        "runtime": {"name": runtime.id, "installed": rprobe.installed, "version": rprobe.version, "endpoint": base_url},
        "models": {
            "default": default_pick.id,
            "small": small.as_ref().map(|s| s.id.clone()),
            "candidates": ranked.iter().chain(cands.iter().filter(|c| !ranked.iter().any(|r| r.id == c.id))).collect::<Vec<_>>(),
        },
        "replaced": replaced,
        "notes": notes,
        "baseline": {
            "success-rate": source.evidence.success_rate.get("mean"),
            "cost-usd-per-task": source.evidence.cost_usd_per_task.get("mean"),
            "trials": source.evidence.trials,
        },
    });
    let report = drop_nulls(&report); // TOML has no null
    lock.localize = Some(report.clone());
    Ok(LocalizeOutcome { lock, vendor, report })
}

/// A vendored file of a lock in `dir`, when its path is inside `dir`.
fn read_vendored(dir: &Path, rel: &str) -> Option<String> {
    let p = dir.join(rel);
    (crate::fsutil::safe_rel(rel) && crate::fsutil::within(dir, &p)).then(|| crate::fsutil::read_regular_text(&p, 20 << 20).ok()).flatten()
}

/// Localize and write the lock to `out`, with its vendored files beside it.
pub fn localize_to(source_path: &Path, out: &Path, reg: &Registry, ctx: &Ctx, opts: &LocalizeOptions) -> Result<LocalizeOutcome> {
    let source = Lockfile::load(source_path)?;
    let mut o = localize(&source, source_path, reg, ctx, opts)?;
    // record the source relative to the new lock when they share a directory
    if source_path.parent() == out.parent() {
        if let Some(name) = source_path.file_name() {
            o.lock.base_lock = Some(name.to_string_lossy().into_owned());
        }
    }
    let base = out.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| ctx.cwd.clone());
    let _ = std::fs::remove_dir_all(base.join(VENDOR_DIR).join(&o.lock.harness.name).join("localize"));
    for f in &o.vendor {
        let dest = base.join(&f.rel);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&dest, &f.bytes)?;
    }
    o.lock.save(out)?;
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parsing() {
        assert_eq!(params_from_id("qwen3:8b"), Some(8.0));
        assert_eq!(params_from_id("qwen3-coder:30b"), Some(30.0));
        assert_eq!(params_from_id("Qwen3-30B-A3B-Instruct"), Some(30.0));
        assert_eq!(params_from_id("qwen3:1.7b"), Some(1.7));
        assert_eq!(params_from_id("llama3.2"), None);
        assert_eq!(tier_for(Some(30.0)), 3);
        assert_eq!(tier_for(Some(20.0)), 2);
        assert_eq!(tier_for(None), 1);
    }

    #[test]
    fn templates() {
        let v = json!({"/provider/{provider}": {"options": {"baseURL": "{base_url}"}, "models": {"{model}": {}}}});
        let out = subst_value(&v, &[("provider", "ollama"), ("model", "qwen3:8b"), ("base_url", "http://localhost:11434/v1")]);
        assert_eq!(out, json!({"/provider/ollama": {"options": {"baseURL": "http://localhost:11434/v1"}, "models": {"qwen3:8b": {}}}}));
    }

    #[test]
    fn relocalizing_merges_into_the_config_entry_it_already_has() {
        let d = std::env::temp_dir().join(format!("hlock-localize-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("proj/.hlock/vendor/opencode/config")).unwrap();
        let ctx = Ctx { home: d.join("home"), cwd: d.join("proj"), env: BTreeMap::new(), path: vec![] };
        std::fs::write(ctx.cwd.join(".hlock/vendor/opencode/config/config.json"), "{\"theme\": \"dark\"}").unwrap();
        let source = Lockfile {
            harness: Harness { name: "opencode".into(), ..Default::default() },
            config: vec![ConfigFile { id: "config".into(), path: "{root}/opencode.json".into(), format: "jsonc".into(), restore: "whole".into(), vendored: Some(".hlock/vendor/opencode/config/config.json".into()), ..Default::default() }],
            ..Default::default()
        };
        let reg = Registry::builtin();
        let opts = LocalizeOptions { runtime: Some("ollama".into()), memory_gb: Some(64.0), workdir: d.join("work"), ..Default::default() };
        let src = ctx.cwd.join("harness.lock");
        source.save(&src).unwrap();
        let out = ctx.cwd.join("local.lock");
        let first = localize_to(&src, &out, &reg, &ctx, &opts).unwrap();
        let entries: Vec<&ConfigFile> = first.lock.config.iter().filter(|c| c.id == "config").collect();
        assert_eq!(entries.len(), 1, "{:?}", first.lock.config);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(ctx.cwd.join(entries[0].vendored.as_ref().unwrap())).unwrap()).unwrap();
        assert_eq!(doc["theme"], "dark", "the file's own settings stay");
        assert!(doc["model"].as_str().unwrap().starts_with("ollama/"), "{doc}");
        // again, from the localized lock: still one entry
        let again = localize_to(&out, &ctx.cwd.join("local2.lock"), &reg, &ctx, &opts).unwrap();
        assert_eq!(again.lock.config.iter().filter(|c| c.id == "config").count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ranking() {
        let c = |id: &str, tier: u8, present: bool, params: f64| Candidate { id: id.into(), tier, params_b: Some(params), min_memory_gb: None, present, tools: true, roles: vec![], from: "catalog" };
        let cands = vec![c("a", 2, false, 20.0), c("b", 3, false, 30.0), c("c", 3, true, 24.0), c("tiny", 1, false, 1.7), c("small", 1, true, 4.0)];
        assert_eq!(rank_default(&cands)[0].id, "c"); // top tier, already on disk
        assert_eq!(pick_small(&cands).unwrap().id, "small"); // on disk beats smaller
        let r = Registry::builtin();
        let ollama = r.runtime("ollama").unwrap();
        assert!(ollama.catalog.iter().any(|m| m.tier == 3) && ollama.catalog.iter().any(|m| m.roles.iter().any(|x| x == "decision")));
        assert!(r.harness("opencode").unwrap().local_provider.len() == 1 && r.harness("pi").unwrap().local_provider.len() == 2);
        assert!(is_local_harness(r.harness("opencode").unwrap()) && !is_local_harness(r.harness("claude-code").unwrap()));
    }
}
