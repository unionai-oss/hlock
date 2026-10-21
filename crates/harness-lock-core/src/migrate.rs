//! `hlock migrate`: a lock moved onto another harness (specs/migrate.md).
//!
//! The lock's portable parts are rewritten for the target: skills and instruction
//! files go where it reads them (vendored when they move, so `sync` has a copy to put
//! there), MCP servers into its config shape, deciders onto its hooks. Models the target
//! can't run move to a provider it can. Plugins and settings don't carry, and are listed.
//! `localize` is the special case that also moves the models onto a local runtime; it
//! shares [`carry_over`] and [`write_settings`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::capture::{drop_nulls, render_config, tree_digest, VendorFile, VENDOR_DIR};
use crate::error::{Error, Result};
use crate::launch::mcp_entry;
use crate::lockfile::{ConfigFile, Evidence, Harness, Instructions, Lockfile, LLM};
use crate::localize::subst_value;
use crate::profiles::{probe_harness, Ctx, HarnessProfile, Registry};
use crate::sync::set_pointer;

#[derive(Debug, Clone, Default)]
pub struct MigrateOptions {
    /// The harness to move to (id or alias).
    pub to: String,
    /// `provider:model` for the default role.
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MigrateOutcome {
    pub lock: Lockfile,
    pub vendor: Vec<VendorFile>,
    pub report: Value,
}

/// What carrying a lock onto another harness changed, and the files to vendor.
#[derive(Debug, Default)]
pub(crate) struct Carry {
    pub replaced: Vec<String>,
    pub notes: Vec<String>,
    pub vendor: Vec<VendorFile>,
}

/// Where a lock's parts live: under a harness's config root or home (`{root}/…`, `~/…`),
/// or in the project.
fn user_level(path: &str) -> bool {
    path.starts_with("{root}") || path.starts_with('~') || Path::new(path).is_absolute()
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Where a skill at `path` goes in `target`: unchanged when the target reads that
/// directory, else its project skill directory (`.agents/skills` first) for a project
/// skill, else its user skill directory. `None`: the target has nowhere for it.
pub(crate) fn skill_dest(target: &HarnessProfile, name: &str, path: &str) -> Option<String> {
    if target.skills.dirs.iter().chain(target.skills.write.iter()).any(|d| d == parent_of(path)) {
        return Some(path.to_string());
    }
    let project: Vec<&String> = target.skills.dirs.iter().filter(|d| !user_level(d)).collect();
    let dir = if user_level(path) {
        target.skills.write.as_ref()
    } else {
        project.iter().find(|d| d.as_str() == ".agents/skills").or(project.first()).copied().or(target.skills.write.as_ref())
    };
    dir.map(|d| format!("{d}/{name}"))
}

/// The project instruction file `name` becomes in `target`: itself when the target reads
/// it, else the target's first project instruction file.
pub(crate) fn instructions_dest(target: &HarnessProfile, name: &str) -> Option<String> {
    let project: Vec<&String> = target.instructions.iter().filter(|i| !user_level(i)).collect();
    if project.iter().any(|i| i.as_str() == name) {
        return Some(name.to_string());
    }
    project.first().map(|i| i.to_string())
}

/// Whether `target` talks to `provider` natively.
pub fn supports_provider(target: &HarnessProfile, provider: &str) -> bool {
    target.api_keys.iter().any(|k| k.provider == provider) || target.model.default_provider.as_deref() == Some(provider)
}

/// The provider and model a role moves to when `target` can't run it: the target's
/// default provider's default model, else the first of its providers hlock has a default
/// for, else its own model (`harness-default`).
pub fn target_model(target: &HarnessProfile) -> Option<(String, String)> {
    let default_for = |p: &str| crate::meta::DEFAULT_MODELS.iter().find(|(x, _)| *x == p).map(|(_, m)| m.to_string());
    let providers: Vec<&str> = target.model.default_provider.iter().map(String::as_str).chain(target.api_keys.iter().map(|k| k.provider.as_str())).collect();
    providers
        .iter()
        .find_map(|p| default_for(p).map(|m| (p.to_string(), m)))
        .or_else(|| providers.first().map(|p| (p.to_string(), "harness-default".to_string())))
}

/// Carry `lock`'s harness-specific parts (copied from `source`) onto `target`: skills,
/// instructions and deciders where it reads them, plugins and settings dropped. Project
/// paths are relative to `ctx.cwd`; moved content is vendored under `vroot`.
pub(crate) fn carry_over(lock: &mut Lockfile, source: &Lockfile, src: Option<&HarnessProfile>, target: &HarnessProfile, ctx: &Ctx, vroot: &str, c: &mut Carry) {
    let src_root = src.map(|p| p.config_root(ctx));
    let old_place = |p: &str| if user_level(p) { ctx.expand(p, src_root.as_deref()) } else { ctx.cwd.join(p) };

    // plugins and settings
    let plugins: Vec<String> = lock.native.iter().filter(|n| n.kind != "package").map(|n| n.name.clone()).collect();
    if !plugins.is_empty() {
        let shown: Vec<&str> = plugins.iter().take(4).map(String::as_str).collect();
        let more = if plugins.len() > 4 { format!(" and {} more", plugins.len() - 4) } else { String::new() };
        c.notes.push(format!("{}'s {} plugins don't carry over: {}{more}", source.harness.name, plugins.len(), shown.join(", ")));
    }
    lock.native.retain(|n| n.kind == "package");
    let settings: Vec<String> = lock.config.iter().filter(|f| f.restore != "record").map(|f| f.id.clone()).collect();
    if !settings.is_empty() {
        c.notes.push(format!("{}'s settings ({}) don't carry over", src.map(|p| p.name.as_str()).unwrap_or(&source.harness.name), settings.join(", ")));
    }
    lock.config.clear();

    // skills
    let builtin = lock.skill.iter().filter(|s| matches!(s.origin.as_str(), "harness-builtin" | "bundled")).count();
    if builtin > 0 {
        c.notes.push(format!("{builtin} skills that ship with {} or its plugins don't carry over", source.harness.name));
    }
    lock.skill.retain(|s| !matches!(s.origin.as_str(), "harness-builtin" | "bundled"));
    let mut dropped = Vec::new();
    for s in lock.skill.iter_mut() {
        let Some(path) = s.path.clone() else { continue };
        let Some(dest) = skill_dest(target, &s.name, &path) else {
            dropped.push(s.name.clone());
            continue;
        };
        if dest != path {
            c.replaced.push(format!("skill {}: {path} → {dest}", s.name));
            s.scope = Some(if user_level(&dest) { "user" } else { "project" }.into());
            s.path = Some(dest.clone());
        }
        // the same template under another root is another place too
        let moved = dest != path || user_level(&path);
        let fetchable = s.vendored.is_some() || (s.dep.declared && s.dep.commit.is_some()) || s.source.as_deref().is_some_and(|x| x.starts_with("git+"));
        if !moved || fetchable {
            continue;
        }
        let from = old_place(&path);
        if !from.join("SKILL.md").is_file() {
            c.notes.push(format!("skill {}: no copy at {path} to move; sync it on {} first, or re-pin", s.name, source.harness.name));
            continue;
        }
        if s.digest.as_ref().is_some_and(|d| *d != tree_digest(&from)) {
            c.notes.push(format!("skill {}: {path} changed since it was pinned; re-pin to move it", s.name));
            continue;
        }
        let rel = format!("{vroot}/skills/{}", s.name);
        let mut warnings = Vec::new();
        match crate::capture::vendor_tree(&from, &rel, &mut c.vendor, &mut warnings) {
            Ok(()) => s.vendored = Some(rel),
            Err(e) => c.notes.push(format!("skill {}: {e}", s.name)),
        }
        c.notes.extend(warnings);
    }
    if !dropped.is_empty() {
        lock.skill.retain(|s| !dropped.contains(&s.name));
        c.notes.push(format!("{} has no skills directory; dropped {}", target.name, dropped.join(", ")));
    }

    // user instruction files: each to its own file in the target, so two never share a path
    let has_dest = target.instructions.iter().any(|i| i.starts_with("{root}/"));
    let user: Vec<String> = lock.instructions.iter().filter(|i| i.scope == "user").map(|i| i.name.clone()).collect();
    let mut dests = crate::launch::user_instruction_targets(target, &user).into_iter();
    let mut dropped = Vec::new();
    let mut moves: Vec<(usize, String)> = Vec::new();
    for (n, i) in lock.instructions.iter().enumerate() {
        if i.scope != "user" || !has_dest {
            continue;
        }
        match dests.next().flatten() {
            Some(tpl) => moves.push((n, tpl.to_string())),
            None => dropped.push(n),
        }
    }
    if !dropped.is_empty() {
        let names: Vec<&str> = dropped.iter().map(|&n| lock.instructions[n].name.as_str()).collect();
        c.notes.push(format!("{} has no place for user instructions {}", target.name, names.join(", ")));
    }

    // project instruction files: where the target reads them
    let src_rules = src.and_then(|p| p.rules.as_ref()).map(|r| r.dir.clone());
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut pinned_instead: Vec<(usize, Instructions)> = Vec::new();
    // files the target already reads claim their names first
    let order: Vec<usize> = {
        let (mut stay, mut go): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        for (n, i) in lock.instructions.iter().enumerate().filter(|(_, i)| i.scope != "user") {
            if instructions_dest(target, &i.name).as_deref() == Some(i.name.as_str()) {
                stay.push(n);
            } else {
                go.push(n);
            }
        }
        stay.into_iter().chain(go).collect()
    };
    for n in order {
        let i = &lock.instructions[n];
        let in_rules = src_rules.as_deref().is_some_and(|d| parent_of(&i.name) == d);
        let dest = if in_rules {
            target.rules.as_ref().map(|r| format!("{}/{}", r.dir, file_name(&i.name)))
        } else {
            instructions_dest(target, &i.name)
        };
        let Some(dest) = dest else {
            c.notes.push(format!("{} has no place for {}; dropped it", target.name, i.name));
            dropped.push(n);
            continue;
        };
        if !taken.insert(dest.clone()) {
            c.notes.push(format!("{} wasn't carried over: {dest} is already in the lock", i.name));
            dropped.push(n);
            continue;
        }
        if dest == i.name {
            continue;
        }
        // the project already has the file the target reads: pin that one
        let existing = ctx.cwd.join(&dest);
        if !in_rules && existing.is_file() {
            if let Ok(bytes) = std::fs::read(&existing) {
                c.replaced.push(format!("instructions {} → {dest} (the project's own)", i.name));
                let scan = crate::scan::summary(&crate::scan::scan_bytes(&bytes, &dest));
                pinned_instead.push((n, Instructions { name: dest, digest: Some(crate::hash::sha256_bytes(&bytes)), scope: "project".into(), scan: Some(scan), ..Default::default() }));
                continue;
            }
        }
        moves.push((n, dest));
    }
    for (n, new) in pinned_instead {
        lock.instructions[n] = new;
    }
    for (n, dest) in moves {
        let i = &mut lock.instructions[n];
        if i.scope != "user" {
            c.replaced.push(format!("instructions {} → {dest}", i.name));
        }
        let old = std::mem::replace(&mut i.name, dest);
        if i.vendored.is_some() || (i.dep.declared && i.dep.commit.is_some()) {
            continue;
        }
        let from = old_place(&old);
        match std::fs::read(&from) {
            Ok(bytes) if i.digest.as_ref().is_none_or(|d| *d == crate::hash::sha256_bytes(&bytes)) => {
                let rel = format!("{vroot}/instructions/{}", file_name(&i.name));
                c.vendor.push(VendorFile { rel: rel.clone(), bytes, executable: false });
                i.vendored = Some(rel);
            }
            Ok(_) => c.notes.push(format!("{old} changed since it was pinned; re-pin to move it")),
            Err(_) => c.notes.push(format!("no copy of {old} to move; sync it on {} first, or re-pin", source.harness.name)),
        }
    }
    let mut n = 0;
    lock.instructions.retain(|_| {
        n += 1;
        !dropped.contains(&(n - 1))
    });

    // deciders: onto the target's hooks
    let mut unwired = Vec::new();
    lock.decider.retain_mut(|d| match crate::decide::wiring(&d.use_, &target.id, Some(target)) {
        Some((via, wiring)) => {
            d.via = via;
            d.wiring = wiring;
            true
        }
        None => {
            unwired.push(d.use_.clone());
            false
        }
    });
    if !unwired.is_empty() {
        c.notes.push(format!("{} can't wire the {} decider{}; dropped", target.name, unwired.join(", "), if unwired.len() == 1 { "" } else { "s" }));
    }
}

/// The MCP servers of `lock` in `target`'s shape, under the file its servers are written to.
pub(crate) fn mcp_settings(lock: &Lockfile, target: &HarnessProfile, per_file: &mut BTreeMap<String, Map<String, Value>>) {
    if let Some((file, ptr)) = target.mcp.write.as_deref().and_then(|w| w.split_once('#')) {
        let shape = target.mcp.shape.as_deref().unwrap_or("standard");
        let servers: Map<String, Value> = lock.tool.iter().filter(|t| t.kind == "mcp").map(|t| (t.name.clone(), mcp_entry(t, shape))).collect();
        if !servers.is_empty() {
            per_file.entry(file.to_string()).or_default().insert(ptr.to_string(), Value::Object(servers));
        }
    }
}

/// The settings that point `target` at an API model `llm` (`[[api-provider]]`, else its
/// first config-file model source). False when the profile says no way to set it.
fn api_model_settings(target: &HarnessProfile, llm: &LLM, per_file: &mut BTreeMap<String, Map<String, Value>>) -> bool {
    let vars = [("provider", llm.provider.as_str()), ("model", llm.model.as_str())];
    if !target.api_provider.is_empty() {
        for ap in &target.api_provider {
            let entry = per_file.entry(ap.file.clone()).or_default();
            for (ptr, v) in &ap.set {
                entry.insert(subst_value(&json!(ptr), &vars).as_str().unwrap_or_default().to_string(), subst_value(v, &vars));
            }
        }
        return true;
    }
    let Some((file, ptr)) = target.model.default.iter().find_map(|s| s.split_once('#')) else { return false };
    let value = if target.model.format.as_deref() == Some("provider/model") { format!("{}/{}", llm.provider, llm.model) } else { llm.model.clone() };
    per_file.entry(file.to_string()).or_default().insert(ptr.to_string(), json!(value));
    true
}

/// The settings that point `target` at a model on a local runtime (`[[local-provider]]`).
pub(crate) fn local_model_settings(target: &HarnessProfile, runtime: &str, model: &str, base_url: &str, per_file: &mut BTreeMap<String, Map<String, Value>>) -> bool {
    let host = base_url.trim_end_matches('/').trim_end_matches("/v1").to_string();
    let vars = [("provider", runtime), ("model", model), ("base_url", base_url), ("host", host.as_str())];
    let lps: Vec<_> = target.local_provider.iter().filter(|lp| lp.applies_to(Some(runtime))).collect();
    for lp in &lps {
        let entry = per_file.entry(lp.file.clone()).or_default();
        for (ptr, v) in &lp.set {
            entry.insert(subst_value(&json!(ptr), &vars).as_str().unwrap_or_default().to_string(), subst_value(v, &vars));
        }
    }
    !lps.is_empty()
}

/// Write `per_file` (pointer → value, per profile file) into `lock` as config entries,
/// vendoring each under `vroot`. An entry the lock already has for a file is merged into,
/// never duplicated. `source_dir` is where the lock's existing vendored files are.
pub(crate) fn write_settings(lock: &mut Lockfile, target: &HarnessProfile, per_file: BTreeMap<String, Map<String, Value>>, source_dir: &Path, vroot: &str, notes: &mut Vec<String>, vendor: &mut Vec<VendorFile>) -> Result<()> {
    for (file, mut sets) in per_file {
        let Some(spec) = target.files.get(&file) else {
            notes.push(format!("the {} profile has no file {file:?}", target.id));
            continue;
        };
        if let Some(i) = lock.config.iter().position(|c| c.id == file) {
            let prev = lock.config.remove(i);
            let fmt = if prev.restore == "pointers" || prev.format == "jsonc" { "json" } else { prev.format.as_str() };
            let doc = prev.vendored.as_deref().and_then(|v| read_vendored(source_dir, v)).and_then(|t| crate::capture::parse_config(&t, fmt).ok());
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
                (_, None) => notes.push(format!("can't read the source lock's vendored {file}; only the new settings are restored")),
            }
        }
        let body = Value::Object(sets.clone());
        let rel = format!("{vroot}/{file}.pointers.json");
        vendor.push(VendorFile { rel: rel.clone(), bytes: render_config(&body, "json").map_err(Error::Profile)?.into_bytes(), executable: false });
        lock.config.push(ConfigFile {
            id: file.clone(),
            path: spec.path.clone(),
            format: spec.format.clone(),
            restore: "pointers".into(),
            pointers: sets.keys().cloned().collect(),
            // what's vendored, as pin digests a pointer-restored file
            digest: Some(crate::hash::content_hash(&body)),
            vendored: Some(rel),
            scan: None,
        });
    }
    Ok(())
}

/// A vendored file of a lock in `dir`, when its path is inside `dir`.
fn read_vendored(dir: &Path, rel: &str) -> Option<String> {
    let p = dir.join(rel);
    (crate::fsutil::safe_rel(rel) && crate::fsutil::within(dir, &p)).then(|| crate::fsutil::read_regular_text(&p, 20 << 20).ok()).flatten()
}

/// The directory a lock at `path` is in.
pub(crate) fn dir_of(path: &Path, ctx: &Ctx) -> PathBuf {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => ctx.cwd.join(p),
        None => ctx.cwd.clone(),
    }
}

