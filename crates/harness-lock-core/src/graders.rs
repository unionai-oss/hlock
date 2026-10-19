//! Graders: how a run of an eval-pack task is scored (SPEC §7.1).
//!
//! A task lists its graders in `pack.toml`, each a short string or a table:
//!
//! - `artifact`: output files exist and, optionally, match expected content.
//! - `tests`: a command or script runs in the workspace after the agent; exit 0 passes,
//!   and it may report partial credit as JSON in `$HLOCK_GRADE`.
//! - `judge`: a model scores the output files against a rubric or a list of criteria.
//!
//! Every file in `expected.artifacts` must also exist, so a task with no graders is
//! graded on its artifacts alone.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{provider_base_url, provider_key_env, AgentConfig};
use crate::tasks::{Task, PLACEHOLDER};

pub const DEFAULT_TESTS_TIMEOUT: u64 = 300;
/// The judge score a run needs to pass, unless the grader sets `pass`.
pub const DEFAULT_JUDGE_PASS: f64 = 0.7;
/// The most of one file a judge is shown, in characters.
const MAX_JUDGE_FILE: usize = 20_000;
/// Lines of a failing test command's output kept in the grade's detail.
const DETAIL_LINES: usize = 12;

const JUDGE_SYSTEM: &str = "You grade the work of an AI agent. Judge only what the files show, \
    be strict and literal, and don't give credit for intentions. Reply with JSON only.";

/// The model that scores `judge` graders: `[evals.judge]` in `harness.toml`, else
/// `[pack.judge]` in `pack.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct JudgeConfig {
    /// `provider:model`, e.g. `anthropic:claude-sonnet-5-5`.
    pub model: String,
    /// An OpenAI-compatible endpoint, for providers hlock doesn't know and local runtimes.
    pub base_url: Option<String>,
    /// The variable the API key is read from (default: the provider's usual one).
    pub key_env: Option<String>,
}

impl JudgeConfig {
    pub fn is_set(&self) -> bool {
        !self.model.trim().is_empty()
    }

    fn agent_config(&self) -> Result<AgentConfig, String> {
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
        let api_key = key_env.as_ref().and_then(|k| std::env::var(k).ok()).filter(|v| !v.is_empty());
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
    /// A model scores `artifacts` against a `rubric` file or a list of `criteria`.
    Judge { rubric: Option<String>, criteria: Vec<String>, artifacts: Vec<String>, pass: f64 },
    /// A grader type from the spec that isn't built yet (`trace`); grading skips it.
    Unsupported(String),
}

/// The result of one grader on one run.
#[derive(Debug, Clone, Serialize)]
pub struct Grade {
    pub grader: String,
    pub success: bool,
    pub score: f64,
    /// Why it failed, or what the judge said.
    pub detail: String,
}

impl Grade {
    fn new(grader: String, success: bool, score: f64, detail: impl Into<String>) -> Grade {
        Grade { grader, success, score: score.clamp(0.0, 1.0), detail: detail.into() }
    }
}

/// How to grade: the judge to use, and whether to skip judges (`hlock evals check --run`
/// grades offline).
#[derive(Debug, Clone, Default)]
pub struct GradeOptions {
    pub judge: Option<JudgeConfig>,
    pub skip_judges: bool,
}

impl Grader {
    /// A short name for reports and traces, e.g. `tests:greet/check.py`.
    pub fn label(&self) -> String {
        match self {
            Grader::Artifact { paths, .. } => format!("artifact:{}", paths.join(",")),
            Grader::Tests { script: Some(s), .. } => format!("tests:{s}"),
            Grader::Tests { command, .. } => format!("tests:{}", command.as_deref().unwrap_or_default()),
            Grader::Judge { rubric: Some(r), .. } => format!("judge:{r}"),
            Grader::Judge { criteria, .. } => format!("judge:{} criteria", criteria.len()),
            Grader::Unsupported(k) => k.clone(),
        }
    }

    /// Whether it does more than check that files exist. A skipped (unsupported) grader
    /// checks nothing.
    pub fn checks_content(&self) -> bool {
        match self {
            Grader::Artifact { equals, same_as, contains, .. } => equals.is_some() || same_as.is_some() || !contains.is_empty(),
            Grader::Unsupported(_) => false,
            _ => true,
        }
    }

