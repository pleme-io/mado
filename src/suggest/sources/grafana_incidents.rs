//! `grafana-incidents` — open incidents from a Grafana instance's annotation
//! feed, surfaced as "go look at this now" suggestions. HTTP against Grafana's
//! annotations API; auth is the `grafana/api-token` secret (Bearer) + a
//! `base_url` param. Enter spawns a session rooted at your code root.
//!
//! Live wiring: `GET <base_url>/api/annotations?tags=incident&limit=N` with
//! `Authorization: Bearer <token>`. Config params: `base_url` (the Grafana
//! root, required), `secret` (override the token's `category/name`, default
//! `grafana/api-token`). Honesty contract: a missing `base_url` is
//! `Unavailable(Unconfigured)`, a missing token `Unavailable(AuthMissing)`, a
//! failed fetch `Unavailable(Error)` — only an OBSERVED response is `Fetched`
//! (so a network blip never reads as "no incidents"). The incident annotation
//! shape is assumed: a JSON array of `{id, text}` rows tagged `incident`.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct GrafanaIncidentsSource;

impl SuggestionSource for GrafanaIncidentsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GrafanaIncidents
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let Some(base) = cfg.param("base_url") else {
            return PollOutcome::unconfigured();
        };
        let secret_key = cfg.param("secret").unwrap_or("grafana/api-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
        let max = cfg.max_items.max(1);
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/api/annotations?tags=incident&limit=");
        url.push_str(&max.to_string());
        let req = HttpReq::new(url)
            .bearer(&token)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env, max))
    }
}

/// Parse a Grafana `/api/annotations` response into suggestions. Pure — the unit
/// the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<AnnotationRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .take(max)
        .filter_map(|row| {
            let cwd = env.code_root();
            let name = String::from("\u{1F6A9} incident"); // 🚩
            let spawn = SpawnSpec::new(cwd, name)?;
            let title: String = row.text.trim().chars().take(60).collect();
            let key = row.id.to_string();
            Some(
                Suggestion::new(SourceKind::GrafanaIncidents, &key, title, spawn)
                    .detail("grafana")
                    .urgent(Urgency::Critical),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct AnnotationRow {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"id":7,"text":"disk full on rio"},
        {"id":42,"text":"api latency spike"}
    ]"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::GrafanaIncidents);
        cfg.max_items = 5;
        cfg.params
            .insert("base_url".into(), "https://grafana.rio".into());
        cfg
    }

    fn url() -> String {
        // Built to match exactly what poll constructs so the mock keys on it.
        String::from("https://grafana.rio/api/annotations?tags=incident&limit=5")
    }

    #[test]
    fn produces_a_suggestion_per_incident() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("grafana/api-token", "tok")
            .http(&url(), FIXTURE);
        let PollOutcome::Fetched(out) = GrafanaIncidentsSource.poll(&env, &cfg()) else {
            panic!("an observed response is Fetched");
        };
        assert_eq!(out.len(), 2);
        let disk = out.iter().find(|s| s.title.contains("disk full")).unwrap();
        assert_eq!(disk.urgency, Urgency::Critical);
        assert_eq!(disk.detail.as_deref(), Some("grafana"));
        assert_eq!(disk.spawn.cwd().to_str().unwrap(), "/code");
        // The session name carries the source emoji; the title stays plain.
        assert!(disk.spawn.name().starts_with('\u{1F6A9}'));
        assert!(out.iter().any(|s| s.title.contains("api latency spike")));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No base_url param → Unconfigured (needs config, not "no incidents").
        let env = MockEnvironment::new();
        let bare = SourceConfig::for_kind(SourceKind::GrafanaIncidents);
        assert_eq!(
            GrafanaIncidentsSource.poll(&env, &bare),
            PollOutcome::unconfigured()
        );
        // base_url present but the token secret is missing → AuthMissing.
        assert_eq!(
            GrafanaIncidentsSource.poll(&env, &cfg()),
            PollOutcome::auth_missing()
        );
        // base_url + token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("grafana/api-token", "tok");
        assert_eq!(
            GrafanaIncidentsSource.poll(&env, &cfg()),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
