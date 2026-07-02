//! The source provider plane + the parallel watcher engine.
//!
//! A [`SuggestionSource`] turns external state (read through the
//! [`SuggestionEnvironment`]) into a set of [`Suggestion`]s. Every source is
//! pure w.r.t. the environment, so [`refresh_once`] — `poll` → `store.ingest`
//! — is the single unit each source is tested through (with a
//! [`MockEnvironment`](super::env::MockEnvironment)).
//!
//! The [`SuggestionEngine`] is the "always-updating in-memory daemon with
//! parallel refreshing sources": it spawns ONE tokio task per enabled source,
//! each ticking on its own cadence, each calling `refresh_once` on the
//! tokio blocking pool (so a subprocess/HTTP poll never stalls the runtime),
//! every tick releasing its update into the shared [`SuggestionStore`]. The
//! picker reads the store; the engine never blocks the GUI.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::core::{SourceKind, SourceStatus, Suggestion};
use super::env::SuggestionEnvironment;
use super::store::SuggestionStore;

/// Per-source runtime config — the typed knobs the shikumi `suggestions`
/// block resolves into. `params` is an open per-kind map (token env override,
/// jira JQL, grafana folder, …); typed-per-kind config is a later refinement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub max_items: usize,
    pub params: BTreeMap<String, String>,
}

impl SourceConfig {
    /// Sensible defaults derived from the source kind.
    #[must_use]
    pub fn for_kind(kind: SourceKind) -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(kind.default_interval_secs()),
            max_items: 5,
            params: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }
}

/// The typed outcome of one poll — the honest border between a provider and
/// the store. `Fetched` means the upstream WAS observed and the carried set
/// is the complete current truth for this source (empty = genuinely nothing —
/// resolved items decay off). `Unavailable` means the upstream COULD NOT be
/// observed (missing param, missing credential, network/tool failure): the
/// store keeps the source's last-known rows (TTL ages them out) and records
/// the health state, so an infrastructure blip never wipes the board or
/// masquerades as "everything resolved".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollOutcome {
    Fetched(Vec<Suggestion>),
    Unavailable(SourceStatus),
}

impl PollOutcome {
    /// The source needs a param (site/base_url/…) the config doesn't supply.
    #[must_use]
    pub const fn unconfigured() -> Self {
        Self::Unavailable(SourceStatus::Unconfigured)
    }
    /// The source's credential/secret is absent.
    #[must_use]
    pub const fn auth_missing() -> Self {
        Self::Unavailable(SourceStatus::AuthMissing)
    }
    /// The fetch/parse failed (network, timeout, tool exit, bad shape).
    #[must_use]
    pub const fn error() -> Self {
        Self::Unavailable(SourceStatus::Error)
    }
}

/// A task-suggestion provider. One impl per [`SourceKind`].
pub trait SuggestionSource: Send + Sync {
    /// Which source this is.
    fn kind(&self) -> SourceKind;

    /// Produce the current suggestion set by reading external state through
    /// `env`. MUST be best-effort + pure w.r.t. `env` and MUST NOT panic.
    /// Honesty contract: return [`PollOutcome::Fetched`] ONLY when the
    /// upstream was actually observed (an observed-empty set is `Fetched` of
    /// an empty `Vec`); a missing param / credential / tool / failed fetch is
    /// the matching [`PollOutcome::Unavailable`] tier. `cfg` carries the
    /// per-source knobs (max_items, params).
    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome;
}

