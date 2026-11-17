//! `hlock lint`: instruction text written for an earlier model that a newer one follows too
//! literally (a line telling it to think less, a token budget in the prompt, an absolute rule
//! without its reason). The rules are data, in `lint_rules.toml`, and each names the target
//! models it applies to. Findings carry exact byte spans, so a tool can propose deleting or
//! rewriting the lines they point at.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::lockfile::Lockfile;
use crate::profiles::Ctx;

const RULES: &str = include_str!("lint_rules.toml");

/// One rule, as `lint_rules.toml` writes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Rule {
    pub id: String,
    pub category: String,
    /// `warning` or `info`.
    pub severity: String,
    /// `*`, or model ids and families (`fable-5-2`).
    pub models: Vec<String>,
    pub patterns: Vec<String>,
    #[serde(default)]
    pub unless: Vec<String>,
    /// Report the whole sentence, and only when it's the last in its block.
    #[serde(default)]
    pub sentence: bool,
    /// Checked only where no other rule matched.
    #[serde(default)]
    pub fallback: bool,
    pub message: String,
    pub fix: String,
    pub source: String,
    #[serde(default, skip_serializing)]
    pub examples: Vec<String>,
    #[serde(default, skip_serializing)]
    pub counterexamples: Vec<String>,
}

#[derive(Deserialize)]
struct RuleFile {
    rule: Vec<Rule>,
}

struct Compiled {
    rule: Rule,
    patterns: Vec<Regex>,
    unless: Vec<Regex>,
}

fn compile(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).unwrap_or_else(|e| panic!("lint_rules.toml: bad pattern {p:?}: {e}"))
}

fn compiled() -> &'static [Compiled] {
    static C: OnceLock<Vec<Compiled>> = OnceLock::new();
    C.get_or_init(|| {
        let f: RuleFile = toml::from_str(RULES).expect("lint_rules.toml parses");
        f.rule
            .into_iter()
            .map(|r| Compiled { patterns: r.patterns.iter().map(|p| compile(p)).collect(), unless: r.unless.iter().map(|p| compile(p)).collect(), rule: r })
            .collect()
    })
}

/// Every rule.
pub fn rules() -> Vec<Rule> {
    compiled().iter().map(|c| c.rule.clone()).collect()
}

/// `claude-Fable-5.2` → `claude-fable-5-2`.
fn normalize(model: &str) -> String {
    model.trim().to_ascii_lowercase().chars().map(|c| if matches!(c, '.' | '_' | ' ' | '@' | ':') { '-' } else { c }).collect()
}

/// Whether a rule applies to a target model. `None` (no model known) gets only the rules
/// for every model; `all` gets every rule. A family matches as a whole part of the id:
/// `fable-5-2` matches `claude-fable-5-2-20261001` and `anthropic/claude-fable-5-2`, not
/// `fable-5-20`.
pub fn applies(rule: &Rule, model: Option<&str>) -> bool {
    if rule.models.iter().any(|m| m == "*") {
        return true;
    }
    let Some(model) = model.map(normalize).filter(|m| !m.is_empty()) else { return false };
    if model == "all" {
        return true;
    }
    rule.models.iter().map(|m| normalize(m)).any(|fam| {
        model.match_indices(&fam).any(|(i, _)| {
            let before = model[..i].chars().next_back();
            let after = model[i + fam.len()..].chars().next();
            !before.is_some_and(|c| c.is_ascii_alphanumeric()) && !after.is_some_and(|c| c.is_ascii_digit())
        })
    })
}

/// One stale line, where it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LintFinding {
    /// The rule id; `hlock/lint/<rule>` in SARIF.
    pub rule: String,
    pub category: String,
    pub severity: String,
    /// The file, as the lock names it (what audit's findings are named by).
    pub file: String,
    /// The file read, to edit (empty for text passed in).
    pub path: String,
    /// The lock entry it's in (`skill`, `instructions`, `subagent`, `command`; `file` for
    /// `--file`) and its name.
    pub kind: String,
    pub entry: String,
    /// 1-based lines and columns of the match (columns count characters; `end_column`
    /// is one past the last character).
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    /// Byte offsets of the match in the file: `text[start..end]`.
    pub start: usize,
    pub end: usize,
    /// Byte offsets of the whole lines `line..=end_line`, with the last line's ending:
    /// what deleting those lines removes.
    pub line_start: usize,
    pub line_end: usize,
    /// The match as written.
    pub matched: String,
    pub message: String,
    pub fix: String,
    pub source: String,
}

