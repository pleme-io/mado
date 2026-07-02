//! `flux-failing` — FluxCD Kustomizations and HelmReleases that are not Ready,
//! surfaced as "go fix this reconciliation" suggestions. Local CLI, no auth
//! beyond your existing `kubectl` context.
//!
//! Live wiring: `kubectl get kustomizations,helmreleases -A -o json` → a list
//! of objects, each carrying `status.conditions`. An object with a `Ready`
//! condition whose `status` is `False` becomes a suggestion that drops you in
//! the code root. Config params: `context` (optional kubeconfig context the
//! poll is scoped to). Honesty contract: a failed `kubectl` run is
//! `Unavailable(Error)` — only an OBSERVED listing is `Fetched` (garbage JSON
//! parses to empty: the upstream WAS observed), so a dead kubeconfig never
//! reads as "everything reconciled".

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct FluxFailingSource;

impl SuggestionSource for FluxFailingSource {
    fn kind(&self) -> SourceKind {
        SourceKind::FluxFailing
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let mut cmd = Cmd::new("kubectl")
            .arg("get")
            .arg("kustomizations,helmreleases")
            .arg("-A")
            .arg("-o")
            .arg("json");
        // Optional `context` param scopes the poll to a named kubeconfig context.
        if let Some(ctx) = cfg.param("context") {
            cmd = cmd.arg("--context").arg(ctx);
        }
        let Some(out) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        let mut items = parse(&out, env);
        items.truncate(cfg.max_items.max(1));
        PollOutcome::Fetched(items)
    }
}

/// Parse `kubectl get … -o json` output into suggestions for objects that
/// carry a `Ready=False` condition. Pure — the unit the source is tested
/// through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(list) = serde_json::from_str::<List>(json) else {
        return Vec::new();
    };
    list.items
        .into_iter()
        .filter_map(|item| {
            // Failing iff a Ready condition reports status False.
            let ready = item
                .status
                .conditions
                .iter()
                .find(|c| c.cond_type == "Ready" && c.status == "False")?;
            let detail = if ready.reason.is_empty() {
                ready.message.chars().take(60).collect::<String>()
            } else {
                ready.reason.clone()
            };
            let cwd = env.code_root();
            let mut name = String::from("\u{1F501} "); // 🔁
            name.push_str(&item.metadata.name);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = String::new();
            title.push_str(&item.kind);
            title.push('/');
            title.push_str(&item.metadata.name);
            title.push_str(" not Ready");
            let mut key = String::new();
            key.push_str(&item.metadata.namespace);
            key.push('/');
            key.push_str(&item.kind);
            key.push('/');
            key.push_str(&item.metadata.name);
            Some(
                Suggestion::new(SourceKind::FluxFailing, &key, title, spawn)
                    .detail(detail)
                    .urgent(Urgency::High),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct List {
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(serde::Deserialize, Default)]
struct Item {
    #[serde(default)]
    kind: String,
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
    conditions: Vec<Condition>,
}

#[derive(serde::Deserialize, Default)]
struct Condition {
    #[serde(default, rename = "type")]
    cond_type: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "items": [
            {
                "kind": "Kustomization",
                "metadata": {"name": "apps", "namespace": "flux-system"},
                "status": {"conditions": [
                    {"type": "Ready", "status": "False", "reason": "BuildFailed", "message": "kustomize build failed"}
                ]}
            },
            {
                "kind": "HelmRelease",
                "metadata": {"name": "ntfy", "namespace": "monitoring"},
                "status": {"conditions": [
                    {"type": "Ready", "status": "True", "reason": "ReconciliationSucceeded", "message": "release reconciled"}
                ]}
            }
        ]
    }"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get kustomizations,helmreleases -A -o json", FIXTURE)
    }

    #[test]
    fn surfaces_only_not_ready_objects() {
        let cfg = SourceConfig::for_kind(SourceKind::FluxFailing);
        let PollOutcome::Fetched(out) = FluxFailingSource.poll(&env(), &cfg) else {
            panic!("an observed listing is Fetched");
        };
        assert_eq!(out.len(), 1, "the Ready=True object is excluded");
        let failing = &out[0];
        assert!(failing.title.contains("Kustomization/apps not Ready"));
        assert_eq!(failing.detail.as_deref(), Some("BuildFailed"));
        assert_eq!(failing.urgency, Urgency::High);
        // No per-object path is reported → spawn drops you in the code root.
        assert_eq!(failing.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No required params/secret here — the only unavailability tier is a
        // failed kubectl run (no fixture → run() returns None → Error, so a
        // dead kubeconfig keeps last-known rows instead of "all reconciled").
        let cfg = SourceConfig::for_kind(SourceKind::FluxFailing);
        assert_eq!(
            FluxFailingSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn context_param_scopes_the_kubectl_call() {
        // With `context` set, the --context args land in the typed argv (and
        // thus the mock key): only the contexted fixture answers.
        let mut cfg = SourceConfig::for_kind(SourceKind::FluxFailing);
        cfg.params.insert("context".into(), "rio".into());
        assert_eq!(
            FluxFailingSource.poll(&env(), &cfg),
            PollOutcome::error(),
            "the un-contexted fixture must not answer a contexted poll"
        );
        let ctx_env = MockEnvironment::new().roots("/code", "/home/op").cmd(
            "kubectl get kustomizations,helmreleases -A -o json --context rio",
            FIXTURE,
        );
        let PollOutcome::Fetched(out) = FluxFailingSource.poll(&ctx_env, &cfg) else {
            panic!("the contexted listing is Fetched");
        };
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
