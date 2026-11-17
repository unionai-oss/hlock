//! What a model accepts in a request: the request parameters some models reject.
//!
//! Some models fail a request that sets a sampling parameter (Claude Fable 5.2 returns
//! a 400 for `temperature`, `top_p` or `top_k`). The table of such models is data:
//! built-in tables ship inside the crate (`profiles/models/*.toml`), and users add or
//! replace them with `models/*.toml` in a profile directory
//! ([`crate::profiles::profile_dirs`]), so a new model needs no code change. The worker
//! leaves a rejected parameter out of the request, and `hlock tune` doesn't sweep it.

use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::profiles::{profile_dirs, project_profile_dir, toml_files, Ctx};

const BUILTIN_MODELS: &[(&str, &str)] = &[("anthropic", include_str!("../profiles/models/anthropic.toml"))];

/// Models that share what they reject.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRule {
    /// Model ids, matched by [`matches`].
    pub ids: Vec<String>,
    /// Request parameters these models reject (`temperature`, `top_p`, …).
    pub rejects: Vec<String>,
    /// Why, for the user.
    pub why: Option<String>,
    /// Where it's documented.
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ModelFile {
    model: Vec<ModelRule>,
}

/// The model tables, by name (a file's stem: `anthropic`).
#[derive(Debug, Clone, Default)]
pub struct ModelTable {
    tables: Vec<(String, Vec<ModelRule>)>,
    /// Files skipped, and why.
    pub notes: Vec<String>,
}

fn parse(text: &str, origin: &str) -> Result<Vec<ModelRule>> {
    let f: ModelFile = toml::from_str(text).map_err(|e| Error::Profile(format!("{origin}: {e}")))?;
    Ok(f.model)
}

/// `.` and `-` are the same in a model id (`claude-fable-5.2` is `claude-fable-5-2`), and
/// case doesn't matter.
fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('.', "-")
}

/// Whether `model` names the model `id`: `id` appears in it with no letter or digit
/// on either side. So `claude-fable-5-2` matches `claude-fable-5-2`,
/// `anthropic:claude-fable-5-2`, `openrouter:anthropic/claude-fable-5.2`, a dated
/// `claude-fable-5-2-20261001` and `us.anthropic.claude-fable-5-2-v1:0`, but not
/// `claude-fable-5-20`.
pub fn matches(id: &str, model: &str) -> bool {
    let (id, model) = (normalize(id), normalize(model));
    if id.is_empty() {
        return false;
    }
    let bytes = model.as_bytes();
    model.match_indices(&id).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + id.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric()) && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

impl ModelTable {
    /// Only the tables compiled into this build.
    pub fn builtin() -> ModelTable {
        let tables = BUILTIN_MODELS
            .iter()
            .map(|(name, text)| (name.to_string(), parse(text, "builtin").unwrap_or_else(|e| panic!("builtin model table {name}: {e}"))))
            .collect();
        ModelTable { tables, notes: Vec::new() }
    }

    /// Built-ins plus `models/*.toml` in every profile directory. A file replaces the
    /// table of the same name, except in the project's own directory, which may only
    /// add tables (as with its harness profiles).
    pub fn load(ctx: &Ctx) -> Result<ModelTable> {
        let mut t = ModelTable::builtin();
        let project = project_profile_dir(ctx);
        for dir in profile_dirs(ctx) {
            t.extend_from_dir(&dir, dir == project)?;
        }
        Ok(t)
    }

    fn extend_from_dir(&mut self, dir: &Path, project: bool) -> Result<()> {
        for path in toml_files(&dir.join("models")) {
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if project && self.tables.iter().any(|(n, _)| *n == name) {
                self.notes.push(format!("{}: skipped; a project can't replace the model table {name:?} (override it in ~/.config/hlock/profiles/models)", path.display()));
                continue;
            }
            let rules = parse(&std::fs::read_to_string(&path)?, &path.to_string_lossy())?;
            self.tables.retain(|(n, _)| *n != name);
            self.tables.push((name, rules));
        }
        Ok(())
    }

