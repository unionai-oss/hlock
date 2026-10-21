//! Hidden-Unicode scanning (`specs/audit.md` §3): characters that hide text from a reviewer
//! but not from an agent. The tiers are APM's (its `content_scanner`), so a file that
//! passes one tool passes the other:
//!
//! - **critical** (blocks `pull`, `add`, `sync` and `launch`): Unicode tag characters
//!   (U+E0001–E007F), bidi overrides and isolates (U+202A–202E, U+2066–2069), variation
//!   selectors 17–256 (U+E0100–E01EF), and unpaired surrogates.
//! - **warning** (printed and recorded): zero-width characters, a BOM after the first
//!   byte, bidi marks, invisible operators, variation selectors 1–15, soft hyphens,
//!   annotation and deprecated format characters.
//! - **info** (with `--verbose`): non-breaking and unusual spaces, a leading BOM, the emoji
//!   presentation selector, a zero-width joiner between emoji.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How bad a hidden character is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// One hidden character, where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The file, as the caller names it (relative to what was scanned).
    pub file: String,
    /// 1-based line and column (the column counts characters).
    pub line: usize,
    pub column: usize,
    /// `U+202E`.
    pub codepoint: String,
    pub severity: Severity,
    /// `tag-character`, `bidi-override`, `variation-selector`, `zero-width`, …
    pub category: String,
    pub description: String,
}

impl Finding {
    /// `style.md:3:14: U+202E right-to-left override (bidi-override)`.
    pub fn label(&self) -> String {
        format!("{}:{}:{}: {} {} ({})", self.file, self.line, self.column, self.codepoint, self.description, self.category)
    }
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2300..=0x23FF | 0x2B00..=0x2BFF)
}

/// What a character is, if it's one of the hidden ones.
fn classify(c: char) -> Option<(Severity, &'static str, &'static str)> {
    use Severity::*;
    Some(match c as u32 {
        0xE0001..=0xE007F => (Critical, "tag-character", "Unicode tag character"),
        0x202A => (Critical, "bidi-override", "left-to-right embedding"),
        0x202B => (Critical, "bidi-override", "right-to-left embedding"),
        0x202C => (Critical, "bidi-override", "pop directional formatting"),
        0x202D => (Critical, "bidi-override", "left-to-right override"),
        0x202E => (Critical, "bidi-override", "right-to-left override"),
        0x2066 => (Critical, "bidi-override", "left-to-right isolate"),
        0x2067 => (Critical, "bidi-override", "right-to-left isolate"),
        0x2068 => (Critical, "bidi-override", "first strong isolate"),
        0x2069 => (Critical, "bidi-override", "pop directional isolate"),
        0xE0100..=0xE01EF => (Critical, "variation-selector", "variation selector (17–256)"),
        0x200B => (Warning, "zero-width", "zero-width space"),
        0x200C => (Warning, "zero-width", "zero-width non-joiner"),
        0x200D => (Warning, "zero-width", "zero-width joiner"),
        0x2060 => (Warning, "zero-width", "word joiner"),
        0xFE00..=0xFE0D => (Warning, "variation-selector", "variation selector (1–14)"),
        0xFE0E => (Warning, "variation-selector", "text presentation selector"),
        0x00AD => (Warning, "invisible-formatting", "soft hyphen"),
        0x200E => (Warning, "bidi-mark", "left-to-right mark"),
        0x200F => (Warning, "bidi-mark", "right-to-left mark"),
        0x061C => (Warning, "bidi-mark", "Arabic letter mark"),
        0x2061..=0x2064 => (Warning, "invisible-formatting", "invisible math operator"),
        0xFFF9..=0xFFFB => (Warning, "annotation-marker", "interlinear annotation character"),
        0x206A..=0x206F => (Warning, "deprecated-formatting", "deprecated format character"),
        0x2028 => (Warning, "line-separator", "line separator"),
        0x2029 => (Warning, "line-separator", "paragraph separator"),
        0x034F => (Warning, "invisible-formatting", "combining grapheme joiner"),
        0x115F | 0x1160 | 0x3164 | 0xFFA0 => (Warning, "invisible-filler", "Hangul filler"),
        0x202F => (Info, "unusual-whitespace", "narrow no-break space"),
        0xFEFF => (Warning, "zero-width", "byte order mark in the middle of the file"),
        0xFE0F => (Info, "emoji-presentation", "emoji presentation selector"),
        0x00A0 => (Info, "unusual-whitespace", "no-break space"),
        0x2000..=0x200A => (Info, "unusual-whitespace", "unusual space"),
        0x205F => (Info, "unusual-whitespace", "medium mathematical space"),
        0x3000 => (Info, "unusual-whitespace", "ideographic space"),
        0x180E => (Info, "unusual-whitespace", "Mongolian vowel separator"),
        _ => return None,
    })
}

