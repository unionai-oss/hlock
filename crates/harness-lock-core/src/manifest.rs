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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(default)]
pub struct Budget {
    pub max_spend_usd: Option<f64>,
    pub max_trials: Option<usize>,
    pub strategy: String,
    pub random_seed: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_spend_usd: None,
            max_trials: None,
            strategy: "auto".into(),
            random_seed: 0,
        }
    }
}

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
        _ => Ok(vec![]),
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
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
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
        let mut m: Manifest = toml::from_str(&text)
            .map_err(|e| Error::Manifest(format!("invalid harness.toml: {e}")))?;
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
        toml::from_str(text).map_err(|e| Error::Manifest(format!("invalid harness.toml: {e}")))
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

    /// Hash of the manifest (of the source file when loaded, else of its TOML).
    pub fn manifest_hash(&self) -> String {
        if let Some(p) = &self.source_path {
            if let Ok(bytes) = std::fs::read(p) {
                return sha256_bytes(&bytes);
            }
        }
        match toml::to_string(self) {
            Ok(s) => sha256_bytes(s.as_bytes()),
            Err(_) => String::new(),
        }
    }
}
