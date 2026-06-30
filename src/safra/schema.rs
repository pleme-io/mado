//! The declared schema borders — typed Rust shapes for the tracked
//! environments + data-kinds a safra curates. The tatara-lisp authoring forms
//! (`deftrackedenv` / `deftrackeddata` / `deferrorsource`, M1) derive from these.
//! See `docs/SAFRA.md` §3.

/// An observability service an environment exposes. Each maps to a concrete
/// source implementation (M2) speaking the right query dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceKind {
    /// VictoriaMetrics (Prometheus-wire-compatible) — PromQL.
    VictoriaMetrics,
    /// Prometheus — PromQL.
    Prometheus,
    /// Grafana (alerts / datasource proxy / grafana-mcp-shaped).
    Grafana,
    /// Datadog (monitors / events).
    Datadog,
    /// Kubernetes API (list + field selectors).
    Kubernetes,
}

impl ServiceKind {
    /// Kebab slug — the stable identity used in config keys + the catalog.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            ServiceKind::VictoriaMetrics => "victoria-metrics",
            ServiceKind::Prometheus => "prometheus",
            ServiceKind::Grafana => "grafana",
            ServiceKind::Datadog => "datadog",
            ServiceKind::Kubernetes => "kubernetes",
        }
    }
}

/// One reachable endpoint on a tracked environment + the secret that
/// authenticates to it. The secret is a `category/name` `SecretRef` (the only
/// secret vocabulary — cofre/SOPS), resolved through
/// `SuggestionEnvironment::secret` at materialization time; never plaintext here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub service: ServiceKind,
    /// Base URL (VM/Prometheus/Grafana/Datadog) or kube-context name (k8s).
    pub base_url: String,
    /// `category/name` SecretRef for auth, or `None` for an unauthenticated
    /// endpoint (e.g. an in-cluster VM with no auth).
    #[serde(default)]
    pub secret_ref: Option<String>,
}

/// A watched environment — the unit a source is scoped to. The tracked set is
/// the Akeyless fleet by default (dev / staging-* / prod-* / cs-* — the
/// portão/shaar registry). `groups` lets the 3-scope config target e.g. all
/// `prod` envs at once.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackedEnvironment {
    pub name: String,
    /// Group memberships for group-scope config (e.g. `["prod", "aws"]`).
    #[serde(default)]
    pub groups: Vec<String>,
    pub endpoints: Vec<Endpoint>,
}

impl TrackedEnvironment {
    /// The endpoint for a given service, if this environment exposes one.
    #[must_use]
    pub fn endpoint(&self, service: ServiceKind) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.service == service)
    }
}

/// A tracked data-type — the schema of one curated structure (firing alerts,
/// unhealthy pods, SLO breaches, …). The query binding + interpreter land in M2;
/// M0 carries the identity + decay that the `CuratedSet` needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackedDataKind {
    /// Stable name / config key (e.g. `firing-alerts`, `oom-kills`).
    pub name: String,
    /// Which service this kind is sourced from.
    pub service: ServiceKind,
    /// Group memberships for group-scope config (e.g. `["alerts", "critical"]`).
    #[serde(default)]
    pub groups: Vec<String>,
    /// Decay TTL (seconds) for this kind's curated set; `0` = never decay.
    pub ttl_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rio() -> TrackedEnvironment {
        TrackedEnvironment {
            name: "rio".into(),
            groups: vec!["homelab".into()],
            endpoints: vec![Endpoint {
                service: ServiceKind::VictoriaMetrics,
                base_url: "http://vmselect.rio:8481".into(),
                secret_ref: None,
            }],
        }
    }

    #[test]
    fn endpoint_lookup_by_service() {
        let e = rio();
        assert!(e.endpoint(ServiceKind::VictoriaMetrics).is_some());
        assert!(e.endpoint(ServiceKind::Datadog).is_none());
    }

    #[test]
    fn service_slugs_are_stable_and_unique() {
        let kinds = [
            ServiceKind::VictoriaMetrics,
            ServiceKind::Prometheus,
            ServiceKind::Grafana,
            ServiceKind::Datadog,
            ServiceKind::Kubernetes,
        ];
        let mut slugs: Vec<&str> = kinds.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), kinds.len(), "every service slug is unique");
    }

    #[test]
    fn schema_round_trips_through_yaml() {
        let env = rio();
        let y = serde_yaml_ng::to_string(&env).unwrap();
        let back: TrackedEnvironment = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(env, back);
    }
}
