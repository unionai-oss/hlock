//! Audit and trust (`specs/audit.md`): scanning what a lock places for hidden Unicode,
//! and `hlock audit`, which also checks integrity, executables, policy and secrets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lockfile::Lockfile;
use crate::profiles::{Ctx, HarnessProfile};
use crate::scan::{Finding, Severity};

/// Content a lock entry places in a harness, and where to read it now.
#[derive(Debug, Clone, Serialize)]
pub struct Content {
    /// `skill`, `instructions`, `config`.
    pub kind: String,
    pub name: String,
    /// What findings are named by: the entry's path as the lock records it.
    pub label: String,
    /// The vendored copy, the stored tree, or the installed file.
    pub path: PathBuf,
    /// Where it was read from: `vendored`, `store`, or `installed`.
    pub from: String,
    /// The entry's `scan` key.
    pub scan: Option<String>,
}

/// What each skill, instruction file and config file of `lock` places, readable here.
pub fn contents(lock: &Lockfile, lock_dir: &Path, profile: Option<&HarnessProfile>, ctx: &Ctx) -> Vec<Content> {
    let root = profile.map(|p| p.config_root(ctx));
    let installed = |p: &str| ctx.expand(p, root.as_deref());
    let mut out = Vec::new();
    let mut push = |kind: &str, name: &str, label: String, vendored: Option<&String>, digest: Option<&String>, at: Option<PathBuf>, scan: &Option<String>| {
        let rel = vendored.filter(|v| crate::fsutil::safe_rel(v)).cloned();
        let vendored = rel.as_ref().map(|v| lock_dir.join(v)).filter(|p| p.exists());
        let stored = digest.and_then(|d| crate::deps::stored(lock_dir, d));
        let (path, from) = match (vendored, stored, at.filter(|p| p.exists())) {
            (Some(v), _, _) => (v, "vendored"),
            (None, Some(s), _) => (s, "store"),
            (None, None, Some(i)) => (i, "installed"),
            _ => return,
        };
        // a vendored copy is named by its path in the repository (for SARIF and --allow-hidden)
        let label = if from == "vendored" { rel.unwrap_or(label) } else { label };
        out.push(Content { kind: kind.into(), name: name.into(), label, path, from: from.into(), scan: scan.clone() });
    };
    for s in lock.skill.iter().filter(|s| !matches!(s.origin.as_str(), "harness-builtin" | "bundled")) {
        let label = s.path.clone().unwrap_or_else(|| format!("skills/{}", s.name));
        push("skill", &s.name, label, s.vendored.as_ref(), s.digest.as_ref(), s.path.as_deref().map(installed), &s.scan);
    }
    for i in &lock.instructions {
        push("instructions", &i.name, i.name.clone(), i.vendored.as_ref(), i.digest.as_ref(), Some(installed(&i.name)), &i.scan);
    }
    for c in &lock.config {
        let at = profile.and_then(|p| p.file_path(&c.id, ctx));
        push("config", &c.id, c.path.clone(), c.vendored.as_ref(), None, at, &c.scan);
    }
    out
}

/// The findings for each piece of content.
pub fn scan_contents(contents: &[Content]) -> Vec<(Content, Vec<Finding>)> {
    contents.iter().map(|c| (c.clone(), crate::scan::scan_path(&c.path, &c.label))).collect()
}

/// Whether a scan's critical findings were let through on purpose for this entry.
pub fn allowed(scan: Option<&str>) -> bool {
    scan == Some("allowed:critical")
}

/// Whether `--allow-hidden FILE` covers a finding: FILE names its file, or a directory of it.
/// A path of two or more parts also matches the end of the finding's path (a fetched file
/// is named by where it's placed); a bare name like `SKILL.md` doesn't match every skill.
pub fn allows(allow: &[String], f: &Finding) -> bool {
    allow.iter().any(|a| {
        let a = a.trim_end_matches('/').trim_start_matches("./");
        !a.is_empty() && (f.file == a || f.file.starts_with(&format!("{a}/")) || (a.contains('/') && (f.file.ends_with(&format!("/{a}")) || f.file.contains(&format!("/{a}/")))))
    })
}

