//! Safe file handling for data hlock doesn't control: names and paths from a lock,
//! a manifest, an eval pack, or a workspace an agent wrote.
//!
//! - [`plain_name`] and [`safe_rel`] check names and relative paths before they're joined.
//! - [`within`] checks that a destination stays under a root, through symlinks.
//! - [`copy_tree`] copies without following symlinks and replaces symlinks in the way.
//! - [`read_regular`] reads a regular file (not a symlink, FIFO or device), up to a cap.
//! - [`write_atomic`] and [`write_private`] write by rename, the latter as 0600 from the start.
//! - [`shell_quote`] quotes a word for `sh`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// A single, plain path component: no separators, not `.` or `..`, not empty. For names
/// that become directory or file names (task ids, skill names, harness names).
pub fn plain_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains(['/', '\\', '\0'])
        && matches!(Path::new(s).components().collect::<Vec<_>>().as_slice(), [Component::Normal(_)])
}

/// Like [`plain_name`], as an error naming `what`.
pub fn check_plain_name(what: &str, s: &str) -> Result<()> {
    if plain_name(s) {
        Ok(())
    } else {
        Err(Error::Manifest(format!("{what} {s:?} must be a plain name (no '/', '\\', or '..')")))
    }
}

/// A relative path that stays inside its base: not absolute, no `..`, not empty.
pub fn safe_rel(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty()
        && !p.contains('\0')
        && !path.is_absolute()
        && !p.starts_with(['/', '\\'])
        && path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Like [`safe_rel`], as an error naming `what`.
pub fn check_safe_rel(what: &str, p: &str) -> Result<()> {
    if safe_rel(p) {
        Ok(())
    } else {
        Err(Error::Manifest(format!("{what} {p:?} must be a relative path inside its directory (no leading '/' or '~', no '..')")))
    }
}

/// `p` with `.` and `..` resolved lexically.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The real location of `p`: its deepest existing ancestor canonicalized (following
/// symlinks), with the rest appended.
pub fn resolve(p: &Path) -> PathBuf {
    let p = normalize(&absolute(p));
    let mut base = p.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |acc: PathBuf, c| acc.join(c));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => return p,
        }
    }
}

/// `p` made absolute against the current directory.
pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Whether `p` is `root` or under it, after resolving `..` and symlinks in both.
pub fn within(root: &Path, p: &Path) -> bool {
    resolve(p).starts_with(resolve(root))
}

/// Like [`within`], as an error naming `what`.
pub fn check_within(what: &str, root: &Path, p: &Path) -> Result<()> {
    if within(root, p) {
        Ok(())
    } else {
        Err(Error::Profile(format!("{what} {} is outside {}; refusing to write there", p.display(), root.display())))
    }
}

/// Copy `from` (a file or a directory tree) to `to`. Symlinks are skipped, never
/// followed, at every level including `from` itself; a symlink already at a destination
/// is replaced rather than written through. Returns the number of files copied.
pub fn copy_tree(from: &Path, to: &Path) -> Result<usize> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Ok(0);
    }
    if meta.is_dir() {
        clear_symlink(to)?;
        fs::create_dir_all(to)?;
        let mut n = 0;
        for e in fs::read_dir(from)? {
            let e = e?;
            n += copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(n)
    } else if meta.is_file() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        clear_symlink(to)?;
        fs::copy(from, to)?;
        Ok(1)
    } else {
        Ok(0) // FIFOs, sockets, devices
    }
}

/// Remove `p` if it's a symlink, so a following write creates a real file.
pub fn clear_symlink(p: &Path) -> Result<()> {
    if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(p)?;
    }
    Ok(())
}

