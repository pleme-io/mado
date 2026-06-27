//! `grafana-alerts` — alerts currently firing in Grafana, surfaced as "go look
//! at this" suggestions. HTTP source: a bearer token from `grafana/api-token`
//! and a `base_url` param point at the Grafana alerts API.
//!
//! Live wiring: `GET <base_url>/api/prometheus/grafana/api/v1/alerts` with
//! `Authorization: Bearer <token>` → `{data:{alerts:[{labels, state}]}}`. An
//! alert whose `state` is `Alerting` (Grafana) or `firing` (Prometheus) becomes
//! a suggestion that drops you in the code root. No token / no base / no JSON →
//! graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct GrafanaAlertsSource;

impl SuggestionSource for GrafanaAlertsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GrafanaAlerts
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let token = env.secret("grafana/api-token").unwrap_or_default();
        let base = cfg.param("base_url").unwrap_or("");
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/api/prometheus/grafana/api/v1/alerts");
        let req = HttpReq::new(url)
            .bearer(&token)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&out, env, cfg.max_items.max(1))
    }
}

/// Parse `…/api/v1/alerts` output into suggestions for firing alerts. Pure — the
/// unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(payload) = serde_json::from_str::<AlertsPayload>(json) else {
        return Vec::new();
    };
    payload
        .data
        .alerts
        .into_iter()
        .filter(|a| a.state == "Alerting" || a.state == "firing")
        .filter_map(|a| {
            let alertname = a
                .labels
                .get("alertname")
                .cloned()
                .unwrap_or_else(|| String::from("alert"));
            let cwd = env.code_root();
            let truncated: String = alertname.chars().take(24).collect();
            let mut name = String::from("\u{1F525} "); // 🔥
            name.push_str(&truncated);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = alertname.clone();
            title.push_str(" firing");
            // Key on alertname + the full (sorted) label-set, not alertname
            // alone: the /api/v1/alerts feed returns one entry per firing
            // INSTANCE (label-set), so N instances of one rule must stay N
            // distinct ids — keying on alertname collapses them in the store.
            let mut key = alertname.clone();
            for (lk, lv) in &a.labels {
                key.push('|');
                key.push_str(lk);
                key.push('=');
                key.push_str(lv);
            }
            Some(
                Suggestion::new(SourceKind::GrafanaAlerts, &key, title, spawn)
                    .detail("grafana")
                    .urgent(Urgency::Critical),
            )
        })
        .take(max)
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct AlertsPayload {
    #[serde(default)]
    data: AlertsData,
}

#[derive(serde::Deserialize, Default)]
struct AlertsData {
    #[serde(default)]
    alerts: Vec<AlertRow>,
}

#[derive(serde::Deserialize, Default)]
struct AlertRow {
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const URL: &str = "https://grafana.rio/api/prometheus/grafana/api/v1/alerts";

    const FIXTURE: &str = r#"{
        "data": {
            "alerts": [
                {"labels": {"alertname": "HighCPU", "severity": "critical"}, "state": "Alerting"},
                {"labels": {"alertname": "DiskFull"}, "state": "firing"},
                {"labels": {"alertname": "Quiet"}, "state": "Normal"}
            ]
        }
    }"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("grafana/api-token", "tok-123")
            .http(URL, FIXTURE)
    }

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::GrafanaAlerts);
        cfg.params
            .insert(String::from("base_url"), String::from("https://grafana.rio"));
        cfg
    }

    #[test]
    fn surfaces_only_firing_alerts() {
        let out = GrafanaAlertsSource.poll(&env(), &cfg());
        assert_eq!(out.len(), 2, "non-firing alert excluded");
        let cpu = out.iter().find(|s| s.title.contains("HighCPU")).unwrap();
        assert!(cpu.title.contains("firing"));
        assert_eq!(cpu.detail.as_deref(), Some("grafana"));
        assert_eq!(cpu.urgency, Urgency::Critical);
        assert_eq!(cpu.spawn.cwd().to_str().unwrap(), "/code");
        assert!(out.iter().any(|s| s.title.contains("DiskFull")));
    }

    #[test]
    fn no_endpoint_yields_nothing() {
        // No http fixture registered → http_get() returns None → empty.
        let out = GrafanaAlertsSource.poll(&MockEnvironment::new(), &cfg());
        assert!(out.is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
