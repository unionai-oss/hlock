//! The manifest: `harness.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Error, Result};
use crate::hash::sha256_bytes;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct Objective {
    pub primary: String,
    pub constraints: BTreeMap<String, String>,
    pub select: String,
}

/// `[budget]`. Keys are kebab-case like the rest of the manifest; the snake_case
/// spellings older docs and `hlock init` wrote still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(default)]
pub struct Budget {
    #[serde(alias = "max_spend_usd")]
    pub max_spend_usd: Option<f64>,
    #[serde(alias = "max_trials")]
    pub max_trials: Option<usize>,
    pub strategy: String,
    #[serde(alias = "random_seed")]
    pub random_seed: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_spend_usd: None,
            max_trials: None,
            strategy: "grid".into(),
            random_seed: 0,
        }
    }
}

/// The search strategies, objectives and select rules `resolve` supports.
pub const STRATEGIES: &[&str] = &["grid"];
pub const PRIMARIES: &[&str] = &["success-rate"];
pub const SELECTS: &[&str] = &["min-cost", "max-quality"];

/// The keys of the manifest's fixed-shape tables; anything else is reported by
/// [`Manifest::problems`]. Tables holding names (`[skills]`, `[harnesses]`, …) aren't
/// checked, and neither are the reserved `[context]` and `[enforce]`.
const TOP_KEYS: &[&str] = &["project", "evals", "tasks", "candidates", "tools", "skills", "instructions", "mcp", "plugins", "packages", "context", "objective", "budget", "enforce", "backend", "target", "harnesses", "deciders"];
const BUDGET_KEYS: &[&str] = &["max-spend-usd", "max-trials", "strategy", "random-seed", "max_spend_usd", "max_trials", "random_seed"];
const OBJECTIVE_KEYS: &[&str] = &["primary", "constraints", "select"];
const EVALS_KEYS: &[&str] = &["packs", "seeds", "splits", "judge"];
const SPLITS_KEYS: &[&str] = &["search"];
const CANDIDATES_KEYS: &[&str] = &["llm", "decision", "harness", "policy"];
const BACKEND_KEYS: &[&str] = &["target"];
const TARGET_KEYS: &[&str] = &["harness", "also"];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Candidates {
    #[serde(default)]
    pub llm: BTreeMap<String, LLMCandidate>,
    #[serde(default)]
    pub decision: Vec<String>,
    #[serde(default, deserialize_with = "de_harness")]
    pub harness: Vec<HarnessCandidate>,
    #[serde(default)]
    pub policy: BTreeMap<String, Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LLMCandidate {
    /// Manifest candidates may spell this `any_of` (underscore) or `any-of`.
    #[serde(alias = "any-of")]
    pub any_of: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessCandidate {
    pub name: String,
    #[serde(default)]
    pub pattern: Vec<String>,
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl Default for HarnessCandidate {
    fn default() -> Self {
        HarnessCandidate {
            name: String::new(),
            pattern: Vec::new(),
            config: BTreeMap::new(),
        }
    }
}

/// Accept both `harness = ["reference"]` and `[[candidates.harness]]` tables.
fn de_harness<'de, D>(d: D) -> std::result::Result<Vec<HarnessCandidate>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(|item| match item {
                serde_json::Value::String(s) => Ok(HarnessCandidate {
                    name: s.clone(),
                    ..Default::default()
                }),
                other => serde_json::from_value(other.clone()).map_err(D::Error::custom),
            })
            .collect(),
        serde_json::Value::Null => Ok(vec![]),
        other => Err(D::Error::custom(format!("`harness` must be a list of names or [[candidates.harness]] tables, not {other}"))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Manifest {
    #[serde(default)]
    pub project: BTreeMap<String, serde_json::Value>,
    /// `[evals]`: the eval packs to measure against. `[tasks]` is the old name.
    #[serde(default, alias = "tasks")]
    pub evals: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub candidates: Candidates,
    #[serde(default)]
    pub tools: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub skills: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub instructions: BTreeMap<String, serde_json::Value>,
    /// MCP servers the project declares (`[mcp]`): a source each, or a table with one.
    #[serde(default)]
    pub mcp: BTreeMap<String, serde_json::Value>,
    /// Harness plugins the project declares (`[plugins]`, `plugin:MARKETPLACE/NAME`).
    #[serde(default)]
    pub plugins: BTreeMap<String, serde_json::Value>,
    /// APM packages (`[packages]`, `apm:OWNER/REPO`): each brings its own skills,
    /// instructions and MCP servers, and the packages it depends on.
    #[serde(default)]
    pub packages: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub context: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub objective: Objective,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default, rename = "enforce")]
    pub enforce: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub backend: BTreeMap<String, serde_json::Value>,
    /// The harness this lock is for (`[target] harness = "claude-code"`).
    #[serde(default)]
    pub target: BTreeMap<String, serde_json::Value>,
    /// Project-defined harnesses (`[harnesses.<id>]`): framework agents and custom scripts.
    #[serde(default)]
    pub harnesses: BTreeMap<String, serde_json::Value>,
    /// Decision models by what they decide (`[deciders.<use>]`).
    #[serde(default)]
    pub deciders: BTreeMap<String, serde_json::Value>,
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// The text this manifest was parsed from, which [`Manifest::manifest_hash`] hashes.
    #[serde(skip)]
    pub source_text: Option<String>,
}

