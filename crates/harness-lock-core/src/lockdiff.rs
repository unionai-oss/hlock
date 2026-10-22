//! `hlock diff`: the content behind a lock's digests (`specs/reviewable-diffs.md`).
//!
//! A lock pins instruction files, skills and config by digest, so a diff of the lock shows
//! that one changed but not how. [`diff`] compares two locks the way `pin` does
//! ([`crate::capture::diff_locks`]), then finds the content behind each changed digest
//! (a vendored or tuned copy, the project's file, the dependency store, or with `fetch` the
//! dependency's source), checks it against the digest, and returns a unified diff of it.
//! Content that can't be found or doesn't match is reported, never shown.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::lockfile::{ConfigFile, FileComponent, Instructions, Lockfile, Skill};

/// Files larger than this are listed with their size, not diffed.
const MAX_DIFF_BYTES: usize = 64 * 1024;
/// The most a lock read from git may be.
const MAX_LOCK_BYTES: usize = 16 << 20;
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// One side of a diff: a lock, and where the files its relative paths name come from.
pub struct Side {
    pub lock: Lockfile,
    /// How the side is shown (`harness.lock.prev`, `main:harness.lock`).
    pub label: String,
    /// The directory the lock's relative paths resolve against.
    dir: PathBuf,
    git: Option<GitRev>,
}

/// A lock read from a git revision. Its files are extracted on demand into `tmp`, which
/// mirrors the repository's layout and is removed when the side is dropped.
struct GitRev {
    repo: PathBuf,
    rev: String,
    /// The lock's directory, relative to the repository (`""` at its root).
    prefix: String,
    tmp: PathBuf,
    extracted: RefCell<BTreeSet<String>>,
}

impl Drop for GitRev {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args).stdin(std::process::Stdio::null());
    let out = crate::process::run(cmd, &crate::process::Limits::timeout(GIT_TIMEOUT)).map_err(|e| Error::Lockfile(format!("can't run git: {e}")))?;
    if !out.success {
        return Err(Error::Lockfile(format!("git {}: {}", args.first().copied().unwrap_or(""), out.stderr_text().trim())));
    }
    Ok(out.stdout)
}

impl Side {
    /// A lock file on disk; its paths resolve against its directory.
    pub fn file(path: &Path) -> Result<Side> {
        let lock = Lockfile::load(path)?;
        let dir = crate::fsutil::absolute(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")));
        Ok(Side { lock, label: path.display().to_string(), dir, git: None })
    }

    /// The lock at `lock`'s path in git revision `rev`.
    pub fn rev(lock: &Path, rev: &str) -> Result<Side> {
        if rev.is_empty() || rev.starts_with('-') || rev.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::Lockfile(format!("{rev:?} isn't a lock file or a git revision")));
        }
        let dir = crate::fsutil::absolute(lock.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")));
        let top = git(&dir, &["rev-parse", "--show-toplevel"]).map_err(|_| Error::Lockfile(format!("{rev:?} isn't a lock file, and {} isn't in a git repository", dir.display())))?;
        let repo = PathBuf::from(String::from_utf8_lossy(&top).trim());
        let prefix = crate::fsutil::resolve(&dir)
            .strip_prefix(crate::fsutil::resolve(&repo))
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .map_err(|_| Error::Lockfile(format!("{} isn't inside {}", dir.display(), repo.display())))?;
        let name = lock.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| crate::LOCKFILE_PATH.into());
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let bytes = git(&repo, &["show", &format!("{rev}:{rel}")]).map_err(|_| Error::Lockfile(format!("{rel} isn't in git revision {rev}")))?;
        if bytes.len() > MAX_LOCK_BYTES {
            return Err(Error::Lockfile(format!("{rev}:{rel} is too large to be a lock")));
        }
        let lock = Lockfile::from_str(&String::from_utf8_lossy(&bytes))?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!("hlock-diff-{}-{nanos}", std::process::id()));
        let git = GitRev { repo, rev: rev.to_string(), prefix, tmp, extracted: RefCell::new(BTreeSet::new()) };
        Ok(Side { lock, label: format!("{rev}:{rel}"), dir, git: Some(git) })
    }

    /// The file or directory at `rel` (relative to the lock) on this side, if it exists.
    fn path(&self, rel: &str) -> Option<PathBuf> {
        if !crate::fsutil::safe_rel(rel) {
            return None;
        }
        let Some(g) = &self.git else {
            let p = self.dir.join(rel);
            return std::fs::symlink_metadata(&p).is_ok().then_some(p);
        };
        let full = if g.prefix.is_empty() { rel.trim_end_matches('/').to_string() } else { format!("{}/{}", g.prefix, rel.trim_end_matches('/')) };
        let dest = g.tmp.join(&full);
        if g.extracted.borrow().contains(&full) {
            return std::fs::symlink_metadata(&dest).is_ok().then_some(dest);
        }
        g.extracted.borrow_mut().insert(full.clone());
        let listing = git(&g.repo, &["ls-tree", "-r", "--full-tree", "-z", &g.rev, "--", &full]).ok()?;
        for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let entry = String::from_utf8_lossy(entry);
            let Some((meta, path)) = entry.split_once('\t') else { continue };
            let mut parts = meta.split_whitespace();
            let (mode, kind, sha) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            // only regular files: a symlink in the tree isn't followed, as `pin` doesn't
            if kind != "blob" || !(mode == "100644" || mode == "100755") || !crate::fsutil::safe_rel(path) {
                continue;
            }
            let Ok(bytes) = git(&g.repo, &["cat-file", "blob", sha]) else { continue };
            let out = g.tmp.join(path);
            if out.parent().map(std::fs::create_dir_all).transpose().is_err() || std::fs::write(&out, bytes).is_err() {
                continue;
            }
            #[cfg(unix)]
            if mode == "100755" {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
            }
        }
        std::fs::symlink_metadata(&dest).is_ok().then_some(dest)
    }
}

