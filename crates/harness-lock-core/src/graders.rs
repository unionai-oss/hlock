//! Graders: how a run of an eval-pack task is scored (SPEC §7.1).
//!
//! A task lists its graders in `pack.toml`, each a short string or a table:
//!
//! - `artifact`: output files exist and, optionally, match expected content.
//! - `tests`: a command or script runs in the workspace after the agent; exit 0 passes,
//!   and it may report partial credit as JSON in `$HLOCK_GRADE`.
//! - `judge`: a model scores the output files against a rubric, a list of criteria, or
//!   ordered levels. A decision model (`crate::decide`) answers one yes/no question per
//!   criterion; an LLM is asked for JSON.
//! - `trace`: a decision model answers criteria about the run's tool calls.
//!
//! Every file in `expected.artifacts` must also exist, so a task with no graders is
//! graded on its artifacts alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{provider_base_url, provider_key_env, AgentConfig};
use crate::decide::{DeciderConfig, Question};
use crate::fsutil::{read_regular, read_regular_text, safe_rel};
use crate::process::Limits;
use crate::tasks::{Task, PLACEHOLDER};

pub const DEFAULT_TESTS_TIMEOUT: u64 = 300;
/// The judge score a run needs to pass, unless the grader sets `pass`.
pub const DEFAULT_JUDGE_PASS: f64 = 0.7;
/// A decider's grade is uncertain within this distance of `pass`, unless the judge sets
/// `uncertain`.
pub const DEFAULT_UNCERTAIN: f64 = 0.15;
/// The decision model a judge uses when none is set and `TYPESAFE_API_KEY` is.
pub const DEFAULT_HOSTED_JUDGE: &str = "typesafe:jev-1.13.0";
/// The most of one file a judge is shown, in characters.
const MAX_JUDGE_FILE: usize = 20_000;
/// Lines of a failing test command's output kept in the grade's detail.
const DETAIL_LINES: usize = 12;
/// The most of a test command's output kept, per stream (only its tail is shown).
const MAX_TEST_OUTPUT: usize = 64 * 1024;
/// The most of an output file an artifact check reads.
const MAX_ARTIFACT: usize = 8 << 20;

const JUDGE_SYSTEM: &str = "You grade the work of an AI agent. Judge only what the files show, \
    be strict and literal, and don't give credit for intentions. Reply with JSON only.";

/// The model that scores `judge` graders: `[evals.judge]` in `harness.toml`, else
/// `[pack.judge]` in `pack.toml`, else `[deciders.judge]` or `[deciders.default]`. A
/// decision model (`typesafe:jev-1.13.0`, `ollama:kev:4b`) or an LLM.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct JudgeConfig {
    /// `provider:model`, e.g. `typesafe:jev-1.13.0` or `anthropic:claude-sonnet-5-5`.
    pub model: String,
    /// The endpoint: an OpenAI-compatible one for an LLM, a System One or Decisions one
    /// for a decider.
    pub base_url: Option<String>,
    /// The variable the API key is read from (default: the provider's usual one).
    pub key_env: Option<String>,
    /// `systemone` or `decisions` for a decider, `chat` to force an LLM; inferred
    /// from the model reference otherwise.
    pub protocol: Option<String>,
    /// TypeSafe's long-context mode.
    pub long_context: bool,
    /// An LLM that re-judges a decider's uncertain grades; its verdict counts.
    pub fallback: Option<String>,
    /// The fallback's OpenAI-compatible endpoint, for providers hlock doesn't know.
    pub fallback_base_url: Option<String>,
    /// How close to `pass` a decider's score is uncertain (default 0.15).
    pub uncertain: Option<f64>,
}

impl JudgeConfig {
    pub fn is_set(&self) -> bool {
        !self.model.trim().is_empty()
    }

    pub fn new(model: &str) -> JudgeConfig {
        JudgeConfig { model: model.into(), ..Default::default() }
    }

    /// Whether it's a decision model rather than an LLM.
    pub fn is_decider(&self) -> bool {
        self.protocol.as_deref() != Some("chat") && crate::decide::is_decider(&self.model, self.protocol.as_deref())
    }

    fn decider(&self) -> DeciderConfig {
        DeciderConfig {
            model: self.model.clone(),
            protocol: self.protocol.clone(),
            base_url: self.base_url.clone(),
            key_env: self.key_env.clone(),
            long_context: self.long_context,
            timeout_seconds: None,
        }
    }

    /// The LLM endpoint, with the key read through `var`.
    fn agent_config(&self, var: &dyn Fn(&str) -> Option<String>) -> Result<AgentConfig, String> {
        let (provider, model) = self
            .model
            .split_once(':')
            .filter(|(p, m)| !p.is_empty() && !m.is_empty())
            .ok_or_else(|| format!("judge model {:?} isn't written provider:model", self.model))?;
        let base_url = self
            .base_url
            .clone()
            .or_else(|| provider_base_url(provider).map(str::to_string))
            .ok_or_else(|| format!("hlock doesn't know where {provider:?} is; set the judge's base-url"))?;
        let key_env = self.key_env.clone().or_else(|| provider_key_env(provider).map(str::to_string));
        let api_key = key_env.as_deref().and_then(var).filter(|v| !v.is_empty());
        if api_key.is_none() && self.base_url.is_none() {
            return Err(format!("{} isn't set; the judge reads its {provider} API key from it", key_env.unwrap_or_default()));
        }
        Ok(AgentConfig { provider: provider.into(), model: model.into(), base_url, api_key, ..Default::default() })
    }
}

/// One grader, parsed from a task's `graders` entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Grader {
    /// Files exist and, when set, match: `equals` (text, ignoring surrounding
    /// whitespace), `same_as` (a file in the pack), and `contains` (substrings).
    Artifact { paths: Vec<String>, equals: Option<String>, same_as: Option<String>, contains: Vec<String> },
    /// A shell `command`, or a `script` in the pack, run in the workspace after
    /// `files` from the pack are copied in.
    Tests { command: Option<String>, script: Option<String>, files: Vec<String>, timeout_seconds: u64 },
    /// A model scores `artifacts` against a `rubric` file, a list of `criteria`, or
    /// ordered `levels` (worst first).
    Judge { rubric: Option<String>, criteria: Vec<Criterion>, levels: Vec<String>, artifacts: Vec<String>, pass: f64 },
    /// A decision model answers `criteria` about the run's tool calls.
    Trace { criteria: Vec<Criterion>, pass: f64 },
}