impl LintFinding {
    /// `AGENTS.md:12:3: never retry (stale-caution)`.
    pub fn label(&self) -> String {
        format!("{}:{}:{}: {} ({})", self.file, self.line, self.column, self.matched, self.rule)
    }
}

/// A line of the file: its byte range (without the ending), and where the next starts.
struct Line {
    start: usize,
    end: usize,
    next: usize,
}

fn split_lines(text: &str) -> Vec<Line> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\n' => {
                out.push(Line { start, end: i, next: i + 1 });
                start = i + 1;
            }
            b'\r' => {
                let next = if b.get(i + 1) == Some(&b'\n') { i + 2 } else { i + 1 };
                out.push(Line { start, end: i, next });
                i = next - 1;
                start = next;
            }
            _ => {}
        }
        i += 1;
    }
    if start < b.len() {
        out.push(Line { start, end: b.len(), next: b.len() });
    }
    out
}

/// `line` with inline code and double-quoted text replaced by `x`, byte for byte: what the
/// instructions show rather than say.
fn mask(line: &str) -> String {
    let mut out = line.as_bytes().to_vec();
    let mut blank = |from: usize, to: usize| out[from..to].iter_mut().for_each(|c| *c = b'x');
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        let close = match c {
            '`' => {
                let run = chars[i..].iter().take_while(|(_, c)| *c == '`').count();
                let tick = "`".repeat(run);
                let from = at + run;
                line[from..].find(&tick).map(|j| from + j + run)
            }
            '"' => line[at + 1..].find('"').map(|j| at + 1 + j + 1),
            '\u{201C}' => line[at + 3..].find('\u{201D}').map(|j| at + 3 + j + 3),
            _ => None,
        };
        match close {
            Some(to) => {
                blank(at, to);
                while i < chars.len() && chars[i].0 < to {
                    i += 1;
                }
            }
            None => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| line.to_string())
}

/// A paragraph, list item, heading or table row: its lines joined by `\n`, masked.
struct Block {
    text: String,
    /// (offset in `text`, the line's index) for each line.
    lines: Vec<(usize, usize)>,
}

fn starts_item(t: &str) -> bool {
    let t = t.trim_start();
    if t.starts_with(['#', '|', '>']) || t == "---" || t == "***" {
        return true;
    }
    let b = t.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        return true;
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && b.len() > digits + 1 && matches!(b[digits], b'.' | b')') && b[digits + 1] == b' '
}

fn blocks(text: &str, lines: &[Line]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut cur: Option<Block> = None;
    let mut fence: Option<char> = None;
    for (i, l) in lines.iter().enumerate() {
        let raw = &text[l.start..l.end];
        let t = raw.trim_start();
        let fence_char = if t.starts_with("```") { Some('`') } else if t.starts_with("~~~") { Some('~') } else { None };
        if fence.is_some() || fence_char.is_some() {
            out.extend(cur.take());
            match (fence, fence_char) {
                (None, Some(c)) => fence = Some(c),
                (Some(f), Some(c)) if f == c => fence = None,
                _ => {}
            }
            continue;
        }
        if t.trim().is_empty() {
            out.extend(cur.take());
            continue;
        }
        let m = mask(raw);
        let heading_or_row = t.starts_with(['#', '|']);
        if starts_item(t) || cur.is_none() {
            out.extend(cur.take());
            cur = Some(Block { text: m, lines: vec![(0, i)] });
        } else if let Some(b) = cur.as_mut() {
            b.text.push('\n');
            b.lines.push((b.text.len(), i));
            b.text.push_str(&m);
        }
        if heading_or_row {
            out.extend(cur.take());
        }
    }
    out.extend(cur);
    out
}

/// Where the sentence around `at` starts and ends in `s`: sentences end at `.`, `!`, `?` or
/// `:` before whitespace (closing `*`, `_`, `)` and quotes go with the sentence).
fn sentence(s: &str, at: usize) -> (usize, usize) {
    let b = s.as_bytes();
    let ends_at = |i: usize| -> Option<usize> {
        if !matches!(b[i], b'.' | b'!' | b'?' | b':') {
            return None;
        }
        let mut j = i + 1;
        while j < b.len() && matches!(b[j], b'*' | b'_' | b')' | b'"' | b'\'') {
            j += 1;
        }
        (j == b.len() || b[j].is_ascii_whitespace()).then_some(j)
    };
    let mut start = 0;
    for i in 0..at.min(b.len()) {
        if let Some(j) = ends_at(i) {
            if j <= at {
                start = j;
            }
        }
    }
    let end = (at..b.len()).find_map(ends_at).unwrap_or(b.len());
    (start, end)
}

