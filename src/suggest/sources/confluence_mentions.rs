//! `confluence-mentions` — Confluence pages that mention you, surfaced as
//! "go read / respond to this" suggestions. Needs an Atlassian Cloud API
//! token (`atlassian/api-token`) plus a `base_url` + `email` in the source's
//! config params; absent either, the source contributes nothing (graceful).
//!
//! Live wiring: `GET <base>/wiki/rest/api/search?limit=N&cql=<cql>` with HTTP
//! Basic auth (email + API token), where the CQL selects pages that mention
//! the current user, newest first. Atlassian's REST search is stable +
//! documented; a non-2xx (bad token / wrong base) → no suggestions.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct ConfluenceMentionsSource;

impl SuggestionSource for ConfluenceMentionsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::ConfluenceMentions
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let token = env.secret("atlassian/api-token").unwrap_or_default();
        let base = cfg.param("base_url").unwrap_or_default();
        let email = cfg.param("email").unwrap_or_default();
        let max = cfg.max_items.max(1).to_string();
        let cql = "mention = currentUser() order by lastModified desc";
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/wiki/rest/api/search?limit=");
        url.push_str(&max);
        url.push_str("&cql=");
        url.push_str(&pct(cql));
        let req = HttpReq::new(url)
            .basic_auth(email, token)
            .header("Accept", "application/json");
        let Some(body) = env.http_get(&req) else {
            return Vec::new();
        };
        let mut items = parse(&body, env);
        items.truncate(cfg.max_items.max(1));
        items
    }
}

/// Parse `/wiki/rest/api/search` output into suggestions. Pure — the unit the
/// source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(resp) = serde_json::from_str::<SearchResponse>(json) else {
        return Vec::new();
    };
    resp.results
        .into_iter()
        .filter_map(|row| {
            let content = row.content;
            let title = content.title;
            let id = content.id;
            let cwd = env.code_root();
            let mut name = String::from("\u{1F4AC} "); // 💬
            let short: String = title.trim().chars().take(32).collect();
            name.push_str(&short);
            let spawn = SpawnSpec::new(cwd, name)?;
            Some(
                Suggestion::new(SourceKind::ConfluenceMentions, &id, title.trim(), spawn)
                    .detail("confluence")
                    .urgent(Urgency::Low),
            )
        })
        .collect()
}

use super::util::pct;

#[derive(serde::Deserialize, Default)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchRow>,
}

#[derive(serde::Deserialize, Default)]
struct SearchRow {
    #[serde(default)]
    content: Content,
}

#[derive(serde::Deserialize, Default)]
struct Content {
    #[serde(default)]
    title: String,
    #[serde(default)]
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "results": [
            {"content": {"id": "12345", "title": "Q3 planning notes"}},
            {"content": {"id": "67890", "title": "Architecture review"}}
        ]
    }"#;

    const URL: &str = "https://x.atlassian.net/wiki/rest/api/search?limit=5&cql=mention%20%3D%20currentUser%28%29%20order%20by%20lastModified%20desc";

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::ConfluenceMentions);
        cfg.params
            .insert("base_url".into(), "https://x.atlassian.net".into());
        cfg.params.insert("email".into(), "me@x.io".into());
        cfg
    }

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("atlassian/api-token", "tok")
            .http(URL, FIXTURE)
    }

    #[test]
    fn produces_a_suggestion_per_mentioning_page() {
        let out = ConfluenceMentionsSource.poll(&env(), &cfg());
        assert_eq!(out.len(), 2);
        let notes = out
            .iter()
            .find(|s| s.title.contains("Q3 planning notes"))
            .unwrap();
        assert_eq!(notes.detail.as_deref(), Some("confluence"));
        assert_eq!(notes.urgency, Urgency::Low);
        // Spawn drops you in the code root with the 💬-prefixed session name.
        assert_eq!(notes.spawn.cwd().to_str().unwrap(), "/code");
        assert!(notes.spawn.name().starts_with('\u{1F4AC}'));
        assert!(out.iter().any(|s| s.title.contains("Architecture review")));
    }

    #[test]
    fn no_token_or_endpoint_yields_nothing() {
        // No http fixture / secret / params registered → http_get returns
        // None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::ConfluenceMentions);
        assert!(ConfluenceMentionsSource
            .poll(&MockEnvironment::new(), &cfg)
            .is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