/// One criterion a judge checks, optionally with what true and false look like.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Criterion {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_true: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_false: Option<String>,
}

impl Criterion {
    pub fn new(text: &str) -> Criterion {
        Criterion { text: text.into(), if_true: None, if_false: None }
    }

    fn question(&self, id: String) -> Question {
        let mut q = Question::noul(&id, &self.text);
        if let (Some(t), Some(f)) = (&self.if_true, &self.if_false) {
            q.criteria = json!({"true": t, "false": f});
        }
        q
    }

    fn line(&self) -> String {
        match (&self.if_true, &self.if_false) {
            (Some(t), Some(f)) => format!("{} (true: {t}; false: {f})", self.text),
            _ => self.text.clone(),
        }
    }
}

/// One part of a judge's grade: a criterion and P(met), or the level reached.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Part {
    pub label: String,
    pub value: f64,
}

/// The result of one grader on one run.
#[derive(Debug, Clone, Serialize)]
pub struct Grade {
    pub grader: String,
    pub success: bool,
    pub score: f64,
    /// Why it failed, or what the judge said.
    pub detail: String,
    /// Per-criterion probabilities (judges and trace graders).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
    /// A decider's grade near the line, or with low confidence.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
    /// The model that answered, as the provider named it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judged_by: Option<String>,
    /// What judging cost, when the provider publishes a price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl Grade {
    fn new(grader: String, success: bool, score: f64, detail: impl Into<String>) -> Grade {
        Grade { grader, success, score: score.clamp(0.0, 1.0), detail: detail.into(), parts: vec![], uncertain: false, judged_by: None, cost_usd: None }
    }
}

/// Which judges a grading runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Judges {
    /// Decision models and LLMs (`resolve`, `hlock evals grade`).
    #[default]
    All,
    /// Decision models only: cheap and repeatable (`hlock evals check --run`).
    Deciders,
    /// None (offline).
    None,
}

/// How to grade.
#[derive(Debug, Clone, Default)]
pub struct GradeOptions {
    /// `[evals.judge]`: wins over the pack's `[pack.judge]`.
    pub judge: Option<JudgeConfig>,
    /// `[deciders.judge]` or `[deciders.default]`: used when neither judge is set.
    pub default_judge: Option<JudgeConfig>,
    /// When no judge is set at all, look for one ([`auto_judge`]).
    pub auto: bool,
    pub judges: Judges,
    /// The run's trace events, for `trace` graders and `$HLOCK_TRACE`.
    pub trace: Option<Value>,
    /// The environment judges read their API keys and endpoints from; `None` reads the
    /// process's, as [`Ctx::from_process`](crate::profiles::Ctx::from_process) sees it.
    pub env: Option<BTreeMap<String, String>>,
    /// The sandbox `tests` graders run in, when the lock's covers `graders`.
    pub sandbox: Option<crate::sandbox::Sandbox>,
}

impl GradeOptions {
    /// Look up a non-empty variable in [`env`](Self::env), or the process's.
    fn var(&self) -> impl Fn(&str) -> Option<String> + '_ {
        let process = self.env.is_none().then(|| crate::profiles::Ctx::from_process().env);
        move |k: &str| self.env.as_ref().or(process.as_ref()).and_then(|e| e.get(k)).filter(|v| !v.is_empty()).cloned()
    }
}

/// The judge to use when none is configured: a Kev model a local Ollama or Ollaya is
/// serving, else Jev when `TYPESAFE_API_KEY` is set. `HLOCK_JUDGE_AUTO=0` turns it
/// off. Looked up once per process.
pub fn auto_judge() -> Option<JudgeConfig> {
    static FOUND: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let ctx = crate::profiles::Ctx::from_process();
    let var = |k: &str| ctx.var(k).map(str::to_string);
    if var("HLOCK_JUDGE_AUTO").as_deref() == Some("0") {
        return None;
    }
    FOUND
        .get_or_init(|| crate::decide::local_decider(&var).or_else(|| var("TYPESAFE_API_KEY").map(|_| DEFAULT_HOSTED_JUDGE.to_string())))
        .clone()
        .map(|m| JudgeConfig::new(&m))
}

impl Grader {
    /// A short name for reports and traces, e.g. `tests:greet/check.py`.
    pub fn label(&self) -> String {
        match self {
            Grader::Artifact { paths, .. } => format!("artifact:{}", paths.join(",")),
            Grader::Tests { script: Some(s), .. } => format!("tests:{s}"),
            Grader::Tests { command, .. } => format!("tests:{}", command.as_deref().unwrap_or_default()),
            Grader::Judge { rubric: Some(r), .. } => format!("judge:{r}"),
            Grader::Judge { criteria, levels, .. } if criteria.is_empty() => format!("judge:{} levels", levels.len()),
            Grader::Judge { criteria, .. } => format!("judge:{} criteria", criteria.len()),
            Grader::Trace { criteria, .. } => format!("trace:{} criteria", criteria.len()),
        }
    }

    /// Whether it does more than check that files exist.
    pub fn checks_content(&self) -> bool {
        match self {
            Grader::Artifact { equals, same_as, contains, .. } => equals.is_some() || same_as.is_some() || !contains.is_empty(),
            _ => true,
        }
    }

    /// Files in the pack this grader reads: test scripts and files, rubrics, golden files.
    pub fn pack_files(&self) -> Vec<String> {
        match self {
            Grader::Artifact { same_as, .. } => same_as.iter().cloned().collect(),
            Grader::Tests { script, files, .. } => script.iter().chain(files).cloned().collect(),
            Grader::Judge { rubric, .. } => rubric.iter().cloned().collect(),
            Grader::Trace { .. } => vec![],
        }
    }
}

// --------------------------------------------------------------------------- //
// Parsing
// --------------------------------------------------------------------------- //

