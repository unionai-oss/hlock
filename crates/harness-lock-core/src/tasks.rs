//! Eval packs: the tasks a stack is measured against.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
pub use crate::graders::{Grade, GradeOptions, Grader, JudgeConfig, Judges};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    pub fixtures: Option<PathBuf>,
    /// Files a correct run leaves in the workspace, used to check the graders and by
    /// the offline reference harness.
    pub solution: Option<PathBuf>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub expected: BTreeMap<String, serde_json::Value>,
    /// Graders as written in `pack.toml`: strings or tables (see [`crate::graders`]).
    #[serde(default)]
    pub graders: Vec<serde_json::Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(rename = "timeout-seconds", default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(skip)]
    pub prompt_text: String,
    #[serde(skip)]
    pub pack_dir: Option<PathBuf>,
}

fn default_timeout() -> u64 {
    600
}

impl Task {
    pub fn expected_artifacts(&self) -> Vec<String> {
        self.expected
            .get("artifacts")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The task's graders, parsed; an entry that doesn't parse is an `Err` saying why.
    pub fn grader_list(&self) -> Vec<std::result::Result<Grader, String>> {
        self.graders.iter().map(|g| crate::graders::parse(g, self)).collect()
    }

    pub fn load_prompt_text(&mut self) {
        if !self.prompt_text.is_empty() {
            return;
        }
        if let (Some(dir), true) = (&self.pack_dir, crate::fsutil::safe_rel(&self.prompt)) {
            let p = dir.join(&self.prompt);
            if let Ok(t) = std::fs::read_to_string(&p) {
                self.prompt_text = t;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PackHeader {
    pub name: String,
    pub version: String,
    pub domain: String,
    pub license: String,
    /// `[pack.judge]`: the model that scores this pack's `judge` graders.
    pub judge: Option<JudgeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PackFile {
    pub pack: PackHeader,
    pub task: Vec<Task>,
}

#[derive(Debug, Clone)]
pub struct EvalPack {
    pub name: String,
    pub version: String,
    pub domain: String,
    pub path: PathBuf,
    pub hash: String,
    pub tasks: Vec<Task>,
    pub judge: Option<JudgeConfig>,
}

/// Marks a spot in a scaffolded eval pack that still needs filling in.
pub const PLACEHOLDER: &str = "TODO(hlock)";

/// Below this many tasks the holdout split is too small to say much.
pub const MIN_TASKS: usize = 10;

/// One comparison [`EvalPack::calibrate`] makes: a criterion (or the overall
/// `(passes)`) on one run, as judges `a` and `b` scored it.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationRow {
    pub task: String,
    /// `solution` or `fixtures` (the run with no work done).
    pub run: String,
    pub grader: String,
    pub criterion: String,
    pub a: f64,
    pub b: f64,
    pub agree: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// A problem [`EvalPack::check`] found. `level` is `error` (the pack isn't usable
/// yet) or `warn` (it runs, but the evidence it produces is weak).
#[derive(Debug, Clone, Serialize)]
pub struct PackIssue {
    pub level: String,
    pub task: Option<String>,
    pub message: String,
}

impl PackIssue {
    fn error(task: Option<&str>, message: String) -> PackIssue {
        PackIssue { level: "error".into(), task: task.map(str::to_string), message }
    }

    fn warn(task: Option<&str>, message: String) -> PackIssue {
        PackIssue { level: "warn".into(), task: task.map(str::to_string), message }
    }
}

#[deprecated(note = "renamed to `EvalPack`")]
pub type TaskPack = EvalPack;

/// Whether `id` can name a task: letters, digits, `.`, `_` and `-`, and a plain name
/// (not `.` or `..`), since it becomes a directory name in trial workspaces.
pub fn valid_task_id(id: &str) -> bool {
    crate::fsutil::plain_name(id) && id.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// A task's id and the paths it names in the pack and the workspace must stay inside
/// them. An empty id is left for [`EvalPack::check`] to report.
fn check_task_paths(t: &Task) -> Result<()> {
    let bad = |m: String| Err(Error::EvalPack(m));
    if !t.id.is_empty() && !valid_task_id(&t.id) {
        return bad(format!("task id {:?} may only have letters, digits, '.', '_' and '-'", t.id));
    }
    let name = if t.id.is_empty() { "a task" } else { t.id.as_str() };
    let paths = [("prompt", Some(t.prompt.clone()).filter(|p| !p.is_empty())), ("fixtures", t.fixtures.as_ref().map(|p| p.to_string_lossy().into_owned())), ("solution", t.solution.as_ref().map(|p| p.to_string_lossy().into_owned()))];
    for (key, p) in paths {
        if let Some(p) = p.filter(|p| !crate::fsutil::safe_rel(p)) {
            return bad(format!("{name}: {key} {p:?} must be a relative path inside the pack (no leading '/', no '..')"));
        }
    }
    if let Some(a) = t.expected_artifacts().into_iter().find(|a| !crate::fsutil::safe_rel(a)) {
        return bad(format!("{name}: expected artifact {a:?} must be a relative path inside the workspace (no leading '/', no '..')"));
    }
    Ok(())
}

/// The workspace for task `id` under `root`, a directory this process made: the id
/// must be a [`valid_task_id`] and the path must stay under `root`.
fn task_workspace(root: &Path, id: &str) -> Result<PathBuf> {
    if !valid_task_id(id) {
        return Err(Error::EvalPack(format!("task id {id:?} can't name a workspace directory")));
    }
    let ws = root.join(id);
    crate::fsutil::check_within("workspace", root, &ws)?;
    Ok(ws)
}

/// Copy each directory in `sources` into `ws`, without following symlinks.
fn fill_workspace(ws: &Path, sources: impl IntoIterator<Item = PathBuf>) -> Result<()> {
    std::fs::create_dir_all(ws)?;
    for src in sources {
        crate::fsutil::copy_tree(&src, ws)?;
    }
    Ok(())
}

impl EvalPack {
    pub fn load(source: &str, base: Option<&Path>) -> Result<EvalPack> {
        let p = PathBuf::from(source);
        let p = if p.is_absolute() {
            p
        } else if let Some(b) = base {
            b.join(&p)
        } else {
            p
        };
        let pack_path = p.join("pack.toml");
        let text = std::fs::read_to_string(&pack_path)
            .map_err(|e| Error::EvalPack(format!("missing pack.toml at {}: {e}", p.display())))?;
        let file: PackFile = toml::from_str(&text)
            .map_err(|e| Error::EvalPack(format!("invalid pack.toml: {e}")))?;
        let mut tasks = file.task;
        for t in tasks.iter_mut() {
            check_task_paths(t)?;
            t.pack_dir = Some(p.clone());
            t.load_prompt_text();
        }
        let hash = crate::hash::sha256_hex(text.as_bytes());
        Ok(EvalPack {
            name: file.pack.name,
            version: file.pack.version,
            domain: file.pack.domain,
            path: p,
            hash,
            tasks,
            judge: file.pack.judge.filter(JudgeConfig::is_set),
        })
    }

    /// Check that the pack is ready to measure stacks with: required fields set,
    /// prompts and fixtures present, no unfilled [`PLACEHOLDER`]s, and tasks that
    /// can fail.
    pub fn check(&self) -> Vec<PackIssue> {
        self.check_with(None)
    }

    /// [`check`](Self::check), knowing the judge `harness.toml` sets (`[evals.judge]`).
    pub fn check_with(&self, judge: Option<&JudgeConfig>) -> Vec<PackIssue> {
        let mut issues = Vec::new();
        for (key, value) in [("name", &self.name), ("version", &self.version), ("domain", &self.domain)] {
            if value.trim().is_empty() {
                issues.push(PackIssue::error(None, format!("[pack] {key} is missing")));
            }
        }
        if let Ok(text) = std::fs::read_to_string(self.path.join("pack.toml")) {
            let lines: Vec<String> = text.lines().enumerate()
                .filter(|(_, l)| l.contains(PLACEHOLDER))
                .map(|(i, _)| (i + 1).to_string())
                .collect();
            if !lines.is_empty() {
                let shown = if lines.len() > 5 { format!("{}, …", lines[..5].join(", ")) } else { lines.join(", ") };
                let word = if lines.len() == 1 { "line" } else { "lines" };
                issues.push(PackIssue::error(None, format!(
                    "pack.toml still has {} {PLACEHOLDER} ({word} {shown})", lines.len()
                )));
            }
        }
        if self.tasks.is_empty() {
            issues.push(PackIssue::error(None, "the pack has no [[task]] entries".into()));
            return issues;
        }
        let mut seen = std::collections::BTreeSet::new();
        for t in &self.tasks {
            let id = Some(t.id.as_str()).filter(|s| !s.is_empty());
            if t.id.is_empty() {
                issues.push(PackIssue::error(None, format!("a task has no id (prompt {:?})", t.prompt)));
            } else if !valid_task_id(&t.id) {
                issues.push(PackIssue::error(id, format!("task id {:?} may only have letters, digits, '.', '_' and '-'", t.id)));
            } else if !seen.insert(t.id.clone()) {
                issues.push(PackIssue::error(id, format!("duplicate task id {:?}", t.id)));
            }
            if t.prompt.is_empty() {
                issues.push(PackIssue::error(id, "no prompt file".into()));
            } else if !self.path.join(&t.prompt).is_file() {
                issues.push(PackIssue::error(id, format!("prompt file {} not found", t.prompt)));
            } else if t.prompt_text.trim().is_empty() {
                issues.push(PackIssue::error(id, format!("prompt file {} is empty", t.prompt)));
            } else {
                let n = t.prompt_text.matches(PLACEHOLDER).count();
                if n > 0 {
                    issues.push(PackIssue::error(id, format!("{} still has {n} {PLACEHOLDER}", t.prompt)));
                }
            }
            let fixtures = t.fixtures.as_ref().map(|f| self.path.join(f));
            if let Some(dir) = &fixtures {
                if !dir.is_dir() {
                    issues.push(PackIssue::error(id, format!("fixtures directory {} not found", t.fixtures.as_ref().unwrap().display())));
                }
            }
            if let Some(sol) = &t.solution {
                if !self.path.join(sol).is_dir() {
                    issues.push(PackIssue::error(id, format!("solution directory {} not found", sol.display())));
                }
            }
            let artifacts = t.expected_artifacts();
            let graders = t.grader_list();
            if artifacts.is_empty() && t.graders.is_empty() {
                issues.push(PackIssue::warn(id, "no expected artifacts or graders, so it always passes".into()));
            } else if !graders.iter().any(|g| matches!(g, Ok(g) if g.checks_content())) && graders.iter().all(|g| g.is_ok()) {
                let mut files = artifacts.clone();
                for g in &graders {
                    if let Ok(Grader::Artifact { paths, .. }) = g {
                        files.extend(paths.iter().filter(|p| !artifacts.contains(p)).cloned());
                    }
                }
                issues.push(PackIssue::warn(id, format!(
                    "only checks that {} exist{}; add a content check, tests, or a judge",
                    files.join(", "),
                    if files.len() == 1 { "s" } else { "" }
                )));
            }
            for (n, g) in graders.iter().enumerate() {
                match g {
                    Err(e) => issues.push(PackIssue::error(id, format!("grader {}: {e}", n + 1))),
                    Ok(g) => {
                        for (f, problem) in crate::graders::pack_file_problems(g, &self.path) {
                            issues.push(PackIssue::error(id, format!("{} grader file {f} {problem}", g.label().split(':').next().unwrap_or("a"))));
                        }
                    }
                }
            }
            if let Some(dir) = fixtures.as_ref().filter(|d| d.is_dir()) {
                for a in artifacts.iter().filter(|a| dir.join(a).exists()) {
                    issues.push(PackIssue::warn(id, format!("expected artifact {a} is already in the fixtures, so it passes without any work")));
                }
            }
        }
        let judged = self.tasks.iter().any(|t| t.grader_list().iter().any(|g| matches!(g, Ok(Grader::Judge { .. } | Grader::Trace { .. }))));
        let effective = judge.filter(|j| j.is_set()).or(self.judge.as_ref());
        if judged && effective.is_none() {
            issues.push(PackIssue::warn(None, "judge graders need a model: set [pack.judge] model in pack.toml or [evals.judge] in harness.toml (hlock otherwise looks for Kev on Ollama or Ollaya, then TYPESAFE_API_KEY)".into()));
        }
        if let Some(j) = self.judge.as_ref().filter(|j| j.model.ends_with("-latest") || j.model.ends_with("-preview")) {
            issues.push(PackIssue::warn(None, format!("[pack.judge] model {:?} is an alias that moves between releases; name a version (e.g. {})", j.model, crate::graders::DEFAULT_HOSTED_JUDGE)));
        }
        for t in &self.tasks {
            let id = Some(t.id.as_str()).filter(|s| !s.is_empty());
            for g in t.grader_list().into_iter().flatten() {
                match &g {
                    Grader::Judge { rubric: Some(r), .. } if effective.is_some_and(JudgeConfig::is_decider) => {
                        let text = std::fs::read_to_string(self.path.join(r)).unwrap_or_default();
                        let (c, l) = crate::graders::rubric_parts(&text);
                        if !text.is_empty() && c.is_empty() && l.is_empty() {
                            issues.push(PackIssue::warn(id, format!("{r} has no criteria (`- ` items) or `## Levels`, so a decision model can't judge it; add them, or use an LLM judge")));
                        }
                    }
                    Grader::Trace { .. } if effective.is_some_and(|j| !j.is_decider()) => {
                        issues.push(PackIssue::warn(id, "trace graders need a decision model as the judge".into()));
                    }
                    _ => {}
                }
            }
        }
        if self.tasks.len() < MIN_TASKS {
            let (_, holdout) = self.split(0.7, 0);
            issues.push(PackIssue::warn(None, format!(
                "{} tasks leaves {} for the holdout split; add tasks until there are at least {MIN_TASKS}",
                self.tasks.len(), holdout.len()
            )));
        }
        // prompts, judge rubrics and fixtures are an agent's input too
        for f in crate::scan::scan_path(&self.path, "") {
            match f.severity {
                crate::scan::Severity::Critical => issues.push(PackIssue::error(None, format!("{}: a character that hides text from a reviewer", f.label()))),
                crate::scan::Severity::Warning => issues.push(PackIssue::warn(None, format!("{}: an invisible character", f.label()))),
                crate::scan::Severity::Info => {}
            }
        }
        issues
    }

    /// Deterministic search/holdout split over task ids. `search_frac` is clamped to
    /// 0–1, and the search split gets at least one task when there is one.
    pub fn split(&self, search_frac: f64, seed: u64) -> (Vec<Task>, Vec<Task>) {
        let mut tasks: Vec<&Task> = self.tasks.iter().collect();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        let frac = if search_frac.is_nan() { 1.0 } else { search_frac.clamp(0.0, 1.0) };
        let n_search = (((tasks.len() as f64) * frac).round() as usize).max(1).min(tasks.len());
        let mut idx: Vec<usize> = (0..tasks.len()).collect();
        // xorshift-ish deterministic shuffle from seed
        let mut state = seed;
        for i in (1..idx.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state as usize) % (i + 1);
            idx.swap(i, j);
        }
        let search: Vec<Task> = idx[..n_search].iter().map(|&i| tasks[i].clone()).collect();
        let holdout: Vec<Task> = idx[n_search..].iter().map(|&i| tasks[i].clone()).collect();
        (search, holdout)
    }

    /// Grade a task's run on what it left in `workspace`, with the pack's judge.
    pub fn grade(&self, task: &Task, workspace: &Path) -> Vec<Grade> {
        self.grade_with(task, workspace, &GradeOptions::default())
    }

    /// The judge grading uses: `opts.judge` (`[evals.judge]`), else the pack's
    /// `[pack.judge]`, else `opts.default_judge` (`[deciders.judge]`/`[deciders.default]`),
    /// else, with `opts.auto`, one hlock finds ([`crate::graders::auto_judge`]).
    pub fn judge_for(&self, opts: &GradeOptions) -> Option<JudgeConfig> {
        opts.judge
            .clone()
            .filter(JudgeConfig::is_set)
            .or_else(|| self.judge.clone())
            .or_else(|| opts.default_judge.clone().filter(JudgeConfig::is_set))
            .or_else(|| if opts.auto { crate::graders::auto_judge() } else { None })
    }

    /// Grade with options, using [`judge_for`](Self::judge_for).
    pub fn grade_with(&self, task: &Task, workspace: &Path, opts: &GradeOptions) -> Vec<Grade> {
        let opts = GradeOptions { judge: self.judge_for(opts), ..opts.clone() };
        crate::graders::grade(task, workspace, &opts)
    }

    /// Grade a copy of `dir` as if it were the workspace a run of task `id` left; with
    /// no `dir`, the task's fixtures plus its solution. Copies, because `tests`
    /// graders write into the workspace.
    pub fn grade_dir(&self, id: &str, dir: Option<&Path>, opts: &GradeOptions) -> Result<Vec<Grade>> {
        let task = self.tasks.iter().find(|t| t.id == id).ok_or_else(|| {
            Error::EvalPack(format!("no task {id:?} in {} (tasks: {})", self.path.display(), self.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", ")))
        })?;
        let sources: Vec<PathBuf> = match dir {
            Some(d) if d.is_dir() => vec![d.to_path_buf()],
            Some(d) => return Err(Error::EvalPack(format!("{} isn't a directory", d.display()))),
            None => {
                let sol = task.solution.as_ref().map(|s| self.path.join(s)).filter(|p| p.is_dir()).ok_or_else(|| {
                    Error::EvalPack(format!("task {id} has no solution directory; name a directory to grade"))
                })?;
                task.fixtures.iter().map(|f| self.path.join(f)).filter(|p| p.is_dir()).chain([sol]).collect()
            }
        };
        // a fresh directory of our own, removed (with only what's in it) when it drops
        let scratch = crate::graders::ScratchDir::new("grade")?;
        let ws = task_workspace(&scratch.0, id)?;
        fill_workspace(&ws, sources)?;
        Ok(self.grade_with(task, &ws, opts))
    }

    /// Grade task `t` in a fresh temporary workspace: its fixtures, plus `extra` (a
    /// solution) when given.
    fn grade_fresh(&self, t: &Task, extra: Option<&Path>, opts: &GradeOptions) -> Result<Vec<Grade>> {
        let scratch = crate::graders::ScratchDir::new("fresh")?;
        let ws = task_workspace(&scratch.0, &t.id)?;
        fill_workspace(&ws, t.fixtures.iter().map(|f| self.path.join(f)).chain(extra.map(Path::to_path_buf)).filter(|p| p.is_dir()))?;
        Ok(self.grade_with(t, &ws, opts))
    }

    /// Judge each task's solution and fixtures-only run with judges `a` and `b` (a
    /// decision model and an LLM, usually), and compare their verdicts per criterion.
    pub fn calibrate(&self, a: &JudgeConfig, b: &JudgeConfig) -> Vec<CalibrationRow> {
        let mut rows = Vec::new();
        let judge_only = |j: &JudgeConfig| GradeOptions { judge: Some(j.clone()), ..Default::default() };
        let base = |label: &str| label.rsplit_once(" (").map(|(l, _)| l.to_string()).unwrap_or_else(|| label.to_string());
        for t in &self.tasks {
            if !t.grader_list().iter().any(|g| matches!(g, Ok(Grader::Judge { .. }))) {
                continue;
            }
            let sol = t.solution.as_ref().map(|s| self.path.join(s)).filter(|p| p.is_dir());
            let runs: Vec<(&str, Option<&Path>)> = sol.as_deref().map(|p| ("solution", Some(p))).into_iter().chain([("fixtures", None)]).collect();
            for (run, extra) in runs {
                let (ga, gb) = match (self.grade_fresh(t, extra, &judge_only(a)), self.grade_fresh(t, extra, &judge_only(b))) {
                    (Ok(ga), Ok(gb)) => (ga, gb),
                    (Err(e), _) | (_, Err(e)) => {
                        rows.push(CalibrationRow { task: t.id.clone(), run: run.into(), grader: "(workspace)".into(), criterion: "(error)".into(), a: 0.0, b: 0.0, agree: false, note: e.to_string() });
                        continue;
                    }
                };
                let ga: Vec<Grade> = ga.into_iter().filter(|g| g.grader.starts_with("judge:")).collect();
                let gb: Vec<Grade> = gb.into_iter().filter(|g| g.grader.starts_with("judge:")).collect();
                for (x, y) in ga.iter().zip(&gb) {
                    let grader = base(&x.grader);
                    let failed = |g: &Grade| g.parts.is_empty() && !g.detail.is_empty() && g.score == 0.0 && !g.success;
                    if failed(x) || failed(y) {
                        let which = if failed(x) { x } else { y };
                        rows.push(CalibrationRow { task: t.id.clone(), run: run.into(), grader: grader.clone(), criterion: "(error)".into(), a: x.score, b: y.score, agree: false, note: which.detail.clone() });
                        continue;
                    }
                    for (px, py) in x.parts.iter().zip(&y.parts) {
                        let level = px.label.starts_with("level") || py.label == "level";
                        let agree = if level { px.value.round() == py.value.round() } else { (px.value >= 0.5) == (py.value >= 0.5) };
                        rows.push(CalibrationRow { task: t.id.clone(), run: run.into(), grader: grader.clone(), criterion: px.label.clone(), a: px.value, b: py.value, agree, note: String::new() });
                    }
                    rows.push(CalibrationRow { task: t.id.clone(), run: run.into(), grader, criterion: "(passes)".into(), a: x.score, b: y.score, agree: x.success == y.success, note: String::new() });
                }
            }
        }
        rows
    }

    /// Try each task's graders without an agent: on its fixtures alone they should fail
    /// (or the task passes with no work), and on its fixtures plus its `solution` they
    /// should pass. Runs `tests` commands in temporary directories, and the judges
    /// `opts.judges` allows (decision models, for `hlock evals check --run`).
    pub fn check_graders(&self, opts: &GradeOptions) -> Vec<PackIssue> {
        let mut issues = Vec::new();
        let judge = self.judge_for(opts);
        let judges_run = match opts.judges {
            Judges::All => judge.is_some(),
            Judges::Deciders => judge.as_ref().is_some_and(JudgeConfig::is_decider),
            Judges::None => false,
        };
        let opts = GradeOptions { judge: judge.clone(), auto: false, trace: None, ..opts.clone() };
        let root = match crate::graders::ScratchDir::new("check") {
            Ok(d) => d,
            Err(e) => return vec![PackIssue::error(None, format!("can't make a directory to try the graders in: {e}"))],
        };
        for t in &self.tasks {
            let id = Some(t.id.as_str()).filter(|s| !s.is_empty());
            let graders = t.grader_list();
            if graders.iter().any(|g| g.is_err()) || (graders.is_empty() && t.expected_artifacts().is_empty()) || !valid_task_id(&t.id) {
                continue; // `check` reports these
            }
            let Ok(ws) = task_workspace(&root.0, &t.id) else { continue };
            // the task's workspace under our own directory, emptied and filled again
            let fresh = |extra: Option<&Path>| -> Result<()> {
                if std::fs::symlink_metadata(&ws).is_ok() {
                    std::fs::remove_dir_all(&ws)?;
                }
                fill_workspace(&ws, t.fixtures.iter().map(|f| self.path.join(f)).chain(extra.map(Path::to_path_buf)).filter(|p| p.is_dir()))
            };
            // skipped judges might fail where the rest pass, so a judged task can only be
            // said to pass without any work when its judges ran
            let judged = graders.iter().any(|g| matches!(g, Ok(Grader::Judge { .. })));
            if !graders.is_empty() && (!judged || judges_run) {
                if let Err(e) = fresh(None) {
                    issues.push(PackIssue::error(id, format!("can't set up its fixtures: {e}")));
                    continue;
                }
                let grades = self.grade_with(t, &ws, &opts);
                if !grades.is_empty() && grades.iter().all(|g| g.success) {
                    issues.push(PackIssue::warn(id, "its graders pass on the fixtures alone, so it passes without any work".into()));
                }
            }
            match t.solution.as_ref().map(|s| self.path.join(s)).filter(|p| p.is_dir()) {
                Some(sol) => {
                    if let Err(e) = fresh(Some(&sol)) {
                        issues.push(PackIssue::error(id, format!("can't set up its solution: {e}")));
                        continue;
                    }
                    let grades = self.grade_with(t, &ws, &opts);
                    for g in grades.iter().filter(|g| g.success && g.uncertain) {
                        let weakest = g.parts.iter().min_by(|a, b| (a.value - 0.5).abs().total_cmp(&(b.value - 0.5).abs()));
                        let which = weakest.map(|p| format!(": {:?} P={:.2}", p.label, p.value)).unwrap_or_default();
                        issues.push(PackIssue::warn(id, format!("{} is uncertain on its solution{which}", g.grader)));
                    }
                    for g in grades.iter().filter(|g| !g.success) {
                        let lines: Vec<&str> = g.detail.lines().filter(|l| !l.trim().is_empty()).collect();
                        let detail = match lines.as_slice() {
                            [] => String::new(),
                            [one] => one.to_string(),
                            [first, .., last] => format!("{first}: {}", last.trim()),
                        };
                        issues.push(PackIssue::error(id, format!("its solution fails {}: {detail}", g.grader)));
                    }
                }
                None if t.solution.is_none() => {
                    issues.push(PackIssue::warn(id, "no solution, so its graders weren't tried on a known-good result".into()));
                }
                None => {} // `check` reports a missing solution directory
            }
        }
        drop(root);
        issues
    }
}
