//! `Signal` — the universal curated item. Every tracked data-type, regardless of
//! service (a Grafana alert, a VictoriaMetrics PromQL breach, a Datadog monitor,
//! an unhealthy pod), normalizes to one `Signal`: "an error to handle, in an
//! environment you track." This is the unifying abstraction the whole curation
//! engine operates over — sources produce `Signal`s; the `CuratedSet` curates
//! them; Ctrl-S renders them. See `docs/SAFRA.md` §4.

use super::curated::{CuratedItem, Severity};

/// One normalized signal from a tracked data-source. Its `signature` is
/// `<env>:<kind>:<identity>` — so the same upstream incident in two environments
/// is two distinct curated items, and the same incident re-observed is one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signal {
    /// Tracked environment this signal belongs to (e.g. `prod-euw1`).
    pub env: String,
    /// Tracked data-kind (e.g. `firing-alerts`, `oom-kills`).
    pub kind: String,
    /// Upstream-stable identity within (env, kind) — alertname+labels, pod name,
    /// monitor id. Two observations with the same identity are the same incident.
    pub identity: String,
    pub severity: Severity,
    /// Human label for the Ctrl-S row (e.g. `OOMKilled api-gateway`).
    pub label: String,
    /// Optional extra context (the breaching value, the message).
    #[serde(default)]
    pub detail: Option<String>,
    /// Optional deep-link to the source (a Grafana panel, a Datadog monitor URL)
    /// so Enter can jump straight to the upstream view.
    #[serde(default)]
    pub link: Option<String>,
}

impl Signal {
    /// Construct a minimal signal (env, kind, identity, severity, label).
    #[must_use]
    pub fn new(
        env: impl Into<String>,
        kind: impl Into<String>,
        identity: impl Into<String>,
        severity: Severity,
        label: impl Into<String>,
    ) -> Self {
        Self {
            env: env.into(),
            kind: kind.into(),
            identity: identity.into(),
            severity,
            label: label.into(),
            detail: None,
            link: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn with_link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }
}

impl CuratedItem for Signal {
    fn signature(&self) -> String {
        // `<env>:<kind>:<identity>` — built with push_str (internal identity,
        // not emitted output; no `format!`, per TYPED EMISSION).
        let mut s = String::with_capacity(self.env.len() + self.kind.len() + self.identity.len() + 2);
        s.push_str(&self.env);
        s.push(':');
        s.push_str(&self.kind);
        s.push(':');
        s.push_str(&self.identity);
        s
    }

    fn severity(&self) -> Severity {
        self.severity
    }

    fn title(&self) -> String {
        self.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safra::curated::CuratedSet;

    #[test]
    fn signature_scopes_by_env_kind_identity() {
        let a = Signal::new("rio", "alerts", "OOMKilled/api", Severity::Critical, "x");
        let b = Signal::new("prod", "alerts", "OOMKilled/api", Severity::Critical, "x");
        assert_ne!(a.signature(), b.signature(), "same incident, different env = distinct");
        assert_eq!(a.signature(), "rio:alerts:OOMKilled/api");
    }

    #[test]
    fn same_incident_dedups_in_a_curated_set() {
        let mut set = CuratedSet::new(0);
        let s = Signal::new("rio", "alerts", "id1", Severity::Warning, "boom");
        set.converge(vec![s.clone()], 1000);
        let d = set.converge(vec![s], 1060);
        assert_eq!(d.updated, 1, "re-observed → recurrence, not a new row");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn builders_attach_detail_and_link() {
        let s = Signal::new("rio", "alerts", "id", Severity::Info, "lbl")
            .with_detail("value=0.97")
            .with_link("https://grafana/d/abc");
        assert_eq!(s.detail.as_deref(), Some("value=0.97"));
        assert_eq!(s.link.as_deref(), Some("https://grafana/d/abc"));
    }
}