fn strings(v: &Value, key: &str) -> Result<Vec<String>, String> {
    match v {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| format!("`{key}` must be a list of strings"))).collect(),
        _ => Err(format!("`{key}` must be a string or a list of strings")),
    }
}

/// Parse one `graders` entry: a string like `"tests:check.py"`, or a table with a `type`.
/// Paths must be relative and stay inside the pack (or the workspace).
pub fn parse(spec: &Value, task: &Task) -> Result<Grader, String> {
    let g = parse_entry(spec, task)?;
    let inside = |what: &str, paths: &[&String]| -> Result<(), String> {
        match paths.iter().find(|p| !safe_rel(p)) {
            Some(p) => Err(format!("{what} {p:?} must be a relative path inside its directory (no leading '/', no '..')")),
            None => Ok(()),
        }
    };
    match &g {
        Grader::Artifact { paths, same_as, .. } => {
            inside("artifact", &paths.iter().collect::<Vec<_>>())?;
            inside("same-as", &same_as.iter().collect::<Vec<_>>())?;
        }
        Grader::Tests { script, files, .. } => {
            inside("script", &script.iter().collect::<Vec<_>>())?;
            inside("file", &files.iter().collect::<Vec<_>>())?;
        }
        Grader::Judge { rubric, artifacts, .. } => {
            inside("rubric", &rubric.iter().collect::<Vec<_>>())?;
            inside("artifact", &artifacts.iter().collect::<Vec<_>>())?;
        }
        Grader::Trace { .. } => {}
    }
    Ok(g)
}

fn parse_entry(spec: &Value, task: &Task) -> Result<Grader, String> {
    let expected = task.expected_artifacts();
    let table = match spec {
        Value::String(s) => {
            let (kind, arg) = s.split_once(':').unwrap_or((s.as_str(), ""));
            let arg = arg.trim();
            return match kind.trim() {
                "artifact" => {
                    let paths: Vec<String> = arg.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect();
                    let paths = if paths.is_empty() { expected } else { paths };
                    if paths.is_empty() {
                        return Err("artifact grader has no files; name them (\"artifact:out.csv\") or set expected.artifacts".into());
                    }
                    Ok(Grader::Artifact { paths, equals: None, same_as: None, contains: vec![] })
                }
                "tests" if !arg.is_empty() => Ok(Grader::Tests { command: None, script: Some(arg.into()), files: vec![], timeout_seconds: DEFAULT_TESTS_TIMEOUT }),
                "judge" if !arg.is_empty() => judge(Some(arg.into()), vec![], vec![], expected, DEFAULT_JUDGE_PASS),
                "trace" if !arg.is_empty() => Ok(Grader::Trace { criteria: vec![Criterion::new(arg)], pass: DEFAULT_JUDGE_PASS }),
                "tests" | "judge" => Err(format!("{s:?} needs a file: \"{kind}:<path in the pack>\"")),
                "trace" => Err(format!("{s:?} needs a criterion: \"trace:Never ran sudo\"")),
                other => Err(format!("unknown grader type {other:?} (use artifact, tests, judge, or trace)")),
            };
        }
        Value::Object(m) => m,
        _ => return Err("a grader must be a string like \"tests:check.py\" or a table with a `type`".into()),
    };
    let kind = table.get("type").and_then(Value::as_str).ok_or("a grader table needs a `type` (artifact, tests, judge, or trace)")?;
    let allowed: &[&str] = match kind {
        "artifact" => &["path", "paths", "equals", "same-as", "contains"],
        "tests" => &["command", "script", "files", "timeout-seconds"],
        "judge" => &["rubric", "criteria", "levels", "artifacts", "pass"],
        "trace" => &["criteria", "pass"],
        other => return Err(format!("unknown grader type {other:?} (use artifact, tests, judge, or trace)")),
    };
    if let Some(k) = table.keys().find(|k| *k != "type" && !allowed.contains(&k.as_str())) {
        return Err(format!("unknown key `{k}` in a {kind} grader (expected {})", allowed.join(", ")));
    }
    let text = |k: &str| -> Result<Option<String>, String> {
        match table.get(k) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("`{k}` must be a string")),
        }
    };
    let list = |k: &str| table.get(k).map(|v| strings(v, k)).transpose().map(Option::unwrap_or_default);
    match kind {
        "artifact" => {
            let mut paths = list("paths")?;
            paths.extend(text("path")?);
            if paths.is_empty() {
                paths = expected;
            }
            if paths.is_empty() {
                return Err("artifact grader has no files; set `path` or expected.artifacts".into());
            }
            let (equals, same_as) = (text("equals")?, text("same-as")?);
            if equals.is_some() && same_as.is_some() {
                return Err("set `equals` or `same-as`, not both".into());
            }
            Ok(Grader::Artifact { paths, equals, same_as, contains: list("contains")? })
        }
        "tests" => {
            let (command, script) = (text("command")?, text("script")?);
            if command.is_some() == script.is_some() {
                return Err("a tests grader needs a `command` or a `script` (one of them)".into());
            }
            let timeout_seconds = match table.get("timeout-seconds") {
                None => DEFAULT_TESTS_TIMEOUT,
                Some(v) => v.as_u64().filter(|n| *n > 0).ok_or("`timeout-seconds` must be a positive integer")?,
            };
            Ok(Grader::Tests { command, script, files: list("files")?, timeout_seconds })
        }
        _ => {
            let pass = match table.get("pass") {
                None => DEFAULT_JUDGE_PASS,
                Some(v) => v.as_f64().filter(|p| (0.0..=1.0).contains(p)).ok_or("`pass` must be a number from 0 to 1")?,
            };
            let criteria = table.get("criteria").map(criteria).transpose()?.unwrap_or_default();
            if kind == "trace" {
                if criteria.is_empty() {
                    return Err("a trace grader needs `criteria` about the run's tool calls".into());
                }
                return Ok(Grader::Trace { criteria, pass });
            }
            let levels = list("levels")?;
            if !levels.is_empty() && !(2..=10).contains(&levels.len()) {
                return Err(format!("`levels` needs 2 to 10 levels, worst first (it has {})", levels.len()));
            }
            let artifacts = list("artifacts")?;
            judge(text("rubric")?, criteria, levels, if artifacts.is_empty() { expected } else { artifacts }, pass)
        }
    }
}

