//! `k8s-unhealthy` — pods in your current cluster that are wedged
//! (CrashLoopBackOff / ImagePull failures / stuck Pending), surfaced as
//! "go look at this" suggestions. Local `kubectl`, current-context, no auth
//! beyond your kubeconfig.
//!
//! Live wiring: `kubectl get pods -A -o json` → the standard PodList. A pod
//! whose phase is `Pending`, or whose first waiting container reports a
//! back-off reason, becomes a Critical suggestion whose spawn drops you at the
//! code root with a `kubectl … describe pod` ready to run. No `kubectl` / no
//! JSON → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct K8sUnhealthySource;

impl SuggestionSource for K8sUnhealthySource {
    fn kind(&self) -> SourceKind {
        SourceKind::K8sUnhealthy
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let cmd = Cmd::new("kubectl")
            .arg("get")
            .arg("pods")
            .arg("-A")
            .arg("-o")
            .arg("json");
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        parse(&out, env, cfg.max_items.max(1))
    }
}

/// Parse `kubectl get pods -A -o json` into suggestions for unhealthy pods.
/// Pure — the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, cap: usize) -> Vec<Suggestion> {
    let Ok(list) = serde_json::from_str::<PodList>(json) else {
        return Vec::new();
    };
    list.items
        .into_iter()
        .filter_map(|pod| {
            // The first container stuck in `waiting` carries the reason we care
            // about (CrashLoopBackOff / ImagePullBackOff / …).
            let reason = pod
                .status
                .container_statuses
                .iter()
                .find_map(|cs| {
                    let r = cs.state.waiting.reason.trim();
                    if r.is_empty() {
                        None
                    } else {
                        Some(r.to_string())
                    }
                })
                .unwrap_or_default();
            let phase = pod.status.phase.trim();
            let bad_reason = matches!(
                reason.as_str(),
                "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull" | "CreateContainerError"
            );
            if phase != "Pending" && !bad_reason {
                return None;
            }
            let namespace = pod.metadata.namespace.trim();
            let pod_name = pod.metadata.name.trim();
            if pod_name.is_empty() {
                return None;
            }
            let label = if reason.is_empty() {
                phase.to_string()
            } else {
                reason.clone()
            };
            let mut name = String::from("\u{2638} "); // ☸
            name.push_str(&pod_name.chars().take(24).collect::<String>());
            let describe = {
                let mut c = String::from("kubectl -n ");
                c.push_str(namespace);
                c.push_str(" describe pod ");
                c.push_str(pod_name);
                c
            };
            let spawn = SpawnSpec::new(env.code_root(), name)?.with_command(describe);
            let mut key = String::new();
            key.push_str(namespace);
            key.push('/');
            key.push_str(pod_name);
            let mut title = String::new();
            title.push_str(namespace);
            title.push('/');
            title.push_str(pod_name);
            title.push(' ');
            title.push_str(&label);
            Some(
                Suggestion::new(SourceKind::K8sUnhealthy, &key, title, spawn)
                    .detail(label)
                    .urgent(Urgency::Critical),
            )
        })
        .take(cap)
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct PodList {
    #[serde(default)]
    items: Vec<Pod>,
}

#[derive(serde::Deserialize, Default)]
struct Pod {
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
    #[serde(default, rename = "containerStatuses")]
    container_statuses: Vec<ContainerStatus>,
}

#[derive(serde::Deserialize, Default)]
struct ContainerStatus {
    #[serde(default)]
    state: ContainerState,
}

#[derive(serde::Deserialize, Default)]
struct ContainerState {
    #[serde(default)]
    waiting: Waiting,
}

#[derive(serde::Deserialize, Default)]
struct Waiting {
    #[serde(default)]
    reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "items": [
            {
                "metadata": {"name": "api-7d9f", "namespace": "prod"},
                "status": {
                    "phase": "Running",
                    "containerStatuses": [
                        {"state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                    ]
                }
            },
            {
                "metadata": {"name": "web-1", "namespace": "prod"},
                "status": {
                    "phase": "Running",
                    "containerStatuses": [
                        {"state": {"running": {"startedAt": "now"}}}
                    ]
                }
            }
        ]
    }"#;

    #[test]
    fn surfaces_only_unhealthy_pods() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get pods -A -o json", FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::K8sUnhealthy);
        let out = K8sUnhealthySource.poll(&env, &cfg);
        assert_eq!(out.len(), 1, "running pod excluded");
        let pod = &out[0];
        assert!(pod.title.contains("prod/api-7d9f"));
        assert!(pod.title.contains("CrashLoopBackOff"));
        assert_eq!(pod.urgency, Urgency::Critical);
        assert_eq!(pod.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(
            pod.spawn.initial_command(),
            Some("kubectl -n prod describe pod api-7d9f")
        );
    }

    #[test]
    fn no_kubectl_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::K8sUnhealthy);
        assert!(K8sUnhealthySource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
