//! The catalog of known OAuth connector MCP servers, a `connector:NAME` dependency source.
//!
//! A Claude Code connector (Slack, Linear, …) is an HTTP MCP server the user authorizes
//! with OAuth against their account. The authorization is per-client and never carried,
//! but its *definition* — the server's MCP URL and that it needs OAuth — is portable:
//! this catalog maps a connector id to its MCP URL so hlock can materialize the server
//! (`auth = "oauth"`) in another harness, where the user re-authorizes once.
//!
//! The catalog is curated and intended to grow; it only names *known* connectors, so a
//! server it can't name still works as a plain remote MCP URL. Ids match the effort's
//! Python-side inference catalog (`hlock/connectors.py`) so `hlock connectors` output can
//! feed `hlock migrate --connectors …`.

use crate::error::{Error, Result};

/// The MCP URL for a known connector id, if any.
pub fn url(id: &str) -> Option<&'static str> {
    match id {
        "slack" => Some("https://mcp.slack.com/mcp"),
        "linear" => Some("https://mcp.linear.app/mcp"),
        "notion" => Some("https://mcp.notion.com/mcp"),
        "confluence" => Some("https://mcp.atlassian.com/atlassian-mcp/confluence"),
        "jira" => Some("https://mcp.atlassian.com/atlassian-mcp/jira"),
        "gmail" => Some("https://mcp.gmail.com/api/v1/serve"),
        "google-docs" => Some("https://mcp.googleapis.com/docs/"),
        "google-sheets" => Some("https://mcp.googleapis.com/sheets/"),
        "google-drive" => Some("https://mcp.googleapis.com/drive/"),
        "google-calendar" => Some("https://mcp.googleapis.com/calendar/"),
        "sentry" => Some("https://mcp.sentry.dev/mcp"),
        "rollbar" => Some("https://mcp.rollbar.com/mcp"),
        "github" => Some("https://api.githubcopilot.com/mcp/"),
        _ => None,
    }
}

/// The connector ids in the catalog, sorted.
pub fn ids() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = [
        "slack", "linear", "notion", "confluence", "jira", "gmail", "google-docs",
        "google-sheets", "google-drive", "google-calendar", "sentry", "rollbar", "github",
    ]
    .into_iter()
    .collect();
    v.sort();
    v
}

/// Whether `id` is a connector the catalog knows.
pub fn known(id: &str) -> bool {
    url(id).is_some()
}

/// Parse and validate `connector:NAME` into its id, naming the known connectors otherwise.
pub fn parse(spec: &str) -> Result<String> {
    let id = spec.strip_prefix("connector:").ok_or_else(|| crate::error::Error::Dependency("not a connector: source".into()))?.trim();
    if !crate::fsutil::plain_token(id) {
        return Err(Error::Dependency(format!("{id:?} isn't a connector name (letters, digits and ._- only)")));
    }
    if url(id).is_none() {
        return Err(Error::Dependency(format!("{id:?} isn't a known connector ({}); pick one, or add the server by its MCP URL", ids().join(", "))));
    }
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_connectors_have_urls() {
        for id in ids() {
            assert!(url(id).is_some_and(|u| u.starts_with("https://")), "{id}");
        }
        assert_eq!(url("slack"), Some("https://mcp.slack.com/mcp"));
    }

    #[test]
    fn parse_validates() {
        assert_eq!(parse("connector:slack").unwrap(), "slack");
        assert!(parse("connector:nope").unwrap_err().to_string().contains("known connector"));
        assert!(parse("connector:a b").is_err());
        assert!(parse("slack").is_err());
    }
}
