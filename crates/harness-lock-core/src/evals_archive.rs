//! A registry version's **evals archive**: the project's eval packs, published next to its
//! setup archive (`specs/registry.md` §5.2). The setup archive commits to it by digest, so a
//! version's eval packs are fixed when it's first published.
//!
//! The packs are the root `harness.toml`'s `[evals] packs` that are local directories with
//! strict, portable paths. Each is packed as exactly the files `tree_digest` hashes, with
//! their executable bits, so a pulled pack has the same `packs_digest` as the pushed one and
//! a lock's `[evidence] eval-pack-digest` can be checked after a pull.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::registry::PackFile;

/// The most entries an evals archive may have.
pub const MAX_ENTRIES: usize = 5000;
/// The largest evals archive (compressed).
pub const MAX_ARCHIVE: usize = 32 << 20;
/// The most an evals archive may unpack to.
pub const MAX_UNPACKED: u64 = 256 << 20;

fn err(msg: impl Into<String>) -> Error {
    Error::Registry(msg.into())
}

/// The packed eval packs of a project.
#[derive(Debug, Clone, Serialize)]
pub struct PackedEvals {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub digest: String,
    pub size_bytes: u64,
    /// The pack directories, as the root `harness.toml` names them (without a leading `./`).
    pub packs: Vec<String>,
    pub files: Vec<PackFile>,
    pub warnings: Vec<String>,
}

// --------------------------------------------------------------------------- //
// paths
// --------------------------------------------------------------------------- //

/// An `[evals] packs` entry as a pack path: a leading `./` and trailing `/` dropped,
/// nothing else normalized.
pub fn pack_entry(entry: &str) -> String {
    let mut s = entry.trim_end_matches('/');
    while let Some(r) = s.strip_prefix("./") {
        s = r;
    }
    s.to_string()
}

/// Whether `path` is a pack path: `/`-separated components each matching
/// `[A-Za-z0-9_-][A-Za-z0-9._-]*`, not ending in `.` (so no `.`, `..`, leading dots,
/// non-ASCII or `\`).
pub fn pack_path_ok(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|c| {
            c.starts_with(|ch: char| ch.is_ascii_alphanumeric() || "_-".contains(ch)) && c.chars().all(|ch| ch.is_ascii_alphanumeric() || "._-".contains(ch)) && !c.ends_with('.')
        })
}

const DEVICE_NAMES: &[&str] = &["con", "prn", "aux", "nul"];

/// Why a component of a path in an evals archive isn't safe on every OS, if it isn't.
fn component_problem(c: &str) -> Option<&'static str> {
    if c.is_empty() || c == "." || c == ".." {
        return Some("an empty, `.` or `..` component");
    }
    if !c.chars().all(|ch| ch.is_ascii_alphanumeric() || "._+@=,-".contains(ch)) {
        return Some("a character other than letters, digits and ._+@=,-");
    }
    if c.ends_with('.') {
        return Some("a name ending in `.`");
    }
    let stem = c.split('.').next().unwrap_or("").to_ascii_lowercase();
    let numbered = |p: &str| stem.strip_prefix(p).is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit() && n != "0");
    if DEVICE_NAMES.contains(&stem.as_str()) || numbered("com") || numbered("lpt") {
        return Some("a Windows device name");
    }
    if c.eq_ignore_ascii_case(".hlock") {
        return Some("a `.hlock` directory");
    }
    if c.eq_ignore_ascii_case(".git") {
        return Some("a `.git` directory");
    }
    None
}

/// Why `path` (relative, `/`-separated) can't be in an evals archive, if it can't.
pub fn path_problem(path: &str) -> Option<&'static str> {
    path.split('/').find_map(component_problem)
}

/// Names refused in an eval pack as likely credentials.
fn sensitive_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n == ".env" || (n.starts_with(".env.") && n != ".env.example"))
        || [".netrc", ".npmrc", ".pypirc", "credentials"].contains(&n.as_str())
        || [".pem", ".key", ".p12", ".pfx"].iter().any(|e| n.ends_with(e))
        || ["id_rsa", "id_ed25519", "id_ecdsa"].iter().any(|p| n.starts_with(p))
}