/// Which locks `hlock diff` compares, from its arguments: none compares `<lock>.prev` (or
/// `HEAD`'s lock, without one) with `<lock>`; one compares it (a lock file or a git
/// revision) with `<lock>`; two compare each other.
pub fn sides(lock: &Path, a: Option<&str>, b: Option<&str>) -> Result<(Side, Side)> {
    let one = |x: &str| if Path::new(x).is_file() { Side::file(Path::new(x)) } else { Side::rev(lock, x) };
    match (a, b) {
        (None, _) => {
            let prev = PathBuf::from(format!("{}.prev", lock.display()));
            let old = if prev.is_file() { Side::file(&prev)? } else { Side::rev(lock, "HEAD")? };
            Ok((old, Side::file(lock)?))
        }
        (Some(a), None) => Ok((one(a)?, Side::file(lock)?)),
        (Some(a), Some(b)) => Ok((one(a)?, one(b)?)),
    }
}

#[derive(Clone, Copy)]
enum Body<'a> {
    Instructions(&'a Instructions),
    Skill(&'a Skill),
    Config(&'a ConfigFile),
    File(&'a FileComponent),
}

impl Body<'_> {
    fn digest(&self) -> Option<&str> {
        match self {
            Body::Instructions(i) => i.digest.as_deref(),
            Body::Skill(s) => s.digest.as_deref(),
            Body::Config(c) => c.digest.as_deref(),
            Body::File(c) => c.digest.as_deref(),
        }
    }

    /// The relative paths on a side that may hold this entry's content, best first.
    fn candidates(&self) -> Vec<(&'static str, String)> {
        let plain = |p: &str| !p.contains('{') && !p.starts_with('~') && crate::fsutil::safe_rel(p);
        let mut out = Vec::new();
        match self {
            Body::Instructions(i) => {
                let from = if i.source.as_deref() == Some("tune") { "tuned" } else { "vendored" };
                out.extend(i.vendored.clone().map(|v| (from, v)));
                if plain(&i.name) {
                    out.push(("project", i.name.clone()));
                }
            }
            Body::Skill(s) => {
                out.extend(s.vendored.clone().map(|v| ("vendored", v)));
                out.extend(s.path.clone().filter(|p| plain(p)).map(|p| ("project", p)));
                out.extend(s.source.as_deref().and_then(|p| p.strip_prefix("./")).filter(|p| plain(p)).map(|p| ("project", p.to_string())));
            }
            Body::Config(c) if c.restore == "record" => out.extend(Some(c.path.clone()).filter(|p| plain(p)).map(|p| ("project", p))),
            Body::Config(c) => out.extend(c.vendored.clone().map(|v| ("vendored", v))),
            Body::File(c) => {
                let from = if c.source.as_deref() == Some("tune") { "tuned" } else { "vendored" };
                out.extend(c.vendored.clone().map(|v| (from, v)));
                if plain(&c.path) {
                    out.push(("project", c.path.clone()));
                }
            }
        }
        out
    }

    /// Whether `path` holds exactly what the lock pinned. An entry without a digest pins
    /// nothing to check against.
    fn matches(&self, path: &Path) -> bool {
        let Some(want) = self.digest() else { return true };
        match self {
            Body::Config(c) if c.restore != "record" => {
                let fmt = if !c.pointers.is_empty() { "json" } else if c.format == "jsonc" { "json" } else { c.format.as_str() };
                matches!(crate::capture::read_config(path, fmt), Ok(Some(doc)) if crate::hash::content_hash(&doc) == want)
            }
            _ => crate::deps::content_digest(path).ok().as_deref() == Some(want),
        }
    }

    fn entry(&self) -> Option<crate::deps::Entry> {
        match self {
            Body::Instructions(i) if i.dep.declared => Some(crate::deps::Entry::Instructions((*i).clone())),
            Body::Skill(s) if s.dep.declared => Some(crate::deps::Entry::Skill((*s).clone())),
            _ => None,
        }
    }
}

/// Where an entry's content was found, or why it wasn't.
fn locate(side: &Side, body: Body, project: &Path, fetch: bool, ctx: &crate::profiles::Ctx) -> std::result::Result<(PathBuf, &'static str), String> {
    let mut stale = false;
    for (from, rel) in body.candidates() {
        if let Some(p) = side.path(&rel) {
            if body.matches(&p) {
                return Ok((p, from));
            }
            stale = true;
        }
    }
    if let Some(p) = body.digest().and_then(|d| crate::deps::stored(project, d)) {
        return Ok((p, "store"));
    }
    if let Some(e) = body.entry() {
        if !fetch {
            return Err("not in the dependency store: fetch it with --fetch".into());
        }
        return crate::deps::fetch_locked(&e, project, ctx).map(|p| (p, "fetched")).map_err(|e| e.to_string());
    }
    if stale {
        return Err("the files at its paths don't match its digest in the lock".into());
    }
    Err(if body.digest().is_none() { "the lock records no content for it".into() } else { "its content isn't in this checkout".into() })
}

/// The files of a located entry, by name: a file under `name`, a directory's files under
/// `name/<path>`.
fn files(path: &Path, name: &str) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    if path.is_dir() {
        for rel in crate::capture::walk_files(path) {
            if let Ok(b) = std::fs::read(path.join(&rel)) {
                out.insert(format!("{name}/{}", rel.to_string_lossy().replace('\\', "/")), b);
            }
        }
    } else if let Ok(b) = std::fs::read(path) {
        out.insert(name.to_string(), b);
    }
    out
}

fn text_of(b: &[u8]) -> Option<&str> {
    std::str::from_utf8(b).ok().filter(|t| !t.contains('\0'))
}

/// `text` with each line's hidden characters shown (`<U+202E>`), so a reviewer reads
/// what the agent reads.
fn shown(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|l| {
            let (body, nl) = l.strip_suffix('\n').map(|b| (b, "\n")).unwrap_or((l, ""));
            format!("{}{nl}", crate::scan::visible_line(body))
        })
        .collect()
}

