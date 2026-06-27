//! `opsgenie-alerts` — open Opsgenie alerts surfaced as "go handle this now"
//! suggestions. HTTP against the Opsgenie REST API, authed with a GenieKey API
//! key pulled from the secret store. Each open alert becomes a Critical
//! suggestion that drops you in your code root to start triage.
//!
//! Live wiring: `GET {base}/v2/alerts?limit=N&query=status:%20open` with an
//! `Authorization: GenieKey <key>` header → `{data:[{id,message,priority}]}`.
//! Missing key / unreachable endpoint / bad JSON → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};
use super::util::PriorityScale;

pub struct OpsgenieAlertsSource;

impl SuggestionSource for OpsgenieAlertsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::OpsgenieAlerts
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let max = cfg.max_items.max(1);
        let token = env.secret("opsgenie/api-key").unwrap_or_default();
        let base = cfg.param("base_url").unwrap_or("https://api.opsgenie.com");
        let mut url = base.to_string();
        url.push_str("/v2/alerts?limit=");
        url.push_str(&max.to_string());
        url.push_str("&query=");
        url.push_str(&pct("status: open"));
        let mut auth = String::from("GenieKey ");
        auth.push_str(&token);
        let req = HttpReq::new(url)
            .header("Authorization", auth)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&out, env, max)
    }
}

/// Parse `{data:[…]}` from the Opsgenie alerts endpoint into suggestions. Pure —
/// the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(payload) = serde_json::from_str::<AlertsPayload>(json) else {
        return Vec::new();
    };
    let cwd = env.code_root();
    payload
        .data
        .into_iter()
        .take(max)
        .filter_map(|a| {
            let mut name = String::from("\u{1F514} "); // 🔔
            name.push_str(&truncate(&a.message, 30));
            let spawn = SpawnSpec::new(cwd.clone(), name)?;
            // Rank by P1–P5: a P1 outranks a P3 outranks a P5 (was a flat
            // Critical for every open alert).
            let rank = super::util::IncidentSeverity::rank_of(&a.priority);
            Some(
                Suggestion::new(SourceKind::OpsgenieAlerts, &a.id, a.message, spawn)
                    .detail(a.priority)
                    .ranked(rank),
            )
        })
        .collect()
}

use super::util::pct;

/// Cap a string at `n` chars (char-boundary safe).
fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>()
}

#[derive(serde::Deserialize, Default)]
struct AlertsPayload {
    #[serde(default)]
    data: Vec<Alert>,
}

#[derive(serde::Deserialize, Default)]
struct Alert {
    #[serde(default)]
    id: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    priority: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::Urgency;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "data": [
            {"id":"a1","message":"db replica down","priority":"P1"},
            {"id":"a2","message":"disk almost full on the rio node plus many extra words","priority":"P3"}
        ]
    }"#;

    #[test]
    fn surfaces_open_alerts_ranked_by_priority() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("opsgenie/api-key", "k3y")
            .http(
                "https://api.opsgenie.com/v2/alerts?limit=5&query=status%3A%20open",
                FIXTURE,
            );
        let cfg = SourceConfig::for_kind(SourceKind::OpsgenieAlerts);
        let out = OpsgenieAlertsSource.poll(&env, &cfg);
        assert_eq!(out.len(), 2);
        let p1 = out
            .iter()
            .find(|s| s.title.contains("db replica down"))
            .unwrap();
        // P1 → Critical (top); P3 → High. The P1 outranks the P3.
        assert_eq!(p1.urgency, Urgency::Critical);
        assert_eq!(p1.detail.as_deref(), Some("P1"));
        let p3 = out.iter().find(|s| s.title.contains("disk almost full")).unwrap();
        assert_eq!(p3.urgency, Urgency::High);
        assert!(p1.rank_key() > p3.rank_key(), "P1 outranks P3");
        // No matching repo dir → triage starts in the code root.
        assert_eq!(p1.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn missing_key_or_endpoint_yields_nothing() {
        // No http fixture registered → http_get returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::OpsgenieAlerts);
        assert!(
            OpsgenieAlertsSource
                .poll(&MockEnvironment::new(), &cfg)
                .is_empty()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