/// Lint `text`, named `file`, against the rules for `model`.
pub fn lint_text(text: &str, file: &str, model: Option<&str>) -> Vec<LintFinding> {
    let lines = split_lines(text);
    let rules: Vec<&Compiled> = compiled().iter().filter(|c| applies(&c.rule, model)).collect();
    let mut out = Vec::new();
    for block in blocks(text, &lines) {
        let s = &block.text;
        // (start, end) in the block, rule
        let mut hits: Vec<(usize, usize, &Compiled)> = Vec::new();
        for pass_fallback in [false, true] {
            for c in rules.iter().filter(|c| c.rule.fallback == pass_fallback) {
                for re in &c.patterns {
                    for caps in re.captures_iter(s) {
                        let m = caps.name("hit").or_else(|| caps.get(0)).expect("a match");
                        let (ss, se) = sentence(s, m.start());
                        if c.unless.iter().any(|u| u.is_match(&s[ss..se])) {
                            continue;
                        }
                        let (mut from, mut to) = (m.start(), m.end());
                        if c.rule.sentence {
                            // only a rule that ends its item: one followed by more explains itself
                            if s[se..].chars().any(|ch| ch.is_alphanumeric()) {
                                continue;
                            }
                            from = ss + s[ss..].find(|ch: char| ch.is_alphanumeric()).unwrap_or(0);
                            to = se;
                        }
                        if pass_fallback && hits.iter().any(|&(a, b, _)| a < se && ss < b) {
                            continue;
                        }
                        // two of a rule's patterns matching the same text is one finding
                        if hits.iter().any(|&(a, b, h)| a < to && from < b && std::ptr::eq(h, *c)) {
                            continue;
                        }
                        hits.push((from, to, c));
                    }
                }
            }
        }
        hits.sort_by_key(|h| h.0);
        for (from, to, c) in hits {
            let at = |off: usize| -> (usize, usize) {
                // the block line `off` is on, and the byte offset in the file
                let (lo, li) = block.lines.iter().rev().find(|(o, _)| *o <= off).copied().unwrap_or((0, block.lines[0].1));
                (li, lines[li].start + (off - lo))
            };
            let (l0, b0) = at(from);
            let (l1, b1) = at(to.max(from + 1) - 1);
            let b1 = b1 + text[b1..].chars().next().map(char::len_utf8).unwrap_or(0);
            let col = |li: usize, b: usize| text[lines[li].start..b].chars().count() + 1;
            out.push(LintFinding {
                rule: c.rule.id.clone(),
                category: c.rule.category.clone(),
                severity: c.rule.severity.clone(),
                file: file.to_string(),
                path: String::new(),
                kind: String::new(),
                entry: String::new(),
                line: l0 + 1,
                column: col(l0, b0),
                end_line: l1 + 1,
                end_column: col(l1, b1),
                start: b0,
                end: b1,
                line_start: lines[l0].start,
                line_end: lines[l1].next,
                matched: text[b0..b1].split_whitespace().collect::<Vec<_>>().join(" "),
                message: c.rule.message.clone(),
                fix: c.rule.fix.clone(),
                source: c.rule.source.clone(),
            });
        }
    }
    out
}

/// Extensions of the files in a skill or other directory that are read as instructions.
const PROSE: [&str; 5] = ["md", "mdc", "markdown", "txt", "mdx"];

/// Lint a file, or the prose files under a directory, naming them as audit does.
pub fn lint_path(root: &Path, label: &str, model: Option<&str>) -> (Vec<LintFinding>, usize) {
    let is_file = root.is_file();
    let mut out = Vec::new();
    let mut n = 0;
    for f in crate::scan::text_files(root) {
        let name = if is_file {
            label.to_string()
        } else {
            let rel = f.strip_prefix(root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            if !rel.rsplit_once('.').is_some_and(|(_, e)| PROSE.contains(&e.to_ascii_lowercase().as_str())) {
                continue;
            }
            if label.is_empty() { rel } else { format!("{}/{rel}", label.trim_end_matches('/')) }
        };
        let Ok(text) = crate::fsutil::read_regular_text(&f, crate::scan::MAX_SCAN) else { continue };
        n += 1;
        for mut x in lint_text(&text, &name, model) {
            x.path = f.display().to_string();
            out.push(x);
        }
    }
    (out, n)
}

/// What `hlock lint` found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintReport {
    /// The lock linted (`None` with only `--file`).
    pub lock: Option<String>,
    /// The target model, and where it came from: `flag`, `lock`, or `none` (only the rules
    /// for every model apply).
    pub model: Option<String>,
    pub model_from: String,
    /// The ids of the rules that applied.
    pub rules: Vec<String>,
    pub files_scanned: usize,
    pub findings: Vec<LintFinding>,
    pub warnings: usize,
    pub infos: usize,
    /// No warnings (what `--ci` fails on).
    pub passed: bool,
}