    /// Files in the pack this grader reads: test scripts and files, rubrics, golden files.
    pub fn pack_files(&self) -> Vec<String> {
        match self {
            Grader::Artifact { same_as, .. } => same_as.iter().cloned().collect(),
            Grader::Tests { script, files, .. } => script.iter().chain(files).cloned().collect(),
            Grader::Judge { rubric, .. } => rubric.iter().cloned().collect(),
            Grader::Unsupported(_) => vec![],
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
pub fn parse(spec: &Value, task: &Task) -> Result<Grader, String> {
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
                "judge" if !arg.is_empty() => judge(Some(arg.into()), vec![], expected, DEFAULT_JUDGE_PASS),
                "tests" | "judge" => Err(format!("{s:?} needs a file: \"{kind}:<path in the pack>\"")),
                "trace" => Ok(Grader::Unsupported("trace".into())),
                other => Err(format!("unknown grader type {other:?} (use artifact, tests, or judge)")),
            };
        }
        Value::Object(m) => m,
        _ => return Err("a grader must be a string like \"tests:check.py\" or a table with a `type`".into()),
    };
    let kind = table.get("type").and_then(Value::as_str).ok_or("a grader table needs a `type` (artifact, tests, or judge)")?;
    let allowed: &[&str] = match kind {
        "artifact" => &["path", "paths", "equals", "same-as", "contains"],
        "tests" => &["command", "script", "files", "timeout-seconds"],
        "judge" => &["rubric", "criteria", "artifacts", "pass"],
        "trace" => return Ok(Grader::Unsupported("trace".into())),
        other => return Err(format!("unknown grader type {other:?} (use artifact, tests, or judge)")),
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
            let artifacts = list("artifacts")?;
            let pass = match table.get("pass") {
                None => DEFAULT_JUDGE_PASS,
                Some(v) => v.as_f64().filter(|p| (0.0..=1.0).contains(p)).ok_or("`pass` must be a number from 0 to 1")?,
            };
            judge(text("rubric")?, list("criteria")?, if artifacts.is_empty() { expected } else { artifacts }, pass)
        }
    }
}