/// The critical findings `allow` doesn't cover, as an error: what to show before nothing is
/// written.
pub fn blocking(findings: &[Finding], allow: &[String], what: &str) -> crate::error::Result<()> {
    let bad: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Critical && !allows(allow, f)).collect();
    if bad.is_empty() {
        return Ok(());
    }
    let mut lines: Vec<String> = bad.iter().take(10).map(|f| format!("  {}", f.label())).collect();
    if bad.len() > 10 {
        lines.push(format!("  … and {} more", bad.len() - 10));
    }
    let files: std::collections::BTreeSet<&str> = bad.iter().map(|f| f.file.as_str()).collect();
    let flags = files.iter().map(|f| format!("--allow-hidden {}", crate::fsutil::shell_quote(f))).collect::<Vec<_>>().join(" ");
    Err(crate::error::Error::Audit(format!(
        "{what} has characters that hide text from a reviewer:\n{}\nnothing was written. Look at the file in an editor that shows them; if they're meant to be there, pass {flags}",
        lines.join("\n")
    )))
}

/// Set the `scan` key of a captured lock's entries from their content here, and say which
/// have hidden characters. `pin` records what's on your machine, so it warns rather than
/// refuses.
pub fn scan_capture(lock: &mut Lockfile, profile: &HarnessProfile, ctx: &Ctx, vendor: &[crate::capture::VendorFile]) -> Vec<String> {
    let root = profile.config_root(ctx);
    let mut warnings = Vec::new();
    let mut note = |kind: &str, name: &str, findings: &[Finding]| {
        let crit = findings.iter().filter(|f| f.severity == Severity::Critical).count();
        let warn = findings.iter().filter(|f| f.severity == Severity::Warning).count();
        if crit > 0 {
            let first = findings.iter().find(|f| f.severity == Severity::Critical).map(|f| f.label()).unwrap_or_default();
            warnings.push(format!("{kind} {name} has {crit} character{} that hide text ({first}); `hlock sync` won't place it until it's cleaned and re-pinned, or let through with --allow-hidden", if crit == 1 { "" } else { "s" }));
        } else if warn > 0 {
            warnings.push(format!("{kind} {name} has {warn} invisible character{} (zero-width or bidi marks); see `hlock audit`", if warn == 1 { "" } else { "s" }));
        }
    };
    for s in lock.skill.iter_mut().filter(|s| !matches!(s.origin.as_str(), "harness-builtin" | "bundled")) {
        let Some(path) = s.path.as_deref().map(|p| ctx.expand(p, Some(&root))) else { continue };
        let f = crate::scan::scan_path(&path, s.path.as_deref().unwrap_or(&s.name));
        note("skill", &s.name, &f);
        s.scan = Some(crate::scan::summary(&f));
    }
    for i in lock.instructions.iter_mut() {
        let f = crate::scan::scan_path(&ctx.expand(&i.name, Some(&root)), &i.name);
        note("instructions", &i.name, &f);
        i.scan = Some(crate::scan::summary(&f));
    }
    for c in lock.config.iter_mut() {
        // what's restored: the vendored (redacted) copy, else the file itself
        let f = match c.vendored.as_ref().and_then(|v| vendor.iter().find(|x| &x.rel == v)) {
            Some(v) => crate::scan::scan_bytes(&v.bytes, &c.path),
            None => profile.file_path(&c.id, ctx).map(|p| crate::scan::scan_path(&p, &c.path)).unwrap_or_default(),
        };
        note("config", &c.path, &f);
        c.scan = Some(crate::scan::summary(&f));
    }
    warnings
}

/// Findings by severity, for summaries.
pub fn counts(findings: &[Finding]) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for f in findings {
        *m.entry(f.severity.as_str()).or_default() += 1;
    }
    m
}


// --------------------------------------------------------------------------- //
// hlock audit
// --------------------------------------------------------------------------- //

/// A problem one of the audit's checks found that isn't a hidden character.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    /// `integrity`, `policy`, `secrets`, `executables`.
    pub check: String,
    /// `error` or `warning`.
    pub severity: String,
    /// The file it's about, as the lock names it (for SARIF).
    pub file: String,
    pub message: String,
    /// A rule id: `hlock/integrity/digest`, `hlock/policy/sources`, …
    pub rule: String,
}

