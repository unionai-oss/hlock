//! Eval packs: the tasks a stack is measured against.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    pub fixtures: Option<PathBuf>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub expected: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub graders: Vec<String>,
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

    pub fn load_prompt_text(&mut self) {
        if !self.prompt_text.is_empty() {
            return;
        }
        if let (Some(dir), false) = (&self.pack_dir, self.prompt.is_empty()) {
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
}

/// Marks a spot in a scaffolded eval pack that still needs filling in.
pub const PLACEHOLDER: &str = "TODO(hlock)";

/// Below this many tasks the holdout split is too small to say much.
pub const MIN_TASKS: usize = 10;

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

#[derive(Debug, Clone)]
pub struct Grade {
    pub grader: String,
    pub success: bool,
    pub score: f64,
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
        })
    }

    /// Check that the pack is ready to measure stacks with: required fields set,
    /// prompts and fixtures present, no unfilled [`PLACEHOLDER`]s, and tasks that
    /// can fail.
    pub fn check(&self) -> Vec<PackIssue> {
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
            let artifacts = t.expected_artifacts();
            if artifacts.is_empty() && t.graders.is_empty() {
                issues.push(PackIssue::warn(id, "no expected artifacts or graders, so it always passes".into()));
            }
            if let Some(dir) = fixtures.as_ref().filter(|d| d.is_dir()) {
                for a in artifacts.iter().filter(|a| dir.join(a).exists()) {
                    issues.push(PackIssue::warn(id, format!("expected artifact {a} is already in the fixtures, so it passes without any work")));
                }
            }
            for g in t.graders.iter().filter(|g| !g.starts_with("artifact:") && !g.starts_with("tests:")) {
                issues.push(PackIssue::warn(id, format!("grader {g:?} isn't supported yet; the task is graded on its artifacts")));
            }
        }
        if self.tasks.len() < MIN_TASKS {
            let (_, holdout) = self.split(0.7, 0);
            issues.push(PackIssue::warn(None, format!(
                "{} tasks leaves {} for the holdout split; add tasks until there are at least {MIN_TASKS}",
                self.tasks.len(), holdout.len()
            )));
        }
        issues
    }

    /// Deterministic search/holdout split over task ids.
    pub fn split(&self, search_frac: f64, seed: u64) -> (Vec<Task>, Vec<Task>) {
        let mut tasks: Vec<&Task> = self.tasks.iter().collect();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        let n_search = ((tasks.len() as f64) * search_frac).round().max(1.0) as usize;
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

    /// Grade a task against its workspace. v0.1 uses artifact checks by default.
    pub fn grade(&self, task: &Task, workspace: &Path) -> Vec<Grade> {
        let expected = task.expected_artifacts();
        if task.graders.is_empty() || task.graders.iter().all(|g| g.starts_with("artifact:")) {
            let missing: Vec<String> = expected
                .iter()
                .filter(|a| !workspace.join(a).exists())
                .cloned()
                .collect();
            let success = missing.is_empty();
            return vec![Grade {
                grader: format!("artifact:{}", expected.join(",")),
                success,
                score: if success { 1.0 } else { 0.0 },
            }];
        }
        // For v0.1 the only supported extra grader type is "tests:" which we
        // treat as a failed grade unless an artifact check passes too. This
        // mirrors the reference (deterministic) grader set.
        let expected_ok = expected.iter().all(|a| workspace.join(a).exists());
        vec![Grade {
            grader: "tests:check".into(),
            success: expected_ok,
            score: if expected_ok { 1.0 } else { 0.0 },
        }]
    }
}