/// `criteria`: strings, or tables with `text` and optional `true` and `false`.
fn criteria(v: &Value) -> Result<Vec<Criterion>, String> {
    let items = v.as_array().ok_or("`criteria` must be a list")?;
    items
        .iter()
        .map(|c| match c {
            Value::String(s) => Ok(Criterion::new(s)),
            Value::Object(m) => {
                if let Some(k) = m.keys().find(|k| !["text", "true", "false"].contains(&k.as_str())) {
                    return Err(format!("unknown key `{k}` in a criterion (expected text, true, false)"));
                }
                let get = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
                let text = get("text").ok_or("a criterion table needs `text`")?;
                let (t, f) = (get("true"), get("false"));
                if t.is_some() != f.is_some() {
                    return Err(format!("criterion {text:?}: set both `true` and `false`, or neither"));
                }
                Ok(Criterion { text, if_true: t, if_false: f })
            }
            _ => Err("`criteria` must be a list of strings or tables".into()),
        })
        .collect()
}

fn judge(rubric: Option<String>, criteria: Vec<Criterion>, levels: Vec<String>, artifacts: Vec<String>, pass: f64) -> Result<Grader, String> {
    if rubric.is_some() == (!criteria.is_empty() || !levels.is_empty()) {
        return Err("a judge grader needs a `rubric` file or inline `criteria` and `levels`, not both".into());
    }
    if artifacts.is_empty() {
        return Err("a judge grader has no files to read; set `artifacts` or expected.artifacts".into());
    }
    Ok(Grader::Judge { rubric, criteria, levels, artifacts, pass })
}

/// Read a rubric file's structure: top-level bullet items are criteria, and a numbered
/// list under a `## Levels` heading (worst first) is the levels. A free-form rubric has
/// neither, and only an LLM judge can read it.
pub fn rubric_parts(text: &str) -> (Vec<Criterion>, Vec<String>) {
    let (mut criteria, mut levels) = (Vec::new(), Vec::new());
    let mut in_levels = false;
    for line in text.lines() {
        let t = line.trim_end();
        if let Some(h) = t.strip_prefix('#') {
            in_levels = h.trim_start_matches('#').trim().eq_ignore_ascii_case("levels");
            continue;
        }
        if t.starts_with(' ') || t.starts_with('\t') {
            continue; // nested items belong to the item above
        }
        if in_levels {
            if let Some((n, rest)) = t.split_once(". ") {
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                    levels.push(rest.trim().to_string());
                }
            }
        } else if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            criteria.push(Criterion::new(item.trim()));
        }
    }
    (criteria, levels)
}

/// Pack files a task's graders name that are missing, and ones that still hold a
/// [`PLACEHOLDER`]: `(path, problem)`.
pub fn pack_file_problems(grader: &Grader, pack_dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for f in grader.pack_files() {
        let p = pack_dir.join(&f);
        if !p.exists() {
            out.push((f, "not found".into()));
        } else if p.is_file() {
            let n = std::fs::read_to_string(&p).map(|t| t.matches(PLACEHOLDER).count()).unwrap_or(0);
            if n > 0 {
                out.push((f, format!("still has {n} {PLACEHOLDER}")));
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- //
// Grading
// --------------------------------------------------------------------------- //

/// Grade a run of `task` whose output is in `workspace`. `tests` graders copy files
/// into the workspace, so grade a copy of anything you want to keep as it is.
pub fn grade(task: &Task, workspace: &Path, opts: &GradeOptions) -> Vec<Grade> {
    // absolute, since `tests` commands run in the workspace
    let pack_dir = std::path::absolute(task.pack_dir.clone().unwrap_or_default()).unwrap_or_default();
    let graders = task.grader_list();
    let covered: BTreeSet<&String> = graders
        .iter()
        .filter_map(|g| match g {
            Ok(Grader::Artifact { paths, .. }) => Some(paths),
            _ => None,
        })
        .flatten()
        .collect();
    let rest: Vec<String> = task.expected_artifacts().into_iter().filter(|a| !covered.contains(a)).collect();
    let mut out = Vec::new();
    if !rest.is_empty() || graders.is_empty() {
        out.push(artifact(&rest, None, None, &[], workspace, &pack_dir));
    }
    let judge = opts.judge.as_ref().filter(|j| j.is_set());
    let decider = judge.is_some_and(JudgeConfig::is_decider);
    let var = opts.var();
    for g in &graders {
        let grade = match g {
            Err(e) => Grade::new("invalid".into(), false, 0.0, e.clone()),
            Ok(Grader::Judge { .. } | Grader::Trace { .. }) if opts.judges == Judges::None => continue,
            Ok(Grader::Judge { .. } | Grader::Trace { .. }) if opts.judges == Judges::Deciders && !decider => continue,
            Ok(Grader::Artifact { paths, equals, same_as, contains }) => {
                artifact(paths, equals.as_deref(), same_as.as_deref(), contains, workspace, &pack_dir)
            }
            Ok(g @ Grader::Tests { .. }) => tests(g, task, workspace, &pack_dir, opts.trace.as_ref(), opts.sandbox.as_ref()),
            Ok(g @ Grader::Judge { .. }) => judge_run(g, task, workspace, &pack_dir, judge, &var),
            Ok(g @ Grader::Trace { .. }) => match &opts.trace {
                Some(trace) => trace_run(g, task, trace, judge, &var),
                None => continue, // nothing ran, so there's no trace to grade
            },
        };
        out.push(grade);
    }
    out
}

fn norm(s: &str) -> String {
    s.replace("\r\n", "\n").trim().to_string()
}

fn short(s: &str) -> String {
    let s = norm(s);
    if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s }
}

fn artifact(paths: &[String], equals: Option<&str>, same_as: Option<&str>, contains: &[String], ws: &Path, pack_dir: &Path) -> Grade {
    let label = format!("artifact:{}", paths.join(","));
    let golden = match same_as.map(|f| std::fs::read_to_string(pack_dir.join(f)).map_err(|e| format!("can't read {f}: {e}"))).transpose() {
        Ok(g) => g,
        Err(e) => return Grade::new(label, false, 0.0, e),
    };
    let mut problems = Vec::new();
    let mut ok = 0usize;
    for p in paths {
        if !safe_rel(p) {
            problems.push(format!("{p} isn't a path inside the workspace"));
            continue;
        }
        let f = ws.join(p);
        match std::fs::symlink_metadata(&f) {
            Err(_) => {
                problems.push(format!("{p} is missing"));
                continue;
            }
            // the run may have planted a link to a file it didn't write
            Ok(m) if m.file_type().is_symlink() => {
                problems.push(format!("{p} is a symlink, not a file the run wrote"));
                continue;
            }
            Ok(_) => {}
        }
        let before = problems.len();
        if equals.is_some() || golden.is_some() || !contains.is_empty() {
            let text = match read_regular_text(&f, MAX_ARTIFACT) {
                Ok(t) => t,
                Err(e) => {
                    problems.push(format!("can't read {p}: {e}"));
                    continue;
                }
            };
            if let Some(want) = equals.or(golden.as_deref()) {
                if norm(&text) != norm(want) {
                    let what = same_as.map(|s| format!("the same as {s}")).unwrap_or_else(|| format!("{:?}", short(want)));
                    problems.push(format!("{p} should be {what}, but is {:?}", short(&text)));
                }
            }
            for c in contains.iter().filter(|c| !text.contains(c.as_str())) {
                problems.push(format!("{p} doesn't contain {c:?}"));
            }
        }
        if problems.len() == before {
            ok += 1;
        }
    }
    let score = if paths.is_empty() { 1.0 } else { ok as f64 / paths.len() as f64 };
    Grade::new(label, problems.is_empty(), score, problems.join("; "))
}

/// Copy a pack file or directory into the workspace under its own name. Symlinks the
/// run left where it lands are replaced, not written through.
fn copy_into(src: &Path, ws: &Path) -> crate::error::Result<()> {
    let name = src.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    crate::fsutil::copy_tree(src, &ws.join(name)).map(|_| ())
}

/// Run a command with a time limit, in its own process group: a timeout stops what it
/// started too (`cd x && pytest`), and so does a normal exit (`server &`). Returns its
/// exit code (None when it timed out or couldn't start) and the tail of its output.
fn run_limited(mut cmd: Command, limit: Duration) -> (Option<i32>, String) {
    cmd.stdin(Stdio::null());
    let limits = Limits { max_bytes: MAX_TEST_OUTPUT, ..Limits::timeout(limit) };
    match crate::process::run(cmd, &limits) {
        Err(e) => (None, format!("can't run it: {e}")),
        Ok(out) if out.timed_out => (None, format!("{}\ntimed out after {}s", out.text(), limit.as_secs())),
        Ok(out) => (out.code.or(Some(-1)), out.text()),
    }
}

/// A new, empty directory only this process can use, for a grader's scratch files.
/// Never an existing path, so nothing planted there is followed.
pub(crate) fn private_dir(tag: &str) -> std::io::Result<PathBuf> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let mut last = None;
    for _ in 0..16 {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hlock-{tag}-{}-{n}-{nanos:x}", std::process::id()));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free temporary directory")))
}

