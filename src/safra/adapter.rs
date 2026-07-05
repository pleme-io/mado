//! The safra → suggestion-stream adapter: ONE `izumi::Source` provider
//! (`SourceKind::Safra`) that owns every configured [`SafraCell`], reconciles
//! each on its OWN resolved cadence inside the engine's fan-out poll, and
//! projects the top curated signals onto the Ctrl-S board via
//! [`project_signal`]. This is the SAFRA.md M0 "one live cell" wiring, riding
//! the existing watcher plane — zero new runtime. (The vigy-hosted variant,
//! where cells become tlisp-authored reconcilers, is the later phase; see
//! `docs/SAFRA.md` §5.)
//!
//! **Blind-endpoint semantics (tier-honest):** `CellSource::observe` returns
//! a plain `Vec`, so the adapter cannot distinguish observed-empty from
//! unreachable — projections come from the CURATED set, which retains items
//! until the kind's TTL, so a blind endpoint degrades to slow TTL decay,
//! never an instant wipe. What IS lossy today: the Safra kind's health reads
//! Ok while an endpoint is blind. Typed per-cell observe outcomes (the
//! `PollOutcome` shape pushed down into `CellSource`) are the named M4
//! convergence-hardening item in SAFRA.md §10.

use std::sync::{Arc, Mutex};

use super::cell::SafraCell;
use super::config::SafraConfig;
use super::gha::GhaDeploymentSource;
use super::project::project_signal;
use super::schema::ServiceKind;
use super::source::CellSource;
use super::vm_alerts::VmAlertsSource;
use crate::suggest::core::{SourceKind, Suggestion};
use crate::suggest::env::SuggestionEnvironment;
use crate::suggest::source::{PollOutcome, SourceConfig};

/// How many top-ranked curated items each cell projects per poll. The board's
/// own `per_source_cap` + `max_visible` window downstream; this only bounds
/// the fan-in so one noisy cell can't flood the store slice.
const PROJECT_PER_CELL: usize = 8;

/// One cell plus its last-reconcile stamp (unix seconds) — the adapter honors
/// each cell's resolved cadence inside the engine's coarser fan-out tick.
struct CellSlot {
    cell: SafraCell,
    last_reconcile: u64,
}

/// The `SourceKind::Safra` provider. Holds the configured cells behind a
/// mutex (`poll` reconciles = mutates; the trait is `&self`).
pub struct SafraSuggestionSource {
    cells: Mutex<Vec<CellSlot>>,
}

impl SafraSuggestionSource {
    /// The registry placeholder: no cells. Polls as `Unavailable(Unconfigured)`
    /// so the health surface says "needs config" instead of silently empty.
    #[must_use]
    pub fn unconfigured() -> Self {
        Self {
            cells: Mutex::new(Vec::new()),
        }
    }

    /// Build every enabled (kind × env) cell the config declares whose
    /// service has a shipped [`CellSource`]. Kinds on services without a
    /// source yet are skipped with a debug note (they arrive with the M2
    /// source matrix).
    #[must_use]
    pub fn from_config(cfg: &SafraConfig) -> Self {
        let mut cells = Vec::new();
        for kind in &cfg.kinds {
            let source: Arc<dyn CellSource> = match kind.service {
                ServiceKind::VictoriaMetrics | ServiceKind::Prometheus => Arc::new(VmAlertsSource),
                ServiceKind::GitHubActions => Arc::new(GhaDeploymentSource::new(cfg.gha.clone())),
                ServiceKind::Grafana
                | ServiceKind::VictoriaLogs
                | ServiceKind::Datadog
                | ServiceKind::OpsGenie
                | ServiceKind::Kubernetes => {
                    tracing::debug!(
                        kind = kind.name,
                        service = kind.service.slug(),
                        "safra kind skipped — no shipped CellSource for its service yet (M2)"
                    );
                    continue;
                }
            };
            for env in &cfg.environments {
                if env.endpoint(kind.service).is_none() {
                    continue;
                }
                if !cfg.cell_enabled(&kind.name, &kind.groups, &env.name, &env.groups) {
                    continue;
                }
                let tuning = cfg.resolve(&kind.name, &kind.groups, &env.name, &env.groups);
                cells.push(CellSlot {
                    cell: SafraCell::new(kind.clone(), env.clone(), Arc::clone(&source), tuning),
                    last_reconcile: 0,
                });
            }
        }
        Self {
            cells: Mutex::new(cells),
        }
    }

