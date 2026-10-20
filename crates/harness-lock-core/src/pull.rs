//! `hlock pull`: fetch a pinned lock (and the files it vendors) from a git repository
//! into the store (`~/.hlock/pulls/<name>/`, laid out like a repo) or into a project lock
//! file, with its origin recorded in a `[pulled]` table.
//!
//! Fetching shells out to `git` (shallow, sparse), so private repos, any host, and
//! branches, tags and commits work the way they do for `git` itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::{json, Value};

use crate::capture::{diff_locks, VENDOR_DIR};
use crate::error::{Error, Result};
use crate::fsutil::{plain_token, safe_rel};
use crate::lockfile::Lockfile;

/// Where a lock lives: a parsed `github:` / `gitlab:` / `git:` spec or browser URL.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Source {
    /// The source as typed, without credentials (`https://user:token@…` loses `user:token@`).
    pub spec: String,
    /// The git remote to fetch, without credentials.
    pub url: String,
    /// The remote as given, credentials included: used only to fetch, never stored or shown.
    #[serde(skip)]
    pub fetch_url: String,
    /// Branch, tag or commit; `None` for the remote's default branch.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// A lock file or directory inside the repo (no leading or trailing `/`).
    pub path: Option<String>,
    /// The default lock name: the owner (or top-level group, or repo for `git:`).
    pub owner: String,
}

/// True if `arg` names a remote source rather than a local lock path.
pub fn is_source(arg: &str) -> bool {
    ["github:", "gitlab:", "git:", "https://", "http://"].iter().any(|p| arg.starts_with(p))
}

pub(crate) fn base_url(var: &str, default: &str) -> String {
    std::env::var(var).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string()).trim_end_matches('/').to_string()
}

/// The host of a URL, without a `user[:password]@` before it.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or("");
    authority.rsplit_once('@').map_or(authority, |(_, h)| h)
}

/// `url` without credentials: all of `user[:password]@` for http(s), where a user name
/// is often a token, else just the `:password` (an ssh user name says who to log in as).
/// Other forms (`git@host:path`) are returned as they are.
pub fn strip_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let end = rest.find('/').unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let Some((user, host)) = authority.rsplit_once('@') else { return url.to_string() };
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        format!("{scheme}://{host}{tail}")
    } else {
        let user = user.split(':').next().unwrap_or("");
        format!("{scheme}://{user}@{host}{tail}")
    }
}

/// A spec without credentials (see [`strip_credentials`]).
fn strip_spec_credentials(spec: &str) -> String {
    match spec.strip_prefix("git:") {
        Some(rest) => format!("git:{}", strip_credentials(rest)),
        None => strip_credentials(spec),
    }
}

fn clean_path(p: &str) -> Option<String> {
    let p = p.trim_matches('/');
    (!p.is_empty()).then(|| p.to_string())
}

fn bad(spec: &str, why: &str) -> Error {
    Error::Pull(format!("can't read source {:?}: {why}", strip_spec_credentials(spec)))
}

/// Check what `git` gets from a parsed source: nothing it could take for an option, no
/// transport helper (`ext::…`), and a path that stays inside the repository.
fn checked(src: Source) -> Result<Source> {
    let spec = src.spec.clone();
    let fetch_url = src.url.clone();
    if fetch_url.starts_with('-') || fetch_url.contains("::") || fetch_url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad(&spec, "the repository URL can't start with '-' or contain '::' or spaces"));
    }
    if let Some(r) = &src.reference {
        if !plain_token(r) {
            return Err(bad(&spec, &format!("{r:?} isn't a branch, tag or commit (letters, digits and ._/@+- only, not starting with '-')")));
        }
    }
    if let Some(p) = &src.path {
        if !safe_rel(p) {
            return Err(bad(&spec, &format!("{p:?} isn't a path inside the repository")));
        }
    }
    let (url, spec) = (strip_credentials(&fetch_url), strip_spec_credentials(&src.spec));
    Ok(Source { url, spec, fetch_url, ..src })
}

/// Split `REPO[@REF]` at the first `@` (owners, groups and repos can't contain one).
fn split_ref(s: &str) -> (&str, Option<String>) {
    match s.split_once('@') {
        Some((a, r)) if !r.is_empty() => (a, Some(r.to_string())),
        Some((a, _)) => (a, None),
        None => (s, None),
    }
}

/// Parse a source spec (see `specs/pull.md` §4.2).
pub fn parse(spec: &str) -> Result<Source> {
    checked(parse_raw(spec)?)
}