/// Lint what a lock places (skills, instruction files, subagents, slash commands) and any
/// `files`, for `model` (else the lock's default model).
pub fn run(lock_path: Option<&Path>, files: &[PathBuf], model: Option<&str>, reg: &crate::profiles::Registry, ctx: &Ctx) -> crate::error::Result<LintReport> {
    let lock = lock_path.map(|p| Lockfile::load(p).map(|l| (p, l))).transpose()?;
    let (model, model_from) = match (model.filter(|m| !m.trim().is_empty()), &lock) {
        (Some(m), _) => (Some(m.to_string()), "flag"),
        (None, Some((_, l))) => match l.llm.iter().find(|x| x.role == "default").or_else(|| l.llm.first()).map(|x| x.model.clone()).filter(|m| !m.is_empty()) {
            Some(m) => (Some(m), "lock"),
            None => (None, "none"),
        },
        (None, None) => (None, "none"),
    };
    let mut findings = Vec::new();
    let mut files_scanned = 0;
    let mut add = |(fs, n): (Vec<LintFinding>, usize), kind: &str, entry: &str| {
        files_scanned += n;
        findings.extend(fs.into_iter().map(|mut f| {
            f.kind = kind.into();
            f.entry = entry.into();
            f
        }));
    };
    if let Some((p, l)) = &lock {
        let dir = crate::sync::lock_dir_of(p, ctx);
        for c in crate::audit::contents(l, &dir, reg.harness(&l.harness.name), ctx) {
            if matches!(c.kind.as_str(), "skill" | "instructions" | "subagent") {
                add(lint_path(&c.path, &c.label, model.as_deref()), &c.kind, &c.name);
            }
        }
        for cmd in &l.command {
            if crate::fsutil::safe_rel(&cmd.path) {
                let at = dir.join(&cmd.path);
                if at.exists() {
                    add(lint_path(&at, &cmd.path, model.as_deref()), "command", &cmd.name);
                }
            }
        }
    }
    for f in files {
        if !f.exists() {
            return Err(crate::error::Error::Audit(format!("{}: no such file or directory", f.display())));
        }
        let label = f.to_string_lossy().replace('\\', "/");
        let label = label.trim_start_matches("./");
        add(lint_path(f, label, model.as_deref()), "file", label);
    }
    let rules = compiled().iter().filter(|c| applies(&c.rule, model.as_deref())).map(|c| c.rule.id.clone()).collect();
    let warnings = findings.iter().filter(|f| f.severity == "warning").count();
    let infos = findings.len() - warnings;
    Ok(LintReport { lock: lock.map(|(p, _)| p.display().to_string()), model, model_from: model_from.into(), rules, files_scanned, findings, warnings, infos, passed: warnings == 0 })
}

