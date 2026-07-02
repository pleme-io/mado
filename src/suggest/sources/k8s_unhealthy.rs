//! `k8s-unhealthy` — pods in your current cluster that are wedged
//! (CrashLoopBackOff / ImagePull failures / stuck Pending), surfaced as
//! "go look at this" suggestions. Local `kubectl`, current-context, no auth
//! beyond your kubeconfig.
//!
//! Live wiring: `kubectl get pods -A -o json` → the standard PodList. A pod
//! whose phase is `Pending`, or whose first waiting container reports a
//! back-off reason, becomes a suggestion (ranked on the typed [`Rank`] ladder
//! by how loud the failure is) whose spawn drops you at the code root with a
//! `kubectl … describe pod` ready to run. Config params: `context` (optional
//! kubeconfig context the poll is scoped to). Honesty contract: a failed
//! `kubectl` run is `Unavailable(Error)` — only an OBSERVED listing is
//! `Fetched` (garbage JSON parses to empty: the upstream WAS observed), so a
//! dead kubeconfig never reads as "everything healthy".

use crate::suggest::core::{Rank, SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct K8sUnhealthySource;

impl SuggestionSource for K8sUnhealthySource {
    fn kind(&self) -> SourceKind {
        SourceKind::K8sUnhealthy
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let mut cmd = Cmd::new("kubectl")
            .arg("get")
            .arg("pods")
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
        PollOutcome::Fetched(parse(&out, env, cfg.max_items.max(1)))
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
                "CrashLoopBackOff"
                    | "ImagePullBackOff"
                    | "ErrImagePull"
                    | "OOMKilled"
                    | "CreateContainerError"
            );
            if phase != "Pending" && !bad_reason {
                return None;
            }
            let namespace = pod.metadata.namespace.trim();
            let pod_name = pod.metadata.name.trim();
            if pod_name.is_empty() {
                return None;
            }
            // Defense-in-depth: namespace + pod name are interpolated into a
            // shell `kubectl … describe pod` command. k8s guarantees DNS-1123
            // names; reject anything else so a corrupt or hostile PodList can't
            // inject shell syntax through the spawn's initial command.
            if !dns_safe(namespace) || !dns_safe(pod_name) {
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
                    .ranked(severity_rank(&reason, phase)),
            )
        })
        .take(cap)
        .collect()
}

/// Map an unhealthy pod onto the typed [`Rank`] ladder: a crash loop is the
/// loudest thing a cluster can say (top of the Critical tier); image-pull
/// failures and OOM kills are Critical; a stuck Pending is should-look-soon,
/// not a fire. Local to this source — the ladder is k8s-reason-specific.
fn severity_rank(reason: &str, phase: &str) -> Rank {
    match reason {
        "CrashLoopBackOff" => Rank::critical_top(),
        "ImagePullBackOff" | "ErrImagePull" | "OOMKilled" | "CreateContainerError" => {
            Rank::critical()
        }
        _ if phase == "Pending" => Rank::high(),
        _ => Rank::critical(),
    }
}

/// A DNS-1123-safe k8s name: non-empty, ≤253 chars, only `[a-z0-9.-]`. This is
/// the exact character class k8s enforces on namespace + pod names, so a real
/// name always passes and anything carrying shell metacharacters is rejected.
fn dns_safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
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
    use crate::suggest::core::Urgency;
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
                "metadata": {"name": "queued-1", "namespace": "prod"},
                "status": {
                    "phase": "Pending",
                    "containerStatuses": []
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
        let PollOutcome::Fetched(out) = K8sUnhealthySource.poll(&env, &cfg) else {
            panic!("an observed listing is Fetched");
        };
        assert_eq!(out.len(), 2, "running pod excluded");
        let pod = out.iter().find(|s| s.title.contains("api-7d9f")).unwrap();
        assert!(pod.title.contains("prod/api-7d9f"));
        assert!(pod.title.contains("CrashLoopBackOff"));
        assert_eq!(pod.urgency, Urgency::Critical);
        assert_eq!(pod.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(
            pod.spawn.initial_command(),
            Some("kubectl -n prod describe pod api-7d9f")
        );
        // The typed ladder: a crash loop tops the stream; a stuck Pending is
        // should-look-soon (High), ranked strictly below.
        let pending = out.iter().find(|s| s.title.contains("queued-1")).unwrap();
        assert_eq!(pending.urgency, Urgency::High);
        assert!(
            pod.rank_key() > pending.rank_key(),
            "CrashLoopBackOff must rank above a stuck Pending"
        );
    }

    #[test]
    fn severity_ladder_is_typed() {
        assert_eq!(severity_rank("CrashLoopBackOff", "Running"), Rank::critical_top());
        assert_eq!(severity_rank("ImagePullBackOff", "Running"), Rank::critical());
        assert_eq!(severity_rank("ErrImagePull", "Running"), Rank::critical());
        assert_eq!(severity_rank("OOMKilled", "Running"), Rank::critical());
        assert_eq!(severity_rank("", "Pending"), Rank::high());
        // A reason always outranks the bare phase — Pending + crash loop is a
        // crash loop.
        assert_eq!(severity_rank("CrashLoopBackOff", "Pending"), Rank::critical_top());
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No required params/secret here — the only unavailability tier is a
        // failed kubectl run (no fixture → run() returns None → Error, so a
        // dead kubeconfig keeps last-known rows instead of "all healthy").
        let cfg = SourceConfig::for_kind(SourceKind::K8sUnhealthy);
        assert_eq!(
            K8sUnhealthySource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn context_param_scopes_the_kubectl_call() {
        // With `context` set, the --context args land in the typed argv (and
        // thus the mock key): only the contexted fixture answers.
        let mut cfg = SourceConfig::for_kind(SourceKind::K8sUnhealthy);
        cfg.params.insert("context".into(), "rio".into());
        let plain = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get pods -A -o json", FIXTURE);
        assert_eq!(
            K8sUnhealthySource.poll(&plain, &cfg),
            PollOutcome::error(),
            "the un-contexted fixture must not answer a contexted poll"
        );
        let ctx_env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get pods -A -o json --context rio", FIXTURE);
        let PollOutcome::Fetched(out) = K8sUnhealthySource.poll(&ctx_env, &cfg) else {
            panic!("the contexted listing is Fetched");
        };
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }

    #[test]
    fn shell_metachar_names_are_rejected() {
        // A hostile/corrupt PodList carrying shell syntax in the pod name must
        // not produce a suggestion (its describe command would be injectable).
        const HOSTILE: &str = r#"{
            "items": [
                {
                    "metadata": {"name": "x; rm -rf ~", "namespace": "prod"},
                    "status": {
                        "phase": "Pending",
                        "containerStatuses": [
                            {"state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                        ]
                    }
                }
            ]
        }"#;
        let out = parse(HOSTILE, &MockEnvironment::new().roots("/code", "/home/op"), 5);
        assert!(out.is_empty(), "injectable pod name must be skipped");
    }

    #[test]
    fn dns_safe_accepts_real_names_rejects_metachars() {
        assert!(dns_safe("api-7d9f"));
        assert!(dns_safe("kube-system.thing"));
        assert!(!dns_safe(""));
        assert!(!dns_safe("x; rm -rf ~"));
        assert!(!dns_safe("Upper"));
        assert!(!dns_safe("$(whoami)"));
    }
}