    fn rules(&self, model: &str) -> Vec<&ModelRule> {
        self.tables.iter().flat_map(|(_, r)| r).filter(|r| r.ids.iter().any(|id| matches(id, model))).collect()
    }

    /// The request parameters `model` (an id or a `provider:model` spec) rejects, sorted.
    pub fn rejected(&self, model: &str) -> Vec<String> {
        let mut out: Vec<String> = self.rules(model).into_iter().flat_map(|r| r.rejects.iter().cloned()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// The rule that has `model` reject `param`, if one does.
    pub fn rejects(&self, model: &str, param: &str) -> Option<&ModelRule> {
        self.rules(model).into_iter().find(|r| r.rejects.iter().any(|p| p == param))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_whole_names_in_any_spelling() {
        for m in ["claude-fable-5-2", "anthropic:claude-fable-5-2", "openrouter:anthropic/claude-fable-5.2", "Claude-Fable-5.2", "claude-fable-5-2-20261001", "us.anthropic.claude-fable-5-2-v1:0", "claude-fable-5-2[1m]"] {
            assert!(matches("claude-fable-5-2", m), "{m}");
        }
        for m in ["claude-fable-5-20", "claude-fable-5", "xclaude-fable-5-2", "claude-sonnet-5-5", ""] {
            assert!(!matches("claude-fable-5-2", m), "{m}");
        }
        assert!(!matches("", "anything"));
    }

    #[test]
    fn builtin_table_has_fable_and_mythos_reject_sampling_params() {
        let t = ModelTable::builtin();
        for m in ["anthropic:claude-fable-5-2", "anthropic:claude-mythos-5-2", "openrouter:anthropic/claude-mythos-5.2"] {
            assert_eq!(t.rejected(m), ["temperature", "top_k", "top_p"], "{m}");
            assert!(t.rejects(m, "temperature").and_then(|r| r.source.as_deref()).is_some_and(|s| s.contains("migration-guide")));
        }
        assert!(t.rejected("anthropic:claude-sonnet-5-5").is_empty());
        assert!(t.rejects("reference:qwen-7b", "temperature").is_none());
    }

    #[test]
    fn profile_dirs_add_and_replace_tables_but_a_project_only_adds() {
        let tmp = std::env::temp_dir().join(format!("hlock-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let user = tmp.join("user");
        std::fs::create_dir_all(user.join("models")).unwrap();
        std::fs::write(user.join("models/anthropic.toml"), "[[model]]\nids = [\"claude-fable-5-2\"]\nrejects = [\"top_k\"]\n").unwrap();
        std::fs::write(user.join("models/acme.toml"), "[[model]]\nids = [\"acme-1\"]\nrejects = [\"temperature\"]\n").unwrap();
        let mut t = ModelTable::builtin();
        t.extend_from_dir(&user, false).unwrap();
        assert_eq!(t.rejected("anthropic:claude-fable-5-2"), ["top_k"]); // replaced
        assert!(t.rejected("anthropic:claude-mythos-5-2").is_empty());
        assert_eq!(t.rejected("openai:acme-1"), ["temperature"]); // added

        let project = tmp.join("project");
        std::fs::create_dir_all(project.join("models")).unwrap();
        std::fs::write(project.join("models/anthropic.toml"), "[[model]]\nids = [\"x\"]\n").unwrap();
        std::fs::write(project.join("models/local.toml"), "[[model]]\nids = [\"mine\"]\nrejects = [\"top_p\"]\n").unwrap();
        let mut t = ModelTable::builtin();
        t.extend_from_dir(&project, true).unwrap();
        assert_eq!(t.rejected("anthropic:claude-fable-5-2"), ["temperature", "top_k", "top_p"]);
        assert_eq!(t.rejected("mine"), ["top_p"]);
        assert!(t.notes.iter().any(|n| n.contains("can't replace the model table \"anthropic\"")), "{:?}", t.notes);

        std::fs::write(project.join("models/local.toml"), "[[model]]\nid = \"typo\"\n").unwrap();
        assert!(ModelTable::builtin().extend_from_dir(&project, true).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
