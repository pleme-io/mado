//! VictoriaMetrics / Prometheus firing-alerts source — the SAFRA.md M0 gate
//! cell. Ingests `GET <base>/api/v1/alerts` (the Prometheus-compatible alerts
//! surface vmalert also speaks), filters to `state == "firing"`, and
//! normalizes each alert to a [`Signal`] whose identity is
//! `alertname{sorted,labels}` — stable across re-observations so recurrence
//! counts instead of duplicating.

use super::curated::Severity;
use super::schema::ServiceKind;
use super::signal::Signal;
use super::source::{CellSource, ObserveCtx};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};

/// The `/api/v1/alerts` response envelope (Prometheus wire shape).
#[derive(Debug, Clone, serde::Deserialize)]
struct AlertsResponse {
    #[serde(default)]
    data: AlertsData,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AlertsData {
    #[serde(default)]
    alerts: Vec<Alert>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Alert {
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    value: Option<String>,
}

/// Map the conventional `severity` label onto the curated ladder. Unlabelled
/// alerts read as warnings; only an explicit `info`/`none` label demotes.
fn severity_of(labels: &std::collections::BTreeMap<String, String>) -> Severity {
    match labels.get("severity").map(String::as_str) {
        Some("critical" | "page") => Severity::Critical,
        Some("info" | "none") => Severity::Info,
        _ => Severity::Warning,
    }
}

/// Stable identity: `alertname{k=v,…}` over the sorted non-name labels (the
/// BTreeMap iterates sorted, so the identity is deterministic).
fn identity_of(labels: &std::collections::BTreeMap<String, String>) -> String {
    let name = labels.get("alertname").map(String::as_str).unwrap_or("alert");
    let mut id = String::from(name);
    id.push('{');
    let mut first = true;
    for (k, v) in labels {
        if k == "alertname" {
            continue;
        }
        if !first {
            id.push(',');
        }
        first = false;
        id.push_str(k);
        id.push('=');
        id.push_str(v);
    }
    id.push('}');
    id
}

/// The concrete [`CellSource`] for `victoria-metrics` / `prometheus` endpoints.
pub struct VmAlertsSource;

impl CellSource for VmAlertsSource {
    fn service(&self) -> ServiceKind {
        ServiceKind::VictoriaMetrics
    }

    fn observe(&self, env: &dyn SuggestionEnvironment, ctx: &ObserveCtx) -> Vec<Signal> {
        let mut url = String::from(ctx.endpoint.base_url.trim_end_matches('/'));
        url.push_str("/api/v1/alerts");
        let mut req = HttpReq::new(url).header("Accept", "application/json");
        if let Some(tok) = ctx.secret.as_deref() {
            req = req.bearer(tok);
        }
        let Some(body) = env.http_get(&req) else {
            // Unreachable endpoint → observed-nothing per the CellSource
            // contract (the curated set decays; the suggest-plane health
            // surface reports the ADAPTER's availability separately).
            return Vec::new();
        };
        parse(&body, &ctx.env.name, &ctx.kind.name)
    }
}

/// Parse the alerts body into firing [`Signal`]s. Pure — the tested unit.
fn parse(json: &str, env_name: &str, kind_name: &str) -> Vec<Signal> {
    let Ok(resp) = serde_json::from_str::<AlertsResponse>(json) else {
        return Vec::new();
    };
    resp.data
        .alerts
        .into_iter()
        .filter(|a| a.state == "firing")
        .map(|a| {
            let identity = identity_of(&a.labels);
            let name = a
                .labels
                .get("alertname")
                .map(String::as_str)
                .unwrap_or("alert");
            let mut label = String::from(name);
            if let Some(s) = a.annotations.get("summary").filter(|s| !s.trim().is_empty()) {
                label.push_str(" — ");
                label.push_str(s.trim());
            }
            let mut sig = Signal::new(env_name, kind_name, identity, severity_of(&a.labels), label);
            if let Some(v) = a.value.as_deref().filter(|v| !v.is_empty()) {
                let mut d = String::from("value ");
                d.push_str(v);
                sig = sig.with_detail(d);
            }
            sig
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safra::schema::{Endpoint, TrackedDataKind, TrackedEnvironment};
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
      "status": "success",
      "data": { "alerts": [
        {"labels":{"alertname":"OOMKilled","pod":"api-1","severity":"critical"},
         "annotations":{"summary":"api-1 OOM"},"state":"firing","value":"3"},
        {"labels":{"alertname":"HighLatency","severity":"warning"},
         "annotations":{},"state":"firing"},
        {"labels":{"alertname":"Resolved","severity":"critical"},
         "annotations":{},"state":"inactive"}
      ]}
    }"#;

    fn ctx_parts() -> (TrackedEnvironment, TrackedDataKind) {
        (
            TrackedEnvironment {
                name: "rio".into(),
                groups: vec![],
                endpoints: vec![Endpoint {
                    service: ServiceKind::VictoriaMetrics,
                    base_url: "http://vm.rio:8481".into(),
                    secret_ref: None,
                }],
            },
            TrackedDataKind {
                name: "firing-alerts".into(),
                service: ServiceKind::VictoriaMetrics,
                groups: vec![],
                ttl_secs: 300,
            },
        )
    }

    #[test]
    fn observes_firing_alerts_only_with_stable_identity() {
        let (env_decl, kind) = ctx_parts();
        let endpoint = env_decl.endpoint(ServiceKind::VictoriaMetrics).unwrap().clone();
        let ctx = ObserveCtx {
            env: &env_decl,
            kind: &kind,
            endpoint: &endpoint,
            secret: None,
        };
        let mock = MockEnvironment::new().http("http://vm.rio:8481/api/v1/alerts", FIXTURE);
        let out = VmAlertsSource.observe(&mock, &ctx);
        assert_eq!(out.len(), 2, "inactive alerts are not signals");
        let oom = out.iter().find(|s| s.label.contains("OOMKilled")).unwrap();
        assert_eq!(oom.severity, Severity::Critical);
        assert_eq!(oom.identity, "OOMKilled{pod=api-1,severity=critical}");
        assert!(oom.label.contains("api-1 OOM"), "summary joins the label");
        assert_eq!(oom.detail.as_deref(), Some("value 3"));
        let lat = out.iter().find(|s| s.label.contains("HighLatency")).unwrap();
        assert_eq!(lat.severity, Severity::Warning);
    }

    #[test]
    fn unreachable_endpoint_and_garbage_are_observed_empty() {
        let (env_decl, kind) = ctx_parts();
        let endpoint = env_decl.endpoint(ServiceKind::VictoriaMetrics).unwrap().clone();
        let ctx = ObserveCtx {
            env: &env_decl,
            kind: &kind,
            endpoint: &endpoint,
            secret: None,
        };
        assert!(VmAlertsSource.observe(&MockEnvironment::new(), &ctx).is_empty());
        assert!(parse("not json", "rio", "k").is_empty());
    }
}
