//! The parts of a harness besides its models and tools, as harness optimizers such as
//! Harness² name them: the system prompt, guardrails, memory, tool guidance, subagents,
//! skills, scripts and middleware.
//!
//! The system prompt is the harness's instruction file (`[[instructions]]`). Guardrails,
//! memory and tool guidance are always-on text too, so they're instruction files with a
//! `role`: a file of their own in a harness that reads a rules directory (Claude Code's
//! `.claude/rules/`), else a headed section of the instruction file (`## Guardrails`), the
//! way Harness² composes them for OpenCode and Codex. "Memory" here is authored, durable
//! text that ships with the agent, never the harness's own memory store, which a lock
//! doesn't capture (`specs/memory.md`).
//!
//! Subagents, scripts and middleware are files the harness loads on their own
//! ([`crate::lockfile::FileComponent`]): `[[subagent]]` from the directories a profile's
//! `subagents` names, `[[script]]` from [`SCRIPTS_DIR`], and `[[middleware]]` from a
//! profile's `middleware` (OpenCode plugin files, Codex's hooks file).

/// The roles an instruction file can have besides the system prompt.
pub const ROLES: &[&str] = &["guardrails", "memory", "tool-guidance"];

/// Where a project keeps the helper scripts its instructions tell the agent to run. No
/// harness has a place of its own for them, so hlock uses one directory for all of them.
pub const SCRIPTS_DIR: &str = ".agents/scripts";

/// The heading a role's section starts with, in a harness's single instruction file.
pub fn heading(role: &str) -> &'static str {
    match role {
        "guardrails" => "## Guardrails",
        "memory" => "## Memory",
        _ => "## Tool guidance",
    }
}

fn role_of_heading(line: &str) -> Option<&'static str> {
    ROLES.iter().copied().find(|r| line.trim_end() == heading(r))
}

/// The role a rules file plays, by its name: `guardrails.md`, `memory.md` (or Harness²'s
/// `LongTermMEMORY.md`), `tool-guidance.md` (or `tool_guidance.md`). Any other file is a plain
/// instruction file.
pub fn role_of_file(name: &str) -> Option<&'static str> {
    let file = name.rsplit('/').next().unwrap_or(name);
    let stem = file.split('.').next().unwrap_or(file).to_ascii_lowercase().replace('_', "-");
    match stem.as_str() {
        "guardrails" => Some("guardrails"),
        "memory" | "longtermmemory" | "long-term-memory" => Some("memory"),
        "tool-guidance" => Some("tool-guidance"),
        _ => None,
    }
}

/// Where a role's file goes in a harness that reads a rules directory, or `None` when the
/// role is a section of the instruction file.
pub fn role_path(profile: &crate::profiles::HarnessProfile, role: &str) -> Option<String> {
    profile.rules.as_ref().map(|r| format!("{}/{role}{}", r.dir.trim_end_matches('/'), r.ext))
}

/// Split an instruction file into its own text and its role sections. A section runs from
/// its heading to the next role heading or the end of the file, so `compose` writes them
/// last.
pub fn split_sections(text: &str) -> (String, Vec<(String, String)>) {
    let mut base = String::new();
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        if let Some(role) = role_of_heading(line) {
            sections.push((role.to_string(), String::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(line);
        } else {
            base.push_str(line);
        }
    }
    let base = base.trim_end().to_string();
    let sections = sections.into_iter().map(|(r, b)| (r, b.trim().to_string())).filter(|(_, b)| !b.is_empty()).collect();
    (base, sections)
}

/// The roles a file holds as sections.
pub fn sections_in(text: &str) -> Vec<String> {
    split_sections(text).1.into_iter().map(|(r, _)| r).collect()
}

/// An instruction file from its own text and role sections, in [`ROLES`] order.
pub fn compose(base: &str, sections: &[(String, String)]) -> String {
    let mut out = base.trim_end().to_string();
    for role in ROLES {
        for (_, body) in sections.iter().filter(|(r, b)| r == role && !b.trim().is_empty()) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(heading(role));
            out.push_str("\n\n");
            out.push_str(body.trim());
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_by_file_name() {
        assert_eq!(role_of_file(".claude/rules/guardrails.md"), Some("guardrails"));
        assert_eq!(role_of_file("LongTermMEMORY.md"), Some("memory"));
        assert_eq!(role_of_file("tool_guidance.md"), Some("tool-guidance"));
        assert_eq!(role_of_file(".github/instructions/memory.instructions.md"), Some("memory"));
        assert_eq!(role_of_file("style.md"), None);
    }

    #[test]
    fn sections_round_trip() {
        let text = compose("Be brief.", &[("memory".into(), "The API is v2.".into()), ("guardrails".into(), "Never push to main.".into())]);
        assert_eq!(text, "Be brief.\n\n## Guardrails\n\nNever push to main.\n\n## Memory\n\nThe API is v2.\n");
        let (base, sections) = split_sections(&text);
        assert_eq!(base, "Be brief.");
        assert_eq!(sections, vec![("guardrails".to_string(), "Never push to main.".to_string()), ("memory".to_string(), "The API is v2.".to_string())]);
        assert_eq!(sections_in(&text), ["guardrails", "memory"]);
        assert_eq!(sections_in("# Title\n\n## Usage\n"), Vec::<String>::new());
    }
}