/// Apply one poll's outcome to the store — the shared sink the engine's
/// watcher tasks and [`refresh_once`] both flow through. A `Fetched` set is
/// rank-sorted BEFORE the `max_items` truncation (so a provider emitting its
/// worst item last never loses its best), then ingested; an `Unavailable`
/// records health and leaves the last-known rows in place.
///
/// Dismissed rows are exempt from the `max_items` budget: they are invisible
/// on the board, so a dismissed top-ranked issue must not crowd offerable
/// rows out of the store — but they still ride along in the ingest so their
/// Dismissed state persists on the live entry (stickiness).
pub fn apply_poll(
    kind: SourceKind,
    outcome: PollOutcome,
    store: &SuggestionStore,
    cfg: &SourceConfig,
    now_ms: u64,
) {
    match outcome {
        PollOutcome::Fetched(items) => {
            let (dismissed, mut live): (Vec<_>, Vec<_>) =
                items.into_iter().partition(|s| store.is_dismissed(s.id));
            live.sort_by(|a, b| b.rank_key().cmp(&a.rank_key()));
            live.truncate(cfg.max_items);
            live.extend(dismissed);
            store.ingest(kind, live, now_ms);
            store.record_poll(kind, SourceStatus::Ok, now_ms);
        }
        PollOutcome::Unavailable(status) => {
            store.record_poll(kind, status, now_ms);
        }
    }
}

/// The single tested unit: poll a source through the environment and apply
/// the outcome to the store. Pure given `(source, env, store, cfg, now_ms)`.
pub fn refresh_once(
    source: &dyn SuggestionSource,
    env: &dyn SuggestionEnvironment,
    store: &SuggestionStore,
    cfg: &SourceConfig,
    now_ms: u64,
) {
    let outcome = source.poll(env, cfg);
    apply_poll(source.kind(), outcome, store, cfg, now_ms);
}

/// Engine-wide config: per-source overrides + global decay/visibility knobs.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub per_source: BTreeMap<SourceKind, SourceConfig>,
    /// Whether a source with no explicit override runs by default.
    pub default_enabled: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            per_source: BTreeMap::new(),
            default_enabled: true,
        }
    }
}

impl EngineConfig {
    /// The effective config for a kind: an explicit override wins, else the
    /// kind's defaults with `enabled = default_enabled`.
    #[must_use]
    pub fn config_for(&self, kind: SourceKind) -> SourceConfig {
        self.per_source.get(&kind).cloned().unwrap_or_else(|| {
            let mut c = SourceConfig::for_kind(kind);
            c.enabled = self.default_enabled;
            c
        })
    }
}

