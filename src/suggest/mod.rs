//! The mado **suggestion stream** — a continuously-refreshing, gently
//! shaded-in stack of "what you could start working on right now" the Ctrl-S
//! session picker surfaces beneath the live + preset rows.
//!
//! ## Shape (the destination, named first)
//!
//! Open Ctrl-S any time and the lower band of the picker is a living,
//! self-refreshing list of task pointers — a PR awaiting your review, a sprint
//! ticket, a failing Flux/k8s resource, a dirty repo, a Cursor agent needing
//! follow-up, an incident firing — each ranked by urgency + score, each
//! **Enter-spawns a pre-named session aimed at that task**. Sources are
//! shikumi config; adding one is one [`SuggestionSource`] impl. Nothing dead
//! or duplicate is ever offered. The terminal proposes the next task before
//! you go looking — flow dominance.
//!
//! ## Layering (reuse-first, no-overlap)
//!
//! * [`core`] — the typed values ([`Suggestion`], [`SourceKind`] catalog,
//!   [`SpawnSpec`], [`Urgency`]). An always-spawnable suggestion is the
//!   in-mado twin of praça's latent `SessionDefinition`; it lifts into praça
//!   (`SessionOrigin::Suggested`) at M3.
//! * [`env`] — the mockable I/O boundary every source reads through (real =
//!   typed `Command`/`curl`/fs/secrets; tests = canned fixtures).
//! * [`store`] — the ephemeral ranked cache the watchers feed + the picker
//!   reads (separate from persisted presets; shade-in birth times live here).
//! * [`source`] — the [`SuggestionSource`] trait + the parallel
//!   [`SuggestionEngine`] watcher plane (one tokio task per source).
//! * [`sources`] — one provider impl per [`SourceKind`].
//!
//! Each source is the TYPED-SPEC+INTERPRETER triplet's interpreter: a pure
//! `poll(env)` tested through a `MockEnvironment`, with the real `Environment`
//! the only live-wiring seam.

// The suggestion plane is an actively-built substrate the binary consumes
// thinly: several typed-API items (HTTP env, secret resolution, decay/snapshot,
// per-item scoring, the initial-command spawn hint) are exercised by the source
// providers + later milestones (shade-in render, warm-restart persist) and by
// the per-module test suites, but not yet by a non-test build of the binary.
// Allow dead_code module-wide while the plane fills out rather than scatter
// per-item allows that churn as each milestone lands.
#![allow(dead_code)]

pub mod core;
pub mod env;
pub mod source;
pub mod sources;
pub mod store;

// The suggest-plane facade. These are the module's public API; not every name
// is consumed by the binary itself (several are used cross-module only under
// cfg(test) or by providers via their full paths), so the unused-import lint
// for the re-export surface is intentionally allowed.
#[allow(unused_imports)]
pub use core::{SourceKind, SourceStatus, SpawnSpec, Suggestion, SuggestionId, Urgency};
#[allow(unused_imports)]
pub use env::{Cmd, HttpReq, MockEnvironment, RealEnvironment, SuggestionEnvironment};
#[allow(unused_imports)]
pub use source::{
    refresh_once, EngineConfig, PollOutcome, SourceConfig, SuggestionEngine, SuggestionSource,
};
#[allow(unused_imports)]
pub use store::{
    shade_ramp, SourceHealth, StoreSnapshot, StoredSuggestion, SuggestionState, SuggestionStore,
};

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// The process-wide suggestion cache — created lazily on first access. The
/// watcher engine (started on its own runtime thread, like vigy) fills it; the
/// Ctrl-S picker bridge reads it. One store, many readers — never a fork.
static STORE: OnceLock<Arc<SuggestionStore>> = OnceLock::new();

/// The shared suggestion store handle (lazy global). Always available; empty
/// until the engine runs (so a disabled engine simply yields no rows).
#[must_use]
pub fn store() -> Arc<SuggestionStore> {
    Arc::clone(STORE.get_or_init(|| Arc::new(SuggestionStore::new())))
}