/// `text` split into lines at `\n`, `\r\n`, or a lone `\r`.
fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, bytes) = (0, text.as_bytes());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                out.push(&text[start..i]);
                i += 1;
                start = i + 1;
            }
            b'\r' => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&text[start..]);
    out
}

/// `s` with hidden and control characters shown as `<U+202E>`, for printing a command a
/// user is asked to approve: what's shown is what runs.
pub fn visible(s: &str) -> String {
    s.chars().map(|c| if classify(c).is_some_and(|(sev, _, _)| sev >= Severity::Warning) || (c.is_control() && c != ' ') { format!("<U+{:04X}>", c as u32) } else { c.to_string() }).collect()
}

/// One line of text with its hidden characters shown as `<U+202E>`, as [`visible`] does,
/// but keeping tabs: for showing file content in a diff.
pub fn visible_line(s: &str) -> String {
    s.chars().map(|c| if classify(c).is_some_and(|(sev, _, _)| sev >= Severity::Warning) || (c.is_control() && c != '\t') { format!("<U+{:04X}>", c as u32) } else { c.to_string() }).collect()
}

/// Hidden characters in `text`, named `file` in the findings.
pub fn scan_text(text: &str, file: &str) -> Vec<Finding> {
    if text.is_ascii() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (ln, line) in lines(text).into_iter().enumerate() {
        if line.is_ascii() {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for (col, &c) in chars.iter().enumerate() {
            let Some((mut sev, mut cat, mut desc)) = classify(c) else { continue };
            if c == '\u{FEFF}' && ln == 0 && col == 0 {
                (sev, cat, desc) = (Severity::Info, "bom", "byte order mark");
            }
            if c == '\u{200D}' {
                // a joiner inside an emoji sequence (a family emoji) is how emoji are spelled
                let before = chars[..col].iter().rev().find(|&&p| p != '\u{FE0F}' && !matches!(p as u32, 0x1F3FB..=0x1F3FF));
                let after = chars.get(col + 1);
                if before.is_some_and(|&b| is_emoji(b)) && after.is_some_and(|&a| is_emoji(a)) {
                    (sev, cat, desc) = (Severity::Info, "zero-width", "zero-width joiner (emoji sequence)");
                }
            }
            out.push(Finding { file: file.to_string(), line: ln + 1, column: col + 1, codepoint: format!("U+{:04X}", c as u32), severity: sev, category: cat.into(), description: desc.into() });
        }
    }
    out
}

/// Bytes that aren't UTF-8: surrogates encoded as UTF-8 (`ED A0..BF ..`) are critical,
/// anything else is binary and isn't scanned (as APM does).
fn scan_invalid(bytes: &[u8], file: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let (mut line, mut col) = (1, 1);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0xED && bytes.get(i + 1).is_some_and(|b| (0xA0..=0xBF).contains(b)) {
            let cp = 0xD000 | ((u32::from(bytes[i + 1]) & 0x3F) << 6) | (u32::from(*bytes.get(i + 2).unwrap_or(&0x80)) & 0x3F);
            out.push(Finding { file: file.into(), line, column: col, codepoint: format!("U+{cp:04X}"), severity: Severity::Critical, category: "invalid-surrogate".into(), description: "unpaired surrogate".into() });
            i += 3;
            col += 1;
            continue;
        }
        if bytes[i] == b'\n' {
            line += 1;
            col = 1;
        } else if bytes[i] & 0xC0 != 0x80 {
            col += 1;
        }
        i += 1;
    }
    out
}

/// The largest file scanned; a larger one is reported as unscanned (critical).
const MAX_SCAN: usize = 20 << 20;