/// Token prefixes of credentials, for free text: the capture scanner's, and npm's.
fn token_prefix(w: &str) -> bool {
    crate::capture::SECRET_PREFIXES.iter().filter(|p| !p.ends_with(' ')).chain(["npm_"].iter()).any(|p| w.starts_with(p))
}

/// The value of a `key = value` or `key: value` line, when it's a literal (quoted, or one bare
/// token): `api_key = os.environ["X"]` reads a credential rather than holding one.
fn keyed_value(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start().strip_prefix("export ").unwrap_or(line.trim_start());
    let i = line.find(['=', ':'])?;
    let key = line[..i].trim().trim_matches(['"', '\'']);
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        return None;
    }
    let v = line[i + 1..].trim().trim_end_matches([',', ';']);
    let v = match v.chars().next() {
        Some(q @ ('"' | '\'')) => v[1..].split(q).next()?,
        _ if v.contains(|c: char| c.is_whitespace() || "()[]{}".contains(c)) => return None,
        _ => v,
    };
    Some((key, v))
}

/// Credential-shaped text, as `file:line` labels: a private key block, a token with a known
/// prefix, or a credential-named key with a literal value that isn't a placeholder (the rule
/// parsed TOML, JSON and YAML get). Only the private key block when it isn't `text_file`.
fn secret_lines(text: &str, file: &str, text_file: bool, out: &mut Vec<String>) {
    for (i, line) in text.lines().enumerate() {
        let private_key = line.find("-----BEGIN ").is_some_and(|s| {
            let rest = &line[s + "-----BEGIN ".len()..];
            rest.find("PRIVATE KEY-----").is_some_and(|e| rest[..e].chars().all(|c| c.is_ascii_uppercase() || c == ' '))
        });
        if !text_file {
            if private_key {
                out.push(format!("{file}:{}", i + 1));
            }
            continue;
        }
        let token = line.split(|c: char| c.is_whitespace() || "=:\"'`,;()[]{}<>".contains(c)).any(|w| w.len() >= 20 && token_prefix(w));
        let keyed = keyed_value(line).is_some_and(|(k, v)| crate::capture::is_secret_key(k) && v.len() >= 8 && !v.contains("HLOCK_SECRET_") && !v.starts_with('$'));
        if private_key || token || keyed {
            out.push(format!("{file}:{}", i + 1));
        }
    }
}