fn judge(rubric: Option<String>, criteria: Vec<String>, artifacts: Vec<String>, pass: f64) -> Result<Grader, String> {
    if rubric.is_none() == criteria.is_empty() {
        return Err("a judge grader needs a `rubric` file or a list of `criteria` (one of them)".into());
    }
    if artifacts.is_empty() {
        return Err("a judge grader has no files to read; set `artifacts` or expected.artifacts".into());
    }
    Ok(Grader::Judge { rubric, criteria, artifacts, pass })
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
    for g in &graders {
        let grade = match g {
            Err(e) => Grade::new("invalid".into(), false, 0.0, e.clone()),
            Ok(Grader::Unsupported(_)) => continue,
            Ok(Grader::Judge { .. }) if opts.skip_judges => continue,
            Ok(Grader::Artifact { paths, equals, same_as, contains }) => {
                artifact(paths, equals.as_deref(), same_as.as_deref(), contains, workspace, &pack_dir)
            }
            Ok(g @ Grader::Tests { .. }) => tests(g, task, workspace, &pack_dir),
            Ok(g @ Grader::Judge { .. }) => judge_run(g, task, workspace, &pack_dir, opts.judge.as_ref()),
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
        let f = ws.join(p);
        if !f.exists() {
            problems.push(format!("{p} is missing"));
            continue;
        }
        let before = problems.len();
        if equals.is_some() || golden.is_some() || !contains.is_empty() {
            let text = std::fs::read(&f).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
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

fn copy_into(src: &Path, ws: &Path) -> std::io::Result<()> {
    let name = src.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    if src.is_dir() {
        crate::adapters::copy_dir(src, &ws.join(name));
        Ok(())
    } else {
        std::fs::copy(src, ws.join(name)).map(|_| ())
    }
}

/// Run a command with a time limit; returns its exit code (None when it timed out or
/// couldn't start) and its combined output.
fn run_limited(mut cmd: Command, limit: Duration) -> (Option<i32>, String) {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // its own process group, so a timeout stops what it started too (`cd x && pytest`)
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (None, format!("can't run it: {e}")),
    };
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code().or(Some(-1)),
            Ok(None) if start.elapsed() > limit => {
                #[cfg(unix)]
                let _ = Command::new("kill").args(["-KILL", &format!("-{}", child.id())]).stderr(Stdio::null()).status();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let mut out = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    out.push_str(&String::from_utf8_lossy(&t_err.join().unwrap_or_default()));
    if code.is_none() && start.elapsed() > limit {
        out.push_str(&format!("\ntimed out after {}s", limit.as_secs()));
    }
    (code, out)
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(DETAIL_LINES)..].join("\n")
}

fn tests(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path) -> Grade {
    let Grader::Tests { command, script, files, timeout_seconds } = g else { unreachable!() };
    let label = g.label();
    for f in files {
        if let Err(e) = copy_into(&pack_dir.join(f), ws) {
            return Grade::new(label, false, 0.0, format!("can't copy {f} into the workspace: {e}"));
        }
    }
    let shell = |line: &str| {
        let mut c = if cfg!(windows) { Command::new("cmd") } else { Command::new("sh") };
        c.args([if cfg!(windows) { "/C" } else { "-c" }, line]);
        c
    };
    let mut cmd = match (command, script) {
        (Some(line), _) => shell(line),
        (None, Some(s)) => {
            let path = pack_dir.join(s);
            if path.extension().is_some_and(|e| e == "py") {
                let py = crate::profiles::python_for(None, &crate::profiles::Ctx::from_process())
                    .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }));
                let mut c = Command::new(py);
                c.arg(&path);
                c
            } else {
                Command::new(&path)
            }
        }
        (None, None) => return Grade::new(label, false, 0.0, "no command or script"),
    };
    static N: AtomicUsize = AtomicUsize::new(0);
    let result = std::env::temp_dir().join(format!("hlock-grade-{}-{}.json", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_file(&result);
    cmd.current_dir(ws)
        .env("HLOCK_WORKSPACE", ws)
        .env("HLOCK_PACK_DIR", pack_dir)
        .env("HLOCK_TASK", &task.id)
        .env("HLOCK_GRADE", &result);
    let (code, output) = run_limited(cmd, Duration::from_secs(*timeout_seconds));
    let reported: Option<Value> = std::fs::read_to_string(&result).ok().and_then(|t| serde_json::from_str(&t).ok());
    let _ = std::fs::remove_file(&result);
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

/// The judge's prompt: the task, the rubric, and the files to score.
pub fn judge_prompt(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path) -> Result<String, String> {
    let Grader::Judge { rubric, criteria, artifacts, .. } = g else { unreachable!() };
    let mut p = format!("<task>\n{}\n</task>\n\n", task.prompt_text.trim());
    match rubric {
        Some(r) => {
            let text = std::fs::read_to_string(pack_dir.join(r)).map_err(|e| format!("can't read rubric {r}: {e}"))?;
            p.push_str(&format!("<rubric>\n{}\n</rubric>\n\n", text.trim()));
        }
        None => {
            let items: Vec<String> = criteria.iter().enumerate().map(|(i, c)| format!("{}. {c}", i + 1)).collect();
            p.push_str(&format!("<criteria>\n{}\n</criteria>\n\n", items.join("\n")));
        }
    }
    p.push_str("The agent left these files in its workspace:\n\n");
    for a in artifacts {
        match std::fs::read(ws.join(a)) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let cut: String = text.chars().take(MAX_JUDGE_FILE).collect();
                let more = if cut.len() < text.len() { "\n[… cut]" } else { "" };
                p.push_str(&format!("<file path={a:?}>\n{cut}{more}\n</file>\n\n"));
            }
            Err(_) => p.push_str(&format!("<file path={a:?} missing=\"true\"/>\n\n")),
        }
    }
    p.push_str(if criteria.is_empty() {
        "Score how fully the files meet the rubric. Reply with JSON only: \
         {\"score\": <a number from 0 to 1>, \"reason\": \"<one or two sentences>\"}"
    } else {
        "Decide whether the files meet each criterion. Reply with JSON only: \
         {\"criteria\": [<true or false for each criterion, in order>], \"reason\": \"<one or two sentences>\"}"
    });
    Ok(p)
}

/// Read a judge's reply: `(score, reason)`.
pub fn judge_reply(reply: &str, n_criteria: usize) -> Result<(f64, String), String> {
    let bad = || format!("the judge didn't reply with JSON: {:?}", short(reply));
    let (i, j) = (reply.find('{').ok_or_else(bad)?, reply.rfind('}').ok_or_else(bad)?);
    let v: Value = serde_json::from_str(reply.get(i..=j).ok_or_else(bad)?).map_err(|_| bad())?;
    let reason = v["reason"].as_str().unwrap_or_default().to_string();
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
        let reason = if unmet.is_empty() { reason } else { format!("unmet: {}. {reason}", unmet.join(", ")) };
        return Ok((met.iter().filter(|m| **m).count() as f64 / n_criteria as f64, reason.trim().to_string()));
    }
    let score = v["score"].as_f64().ok_or_else(bad)?;
    Ok((score.clamp(0.0, 1.0), reason))
}

fn judge_run(g: &Grader, task: &Task, ws: &Path, pack_dir: &Path, judge: Option<&JudgeConfig>) -> Grade {
    let Grader::Judge { criteria, pass, .. } = g else { unreachable!() };
    let Some(judge) = judge.filter(|j| j.is_set()) else {
        return Grade::new(g.label(), false, 0.0, "no judge model: set [evals.judge] model in harness.toml, or [pack.judge] in pack.toml");
    };
    let label = format!("{} ({})", g.label(), judge.model);
    let run = || -> Result<(f64, String), String> {
        let cfg = judge.agent_config()?;
        let prompt = judge_prompt(g, task, ws, pack_dir)?;
        let (reply, _, _) = crate::agent::complete(&cfg, JUDGE_SYSTEM, &prompt).map_err(|e| e.to_string())?;
        judge_reply(&reply, criteria.len())
    };
    match run() {
        Ok((score, reason)) => Grade::new(label, score >= *pass, score, reason),
        Err(e) => Grade::new(label, false, 0.0, e),
    }
}