/// What `hlock audit` found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub lock: String,
    /// 1. every vendored, fetched and installed file matches its digest
    pub integrity: Vec<Problem>,
    /// 2. hidden characters in what the lock places
    pub content: Vec<Finding>,
    pub files_scanned: usize,
    /// 3. what it would run here, with trust status
    pub executables: Vec<crate::trust::Executable>,
    /// 4. the project's policy
    pub policy: Vec<Problem>,
    /// 5. values pin's redaction would have replaced
    pub secrets: Vec<Problem>,
    /// Files `--strip` cleaned.
    pub stripped: Vec<String>,
    /// With `--ci`, warnings about the lock's own content count as failures.
    pub ci: bool,
    pub passed: bool,
}

impl Report {
    pub fn errors(&self) -> usize {
        let crit = self.content.iter().filter(|f| f.severity == Severity::Critical).count();
        crit + [&self.integrity, &self.policy, &self.secrets].iter().flat_map(|v| v.iter()).filter(|p| p.severity == "error").count()
    }

    /// Warnings about the lock's own content (hidden characters, credential-looking words):
    /// what `--ci` fails on. Not an installed copy that drifted, or a policy in `warn` mode.
    pub fn content_warnings(&self) -> usize {
        self.content.iter().filter(|f| f.severity == Severity::Warning).count() + self.secrets.iter().filter(|p| p.severity == "warning").count()
    }

    pub fn warnings(&self) -> usize {
        let warn = self.content.iter().filter(|f| f.severity == Severity::Warning).count();
        warn + [&self.integrity, &self.policy, &self.secrets].iter().flat_map(|v| v.iter()).filter(|p| p.severity == "warning").count()
    }
}

fn problem(check: &str, severity: &str, file: &str, rule: &str, message: String) -> Problem {
    Problem { check: check.into(), severity: severity.into(), file: file.into(), rule: rule.into(), message }
}

