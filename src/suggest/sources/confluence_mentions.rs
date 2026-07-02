//! `confluence-mentions` — Confluence pages that mention you, surfaced as
//! "go read / respond to this" suggestions. Needs an Atlassian Cloud API
//! token (`atlassian/api-token`, overridable via the `secret` param) plus a
//! `base_url` + `email` in the source's config params.
//!
//! Live wiring: `GET <base>/wiki/rest/api/search?limit=N&cql=<cql>` with HTTP
//! Basic auth (email + API token), where the CQL selects pages that mention
//! the current user, newest first. Honesty contract: a missing `base_url` is
//! `Unavailable(Unconfigured)`, a missing token `Unavailable(AuthMissing)`, a
//! failed fetch (bad token / wrong base → non-2xx) `Unavailable(Error)` —
//! only an OBSERVED response is `Fetched` (so an outage never reads as
//! "nobody mentioned you").

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct ConfluenceMentionsSource;

impl SuggestionSource for ConfluenceMentionsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::ConfluenceMentions
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let Some(base) = cfg.param("base_url") else {
            return PollOutcome::unconfigured();
        };
        let secret_key = cfg.param("secret").unwrap_or("atlassian/api-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
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
            return PollOutcome::error();
        };
        let mut items = parse(&body, env);
        items.truncate(cfg.max_items.max(1));
        PollOutcome::Fetched(items)
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
        let PollOutcome::Fetched(out) = ConfluenceMentionsSource.poll(&env(), &cfg()) else {
            panic!("an observed response is Fetched");
        };
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
    fn honesty_tiers_are_typed_not_empty() {
        // No base_url param → Unconfigured (needs config, not "no mentions").
        let env = MockEnvironment::new();
        let bare = SourceConfig::for_kind(SourceKind::ConfluenceMentions);
        assert_eq!(
            ConfluenceMentionsSource.poll(&env, &bare),
            PollOutcome::unconfigured()
        );
        // Params present but the token secret is missing → AuthMissing.
        assert_eq!(
            ConfluenceMentionsSource.poll(&env, &cfg()),
            PollOutcome::auth_missing()
        );
        // Params + token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("atlassian/api-token", "tok");
        assert_eq!(
            ConfluenceMentionsSource.poll(&env, &cfg()),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