/// [`parse`] without the checks; `url` is the remote as given and `fetch_url` is empty.
fn parse_raw(spec: &str) -> Result<Source> {
    let spec = spec.trim();
    let github = base_url("HLOCK_GITHUB_URL", "https://github.com");
    let gitlab = base_url("HLOCK_GITLAB_URL", "https://gitlab.com");
    if let Some(rest) = spec.strip_prefix("github:") {
        let (body, reference) = split_ref(rest);
        let (body, mut path) = match body.split_once("//") {
            Some((b, p)) => (b, clean_path(p)),
            None => (body, None),
        };
        let parts: Vec<&str> = body.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() < 2 {
            return Err(bad(spec, "expected github:OWNER/REPO"));
        }
        if parts.len() > 2 {
            path = clean_path(&parts[2..].join("/"));
        }
        let repo = parts[1].trim_end_matches(".git");
        return Ok(Source { spec: spec.into(), url: format!("{github}/{}/{repo}.git", parts[0]), fetch_url: String::new(), reference, path, owner: parts[0].into() });
    }
    if let Some(rest) = spec.strip_prefix("gitlab:") {
        let (body, reference) = split_ref(rest);
        let (body, path) = match body.split_once("//") {
            Some((b, p)) => (b, clean_path(p)),
            None => (body, None),
        };
        let parts: Vec<&str> = body.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() < 2 {
            return Err(bad(spec, "expected gitlab:GROUP/REPO"));
        }
        let repo = parts.join("/");
        return Ok(Source { spec: spec.into(), url: format!("{gitlab}/{}.git", repo.trim_end_matches(".git")), fetch_url: String::new(), reference, path, owner: parts[0].into() });
    }
    if let Some(rest) = spec.strip_prefix("git:") {
        // The ref follows the last `@` that comes after every `/` and `:` (so `git@host:x` isn't one).
        let (mut url, mut reference) = (rest.to_string(), None);
        if let Some(at) = rest.rfind('@') {
            let after_sep = rest.rfind(['/', ':']).map_or(true, |i| at > i);
            if after_sep && at + 1 < rest.len() {
                reference = Some(rest[at + 1..].to_string());
                url = rest[..at].to_string();
            }
        }
        let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
        let mut path = None;
        if let Some(i) = url[scheme_end..].find("//") {
            path = clean_path(&url[scheme_end + i + 2..]);
            url.truncate(scheme_end + i);
        }
        let url = url.trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err(bad(spec, "expected git:URL"));
        }
        let name = url.rsplit(['/', ':']).next().unwrap_or(&url).trim_end_matches(".git").to_string();
        return Ok(Source { spec: spec.into(), url, fetch_url: String::new(), reference, path, owner: name });
    }
    if spec.starts_with("https://") || spec.starts_with("http://") {
        let host = host_of(spec);
        let after_host = spec.split_once("://").map(|(_, r)| r).unwrap_or("").splitn(2, '/').nth(1).unwrap_or("");
        let scheme_host = &spec[..spec.len() - after_host.len()];
        let segs: Vec<&str> = after_host.split(['?', '#']).next().unwrap_or("").trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        // `…/-/blob|tree/REF/PATH` is GitLab's layout on any host.
        let dash = segs.iter().position(|s| *s == "-");
        if dash.is_some() || host == host_of(&gitlab) || host == "gitlab.com" {
            let end = dash.unwrap_or(segs.len());
            if end < 2 {
                return Err(bad(spec, "expected a GitLab project URL"));
            }
            let repo = segs[..end].join("/");
            let (reference, path) = blob_tree(&segs[end.min(segs.len())..], dash.is_some());
            return Ok(Source { spec: spec.into(), url: format!("{scheme_host}{}.git", repo.trim_end_matches(".git")), fetch_url: String::new(), reference, path, owner: segs[0].into() });
        }
        if host == host_of(&github) || host == "github.com" {
            if segs.len() < 2 {
                return Err(bad(spec, "expected a GitHub repository URL"));
            }
            let (reference, path) = blob_tree(&segs[2..], false);
            let repo = segs[1].trim_end_matches(".git");
            return Ok(Source { spec: spec.into(), url: format!("{scheme_host}{}/{repo}.git", segs[0]), fetch_url: String::new(), reference, path, owner: segs[0].into() });
        }
        let name = segs.last().map(|s| s.trim_end_matches(".git").to_string()).unwrap_or_default();
        return Ok(Source { spec: spec.into(), url: spec.trim_end_matches('/').into(), fetch_url: String::new(), reference: None, path: None, owner: name });
    }
    Err(bad(spec, "expected github:OWNER/REPO, gitlab:GROUP/REPO, git:URL, or a repository URL"))
}

/// `[-,] blob|tree, REF, PATH…` → (ref, path). A ref with `/` in it can't be told apart
/// from the path in a URL; the first segment is taken as the ref.
fn blob_tree(segs: &[&str], skip_dash: bool) -> (Option<String>, Option<String>) {
    let segs = if skip_dash { segs.get(1..).unwrap_or(&[]) } else { segs };
    match segs {
        [kind, reference, rest @ ..] if *kind == "blob" || *kind == "tree" => (Some(reference.to_string()), clean_path(&rest.join("/"))),
        _ => (None, None),
    }
}

/// The lock name for a pulled lock: `owner` plus `-x` for a remote `harness.x.lock`.
pub fn default_name(owner: &str, remote_lock: &str) -> String {
    let stem = remote_lock.strip_suffix(".lock").unwrap_or(remote_lock);
    let base = match stem.strip_prefix("harness.") {
        Some(x) if !x.is_empty() => format!("{owner}-{x}"),
        _ => owner.to_string(),
    };
    sanitize(&base)
}

