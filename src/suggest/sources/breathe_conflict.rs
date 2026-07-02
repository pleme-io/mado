//! `breathe-conflict` — breathe resource-homeostasis bands stuck in a Conflict
//! phase (field-ownership contention on the workload's limits), surfaced as
//! "go untangle this" suggestions. Local CLI, no auth.
//!
//! Live wiring: `kubectl get breathebands -A -o json` → `{items:[…]}`. A band
//! whose `status.phase` is `Conflict`, or which carries a `Conflict` condition
//! with `status: "True"`, becomes a suggestion that drops you in the code root.
//! Config params: `context` (optional kubeconfig context the poll is scoped
//! to). Honesty contract: a failed `kubectl` run is `Unavailable(Error)` —
//! only an OBSERVED listing is `Fetched` (garbage JSON parses to empty: the
//! upstream WAS observed), so a dead kubeconfig never reads as "no conflicts".
//! The breathe CRD shape is assumed (metadata.name/namespace +
//! status.phase/conditions).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct BreatheConflictSource;

impl SuggestionSource for BreatheConflictSource {
    fn kind(&self) -> SourceKind {
        SourceKind::BreatheConflict
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let mut cmd = Cmd::new("kubectl")
            .arg("get")
            .arg("breathebands")
            .arg("-A")
            .arg("-o")
            .arg("json");
        // Optional `context` param scopes the poll to a named kubeconfig context.
        if let Some(ctx) = cfg.param("context") {
            cmd = cmd.arg("--context").arg(ctx);
        }
        let Some(json) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        let mut suggestions = parse(&json, env);
        suggestions.truncate(cfg.max_items.max(1));
        PollOutcome::Fetched(suggestions)
    }
}

/// Parse `kubectl get breathebands -o json` into suggestions for Conflict bands.
/// Pure — the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(list) = serde_json::from_str::<BandList>(json) else {
        return Vec::new();
    };
    let cwd = env.code_root();
    list.items
        .into_iter()
        .filter(|band| {
            band.status.phase == "Conflict"
                || band
                    .status
                    .conditions
                    .iter()
                    .any(|c| c.cond_type == "Conflict" && c.status == "True")
        })
        .filter_map(|band| {
            let name = band.metadata.name;
            if name.is_empty() {
                return None;
            }
            let mut session = String::from("\u{1F4A8} "); // 💨
            session.push_str(&name);
            let spawn = SpawnSpec::new(cwd.clone(), session)?;
            let mut key = String::new();
            key.push_str(&band.metadata.namespace);
            key.push('/');
            key.push_str(&name);
            let mut title = String::from("breathe band ");
            title.push_str(&name);
            title.push_str(" in Conflict");
            Some(
                Suggestion::new(SourceKind::BreatheConflict, &key, title, spawn)
                    .detail(band.metadata.namespace)
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct BandList {
    #[serde(default)]
    items: Vec<BandRow>,
}

#[derive(serde::Deserialize, Default)]
struct BandRow {
    #[serde(default)]
    metadata: Meta,
    #[serde(default)]
    status: Status,
}

#[derive(serde::Deserialize, Default)]
struct Meta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

#[derive(serde::Deserialize, Default)]
struct Status {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    conditions: Vec<Condition>,
}

#[derive(serde::Deserialize, Default)]
struct Condition {
    #[serde(default, rename = "type")]
    cond_type: String,
    #[serde(default)]
    status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "items": [
            {"metadata":{"name":"ntfy","namespace":"observability"},"status":{"phase":"Conflict","conditions":[]}},
            {"metadata":{"name":"grafana","namespace":"monitoring"},"status":{"phase":"Ready","conditions":[{"type":"Conflict","status":"True"}]}},
            {"metadata":{"name":"loki","namespace":"monitoring"},"status":{"phase":"Ready","conditions":[{"type":"Ready","status":"True"}]}}
        ]
    }"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get breathebands -A -o json", FIXTURE)
    }

    #[test]
    fn surfaces_only_bands_in_conflict() {
        let cfg = SourceConfig::for_kind(SourceKind::BreatheConflict);
        let PollOutcome::Fetched(out) = BreatheConflictSource.poll(&env(), &cfg) else {
            panic!("an observed listing is Fetched");
        };
        // ntfy (phase Conflict) + grafana (Conflict condition); loki excluded.
        assert_eq!(out.len(), 2, "healthy band excluded");
        let ntfy = out.iter().find(|s| s.title.contains("ntfy")).unwrap();
        assert!(ntfy.title.contains("in Conflict"));
        assert_eq!(ntfy.detail.as_deref(), Some("observability"));
        assert_eq!(ntfy.urgency, Urgency::Normal);
        assert_eq!(ntfy.spawn.cwd().to_str().unwrap(), "/code");
        // A band whose phase is Ready but carries a Conflict=True condition.
        let grafana = out.iter().find(|s| s.title.contains("grafana")).unwrap();
        assert!(grafana.title.contains("in Conflict"));
        assert_eq!(grafana.detail.as_deref(), Some("monitoring"));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No required params/secret here — the only unavailability tier is a
        // failed kubectl run (no fixture → run() returns None → Error, so a
        // dead kubeconfig keeps last-known rows instead of "no conflicts").
        let cfg = SourceConfig::for_kind(SourceKind::BreatheConflict);
        assert_eq!(
            BreatheConflictSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn context_param_scopes_the_kubectl_call() {
        // With `context` set, the --context args land in the typed argv (and
        // thus the mock key): only the contexted fixture answers.
        let mut cfg = SourceConfig::for_kind(SourceKind::BreatheConflict);
        cfg.params.insert("context".into(), "rio".into());
        assert_eq!(
            BreatheConflictSource.poll(&env(), &cfg),
            PollOutcome::error(),
            "the un-contexted fixture must not answer a contexted poll"
        );
        let ctx_env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get breathebands -A -o json --context rio", FIXTURE);
        let PollOutcome::Fetched(out) = BreatheConflictSource.poll(&ctx_env, &cfg) else {
            panic!("the contexted listing is Fetched");
        };
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
