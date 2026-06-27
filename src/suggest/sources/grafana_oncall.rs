//! `grafana-oncall` — your on-call shifts from Grafana OnCall, surfaced as
//! "you're on the hook" suggestions. HTTP against the Grafana OnCall API; auth
//! is the `grafana/oncall-token` secret (Bearer) + an `oncall_url` param naming
//! the API base. Enter spawns a session rooted at your code root.
//!
//! Live wiring: `GET <oncall_url>/api/v1/shifts?per_page=N` with
//! `Authorization: Bearer <token>`. A missing secret / missing `oncall_url`
//! param degrades to an empty Bearer + empty base, so the request keys on a URL
//! nothing answers → no suggestions; a non-JSON body → likewise empty
//! (graceful).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct GrafanaOncallSource;

impl SuggestionSource for GrafanaOncallSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GrafanaOncall
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let token = env.secret("grafana/oncall-token").unwrap_or_default();
        let base = cfg.param("oncall_url").unwrap_or("");
        let max = cfg.max_items.max(1);
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/api/v1/shifts?per_page=");
        url.push_str(&max.to_string());
        let req = HttpReq::new(url).bearer(&token);
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&out, env, max)
    }
}

/// Parse a Grafana OnCall `/api/v1/shifts` response into suggestions. Pure — the
/// unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(result) = serde_json::from_str::<ShiftResult>(json) else {
        return Vec::new();
    };
    result
        .results
        .into_iter()
        .filter(|s| !s.id.is_empty())
        .take(max)
        .filter_map(|shift| {
            let cwd = env.code_root();
            let title: String = shift.name.trim().chars().take(40).collect();
            let name = "\u{1F4DF} on-call"; // 📟 on-call
            let spawn = SpawnSpec::new(cwd, name)?;
            Some(
                Suggestion::new(SourceKind::GrafanaOncall, &shift.id, title, spawn)
                    .detail("oncall")
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct ShiftResult {
    #[serde(default)]
    results: Vec<Shift>,
}

#[derive(serde::Deserialize, Default)]
struct Shift {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "results": [
            {"id":"S1","name":"primary rotation"},
            {"id":"S2","name":"secondary rotation"}
        ]
    }"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::GrafanaOncall);
        cfg.max_items = 5;
        cfg.params
            .insert("oncall_url".into(), "https://oncall.example".into());
        cfg
    }

    #[test]
    fn produces_a_suggestion_per_shift() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("grafana/oncall-token", "tok")
            .http("https://oncall.example/api/v1/shifts?per_page=5", FIXTURE);
        let out = GrafanaOncallSource.poll(&env, &cfg());
        assert_eq!(out.len(), 2);
        let one = out
            .iter()
            .find(|s| s.title.contains("primary rotation"))
            .unwrap();
        assert_eq!(one.detail.as_deref(), Some("oncall"));
        assert_eq!(one.urgency, Urgency::Normal);
        assert_eq!(one.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn no_secret_or_url_yields_nothing() {
        // Secret + oncall_url both absent → empty Bearer + empty base; the
        // request URL has no fixture registered → http_get returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::GrafanaOncall);
        assert!(GrafanaOncallSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