/// The parallel watcher engine — one tokio task per enabled source.
pub struct SuggestionEngine {
    handles: Vec<tokio::task::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl SuggestionEngine {
    /// Spawn one watcher task per enabled source. Must be called from within a
    /// tokio runtime (mado's, the same one the vigy host uses).
    #[must_use]
    pub fn start(
        sources: Vec<Arc<dyn SuggestionSource>>,
        env: Arc<dyn SuggestionEnvironment>,
        store: Arc<SuggestionStore>,
        cfg: EngineConfig,
    ) -> Self {
        let stop: crate::livestream::StopFlag = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for src in sources {
            let scfg = cfg.config_for(src.kind());
            if !scfg.enabled {
                continue;
            }
            let kind = src.kind();
            // Stage 1 — one continuous refresh loop per source, driven by the
            // reusable `livestream::spawn_interval_refresh`. Each tick re-polls
            // the source (off the async reactor thread — poll may block on a
            // subprocess/HTTP) and pushes its typed outcome into the shared
            // store, so a source's data is re-fetched continuously, never once.
            //
            // The refresher captures the source+env+cfg; the sink computes a
            // fresh `now_ms` per tick and applies the outcome (rank-sort +
            // truncate + ingest on Fetched; health-only on Unavailable — the
            // last-known rows survive a blip). A panic is likewise preserved
            // rather than wiping the source's slice.
            let refresh: Arc<dyn Fn() -> PollOutcome + Send + Sync> = {
                let s = Arc::clone(&src);
                let e = Arc::clone(&env);
                let cfg2 = scfg.clone();
                Arc::new(move || s.poll(e.as_ref(), &cfg2))
            };
            let sink = {
                let store = Arc::clone(&store);
                let env = Arc::clone(&env);
                let cfg2 = scfg.clone();
                move |outcome: PollOutcome| {
                    let now_ms = env.now_unix().saturating_mul(1000);
                    apply_poll(kind, outcome, &store, &cfg2, now_ms);
                }
            };
            let on_panic = move || {
                // The contract says poll must not panic. If it did, LOG it
                // (don't swallow silently) and PRESERVE the last-known rows —
                // an ingest of the empty default would wipe this source's whole
                // slice every tick.
                tracing::warn!(
                    kind = ?kind,
                    "suggestion source panicked; keeping last-known rows"
                );
            };
            // Freshness nudge: Ctrl-S opening fires the shared notify so every
            // watcher whose data is older than its pacing gap re-polls RIGHT
            // NOW — the board you open onto is being re-verified at that
            // moment. The per-watcher gap (interval/4, clamped 5s..60s) makes
            // the pacing structural: a nudge storm cannot hammer an API.
            let min_gap = (scfg.interval / 4)
                .max(Duration::from_secs(5))
                .min(Duration::from_secs(60));
            handles.push(crate::livestream::spawn_interval_refresh_nudged(
                scfg.interval,
                Arc::clone(&stop),
                refresh,
                sink,
                on_panic,
                Some((super::board_nudge(), min_gap)),
            ));
        }
        Self { handles, stop }
    }

    /// Signal every watcher to stop + abort the tasks.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in &self.handles {
            h.abort();
        }
    }

    #[must_use]
    pub fn active_watchers(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for SuggestionEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::{SpawnSpec, Suggestion};
    use crate::suggest::env::MockEnvironment;

    /// A source that echoes one suggestion per line of a fixture command,
    /// honoring the honesty contract (a failed run = `Unavailable`).
    struct FixtureSource;
    impl SuggestionSource for FixtureSource {
        fn kind(&self) -> SourceKind {
            SourceKind::TendRepos
        }
        fn poll(&self, env: &dyn SuggestionEnvironment, _cfg: &SourceConfig) -> PollOutcome {
            let Some(out) = env.run(&crate::suggest::env::Cmd::new("tend").arg("status")) else {
                return PollOutcome::error();
            };
            PollOutcome::Fetched(
                out.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|repo| {
                        Suggestion::new(
                            SourceKind::TendRepos,
                            repo,
                            repo,
                            SpawnSpec::new("/code", repo).unwrap(),
                        )
                    })
                    .collect(),
            )
        }
    }

    #[test]
    fn refresh_once_polls_and_ingests() {
        let env = MockEnvironment::new().cmd("tend status", "mado\ntear\n");
        let store = SuggestionStore::new();
        let cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        refresh_once(&FixtureSource, &env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2);
        assert!(store.ranked(10, 1000).iter().any(|s| s.title == "mado"));
        let health = store.health();
        assert!(
            health
                .iter()
                .any(|(k, h)| *k == SourceKind::TendRepos
                    && h.status == crate::suggest::core::SourceStatus::Ok),
            "an observed poll records Ok health"
        );
    }

    #[test]
    fn refresh_once_truncates_to_max_items() {
        let env = MockEnvironment::new().cmd("tend status", "a\nb\nc\nd\ne\n");
        let store = SuggestionStore::new();
        let mut cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        cfg.max_items = 2;
        refresh_once(&FixtureSource, &env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2, "capped to max_items");
    }

    #[test]
    fn unavailable_keeps_last_known_rows_and_records_health() {
        // First poll observes two repos; the second poll's upstream is gone
        // (no cmd fixture → run returns None → Unavailable). The board keeps
        // the last-known rows instead of flickering empty, and health flips.
        let store = SuggestionStore::new();
        let cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        let ok_env = MockEnvironment::new().cmd("tend status", "mado\ntear\n");
        refresh_once(&FixtureSource, &ok_env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2);

        let dead_env = MockEnvironment::new();
        refresh_once(&FixtureSource, &dead_env, &store, &cfg, 2000);
        assert_eq!(store.len(), 2, "a fetch failure never wipes the board");
        let health = store.health();
        let (_, h) = health
            .iter()
            .find(|(k, _)| *k == SourceKind::TendRepos)
            .expect("health recorded");
        assert_eq!(h.status, crate::suggest::core::SourceStatus::Error);
        assert_eq!(h.last_ok_ms, 1000, "last good observation remembered");

        // A later OBSERVED-empty poll is the truth → rows genuinely resolve.
        let empty_env = MockEnvironment::new().cmd("tend status", "");
        refresh_once(&FixtureSource, &empty_env, &store, &cfg, 3000);
        assert_eq!(store.len(), 0, "observed-empty means resolved");
    }

    #[test]
    fn dismissed_rows_do_not_consume_the_max_items_budget() {
        use crate::suggest::core::{SuggestionId, Urgency};
        let store = SuggestionStore::new();
        let mut cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        cfg.max_items = 2;
        let mk = |k: &str, u: Urgency| {
            Suggestion::new(SourceKind::TendRepos, k, k, SpawnSpec::new("/code", k).unwrap())
                .urgent(u)
        };
        // Round 1: two hot rows fill the budget; the operator dismisses both.
        apply_poll(
            SourceKind::TendRepos,
            PollOutcome::Fetched(vec![
                mk("hot1", Urgency::Critical),
                mk("hot2", Urgency::Critical),
            ]),
            &store,
            &cfg,
            1000,
        );
        assert!(store.dismiss(SuggestionId::derive(SourceKind::TendRepos, "hot1")));
        assert!(store.dismiss(SuggestionId::derive(SourceKind::TendRepos, "hot2")));
        // Round 2: upstream still reports the dismissed pair PLUS two calmer
        // offerable rows. The dismissed pair must not eat the budget — both
        // offerable rows reach the board, and the dismissals stay sticky.
        apply_poll(
            SourceKind::TendRepos,
            PollOutcome::Fetched(vec![
                mk("hot1", Urgency::Critical),
                mk("hot2", Urgency::Critical),
                mk("calm1", Urgency::Normal),
                mk("calm2", Urgency::Normal),
            ]),
            &store,
            &cfg,
            2000,
        );
        let board = store.ranked(10, 2000);
        assert_eq!(board.len(), 2, "both offerable rows surfaced");
        assert!(board.iter().all(|s| s.title.starts_with("calm")));
        assert!(
            store.is_dismissed(SuggestionId::derive(SourceKind::TendRepos, "hot1")),
            "dismissal survives riding along outside the budget"
        );
    }

    #[test]
    fn apply_poll_rank_sorts_before_truncation() {
        // A provider emitting its BEST item last must not lose it to the
        // max_items cut — apply_poll sorts by rank first.
        let store = SuggestionStore::new();
        let mut cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        cfg.max_items = 1;
        let low = Suggestion::new(
            SourceKind::TendRepos,
            "low",
            "low",
            SpawnSpec::new("/code", "low").unwrap(),
        )
        .urgent(crate::suggest::core::Urgency::Low);
        let hot = Suggestion::new(
            SourceKind::TendRepos,
            "hot",
            "hot",
            SpawnSpec::new("/code", "hot").unwrap(),
        )
        .urgent(crate::suggest::core::Urgency::Critical);
        apply_poll(
            SourceKind::TendRepos,
            PollOutcome::Fetched(vec![low, hot]),
            &store,
            &cfg,
            1000,
        );
        let ranked = store.ranked(10, 1000);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].title, "hot", "the best item survives the cap");
    }

    #[test]
    fn disabled_source_is_skipped_by_config_for() {
        let mut cfg = EngineConfig::default();
        let mut sc = SourceConfig::for_kind(SourceKind::TendRepos);
        sc.enabled = false;
        cfg.per_source.insert(SourceKind::TendRepos, sc);
        assert!(!cfg.config_for(SourceKind::TendRepos).enabled);
        // An unconfigured source falls back to enabled defaults.
        assert!(cfg.config_for(SourceKind::GitBranchPr).enabled);
    }
}
