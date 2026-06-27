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

use super::core::SourceKind;
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

/// A task-suggestion provider. One impl per [`SourceKind`].
pub trait SuggestionSource: Send + Sync {
    /// Which source this is.
    fn kind(&self) -> SourceKind;

    /// Produce the current suggestion set by reading external state through
    /// `env`. MUST be best-effort + pure w.r.t. `env`: an unauthed/missing
    /// dependency returns an empty `Vec`, never an error or panic. `cfg`
    /// carries the per-source knobs (max_items, params).
    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig)
        -> Vec<super::core::Suggestion>;
}

/// The single tested unit: poll a source through the environment and ingest
/// its set into the store. Pure given `(source, env, store, cfg, now_ms)`.
pub fn refresh_once(
    source: &dyn SuggestionSource,
    env: &dyn SuggestionEnvironment,
    store: &SuggestionStore,
    cfg: &SourceConfig,
    now_ms: u64,
) {
    let mut items = source.poll(env, cfg);
    items.truncate(cfg.max_items);
    store.ingest(source.kind(), items, now_ms);
}

/// Engine-wide config: per-source overrides + global decay/visibility knobs.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub per_source: BTreeMap<SourceKind, SourceConfig>,
    /// Entries older than this (ms since last seen) are decayed. 0 disables.
    pub ttl_ms: u64,
    /// Whether a source with no explicit override runs by default.
    pub default_enabled: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            per_source: BTreeMap::new(),
            ttl_ms: 0,
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
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let ttl_ms = cfg.ttl_ms;
        for src in sources {
            let scfg = cfg.config_for(src.kind());
            if !scfg.enabled {
                continue;
            }
            let env = Arc::clone(&env);
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            handles.push(tokio::spawn(async move {
                // First tick fires immediately (interval's default behaviour),
                // so suggestions appear shortly after startup, then on cadence.
                let mut tick = tokio::time::interval(scfg.interval);
                loop {
                    tick.tick().await;
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let now_ms = env.now_unix().saturating_mul(1000);
                    let s = Arc::clone(&src);
                    let e = Arc::clone(&env);
                    let cfg2 = scfg.clone();
                    // poll may block (subprocess / curl) — keep it off the
                    // async reactor thread.
                    let items = tokio::task::spawn_blocking(move || {
                        let mut v = s.poll(e.as_ref(), &cfg2);
                        v.truncate(cfg2.max_items);
                        v
                    })
                    .await
                    .unwrap_or_default();
                    store.ingest(src.kind(), items, now_ms);
                    if ttl_ms > 0 {
                        store.decay(now_ms, ttl_ms);
                    }
                }
            }));
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

    /// A source that echoes one suggestion per line of a fixture command.
    struct FixtureSource;
    impl SuggestionSource for FixtureSource {
        fn kind(&self) -> SourceKind {
            SourceKind::TendRepos
        }
        fn poll(
            &self,
            env: &dyn SuggestionEnvironment,
            _cfg: &SourceConfig,
        ) -> Vec<Suggestion> {
            let out = env.run(&crate::suggest::env::Cmd::new("tend").arg("status")).unwrap_or_default();
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
                .collect()
        }
    }

    #[test]
    fn refresh_once_polls_and_ingests() {
        let env = MockEnvironment::new().cmd("tend status", "mado\ntear\n");
        let store = SuggestionStore::new();
        let cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        refresh_once(&FixtureSource, &env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2);
        assert!(store.ranked(10).iter().any(|s| s.title == "mado"));
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