/// The diff of one file, as JSON: its status, line counts, hunks, and hidden characters.
fn file_diff(name: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> Option<Value> {
    if old == new {
        return None;
    }
    let status = match (old, new) {
        (None, Some(_)) => "added",
        (Some(_), None) => "removed",
        _ => "modified",
    };
    let size = |b: Option<&[u8]>| b.map(<[u8]>::len);
    let mut v = json!({"path": name, "status": status, "old_bytes": size(old), "new_bytes": size(new)});
    let (ot, nt) = (old.map(text_of), new.map(text_of));
    let too_large = size(old).max(size(new)).unwrap_or(0) > MAX_DIFF_BYTES;
    if ot.flatten().is_none() && old.is_some() || nt.flatten().is_none() && new.is_some() {
        v["binary"] = json!(true);
        return Some(v);
    }
    let (ot, nt) = (ot.flatten().unwrap_or(""), nt.flatten().unwrap_or(""));
    let findings = crate::scan::scan_text(nt, name);
    if !findings.is_empty() {
        v["hidden"] = json!(crate::scan::summary(&findings));
    }
    let (os, ns) = (shown(ot), shown(nt));
    let d = similar::TextDiff::from_lines(&os, &ns);
    let (mut added, mut removed) = (0, 0);
    for c in d.iter_all_changes() {
        match c.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    v["added"] = json!(added);
    v["removed"] = json!(removed);
    if too_large {
        v["too_large"] = json!(true);
    } else {
        let hunks: Vec<String> = d.unified_diff().context_radius(3).iter_hunks().map(|h| h.to_string()).collect();
        v["hunks"] = json!(hunks.concat());
    }
    Some(v)
}

/// Options for [`diff`].
#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Fetch a declared dependency's content at its locked commit when the store lacks it.
    pub fetch: bool,
}

/// Compare `old` with `new`: `pin`'s summary lines, then each changed or added
/// instruction file, skill and config file with the text diff of its content, and the
/// change in evidence. `project` is where the dependency store is.
pub fn diff(old: &Side, new: &Side, project: &Path, opts: &DiffOptions, ctx: &crate::profiles::Ctx) -> Value {
    let summary = crate::capture::diff_locks(&old.lock, &new.lock);
    let mut components = Vec::new();
    let mut unresolved = 0;
    let mut push = |kind: &str, name: String, o: Option<Body>, n: Option<Body>, source: Option<&str>| {
        let change = match (o, n) {
            (None, Some(_)) => "+",
            (Some(_), None) => "-",
            (Some(a), Some(b)) if a.digest() != b.digest() => "~",
            _ => return,
        };
        let mut c = json!({"kind": kind, "name": name, "change": change});
        if let Some(s) = source {
            c["source"] = json!(s);
        }
        let mut unavailable = Vec::new();
        let mut found = |label: &str, side: &Side, body: Option<Body>| -> Option<BTreeMap<String, Vec<u8>>> {
            let body = body?;
            match locate(side, body, project, opts.fetch, ctx) {
                Ok((p, from)) => {
                    c[format!("{label}_from")] = json!(from);
                    Some(files(&p, &name))
                }
                Err(why) => {
                    unavailable.push(json!({"side": label, "reason": why}));
                    None
                }
            }
        };
        // a removed entry is listed, not shown
        let ofiles = if change == "-" { None } else { found("old", old, o) };
        let nfiles = found("new", new, n);
        let missing = !unavailable.is_empty();
        if !missing {
            let (ofiles, nfiles) = (ofiles.unwrap_or_default(), nfiles.unwrap_or_default());
            let names: BTreeSet<&String> = ofiles.keys().chain(nfiles.keys()).collect();
            let diffs: Vec<Value> = names.into_iter().filter_map(|f| file_diff(f, ofiles.get(f).map(Vec::as_slice), nfiles.get(f).map(Vec::as_slice))).collect();
            c["files"] = json!(diffs);
        } else {
            unresolved += 1;
            c["unavailable"] = json!(unavailable);
        }
        components.push(c);
    };

    let (oi, ni) = (index(&old.lock.instructions, |i| i.name.clone()), index(&new.lock.instructions, |i| i.name.clone()));
    for k in oi.keys().chain(ni.keys()).collect::<BTreeSet<_>>() {
        let n = ni.get(k).copied();
        push("instructions", k.clone(), oi.get(k).copied().map(Body::Instructions), n.map(Body::Instructions), n.and_then(|i| i.source.as_deref()).filter(|s| *s == "tune"));
    }
    let key = |s: &Skill| format!("{} ({})", s.name, s.scope.as_deref().unwrap_or("?"));
    let (os, ns) = (index(&old.lock.skill, key), index(&new.lock.skill, key));
    for k in os.keys().chain(ns.keys()).collect::<BTreeSet<_>>() {
        let name = ns.get(k).or(os.get(k)).map(|s| s.name.clone()).unwrap_or_default();
        push("skill", name, os.get(k).copied().map(Body::Skill), ns.get(k).copied().map(Body::Skill), None);
    }
    let (oc, nc) = (index(&old.lock.config, |c| c.path.clone()), index(&new.lock.config, |c| c.path.clone()));
    for k in oc.keys().chain(nc.keys()).collect::<BTreeSet<_>>() {
        push("config", k.clone(), oc.get(k).copied().map(Body::Config), nc.get(k).copied().map(Body::Config), None);
    }
    let key = |c: &FileComponent| format!("{} ({})", c.name, c.scope);
    for (kind, o, n) in [("subagent", &old.lock.subagent, &new.lock.subagent), ("script", &old.lock.script, &new.lock.script), ("middleware", &old.lock.middleware, &new.lock.middleware)] {
        let (oi, ni) = (index(o, key), index(n, key));
        for k in oi.keys().chain(ni.keys()).collect::<BTreeSet<_>>() {
            let nb = ni.get(k).copied();
            let name = nb.or(oi.get(k).copied()).map(|c| c.name.clone()).unwrap_or_default();
            push(kind, name, oi.get(k).copied().map(Body::File), nb.map(Body::File), nb.and_then(|c| c.source.as_deref()).filter(|s| *s == "tune"));
        }
    }

    json!({
        "old": old.label,
        "new": new.label,
        "summary": summary,
        "components": components,
        "evidence": evidence(&old.lock, &new.lock),
        "unresolved": unresolved,
    })
}

fn index<T>(items: &[T], key: impl Fn(&T) -> String) -> BTreeMap<String, &T> {
    items.iter().map(|x| (key(x), x)).collect()
}

/// How the evidence moved, one line per measure that changed.
fn evidence(old: &Lockfile, new: &Lockfile) -> Vec<String> {
    let (o, n) = (&old.evidence, &new.evidence);
    let mut out = Vec::new();
    let sr = |e: &crate::lockfile::Evidence| e.success_rate.get("mean").copied();
    if sr(o) != sr(n) || o.trials != n.trials {
        let f = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
        out.push(format!("~ evidence success-rate: {} → {}  (n={} → {})", f(sr(o)), f(sr(n)), o.trials, n.trials));
    }
    let cost = |e: &crate::lockfile::Evidence| e.cost_usd_per_task.get("mean").copied();
    if cost(o) != cost(n) {
        let f = |v: Option<f64>| v.map(|v| format!("{v:.4}")).unwrap_or_else(|| "—".into());
        out.push(format!("~ evidence cost-usd-per-task: {} → {}", f(cost(o)), f(cost(n))));
    }
    if o.split != n.split && !(o.split.is_empty() && n.split.is_empty()) {
        out.push(format!("~ evidence split: {} → {}", if o.split.is_empty() { "—" } else { &o.split }, if n.split.is_empty() { "—" } else { &n.split }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha256_bytes;

    fn ctx() -> crate::profiles::Ctx {
        crate::profiles::Ctx::from_process()
    }

    fn lock_with(dir: &Path, name: &str, text: &str, vendored: Option<&str>) -> Lockfile {
        if let Some(v) = vendored {
            std::fs::create_dir_all(dir.join(v).parent().unwrap()).unwrap();
            std::fs::write(dir.join(v), text).unwrap();
        }
        Lockfile {
            instructions: vec![Instructions { name: name.into(), digest: Some(sha256_bytes(text.as_bytes())), scope: "project".into(), vendored: vendored.map(str::to_string), ..Default::default() }],
            ..Default::default()
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-lockdiff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn shows_the_text_behind_a_changed_digest() {
        let d = tmp("text");
        let old = lock_with(&d, "AGENTS.md", "Be brief.\nRun tests.\n", Some(".hlock/vendor/a/AGENTS.md"));
        let new = lock_with(&d, "AGENTS.md", "Be brief.\nRun the tests first.\n", Some(".hlock/vendor/b/AGENTS.md"));
        old.save(&d.join("harness.lock.prev")).unwrap();
        new.save(&d.join("harness.lock")).unwrap();
        let (o, n) = sides(&d.join("harness.lock"), None, None).unwrap();
        let v = diff(&o, &n, &d, &DiffOptions::default(), &ctx());
        let c = &v["components"][0];
        assert_eq!(c["change"], "~");
        assert_eq!(c["new_from"], "vendored");
        let f = &c["files"][0];
        assert_eq!(f["path"], "AGENTS.md");
        assert_eq!((f["added"].as_u64(), f["removed"].as_u64()), (Some(1), Some(1)));
        let hunks = f["hunks"].as_str().unwrap();
        assert!(hunks.contains("-Run tests.") && hunks.contains("+Run the tests first."), "{hunks}");
        assert_eq!(v["unresolved"], 0);
    }

    #[test]
    fn content_that_does_not_match_its_digest_is_not_shown() {
        let d = tmp("tampered");
        let old = lock_with(&d, "AGENTS.md", "one\n", Some(".hlock/vendor/a/AGENTS.md"));
        let new = lock_with(&d, "AGENTS.md", "two\n", Some(".hlock/vendor/b/AGENTS.md"));
        std::fs::write(d.join(".hlock/vendor/b/AGENTS.md"), "edited by hand\n").unwrap();
        let (o, n) = (Side { lock: old, label: "a".into(), dir: d.clone(), git: None }, Side { lock: new, label: "b".into(), dir: d.clone(), git: None });
        let v = diff(&o, &n, &d, &DiffOptions::default(), &ctx());
        let c = &v["components"][0];
        assert!(c.get("files").is_none());
        assert_eq!(c["unavailable"][0]["side"], "new");
        assert!(c["unavailable"][0]["reason"].as_str().unwrap().contains("don't match"));
        assert_eq!(v["unresolved"], 1);
    }

    #[test]
    fn hidden_characters_are_shown_and_flagged() {
        let d = tmp("hidden");
        let old = lock_with(&d, "AGENTS.md", "ok\n", Some(".hlock/vendor/a/AGENTS.md"));
        let new = lock_with(&d, "AGENTS.md", "ok\nsafe\u{202E}evil\n", Some(".hlock/vendor/b/AGENTS.md"));
        let (o, n) = (Side { lock: old, label: "a".into(), dir: d.clone(), git: None }, Side { lock: new, label: "b".into(), dir: d.clone(), git: None });
        let f = &diff(&o, &n, &d, &DiffOptions::default(), &ctx())["components"][0]["files"][0];
        assert!(f["hunks"].as_str().unwrap().contains("+safe<U+202E>evil"));
        assert!(f["hidden"].as_str().unwrap().starts_with("critical"));
    }

    #[test]
    fn a_skill_directory_diffs_file_by_file() {
        let d = tmp("skill");
        let mk = |v: &str, body: &str, extra: bool| {
            let dir = d.join(v);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), body).unwrap();
            if extra {
                std::fs::write(dir.join("notes.md"), "new file\n").unwrap();
            }
            Skill { name: "review".into(), scope: Some("project".into()), digest: Some(crate::capture::tree_digest(&dir)), vendored: Some(v.into()), ..Default::default() }
        };
        let old = Lockfile { skill: vec![mk("v/a", "step 1\n", false)], ..Default::default() };
        let new = Lockfile { skill: vec![mk("v/b", "step 1\nstep 2\n", true)], ..Default::default() };
        let (o, n) = (Side { lock: old, label: "a".into(), dir: d.clone(), git: None }, Side { lock: new, label: "b".into(), dir: d.clone(), git: None });
        let files = diff(&o, &n, &d, &DiffOptions::default(), &ctx())["components"][0]["files"].as_array().unwrap().clone();
        let by: BTreeMap<&str, &str> = files.iter().map(|f| (f["path"].as_str().unwrap(), f["status"].as_str().unwrap())).collect();
        assert_eq!(by, BTreeMap::from([("review/SKILL.md", "modified"), ("review/notes.md", "added")]));
    }

    #[test]
    fn evidence_lines_report_what_moved() {
        let mut a = Lockfile::default();
        let mut b = Lockfile::default();
        a.evidence.success_rate.insert("mean".into(), 0.5);
        b.evidence.success_rate.insert("mean".into(), 0.75);
        a.evidence.trials = 4;
        b.evidence.trials = 4;
        assert_eq!(evidence(&a, &b), vec!["~ evidence success-rate: 0.50 → 0.75  (n=4 → 4)".to_string()]);
    }

    #[test]
    fn a_git_revision_side_reads_the_lock_and_its_files_from_git() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let d = tmp("git");
        let run = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&d).args(args).output().unwrap().status.success(), "git {args:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(d.join("AGENTS.md"), "old text\n").unwrap();
        lock_with(&d, "AGENTS.md", "old text\n", None).save(&d.join("harness.lock")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        std::fs::write(d.join("AGENTS.md"), "new text\n").unwrap();
        lock_with(&d, "AGENTS.md", "new text\n", None).save(&d.join("harness.lock")).unwrap();
        let (o, n) = sides(&d.join("harness.lock"), Some("HEAD"), None).unwrap();
        assert!(o.label.starts_with("HEAD:"));
        let v = diff(&o, &n, &d, &DiffOptions::default(), &ctx());
        let c = &v["components"][0];
        assert_eq!((c["old_from"].as_str(), c["new_from"].as_str()), (Some("project"), Some("project")));
        let hunks = c["files"][0]["hunks"].as_str().unwrap();
        assert!(hunks.contains("-old text") && hunks.contains("+new text"), "{hunks}");
        assert!(sides(&d.join("harness.lock"), Some("--output=/tmp/x"), None).is_err());
    }
}
