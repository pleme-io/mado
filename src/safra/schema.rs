//! The declared schema borders — typed Rust shapes for the tracked
//! environments + data-kinds a safra curates. The tatara-lisp authoring forms
//! (`deftrackedenv` / `deftrackeddata` / `deferrorsource`, M1) derive from these.
//! See `docs/SAFRA.md` §3.

/// An observability service an environment exposes. Each maps to a concrete
/// source implementation (M2) speaking the right query dialect.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    pleme_kindstr_derive::KindStr,
)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceKind {
    /// VictoriaMetrics (Prometheus-wire-compatible) — PromQL.
    #[kind(name = "victoria-metrics")]
    VictoriaMetrics,
    /// Prometheus — PromQL.
    #[kind(name = "prometheus")]
    Prometheus,
    /// Grafana (alerts / datasource proxy / grafana-mcp-shaped) — the Lareira
    /// `grafana-rio` MCP surface.
    #[kind(name = "grafana")]
    Grafana,
    /// VictoriaLogs — the Lareira log store (LogsQL error-log spikes).
    #[kind(name = "victoria-logs")]
    VictoriaLogs,
    /// Datadog (monitors / events) — the `mirror`-tap paging surface.
    #[kind(name = "datadog")]
    Datadog,
    /// OpsGenie (open/unacked alerts) — the `mirror`-tap team paging
    /// state (read-only).
    #[kind(name = "opsgenie")]
    OpsGenie,
    /// Kubernetes API (list + field selectors).
    #[kind(name = "kubernetes")]
    Kubernetes,
    /// GitHub Actions — deployment-flow runs that change an environment
    /// (the deploy / promote / failover / config-rollout workflows).
    #[kind(name = "github-actions")]
    GitHubActions,
}

impl ServiceKind {
    /// Kebab slug — the stable identity used in config keys + the catalog.
    /// Thin `self`-taking wrapper over the `#[derive(KindStr)]`-generated
    /// [`Self::as_str`] (one authored `#[kind(name = …)]` table replaces the
    /// former hand-kept match; [`Self::from_str_kind`] is the paired inverse).
    ///
    /// NOTE: the slug is authored explicitly and is deliberately NOT the serde
    /// `rename_all = "kebab-case"` output for `GitHubActions` (serde emits
    /// `git-hub-actions`; the slug is `github-actions`). Both string reps are
    /// preserved byte-for-byte by this refactor — unifying them is a separate,
    /// behaviour-changing decision, out of scope for the derive adoption.
    #[must_use]
    pub fn slug(self) -> &'static str {
        self.as_str()
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
/// whatever fleet the deployment config layer declares (e.g. `dev` /
/// `staging-*` / `prod-*`); mado ships none. `groups` lets the 3-scope config
/// target e.g. all `prod` envs at once.
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
            ServiceKind::VictoriaLogs,
            ServiceKind::Datadog,
            ServiceKind::OpsGenie,
            ServiceKind::Kubernetes,
            ServiceKind::GitHubActions,
        ];
        let mut slugs: Vec<&str> = kinds.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), kinds.len(), "every service slug is unique");
    }

    /// Byte-pin: the `#[derive(KindStr)]` slug table is byte-for-byte the
    /// former hand-written `slug()` match arms (the vt.rs
    /// `csi_matches_the_legacy_format_strings` idiom). If codegen ever drifts
    /// one byte, this fails — and the `github-actions` / serde `git-hub-actions`
    /// divergence stays intentional + pinned.
    #[test]
    fn service_slugs_are_byte_identical_to_the_former_hand_match() {
        assert_eq!(ServiceKind::VictoriaMetrics.slug(), "victoria-metrics");
        assert_eq!(ServiceKind::Prometheus.slug(), "prometheus");
        assert_eq!(ServiceKind::Grafana.slug(), "grafana");
        assert_eq!(ServiceKind::VictoriaLogs.slug(), "victoria-logs");
        assert_eq!(ServiceKind::Datadog.slug(), "datadog");
        assert_eq!(ServiceKind::OpsGenie.slug(), "opsgenie");
        assert_eq!(ServiceKind::Kubernetes.slug(), "kubernetes");
        assert_eq!(ServiceKind::GitHubActions.slug(), "github-actions");
        // The derived paired inverse round-trips every slug.
        assert_eq!(
            ServiceKind::from_str_kind("github-actions"),
            Some(ServiceKind::GitHubActions)
        );
        assert_eq!(
            ServiceKind::from_str_kind("victoria-logs"),
            Some(ServiceKind::VictoriaLogs)
        );
        assert_eq!(ServiceKind::from_str_kind("nope"), None);
    }

    #[test]
    fn schema_round_trips_through_yaml() {
        let env = rio();
        let y = serde_yaml_ng::to_string(&env).unwrap();
        let back: TrackedEnvironment = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(env, back);
    }
}