/// A [`private_dir`] removed when dropped.
pub(crate) struct ScratchDir(pub PathBuf);

impl ScratchDir {
    pub(crate) fn new(tag: &str) -> std::io::Result<ScratchDir> {
        private_dir(tag).map(ScratchDir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(DETAIL_LINES)..].join("\n")
}

fn tests(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path, trace: Option<&Value>, sandbox: Option<&crate::sandbox::Sandbox>) -> Grade {
    let label = g.label();
    let Grader::Tests { command, script, files, timeout_seconds } = g else {
        return Grade::new(label, false, 0.0, "not a tests grader");
    };
    for f in files {
        if let Err(e) = copy_into(&pack_dir.join(f), ws) {
            return Grade::new(label, false, 0.0, format!("can't copy {f} into the workspace: {e}"));
        }
    }
    let argv: Vec<String> = match (command, script) {
        (Some(line), _) if cfg!(windows) => vec!["cmd".into(), "/C".into(), line.clone()],
        (Some(line), _) => vec!["sh".into(), "-c".into(), line.clone()],
        (None, Some(s)) => {
            let path = pack_dir.join(s).to_string_lossy().into_owned();
            if s.ends_with(".py") {
                // inside a sandbox the image's python runs it
                let py = match sandbox {
                    Some(_) => PathBuf::from("python3"),
                    None => crate::profiles::python_for(None, &crate::profiles::Ctx::from_process())
                        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" })),
                };
                vec![py.to_string_lossy().into_owned(), path]
            } else {
                vec![path]
            }
        }
        (None, None) => return Grade::new(label, false, 0.0, "no command or script"),
    };
    // the result and trace files live in a fresh directory of our own, not at a name in
    // the shared temp directory that someone could have planted a link at
    let scratch = match ScratchDir::new("grade") {
        Ok(d) => d,
        Err(e) => return Grade::new(label, false, 0.0, format!("can't make a directory for the grade: {e}")),
    };
    let result = scratch.0.join("grade.json");
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if let Some(t) = trace {
        let f = scratch.0.join("trace.json");
        if let Err(e) = crate::fsutil::write_private(&f, serde_json::to_vec(t).unwrap_or_default()) {
            return Grade::new(label, false, 0.0, format!("can't write the trace for the grader: {e}"));
        }
        env.insert("HLOCK_TRACE".into(), f.to_string_lossy().into_owned());
    }
    env.insert("HLOCK_WORKSPACE".into(), ws.to_string_lossy().into_owned());
    env.insert("HLOCK_PACK_DIR".into(), pack_dir.to_string_lossy().into_owned());
    env.insert("HLOCK_TASK".into(), task.id.clone());
    env.insert("HLOCK_GRADE".into(), result.to_string_lossy().into_owned());
    let limit = Duration::from_secs(*timeout_seconds);
    let (code, output) = match sandbox {
        Some(sb) => {
            use crate::sandbox::Mount;
            let mounts = vec![Mount::rw(&scratch.0), Mount::ro(pack_dir)];
            let limits = Limits { max_bytes: MAX_TEST_OUTPUT, ..Limits::timeout(limit) };
            match sb.run_command("graders", &argv, ws, env, mounts, &crate::profiles::Ctx::from_process(), &limits) {
                Err(e) => (None, format!("can't run it in the sandbox: {e}")),
                Ok(out) if out.timed_out => (None, format!("{}\ntimed out after {}s", out.text(), limit.as_secs())),
                Ok(out) => (out.code.or(Some(-1)), out.text()),
            }
        }
        None => {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(ws).envs(&env);
            run_limited(cmd, limit)
        }
    };
    let reported: Option<Value> = read_regular(&result, 1 << 20).ok().and_then(|b| serde_json::from_slice(&b).ok());
    drop(scratch);
    let passed = code == Some(0);
    let detail = |fallback: String| reported.as_ref().and_then(|r| r["detail"].as_str()).map(str::to_string).unwrap_or(fallback);
    match code {
        None => Grade::new(label, false, 0.0, tail(&output)),
        Some(c) => {
            let score = reported.as_ref().and_then(|r| r["score"].as_f64()).unwrap_or(if passed { 1.0 } else { 0.0 });
            let success = reported.as_ref().and_then(|r| r["success"].as_bool()).unwrap_or(passed);
            let fallback = if success { String::new() } else { format!("exit code {c}\n{}", tail(&output)).trim_end().to_string() };
            Grade::new(label, success, score, detail(fallback))
        }
    }
}

/// The files a judge reads, by path: their text (cut at [`MAX_JUDGE_FILE`] characters),
/// or `None` when the run didn't write one as a regular file (a symlink it planted
/// isn't followed).
fn judge_files(artifacts: &[String], ws: &Path) -> Vec<(String, Option<String>)> {
    // enough bytes for MAX_JUDGE_FILE characters of any width, and one more
    let cap = MAX_JUDGE_FILE * 4 + 1;
    artifacts
        .iter()
        .map(|a| {
            let read = if safe_rel(a) { read_regular(&ws.join(a), cap).ok() } else { None };
            let text = read.map(|b| {
                let text = String::from_utf8_lossy(&b).into_owned();
                let cut: String = text.chars().take(MAX_JUDGE_FILE).collect();
                if cut.len() < text.len() || b.len() >= cap { format!("{cut}\n[… cut]") } else { cut }
            });
            (a.clone(), text)
        })
        .collect()
}

/// What a judge grader asks: its criteria and levels, from the grader or its rubric
/// file, and the rubric's text.
fn judge_spec(g: &Grader, pack_dir: &Path) -> Result<(Vec<Criterion>, Vec<String>, Option<String>), String> {
    let Grader::Judge { rubric, criteria, levels, .. } = g else { return Err(format!("{} isn't a judge grader", g.label())) };
    match rubric {
        Some(r) => {
            let text = std::fs::read_to_string(pack_dir.join(r)).map_err(|e| format!("can't read rubric {r}: {e}"))?;
            let (c, l) = rubric_parts(&text);
            // the same rule as inline `levels`
            if !l.is_empty() && !(2..=10).contains(&l.len()) {
                return Err(format!("rubric {r}: `## Levels` needs 2 to 10 levels, worst first (it has {})", l.len()));
            }
            Ok((c, l, Some(text)))
        }
        None => Ok((criteria.clone(), levels.clone(), None)),
    }
}

/// The LLM judge's prompt: the task, the rubric, criteria or levels, and the files.
pub fn judge_prompt(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path) -> Result<String, String> {
    let Grader::Judge { artifacts, .. } = g else { return Err(format!("{} isn't a judge grader", g.label())) };
    let (criteria, levels, rubric) = judge_spec(g, pack_dir)?;
    let mut p = format!("<task>\n{}\n</task>\n\n", task.prompt_text.trim());
    if let Some(text) = &rubric {
        p.push_str(&format!("<rubric>\n{}\n</rubric>\n\n", text.trim()));
    }
    let structured = rubric.is_none() || !criteria.is_empty() || !levels.is_empty();
    if structured && !criteria.is_empty() {
        let items: Vec<String> = criteria.iter().enumerate().map(|(i, c)| format!("{}. {}", i + 1, c.line())).collect();
        p.push_str(&format!("<criteria>\n{}\n</criteria>\n\n", items.join("\n")));
    }
    if structured && !levels.is_empty() {
        let items: Vec<String> = levels.iter().enumerate().map(|(i, l)| format!("{i}. {l}")).collect();
        p.push_str(&format!("<levels>\n{}\n</levels>\n\n", items.join("\n")));
    }
    p.push_str("The agent left these files in its workspace:\n\n");
    for (a, text) in judge_files(artifacts, ws) {
        match text {
            Some(t) => p.push_str(&format!("<file path={a:?}>\n{t}\n</file>\n\n")),
            None => p.push_str(&format!("<file path={a:?} missing=\"true\"/>\n\n")),
        }
    }
    let mut keys = Vec::new();
    if structured && !criteria.is_empty() {
        keys.push("\"criteria\": [<true or false for each criterion, in order>]".to_string());
    }
    if structured && !levels.is_empty() {
        keys.push(format!("\"level\": <the number of the level the files reach, 0 to {}>", levels.len() - 1));
    }
    if !structured {
        keys.push("\"score\": <a number from 0 to 1: how fully the files meet the rubric>".to_string());
    }
    keys.push("\"reason\": \"<one or two sentences>\"".to_string());
    p.push_str(&format!("Reply with JSON only: {{{}}}", keys.join(", ")));
    Ok(p)
}

/// Read a judge's reply: `(score, reason)`.
pub fn judge_reply(reply: &str, n_criteria: usize) -> Result<(f64, String), String> {
    judge_reply_parts(reply, n_criteria, 0).map(|(s, r, _)| (s, r))
}

/// Read an LLM judge's reply: the score (the mean of the criteria met and the level
/// reached, each 0–1), the reason, and the parts.
fn judge_reply_parts(reply: &str, n_criteria: usize, n_levels: usize) -> Result<(f64, String, Vec<Part>), String> {
    let bad = || format!("the judge didn't reply with JSON: {:?}", short(reply));
    let (i, j) = (reply.find('{').ok_or_else(bad)?, reply.rfind('}').ok_or_else(bad)?);
    let v: Value = serde_json::from_str(reply.get(i..=j).ok_or_else(bad)?).map_err(|_| bad())?;
    let mut reason = v["reason"].as_str().unwrap_or_default().to_string();
    let (mut scores, mut parts) = (Vec::new(), Vec::new());
    if n_criteria > 0 {
        let met: Vec<bool> = v["criteria"]
            .as_array()
            .ok_or_else(bad)?
            .iter()
            .map(|c| c.as_bool().or_else(|| c["met"].as_bool()).unwrap_or(false))
            .collect();
        if met.len() != n_criteria {
            return Err(format!("the judge answered {} of {n_criteria} criteria", met.len()));
        }
        let unmet: Vec<String> = met.iter().enumerate().filter(|(_, m)| !**m).map(|(i, _)| (i + 1).to_string()).collect();
        if !unmet.is_empty() {
            reason = format!("unmet: {}. {reason}", unmet.join(", ")).trim().to_string();
        }
        parts.extend(met.iter().map(|m| Part { label: String::new(), value: if *m { 1.0 } else { 0.0 } }));
        scores.push(met.iter().filter(|m| **m).count() as f64 / n_criteria as f64);
    }
    if n_levels == 1 {
        return Err("a scale needs 2 to 10 levels; this one has 1".into());
    }
    if n_levels > 0 {
        let level = v["level"].as_f64().ok_or_else(bad)?.clamp(0.0, (n_levels - 1) as f64);
        scores.push(level / (n_levels - 1) as f64);
        parts.push(Part { label: String::new(), value: level });
    }
    if n_criteria == 0 && n_levels == 0 {
        scores.push(v["score"].as_f64().ok_or_else(bad)?.clamp(0.0, 1.0));
    }
    Ok((unit(scores.iter().sum::<f64>() / scores.len() as f64), reason, parts))
}

/// `x` within 0–1 (NaN is 0), for scores and probabilities a model reported.
fn unit(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

const NO_JUDGE: &str = "no judge model: set [evals.judge] model in harness.toml or [pack.judge] in pack.toml, run Kev on Ollama or Ollaya, or set TYPESAFE_API_KEY";

/// Reads the judge's keys and endpoints through `var`.
type Var<'a> = &'a dyn Fn(&str) -> Option<String>;

fn judge_run(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path, judge: Option<&JudgeConfig>, var: Var) -> Grade {
    let Some(judge) = judge else {
        return Grade::new(g.label(), false, 0.0, NO_JUDGE);
    };
    if !judge.is_decider() {
        return llm_judge(g, task, ws, pack_dir, judge, var);
    }
    let grade = decider_judge(g, task, ws, pack_dir, judge, var);
    match judge.fallback.as_deref().filter(|f| !f.trim().is_empty()) {
        Some(fallback) if grade.uncertain => {
            let llm = llm_judge(g, task, ws, pack_dir, &JudgeConfig { base_url: judge.fallback_base_url.clone(), ..JudgeConfig::new(fallback) }, var);
            let detail = format!(
                "uncertain at {:.2} by {}; re-judged by {fallback}: {}",
                grade.score,
                grade.judged_by.as_deref().unwrap_or(&judge.model),
                llm.detail
            );
            Grade { detail, parts: grade.parts, uncertain: true, ..llm }
        }
        _ => grade,
    }
}

fn llm_judge(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path, judge: &JudgeConfig, var: Var) -> Grade {
    let label = format!("{} ({})", g.label(), judge.model);
    let Grader::Judge { pass, .. } = g else { return Grade::new(label, false, 0.0, "not a judge grader") };
    let run = || -> Result<(f64, String, Vec<Part>), String> {
        let (criteria, levels, rubric) = judge_spec(g, pack_dir)?;
        let structured = rubric.is_none() || !criteria.is_empty() || !levels.is_empty();
        let cfg = judge.agent_config(var)?;
        let prompt = judge_prompt(g, task, ws, pack_dir)?;
        let (reply, _, _) = crate::agent::complete(&cfg, JUDGE_SYSTEM, &prompt).map_err(|e| e.to_string())?;
        let (n_c, n_l) = if structured { (criteria.len(), levels.len()) } else { (0, 0) };
        let (score, reason, mut parts) = judge_reply_parts(&reply, n_c, n_l)?;
        let mut labels: Vec<String> = criteria.iter().map(|c| c.text.clone()).collect();
        if n_l > 0 {
            labels.push("level".into());
        }
        for (p, l) in parts.iter_mut().zip(labels) {
            p.label = l;
        }
        Ok((score, reason, parts))
    };
    match run() {
        Ok((score, reason, parts)) => Grade { parts, judged_by: Some(judge.model.clone()), ..Grade::new(label, score >= *pass, score, reason) },
        Err(e) => Grade::new(label, false, 0.0, e),
    }
}

/// Score answers into a grade: criteria (nouls) and a level (a score question), each
/// counted as one part of the mean. Probabilities are kept within 0–1, and a level
/// outside the scale is an error.
fn decided_grade(label: String, decided: &crate::decide::Decided, criteria: &[Criterion], levels: &[String], pass: f64, margin: f64) -> Result<Grade, String> {
    let (mut scores, mut parts, mut certainty) = (Vec::new(), Vec::new(), 1.0f64);
    let mut unmet = Vec::new();
    for (i, c) in criteria.iter().enumerate() {
        let Some(a) = decided.get(&format!("c{}", i + 1)) else { continue };
        if a.refused {
            return Err(format!("{} gave no answer for {:?}", decided.model, c.text));
        }
        let p = unit(a.probability());
        certainty = certainty.min(unit(a.certainty()));
        if p < 0.5 {
            unmet.push(format!("{} ({p:.2})", c.text));
        }
        parts.push(Part { label: c.text.clone(), value: p });
    }
    if !criteria.is_empty() {
        let ps: Vec<f64> = parts.iter().map(|p| p.value).collect();
        scores.push(ps.iter().sum::<f64>() / ps.len().max(1) as f64);
    }
    let mut detail = if unmet.is_empty() { String::new() } else { format!("unmet: {}", unmet.join("; ")) };
    if let Some(a) = decided.get("level").filter(|_| levels.len() >= 2) {
        if a.refused {
            return Err(format!("{} gave no answer for the level", decided.model));
        }
        let top = levels.len() - 1;
        if let Some(l) = a.probabilities.keys().find(|l| !l.parse::<usize>().is_ok_and(|n| n <= top)) {
            return Err(format!("{} answered level {l:?}, which isn't on the 0 to {top} scale", decided.model));
        }
        let level = a.value.as_f64().unwrap_or(0.0);
        if !(0.0..=top as f64).contains(&level) {
            return Err(format!("{} answered level {level}, which isn't on the 0 to {top} scale", decided.model));
        }
        certainty = certainty.min(unit(a.certainty()));
        let reached = levels.get(level.round() as usize).cloned().unwrap_or_default();
        scores.push(level / (levels.len() - 1) as f64);
        parts.push(Part { label: format!("level: {reached}"), value: level });
        let sep = if detail.is_empty() { "" } else { "; " };
        detail = format!("{detail}{sep}level {level:.1} of {}: {reached}", levels.len() - 1);
    }
    let score = unit(scores.iter().sum::<f64>() / scores.len().max(1) as f64);
    if score < pass && detail.is_empty() {
        detail = format!("score {score:.2} is below {pass}");
    }
    let uncertain = (score - pass).abs() < margin || certainty < 0.5;
    Ok(Grade { parts, uncertain, judged_by: Some(decided.model.clone()), cost_usd: decided.cost_usd, ..Grade::new(label, score >= pass, score, detail) })
}

fn decider_endpoint(judge: &JudgeConfig, var: Var) -> Result<crate::decide::Endpoint, String> {
    judge.decider().endpoint(var)
}

fn decider_judge(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path, judge: &JudgeConfig, var: Var) -> Grade {
    let label = format!("{} ({})", g.label(), judge.model);
    let Grader::Judge { artifacts, pass, .. } = g else { return Grade::new(label, false, 0.0, "not a judge grader") };
    let run = || -> Result<Grade, String> {
        let (criteria, levels, _) = judge_spec(g, pack_dir)?;
        if criteria.is_empty() && levels.is_empty() {
            return Err("a decision model needs criteria or levels; this rubric has neither (list criteria as `- ` items, or add a `## Levels` list), or use an LLM judge".into());
        }
        let files: serde_json::Map<String, Value> =
            judge_files(artifacts, ws).into_iter().map(|(a, t)| (a, t.map(Value::String).unwrap_or_else(|| json!("(missing: the run didn't write this file)")))).collect();
        let state = json!({"task": task.prompt_text.trim(), "files": files});
        let mut qs: Vec<Question> = criteria.iter().enumerate().map(|(i, c)| c.question(format!("c{}", i + 1))).collect();
        if levels.len() >= 2 {
            qs.push(Question::score("level", "Where do the files the agent wrote sit on this scale, for the task in the state?", &levels));
        }
        let decided = crate::decide::decide(&decider_endpoint(judge, var)?, &state, &qs)?;
        decided_grade(label.clone(), &decided, &criteria, &levels, *pass, judge.uncertain.unwrap_or(DEFAULT_UNCERTAIN))
    };
    run().unwrap_or_else(|e| Grade::new(label, false, 0.0, e))
}

/// The tool calls in a trace, as a decider's state: one line per call.
fn trace_state(trace: &Value) -> Value {
    let events = trace.get("events").unwrap_or(trace);
    let calls: Vec<Value> = events
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"].as_str().is_some_and(|t| t.starts_with("tool")) || e.get("tool").is_some() || e.get("tool_name").is_some())
        .map(|e| json!({"tool": e.get("tool").or_else(|| e.get("tool_name")), "input": e.get("input").or_else(|| e.get("tool_input")), "status": e.get("status")}))
        .collect();
    json!({"tool_calls": calls})
}

fn trace_run(g: &Grader, task: &Task, trace: &Value, judge: Option<&JudgeConfig>, var: Var) -> Grade {
    let label = g.label();
    let Grader::Trace { criteria, pass } = g else { return Grade::new(label, false, 0.0, "not a trace grader") };
    let Some(judge) = judge.filter(|j| j.is_decider()) else {
        return Grade::new(label, false, 0.0, "trace graders need a decision model as the judge (see [evals.judge])");
    };
    let label = format!("{label} ({})", judge.model);
    let run = || -> Result<Grade, String> {
        let mut state = trace_state(trace);
        state["task"] = json!(task.prompt_text.trim());
        let qs: Vec<Question> = criteria.iter().enumerate().map(|(i, c)| c.question(format!("c{}", i + 1))).collect();
        let decided = crate::decide::decide(&decider_endpoint(judge, var)?, &state, &qs)?;
        decided_grade(label.clone(), &decided, criteria, &[], *pass, judge.uncertain.unwrap_or(DEFAULT_UNCERTAIN))
    };
    run().unwrap_or_else(|e| Grade::new(label, false, 0.0, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::{Answer, Decided, Kind};

    fn noul(id: &str, p: Option<f64>) -> Answer {
        Answer { id: id.into(), kind: Kind::Noul, value: p.map_or(Value::Null, Value::from), probabilities: Default::default(), confidence: None, refused: p.is_none() }
    }

    #[test]
    fn a_refused_answer_is_an_error_not_a_zero() {
        let criteria = [Criterion { text: "has tests".into(), if_true: None, if_false: None }, Criterion { text: "is short".into(), if_true: None, if_false: None }];
        let ok = Decided { model: "m".into(), answers: vec![noul("c1", Some(0.9)), noul("c2", Some(0.8))], ..Default::default() };
        assert!(decided_grade("j".into(), &ok, &criteria, &[], 0.5, 0.1).unwrap().success);
        let refused = Decided { model: "m".into(), answers: vec![noul("c1", Some(0.9)), noul("c2", None)], ..Default::default() };
        let e = decided_grade("j".into(), &refused, &criteria, &[], 0.5, 0.1).unwrap_err();
        assert!(e.contains("no answer") && e.contains("is short"), "{e}");
    }
}