/// Secret-looking values in a config document: a value under a secret key, or a value that
/// looks like a credential on its own, that isn't a placeholder or a reference.
fn secrets_in(v: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                path.push(k.clone());
                if let serde_json::Value::String(s) = x {
                    let placeholder = s.contains(crate::secrets::PREFIX) || s.trim().starts_with('$') || s.starts_with("{env:") || s.starts_with("env:") || crate::capture::is_placeholder_key(s) || s.is_empty();
                    if !placeholder && (crate::capture::looks_secret(s) || (crate::capture::is_secret_key(k) && s.len() >= 8 && !s.contains(' '))) {
                        out.push(format!("/{}", path.join("/")));
                    }
                } else {
                    secrets_in(x, path, out);
                }
                path.pop();
            }
        }
        serde_json::Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                path.push(i.to_string());
                secrets_in(x, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Every vendored, stored and installed copy against its digest in the lock, and the
/// declared dependencies that aren't here to check.
fn integrity(lock: &Lockfile, dir: &Path, contents: &[Content]) -> Vec<Problem> {
    let mut integrity = Vec::new();
    for c in contents {
        let want = match c.kind.as_str() {
            "skill" => lock.skill.iter().find(|s| s.name == c.name).and_then(|s| s.digest.clone()),
            "instructions" => lock.instructions.iter().find(|i| i.name == c.name).and_then(|i| i.digest.clone()),
            _ => None,
        };
        if let Some(want) = want {
            let have = crate::deps::content_digest(&c.path).ok();
            if have.as_deref() != Some(want.as_str()) {
                let sev = if c.from == "installed" { "warning" } else { "error" };
                let what = if c.from == "installed" { "the installed copy differs from the lock (hlock check says how)" } else { "doesn't match its digest in the lock: it was edited after it was pinned" };
                integrity.push(problem("integrity", sev, &c.label, "hlock/integrity/digest", format!("{} {}: {what}", c.kind, c.name)));
            }
        }
    }
    // a declared dependency that isn't fetched here can't be checked (sync checks it as it fetches)
    let here = |kind: &str, name: &str| contents.iter().any(|c| c.kind == kind && c.name == name);
    for s in lock.skill.iter().filter(|s| s.dep.declared && s.digest.is_some() && !here("skill", &s.name)) {
        integrity.push(problem("integrity", "warning", s.source.as_deref().unwrap_or(&s.name), "hlock/integrity/unfetched", format!("skill {}: not fetched here, so not checked (hlock sync fetches it and checks its digest)", s.name)));
    }
    for i in lock.instructions.iter().filter(|i| i.dep.declared && i.digest.is_some() && !here("instructions", &i.name)) {
        integrity.push(problem("integrity", "warning", i.source.as_deref().unwrap_or(&i.name), "hlock/integrity/unfetched", format!("instructions {}: not fetched here, so not checked (hlock sync fetches it and checks its digest)", i.name)));
    }
    for cf in &lock.config {
        let (Some(v), Some(want)) = (cf.vendored.as_ref().filter(|v| crate::fsutil::safe_rel(v)), &cf.digest) else { continue };
        if cf.restore == "record" {
            continue;
        }
        let fmt = if cf.pointers.is_empty() { if cf.format == "jsonc" { "json" } else { cf.format.as_str() } } else { "json" };
        match crate::capture::read_config(&dir.join(v), fmt) {
            Ok(Some(doc)) if &crate::hash::content_hash(&doc) != want => integrity.push(problem("integrity", "error", v, "hlock/integrity/digest", format!("config {}: the vendored copy doesn't match its digest in the lock", cf.path))),
            Ok(None) | Err(_) => integrity.push(problem("integrity", "error", v, "hlock/integrity/missing", format!("config {}: the vendored copy {v} is missing or unreadable", cf.path))),
            _ => {}
        }
    }
    for s in lock.skill.iter().filter(|s| s.vendored.is_some()) {
        if let Some(v) = &s.vendored {
            if !dir.join(v).is_dir() {
                integrity.push(problem("integrity", "error", v, "hlock/integrity/missing", format!("skill {}: the vendored copy {v} is missing", s.name)));
            }
        }
    }
    integrity
}

/// Run the audit on a lock: integrity, hidden characters, executables, policy, secrets.
/// With `strip`, hidden characters are taken out of vendored files first, and their digests
/// and scans are updated in the lock.
pub fn run(lock_path: &Path, reg: &crate::profiles::Registry, ctx: &Ctx, ci: bool, strip: bool, verbose: bool) -> crate::error::Result<Report> {
    let mut lock = Lockfile::load(lock_path)?;
    let dir = crate::sync::lock_dir_of(lock_path, ctx);
    let profile = reg.harness(&lock.harness.name);
    let mut stripped = Vec::new();
    if strip {
        // a file edited after it was pinned isn't cleaned: re-digesting it would record the
        // edit as what's pinned
        let before = integrity(&lock, &dir, &contents(&lock, &dir, profile, ctx));
        let bad: Vec<&Problem> = before.iter().filter(|p| p.severity == "error").collect();
        if !bad.is_empty() {
            return Err(crate::error::Error::Audit(format!(
                "refusing to --strip: what it would clean doesn't match the lock\n{}\nstripping would record the edits as what's pinned. Restore the vendored files (git checkout), or re-pin",
                bad.iter().map(|p| format!("  {}", p.message)).collect::<Vec<_>>().join("\n")
            )));
        }
        stripped = strip_vendored(&mut lock, &dir, profile, ctx)?;
        if !stripped.is_empty() {
            lock.save(lock_path)?;
        }
    }
    let contents = contents(&lock, &dir, profile, ctx);

    // 1. integrity
    let integrity = integrity(&lock, &dir, &contents);

    // 2. content
    let scanned = scan_contents(&contents);
    let files_scanned = contents.iter().map(|c| crate::scan::text_files(&c.path).len()).sum();
    let mut content = Vec::new();
    for (c, fs) in scanned {
        let allowed = allowed(c.scan.as_deref());
        for mut f in fs {
            if f.severity == Severity::Critical && allowed {
                // let through on purpose: reported, but as a warning
                f.severity = Severity::Warning;
                f.description = format!("{} (allowed in the lock)", f.description);
            }
            if f.severity > Severity::Info || verbose {
                content.push(f);
            }
        }
    }

    // 3. executables
    let store = crate::trust::TrustStore::load(ctx)?;
    let policy = crate::policy::Policy::for_lock(lock_path, ctx)?;
    let executables = crate::trust::assess(&lock, &dir, profile, ctx, &policy, &store);

    // 4. policy
    let lock_file = lock_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| crate::LOCKFILE_PATH.into());
    let policy_problems: Vec<Problem> = if policy.enforcement == "off" {
        Vec::new()
    } else {
        let sev = if policy.blocks() { "error" } else { "warning" };
        policy.check_lock(&lock).into_iter().map(|v| problem("policy", sev, &lock_file, &format!("hlock/policy/{}", v.section), format!("{} ({})", v.message, v.file))).collect()
    };

    // 5. secrets
    let mut secrets = Vec::new();
    for cf in &lock.config {
        let Some(v) = cf.vendored.as_ref().filter(|v| crate::fsutil::safe_rel(v)) else { continue };
        let fmt = if cf.pointers.is_empty() { if cf.format == "jsonc" { "json" } else { cf.format.as_str() } } else { "json" };
        if let Ok(Some(doc)) = crate::capture::read_config(&dir.join(v), fmt) {
            let mut found = Vec::new();
            secrets_in(&doc, &mut Vec::new(), &mut found);
            for ptr in found {
                secrets.push(problem("secrets", "error", v, "hlock/secrets/value", format!("config {}: {ptr} holds what looks like a secret value; pin replaces those with HLOCK_SECRET_* placeholders (was it edited by hand?)", cf.path)));
            }
        }
    }
    for c in contents.iter().filter(|c| c.from == "vendored" && c.kind != "config") {
        for f in crate::scan::text_files(&c.path) {
            let Ok(text) = crate::fsutil::read_regular_text(&f, 2 << 20) else { continue };
            if crate::capture::redact_text(&text) != text {
                let rel = f.strip_prefix(&dir).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| f.display().to_string());
                secrets.push(problem("secrets", "warning", &rel, "hlock/secrets/value", format!("{} {}: {rel} has a word that looks like a credential; check it isn't one before committing", c.kind, c.name)));
            }
        }
    }
    for t in &lock.tool {
        for (k, v) in t.env.iter().chain(t.headers.iter()) {
            if !v.contains(crate::secrets::PREFIX) && crate::capture::looks_secret(v) {
                secrets.push(problem("secrets", "error", &lock_file, "hlock/secrets/value", format!("MCP server {}: {k} holds what looks like a secret value, not a placeholder", t.name)));
            }
        }
    }

    let mut report = Report { lock: lock_path.display().to_string(), integrity, content, files_scanned, executables, policy: policy_problems, secrets, stripped, ci, passed: false };
    report.passed = report.errors() == 0 && (!ci || report.content_warnings() == 0);
    Ok(report)
}