/// Extensions of files that are never text (images, archives, fonts): not scanned.
const BINARY: [&str; 18] = ["png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tgz", "bz2", "xz", "woff", "woff2", "ttf", "otf", "mp4", "wasm"];

fn is_binary_name(file: &str) -> bool {
    file.rsplit('/').next().and_then(|f| f.rsplit_once('.')).is_some_and(|(_, e)| BINARY.contains(&e.to_ascii_lowercase().as_str()))
}

/// Hidden characters in a file's bytes. A file that isn't valid UTF-8 (or has a NUL byte)
/// is scanned anyway, decoding what it can: one stray byte mustn't turn the scan off. Only
/// files named as binary (`.png`, `.zip`, …) are skipped.
pub fn scan_bytes(bytes: &[u8], file: &str) -> Vec<Finding> {
    if is_binary_name(file) {
        return Vec::new();
    }
    match std::str::from_utf8(bytes) {
        Ok(t) if !t.contains('\0') => scan_text(t, file),
        _ => {
            let mut out = scan_invalid(bytes, file);
            out.extend(scan_text(&String::from_utf8_lossy(bytes).replace('\0', ""), file));
            out
        }
    }
}

/// Hidden characters in a parsed config document's strings: `"\u202e"` is ASCII in the
/// file and a real character once the harness reads it.
pub fn scan_value(v: &serde_json::Value, file: &str) -> Vec<Finding> {
    fn walk(v: &serde_json::Value, file: &str, path: &mut Vec<String>, out: &mut Vec<Finding>) {
        match v {
            serde_json::Value::String(s) => {
                for mut f in scan_text(s, file) {
                    f.description = format!("{} in /{}", f.description, path.join("/"));
                    f.line = 0;
                    f.column = 0;
                    out.push(f);
                }
            }
            serde_json::Value::Array(a) => a.iter().enumerate().for_each(|(i, x)| {
                path.push(i.to_string());
                walk(x, file, path, out);
                path.pop();
            }),
            serde_json::Value::Object(m) => m.iter().for_each(|(k, x)| {
                for mut f in scan_text(k, file) {
                    f.description = format!("{} in a key under /{}", f.description, path.join("/"));
                    f.line = 0;
                    f.column = 0;
                    out.push(f);
                }
                path.push(k.clone());
                walk(x, file, path, out);
                path.pop();
            }),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, file, &mut Vec::new(), &mut out);
    out
}

/// Files under `root` (or `root` itself), sorted, not following symlinks and skipping `.git`.
fn files(root: &Path) -> Vec<PathBuf> {
    fn rec(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if e.file_name() == ".git" || t.is_symlink() {
                continue;
            }
            if t.is_dir() {
                rec(&e.path(), out);
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    let mut out = Vec::new();
    match std::fs::symlink_metadata(root) {
        Ok(m) if m.is_file() => out.push(root.to_path_buf()),
        Ok(m) if m.is_dir() => rec(root, &mut out),
        _ => {}
    }
    out.sort();
    out
}

/// The files a scan reads under `root` (or `root` itself).
pub fn text_files(root: &Path) -> Vec<PathBuf> {
    files(root)
}

/// Hidden characters in a file or every file under a directory. Files are named by
/// `label` joined with their path under `root` (`label` itself for a file).
pub fn scan_path(root: &Path, label: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let is_file = root.is_file();
    for f in files(root) {
        let name = if is_file {
            label.to_string()
        } else {
            let rel = f.strip_prefix(root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            if label.is_empty() { rel } else { format!("{}/{rel}", label.trim_end_matches('/')) }
        };
        if is_binary_name(&name) {
            continue;
        }
        // a file too large to scan isn't placed unscanned
        if std::fs::metadata(&f).is_ok_and(|m| m.len() > MAX_SCAN as u64) {
            out.push(Finding { file: name, line: 0, column: 0, codepoint: String::new(), severity: Severity::Critical, category: "unscanned".into(), description: format!("larger than {} MB, too large to scan", MAX_SCAN >> 20) });
            continue;
        }
        let Ok(bytes) = crate::fsutil::read_regular(&f, MAX_SCAN) else { continue };
        let found = scan_bytes(&bytes, &name);
        // a config file's escapes (`\u202e`) become characters when it's read
        let parsed: Option<serde_json::Value> = match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
            Some(e) if e == "json" => std::str::from_utf8(&bytes).ok().and_then(|t| serde_json::from_str(t).ok()),
            Some(e) if e == "toml" => std::str::from_utf8(&bytes).ok().and_then(|t| toml::from_str::<toml::Value>(t).ok()).and_then(|v| serde_json::to_value(v).ok()),
            Some(e) if e == "yaml" || e == "yml" => std::str::from_utf8(&bytes).ok().and_then(|t| serde_norway::from_str(t).ok()),
            _ => None,
        };
        let raw: std::collections::BTreeSet<String> = found.iter().map(|x| x.codepoint.clone()).collect();
        out.extend(found);
        if let Some(v) = parsed {
            // only what the raw scan didn't already see
            out.extend(scan_value(&v, &name).into_iter().filter(|x| !raw.contains(&x.codepoint)));
        }
    }
    out
}

/// What a lock entry's `scan` key says about findings: `clean`, `warnings:N`, or
/// `critical:N` (which only an exception, `allowed:critical`, lets through).
pub fn summary(findings: &[Finding]) -> String {
    let critical = findings.iter().filter(|f| f.severity == Severity::Critical).count();
    let warnings = findings.iter().filter(|f| f.severity == Severity::Warning).count();
    if critical > 0 {
        format!("critical:{critical}")
    } else if warnings > 0 {
        format!("warnings:{warnings}")
    } else {
        "clean".into()
    }
}

/// The `scan` key for an entry: [`summary`], or `allowed:critical` when the critical
/// findings were let through on purpose (`--allow-hidden`).
pub fn record(findings: &[Finding], allowed: bool) -> String {
    let s = summary(findings);
    if allowed && s.starts_with("critical") { "allowed:critical".into() } else { s }
}

/// `text` without its critical and warning characters (info ones, and a zero-width joiner
/// inside an emoji sequence, stay), as `hlock audit --strip` writes it.
pub fn strip(text: &str) -> String {
    // ZWNJ and bidi marks are part of ordinary Persian, Hebrew and Indic text: kept
    let keep = |f: &Finding| matches!(f.codepoint.as_str(), "U+200C" | "U+200E" | "U+200F" | "U+061C");
    let hidden: std::collections::BTreeSet<(usize, usize)> = scan_text(text, "").into_iter().filter(|f| f.severity >= Severity::Warning && !keep(f)).map(|f| (f.line, f.column)).collect();
    if hidden.is_empty() {
        return text.to_string();
    }
    // walk the text as `lines` does, keeping the line endings as they were
    let mut out = String::with_capacity(text.len());
    let (mut ln, mut col) = (1, 1);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push(c);
                (ln, col) = (ln + 1, 1);
            }
            '\r' => {
                out.push(c);
                if chars.peek() == Some(&'\n') {
                    out.push(chars.next().unwrap_or('\n'));
                }
                (ln, col) = (ln + 1, 1);
            }
            _ => {
                if !hidden.contains(&(ln, col)) {
                    out.push(c);
                }
                col += 1;
            }
        }
    }
    out
}

