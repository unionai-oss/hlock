//! `hlock apm push` / `hlock apm pull`: APM packages on the registry `hlock login` saved,
//! over APM's Registry HTTP API v1 (`PUT` and `GET …/versions[/{v}/download]`).
//!
//! An hlock registry serves APM's endpoints under the same `/api/v1` prefix as its own, so
//! its APM base URL (what goes in apm.yml's `registries:`) is `<url>/api`. A package is
//! published as `apm publish` builds it: a flat zip of `apm.yml`, `.apm/`, and the README,
//! CHANGELOG and LICENSE, with sorted entries and no timestamps, so the same files give
//! the same digest.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::fsutil::safe_rel;
use crate::registry::{parse_ref, slug, version_ok, Client, PackFile, Ref};

/// The largest package `push` sends and `pull` accepts (compressed): APM's suggested limit.
pub const MAX_ARCHIVE: usize = 50 << 20;
/// The most entries a package may have.
const MAX_ENTRIES: usize = 5000;
/// The most a package may unpack to.
const MAX_UNPACKED: u64 = 256 << 20;
/// Files at the package root that go along with `apm.yml` and `.apm/` (case-insensitive).
const DOCS: &[&str] = &["readme.md", "changelog.md", "license", "licence", "license.md", "licence.md", "license.txt", "licence.txt"];

fn err(msg: impl Into<String>) -> Error {
    Error::Registry(msg.into())
}

/// The APM base URL of an hlock registry: what APM appends `/v1/packages/…` to.
pub fn apm_base(url: &str) -> String {
    format!("{url}/api")
}

/// An APM package packed for `push`.
#[derive(Debug, Clone, Serialize)]
pub struct Packed {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub digest: String,
    pub size_bytes: u64,
    pub files: Vec<PackFile>,
    /// `apm.yml`'s `name`.
    pub name: String,
    /// `apm.yml`'s `version`.
    pub version: String,
    pub warnings: Vec<String>,
}

/// macOS sidecar files, which `apm publish` leaves out too.
fn junk(name: &str) -> bool {
    name == ".DS_Store" || name.starts_with("._")
}

/// Every regular file under `dir`, as (path relative to `base`, file), sorted. Symlinks are
/// left out with a warning.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, warnings: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).map_err(|e| crate::error::io_at(dir, e))?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = p.strip_prefix(base).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or(name.clone());
        if junk(&name) {
            continue;
        }
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => warnings.push(format!("{rel} is a symlink; left out")),
            Ok(m) if m.is_dir() => walk(base, &p, out, warnings)?,
            Ok(m) if m.is_file() => out.push((rel, p)),
            Ok(_) => warnings.push(format!("{rel} isn't a regular file; left out")),
            Err(_) => {}
        }
    }
    Ok(())
}