/// Take hidden characters out of vendored files (`--strip`), and update each entry's digest
/// and scan. Returns the files changed.
pub fn strip_vendored(lock: &mut Lockfile, dir: &Path, profile: Option<&HarnessProfile>, ctx: &Ctx) -> crate::error::Result<Vec<String>> {
    let mut changed = Vec::new();
    for c in contents(lock, dir, profile, ctx).into_iter().filter(|c| c.from == "vendored") {
        let files = crate::scan::strip_path(&c.path)?;
        if files.is_empty() {
            continue;
        }
        changed.extend(files.iter().map(|f| f.strip_prefix(dir).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default()));
        let scan = Some(crate::scan::summary(&crate::scan::scan_path(&c.path, &c.label)));
        match c.kind.as_str() {
            "skill" => {
                if let Some(s) = lock.skill.iter_mut().find(|s| s.name == c.name) {
                    s.digest = Some(crate::capture::tree_digest(&c.path));
                    s.scan = scan;
                }
            }
            "instructions" => {
                if let Some(i) = lock.instructions.iter_mut().find(|i| i.name == c.name) {
                    i.digest = crate::deps::content_digest(&c.path).ok();
                    i.scan = scan;
                }
            }
            _ => {
                if let Some(cf) = lock.config.iter_mut().find(|x| x.id == c.name) {
                    let fmt = if cf.pointers.is_empty() { if cf.format == "jsonc" { "json".to_string() } else { cf.format.clone() } } else { "json".into() };
                    if let Ok(Some(doc)) = crate::capture::read_config(&c.path, &fmt) {
                        cf.digest = Some(crate::hash::content_hash(&doc));
                    }
                    cf.scan = scan;
                }
            }
        }
    }
    Ok(changed)
}

// --------------------------------------------------------------------------- //
// SARIF 2.1.0
// --------------------------------------------------------------------------- //

