//! `jira-assigned` — Jira issues assigned to *you* that aren't Done yet,
//! surfaced as "go work this ticket" suggestions. Atlassian Cloud REST, no
//! local CLI.
//!
//! Live wiring: `GET https://<site>/rest/api/3/search?jql=<jql>&maxResults=N&
//! fields=summary,status,priority`, HTTP-Basic with the operator's email +
//! the `atlassian/api-token` sops secret. Config params: `site` (the
//! `*.atlassian.net` host, required), `email` (the account email, required),
//! `jql` (override the default query), `secret` (override the token's
//! `category/name`). A missing site/email/token, a non-2xx response, or bad
//! JSON all yield no suggestions (graceful empty, never a panic).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};
use super::util::PriorityScale;

/// Default JQL: everything assigned to me that isn't in the Done category,
/// freshest first. Operators override with the `jql` param.
const DEFAULT_JQL: &str =
    "assignee=currentUser() AND statusCategory != Done ORDER BY updated DESC";

pub struct JiraAssignedSource;

impl SuggestionSource for JiraAssignedSource {
    fn kind(&self) -> SourceKind {
        SourceKind::JiraAssigned
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let Some(site) = cfg.param("site") else {
            return Vec::new();
        };
        let Some(email) = cfg.param("email") else {
            return Vec::new();
        };
        let secret_key = cfg.param("secret").unwrap_or("atlassian/api-token");
        let Some(token) = env.secret(secret_key) else {
            return Vec::new();
        };
        let jql = cfg.param("jql").unwrap_or(DEFAULT_JQL);
        let limit = cfg.max_items.max(1).to_string();
        let mut url = String::from("https://");
        url.push_str(site);
        // /search/jql — current Jira Cloud endpoint (legacy /search removed 2025-05-01).
        url.push_str("/rest/api/3/search/jql?jql=");
        url.push_str(&pct(jql));
        url.push_str("&maxResults=");
        url.push_str(&limit);
        url.push_str("&fields=summary,status,priority");
        let req = HttpReq::new(url)
            .basic_auth(email, token)
            .header("Accept", "application/json");
        let Some(body) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&body, env, cfg.max_items)
    }
}

/// Percent-encode a URL query value without an external dependency: unreserved
use super::util::pct;

/// Parse `/rest/api/3/search` output into suggestions. Pure — the unit the
/// source is tested through. Capped at `max.max(1)`.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(result) = serde_json::from_str::<SearchResult>(json) else {
        return Vec::new();
    };
    result
        .issues
        .into_iter()
        .filter_map(|issue| {
            let key = issue.key.trim();
            if key.is_empty() {
                return None;
            }
            // 📋 <KEY> — the session name leads with the source emoji.
            let mut name = String::from("\u{1F4CB} ");
            name.push_str(key);
            let spawn = SpawnSpec::new(env.code_root(), name)?;
            let summary: String = issue.fields.summary.trim().chars().take(80).collect();
            let mut title = String::new();
            title.push_str(key);
            if !summary.is_empty() {
                title.push(' ');
                title.push_str(&summary);
            }
            let mut detail = issue.fields.status.name.trim().to_string();
            let prio = issue.fields.priority.name.trim();
            if !prio.is_empty() {
                if !detail.is_empty() {
                    detail.push_str(" \u{00B7} "); // ·
                }
                detail.push_str(prio);
            }
            // Priority drives rank: a high-priority ticket rises to the top of
            // the session-generation stream (operator directive). Highest/High
            // → Critical, scored so Highest leads.
            Some(
                Suggestion::new(SourceKind::JiraAssigned, key, title, spawn)
                    .detail(detail)
                    .ranked(super::util::JiraPriority::rank_of(prio)),
            )
        })
        .take(max.max(1))
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct SearchResult {
    #[serde(default)]
    issues: Vec<Issue>,
}

#[derive(serde::Deserialize, Default)]
struct Issue {
    #[serde(default)]
    key: String,
    #[serde(default)]
    fields: Fields,
}

#[derive(serde::Deserialize, Default)]
struct Fields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    status: NamedField,
    #[serde(default)]
    priority: NamedField,
}

#[derive(serde::Deserialize, Default)]
struct NamedField {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::Urgency;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "issues": [
            {"key":"PLEME-1","fields":{"summary":"fix the parser","status":{"name":"In Progress"},"priority":{"name":"High"}}},
            {"key":"PLEME-2","fields":{"summary":"bump deps","status":{"name":"To Do"},"priority":{"name":"Low"}}}
        ]
    }"#;

    /// The exact search URL the source builds for the default JQL — rebuilt
    /// here through `pct` so the fixture key can never drift from the encoder.
    fn search_url() -> String {
        let mut u = String::from("https://pleme.atlassian.net/rest/api/3/search/jql?jql=");
        u.push_str(&pct(DEFAULT_JQL));
        u.push_str("&maxResults=5&fields=summary,status,priority");
        u
    }

    fn cfg() -> SourceConfig {
        let mut c = SourceConfig::for_kind(SourceKind::JiraAssigned);
        c.params
            .insert("site".to_string(), "pleme.atlassian.net".to_string());
        c.params
            .insert("email".to_string(), "op@pleme.io".to_string());
        c
    }

    #[test]
    fn produces_a_suggestion_per_assigned_issue() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("atlassian/api-token", "tok")
            .http(search_url(), FIXTURE);
        let out = JiraAssignedSource.poll(&env, &cfg());
        assert_eq!(out.len(), 2);
        let one = out.iter().find(|s| s.title.contains("PLEME-1")).unwrap();
        assert!(one.title.contains("fix the parser"));
        assert!(one.detail.as_deref().unwrap().contains("In Progress"));
        assert!(one.detail.as_deref().unwrap().contains("High"));
        assert_eq!(one.spawn.name(), "\u{1F4CB} PLEME-1");
        assert_eq!(one.spawn.cwd().to_str().unwrap(), "/code");
        // Priority drives rank: the High ticket rises to the Critical tier (the
        // top of the session-generation stream); the Low ticket stays calm.
        assert_eq!(one.urgency, Urgency::Critical);
        let two = out.iter().find(|s| s.title.contains("PLEME-2")).unwrap();
        assert_eq!(two.urgency, Urgency::Low);
        assert!(
            one.rank_key() > two.rank_key(),
            "the High-priority ticket must rank above the Low one"
        );
    }

    #[test]
    fn unconfigured_or_unauthed_yields_nothing() {
        // No site/email params and no token secret → nothing to query.
        let env = MockEnvironment::new();
        let bare = SourceConfig::for_kind(SourceKind::JiraAssigned);
        assert!(JiraAssignedSource.poll(&env, &bare).is_empty());
        // Configured but the token secret is missing → still empty.
        assert!(JiraAssignedSource.poll(&env, &cfg()).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