/// The string entries of `[evals] packs` (or the old `[tasks] packs`) in a manifest's text.
pub fn declared_packs(manifest: &toml::Value) -> Vec<String> {
    let t = manifest.get("evals").or_else(|| manifest.get("tasks"));
    t.and_then(|t| t.get("packs")).and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Whether any component between `root` and `root/rel` is a symlink.
fn through_symlink(root: &Path, rel: &str) -> bool {
    let mut p = root.to_path_buf();
    rel.split('/').any(|c| {
        p.push(c);
        std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
    })
}

fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

// --------------------------------------------------------------------------- //
// pack
// --------------------------------------------------------------------------- //

/// Pack the eval packs the root `harness.toml` in `dir` names, after checking them; `None`
/// when it names no local pack. Refuses (listing every problem) rather than leave a file
/// out, since a file left out would change the packs' digest.
pub fn pack(dir: &Path) -> Result<Option<PackedEvals>> {
    let Ok(text) = std::fs::read_to_string(dir.join("harness.toml")) else { return Ok(None) };
    let manifest: toml::Value = toml::from_str(&text).map_err(|e| err(format!("harness.toml isn't valid TOML: {e}")))?;
    let mut warnings = Vec::new();
    let mut problems = Vec::new();
    let mut packs: Vec<String> = Vec::new();
    let root = std::fs::canonicalize(dir).map_err(|e| crate::error::io_at(dir, e))?;
    for entry in declared_packs(&manifest) {
        if entry.contains(':') {
            warnings.push(format!("eval pack {entry} isn't a local directory; it isn't published"));
            continue;
        }
        let p = pack_entry(&entry);
        let bad = |why: &str| format!("eval pack {entry:?}: {why}");
        if !pack_path_ok(&p) {
            problems.push(bad("a pack path is relative, with components of letters, digits and ._- that don't start with `.` (no `..`)"));
        } else if crate::pull::SETUP_FILES.iter().chain(["apm.yaml", "readme.md"].iter()).any(|f| f.eq_ignore_ascii_case(&p)) {
            problems.push(bad("it's one of the setup's own files"));
        } else if let Some(other) = packs.iter().find(|o| overlaps(o, &p)) {
            problems.push(bad(&format!("it overlaps eval pack {other:?}")));
        } else if through_symlink(dir, &p) || std::fs::canonicalize(dir.join(&p)).is_ok_and(|c| !c.starts_with(&root)) {
            problems.push(bad("it's a symlink, under one, or outside the project"));
        } else if !dir.join(&p).join("pack.toml").is_file() {
            problems.push(bad("there's no pack.toml there (a missing pack can't be published)"));
        } else {
            packs.push(p);
        }
    }
    if packs.is_empty() && problems.is_empty() {
        return Ok(None);
    }
    // (path in the archive, file on disk, size, executable)
    let mut entries: Vec<(String, PathBuf, u64, bool)> = Vec::new();
    let mut counts: Vec<(String, usize, u64)> = Vec::new();
    for p in &packs {
        let base = dir.join(p);
        let (mut n, mut size) = (0, 0);
        for rel in crate::capture::walk_files(&base) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            let path = format!("{p}/{rel}");
            let f = base.join(&rel);
            let len = std::fs::symlink_metadata(&f).map(|m| m.len()).unwrap_or(0);
            n += 1;
            size += len;
            entries.push((path, f.clone(), len, crate::capture::is_executable(&f)));
        }
        counts.push((p.clone(), n, size));
    }
    let breakdown = || counts.iter().map(|(p, n, s)| format!("  {p}: {n} files, {} KiB", s >> 10)).collect::<Vec<_>>().join("\n");
    if entries.len() > MAX_ENTRIES {
        return Err(err(format!("the eval packs have {} files; an evals archive holds at most {MAX_ENTRIES}:\n{}\npush with --no-evals to publish without them", entries.len(), breakdown())));
    }
    let total: u64 = entries.iter().map(|e| e.2).sum();
    if total > MAX_UNPACKED {
        return Err(err(format!("the eval packs are {} MiB; an evals archive holds at most {} MiB:\n{}\npush with --no-evals to publish without them", total >> 20, MAX_UNPACKED >> 20, breakdown())));
    }
    let mut lower = std::collections::BTreeMap::new();
    for (path, _, _, _) in &entries {
        if let Some(why) = path_problem(path) {
            problems.push(format!("{path}: {why}"));
        }
        if let Some(prev) = lower.insert(path.to_ascii_lowercase(), path.clone()) {
            problems.push(format!("{path}: the same path as {prev} on a case-insensitive file system"));
        }
        if sensitive_name(path.rsplit('/').next().unwrap_or("")) {
            problems.push(format!("{path}: named like a credential file"));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut files = Vec::new();
    let mut secrets = Vec::new();
    let mut hidden = Vec::new();
    let gz = flate2::GzBuilder::new().mtime(0).operating_system(255).write(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for (rel, p, len, exec) in &entries {
        if *len > crate::scan::MAX_SCAN as u64 {
            problems.push(format!("{rel}: larger than {} MiB, too large to scan", crate::scan::MAX_SCAN >> 20));
            continue;
        }
        let bytes = match crate::fsutil::read_regular(p, crate::scan::MAX_SCAN) {
            Ok(b) => b,
            Err(e) => {
                problems.push(format!("{rel}: can't read it: {e}"));
                continue;
            }
        };
        if let Some(v) = crate::registry::parsed(rel, &bytes) {
            crate::registry::secret_lines(&v, rel, "", &mut secrets);
        }
        // every rule in a text file; a private key block in any file
        let text = (!crate::scan::is_binary_name(rel)).then(|| std::str::from_utf8(&bytes).ok()).flatten();
        secret_lines(text.map(std::borrow::Cow::Borrowed).unwrap_or_else(|| String::from_utf8_lossy(&bytes)).as_ref(), rel, text.is_some(), &mut secrets);
        hidden.extend(crate::scan::scan_bytes(&bytes, rel));
        let mut h = tar::Header::new_ustar();
        h.set_size(bytes.len() as u64);
        h.set_mode(if *exec { 0o755 } else { 0o644 });
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut h, rel, bytes.as_slice()).map_err(|e| err(format!("can't pack {rel}: {e}")))?;
        files.push(PackFile { path: rel.clone(), size: bytes.len() as u64, digest: crate::sha256_hex(&bytes) });
    }
    secrets.sort();
    secrets.dedup();
    problems.extend(secrets.into_iter().map(|s| format!("{s}: looks like a credential")));
    if !problems.is_empty() {
        return Err(Error::Audit(format!(
            "the eval packs can't be published as they are; nothing was pushed:\n{}\nfix these, or push with --no-evals to publish without the eval packs",
            problems.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n")
        )));
    }
    crate::audit::blocking(&hidden, &[], "the eval packs")?;
    let bytes = tar.into_inner().and_then(|gz| gz.finish()).map_err(|e| err(format!("can't pack the eval packs: {e}")))?;
    if bytes.len() > MAX_ARCHIVE {
        return Err(err(format!("the eval packs pack to {} MiB; an evals archive holds at most {} MiB:\n{}\npush with --no-evals to publish without them", bytes.len() >> 20, MAX_ARCHIVE >> 20, breakdown())));
    }
    let digest = format!("sha256:{}", crate::sha256_hex(&bytes));
    Ok(Some(PackedEvals { size_bytes: bytes.len() as u64, bytes, digest, packs, files, warnings }))
}

// --------------------------------------------------------------------------- //
// unpack
// --------------------------------------------------------------------------- //

/// Unpack an evals archive into `dest`, refusing it unless every entry is a regular file
/// (mode `0644` or `0755`) at a safe path under a pack directory that `declared` (the setup's
/// root `harness.toml` `[evals] packs`) lists. Returns the pack directories and the files.
pub fn unpack(bytes: &[u8], dest: &Path, declared: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let bad = |why: String| err(format!("the evals archive is unsafe or invalid: {why}; nothing was written"));
    // first pass: the entries, so the pack directories are known before anything is written
    let mut list: Vec<(String, bool)> = Vec::new();
    let mut sizes: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut total = 0u64;
    {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for (i, entry) in archive.entries().map_err(|e| bad(e.to_string()))?.enumerate() {
            if i >= MAX_ENTRIES {
                return Err(bad(format!("more than {MAX_ENTRIES} entries")));
            }
            let entry = entry.map_err(|e| bad(e.to_string()))?;
            let path = String::from_utf8(entry.path_bytes().into_owned()).map_err(|_| bad("a path that isn't UTF-8".into()))?;
            let path = path.trim_start_matches("./").to_string();
            match entry.header().entry_type() {
                tar::EntryType::Directory => continue,
                tar::EntryType::Regular | tar::EntryType::Continuous => {}
                t => return Err(bad(format!("{path:?} is a {t:?}, not a regular file"))),
            }
            let mode = entry.header().mode().unwrap_or(0) & 0o7777;
            if mode != 0o644 && mode != 0o755 {
                return Err(bad(format!("{path:?} has mode {mode:o} (only 644 and 755)")));
            }
            if path.starts_with('/') || !crate::fsutil::safe_rel(&path) {
                return Err(bad(format!("{path:?} is outside the archive")));
            }
            if let Some(why) = path_problem(&path) {
                return Err(bad(format!("{path:?} has {why}")));
            }
            // the size the body is read with (a PAX `size` overrides the ustar one), which
            // must be the header's, so the limit counts what's actually written
            let size = entry.size();
            if entry.header().size().ok() != Some(size) {
                return Err(bad(format!("{path:?} has a size that differs from its header's")));
            }
            total += size;
            if total > MAX_UNPACKED {
                return Err(bad(format!("it unpacks to more than {} MiB", MAX_UNPACKED >> 20)));
            }
            sizes.insert(path.clone(), size);
            list.push((path, mode == 0o755));
        }
    }
    let mut lower = std::collections::BTreeSet::new();
    for (p, _) in &list {
        if !lower.insert(p.to_ascii_lowercase()) {
            return Err(bad(format!("{p:?} is the same path as another on a case-insensitive file system")));
        }
    }
    // pack directories: a pack path holding pack.toml, with no pack directory above it
    let mut candidates: Vec<String> = list.iter().filter_map(|(p, _)| p.strip_suffix("/pack.toml")).filter(|d| pack_path_ok(d)).map(str::to_string).collect();
    candidates.sort_by_key(|d| d.matches('/').count());
    let mut packs: Vec<String> = Vec::new();
    for d in candidates {
        if !packs.iter().any(|p| d.starts_with(&format!("{p}/"))) {
            packs.push(d);
        }
    }
    let declared: Vec<String> = declared.iter().filter(|e| !e.contains(':')).map(|e| pack_entry(e)).collect();
    if let Some(p) = packs.iter().find(|p| !declared.contains(p)) {
        return Err(bad(format!("{p:?} isn't an eval pack the setup's harness.toml lists")));
    }
    if let Some((p, _)) = list.iter().find(|(p, _)| !packs.iter().any(|d| p.starts_with(&format!("{d}/")))) {
        return Err(bad(format!("{p:?} isn't inside an eval pack")));
    }
    // second pass: write them
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().map_err(|e| bad(e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        if entry.header().entry_type() == tar::EntryType::Directory {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path_bytes()).trim_start_matches("./").to_string();
        let exec = list.iter().find(|(p, _)| *p == path).is_some_and(|(_, x)| *x);
        let to = dest.join(&path);
        crate::fsutil::check_within(&path, dest, &to)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::io_at(parent, e))?;
        }
        let size = *sizes.get(&path).ok_or_else(|| bad(format!("{path:?} wasn't in the first read")))?;
        let mut buf = Vec::new();
        entry.by_ref().take(size + 1).read_to_end(&mut buf).map_err(|e| bad(e.to_string()))?;
        if buf.len() as u64 != size {
            return Err(bad(format!("{path:?} isn't the size its header says")));
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&to).map_err(|e| bad(format!("{path}: {e}")))?;
        f.write_all(&buf).map_err(|e| crate::error::io_at(&to, e))?;
        drop(f);
        set_mode(&to, exec)?;
    }
    Ok((packs, list.into_iter().map(|(p, _)| p).collect()))
}

/// Set a file's mode to `0755` or `0644` explicitly (a create mode is masked by umask).
#[cfg(unix)]
pub(crate) fn set_mode(p: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 })).map_err(|e| crate::error::io_at(p, e))
}