/// The report as SARIF 2.1.0, as `hlock audit --format sarif` writes its own.
pub fn sarif(r: &LintReport) -> serde_json::Value {
    use serde_json::json;
    let level = |s: &str| if s == "warning" { "warning" } else { "note" };
    let mut rules = std::collections::BTreeMap::new();
    for c in compiled() {
        if r.rules.contains(&c.rule.id) {
            let id = format!("hlock/lint/{}", c.rule.id);
            rules.insert(id.clone(), json!({"id": id, "shortDescription": {"text": c.rule.message}, "help": {"text": c.rule.fix}, "helpUri": c.rule.source, "defaultConfiguration": {"level": level(&c.rule.severity)}}));
        }
    }
    let results: Vec<_> = r
        .findings
        .iter()
        .map(|f| {
            json!({
                "ruleId": format!("hlock/lint/{}", f.rule),
                "level": level(&f.severity),
                "message": {"text": format!("{} {}", f.message, f.fix)},
                "locations": [{"physicalLocation": {"artifactLocation": {"uri": f.file}, "region": {"startLine": f.line, "startColumn": f.column, "endLine": f.end_line, "endColumn": f.end_column}}}],
            })
        })
        .collect();
    json!({
        "$schema": "https://docs.oasis-open.org/sarif/sarif/v2.1.0/cos02/schemas/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {"name": "hlock-lint", "version": env!("CARGO_PKG_VERSION"), "informationUri": "https://docs.hlock.ai/guides/lint/", "rules": rules.into_values().collect::<Vec<_>>()}},
            "results": results,
            "columnKind": "unicodeCodePoints",
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(text: &str, model: &str) -> Vec<String> {
        lint_text(text, "f.md", Some(model)).into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn every_rule_flags_its_examples_and_not_its_counterexamples() {
        for r in rules() {
            assert!(!r.examples.is_empty(), "{} has no examples", r.id);
            assert!(matches!(r.severity.as_str(), "warning" | "info"), "{}: severity {}", r.id, r.severity);
            assert!(r.source.starts_with("https://"), "{}", r.id);
            for e in &r.examples {
                assert!(ids(e, "all").contains(&r.id), "{} doesn't flag {e:?}", r.id);
            }
            for e in &r.counterexamples {
                assert!(!ids(e, "all").contains(&r.id), "{} flags {e:?}", r.id);
            }
        }
    }

    #[test]
    fn rules_apply_to_their_models() {
        let r = rules();
        let think = r.iter().find(|r| r.id == "think-less").unwrap();
        let abs = r.iter().find(|r| r.id == "unscoped-absolute").unwrap();
        for m in ["claude-fable-5-2", "claude-fable-5.2", "anthropic/claude-fable-5-2", "claude-fable-5-2-20261001", "claude-mythos-5-2", "all"] {
            assert!(applies(think, Some(m)), "{m}");
        }
        for m in ["claude-opus-5-5", "opus", "claude-fable-5-1", "claude-fable-5-20", ""] {
            assert!(!applies(think, Some(m)), "{m}");
        }
        assert!(!applies(think, None));
        assert!(applies(abs, None) && applies(abs, Some("gpt-5")));
        assert_eq!(ids("Answer directly without deliberating.", "claude-opus-5-5"), Vec::<String>::new());
        assert_eq!(ids("Answer directly without deliberating.", "claude-fable-5-2"), ["think-less"]);
    }

    #[test]
    fn spans_point_at_the_text() {
        let text = "# Rules\n\nRun the linter first.\n\n- Treat that answer as done\n  and move on.\n- Never push to main.\r\n";
        let f = lint_text(text, "AGENTS.md", Some("claude-fable-5-2"));
        assert_eq!(f.len(), 2, "{f:#?}");
        let done = &f[0];
        assert_eq!((done.rule.as_str(), done.line, done.column, done.end_line), ("treat-answer-as-done", 5, 3, 5));
        assert_eq!(&text[done.start..done.end], "Treat that answer as done");
        assert_eq!(&text[done.line_start..done.line_end], "- Treat that answer as done\n");
        let never = &f[1];
        assert_eq!((never.rule.as_str(), never.line, never.column), ("unscoped-absolute", 7, 3));
        assert_eq!(&text[never.start..never.end], "Never push to main.");
        assert_eq!(&text[never.line_start..never.line_end], "- Never push to main.\r\n");
        assert_eq!(never.end_column, 22);
        assert_eq!(never.label(), "AGENTS.md:7:3: Never push to main. (unscoped-absolute)");
    }

    #[test]
    fn wrapped_lines_are_one_block() {
        let text = "Before you answer, write out\nyour full reasoning.\n";
        let f = lint_text(text, "f", Some("claude-fable-5-2"));
        assert_eq!((f[0].line, f[0].end_line, f[0].matched.as_str()), (1, 2, "write out your full reasoning"));
        assert_eq!(&text[f[0].line_start..f[0].line_end], text);
        // a reason on the next line of the same sentence counts
        assert!(lint_text("- Never echo a value\n  unless the user asks.\n", "f", None).is_empty());
        // a sentence that wraps onto a line starting with "never" isn't a rule
        assert!(lint_text("The worker\nnever retries on its own.\n", "f", None).is_empty());
    }

    #[test]
    fn code_and_quotes_are_not_instructions() {
        let text = "```sh\n# never retry\nhlock sync --plan  # always look first\n```\nWrite \"ask before deleting files\" rather than \"never delete files\".\nSet `never` in the config.\n";
        assert_eq!(lint_text(text, "f", Some("all")), Vec::new());
        assert_eq!(mask("a `b` \"c\" \u{201C}d\u{201D} e").len(), "a `b` \"c\" \u{201C}d\u{201D} e".len());
    }

    #[test]
    fn a_line_another_rule_names_isnt_also_an_absolute() {
        assert_eq!(ids("Never retry a failed command.", "claude-fable-5-2"), ["stale-caution"]);
        assert_eq!(ids("Never retry a failed command.", "claude-opus-5-5"), ["unscoped-absolute"]);
    }
}