/// The report as SARIF 2.1.0, for GitHub code scanning. Hidden characters use APM's rule
/// ids (`apm/hidden-unicode/<category>`) so the two tools' alerts line up.
pub fn sarif(r: &Report) -> serde_json::Value {
    use serde_json::json;
    let level = |sev: &str| match sev {
        "critical" | "error" => "error",
        "warning" => "warning",
        _ => "note",
    };
    let mut rules: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut results = Vec::new();
    // the lock as the repository names it (a relative path), else its file name
    let lock_uri = if Path::new(&r.lock).is_relative() { r.lock.trim_start_matches("./").to_string() } else { r.lock.rsplit('/').next().unwrap_or(&r.lock).to_string() };
    for f in &r.content {
        let id = format!("apm/hidden-unicode/{}", f.category);
        rules.entry(id.clone()).or_insert_with(|| json!({"id": id, "shortDescription": {"text": format!("Hidden Unicode: {}", f.category)}, "defaultConfiguration": {"level": level(f.severity.as_str())}, "helpUri": "https://docs.hlock.ai/guides/audit/#hidden-characters"}));
        results.push(json!({
            "ruleId": id,
            "level": level(f.severity.as_str()),
            "message": {"text": format!("{} ({})", f.description, f.codepoint)},
            // a finding in a parsed config value, or a file too large to scan, has no line
            "locations": [if f.line > 0 { json!({"physicalLocation": {"artifactLocation": {"uri": f.file}, "region": {"startLine": f.line, "startColumn": f.column}}}) } else { json!({"physicalLocation": {"artifactLocation": {"uri": f.file}}}) }],
            "properties": {"codepoint": f.codepoint, "category": f.category},
        }));
    }
    for p in r.integrity.iter().chain(&r.policy).chain(&r.secrets) {
        rules.entry(p.rule.clone()).or_insert_with(|| json!({"id": p.rule, "shortDescription": {"text": p.rule.trim_start_matches("hlock/").replace('/', ": ")}, "defaultConfiguration": {"level": level(&p.severity)}, "helpUri": "https://docs.hlock.ai/guides/audit/"}));
        results.push(json!({
            "ruleId": p.rule,
            "level": level(&p.severity),
            "message": {"text": p.message},
            "locations": [{"physicalLocation": {"artifactLocation": {"uri": if p.file.ends_with(".lock") && !p.file.contains('/') { lock_uri.clone() } else { p.file.clone() }}}}],
        }));
    }
    for e in r.executables.iter().filter(|e| e.status == "untrusted" || e.status == "denied") {
        let id = "hlock/executables/untrusted".to_string();
        rules.entry(id.clone()).or_insert_with(|| json!({"id": id, "shortDescription": {"text": "An executable not yet trusted on this machine"}, "defaultConfiguration": {"level": "note"}, "helpUri": "https://docs.hlock.ai/guides/audit/#executables"}));
        results.push(json!({
            "ruleId": id,
            "level": "note",
            "message": {"text": format!("{} {} runs `{}` ({})", e.kind, e.name, e.detail, e.status)},
            "locations": [{"physicalLocation": {"artifactLocation": {"uri": lock_uri}}}],
            "properties": {"digest": e.digest},
        }));
    }
    json!({
        "$schema": "https://docs.oasis-open.org/sarif/sarif/v2.1.0/cos02/schemas/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {"name": "hlock-audit", "version": env!("CARGO_PKG_VERSION"), "informationUri": "https://docs.hlock.ai/guides/audit/", "rules": rules.into_values().collect::<Vec<_>>()}},
            "results": results,
            // columns count code points, as the scanner does
            "columnKind": "unicodeCodePoints",
            "invocations": [{"executionSuccessful": true, "properties": {"filesScanned": r.files_scanned}}],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_hidden_names_files_or_directories() {
        let f = crate::scan::scan_text("x\u{202E}", ".hlock/vendor/pi/skills/x/SKILL.md").remove(0);
        assert!(allows(&[".hlock/vendor/pi/skills/x/SKILL.md".into()], &f));
        assert!(allows(&[".hlock/vendor/pi/skills/x/".into()], &f));
        assert!(allows(&["skills/x/SKILL.md".into()], &f), "a path of two parts matches the end");
        assert!(allows(&["skills/x".into()], &f));
        assert!(!allows(&["SKILL.md".into()], &f), "a bare name doesn't match every skill");
        assert!(!allows(&["".into()], &f) && !allows(&["other.md".into()], &f));
        let e = blocking(&[f.clone()], &[], "skill x").unwrap_err().to_string();
        assert!(e.contains("U+202E") && e.contains("--allow-hidden") && e.contains("nothing was written"), "{e}");
        assert!(blocking(&[f], &["skills/x/SKILL.md".into()], "skill x").is_ok());
    }
}