pub(crate) fn sanitize(s: &str) -> String {
    let out: String = s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    let out = out.trim_matches(['-', '.']).to_string();
    if out.is_empty() { "pulled".into() } else { out }
}

// --------------------------------------------------------------------------- //
// git
// --------------------------------------------------------------------------- //

/// How long one git command may take.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        // no transport helpers (`ext::`, `fd::`) from a source someone else wrote
        .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
        .stdin(Stdio::null());
    let limits = crate::process::Limits::timeout(GIT_TIMEOUT);
    let out = crate::process::run(cmd, &limits).map_err(|e| Error::Pull(format!("can't run git: {e} (hlock pull needs git on PATH)")))?;
    if !out.success {
        let err = if out.timed_out { format!("timed out after {}s", GIT_TIMEOUT.as_secs()) } else { redact_urls(out.stderr_text().trim()) };
        return Err(Error::Pull(format!("git {} failed: {err}", args.first().unwrap_or(&""))));
    }
    Ok(out.stdout_text().trim().to_string())
}

/// `text` with credentials taken out of every URL in it (git echoes the remote it failed on).
pub(crate) fn redact_urls(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let start = rest[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || "+.-".contains(c))).map_or(0, |j| j + 1);
        let end = rest[i..].find(|c: char| c.is_whitespace() || "'\"`<>".contains(c)).map_or(rest.len(), |j| i + j);
        out.push_str(&rest[..start]);
        out.push_str(&strip_credentials(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A scratch checkout, removed on drop.
struct Checkout(PathBuf);

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir() -> Result<Checkout> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("hlock-pull-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| crate::error::io_at(&dir, e))?;
    Ok(Checkout(dir))
}

fn in_repo(path: Option<&str>, rel: &str) -> String {
    match path {
        Some(p) => format!("/{p}/{rel}"),
        None => format!("/{rel}"),
    }
}

/// Shallow-fetch `src` and check out only its lock files. Returns the resolved commit.
fn fetch(src: &Source, dir: &Path) -> Result<String> {
    git(dir, &["init", "-q"])?;
    // `--` keeps a URL or ref from being read as an option (parse rejects a leading '-' too)
    let url = if src.fetch_url.is_empty() { &src.url } else { &src.fetch_url };
    git(dir, &["remote", "add", "--", "origin", url])?;
    let reference = src.reference.as_deref().unwrap_or("HEAD");
    if let Err(e) = git(dir, &["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", "--", reference]) {
        let hint = if src.reference.is_some() {
            format!("check that {reference:?} is a branch, tag, or full commit SHA, and that `git ls-remote {}` works", src.url)
        } else {
            format!("if the repository is private, check that `git ls-remote {}` works, or use a git: source with an SSH remote", src.url)
        };
        let e = match e {
            Error::Pull(m) => m,
            e => e.to_string(),
        };
        return Err(Error::Pull(format!("can't fetch {}: {e}\n{hint}", src.spec)));
    }
    // A lock path selects one file; a directory (or the root) looks for harness locks in it.
    let patterns: Vec<String> = match src.path.as_deref() {
        Some(p) if p.ends_with(".lock") => vec![format!("/{p}")],
        p => vec![in_repo(p, "harness.lock"), in_repo(p, "harness.*.lock")],
    };
    let mut args = vec!["sparse-checkout", "set", "--no-cone"];
    args.extend(patterns.iter().map(String::as_str));
    git(dir, &args)?;
    git(dir, &["checkout", "-q", "FETCH_HEAD"])?;
    git(dir, &["rev-parse", "HEAD"])
}

/// The lock to pull from a checkout, and the other harness locks next to it.
fn choose_lock(src: &Source, dir: &Path) -> Result<(PathBuf, Vec<String>)> {
    if let Some(p) = src.path.as_deref().filter(|p| p.ends_with(".lock")) {
        let f = dir.join(p);
        if !f.is_file() {
            return Err(Error::Lockfile(format!("{} has no {p}", src.spec)));
        }
        return Ok((f, Vec::new()));
    }
    let base = src.path.as_deref().map(|p| dir.join(p)).unwrap_or_else(|| dir.to_path_buf());
    let mut locks: Vec<String> = std::fs::read_dir(&base)
        .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().map(str::to_string)).collect())
        .unwrap_or_default();
    locks.retain(|n| n == "harness.lock" || (n.starts_with("harness.") && n.ends_with(".lock") && n.len() > "harness..lock".len()));
    locks.sort();
    let at = src.path.as_deref().map(|p| format!("{p}/")).unwrap_or_default();
    if locks.iter().any(|n| n == "harness.lock") {
        let others = locks.into_iter().filter(|n| n != "harness.lock").map(|n| format!("{at}{n}")).collect();
        return Ok((base.join("harness.lock"), others));
    }
    match locks.len() {
        0 => Err(Error::Lockfile(format!("{} has no harness.lock{}", src.spec, if at.is_empty() { String::new() } else { format!(" in {at}") }))),
        1 => Ok((base.join(&locks[0]), Vec::new())),
        _ => Err(Error::Lockfile(format!(
            "{} has {} locks; pick one:\n{}",
            src.spec,
            locks.len(),
            locks.iter().map(|n| format!("  hlock pull {}", with_path(src, &format!("{at}{n}")))).collect::<Vec<_>>().join("\n")
        ))),
    }
}