    /// Configured cell count (observability + tests).
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<CellSlot>> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl izumi::Source<SourceKind, izumi::SpawnSpec> for SafraSuggestionSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Safra
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, _cfg: &SourceConfig) -> PollOutcome {
        let mut cells = self.lock();
        if cells.is_empty() {
            return PollOutcome::unconfigured();
        }
        let now = env.now_unix();
        let root = env.code_root();
        let mut out: Vec<Suggestion> = Vec::new();
        for slot in cells.iter_mut() {
            // Per-cell cadence inside the fan-out: a 30s prod cell re-observes
            // every fan-out tick, a 600s cell only when due — batch efficiency
            // per SAFRA.md §5 without one watcher per cell.
            let due = slot.last_reconcile == 0
                || now.saturating_sub(slot.last_reconcile) >= slot.cell.cadence_secs();
            if due {
                let _ = slot.cell.reconcile(env);
                slot.last_reconcile = now;
            }
            for tracked in slot.cell.ranked().into_iter().take(PROJECT_PER_CELL) {
                if let Some(s) = project_signal(&tracked.item, SourceKind::Safra, &root) {
                    out.push(s);
                }
            }
        }
        PollOutcome::Fetched(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safra::gha::{GhaFilter, TextMatch};
    use crate::safra::schema::{Endpoint, TrackedDataKind, TrackedEnvironment};
    use crate::suggest::env::MockEnvironment;
    use izumi::Source as _;

    const VM_FIXTURE: &str = r#"{
      "data": { "alerts": [
        {"labels":{"alertname":"OOMKilled","severity":"critical"},
         "annotations":{"summary":"api OOM"},"state":"firing"}
      ]}
    }"#;

    fn cfg() -> SafraConfig {
        SafraConfig {
            enabled: true,
            environments: vec![TrackedEnvironment {
                name: "rio".into(),
                groups: vec!["homelab".into()],
                endpoints: vec![Endpoint {
                    service: ServiceKind::VictoriaMetrics,
                    base_url: "http://vm.rio:8481".into(),
                    secret_ref: None,
                }],
            }],
            kinds: vec![TrackedDataKind {
                name: "firing-alerts".into(),
                service: ServiceKind::VictoriaMetrics,
                groups: vec!["alerts".into()],
                ttl_secs: 300,
            }],
            gha: GhaFilter {
                repos: vec![],
                workflow_allow: vec![TextMatch::Contains("Deploy".into())],
                ..GhaFilter::default()
            },
            ..SafraConfig::default()
        }
    }

    #[test]
    fn unconfigured_adapter_reports_needs_config() {
        let a = SafraSuggestionSource::unconfigured();
        let cfg = SourceConfig::for_kind(SourceKind::Safra);
        assert_eq!(
            a.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::unconfigured()
        );
    }

    #[test]
    fn from_config_builds_only_matched_enabled_cells() {
        let a = SafraSuggestionSource::from_config(&cfg());
        assert_eq!(a.cell_count(), 1, "one (kind × env) cell matched");
        // A kind whose service no environment exposes yields no cell.
        let mut c2 = cfg();
        c2.kinds[0].service = ServiceKind::GitHubActions;
        assert_eq!(
            SafraSuggestionSource::from_config(&c2).cell_count(),
            0,
            "no endpoint for the kind's service → no cell"
        );
        // A disabled cell (specific-scope) is skipped.
        let mut c3 = cfg();
        c3.cells.insert(
            SafraConfig::cell_key("firing-alerts", "rio"),
            crate::safra::config::CellTuning {
                enabled: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(SafraSuggestionSource::from_config(&c3).cell_count(), 0);
    }

    #[test]
    fn poll_reconciles_and_projects_board_rows() {
        let a = SafraSuggestionSource::from_config(&cfg());
        let scfg = SourceConfig::for_kind(SourceKind::Safra);
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .http("http://vm.rio:8481/api/v1/alerts", VM_FIXTURE);
        let PollOutcome::Fetched(out) = a.poll(&env, &scfg) else {
            panic!("configured cells → Fetched");
        };
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.source, SourceKind::Safra);
        assert!(s.title.contains("OOMKilled"));
        assert_eq!(s.urgency, crate::suggest::core::Urgency::Critical);
        assert!(s.spawn.name().starts_with("rio"), "session scoped to the env");
        // The projected id is stable across polls (dedup + shade continuity).
        let PollOutcome::Fetched(again) = a.poll(&env, &scfg) else {
            panic!("second poll fetches");
        };
        assert_eq!(again[0].id, s.id);
    }

    #[test]
    fn per_cell_cadence_gates_reconcile_between_fanouts() {
        let a = SafraSuggestionSource::from_config(&cfg());
        let scfg = SourceConfig::for_kind(SourceKind::Safra);
        // First poll at t: observes the alert.
        let env1 = MockEnvironment::new()
            .roots("/code", "/home/op")
            .http("http://vm.rio:8481/api/v1/alerts", VM_FIXTURE)
            .at(1_000);
        let PollOutcome::Fetched(first) = a.poll(&env1, &scfg) else {
            panic!()
        };
        assert_eq!(first.len(), 1);
        // Second poll 10s later WITHOUT the fixture: the cell is not yet due
        // (default cadence 120s), so it does NOT re-observe — the curated
        // truth (and the projection) survives instead of flickering.
        let env2 = MockEnvironment::new().roots("/code", "/home/op").at(1_010);
        let PollOutcome::Fetched(second) = a.poll(&env2, &scfg) else {
            panic!()
        };
        assert_eq!(second.len(), 1, "not-due cell keeps its curated truth");
    }
}