#[cfg(not(unix))]
pub(crate) fn set_mode(_p: &Path, _exec: bool) -> Result<()> {
    Ok(())
}

/// A `Version`'s `evals` object, when the registry supports evals: `Some(None)` when the
/// version declared no evals archive (`evals: null`), `None` when the field is absent (the
/// registry doesn't take eval packs).
pub fn version_evals(v: &Value) -> Option<Option<&Value>> {
    let m = v.as_object()?;
    if !m.contains_key("evals") {
        return None;
    }
    Some(m.get("evals").filter(|e| e.get("digest").and_then(Value::as_str).is_some()))
}

/// A tar.gz of plain `entries`, then `path` whose ustar header says it's empty while a PAX
/// `size` record says it's `data`'s length: the body a reader reads isn't the header's size.
#[cfg(test)]
pub(crate) fn pax_sized_tarball(entries: &[(&str, &[u8])], path: &str, data: &[u8]) -> Vec<u8> {
    let mut t = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
    let file = |p: &str, size: u64| {
        let mut h = tar::Header::new_ustar();
        h.set_path(p).unwrap();
        h.set_size(size);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        h
    };
    for (p, d) in entries {
        t.append(&file(p, d.len() as u64), *d).unwrap();
    }
    let body = format!(" size={}\n", data.len());
    let record = (1..).map(|n: usize| format!("{}{body}", n + body.len())).find(|r| r.len() == r.split(' ').next().unwrap().parse::<usize>().unwrap()).unwrap();
    let mut x = tar::Header::new_ustar();
    x.set_path("PaxHeader").unwrap();
    x.set_size(record.len() as u64);
    x.set_entry_type(tar::EntryType::XHeader);
    x.set_cksum();
    t.append(&x, record.as_bytes()).unwrap();
    t.append(&file(path, 0), data).unwrap();
    t.into_inner().unwrap().finish().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_in_text_and_names() {
        let hits = |text: &str, text_file: bool| {
            let mut out = Vec::new();
            secret_lines(text, "f", text_file, &mut out);
            out
        };
        // a private key block, in any file
        // split so the repository's own private-key hook doesn't take these for keys
        assert_eq!(hits(concat!("x\n-----BEGIN OPENSSH ", "PRIVATE KEY-----\nb3Blbn\n"), true), ["f:2"]);
        assert_eq!(hits(concat!("-----BEGIN RSA ", "PRIVATE KEY-----"), false), ["f:1"]);
        assert!(hits("-----BEGIN PUBLIC KEY-----", true).is_empty());
        // a token with a known prefix, npm's too
        assert_eq!(hits("//registry.npmjs.org/:_authToken=npm_abcdefghijklmnopqrstuvwxyz0123", true), ["f:1"]);
        // a credential-named key with a literal value that isn't a placeholder
        for (line, hit) in [
            ("API_KEY=abc123defg", true),
            ("export GITHUB_TOKEN='abcdef123456'", true),
            ("password: hunter2hunter2", true),
            ("\"client_secret\": \"s3cr3t-value\",", true),
            ("api_key = os.environ[\"API_KEY\"]", false),
            ("token = get_token()", false),
            ("token: ${HLOCK_SECRET_TOKEN}", false),
            ("password = \"$PASS\"", false),
            ("password = short", false),
            ("name = a-long-ordinary-value", false),
        ] {
            assert_eq!(!hits(line, true).is_empty(), hit, "{line}");
            assert!(hits(line, false).is_empty(), "{line}: only a key block counts outside text files");
        }
        for n in [".netrc", ".npmrc", ".pypirc", "credentials", "cert.p12", "cert.PFX", "server.key", "id_ed25519"] {
            assert!(sensitive_name(n), "{n}");
        }
        assert!(!sensitive_name("credentials.md") && !sensitive_name("netrc.txt"));
    }

    #[test]
    fn a_pax_size_that_isnt_the_headers_is_refused() {
        let d = std::env::temp_dir().join(format!("hlock-evals-pax-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let bytes = pax_sized_tarball(&[("evals/pack.toml", b"[pack]\n")], "evals/big", &vec![b'x'; 4096]);
        let e = unpack(&bytes, &d, &["evals".into()]).unwrap_err().to_string();
        assert!(e.contains("differs from its header"), "{e}");
        assert!(!d.join("evals/big").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-evals-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn project(d: &Path, packs: &str) {
        std::fs::write(d.join("harness.toml"), format!("[project]\nname = \"x\"\n[evals]\npacks = {packs}\n")).unwrap();
        std::fs::create_dir_all(d.join("evals/t1/workspace")).unwrap();
        std::fs::write(d.join("evals/pack.toml"), "[pack]\nname = \"x/e\"\nversion = \"1\"\n").unwrap();
        std::fs::write(d.join("evals/t1/prompt.md"), "Fix it.\n").unwrap();
        std::fs::write(d.join("evals/t1/workspace/data.csv"), "a,b\n1,2\n").unwrap();
    }

    #[test]
    fn pack_paths() {
        for ok in ["evals", "a/b_c", "e-1/x.y"] {
            assert!(pack_path_ok(ok), "{ok}");
        }
        for bad in ["", ".", "..", "./x", "a/../b", ".claude", "a/.hlock", "évals", "a\\b", "x.", "/abs", "a//b", "C:x"] {
            assert!(!pack_path_ok(bad), "{bad}");
        }
        assert_eq!(pack_entry("./evals/"), "evals");
        assert_eq!(path_problem("evals/t/CON.txt"), Some("a Windows device name"));
        assert_eq!(path_problem("evals/com1"), Some("a Windows device name"));
        assert_eq!(path_problem("evals/com10"), None);
        assert_eq!(path_problem("evals/a b"), Some("a character other than letters, digits and ._+@=,-"));
        assert_eq!(path_problem("evals/.HLOCK/x"), Some("a `.hlock` directory"));
        assert_eq!(path_problem("evals/.gitignore"), None);
        assert!(sensitive_name(".env") && sensitive_name(".env.local") && !sensitive_name(".env.example") && sensitive_name("id_rsa.pub") && sensitive_name("tls.KEY"));
    }

    #[test]
    fn packs_round_trip_with_their_digest_and_modes() {
        let d = tmp("round");
        project(&d, "[\"./evals\", \"github:a/b\"]");
        #[cfg(unix)]
        {
            std::fs::write(d.join("evals/t1/check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
            set_mode(&d.join("evals/t1/check.sh"), true).unwrap();
        }
        let a = pack(&d).unwrap().unwrap();
        let b = pack(&d).unwrap().unwrap();
        assert_eq!(a.bytes, b.bytes);
        assert_eq!(a.packs, ["evals"]);
        assert!(a.warnings.iter().any(|w| w.contains("github:a/b")));
        let out = tmp("round-out");
        let (packs, files) = unpack(&a.bytes, &out, &["./evals".into()]).unwrap();
        assert_eq!(packs, ["evals"]);
        assert!(files.contains(&"evals/t1/prompt.md".to_string()));
        assert_eq!(crate::capture::tree_digest(&d.join("evals")), crate::capture::tree_digest(&out.join("evals")));
        // not listed by the setup's harness.toml
        let e = unpack(&a.bytes, &tmp("round-other"), &["other".into()]).unwrap_err().to_string();
        assert!(e.contains("isn't an eval pack"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn packing_refuses_what_it_cant_publish() {
        let d = tmp("refuse");
        project(&d, "[\"evals\"]");
        std::fs::create_dir_all(d.join("evals/t1/.hlock")).unwrap();
        std::fs::write(d.join("evals/t1/.hlock/x"), "x").unwrap();
        std::fs::write(d.join("evals/t1/workspace/.env"), "A=1").unwrap();
        std::fs::write(d.join("evals/t1/workspace/conf.py"), "KEY = 'ghp_abcdefghijklmnopqrstuvwxyz0123'\n").unwrap();
        std::fs::write(d.join("evals/t1/workspace/a file.txt"), "x").unwrap();
        let e = pack(&d).unwrap_err().to_string();
        for want in [".hlock` directory", "named like a credential", "conf.py:1: looks like a credential", "a character other than"] {
            assert!(e.contains(want), "{want}: {e}");
        }
        // a bad pack path, a missing pack, overlapping packs
        for (packs, want) in [("[\"../evals\"]", "a pack path"), ("[\"nope\"]", "no pack.toml"), ("[\"evals\", \"evals/t1\"]", "overlaps"), ("[\"harness.toml\"]", "own files")] {
            project(&d, packs);
            let e = pack(&d).unwrap_err().to_string();
            assert!(e.contains(want), "{packs}: {e}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_pack_through_a_symlink_is_refused() {
        let d = tmp("link");
        let outside = tmp("link-outside");
        std::fs::create_dir_all(outside.join("evals")).unwrap();
        std::fs::write(outside.join("evals/pack.toml"), "[pack]\n").unwrap();
        std::fs::write(d.join("harness.toml"), "[evals]\npacks = [\"ext/evals\"]\n").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("ext")).unwrap();
        let e = pack(&d).unwrap_err().to_string();
        assert!(e.contains("symlink"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn unsafe_evals_archives_are_refused() {
        let tarball = |entries: &[(&str, u32)]| {
            let mut t = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
            for (path, mode) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(1);
                h.set_mode(*mode);
                h.set_entry_type(tar::EntryType::Regular);
                h.set_cksum();
                t.append_data(&mut h, path, b"x".as_slice()).unwrap();
            }
            t.into_inner().unwrap().finish().unwrap()
        };
        let declared = vec!["evals".to_string(), ".claude".to_string()];
        for (entries, why) in [
            (vec![("evals/pack.toml", 0o644), ("evals/x", 0o4755)], "mode"),
            (vec![("evals/pack.toml", 0o644), ("src/x.py", 0o644)], "isn't inside an eval pack"),
            (vec![(".claude/pack.toml", 0o644), (".claude/settings.json", 0o644)], "isn't inside an eval pack"),
            (vec![("evals/pack.toml", 0o644), ("evals/A", 0o644), ("evals/a", 0o644)], "case-insensitive"),
            (vec![("evals/pack.toml", 0o644), ("evals/.hlock/x", 0o644)], "`.hlock`"),
        ] {
            let d = tmp("unsafe");
            let e = unpack(&tarball(&entries), &d, &declared).unwrap_err().to_string();
            assert!(e.contains(why), "{why}: {e}");
            assert!(std::fs::read_dir(&d).unwrap().next().is_none(), "nothing written");
        }
    }
}