/// `src`'s spec pointing at `path` in the repo instead (for "pull this one" hints).
pub fn with_path(src: &Source, path: &str) -> String {
    let reference = src.reference.as_deref().map(|r| format!("@{r}")).unwrap_or_default();
    if let Some(rest) = src.spec.strip_prefix("github:") {
        let repo: Vec<&str> = split_ref(rest).0.split("//").next().unwrap_or("").split('/').filter(|s| !s.is_empty()).take(2).collect();
        return format!("github:{}/{path}{reference}", repo.join("/"));
    }
    if src.spec.starts_with("gitlab:") || src.spec.starts_with("git:") {
        let prefix = if src.spec.starts_with("gitlab:") { "gitlab:" } else { "git:" };
        let repo = if prefix == "gitlab:" {
            split_ref(&src.spec[prefix.len()..]).0.split("//").next().unwrap_or("").trim_matches('/').to_string()
        } else {
            src.url.clone()
        };
        return format!("{prefix}{repo}//{path}{reference}");
    }
    format!("git:{}//{path}{reference}", src.url)
}

// --------------------------------------------------------------------------- //
// pull
// --------------------------------------------------------------------------- //

/// Another harness lock next to the pulled one, and the source that pulls it.
#[derive(Debug, Clone, Serialize)]
pub struct OtherLock {
    pub path: String,
    pub source: String,
}

/// What [`pull`] did.
#[derive(Debug, Clone, Serialize)]
pub struct PullOutcome {
    pub lock: Lockfile,
    pub path: PathBuf,
    /// The store name, or the lock's vendor namespace for a project file.
    pub name: String,
    /// True when written to the store (not a project file).
    pub stored: bool,
    pub source: Source,
    pub commit: String,
    /// The commit of the lock this pull replaced (a refresh), if any.
    pub previous_commit: Option<String>,
    /// Changes from the replaced lock (empty for a first pull).
    pub diff: Vec<String>,
    pub vendored: Vec<String>,
    /// Other harness locks in the same repo directory.
    pub others: Vec<OtherLock>,
    pub warnings: Vec<String>,
}

/// The new home for a vendored path: `.hlock/vendor/[<ns>/]<harness>/<rest>`, where
/// `rest` follows `<harness>/` in the old path (or `.hlock/vendor/`, failing that).
fn relocate(old: &str, ns: Option<&str>, harness: &str) -> String {
    let prefix = format!("{VENDOR_DIR}/");
    let inner = old.strip_prefix(&prefix).unwrap_or(old);
    let parts: Vec<&str> = inner.split('/').collect();
    let rest = match parts.iter().position(|p| *p == harness) {
        Some(i) if i <= 1 => parts[i + 1..].join("/"),
        _ => inner.to_string(),
    };
    match ns {
        Some(ns) => format!("{VENDOR_DIR}/{ns}/{harness}/{rest}"),
        None => format!("{VENDOR_DIR}/{harness}/{rest}"),
    }
}

/// A sibling of `p` for staging or setting aside (`.<name>.hlock-<tag>-<pid>`).
fn beside(p: &Path, tag: &str) -> PathBuf {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    p.with_file_name(format!(".{name}.hlock-{tag}-{}", std::process::id()))
}

/// Put the staged vendor directory `staged` in place of `vdir`: set the old one aside,
/// rename the new one in, then remove the old one, so a failed pull leaves the old files.
fn swap_in(staged: &Path, vdir: &Path) -> Result<()> {
    let old = beside(vdir, "old");
    let _ = std::fs::remove_dir_all(&old);
    let had_old = std::fs::symlink_metadata(vdir).is_ok();
    if had_old {
        std::fs::rename(vdir, &old).map_err(|e| crate::error::io_at(vdir, e))?;
    }
    if std::fs::symlink_metadata(staged).is_ok() {
        if let Err(e) = std::fs::rename(staged, vdir) {
            if had_old {
                let _ = std::fs::rename(&old, vdir);
            }
            return Err(crate::error::io_at(vdir, e));
        }
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old).or_else(|_| std::fs::remove_file(&old));
    }
    Ok(())
}

pub(crate) fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The `[pulled]` table of a lock, if it was pulled.
pub fn pulled_from(lock: &Lockfile) -> Option<&Value> {
    lock.pulled.as_ref().filter(|v| v.get("from").and_then(Value::as_str).is_some())
}

fn same_repo(a: &str, b: &str) -> bool {
    let norm = |u: &str| u.trim_end_matches('/').trim_end_matches(".git").to_lowercase();
    norm(a) == norm(b)
}

// --------------------------------------------------------------------------- //
// the store: ~/.hlock/pulls/<name>/
// --------------------------------------------------------------------------- //

/// hlock's global directory: `HLOCK_HOME`, else `~/.hlock`.
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HLOCK_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    home.join(".hlock")
}

