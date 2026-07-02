//! `grafana-alerts` — alerts currently firing in Grafana, surfaced as "go look
//! at this" suggestions. HTTP source: a bearer token from `grafana/api-token`
//! and a `base_url` param point at the Grafana alerts API.
//!
//! Live wiring: `GET <base_url>/api/prometheus/grafana/api/v1/alerts` with
//! `Authorization: Bearer <token>` → `{data:{alerts:[{labels, state}]}}`. An
//! alert whose `state` is `Alerting` (Grafana) or `firing` (Prometheus) becomes
//! a suggestion that drops you in the code root. Config params: `base_url`
//! (the Grafana root, required), `secret` (override the token's
//! `category/name`, default `grafana/api-token`). Honesty contract: a missing
//! `base_url` is `Unavailable(Unconfigured)`, a missing token
//! `Unavailable(AuthMissing)`, a failed fetch `Unavailable(Error)` — only an
//! OBSERVED response is `Fetched` (so a network blip never reads as "nothing
//! firing").

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};
use super::util::PriorityScale;

pub struct GrafanaAlertsSource;

impl SuggestionSource for GrafanaAlertsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GrafanaAlerts
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let Some(base) = cfg.param("base_url") else {
            return PollOutcome::unconfigured();
        };
        let secret_key = cfg.param("secret").unwrap_or("grafana/api-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/api/prometheus/grafana/api/v1/alerts");
        let req = HttpReq::new(url)
            .bearer(&token)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env, cfg.max_items.max(1)))
    }
}

/// Parse `…/api/v1/alerts` output into suggestions for firing alerts. Pure — the
/// unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(payload) = serde_json::from_str::<AlertsPayload>(json) else {
        return Vec::new();
    };
    let mut out: Vec<Suggestion> = payload
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
            // Rank by the alert's `severity` label, not a flat Critical: a
            // firing `warning` is real but not as urgent as a `critical`. A
            // missing label keeps it Critical (firing-but-unlabeled).
            let severity = a.labels.get("severity").map(String::as_str).unwrap_or("");
            let mut detail = String::from("grafana");
            if !severity.is_empty() {
                detail.push_str(" \u{00B7} "); // ·
                detail.push_str(severity);
            }
            Some(
                Suggestion::new(SourceKind::GrafanaAlerts, &key, title, spawn)
                    .detail(detail)
                    .ranked(super::util::IncidentSeverity::rank_of(severity)),
            )
        })
        .collect();
    // Rank BEFORE the cap: the alerts API has no server-side ordering we can
    // trust, so cutting in upstream order could drop a critical while keeping
    // warnings. Sort by rank, then cap.
    out.sort_by(|a, b| b.rank_key().cmp(&a.rank_key()));
    out.truncate(max);
    out
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
    use crate::suggest::core::Urgency;
    use crate::suggest::env::MockEnvironment;

    const URL: &str = "https://grafana.rio/api/prometheus/grafana/api/v1/alerts";

    const FIXTURE: &str = r#"{
        "data": {
            "alerts": [
                {"labels": {"alertname": "HighCPU", "severity": "critical"}, "state": "Alerting"},
                {"labels": {"alertname": "SlowQuery", "severity": "warning"}, "state": "firing"},
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
        let PollOutcome::Fetched(out) = GrafanaAlertsSource.poll(&env(), &cfg()) else {
            panic!("an observed response is Fetched");
        };
        assert_eq!(out.len(), 3, "non-firing alert excluded");
        let cpu = out.iter().find(|s| s.title.contains("HighCPU")).unwrap();
        assert!(cpu.title.contains("firing"));
        // Detail now carries the severity; a critical alert stays Critical.
        assert_eq!(cpu.detail.as_deref(), Some("grafana \u{00B7} critical"));
        assert_eq!(cpu.urgency, Urgency::Critical);
        assert_eq!(cpu.spawn.cwd().to_str().unwrap(), "/code");
        // A firing WARNING is real but ranks below a critical (High, not Critical).
        let warn = out.iter().find(|s| s.title.contains("SlowQuery")).unwrap();
        assert_eq!(warn.urgency, Urgency::High);
        assert!(cpu.rank_key() > warn.rank_key(), "critical outranks warning");
        // An alert with NO severity label stays Critical (firing-but-unlabeled).
        let disk = out.iter().find(|s| s.title.contains("DiskFull")).unwrap();
        assert_eq!(disk.urgency, Urgency::Critical);
        assert_eq!(disk.detail.as_deref(), Some("grafana"));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No base_url param → Unconfigured (needs config, not "nothing firing").
        let env = MockEnvironment::new();
        let bare = SourceConfig::for_kind(SourceKind::GrafanaAlerts);
        assert_eq!(
            GrafanaAlertsSource.poll(&env, &bare),
            PollOutcome::unconfigured()
        );
        // base_url present but the token secret is missing → AuthMissing.
        assert_eq!(
            GrafanaAlertsSource.poll(&env, &cfg()),
            PollOutcome::auth_missing()
        );
        // base_url + token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("grafana/api-token", "tok-123");
        assert_eq!(GrafanaAlertsSource.poll(&env, &cfg()), PollOutcome::error());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