/// The process-wide freshness nudge every watcher selects on alongside its
/// cadence tick (see `spawn_interval_refresh_nudged`).
static NUDGE: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

/// The shared board-refresh notify (lazy global) — watchers wait on it, the
/// GUI fires it.
#[must_use]
pub fn board_nudge() -> Arc<tokio::sync::Notify> {
    Arc::clone(NUDGE.get_or_init(|| Arc::new(tokio::sync::Notify::new())))
}

/// Ask every suggestion watcher to refresh EARLY (paced per-watcher — a
/// watcher inside its pacing gap absorbs the nudge). Fired when the Ctrl-S
/// board opens, so the operator opens onto data being re-verified right now
/// instead of whenever the next interval happens to land. Sync-callable from
/// the GUI thread; wakes only parked watchers, never blocks.
pub fn request_board_refresh() {
    board_nudge().notify_waiters();
}

/// Resolve the warm-restart snapshot path. `$MADO_SUGGEST_DB` is an explicit
/// override; else `$MADO_STATE_DIR` (tests inject a temp dir); else the OS
/// state dir (`~/.local/state` on Linux — warm-restart data is operator-
/// meaningful *state*, not throwaway cache), falling back to the data dir, then
/// the temp dir. The file lives under a `mado/` subdir.
#[must_use]
pub fn state_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MADO_SUGGEST_DB") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("MADO_STATE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("mado").join("suggestions.json")
}

/// Translate the typed shikumi `suggestions` config into an [`EngineConfig`].
/// Reads the MERGED source list (prescribed arm-list ⊕ operator overrides), so
/// a params-only yaml override never disarms the rest of the surface.
#[must_use]
pub fn engine_config_from(cfg: &crate::config::SuggestionsConfig) -> EngineConfig {
    let mut ec = EngineConfig {
        per_source: std::collections::BTreeMap::new(),
        default_enabled: cfg.default_enabled,
    };
    for s in &cfg.effective_sources() {
        if let Some(kind) = SourceKind::from_slug(&s.kind) {
            let mut sc = SourceConfig::for_kind(kind);
            sc.enabled = s.enabled;
            if let Some(iv) = s.interval_secs {
                sc.interval = std::time::Duration::from_secs(iv.max(1));
            }
            if let Some(mx) = s.max_items {
                sc.max_items = mx;
            }
            sc.params = s.params.clone();
            ec.per_source.insert(kind, sc);
        }
    }
    ec
}