/// Where pulled locks live: one directory per name, laid out like the repo it came from
/// (`<name>/harness.lock` and `<name>/.hlock/vendor/<harness>/`).
pub fn store_dir() -> PathBuf {
    home_dir().join("pulls")
}

/// Where `launch` puts the isolated config roots for a stored lock.
pub fn env_dir(name: &str) -> PathBuf {
    home_dir().join("env").join(name)
}

fn check_name(n: &str) -> Result<()> {
    let s = sanitize(n);
    if s != n {
        return Err(Error::Lockfile(format!("{n:?} isn't a valid name: use only lowercase letters, digits, '.', '_' and '-' (e.g. {s:?})")));
    }
    Ok(())
}

/// The lock of the stored pull `name`, if there is one.
pub fn find(name: &str) -> Option<PathBuf> {
    if check_name(name).is_err() {
        return None;
    }
    let p = store_dir().join(name).join(crate::LOCKFILE_PATH);
    p.is_file().then_some(p)
}

/// A pulled lock in the store.
#[derive(Debug, Clone, Serialize)]
pub struct Stored {
    pub name: String,
    pub path: PathBuf,
    pub harness: String,
    pub version: Option<String>,
    pub pulled: Option<Value>,
    /// Set when the lock can't be read.
    pub error: Option<String>,
}