/// The nearest `harness.toml` at or above `start`, like Cargo finds `Cargo.toml`.
pub fn find_manifest(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(crate::MANIFEST_PATH);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Manifest(format!("cannot read {}: {e}", path.display())))?;
        let mut m = Manifest::from_str(&text)?;
        // a bad [deciders] table is an error here, not a silent fallback later
        m.deciders()?;
        m.source_path = Some(path.to_path_buf());
        Ok(m)
    }

    /// The project directory: where the manifest lives (else the working directory).
    /// Relative paths in the manifest (eval packs, instructions) resolve against it.
    pub fn root(&self) -> PathBuf {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match self.source_path.as_ref().and_then(|p| p.parent()) {
            Some(d) if d.as_os_str().is_empty() => cwd,
            Some(d) if d.is_absolute() => d.to_path_buf(),
            Some(d) => cwd.join(d),
            None => cwd,
        }
    }

    /// Parse from an in-memory TOML string.
    pub fn from_str(text: &str) -> Result<Manifest> {
        let mut m: Manifest = toml::from_str(text).map_err(|e| Error::Manifest(format!("invalid harness.toml: {e}")))?;
        m.source_text = Some(text.to_string());
        Ok(m)
    }

    /// Problems that would make `resolve` or `tune` quietly do something other than
    /// what the manifest says: a key a fixed-shape table doesn't know (naming the closest
    /// one), and a `strategy`, `primary` or `select` that isn't supported.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(doc) = self.source_text.as_deref().and_then(|t| t.parse::<toml::Table>().ok()) {
            let mut report = |table: Option<&toml::Table>, known: &[&str], name: &str| {
                for key in table.into_iter().flat_map(|t| t.keys()).filter(|k| !known.contains(&k.as_str())) {
                    let hint = crate::tasks::closest(key, known).map(|k| format!(" (did you mean `{k}`?)")).unwrap_or_default();
                    out.push(format!("unknown key `{key}` in {name}{hint}"));
                }
            };
            let table = |path: &[&str]| path.iter().try_fold(&doc, |t, k| t.get(*k).and_then(toml::Value::as_table));
            report(Some(&doc), TOP_KEYS, "harness.toml");
            report(table(&["budget"]), BUDGET_KEYS, "[budget]");
            report(table(&["objective"]), OBJECTIVE_KEYS, "[objective]");
            report(table(&["evals"]).or(table(&["tasks"])), EVALS_KEYS, "[evals]");
            report(table(&["evals", "splits"]), SPLITS_KEYS, "[evals.splits]");
            report(table(&["candidates"]), CANDIDATES_KEYS, "[candidates]");
            report(table(&["backend"]), BACKEND_KEYS, "[backend]");
            report(table(&["target"]), TARGET_KEYS, "[target]");
        }
        let mut supported = |key: &str, value: &str, allowed: &[&str]| {
            if !value.is_empty() && !allowed.contains(&value) {
                out.push(format!("{key} = {value:?} isn't supported (use {})", allowed.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>().join(" or ")));
            }
        };
        supported("[budget] strategy", &self.budget.strategy, STRATEGIES);
        supported("[objective] primary", &self.objective.primary, PRIMARIES);
        supported("[objective] select", &self.objective.select, SELECTS);
        out
    }

    /// [`Manifest::problems`] as an error, for the commands that search.
    pub fn check_search(&self) -> Result<()> {
        let p = self.problems();
        if p.is_empty() {
            return Ok(());
        }
        Err(Error::Manifest(format!("harness.toml: {}", p.join("; "))))
    }

    pub fn name(&self) -> String {
        self.project
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "agent".to_string())
    }

    pub fn eval_packs(&self) -> Vec<String> {
        self.evals
            .get("packs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[deprecated(note = "renamed to `eval_packs`")]
    pub fn task_packs(&self) -> Vec<String> {
        self.eval_packs()
    }

    pub fn seeds(&self) -> u64 {
        self.evals
            .get("seeds")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
    }

    /// `[evals.judge]`: the model that scores `judge` graders, over any pack's own.
    /// An invalid table reads as unset; [`Manifest::judge_checked`] reports it.
    pub fn judge(&self) -> Option<crate::graders::JudgeConfig> {
        self.judge_checked().ok().flatten()
    }

    /// [`Manifest::judge`], with an error for an invalid `[evals.judge]` table.
    pub fn judge_checked(&self) -> Result<Option<crate::graders::JudgeConfig>> {
        match self.evals.get("judge") {
            None => Ok(None),
            Some(v) => {
                let j: crate::graders::JudgeConfig = serde_json::from_value(v.clone()).map_err(|e| Error::Manifest(format!("[evals.judge]: {e}")))?;
                Ok(j.is_set().then_some(j))
            }
        }
    }

    /// `[deciders.<use>]`, checked, with defaults filled in.
    pub fn deciders(&self) -> Result<BTreeMap<String, crate::decide::DeciderSpec>> {
        self.deciders
            .iter()
            .map(|(use_, v)| {
                let spec: crate::decide::DeciderSpec = serde_json::from_value(v.clone()).map_err(|e| Error::Manifest(format!("[deciders.{use_}]: {e}")))?;
                Ok((use_.clone(), spec.validated(use_).map_err(Error::Manifest)?))
            })
            .collect()
    }

    /// The judge `[deciders.judge]`, else `[deciders.default]`, names: used when
    /// neither `[evals.judge]` nor a pack's `[pack.judge]` is set.
    /// An invalid `[deciders]` table reads as unset; [`Manifest::default_judge_checked`]
    /// reports it.
    pub fn default_judge(&self) -> Option<crate::graders::JudgeConfig> {
        self.default_judge_checked().ok().flatten()
    }

    /// [`Manifest::default_judge`], with an error for an invalid `[deciders]` table. A
    /// `[deciders.judge]` with only `any-of` falls back to `[deciders.default]`'s model.
    pub fn default_judge_checked(&self) -> Result<Option<crate::graders::JudgeConfig>> {
        let specs = self.deciders()?;
        let Some(spec) = ["judge", "default"].iter().filter_map(|u| specs.get(*u)).find(|s| s.model.is_some()) else { return Ok(None) };
        Ok(Some(crate::graders::JudgeConfig {
            model: spec.model.clone().unwrap_or_default(),
            protocol: spec.protocol.clone(),
            base_url: spec.base_url.clone(),
            key_env: spec.key_env.clone(),
            long_context: spec.long_context,
            ..Default::default()
        }))
    }

    pub fn backend_target(&self) -> String {
        self.backend
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "local".to_string())
    }

    /// `[target] harness`, the harness the lock is meant to run in.
    pub fn target_harness(&self) -> Option<String> {
        self.target
            .get("harness")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    pub fn search_frac(&self) -> f64 {
        self.evals
            .get("splits")
            .and_then(|v| v.get("search"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.7)
    }

    /// Hash of the dependency sections only (`[skills]`, `[mcp]`, `[instructions]`,
    /// `[plugins]`, `[packages]`, `[tools]`): what a pinned lock's `manifest-hash`
    /// records, so editing `[candidates]` in the same file doesn't invalidate a pin.
    pub fn deps_hash(&self) -> String {
        crate::hash::content_hash(&serde_json::json!({
            "skills": self.skills,
            "mcp": self.mcp,
            "instructions": self.instructions,
            "plugins": self.plugins,
            "packages": self.packages,
            "tools": self.tools,
        }))
    }

    /// Hash of the manifest: of the text it was parsed from (so it matches what the run
    /// used, whether loaded or parsed), else of its TOML (built in code).
    pub fn manifest_hash(&self) -> String {
        if let Some(text) = &self.source_text {
            return sha256_bytes(text.as_bytes());
        }
        match toml::to_string(self) {
            Ok(s) => sha256_bytes(s.as_bytes()),
            Err(_) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_tables_are_errors_not_defaults() {
        let e = Manifest::from_str("[candidates]\nharness = \"reference\"\n").unwrap_err();
        assert!(e.to_string().contains("must be a list"), "{e}");
        let m = Manifest::from_str("[evals.judge]\nmodel = 3\n").unwrap();
        assert!(m.judge_checked().is_err() && m.judge().is_none());
        let m = Manifest::from_str("[deciders.judge]\nmodel = \"anthropic:x\"\n").unwrap();
        assert!(m.default_judge_checked().is_err() && m.default_judge().is_none());
        // a use with only any-of candidates falls back to [deciders.default]'s model
        let m = Manifest::from_str("[deciders.judge]\nany-of = [\"ollama:kev:4b\"]\n[deciders.default]\nmodel = \"typesafe:jev-1.13.0\"\n").unwrap();
        assert_eq!(m.default_judge_checked().unwrap().unwrap().model, "typesafe:jev-1.13.0");
    }

    #[test]
    fn load_checks_deciders_and_hashes_what_it_parsed() {
        let d = std::env::temp_dir().join(format!("hlock-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("harness.toml");
        std::fs::write(&p, "[deciders.stop]\nmodel = \"anthropic:x\"\n").unwrap();
        assert!(Manifest::load(&p).unwrap_err().to_string().contains("[deciders.stop]"));
        let text = "[project]\nname = \"a\"\n";
        std::fs::write(&p, text).unwrap();
        let m = Manifest::load(&p).unwrap();
        assert_eq!(m.manifest_hash(), Manifest::from_str(text).unwrap().manifest_hash());
        std::fs::write(&p, "[project]\nname = \"changed\"\n").unwrap();
        assert_eq!(m.manifest_hash(), sha256_bytes(text.as_bytes()), "the hash is of what was parsed, not the file now");
        let _ = std::fs::remove_dir_all(&d);
    }
}
