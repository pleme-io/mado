//! Alert → prewarm strategy — the on-call intelligence.
//!
//! Given a curated [`Signal`], build the [`PrewarmSpec`] that gets the operator
//! "truly ready to jump into the issue": point kubectl at the right cluster,
//! surface the failing pod, and open the alert's dashboard/runbook. This is the
//! strategy half (mado-state-derived); the executor
//! ([`crate::prewarm::PrewarmEnv`]) runs it in the freshly-spawned session's
//! shell (the scope boundary — see `docs/PREWARM.md`).
//!
//! **Grounded in the real VM Signal shape**, not the idealized one: a firing
//! alert's `identity` is `alertname{k=v,…}` (e.g.
//! `OOMKilled{pod=api-1,severity=critical}`), NOT a bare pod name — so the pod
//! is extracted from the label set best-effort, and the always-correct steps
//! (kube-context from `env`, open the deep-link) never depend on that parse.
//!
//! Every command flows through [`crate::prewarm::PrewarmStep`]'s injection guard
//! at construction, so an alert label carrying a control byte can never build a
//! runnable step (the PTY-injection defense).

use super::signal::Signal;
use crate::prewarm::{PrewarmSpec, PrewarmStep};

/// Extract the pod name from a firing-alert identity's label set, if present.
/// `OOMKilled{pod=api-1,severity=critical}` → `Some("api-1")`. Returns `None`
/// when the identity carries no `pod=` label (then no describe step is added —
/// the strategy degrades to context + link, never a nonsense `kubectl describe
/// pod OOMKilled{…}`).
#[must_use]
fn pod_from_identity(identity: &str) -> Option<&str> {
    let inner = identity
        .split_once('{')?
        .1
        .strip_suffix('}')
        .unwrap_or_default();
    inner
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "pod")
        .map(|(_, v)| v)
}

/// Build the prewarm strategy for a curated signal. The steps, in order:
///
/// 1. `KubeContext(env)` — point kubectl at the incident's cluster (the env
///    name is the context; a richer `Endpoint.base_url` resolution is the M2
///    refinement). Always added for a non-empty env.
/// 2. `describe pod <pod>` — only when a pod is extractable from the identity
///    label set (best-effort; no fragile guess when absent).
/// 3. `OpenUrl(link)` — the alert's dashboard/runbook deep-link, when present
///    and a valid URL.
///
/// An empty `PrewarmSpec` (a bare session) is a valid outcome — a signal with no
/// env, no pod, and no link simply seats you there.
#[must_use]
pub fn prewarm_for_signal(sig: &Signal) -> PrewarmSpec {
    let mut steps: Vec<PrewarmStep> = Vec::with_capacity(3);

    if let Some(step) = PrewarmStep::kube_context(&sig.env) {
        steps.push(step);
    }

    if let Some(pod) = pod_from_identity(&sig.identity) {
        // `describe pod <pod>` — the pod name is a validated label value, but
        // it still re-flows through the step's injection guard (defense in
        // depth: upstream labels are untrusted). Built with push_str, never
        // format!() (TYPED EMISSION).
        let mut cmd = String::with_capacity(19 + pod.len());
        cmd.push_str("kubectl describe pod ");
        cmd.push_str(pod);
        if let Some(step) = PrewarmStep::run(&cmd) {
            steps.push(step);
        }
    }

    if let Some(link) = sig.link.as_deref() {
        if let Ok(url) = url::Url::parse(link) {
            steps.push(PrewarmStep::OpenUrl(url));
        }
    }

    PrewarmSpec::new(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safra::curated::Severity;

    fn oom_signal() -> Signal {
        Signal::new(
            "rio",
            "firing-alerts",
            "OOMKilled{pod=api-1,severity=critical}",
            Severity::Critical,
            "OOMKilled api-1",
        )
        .with_link("https://grafana.rio/d/abc/pods?var-pod=api-1")
    }

    #[test]
    fn pod_extraction_from_the_real_identity_shape() {
        assert_eq!(
            pod_from_identity("OOMKilled{pod=api-1,severity=critical}"),
            Some("api-1")
        );
        assert_eq!(
            pod_from_identity("HighLatency{service=gw,pod=gw-7}"),
            Some("gw-7")
        );
        // No pod label → None (no nonsense describe step).
        assert_eq!(pod_from_identity("DiskFull{node=n1}"), None);
        assert_eq!(pod_from_identity("bareIdentity"), None);
    }

    #[test]
    fn oom_alert_builds_the_full_on_call_strategy() {
        let spec = prewarm_for_signal(&oom_signal());
        let steps = spec.steps();
        assert_eq!(steps.len(), 3, "context + describe + open");
        assert_eq!(steps[0], PrewarmStep::KubeContext("rio".into()));
        assert_eq!(
            steps[1].shell_command().as_deref(),
            Some("kubectl describe pod api-1")
        );
        assert!(matches!(&steps[2], PrewarmStep::OpenUrl(u) if u.as_str().contains("grafana.rio")));
    }

    #[test]
    fn degrades_gracefully_without_pod_or_link() {
        // A signal with an env but no extractable pod + no link → just the
        // kube-context step (never a nonsense describe, never a broken URL).
        let sig = Signal::new(
            "rio",
            "firing-alerts",
            "DiskFull{node=n1}",
            Severity::Warning,
            "disk full",
        );
        let spec = prewarm_for_signal(&sig);
        assert_eq!(spec.steps(), &[PrewarmStep::KubeContext("rio".into())]);
    }

    #[test]
    fn a_malformed_link_is_dropped_not_run() {
        let sig = Signal::new("rio", "firing-alerts", "X{node=n1}", Severity::Info, "x")
            .with_link("not a url");
        let spec = prewarm_for_signal(&sig);
        // Only the kube-context step — the unparseable link is silently dropped.
        assert_eq!(spec.steps(), &[PrewarmStep::KubeContext("rio".into())]);
    }

    #[test]
    fn injection_in_the_pod_label_cannot_build_a_step() {
        // A pod label carrying a newline (an injection attempt) is rejected at
        // the step border — the describe step is simply not added.
        let sig = Signal::new(
            "rio",
            "firing-alerts",
            "X{pod=api\n-rm-rf}",
            Severity::Critical,
            "x",
        );
        let spec = prewarm_for_signal(&sig);
        // Context yes, describe dropped (injection-guarded), no link.
        assert_eq!(spec.steps(), &[PrewarmStep::KubeContext("rio".into())]);
    }
}