/// Write `lock` to `out` with its vendored files beside it. What this run vendored under
/// `sub` (`<harness>/localize`) goes in a directory of `out`'s own (`sub/<out stem>`),
/// replacing what an earlier run for `out` left, so two locks never share a file.
/// `base-lock` names the source relative to `out` when they share a directory.
pub(crate) fn save_to(lock: &mut Lockfile, vendor: &[VendorFile], source_path: &Path, out: &Path, ctx: &Ctx, sub: &str) -> Result<()> {
    if source_path.parent() == out.parent() {
        if let Some(name) = source_path.file_name() {
            lock.base_lock = Some(name.to_string_lossy().into_owned());
        }
    }
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "lock".into());
    let (from, to) = (format!("{VENDOR_DIR}/{sub}"), format!("{VENDOR_DIR}/{sub}/{stem}"));
    let ours = |x: &str| vendor.iter().any(|f| f.rel == x || f.rel.starts_with(&format!("{x}/")));
    let moved = |x: &str| x.strip_prefix(&format!("{from}/")).map(|rest| format!("{to}/{rest}"));
    let fix = |v: &mut Option<String>| {
        if let Some(new) = v.as_deref().filter(|x| ours(x)).and_then(moved) {
            *v = Some(new);
        }
    };
    lock.skill.iter_mut().for_each(|s| fix(&mut s.vendored));
    lock.instructions.iter_mut().for_each(|i| fix(&mut i.vendored));
    lock.config.iter_mut().for_each(|c| fix(&mut c.vendored));
    let base = dir_of(out, ctx);
    let _ = std::fs::remove_dir_all(base.join(&to));
    for f in vendor {
        let dest = base.join(moved(&f.rel).unwrap_or_else(|| f.rel.clone()));
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&dest, &f.bytes)?;
        #[cfg(unix)]
        if f.executable {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
        }
    }
    lock.save(out)?;
    Ok(())
}