/// `apm.yml`'s `name` and `version` (a YAML number is read as written, e.g. `1.0`).
fn name_and_version(text: &str, file: &str) -> Result<(String, String)> {
    let v: Value = serde_norway::from_str(text).map_err(|e| err(format!("{file} isn't valid YAML: {e}")))?;
    let scalar = |k: &str| match &v[k] {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let name = scalar("name").ok_or_else(|| err(format!("{file} has no `name`; an APM package needs one")))?;
    let version = scalar("version").ok_or_else(|| err(format!("{file} has no `version`; an APM package needs one to be published")))?;
    Ok((name, version))
}

/// Pack the APM package in `dir` (`apm.yml` and `.apm/`) into a deterministic zip, after
/// checking it as `hlock push` checks a setup: no credential-looking values, no hidden
/// characters.
pub fn pack(dir: &Path) -> Result<Packed> {
    let mut warnings = Vec::new();
    let manifest = match (dir.join("apm.yml").is_file(), dir.join("apm.yaml").is_file()) {
        (true, true) => {
            warnings.push("both apm.yml and apm.yaml exist; publishing apm.yml".into());
            dir.join("apm.yml")
        }
        (true, false) => dir.join("apm.yml"),
        (false, true) => dir.join("apm.yaml"),
        _ => return Err(err(format!("{} has no apm.yml; an APM package needs one (with `name` and `version`) and a .apm/ directory", dir.display()))),
    };
    if !std::fs::symlink_metadata(&manifest).is_ok_and(|m| m.is_file()) {
        return Err(err(format!("{} is a symlink; refusing to publish it", manifest.display())));
    }
    let text = crate::fsutil::read_regular_text(&manifest, 1 << 20).map_err(|e| crate::error::io_at(&manifest, e))?;
    let (name, version) = name_and_version(&text, "apm.yml")?;
    let primitives = dir.join(".apm");
    if !std::fs::symlink_metadata(&primitives).is_ok_and(|m| m.is_dir()) {
        return Err(err(format!(
            "{} has no .apm/ directory. An APM package keeps its skills, prompts and instructions there; a project's apm.yml and apm.lock.yaml are published with `hlock push`",
            dir.display()
        )));
    }
    // (path in the archive, file on disk)
    let mut entries: Vec<(String, PathBuf)> = vec![("apm.yml".into(), manifest)];
    walk(dir, &primitives, &mut entries, &mut warnings)?;
    if entries.len() == 1 {
        return Err(err(".apm/ is empty; there's nothing in the package"));
    }
    let mut root: Vec<_> = std::fs::read_dir(dir).map_err(|e| crate::error::io_at(dir, e))?.filter_map(|e| e.ok()).collect();
    root.sort_by_key(|e| e.file_name());
    for e in root {
        let n = e.file_name().to_string_lossy().into_owned();
        if DOCS.contains(&n.to_lowercase().as_str()) {
            match std::fs::symlink_metadata(e.path()) {
                Ok(m) if m.is_file() => entries.push((n, e.path())),
                Ok(_) => warnings.push(format!("{n} isn't a regular file; left out")),
                Err(_) => {}
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if entries.len() > MAX_ENTRIES {
        return Err(err(format!("the package has {} files; at most {MAX_ENTRIES} can be published", entries.len())));
    }
    let mut files = Vec::new();
    let mut secrets = Vec::new();
    let mut hidden = Vec::new();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut total = 0u64;
    for (rel, p) in &entries {
        let bytes = crate::fsutil::read_regular(p, MAX_UNPACKED as usize).map_err(|e| crate::error::io_at(p, e))?;
        total += bytes.len() as u64;
        if total > MAX_UNPACKED {
            return Err(err(format!("the package is larger than {} MiB unpacked", MAX_UNPACKED >> 20)));
        }
        if let Some(v) = crate::registry::parsed(rel, &bytes) {
            crate::registry::secret_lines(&v, rel, "", &mut secrets);
        }
        hidden.extend(crate::scan::scan_bytes(&bytes, rel));
        zip.start_file(rel.as_str(), options).map_err(|e| err(format!("can't pack {rel}: {e}")))?;
        zip.write_all(&bytes).map_err(|e| err(format!("can't pack {rel}: {e}")))?;
        files.push(PackFile { path: rel.clone(), size: bytes.len() as u64, digest: crate::sha256_hex(&bytes) });
    }
    if !secrets.is_empty() {
        secrets.sort();
        secrets.dedup();
        return Err(Error::Audit(format!(
            "these look like credentials, not placeholders; nothing was published:\n{}\nreplace them with ${{HLOCK_SECRET_…}} references",
            secrets.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n")
        )));
    }
    crate::audit::blocking(&hidden, &[], "the package")?;
    let bytes = zip.finish().map_err(|e| err(format!("can't pack the package: {e}")))?.into_inner();
    if bytes.len() > MAX_ARCHIVE {
        return Err(err(format!("the package packs to {} bytes; at most {} MiB can be published", bytes.len(), MAX_ARCHIVE >> 20)));
    }
    let digest = format!("sha256:{}", crate::sha256_hex(&bytes));
    Ok(Packed { size_bytes: bytes.len() as u64, bytes, digest, files, name, version, warnings })
}

/// What `apm install` takes for a published version: `ns/name#version`.
fn install_spec(package: &str, version: &str) -> String {
    format!("{package}#{version}")
}

/// `hlock apm push`: pack the APM package in `dir`, then publish it as `name@<apm.yml version>`.
pub fn push(url: &str, dir: &Path, name: Option<&str>, dry_run: bool) -> Result<Value> {
    let client = Client::for_url(url);
    let packed = pack(dir)?;
    if !version_ok(&packed.version) {
        return Err(err(format!("apm.yml's version {:?} can't be published: letters, digits and ._+- only, starting with a letter or digit", packed.version)));
    }
    let project = slug(packed.name.rsplit('/').next().unwrap_or(&packed.name));
    let package = match name {
        Some(n) => {
            let r = parse_ref(n).ok_or_else(|| err(format!("{n:?} isn't NAMESPACE/NAME (lowercase letters, digits, - and, in the name, _ .)")))?;
            if r.version.is_some() {
                return Err(err(format!("{n}: the version comes from apm.yml ({}); pass NAMESPACE/NAME without @VERSION", packed.version)));
            }
            r.package()
        }
        None if client.has_token() => {
            let who = client.whoami()?;
            let ns = who.pointer("/user/handle").and_then(Value::as_str).ok_or_else(|| err("the registry's whoami has no user.handle; pass NAMESPACE/NAME"))?;
            format!("{ns}/{project}")
        }
        None if dry_run => format!("<your-namespace>/{project}"),
        None => return Err(err(format!("not logged in to {url}: run `hlock login {url}`, or pass NAMESPACE/NAME and set {}", crate::registry::TOKEN_ENV))),
    };
    let version = packed.version.clone();
    let mut out = json!({
        "package": package,
        "version": version,
        "digest": packed.digest,
        "size_bytes": packed.size_bytes,
        "files": packed.files,
        "warnings": packed.warnings,
        "registry": url,
        "apm_registry": apm_base(url),
        "install": install_spec(&package, &version),
    });
    if dry_run {
        out["dry_run"] = json!(true);
        return Ok(out);
    }
    let r = parse_ref(&package).ok_or_else(|| err(format!("{package:?} isn't NAMESPACE/NAME")))?;
    if !client.has_token() {
        return Err(err(format!("not logged in to {url}: run `hlock login {url}` (or set {})", crate::registry::TOKEN_ENV)));
    }
    match client.publish_as(&r, &version, &packed.bytes, "application/zip")? {
        Some(resp) => {
            if let Value::Object(m) = resp {
                for (k, v) in m {
                    if !v.is_null() && k != "files" {
                        out[k] = v;
                    }
                }
            }
            out["published"] = json!(true);
        }
        None => {
            let existing = published(&client, &r)?.into_iter().find(|v| v["version"].as_str() == Some(version.as_str()));
            let digest = existing.as_ref().and_then(|v| v["digest"].as_str()).unwrap_or("unknown digest");
            if digest != packed.digest {
                return Err(err(format!("{package}@{version} is already published with other content ({digest}); versions can't change. Bump `version` in apm.yml")));
            }
            out["published"] = json!(false);
            out["already_published"] = json!(true);
            out["published_at"] = existing.map(|v| v["published_at"].clone()).unwrap_or(Value::Null);
        }
    }
    Ok(out)
}

/// A package's published versions that aren't yanked.
fn published(client: &Client, r: &Ref) -> Result<Vec<Value>> {
    let list = client.versions(r)?;
    let versions = list["versions"].as_array().ok_or_else(|| err(format!("the registry's version list for {} has no `versions`", r.package())))?;
    Ok(versions.iter().filter(|v| v["version"].is_string() && v["yanked"].as_bool() != Some(true)).cloned().collect())
}

/// The newest of `versions`: the highest semver among those that are semver, else the
/// last published.
fn newest(versions: &[Value]) -> Option<&Value> {
    let semver = versions.iter().filter_map(|v| semver::Version::parse(v["version"].as_str()?.trim_start_matches('v')).ok().map(|s| (s, v))).max_by(|a, b| a.0.cmp(&b.0));
    semver.map(|(_, v)| v).or_else(|| versions.iter().max_by_key(|v| v["published_at"].as_str().unwrap_or("")))
}

/// Unpack a package archive (a zip, or a gzip'd tar) into `dest`, refusing anything but
/// plain files at relative paths. Returns the paths.
pub fn unpack(bytes: &[u8], dest: &Path) -> Result<Vec<String>> {
    let bad = |why: String| err(format!("the package archive is unsafe or invalid: {why}; nothing was written"));
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut write = |path: String, size: u64, reader: &mut dyn Read, out: &mut Vec<String>| -> Result<()> {
        if out.len() >= MAX_ENTRIES {
            return Err(bad(format!("more than {MAX_ENTRIES} entries")));
        }
        let path = path.trim_start_matches("./").to_string();
        if path.starts_with('/') || path.contains('\\') || !safe_rel(&path) {
            return Err(bad(format!("{path:?} is outside the archive")));
        }
        total += size;
        if total > MAX_UNPACKED {
            return Err(bad(format!("it unpacks to more than {} MiB", MAX_UNPACKED >> 20)));
        }
        let to = dest.join(&path);
        crate::fsutil::check_within(&path, dest, &to)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::io_at(parent, e))?;
        }
        let mut buf = Vec::new();
        reader.take(MAX_UNPACKED + 1).read_to_end(&mut buf).map_err(|e| bad(e.to_string()))?;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&to).map_err(|e| bad(format!("{path}: {e}")))?;
        f.write_all(&buf).map_err(|e| crate::error::io_at(&to, e))?;
        out.push(path);
        Ok(())
    };
    if bytes.starts_with(b"PK") {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| bad(e.to_string()))?;
        for i in 0..archive.len() {
            let mut f = archive.by_index(i).map_err(|e| bad(e.to_string()))?;
            let name = f.name().to_string();
            if f.is_dir() {
                if !safe_rel(name.trim_end_matches('/')) {
                    return Err(bad(format!("{name:?} is outside the archive")));
                }
                continue;
            }
            if f.is_symlink() || !f.is_file() {
                return Err(bad(format!("{name:?} is a link, not a regular file")));
            }
            let size = f.size();
            write(name, size, &mut f, &mut out)?;
        }
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in archive.entries().map_err(|e| bad(e.to_string()))? {
            let mut entry = entry.map_err(|e| bad(e.to_string()))?;
            let name = String::from_utf8(entry.path_bytes().into_owned()).map_err(|_| bad("a path that isn't UTF-8".into()))?;
            match entry.header().entry_type() {
                tar::EntryType::Directory => {
                    if !safe_rel(name.trim_start_matches("./").trim_end_matches('/')) && name.trim_matches(['.', '/']) != "" {
                        return Err(bad(format!("{name:?} is outside the archive")));
                    }
                    continue;
                }
                tar::EntryType::Regular | tar::EntryType::Continuous => {}
                t => return Err(bad(format!("{name:?} is a {t:?}, not a regular file"))),
            }
            let size = entry.header().size().unwrap_or(0);
            write(name, size, &mut entry, &mut out)?;
        }
    } else {
        return Err(bad("it's neither a zip nor a tar.gz".into()));
    }
    Ok(out)
}