/// Strip the hidden characters from every text file under `root` (or `root` itself).
/// Returns the files changed.
pub fn strip_path(root: &Path) -> crate::error::Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for f in files(root) {
        let Ok(bytes) = crate::fsutil::read_regular(&f, MAX_SCAN) else { continue };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else { continue };
        let clean = strip(&text);
        if clean != text {
            crate::fsutil::write_atomic(&f, clean)?;
            changed.push(f);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sev(text: &str) -> Vec<(String, Severity)> {
        scan_text(text, "f").into_iter().map(|f| (f.codepoint, f.severity)).collect()
    }

    #[test]
    fn tiers_match_apm() {
        assert!(scan_text("plain ascii", "f").is_empty());
        assert_eq!(sev("a\u{202E}b"), [("U+202E".into(), Severity::Critical)]);
        assert_eq!(sev("x\u{E0041}"), [("U+E0041".into(), Severity::Critical)]);
        assert_eq!(sev("x\u{E0100}"), [("U+E0100".into(), Severity::Critical)]);
        assert_eq!(sev("x\u{2066}y\u{2069}"), [("U+2066".into(), Severity::Critical), ("U+2069".into(), Severity::Critical)]);
        assert_eq!(sev("a\u{200B}b"), [("U+200B".into(), Severity::Warning)]);
        assert_eq!(sev("a\u{200F}b\u{00AD}"), [("U+200F".into(), Severity::Warning), ("U+00AD".into(), Severity::Warning)]);
        assert_eq!(sev("a\u{FEFF}b"), [("U+FEFF".into(), Severity::Warning)], "a BOM after the start");
        assert_eq!(sev("\u{FEFF}start"), [("U+FEFF".into(), Severity::Info)], "a leading BOM");
        assert_eq!(sev("a\u{00A0}b"), [("U+00A0".into(), Severity::Info)]);
        // a joiner between emoji is how a family emoji is spelled
        assert_eq!(sev("\u{1F468}\u{200D}\u{1F469}"), [("U+200D".into(), Severity::Info)]);
        assert_eq!(sev("\u{1F44D}\u{1F3FD}\u{200D}\u{2642}"), [("U+200D".into(), Severity::Info)]);
        assert_eq!(sev("é ü ñ 日本語 ✓ →"), [], "ordinary non-ASCII is fine");
    }

    #[test]
    fn positions_count_characters_from_one() {
        let f = &scan_text("ok\nab日\u{202E}c", "rules/style.md")[0];
        assert_eq!((f.line, f.column, f.file.as_str()), (2, 4, "rules/style.md"));
        assert_eq!(f.label(), "rules/style.md:2:4: U+202E right-to-left override (bidi-override)");
    }

    #[test]
    fn surrogates_binary_and_summaries() {
        let f = scan_bytes(b"ok\n\xED\xA0\x80x", "f");
        assert_eq!((f[0].codepoint.as_str(), f[0].severity, f[0].line), ("U+D800", Severity::Critical, 2));
        // one stray byte doesn't turn the scan off; a file named as binary isn't scanned
        assert_eq!(scan_bytes("x\u{202E}\0".as_bytes(), "a.md")[0].codepoint, "U+202E");
        assert!(scan_bytes(&[b"x".as_slice(), "\u{202E}".as_bytes(), b"\xff"].concat(), "a.md").iter().any(|f| f.codepoint == "U+202E"));
        assert!(scan_bytes("\u{202E}".as_bytes(), "logo.png").is_empty());
        assert_eq!(scan_text("a\rb\u{202E}", "f")[0].line, 2, "a lone CR ends a line");
        assert_eq!(scan_text("a\u{3164}b\u{2028}", "f").len(), 2);
        assert_eq!(visible("sh -c x\u{202E}y"), "sh -c x<U+202E>y");
        let v: serde_json::Value = serde_json::from_str(r#"{"hooks": {"Stop": [{"command": "echo \u202e hi"}]}}"#).unwrap();
        assert_eq!(scan_value(&v, "settings.json")[0].codepoint, "U+202E");
        assert_eq!(summary(&scan_text("a\u{200B}\u{200B}", "f")), "warnings:2");
        assert_eq!(summary(&scan_text("a\u{202E}", "f")), "critical:1");
        assert_eq!(record(&scan_text("a\u{202E}", "f"), true), "allowed:critical");
        assert_eq!(summary(&[]), "clean");
    }

    #[test]
    fn strip_removes_what_hides_text() {
        assert_eq!(strip("do\u{202E}this\u{200B}\nkeep\u{00A0}space"), "dothis\nkeep\u{00A0}space");
        assert_eq!(strip("\u{0645}\u{06CC}\u{200C}\u{062E}\u{200F}\r\nx\u{202E}"), "\u{0645}\u{06CC}\u{200C}\u{062E}\u{200F}\r\nx", "ZWNJ and bidi marks are text; line endings stay");
        assert_eq!(strip("\u{1F468}\u{200D}\u{1F469}"), "\u{1F468}\u{200D}\u{1F469}");
        let d = std::env::temp_dir().join(format!("hlock-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/a.md"), "x\u{E0041}y").unwrap();
        std::fs::write(d.join("b.md"), "clean").unwrap();
        let found = scan_path(&d, "skill");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, "skill/sub/a.md");
        assert_eq!(strip_path(&d).unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(d.join("sub/a.md")).unwrap(), "xy");
        let _ = std::fs::remove_dir_all(&d);
    }
}
