//! `jira-sprint` — issues assigned to you in the current open sprint, surfaced
//! as "go work this ticket" suggestions. HTTP against Atlassian Cloud's Jira
//! search API; auth is the `atlassian/api-token` secret + an `email` param
//! (HTTP Basic). Enter spawns a session rooted at your code root.
//!
//! Live wiring: `GET <base_url>/rest/api/3/search?maxResults=N&fields=summary&jql=<jql>`
//! with `Authorization: Basic <email:token>`. Missing secret / missing
//! `base_url` param / non-JSON body → no suggestions (graceful).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct JiraSprintSource;

impl SuggestionSource for JiraSprintSource {
    fn kind(&self) -> SourceKind {
        SourceKind::JiraSprint
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let Some(token) = env.secret("atlassian/api-token") else {
            return Vec::new();
        };
        let Some(base) = cfg.param("base_url") else {
            return Vec::new();
        };
        let email = cfg.param("email").unwrap_or("");
        let jql = cfg.param("jql").unwrap_or(
            "assignee=currentUser() AND sprint in openSprints() AND statusCategory != Done",
        );
        let max = cfg.max_items.max(1);
        let mut url = String::new();
        url.push_str(base);
        // /search/jql — the current Jira Cloud endpoint (legacy /search was
        // removed 2025-05-01); the response shape (issues[].key/fields) is the same.
        url.push_str("/rest/api/3/search/jql?maxResults=");
        url.push_str(&max.to_string());
        url.push_str("&fields=summary&jql=");
        url.push_str(&pct(jql));
        let req = HttpReq::new(url)
            .basic_auth(email, token)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&out, env, max)
    }
}

/// Parse a Jira `/rest/api/3/search` response into suggestions. Pure — the unit
/// the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(result) = serde_json::from_str::<SearchResult>(json) else {
        return Vec::new();
    };
    result
        .issues
        .into_iter()
        .filter(|i| !i.key.is_empty())
        .take(max)
        .filter_map(|issue| {
            let cwd = env.code_root();
            let summary: String = issue.fields.summary.trim().chars().take(80).collect();
            let mut name = String::from("\u{1F3AB} "); // 🎫
            name.push_str(&issue.key);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = String::new();
            title.push_str(&issue.key);
            title.push(' ');
            title.push_str(&summary);
            Some(
                Suggestion::new(SourceKind::JiraSprint, &issue.key, title, spawn)
                    .detail(issue.key.clone())
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

/// Percent-encode a JQL string for use in a query parameter. Keeps the
use super::util::pct;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "issues": [
            {"key":"PROJ-1","fields":{"summary":"fix the parser"}},
            {"key":"PROJ-2","fields":{"summary":"bump deps"}}
        ]
    }"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::JiraSprint);
        cfg.max_items = 5;
        cfg.params
            .insert("base_url".into(), "https://acme.atlassian.net".into());
        cfg.params.insert("email".into(), "me@acme.io".into());
        cfg.params.insert("jql".into(), "sprint=42".into());
        cfg
    }

    fn url() -> String {
        // Built with the same helper poll uses so the mock matches exactly.
        let mut u = String::from(
            "https://acme.atlassian.net/rest/api/3/search/jql?maxResults=5&fields=summary&jql=",
        );
        u.push_str(&pct("sprint=42"));
        u
    }

    #[test]
    fn produces_a_suggestion_per_issue() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("atlassian/api-token", "tok")
            .http(&url(), FIXTURE);
        let out = JiraSprintSource.poll(&env, &cfg());
        assert_eq!(out.len(), 2);
        let one = out.iter().find(|s| s.title.contains("PROJ-1")).unwrap();
        assert!(one.title.contains("fix the parser"));
        assert_eq!(one.detail.as_deref(), Some("PROJ-1"));
        assert_eq!(one.urgency, Urgency::Normal);
        assert_eq!(one.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn jql_is_percent_encoded() {
        // The '=' in the JQL must be %3D — proving the request URL the mock
        // keys on is exactly what poll builds.
        assert_eq!(pct("sprint=42"), "sprint%3D42");
    }

    #[test]
    fn no_secret_yields_nothing() {
        // Secret missing → poll bails before any HTTP call.
        let env = MockEnvironment::new().http(&url(), FIXTURE);
        assert!(JiraSprintSource.poll(&env, &cfg()).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