/// `hlock apm pull NS/NAME[@VERSION]`: download a published APM package, check it against
/// its digest and for hidden characters, and write its files into `into` (default `./NAME`).
pub fn pull(url: &str, spec: &str, into: Option<&Path>, force: bool, allow_hidden: &[String]) -> Result<Value> {
    let r = parse_ref(spec).ok_or_else(|| err(format!("{spec:?} isn't NAMESPACE/NAME[@VERSION]")))?;
    let client = Client::for_url(url);
    let versions = published(&client, &r)?;
    let chosen = match &r.version {
        Some(v) => versions.iter().find(|x| x["version"].as_str() == Some(v.as_str())).ok_or_else(|| {
            let known: Vec<&str> = versions.iter().filter_map(|x| x["version"].as_str()).take(10).collect();
            err(format!("{} has no version {v} (published: {})", r.package(), if known.is_empty() { "none".into() } else { known.join(", ") }))
        })?,
        None => newest(&versions).ok_or_else(|| err(format!("{} has no published versions", r.package())))?,
    };
    let version = chosen["version"].as_str().unwrap_or_default().to_string();
    let expected = chosen["digest"].as_str().map(str::to_string).ok_or_else(|| err(format!("the registry gave no digest for {}@{version}", r.package())))?;
    let (bytes, header) = client.download_at_most(&r, &version, MAX_ARCHIVE)?;
    let got = format!("sha256:{}", crate::sha256_hex(&bytes));
    if got != expected || header.as_ref().is_some_and(|h| *h != expected) {
        return Err(err(format!("{}@{version}'s archive doesn't match its digest (expected {expected}, got {got}); nothing was written", r.package())));
    }
    let scratch = crate::pull::scratch_dir()?;
    let mut paths = unpack(&bytes, &scratch.0)?;
    if paths.iter().any(|p| p == "apm.yaml") && !paths.iter().any(|p| p == "apm.yml") {
        std::fs::rename(scratch.0.join("apm.yaml"), scratch.0.join("apm.yml")).map_err(|e| crate::error::io_at(&scratch.0, e))?;
        paths.iter_mut().filter(|p| *p == "apm.yaml").for_each(|p| *p = "apm.yml".into());
    }
    if !paths.iter().any(|p| p == "apm.yml") {
        let setup = paths.iter().any(|p| p == "harness.lock" || p == "harness.toml");
        return Err(err(if setup {
            format!("{} is an hlock setup, not an APM package: pull it with `hlock pull {}`", r.package(), r.package())
        } else {
            format!("{}@{version} has no apm.yml at its root; it isn't an APM package", r.package())
        }));
    }
    let apm_text = std::fs::read_to_string(scratch.0.join("apm.yml")).map_err(|e| crate::error::io_at(&scratch.0, e))?;
    let apm = name_and_version(&apm_text, "apm.yml").ok();
    let mut hidden = Vec::new();
    for p in &paths {
        let b = std::fs::read(scratch.0.join(p)).map_err(|e| crate::error::io_at(&scratch.0.join(p), e))?;
        hidden.extend(crate::scan::scan_bytes(&b, p));
    }
    crate::audit::blocking(&hidden, allow_hidden, &format!("{}@{version}", r.package()))?;
    let dest = into.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(&r.name));
    paths.sort();
    let existing: Vec<&String> = paths.iter().filter(|p| std::fs::symlink_metadata(dest.join(p)).is_ok()).collect();
    if !existing.is_empty() && !force {
        let shown: Vec<String> = existing.iter().take(5).map(|p| format!("  {p}")).collect();
        return Err(Error::Pull(format!(
            "{} already has {} of the package's files:\n{}\nnothing was written; pass --force to replace them, or --into another directory",
            dest.display(),
            existing.len(),
            shown.join("\n")
        )));
    }
    for p in &paths {
        let to = dest.join(p);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::io_at(parent, e))?;
            crate::fsutil::check_within(p, &dest, &to)?;
        }
        if std::fs::symlink_metadata(&to).is_ok_and(|m| m.file_type().is_symlink()) {
            std::fs::remove_file(&to).map_err(|e| crate::error::io_at(&to, e))?;
        }
        std::fs::copy(scratch.0.join(p), &to).map_err(|e| crate::error::io_at(&to, e))?;
    }
    Ok(json!({
        "package": r.package(),
        "version": version,
        "digest": expected,
        "registry": url,
        "apm_registry": apm_base(url),
        "into": dest,
        "files": paths,
        "replaced": existing.len(),
        "apm": apm.map(|(name, version)| json!({ "name": name, "version": version })),
        "install": install_spec(&r.package(), &version),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-apm-registry-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn package(d: &Path) {
        std::fs::write(d.join("apm.yml"), "name: Review Skills\nversion: 1.0\n").unwrap();
        std::fs::create_dir_all(d.join(".apm/skills/review")).unwrap();
        std::fs::write(d.join(".apm/skills/review/SKILL.md"), "Review the diff.\n").unwrap();
        std::fs::write(d.join(".apm/.DS_Store"), "junk").unwrap();
        std::fs::write(d.join("README.md"), "# Review\n").unwrap();
        std::fs::write(d.join("LICENSE"), "MIT\n").unwrap();
        std::fs::write(d.join("harness.lock"), "not in a package").unwrap();
    }

    #[test]
    fn packing_is_deterministic_and_round_trips() {
        let d = tmp("pack");
        package(&d);
        let a = pack(&d).unwrap();
        let b = pack(&d).unwrap();
        assert_eq!(a.bytes, b.bytes);
        assert_eq!((a.name.as_str(), a.version.as_str()), ("Review Skills", "1.0"));
        let paths: Vec<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".apm/skills/review/SKILL.md", "LICENSE", "README.md", "apm.yml"]);
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert_eq!(unpack(&a.bytes, &out).unwrap().len(), 4);
        assert_eq!(std::fs::read_to_string(out.join(".apm/skills/review/SKILL.md")).unwrap(), "Review the diff.\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn packing_needs_a_package() {
        let d = tmp("needs");
        assert!(pack(&d).unwrap_err().to_string().contains("no apm.yml"));
        std::fs::write(d.join("apm.yml"), "name: x\n").unwrap();
        assert!(pack(&d).unwrap_err().to_string().contains("no `version`"));
        std::fs::write(d.join("apm.yml"), "name: x\nversion: 1.0.0\n").unwrap();
        assert!(pack(&d).unwrap_err().to_string().contains("hlock push"));
        std::fs::create_dir_all(d.join(".apm/skills/x")).unwrap();
        std::fs::write(d.join(".apm/skills/x/SKILL.md"), "hi\u{202E}there\n").unwrap();
        assert!(pack(&d).unwrap_err().to_string().contains("U+202E"));
        std::fs::write(d.join(".apm/skills/x/SKILL.md"), "hi\n").unwrap();
        std::fs::write(d.join(".apm/mcp.json"), r#"{"env": {"GITHUB_TOKEN": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"}}"#).unwrap();
        assert!(pack(&d).unwrap_err().to_string().contains("GITHUB_TOKEN"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unsafe_zips_are_refused() {
        let d = tmp("unsafe");
        let zipped = |name: &str, link: bool| {
            let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let o = zip::write::SimpleFileOptions::default();
            if link {
                z.add_symlink(name, "/etc/passwd", o).unwrap();
            } else {
                z.start_file(name, o).unwrap();
                z.write_all(b"x").unwrap();
            }
            z.finish().unwrap().into_inner()
        };
        for (bytes, why) in [(zipped("../evil", false), "outside"), (zipped("/etc/evil", false), "outside"), (zipped("apm.yml", true), "link")] {
            let out = d.join(why);
            let _ = std::fs::remove_dir_all(&out);
            std::fs::create_dir_all(&out).unwrap();
            let e = unpack(&bytes, &out).unwrap_err().to_string();
            assert!(e.contains(why), "{why}: {e}");
        }
        assert!(unpack(b"not an archive", &d).unwrap_err().to_string().contains("neither"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_newest_version() {
        let v = |s: &str, at: &str| json!({ "version": s, "published_at": at });
        assert_eq!(newest(&[v("1.2.0", "a"), v("1.10.0", "b"), v("1.9.0", "c")]).unwrap()["version"], "1.10.0");
        assert_eq!(newest(&[v("stable", "2026-01"), v("edge", "2026-02")]).unwrap()["version"], "edge");
        assert!(newest(&[]).is_none());
    }
}
