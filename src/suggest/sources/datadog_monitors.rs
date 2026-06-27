//! `datadog-monitors` — Datadog monitors currently in the `Alert` state,
//! surfaced as "go look at this fire" suggestions. HTTP against Datadog's v1
//! monitor API; auth is the `datadog/api-key` + `datadog/app-key` secrets sent
//! as headers. Enter spawns a session rooted at your code root.
//!
//! Live wiring: `GET <base_url>/api/v1/monitor?group_states=alert` with
//! `DD-API-KEY` + `DD-APPLICATION-KEY` headers. Each monitor whose
//! `overall_state` is `Alert` becomes a suggestion. Missing keys (sent as
//! empty) just yield an unauthorized body; a non-JSON / empty body → no
//! suggestions (graceful).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};
use super::util::PriorityScale;

pub struct DatadogMonitorsSource;

impl SuggestionSource for DatadogMonitorsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::DatadogMonitors
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let apikey = env.secret("datadog/api-key").unwrap_or_default();
        let appkey = env.secret("datadog/app-key").unwrap_or_default();
        let base = cfg.param("base_url").unwrap_or("https://api.datadoghq.com");
        let max = cfg.max_items.max(1);
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/api/v1/monitor?group_states=alert");
        let req = HttpReq::new(url)
            .header("DD-API-KEY", apikey)
            .header("DD-APPLICATION-KEY", appkey)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&out, env, max)
    }
}

/// Parse a Datadog `/api/v1/monitor` response into suggestions for monitors in
/// the `Alert` state. Pure — the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<MonitorRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter(|m| m.overall_state == "Alert")
        .take(max)
        .filter_map(|m| {
            let cwd = env.code_root();
            // Display name (no emoji) drives both the session name and the
            // title; the picker prepends the source emoji to the title.
            let label: String = m.name.trim().chars().take(24).collect();
            let mut name = String::from("\u{1F415} "); // 🐕
            name.push_str(&label);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = String::new();
            title.push_str(&label);
            title.push_str(" alerting");
            let key = m.id.to_string();
            // Rank by Datadog's 1–5 monitor priority (P1 highest): a P1 outranks
            // a P3. Absent priority stays Critical (firing-but-unprioritized).
            let level = match m.priority {
                Some(1) => "p1",
                Some(2) => "p2",
                Some(3) => "p3",
                Some(4) => "p4",
                Some(5) => "p5",
                _ => "",
            };
            let rank = super::util::IncidentSeverity::rank_of(level);
            let mut detail = String::from("datadog");
            if let Some(p) = m.priority {
                detail.push_str(" \u{00B7} P"); // ·
                detail.push_str(&p.to_string());
            }
            Some(
                Suggestion::new(SourceKind::DatadogMonitors, &key, title, spawn)
                    .detail(detail)
                    .ranked(rank),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct MonitorRow {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    overall_state: String,
    /// Datadog monitor priority 1 (highest) .. 5 (lowest); absent = unset.
    #[serde(default)]
    priority: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::Urgency;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"id":111,"name":"api latency p99","overall_state":"Alert","priority":1},
        {"id":222,"name":"disk usage","overall_state":"OK"},
        {"id":333,"name":"error rate spike","overall_state":"Alert","priority":3}
    ]"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::DatadogMonitors);
        cfg.max_items = 5;
        cfg
    }

    fn url() -> String {
        // The default base_url — matches exactly what poll builds.
        String::from("https://api.datadoghq.com/api/v1/monitor?group_states=alert")
    }

    #[test]
    fn produces_a_suggestion_per_alerting_monitor() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("datadog/api-key", "ddk")
            .secret_val("datadog/app-key", "dak")
            .http(&url(), FIXTURE);
        let out = DatadogMonitorsSource.poll(&env, &cfg());
        // The OK monitor is excluded; only the two Alert monitors remain.
        assert_eq!(out.len(), 2, "non-alert monitor excluded");
        let api = out.iter().find(|s| s.title.contains("api latency")).unwrap();
        assert!(api.title.contains("alerting"));
        // P1 → Critical, detail carries the priority.
        assert_eq!(api.detail.as_deref(), Some("datadog \u{00B7} P1"));
        assert_eq!(api.urgency, Urgency::Critical);
        assert_eq!(api.spawn.cwd().to_str().unwrap(), "/code");
        // A P3 monitor ranks below the P1 (High, not Critical).
        let p3 = out.iter().find(|s| s.title.contains("error rate spike")).unwrap();
        assert_eq!(p3.urgency, Urgency::High);
        assert!(api.rank_key() > p3.rank_key(), "P1 monitor outranks P3");
    }

    #[test]
    fn no_http_yields_nothing() {
        // No fixture registered → http_get returns None → empty.
        assert!(DatadogMonitorsSource
            .poll(&MockEnvironment::new(), &cfg())
            .is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