/// Every lock in the store, by name.
pub fn list() -> Vec<Stored> {
    let mut out: Vec<Stored> = std::fs::read_dir(store_dir())
        .map(|rd| rd.filter_map(|e| e.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let path = e.path().join(crate::LOCKFILE_PATH);
            if !path.is_file() {
                return None;
            }
            Some(match Lockfile::load(&path) {
                Ok(l) => Stored { name, harness: l.harness.name.clone(), version: l.harness.version.clone(), pulled: l.pulled.clone(), path, error: None },
                Err(e) => Stored { name, path, harness: String::new(), version: None, pulled: None, error: Some(e.to_string()) },
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Delete the stored pull `name` and its launch environments.
pub fn remove(name: &str) -> Result<PathBuf> {
    check_name(name)?;
    let dir = store_dir().join(name);
    if !dir.join(crate::LOCKFILE_PATH).is_file() {
        return Err(Error::Lockfile(format!("no pulled lock named {name:?}")));
    }
    std::fs::remove_dir_all(&dir)?;
    let _ = std::fs::remove_dir_all(env_dir(name));
    Ok(dir)
}

// --------------------------------------------------------------------------- //
// pull
// --------------------------------------------------------------------------- //

/// Where [`pull`] writes.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// The store, as `<store>/<name>/harness.lock` (`None`: the default name).
    Store(Option<String>),
    /// A lock file in a project; its vendored files go in that lock's vendor namespace.
    File(PathBuf),
}

/// Pull the lock at `spec` into `target`. Replaces an existing lock there only when it
/// was pulled from the same repository, or with `force`.
pub fn pull(spec: &str, target: &Target, force: bool) -> Result<PullOutcome> {
    let src = parse(spec)?;
    if let Target::Store(Some(n)) = target {
        check_name(n)?;
    }
    let scratch = scratch_dir()?;
    let commit = fetch(&src, &scratch.0)?;
    let (remote_lock, others) = choose_lock(&src, &scratch.0)?;
    // a lock that's a symlink (or under one) could point anywhere on this machine
    if !std::fs::symlink_metadata(&remote_lock).is_ok_and(|m| m.is_file()) || !crate::fsutil::within(&scratch.0, &remote_lock) {
        return Err(Error::Pull(format!("{}'s lock is a symlink or outside the repository; refusing to pull it", src.spec)));
    }
    // load() validates the lock: names that become paths, values that become commands
    let mut lock = Lockfile::load(&remote_lock)?;
    let remote_file = remote_lock.file_name().and_then(|n| n.to_str()).unwrap_or("harness.lock").to_string();
    let (name, out, ns) = match target {
        Target::Store(n) => {
            let name = n.clone().unwrap_or_else(|| default_name(&src.owner, &remote_file));
            let out = store_dir().join(&name).join(crate::LOCKFILE_PATH);
            (name, out, None)
        }
        Target::File(f) => {
            let ns = crate::capture::vendor_namespace(f);
            (ns.clone().unwrap_or_else(|| "harness".into()), f.clone(), ns)
        }
    };
    let shown = match target {
        Target::Store(_) => format!("a pulled lock named {name:?}"),
        Target::File(f) => f.display().to_string(),
    };
    let previous = if out.exists() { Lockfile::load(&out).ok() } else { None };
    if out.exists() && !force {
        let prev_url = previous.as_ref().and_then(pulled_from).and_then(|p| p.get("url")).and_then(Value::as_str);
        let other_name = if matches!(target, Target::Store(_)) { "pass --as NAME to keep both" } else { "pass another -o FILE to keep both" };
        match prev_url {
            Some(u) if same_repo(u, &src.url) => {}
            Some(u) => {
                return Err(Error::Lockfile(format!("{shown} was pulled from {u}, not {}; {other_name}, or --force to replace it", src.url)))
            }
            None => return Err(Error::Lockfile(format!("{shown} exists and wasn't pulled; {other_name}, or --force to replace it"))),
        }
    }

    // Fetch the vendored paths too, then copy them into the target's vendor directory.
    let others = others.into_iter().map(|p| OtherLock { source: with_path(&src, &p), path: p }).collect();
    let lock_dir_rel = remote_lock.parent().and_then(|p| p.strip_prefix(&scratch.0).ok()).map(Path::to_path_buf).unwrap_or_default();
    let harness = lock.harness.name.clone();
    // both name the vendor directory that's replaced below
    crate::fsutil::check_plain_name("the pulled lock's harness name", &harness)?;
    if let Some(ns) = &ns {
        crate::fsutil::check_plain_name("the lock file's vendor namespace", ns)?;
    }
    let mut paths: Vec<String> = Vec::new();
    paths.extend(lock.skill.iter().filter_map(|s| s.vendored.clone()));
    paths.extend(lock.instructions.iter().filter_map(|i| i.vendored.clone()));
    paths.extend(lock.config.iter().filter_map(|c| c.vendored.clone()));
    if let Some(p) = paths.iter().find(|p| !safe_rel(p)) {
        return Err(Error::Lockfile(format!("{} vendors {p:?}, which isn't a path inside the repository; refusing to pull it", src.spec)));
    }
    if !paths.is_empty() {
        let rel = |p: &str| format!("/{}", lock_dir_rel.join(p).to_string_lossy().trim_start_matches('/'));
        let patterns: Vec<String> = paths.iter().map(|p| rel(p)).collect();
        let mut args = vec!["sparse-checkout", "add"];
        args.extend(patterns.iter().map(String::as_str));
        git(&scratch.0, &args)?;
    }
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let vdir = match (&ns, target) {
        (_, Target::Store(_)) => dir.join(VENDOR_DIR),
        (Some(ns), _) => dir.join(VENDOR_DIR).join(ns),
        (None, _) => dir.join(VENDOR_DIR).join(&harness),
    };
    // Copy into a staging directory beside the vendor directory, and swap it in only once
    // everything is copied, so a failed pull leaves the old files.
    let staged = beside(&vdir, "new");
    let _ = std::fs::remove_dir_all(&staged);
    let mut warnings = Vec::new();
    let mut vendored = Vec::new();
    let lock_root = scratch.0.join(&lock_dir_rel);
    let mut move_one = |slot: &mut Option<String>, what: String| -> Result<()> {
        let Some(old) = slot.clone() else { return Ok(()) };
        let from = lock_root.join(&old);
        let Ok(meta) = std::fs::symlink_metadata(&from) else {
            warnings.push(format!("{what}: {old} isn't in the repository; sync will treat it as manual"));
            *slot = None;
            return Ok(());
        };
        // a symlink in the repository (or a symlinked parent) could point anywhere here
        if meta.file_type().is_symlink() || !crate::fsutil::within(&scratch.0, &from) {
            warnings.push(format!("{what}: {old} is a symlink in the repository; skipped (sync will treat it as manual)"));
            *slot = None;
            return Ok(());
        }
        let new = relocate(&old, ns.as_deref(), &harness);
        let dest = dir.join(&new);
        let inside = crate::fsutil::normalize(&dest);
        let Ok(rel) = inside.strip_prefix(crate::fsutil::normalize(&vdir)) else {
            return Err(Error::Pull(format!("{what}: {new} isn't inside {}; refusing to write there", vdir.display())));
        };
        crate::fsutil::check_within(&what, &vdir, &dest)?;
        crate::fsutil::copy_tree(&from, &staged.join(rel)).map_err(|e| crate::error::at(&from, e))?;
        vendored.push(new.clone());
        *slot = Some(new);
        Ok(())
    };
    let copied = (|| -> Result<()> {
        for s in lock.skill.iter_mut() {
            move_one(&mut s.vendored, format!("skill {}", s.name))?;
        }
        for i in lock.instructions.iter_mut() {
            move_one(&mut i.vendored, format!("instructions {}", i.name))?;
        }
        for c in lock.config.iter_mut() {
            move_one(&mut c.vendored, format!("config {}", c.path))?;
        }
        Ok(())
    })();
    if let Err(e) = copied.and_then(|_| swap_in(&staged, &vdir)) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }

    let repo_path = lock_dir_rel.join(&remote_file).to_string_lossy().into_owned();
    let mut pulled = json!({ "from": src.spec, "url": src.url, "path": repo_path, "commit": commit, "at": now_rfc3339() });
    if let Some(r) = &src.reference {
        pulled["ref"] = json!(r);
    }
    lock.pulled = Some(pulled);
    let previous_commit = previous.as_ref().and_then(pulled_from).and_then(|p| p.get("commit")).and_then(Value::as_str).map(str::to_string);
    let diff = previous.as_ref().map(|p| diff_locks(p, &lock)).unwrap_or_default();
    lock.save(&out)?;
    Ok(PullOutcome { lock, path: out, name, stored: matches!(target, Target::Store(_)), source: src, commit, previous_commit, diff, vendored, others, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Source {
        parse(s).unwrap()
    }

    #[test]
    fn github_specs() {
        let s = p("github:alice/agent-setup");
        assert_eq!(s.url, "https://github.com/alice/agent-setup.git");
        assert_eq!((s.reference, s.path, s.owner.as_str()), (None, None, "alice"));
        let s = p("github:alice/agent-setup/team/harness.fast.lock@feature/x");
        assert_eq!(s.path.as_deref(), Some("team/harness.fast.lock"));
        assert_eq!(s.reference.as_deref(), Some("feature/x"));
        assert_eq!(p("github:alice/agent-setup//team").path.as_deref(), Some("team"));
        assert!(parse("github:alice").is_err());
    }

    #[test]
    fn gitlab_specs() {
        let s = p("gitlab:acme/platform/agents//team@main");
        assert_eq!(s.url, "https://gitlab.com/acme/platform/agents.git");
        assert_eq!((s.path.as_deref(), s.reference.as_deref(), s.owner.as_str()), (Some("team"), Some("main"), "acme"));
        assert_eq!(p("gitlab:acme/agents").path, None);
    }

    #[test]
    fn git_specs() {
        let s = p("git:git@github.com:alice/private.git");
        assert_eq!((s.url.as_str(), s.reference.as_deref(), s.owner.as_str()), ("git@github.com:alice/private.git", None, "private"));
        let s = p("git:git@github.com:alice/private.git@v1");
        assert_eq!((s.url.as_str(), s.reference.as_deref()), ("git@github.com:alice/private.git", Some("v1")));
        let s = p("git:https://git.example.com/a/b.git//sub/harness.lock@abc123");
        assert_eq!(s.url, "https://git.example.com/a/b.git");
        assert_eq!((s.path.as_deref(), s.reference.as_deref()), (Some("sub/harness.lock"), Some("abc123")));
        let s = p("git:file:///tmp/repo");
        assert_eq!((s.url.as_str(), s.path.as_deref(), s.owner.as_str()), ("file:///tmp/repo", None, "repo"));
    }

    #[test]
    fn browser_urls() {
        let s = p("https://github.com/alice/agent-setup");
        assert_eq!((s.url.as_str(), s.reference, s.path), ("https://github.com/alice/agent-setup.git", None, None));
        let s = p("https://github.com/alice/agent-setup/blob/main/harness.fast.lock");
        assert_eq!((s.reference.as_deref(), s.path.as_deref()), (Some("main"), Some("harness.fast.lock")));
        let s = p("https://gitlab.com/acme/platform/agents/-/tree/main/team");
        assert_eq!(s.url, "https://gitlab.com/acme/platform/agents.git");
        assert_eq!((s.reference.as_deref(), s.path.as_deref(), s.owner.as_str()), (Some("main"), Some("team"), "acme"));
        let s = p("https://gitlab.example.com/g/r/-/blob/v2/harness.lock");
        assert_eq!((s.url.as_str(), s.reference.as_deref()), ("https://gitlab.example.com/g/r.git", Some("v2")));
        assert_eq!(p("https://git.example.com/a/b.git").url, "https://git.example.com/a/b.git");
        assert!(parse("alice/agent-setup").is_err());
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(default_name("alice", "harness.lock"), "alice");
        assert_eq!(default_name("Alice", "harness.fast.lock"), "alice-fast");
        assert_eq!(relocate(".hlock/vendor/claude-code/skills/x", Some("alice"), "claude-code"), ".hlock/vendor/alice/claude-code/skills/x");
        assert_eq!(relocate(".hlock/vendor/fast/pi/AGENTS.md", None, "pi"), ".hlock/vendor/pi/AGENTS.md");
        assert_eq!(relocate("vendor/x", Some("a"), "pi"), ".hlock/vendor/a/pi/vendor/x");
        assert!(safe_rel(".hlock/vendor/x") && !safe_rel("../x") && !safe_rel("/etc/passwd") && !safe_rel("a/../../b"));
        let src = p("github:alice/agent-setup@v2");
        assert_eq!(with_path(&src, "harness.fast.lock"), "github:alice/agent-setup/harness.fast.lock@v2");
        assert_eq!(with_path(&p("gitlab:acme/p/agents"), "harness.x.lock"), "gitlab:acme/p/agents//harness.x.lock");
        assert!(now_rfc3339().ends_with('Z') && now_rfc3339().len() == 20);
    }

    #[test]
    fn option_like_refs_and_urls_are_refused() {
        for spec in ["github:alice/setup@--upload-pack=touch /tmp/x", "github:alice/setup@--upload-pack=x", "git:--upload-pack=x", "git:ext::sh -c touch% /tmp/x", "github:alice/setup//../../etc"] {
            assert!(matches!(parse(spec), Err(Error::Pull(_))), "{spec}");
        }
        let s = p("github:alice/setup@feature/x");
        assert_eq!(s.reference.as_deref(), Some("feature/x"));
    }

    #[test]
    fn credentials_are_kept_out_of_what_is_stored_and_shown() {
        let s = p("https://user:ghp_secret@github.com/alice/agent-setup");
        assert_eq!((s.url.as_str(), s.spec.as_str(), s.owner.as_str()), ("https://github.com/alice/agent-setup.git", "https://github.com/alice/agent-setup", "alice"));
        assert_eq!(s.fetch_url, "https://user:ghp_secret@github.com/alice/agent-setup.git");
        // a token as the user name, and GitHub still recognized (ref and path read from the URL)
        let s = p("https://ghp_secret@github.com/alice/agent-setup/blob/main/harness.lock");
        assert_eq!((s.url.as_str(), s.reference.as_deref(), s.path.as_deref()), ("https://github.com/alice/agent-setup.git", Some("main"), Some("harness.lock")));
        assert!(!s.spec.contains("ghp_") && s.fetch_url.contains("ghp_secret@"));
        let s = p("git:https://ghp_secret@git.example.com/a/b.git@v1");
        assert_eq!((s.url.as_str(), s.spec.as_str(), s.reference.as_deref()), ("https://git.example.com/a/b.git", "git:https://git.example.com/a/b.git@v1", Some("v1")));
        // an ssh user name stays (it says who to log in as), a password doesn't
        assert_eq!(strip_credentials("ssh://git:pw@host/x.git"), "ssh://git@host/x.git");
        assert_eq!(p("git:git@github.com:alice/private.git").url, "git@github.com:alice/private.git");
        assert!(!serde_json::to_string(&p("https://u:ghp_secret@github.com/a/b")).unwrap().contains("ghp_"));
        assert_eq!(redact_urls("fatal: unable to access 'https://u:tok@h/x.git/': 403"), "fatal: unable to access 'https://h/x.git/': 403");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-pull-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sh_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A git repo at `dir/repo` holding `lock` as harness.lock and the files `setup` adds.
    fn repo(dir: &Path, lock: &str, setup: impl Fn(&Path)) -> String {
        let r = dir.join("repo");
        std::fs::create_dir_all(&r).unwrap();
        std::fs::write(r.join("harness.lock"), lock).unwrap();
        setup(&r);
        sh_git(&r, &["init", "-q"]);
        sh_git(&r, &["add", "-A"]);
        sh_git(&r, &["commit", "-q", "-m", "pin"]);
        format!("git:file://{}", r.display())
    }

    const HEAD: &str = "lock-version = 1\ngenerated-by = \"t\"\ngenerated-at = \"\"\n";

    #[test]
    fn a_harness_name_that_is_a_path_is_refused_before_anything_is_deleted() {
        let d = tmp("name");
        let spec = repo(&d, &format!("{HEAD}[harness]\nname = \"../../x\"\n"), |_| {});
        let project = d.join("project");
        std::fs::create_dir_all(project.join("x")).unwrap();
        std::fs::write(project.join("x/keep"), "mine").unwrap();
        let e = pull(&spec, &Target::File(project.join("harness.lock")), true).unwrap_err();
        assert!(e.to_string().contains("plain name"), "{e}");
        assert_eq!(std::fs::read_to_string(project.join("x/keep")).unwrap(), "mine");
        assert!(!project.join("harness.lock").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_the_repository_are_not_followed() {
        let d = tmp("symlink");
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/secret.txt"), "s3cret").unwrap();
        let lock = format!(
            "{HEAD}[harness]\nname = \"pi\"\n[[skill]]\nname = \"ok\"\norigin = \"x\"\nvendored = \".hlock/vendor/pi/skills/ok\"\n\
             [[skill]]\nname = \"leak\"\norigin = \"x\"\nvendored = \".hlock/vendor/pi/skills/leak\"\n"
        );
        let outside = d.join("outside");
        let spec = repo(&d, &lock, |r| {
            std::fs::create_dir_all(r.join(".hlock/vendor/pi/skills/ok")).unwrap();
            std::fs::write(r.join(".hlock/vendor/pi/skills/ok/SKILL.md"), "ok").unwrap();
            std::os::unix::fs::symlink(&outside, r.join(".hlock/vendor/pi/skills/leak")).unwrap();
        });
        let project = d.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let o = pull(&spec, &Target::File(project.join("harness.lock")), false).unwrap();
        let vdir = project.join(".hlock/vendor/pi");
        assert_eq!(std::fs::read_to_string(vdir.join("skills/ok/SKILL.md")).unwrap(), "ok");
        assert!(std::fs::symlink_metadata(vdir.join("skills/leak")).is_err(), "the symlink isn't copied or followed");
        assert!(o.warnings.iter().any(|w| w.contains("leak") && w.contains("symlink")), "{:?}", o.warnings);
        assert_eq!(o.lock.skill.iter().find(|s| s.name == "leak").unwrap().vendored, None);
        // a re-pull swaps the vendor directory in whole and leaves no staging behind
        pull(&spec, &Target::File(project.join("harness.lock")), false).unwrap();
        let names: Vec<String> = std::fs::read_dir(project.join(".hlock/vendor")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["pi"]);
        assert!(vdir.join("skills/ok/SKILL.md").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }
}
