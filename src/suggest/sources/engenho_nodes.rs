//! `engenho-nodes` — Kubernetes nodes that aren't Ready, surfaced as "go look
//! at this node" suggestions. Local CLI, no auth beyond your kubeconfig.
//!
//! Live wiring: `kubectl get nodes -o json` → `{items:[{metadata:{name},
//! status:{conditions:[{type,status}]}}]}`. A node carrying a `Ready`
//! condition whose status is not `True` becomes a suggestion that drops you in
//! your code root. Config params: `context` (optional kubeconfig context the
//! poll is scoped to). Honesty contract: a failed `kubectl` run is
//! `Unavailable(Error)` — only an OBSERVED listing is `Fetched` (garbage JSON
//! parses to empty: the upstream WAS observed), so an unreachable cluster
//! never reads as "all nodes Ready".

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct EngenhoNodesSource;

impl SuggestionSource for EngenhoNodesSource {
    fn kind(&self) -> SourceKind {
        SourceKind::EngenhoNodes
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let mut cmd = Cmd::new("kubectl")
            .arg("get")
            .arg("nodes")
            .arg("-o")
            .arg("json");
        // Optional `context` param scopes the poll to a named kubeconfig context.
        if let Some(ctx) = cfg.param("context") {
            cmd = cmd.arg("--context").arg(ctx);
        }
        let Some(out) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env, cfg))
    }
}

/// Parse `kubectl get nodes -o json` into suggestions for not-Ready nodes.
/// Pure (modulo the code-root lookup) — the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
    let Ok(list) = serde_json::from_str::<NodeList>(json) else {
        return Vec::new();
    };
    let cap = cfg.max_items.max(1);
    list.items
        .into_iter()
        .filter(Node::not_ready)
        .filter_map(|node| {
            let node_name = node.metadata.name;
            if node_name.is_empty() {
                return None;
            }
            let cwd = env.code_root();
            let mut name = String::from("\u{1F5A5} "); // 🖥
            name.push_str(&node_name);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = String::from("node ");
            title.push_str(&node_name);
            title.push_str(" not Ready");
            Some(
                Suggestion::new(SourceKind::EngenhoNodes, &node_name, title, spawn)
                    .detail("kubernetes")
                    .urgent(Urgency::Normal),
            )
        })
        .take(cap)
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct NodeList {
    #[serde(default)]
    items: Vec<Node>,
}

#[derive(serde::Deserialize, Default)]
struct Node {
    #[serde(default)]
    metadata: NodeMeta,
    #[serde(default)]
    status: NodeStatus,
}

#[derive(serde::Deserialize, Default)]
struct NodeMeta {
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize, Default)]
struct NodeStatus {
    #[serde(default)]
    conditions: Vec<NodeCondition>,
}

#[derive(serde::Deserialize, Default)]
struct NodeCondition {
    #[serde(default, rename = "type")]
    cond_type: String,
    #[serde(default)]
    status: String,
}

impl Node {
    fn not_ready(&self) -> bool {
        self.status
            .conditions
            .iter()
            .any(|c| c.cond_type == "Ready" && c.status != "True")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "items": [
            {"metadata":{"name":"rio"},"status":{"conditions":[
                {"type":"MemoryPressure","status":"False"},
                {"type":"Ready","status":"True"}
            ]}},
            {"metadata":{"name":"zek"},"status":{"conditions":[
                {"type":"Ready","status":"False"}
            ]}}
        ]
    }"#;

    #[test]
    fn surfaces_only_not_ready_nodes() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get nodes -o json", FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::EngenhoNodes);
        let PollOutcome::Fetched(out) = EngenhoNodesSource.poll(&env, &cfg) else {
            panic!("an observed listing is Fetched");
        };
        assert_eq!(out.len(), 1, "Ready node excluded");
        let zek = &out[0];
        assert!(zek.title.contains("zek") && zek.title.contains("not Ready"));
        assert_eq!(zek.detail.as_deref(), Some("kubernetes"));
        assert_eq!(zek.urgency, Urgency::Normal);
        // Not-Ready nodes spawn into the code root.
        assert_eq!(zek.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No required params/secret here — the only unavailability tier is a
        // failed kubectl run (no fixture → run() returns None → Error, so an
        // unreachable cluster keeps last-known rows instead of "all Ready").
        let cfg = SourceConfig::for_kind(SourceKind::EngenhoNodes);
        assert_eq!(
            EngenhoNodesSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn context_param_scopes_the_kubectl_call() {
        // With `context` set, the --context args land in the typed argv (and
        // thus the mock key): only the contexted fixture answers.
        let mut cfg = SourceConfig::for_kind(SourceKind::EngenhoNodes);
        cfg.params.insert("context".into(), "rio".into());
        let plain = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get nodes -o json", FIXTURE);
        assert_eq!(
            EngenhoNodesSource.poll(&plain, &cfg),
            PollOutcome::error(),
            "the un-contexted fixture must not answer a contexted poll"
        );
        let ctx_env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get nodes -o json --context rio", FIXTURE);
        let PollOutcome::Fetched(out) = EngenhoNodesSource.poll(&ctx_env, &cfg) else {
            panic!("the contexted listing is Fetched");
        };
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn garbage_json_is_safe() {
        let env = MockEnvironment::new().roots("/code", "/home/op");
        let cfg = SourceConfig::for_kind(SourceKind::EngenhoNodes);
        assert!(parse("not json", &env, &cfg).is_empty());
    }
}
