//! `SafraCell` — one (tracked-data-kind × tracked-environment) cell: the unit a
//! vigy agent reconciles. It owns the cell's long-term [`CuratedSet`], its
//! [`CellSource`], and its resolved tuning, and exposes one operation —
//! `reconcile` — that observes the current upstream batch and converges the
//! curated set to it. This is the typed "vigy agent rebuilds the curated
//! structure" (boot tick = first reconcile; steady-state = batch-delta).
//! See `docs/SAFRA.md` §5.

use std::sync::Arc;

use super::config::ResolvedTuning;
use super::curated::{CuratedSet, Delta, Tracked};
use super::schema::{TrackedDataKind, TrackedEnvironment};
use super::signal::Signal;
use super::source::{CellSource, ObserveCtx};
use crate::suggest::env::SuggestionEnvironment;

/// A live reconcile cell. Constructed once per enabled (kind × env) at boot;
/// `reconcile` is called on the cell's cadence by its vigy agent.
pub struct SafraCell {
    kind: TrackedDataKind,
    env: TrackedEnvironment,
    source: Arc<dyn CellSource>,
    curated: CuratedSet<Signal>,
    tuning: ResolvedTuning,
}

impl SafraCell {
    /// Build a cell for `(kind × env)` with its source + resolved tuning. The
    /// curated set's decay TTL comes from the kind; the cell is born empty and
    /// rebuilt by the first `reconcile`.
    #[must_use]
    pub fn new(
        kind: TrackedDataKind,
        env: TrackedEnvironment,
        source: Arc<dyn CellSource>,
        tuning: ResolvedTuning,
    ) -> Self {
        let ttl = kind.ttl_secs;
        Self { kind, env, source, curated: CuratedSet::new(ttl), tuning }
    }

    /// The cadence (seconds) this cell's vigy agent should re-reconcile at.
    #[must_use]
    pub fn cadence_secs(&self) -> u64 {
        self.tuning.cadence_secs
    }

    /// Observe the current upstream batch and converge the curated set to it.
    /// Materializes the endpoint secret from SOPS (via the environment) once per
    /// tick, builds the [`ObserveCtx`], asks the source for the full current
    /// batch, and returns the typed [`Delta`]. Best-effort: a cell whose kind has
    /// no matching endpoint on the environment is a no-op (empty observe →
    /// decay-only), never a panic.
    pub fn reconcile(&mut self, env: &dyn SuggestionEnvironment) -> Delta {
        let now = env.now_unix();
        let Some(endpoint) = self.env.endpoint(self.kind.service) else {
            // No endpoint for this kind on this environment — nothing to observe;
            // still age out anything stale.
            return Delta { added: 0, updated: 0, expired: self.curated.decay(now) };
        };
        let secret = endpoint.secret_ref.as_deref().and_then(|r| env.secret(r));
        let ctx = ObserveCtx { env: &self.env, kind: &self.kind, endpoint, secret };
        let observed = self.source.observe(env, &ctx);
        let delta = self.curated.converge(observed, now);
        // Keep the backlog under control: evict the lowest-ranked beyond the cap
        // AFTER convergence, so re-observed items bump recurrence before any
        // eviction (the lowest-ranked are never on the bounded visible board).
        self.curated.cap(self.tuning.max_items);
        delta
    }

    /// The curated items ranked for surfacing (highest-priority first).
    #[must_use]
    pub fn ranked(&self) -> Vec<&Tracked<Signal>> {
        self.curated.ranked()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.curated.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::curated::Severity;
    use super::super::schema::{Endpoint, ServiceKind};
    use super::super::source::mock::MockSource;
    use crate::suggest::env::MockEnvironment;

    fn kind() -> TrackedDataKind {
        TrackedDataKind {
            name: "alerts".into(),
            service: ServiceKind::VictoriaMetrics,
            groups: vec![],
            ttl_secs: 300,
        }
    }

    fn env_rio() -> TrackedEnvironment {
        TrackedEnvironment {
            name: "rio".into(),
            groups: vec![],
            endpoints: vec![Endpoint {
                service: ServiceKind::VictoriaMetrics,
                base_url: "http://vm.rio:8481".into(),
                secret_ref: None,
            }],
        }
    }

    fn tuning() -> ResolvedTuning {
        ResolvedTuning { cadence_secs: 60, max_items: 50, enabled: true, quota_pct: 1.0 }
    }

    fn sig(id: &str, sev: Severity) -> Signal {
        Signal::new("rio", "alerts", id, sev, id)
    }

    #[test]
    fn first_reconcile_rebuilds_from_upstream() {
        let src = Arc::new(MockSource {
            service: ServiceKind::VictoriaMetrics,
            batch: vec![sig("a", Severity::Critical), sig("b", Severity::Warning)],
        });
        let mut cell = SafraCell::new(kind(), env_rio(), src, tuning());
        let d = cell.reconcile(&MockEnvironment::new());
        assert_eq!(d.added, 2, "boot tick rebuilds the curated structure");
        assert_eq!(cell.len(), 2);
        assert_eq!(cell.ranked()[0].item.identity, "a", "critical ranks first");
    }

    #[test]
    fn steady_state_reconcile_bumps_recurrence_only() {
        let src = Arc::new(MockSource {
            service: ServiceKind::VictoriaMetrics,
            batch: vec![sig("a", Severity::Critical)],
        });
        let mut cell = SafraCell::new(kind(), env_rio(), src, tuning());
        cell.reconcile(&MockEnvironment::new());
        let d = cell.reconcile(&MockEnvironment::new());
        assert_eq!(d, Delta { added: 0, updated: 1, expired: 0 }, "unchanged upstream = no churn");
        assert_eq!(cell.ranked()[0].recurrence, 2);
    }

    #[test]
    fn missing_endpoint_is_a_noop_not_a_panic() {
        // A kind whose service the environment doesn't expose → decay-only no-op.
        let datadog_kind = TrackedDataKind { service: ServiceKind::Datadog, ..kind() };
        let src = Arc::new(MockSource { service: ServiceKind::Datadog, batch: vec![sig("x", Severity::Info)] });
        let mut cell = SafraCell::new(datadog_kind, env_rio(), src, tuning());
        let d = cell.reconcile(&MockEnvironment::new());
        assert_eq!(d, Delta::default(), "no rio Datadog endpoint → nothing observed");
        assert_eq!(cell.len(), 0);
    }
}