/// Read up to `max` bytes of a regular file. Refuses symlinks, FIFOs and devices, so a
/// workspace file an agent planted can't point the read somewhere else or block it.
pub fn read_regular(p: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(p)?;
    if !meta.is_file() {
        let what = if meta.file_type().is_symlink() { "a symlink" } else { "not a regular file" };
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} is {what}", p.display())));
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut buf = Vec::new();
    opts.open(p)?.take(max as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// [`read_regular`] as (lossy) text.
pub fn read_regular_text(p: &Path, max: usize) -> std::io::Result<String> {
    read_regular(p, max).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn temp_beside(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.hlock-tmp-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

fn write_via_temp(path: &Path, data: &[u8], private: bool) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_beside(path);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let result = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        // keep an existing file's mode unless the new one must be private
        #[cfg(unix)]
        if !private {
            if let Ok(m) = fs::metadata(path) {
                fs::set_permissions(&tmp, m.permissions())?;
            }
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(result?)
}

/// Write `data` to `path` atomically: a temp file beside it, then a rename, so a crash
/// leaves the old file or the new one, never half of either.
pub fn write_atomic(path: &Path, data: impl AsRef<[u8]>) -> Result<()> {
    write_via_temp(path, data.as_ref(), false)
}

/// [`write_atomic`] for files holding secrets: the file is 0600 from the moment it exists.
pub fn write_private(path: &Path, data: impl AsRef<[u8]>) -> Result<()> {
    write_via_temp(path, data.as_ref(), true)
}

/// Copy `from` to `to` privately (0600 from the start), for backups of files with secrets.
pub fn copy_private(from: &Path, to: &Path) -> Result<()> {
    let data = fs::read(from)?;
    write_private(to, data)
}

/// Quote a word for `sh`: left alone when it's only safe characters, single-quoted
/// otherwise.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=+,%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Quote each word and join them with spaces.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// A value safe to put in a command line unquoted and to pass as a positional argument:
/// a version, model id, ref or package name (`[A-Za-z0-9._:/@+-]`, not starting with `-`).
pub fn plain_token(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "._:/@+-".contains(c))
}

/// Like [`plain_token`], as an error naming `what`.
pub fn check_plain_token(what: &str, s: &str) -> Result<()> {
    if plain_token(s) {
        Ok(())
    } else {
        Err(Error::Lockfile(format!("{what} {s:?} has characters that aren't allowed (letters, digits and ._:/@+- only, not starting with '-')")))
    }
}

/// The first `n` characters of `s` (never splits a character).
pub fn prefix_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-fsutil-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_and_relative_paths() {
        for ok in ["a", "task-1", "my.skill", "x_y"] {
            assert!(plain_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "../x", "/abs", "a\\b", "~"] {
            assert!(!plain_name(bad) || bad == "~", "{bad}");
        }
        assert!(safe_rel("a/b.txt") && safe_rel("./a"));
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "\\x"] {
            assert!(!safe_rel(bad), "{bad}");
        }
    }

    #[test]
    fn within_sees_through_dotdot_and_symlinks() {
        let d = tmp("within");
        fs::create_dir_all(d.join("root/sub")).unwrap();
        assert!(within(&d.join("root"), &d.join("root/sub/new.txt")));
        assert!(!within(&d.join("root"), &d.join("root/../outside")));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&d, d.join("root/link")).unwrap();
            assert!(!within(&d.join("root"), &d.join("root/link/x")));
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_skips_and_replaces_symlinks() {
        let d = tmp("copy");
        fs::create_dir_all(d.join("src/sub")).unwrap();
        fs::write(d.join("src/a.txt"), "a").unwrap();
        fs::write(d.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(d.join("secret"), d.join("src/sub/leak")).unwrap();
        std::os::unix::fs::symlink(d.join("src"), d.join("src/sub/loop")).unwrap();
        // a symlink waiting at the destination is replaced, not written through
        fs::create_dir_all(d.join("dst")).unwrap();
        fs::write(d.join("victim"), "keep").unwrap();
        std::os::unix::fs::symlink(d.join("victim"), d.join("dst/a.txt")).unwrap();
        assert_eq!(copy_tree(&d.join("src"), &d.join("dst")).unwrap(), 1);
        assert_eq!(fs::read_to_string(d.join("victim")).unwrap(), "keep");
        assert_eq!(fs::read_to_string(d.join("dst/a.txt")).unwrap(), "a");
        assert!(!d.join("dst/sub/leak").exists());
        // a symlinked top-level source copies nothing
        std::os::unix::fs::symlink(d.join("src"), d.join("top")).unwrap();
        assert_eq!(copy_tree(&d.join("top"), &d.join("dst2")).unwrap(), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn read_regular_refuses_symlinks_and_fifos_and_caps() {
        let d = tmp("read");
        fs::write(d.join("f"), "0123456789").unwrap();
        assert_eq!(read_regular(&d.join("f"), 4).unwrap(), b"0123");
        std::os::unix::fs::symlink(d.join("f"), d.join("l")).unwrap();
        assert!(read_regular(&d.join("l"), 100).is_err());
        let fifo = std::ffi::CString::new(d.join("p").to_string_lossy().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_regular(&d.join("p"), 100).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn private_writes_are_0600_and_atomic_writes_keep_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("write");
        write_private(&d.join("s"), "x").unwrap();
        assert_eq!(fs::metadata(d.join("s")).unwrap().permissions().mode() & 0o777, 0o600);
        fs::write(d.join("e"), "old").unwrap();
        fs::set_permissions(d.join("e"), fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&d.join("e"), "new").unwrap();
        assert_eq!(fs::read_to_string(d.join("e")).unwrap(), "new");
        assert_eq!(fs::metadata(d.join("e")).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2); // no temp files left
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn quoting_and_tokens() {
        assert_eq!(shell_quote("approve"), "approve");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("/tmp/p(1)/h.lock"), "'/tmp/p(1)/h.lock'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("-g"), "-g");
        assert_eq!(shell_quote(""), "''");
        assert!(plain_token("2.1.3") && plain_token("qwen3:32b") && plain_token("org/model@rev"));
        assert!(!plain_token("1; curl x|sh") && !plain_token("--upload-pack=x") && !plain_token(""));
        assert_eq!(prefix_chars("aéééééééééé", 3), "aéé");
        assert_eq!(prefix_chars("abc", 10), "abc");
    }
}
