//! `CellSource` — the per-(kind × env) fetch seam. A source observes the current
//! batch of upstream items for one tracked data-kind on one tracked
//! environment's endpoint and normalizes them to [`Signal`]s. The concrete
//! sources (VictoriaMetrics PromQL, Grafana, Datadog, k8s) land in M2; this trait
//! is the abstraction they implement, and the mock here is the test seam (the
//! TYPED-SPEC + INTERPRETER triplet — every source is exercised without a real
//! cluster). See `docs/SAFRA.md` §3, §5.

use super::schema::{Endpoint, TrackedDataKind, TrackedEnvironment};
use super::signal::Signal;
use crate::suggest::env::SuggestionEnvironment;

/// Everything a source needs to observe one cell: which environment + kind, the
/// resolved endpoint to hit, and the already-materialized secret (read once from
/// SOPS via the `SuggestionEnvironment`, never re-resolved per call).
pub struct ObserveCtx<'a> {
    pub env: &'a TrackedEnvironment,
    pub kind: &'a TrackedDataKind,
    pub endpoint: &'a Endpoint,
    /// The endpoint's auth secret, materialized; `None` for an unauthenticated
    /// endpoint or an unmaterialized `SecretRef`.
    pub secret: Option<String>,
}

/// Observe the current batch of one tracked data-kind from one environment's
/// endpoint, normalized to [`Signal`]s. Implementations do all I/O through the
/// `SuggestionEnvironment` (so they are mockable); they return the **full
/// current set** for the cell (batch, not deltas) — the [`CuratedSet`] computes
/// the delta. An empty `Vec` means "observed, nothing firing" (which lets stale
/// items decay); a source that cannot reach its endpoint should also return
/// empty (best-effort, never panics).
///
/// [`CuratedSet`]: super::curated::CuratedSet
pub trait CellSource: Send + Sync {
    /// The service kind this source speaks (must match the cell's endpoint).
    fn service(&self) -> super::schema::ServiceKind;
    /// Observe the current batch for this cell.
    fn observe(&self, env: &dyn SuggestionEnvironment, ctx: &ObserveCtx) -> Vec<Signal>;
}

#[cfg(test)]
pub(crate) mod mock {
    use super::super::schema::ServiceKind;
    use super::*;

    /// A canned source that returns a fixed batch — the test seam for the cell +
    /// engine reconcile loops. Records whether it saw the materialized secret so
    /// tests can assert the SOPS→source plumbing.
    pub struct MockSource {
        pub service: ServiceKind,
        pub batch: Vec<Signal>,
    }

    impl CellSource for MockSource {
        fn service(&self) -> ServiceKind {
            self.service
        }
        fn observe(&self, _env: &dyn SuggestionEnvironment, _ctx: &ObserveCtx) -> Vec<Signal> {
            self.batch.clone()
        }
    }
}