/// Spawn the parallel watcher engine on its own multi-thread tokio runtime
/// thread — mirrors the vigy runtime (the GUI thread is not async). Gated by
/// `suggestions.enabled`; best-effort (a runtime build failure logs + leaves
/// the store empty, so the picker simply shows no suggestions). The safra
/// config builds the curated-observability adapter (cells + endpoints) that
/// replaces the registry's unconfigured placeholder when enabled.
pub fn spawn_engine_thread(
    cfg: &crate::config::SuggestionsConfig,
    safra: &crate::safra::SafraConfig,
) {
    if !cfg.enabled {
        tracing::debug!("suggestion stream disabled (suggestions.enabled = false)");
        return;
    }
    let safra_cfg = safra.clone();
    let engine_cfg = engine_config_from(cfg);
    let global_ttl_ms = cfg.ttl_secs.saturating_mul(1000);
    // Per-source TTL: a source's items live for max(3× its poll interval, the
    // global floor) — so a slow (e.g. hourly) source never flickers under a fast
    // global TTL. Built here, before `engine_cfg` moves into `start`.
    let mut ttl_map: std::collections::BTreeMap<SourceKind, u64> = std::collections::BTreeMap::new();
    for &kind in SourceKind::ALL {
        let interval_ttl = engine_cfg
            .config_for(kind)
            .interval
            .as_secs()
            .saturating_mul(3)
            .saturating_mul(1000);
        ttl_map.insert(kind, interval_ttl.max(global_ttl_ms));
    }
    let persist = cfg.persist;
    let max_entries = cfg.max_entries;
    // 0 = "persist on every change" → a 1s minimum tick (tokio rejects a 0
    // interval); otherwise coalesce writes to this cadence.
    let debounce = std::time::Duration::from_secs(cfg.persist_debounce_secs.max(1));
    let res = std::thread::Builder::new()
        .name("mado-suggest".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("suggest-tokio")
                .build()
            {
                Ok(rt) => rt.block_on(async move {
                    let env: Arc<dyn SuggestionEnvironment> = Arc::new(RealEnvironment::discover());
                    let store = store();
                    let path = state_path();
                    // Warm restart: re-surface the last-known tasks INSTANTLY
                    // (ages rebased to the snapshot's save time), then age out
                    // anything already stale AT SAVE, before the watchers
                    // re-poll. The picker is populated on the first frame.
                    if persist {
                        let now_ms = env.now_unix().saturating_mul(1000);
                        store.load_file(&path, now_ms);
                        store.decay_per_source(now_ms, |k| {
                            ttl_map.get(&k).copied().unwrap_or(global_ttl_ms)
                        });
                        store.gc(max_entries);
                    }
                    // The safra plane: swap the registry's unconfigured
                    // placeholder for the config-built adapter when the
                    // operator's safra: section declares cells.
                    let mut sources_vec = sources::registry();
                    if safra_cfg.enabled {
                        let adapter =
                            crate::safra::SafraSuggestionSource::from_config(&safra_cfg);
                        tracing::info!(cells = adapter.cell_count(), "safra plane live");
                        sources_vec.retain(|s| s.kind() != SourceKind::Safra);
                        sources_vec.push(Arc::new(adapter));
                    }
                    let engine = SuggestionEngine::start(
                        sources_vec,
                        Arc::clone(&env),
                        Arc::clone(&store),
                        engine_cfg,
                    );
                    tracing::info!(
                        watchers = engine.active_watchers(),
                        persist,
                        "mado suggestion engine live"
                    );
                    // Maintenance loop — the SINGLE owner of decay + debounced
                    // persist, off the watcher hot path. The 27 watchers only
                    // ever touch RAM; this coalesces a startup burst of first
                    // ticks into ONE disk write, and only when the change-
                    // generation actually advanced. Keeps the runtime + engine
                    // alive for the process lifetime.
                    let mut last_gen = store.generation();
                    let mut tick = tokio::time::interval(debounce);
                    loop {
                        tick.tick().await;
                        let now_ms = env.now_unix().saturating_mul(1000);
                        store.decay_per_source(now_ms, |k| {
                            ttl_map.get(&k).copied().unwrap_or(global_ttl_ms)
                        });
                        store.gc(max_entries);
                        let current_gen = store.generation();
                        if persist && current_gen != last_gen {
                            store.persist_file(&path, now_ms);
                            last_gen = current_gen;
                        }
                    }
                }),
                Err(e) => {
                    tracing::warn!(err = %e, "could not create suggestion tokio runtime");
                }
            }
        });
    if let Err(e) = res {
        tracing::warn!(err = %e, "could not spawn mado-suggest thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test (not two) so the process-global env mutations can't race in the
    /// parallel test runner — nothing else reads these vars.
    #[test]
    fn state_path_resolves_overrides_then_state_dir() {
        // SAFETY: env mutation is `unsafe` in edition 2024; this is the sole
        // mutator of these vars and runs its set/clear sequentially.
        unsafe {
            // Explicit DB override wins outright.
            std::env::set_var("MADO_SUGGEST_DB", "/tmp/mado-explicit/snap.json");
            assert_eq!(state_path(), PathBuf::from("/tmp/mado-explicit/snap.json"));

            // Without the explicit override, the state-dir override applies, under
            // a `mado/` subdir.
            std::env::remove_var("MADO_SUGGEST_DB");
            std::env::set_var("MADO_STATE_DIR", "/tmp/mado-state-test");
            assert_eq!(
                state_path(),
                PathBuf::from("/tmp/mado-state-test/mado/suggestions.json")
            );

            std::env::remove_var("MADO_STATE_DIR");
        }
    }
}