/// Move a lock onto another harness. Nothing is written; `vendor` holds files to place
/// next to the new lock.
pub fn migrate(source: &Lockfile, source_path: &Path, reg: &Registry, ctx: &Ctx, opts: &MigrateOptions) -> Result<MigrateOutcome> {
    let target = reg.require_harness(&opts.to)?;
    if target.kind != "cli" {
        return Err(Error::Profile(format!("{} is a {} harness; migrate moves a lock between agent CLIs", target.name, target.kind)));
    }
    let src = reg.harness(&source.harness.name);
    if src.is_some_and(|p| p.id == target.id) {
        return Err(Error::Profile(format!("the lock is already on {}; `hlock localize` moves it to local models", target.name)));
    }
    let mut c = Carry::default();
    c.replaced.push(format!("harness {} → {}", source.harness.name, target.id));
    let vroot = format!("{VENDOR_DIR}/{}/migrate", target.id);
    let source_dir = dir_of(source_path, ctx);

    let mut lock = source.clone();
    lock.kind = Some("migrated".into());
    lock.base_lock = Some(source_path.to_string_lossy().into_owned());
    lock.stack.insert("selection".into(), json!("migrate"));
    lock.stack.insert("harness".into(), json!(target.id));
    lock.provenance.backend = "migrate".into();
    let hp = probe_harness(target, ctx);
    lock.harness = Harness { name: target.id.clone(), version: hp.version.clone(), source: hp.source.clone(), pattern: source.harness.pattern.clone(), ..Default::default() };
    if !hp.installed {
        c.notes.push(format!("{} isn't installed; `hlock sync --install` installs it", target.name));
    }
    carry_over(&mut lock, source, src, target, ctx, &vroot, &mut c);

    // ---- models ----
    let wanted = match &opts.model {
        Some(m) => {
            let (p, name) = m.split_once(':').filter(|(p, n)| !p.is_empty() && !n.is_empty()).ok_or_else(|| Error::Profile(format!("--model {m:?} isn't written provider:model")))?;
            if !supports_provider(target, p) {
                let known: Vec<&str> = target.model.default_provider.iter().map(String::as_str).chain(target.api_keys.iter().map(|k| k.provider.as_str())).collect();
                return Err(Error::Profile(format!("{} doesn't talk to {p:?} (it supports: {})", target.name, if known.is_empty() { "none".into() } else { known.join(", ") })));
            }
            Some((p.to_string(), name.to_string()))
        }
        None => None,
    };
    let can_local = target.local_models == Some(true) && !target.local_provider.is_empty();
    let is_local = |l: &LLM| l.runtime.as_deref().and_then(|r| source.runtime.iter().find(|x| x.name == r)).is_some();
    if lock.llm.is_empty() {
        if let Some((provider, model)) = &wanted {
            lock.llm.push(LLM { role: "default".into(), provider: provider.clone(), model: model.clone(), ..Default::default() });
        }
    }
    let default_idx = lock.llm.iter().position(|l| l.role == "default").or(if lock.llm.is_empty() { None } else { Some(0) });
    let fallback = target_model(target);
    for (n, l) in lock.llm.iter_mut().enumerate() {
        let from = format!("{}:{}", l.provider, l.model);
        // why the role can't stay as it is
        let why = if matches!(l.provider.as_str(), "reference" | "unknown") || l.model == "harness-default" {
            None
        } else if is_local(l) {
            (!can_local).then(|| format!("{} can't use a local model endpoint", target.name))
        } else {
            (!supports_provider(target, &l.provider)).then(|| format!("{} doesn't talk to {}; --model picks another", target.name, l.provider))
        };
        let (to, why) = match (&wanted, why) {
            (Some(w), _) if Some(n) == default_idx => (w.clone(), "--model".to_string()),
            (_, None) => continue,
            (_, Some(why)) => match &fallback {
                Some(f) => (f.clone(), why),
                None => return Err(Error::Profile(format!("{} talks to no API provider hlock knows; `hlock migrate --local` moves the models to a local runtime", target.name))),
            },
        };
        if (to.0.as_str(), to.1.as_str()) == (l.provider.as_str(), l.model.as_str()) {
            continue;
        }
        c.replaced.push(format!("llm {}: {from} → {}:{} ({why})", l.role, to.0, to.1));
        *l = LLM { role: l.role.clone(), provider: to.0, model: to.1, resolved_from: Some(format!("migrate:{from}")), params: l.params.clone(), ..Default::default() };
    }
    // local runtimes stay while something still uses them
    let used: BTreeSet<String> = lock.llm.iter().filter_map(|l| l.runtime.clone()).chain(lock.decider.iter().filter_map(|d| d.runtime.clone())).collect();
    lock.runtime.retain(|r| used.contains(&r.name));

    // ---- settings: the model and the MCP servers ----
    let mut per_file: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    if let Some(l) = default_idx.and_then(|n| lock.llm.get(n)) {
        let set = if let Some(rt) = l.runtime.as_deref() {
            let base_url = l.endpoint.clone().or_else(|| lock.runtime.iter().find(|r| r.name == rt).and_then(|r| r.endpoint.clone())).unwrap_or_default();
            local_model_settings(target, rt, &l.model, &base_url, &mut per_file)
        } else if matches!(l.provider.as_str(), "reference" | "unknown") || l.model == "harness-default" {
            true
        } else {
            api_model_settings(target, l, &mut per_file)
        };
        if !set {
            c.notes.push(format!("the {} profile doesn't say how to set its model; set {}:{} yourself", target.name, l.provider, l.model));
        }
    }
    mcp_settings(&lock, target, &mut per_file);
    write_settings(&mut lock, target, per_file, &source_dir, &vroot, &mut c.notes, &mut c.vendor)?;

    // the evidence measured the old harness
    lock.evidence = Evidence::default();
    if source.evidence.trials > 0 {
        c.notes.push(format!("the source lock's evidence was measured on {}; `hlock resolve` measures {}", source.harness.name, target.name));
    }
    let report = json!({
        "source": source_path,
        "harness": {"from": source.harness.name, "to": target.id, "name": target.name, "installed": hp.installed, "version": hp.version},
        "carried": {
            "mcp": lock.tool.iter().filter(|t| t.kind == "mcp").count(),
            "skills": lock.skill.len(),
            "instructions": lock.instructions.len(),
            "deciders": lock.decider.len(),
        },
        "replaced": c.replaced,
        "notes": c.notes,
        "baseline": {
            "success-rate": source.evidence.success_rate.get("mean"),
            "cost-usd-per-task": source.evidence.cost_usd_per_task.get("mean"),
            "trials": source.evidence.trials,
        },
    });
    let report = drop_nulls(&report); // TOML has no null
    lock.migrate = Some(report.clone());
    Ok(MigrateOutcome { lock, vendor: c.vendor, report })
}

