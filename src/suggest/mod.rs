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
pub use core::{SourceKind, SpawnSpec, Suggestion, SuggestionId, Urgency};
#[allow(unused_imports)]
pub use env::{Cmd, HttpReq, MockEnvironment, RealEnvironment, SuggestionEnvironment};
#[allow(unused_imports)]
pub use source::{refresh_once, EngineConfig, SourceConfig, SuggestionEngine, SuggestionSource};
#[allow(unused_imports)]
pub use store::{shade_ramp, StoreSnapshot, StoredSuggestion, SuggestionStore};

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

/// Translate the typed shikumi `suggestions` config into an [`EngineConfig`].
#[must_use]
pub fn engine_config_from(cfg: &crate::config::SuggestionsConfig) -> EngineConfig {
    let mut ec = EngineConfig {
        per_source: std::collections::BTreeMap::new(),
        ttl_ms: cfg.ttl_secs.saturating_mul(1000),
        default_enabled: cfg.default_enabled,
    };
    for s in &cfg.sources {
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
/// the store empty, so the picker simply shows no suggestions).
pub fn spawn_engine_thread(cfg: &crate::config::SuggestionsConfig) {
    if !cfg.enabled {
        tracing::debug!("suggestion stream disabled (suggestions.enabled = false)");
        return;
    }
    let engine_cfg = engine_config_from(cfg);
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
                    let engine =
                        SuggestionEngine::start(sources::registry(), env, store(), engine_cfg);
                    tracing::info!(
                        watchers = engine.active_watchers(),
                        "mado suggestion engine live"
                    );
                    // Keep the runtime + engine alive for the process lifetime.
                    std::future::pending::<()>().await;
                    drop(engine);
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