/// The default output for migrating `source` to `to`: `harness.<to>.lock` beside it.
pub fn default_out(source: &Path, to: &str) -> PathBuf {
    source.with_file_name(format!("harness.{to}.lock"))
}

/// Migrate and write the lock to `out`, with its vendored files beside it.
pub fn migrate_to(source_path: &Path, out: &Path, reg: &Registry, ctx: &Ctx, opts: &MigrateOptions) -> Result<MigrateOutcome> {
    let source = Lockfile::load(source_path)?;
    let mut o = migrate(&source, source_path, reg, ctx, opts)?;
    let sub = format!("{}/migrate", o.lock.harness.name);
    save_to(&mut o.lock, &o.vendor, source_path, out, ctx, &sub)?;
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{Decider, Skill, Tool, Wiring};

    #[test]
    fn where_parts_go() {
        let r = Registry::builtin();
        let codex = r.harness("codex").unwrap();
        let opencode = r.harness("opencode").unwrap();
        let hermes = r.harness("hermes").unwrap();
        assert_eq!(skill_dest(codex, "review", ".claude/skills/review").as_deref(), Some(".agents/skills/review"));
        assert_eq!(skill_dest(opencode, "review", ".claude/skills/review").as_deref(), Some(".claude/skills/review"));
        assert_eq!(skill_dest(hermes, "review", ".claude/skills/review").as_deref(), Some("{root}/skills/review"));
        assert_eq!(skill_dest(codex, "x", "{root}/skills/x").as_deref(), Some("{root}/skills/x"));
        assert_eq!(skill_dest(codex, "x", "~/.agents/skills/x").as_deref(), Some("~/.agents/skills/x"));
        assert_eq!(skill_dest(r.harness("goose").unwrap(), "x", "{root}/skills/x").as_deref(), Some("~/.agents/skills/x"));
        assert_eq!(instructions_dest(codex, "CLAUDE.md").as_deref(), Some("AGENTS.md"));
        assert_eq!(instructions_dest(opencode, "CLAUDE.md").as_deref(), Some("CLAUDE.md"));
        assert_eq!(instructions_dest(r.harness("gemini-cli").unwrap(), "CLAUDE.md").as_deref(), Some("GEMINI.md"));
    }

    #[test]
    fn providers() {
        let r = Registry::builtin();
        let codex = r.harness("codex").unwrap();
        assert!(supports_provider(codex, "openai") && !supports_provider(codex, "anthropic"));
        assert!(supports_provider(r.harness("claude-code").unwrap(), "anthropic"));
        assert_eq!(target_model(codex).unwrap().0, "openai");
        assert_eq!(target_model(r.harness("claude-code").unwrap()).unwrap().0, "anthropic");
        assert_eq!(target_model(r.harness("cursor-agent").unwrap()).unwrap(), ("cursor".to_string(), "harness-default".to_string()));
    }

    #[test]
    fn claude_code_to_codex() {
        let d = std::env::temp_dir().join(format!("hlock-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let proj = d.join("proj");
        std::fs::create_dir_all(proj.join(".claude/skills/review")).unwrap();
        std::fs::write(proj.join(".claude/skills/review/SKILL.md"), "---\nname: review\n---\n").unwrap();
        std::fs::write(proj.join("CLAUDE.md"), "be brief\n").unwrap();
        let ctx = Ctx { home: d.join("home"), cwd: proj.clone(), env: BTreeMap::new(), path: vec![] };
        let source = Lockfile {
            harness: Harness { name: "claude-code".into(), ..Default::default() },
            llm: vec![LLM { role: "default".into(), provider: "anthropic".into(), model: "opus".into(), ..Default::default() }],
            tool: vec![Tool { name: "gh".into(), kind: "mcp".into(), command: Some("npx".into()), ..Default::default() }],
            skill: vec![Skill { name: "review".into(), origin: "first-party".into(), digest: Some(tree_digest(&proj.join(".claude/skills/review"))), path: Some(".claude/skills/review".into()), scope: Some("project".into()), enabled: true, ..Default::default() }],
            instructions: vec![Instructions { name: "CLAUDE.md".into(), digest: Some(crate::hash::sha256_bytes(b"be brief\n")), scope: "project".into(), ..Default::default() }],
            decider: vec![
                Decider { use_: "approve".into(), provider: "typesafe".into(), model: "kev-9b".into(), via: "hook".into(), wiring: vec![Wiring { file: "settings".into(), event: "PermissionRequest".into() }], ..Default::default() },
                Decider { use_: "route".into(), provider: "typesafe".into(), model: "kev-9b".into(), via: "hook".into(), ..Default::default() },
            ],
            config: vec![ConfigFile { id: "settings".into(), restore: "whole".into(), ..Default::default() }],
            ..Default::default()
        };
        let reg = Registry::builtin();
        source.save(&proj.join("harness.lock")).unwrap();
        let o = migrate(&source, &proj.join("harness.lock"), &reg, &ctx, &MigrateOptions { to: "codex".into(), model: None }).unwrap();
        let l = &o.lock;
        assert_eq!(l.harness.name, "codex");
        assert_eq!((l.llm[0].provider.as_str(), l.llm[0].model.as_str()), ("openai", "gpt-5.5"));
        assert_eq!(l.skill[0].path.as_deref(), Some(".agents/skills/review"));
        assert_eq!(l.skill[0].vendored.as_deref(), Some(".hlock/vendor/codex/migrate/skills/review"));
        // each output lock gets its own vendored files
        let a = migrate_to(&proj.join("harness.lock"), &proj.join("a.lock"), &reg, &ctx, &MigrateOptions { to: "codex".into(), model: None }).unwrap();
        let b = migrate_to(&proj.join("harness.lock"), &proj.join("b.lock"), &reg, &ctx, &MigrateOptions { to: "codex".into(), model: Some("openai:gpt-5.5-mini".into()) }).unwrap();
        let cfg = |l: &Lockfile| l.config.iter().find(|c| c.id == "config").and_then(|c| c.vendored.clone()).unwrap();
        assert_eq!(cfg(&a.lock), ".hlock/vendor/codex/migrate/a/config.pointers.json");
        assert_eq!(cfg(&b.lock), ".hlock/vendor/codex/migrate/b/config.pointers.json");
        assert!(proj.join(cfg(&a.lock)).is_file() && proj.join(a.lock.skill[0].vendored.as_ref().unwrap()).join("SKILL.md").is_file());
        assert_eq!(l.instructions[0].name, "AGENTS.md");
        assert!(l.instructions[0].vendored.is_some());
        assert_eq!(l.decider.len(), 1);
        assert_eq!(l.decider[0].wiring[0].file, "config");
        let cfg = l.config.iter().find(|c| c.id == "config").unwrap();
        assert!(cfg.pointers.contains(&"/mcp_servers".to_string()) && cfg.pointers.contains(&"/model".to_string()), "{:?}", cfg.pointers);
        assert!(o.vendor.iter().any(|f| f.rel.ends_with("config.pointers.json")));

        // the project's own AGENTS.md is pinned instead of moving CLAUDE.md onto it
        std::fs::write(proj.join("AGENTS.md"), "shared\n").unwrap();
        let o = migrate(&source, &proj.join("harness.lock"), &reg, &ctx, &MigrateOptions { to: "codex".into(), model: Some("openai:gpt-5.5-mini".into()) }).unwrap();
        assert_eq!(o.lock.instructions[0].name, "AGENTS.md");
        assert_eq!(o.lock.instructions[0].vendored, None);
        assert_eq!(o.lock.instructions[0].digest.as_deref(), Some(crate::hash::sha256_bytes(b"shared\n").as_str()));
        assert_eq!(o.lock.llm[0].model, "gpt-5.5-mini");

        let err = migrate(&source, &proj.join("harness.lock"), &reg, &ctx, &MigrateOptions { to: "codex".into(), model: Some("anthropic:opus".into()) }).unwrap_err();
        assert!(err.to_string().contains("doesn't talk to"), "{err}");
        let err = migrate(&source, &proj.join("harness.lock"), &reg, &ctx, &MigrateOptions { to: "claude".into(), model: None }).unwrap_err();
        assert!(err.to_string().contains("already on"), "{err}");
        let err = migrate(&source, &proj.join("harness.lock"), &reg, &ctx, &MigrateOptions { to: "pydantic-ai".into(), model: None }).unwrap_err();
        assert!(err.to_string().contains("between agent CLIs"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
