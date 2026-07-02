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
pub mod pace;
pub mod source;
pub mod sources;
pub mod store;

// The suggest-plane facade. These are the module's public API; not every name
// is consumed by the binary itself (several are used cross-module only under
// cfg(test) or by providers via their full paths), so the unused-import lint
// for the re-export surface is intentionally allowed.
#[allow(unused_imports)]
pub use core::{CorrKey, SourceKind, SourceStatus, SpawnSpec, Suggestion, SuggestionId, Urgency};
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

// ── Engine hot-reload control plane ─────────────────────────────────────

/// Command ingress for the engine maintenance loop. Boxed pair keeps the
/// channel payload one pointer wide.
enum EngineCommand {
    /// Tear down the running engine (if any) and rebuild it from the new
    /// `suggestions` + `safra` config sections. `enabled = false` parks the
    /// loop with no engine; `enabled = true` (re)builds — enable, disable,
    /// and reconfigure are ONE uniform path.
    Swap(Box<(crate::config::SuggestionsConfig, crate::safra::SafraConfig)>),
}

/// Handle for requesting an engine rebuild from the GUI render thread.
/// `UnboundedSender::send` is sync-callable — no runtime handle needed at
/// the call site.
pub struct EngineControl {
    tx: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
}

impl EngineControl {
    /// Request a rebuild against the given config sections. Idempotent at
    /// the receiver (a swap equal to the running config is dropped), so
    /// double-fire from the two render adapters is harmless.
    pub fn swap(
        &self,
        suggestions: crate::config::SuggestionsConfig,
        safra: crate::safra::SafraConfig,
    ) {
        let _ = self
            .tx
            .send(EngineCommand::Swap(Box::new((suggestions, safra))));
    }
}

/// Registered once by `spawn_engine_thread`; holding the sender in a static
/// keeps the control channel open for the process lifetime (recv can never
/// yield `None` while the static lives).
static ENGINE_CONTROL: OnceLock<EngineControl> = OnceLock::new();

/// The engine control handle — `None` only before `spawn_engine_thread`
/// ran (the thread now spawns unconditionally, so after boot this is
/// always `Some` and a boot-disabled engine can still be hot-enabled).
#[must_use]
pub fn engine_control() -> Option<&'static EngineControl> {
    ENGINE_CONTROL.get()
}

// ── The agent-facing board surface (shared by two ingresses) ────────────
//
// `mado mcp` is a SEPARATE process from the GUI, so its process-global
// store is NOT the operator's live board. The three functions below are the
// ONE implementation both ingresses call: the GUI's kanshou leaves (the
// live-board truth an MCP tool forwards to — the `list_sessions` idiom) and
// the MCP tools' local fallback (headless mado, no GUI running).

/// Wall-clock unix milliseconds — the board surface's single clock read.
#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// JSON view of THIS process's living board: ranked offerable rows (id,
/// source, title, urgency, lifecycle state, recurrence, spawn target) plus
/// per-source poll health. Ids are DECIMAL STRINGS — a u64 does not survive
/// JSON number precision.
#[must_use]
pub fn board_json(max: usize) -> serde_json::Value {
    let store = store();
    let now_ms = now_unix_ms();
    let rows: Vec<serde_json::Value> = store
        .ranked_stored(max.clamp(1, 200), now_ms)
        .into_iter()
        .map(|st| {
            serde_json::json!({
                "id": st.suggestion.id.0.to_string(),
                "source": st.suggestion.source.slug(),
                "title": st.suggestion.title,
                "detail": st.suggestion.detail,
                "urgency": serde_json::to_value(st.suggestion.urgency)
                    .unwrap_or(serde_json::Value::Null),
                "state": serde_json::to_value(&st.state)
                    .unwrap_or(serde_json::Value::Null),
                "times_seen": st.times_seen,
                "waiting_secs": now_ms.saturating_sub(st.first_seen_ms) / 1000,
                "cwd": st.suggestion.spawn.cwd().to_string_lossy(),
                "session_name": st.suggestion.spawn.name(),
                "initial_command": st.suggestion.spawn.initial_command(),
            })
        })
        .collect();
    let health: Vec<serde_json::Value> = store
        .health()
        .into_iter()
        .map(|(kind, h)| {
            serde_json::json!({
                "source": kind.slug(),
                "status": h.status.label(),
                "last_poll_secs_ago": now_ms.saturating_sub(h.last_poll_ms) / 1000,
                "ever_ok": h.last_ok_ms > 0,
            })
        })
        .collect();
    serde_json::json!({ "suggestions": rows, "health": health })
}

/// Typed parameters for an agent-injected board row — deserialized from the
/// MCP tool input AND from the kanshou call leaf's JSON argument (one
/// border, two ingresses).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct InjectParams {
    /// The task line the board shows (required, non-blank).
    pub title: String,
    /// Stable dedup key — re-injecting the same key updates the row.
    /// Defaults to the title.
    #[serde(default)]
    pub key: Option<String>,
    /// Secondary context shown dimmer.
    #[serde(default)]
    pub detail: Option<String>,
    /// idle | low | normal | high | critical. Default normal.
    #[serde(default)]
    pub urgency: Option<String>,
    /// Working directory the accepted session spawns into. Defaults to the
    /// operator's code root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Session name for the accepted row. Defaults to `🤝 <title…>`.
    #[serde(default)]
    pub session_name: Option<String>,
    /// Kickoff command typed into the fresh session (control bytes rejected
    /// at the typed border).
    #[serde(default)]
    pub command: Option<String>,
}

/// Push a task onto THIS process's board (the 🤝 agent lane, additive
/// upsert). Returns the row id as a decimal string.
pub fn inject(params: InjectParams) -> Result<serde_json::Value, String> {
    let title = params.title.trim().to_owned();
    if title.is_empty() {
        return Err(String::from("title must be non-empty"));
    }
    let cwd = params
        .cwd
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            std::env::var_os("PLEME_CODE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("code"))
                .to_string_lossy()
                .into_owned()
        });
    let name = params
        .session_name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            let mut n = String::from("\u{1F91D} "); // 🤝 agent-lane native name
            n.push_str(title.chars().take(40).collect::<String>().trim());
            n
        });
    let Some(mut spawn) = SpawnSpec::new(cwd, name) else {
        return Err(String::from("cwd and session_name must be non-empty"));
    };
    if let Some(cmd) = params.command {
        spawn = spawn.with_command(cmd);
    }
    let key = params.key.unwrap_or_else(|| title.clone());
    let mut s = Suggestion::new(SourceKind::Agent, &key, &title, spawn);
    if let Some(d) = params.detail {
        s = s.detail(d);
    }
    if let Some(u) = params.urgency {
        match serde_json::from_value::<Urgency>(serde_json::Value::String(u)) {
            Ok(parsed) => s = s.urgent(parsed),
            Err(_) => return Err(String::from("urgency must be idle|low|normal|high|critical")),
        }
    }
    let id = s.id;
    store().upsert(s, now_unix_ms());
    Ok(serde_json::json!({ "id": id.0.to_string() }))
}

/// Dismiss (or snooze) a board row by its decimal-string id on THIS
/// process's board.
pub fn dismiss(id_str: &str, snooze_secs: Option<u64>) -> Result<serde_json::Value, String> {
    let Ok(raw) = id_str.parse::<u64>() else {
        return Err(String::from("id must be the decimal u64 string from the board list"));
    };
    let id = core::SuggestionId(raw);
    let st = store();
    let done = match snooze_secs {
        Some(secs) => st.snooze(id, now_unix_ms().saturating_add(secs.saturating_mul(1000))),
        None => st.dismiss(id),
    };
    if done {
        Ok(serde_json::json!({ "dismissed": true, "id": id_str }))
    } else {
        Err(String::from("no suggestion with that id (it may have decayed)"))
    }
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

/// Config-derived maintenance knobs — rebuilt on every hot swap so TTLs,
/// persistence cadence, and entry caps track the LIVE config, not the boot
/// config.
struct LoopKnobs {
    /// Per-source TTL: a source's items live for max(3× its poll interval,
    /// the global floor) — so a slow (e.g. hourly) source never flickers
    /// under a fast global TTL.
    ttl_map: std::collections::BTreeMap<SourceKind, u64>,
    global_ttl_ms: u64,
    /// WRITING is additionally re-decided every maintenance tick via the
    /// single-writer election; this knob is the config half of that AND.
    persist: bool,
    max_entries: usize,
    /// 0 = "persist on every change" → a 1s minimum tick (tokio rejects a
    /// 0 interval); otherwise coalesce writes to this cadence.
    debounce: std::time::Duration,
}

impl LoopKnobs {
    fn from_config(
        cfg: &crate::config::SuggestionsConfig,
        engine_cfg: &EngineConfig,
    ) -> Self {
        let global_ttl_ms = cfg.ttl_secs.saturating_mul(1000);
        let mut ttl_map: std::collections::BTreeMap<SourceKind, u64> =
            std::collections::BTreeMap::new();
        for &kind in SourceKind::ALL {
            let interval_ttl = engine_cfg
                .config_for(kind)
                .interval
                .as_secs()
                .saturating_mul(3)
                .saturating_mul(1000);
            ttl_map.insert(kind, interval_ttl.max(global_ttl_ms));
        }
        Self {
            ttl_map,
            global_ttl_ms,
            persist: cfg.persist,
            max_entries: cfg.max_entries,
            debounce: std::time::Duration::from_secs(cfg.persist_debounce_secs.max(1)),
        }
    }
}

/// Build a RUNNING engine from the live config sections: merged source
/// registry (with the safra adapter swapped in when its section declares
/// cells) + watcher start. The one boot/hot-swap construction path.
fn build_engine(
    sugg: &crate::config::SuggestionsConfig,
    safra: &crate::safra::SafraConfig,
    env: &Arc<dyn SuggestionEnvironment>,
    store: &Arc<SuggestionStore>,
) -> (SuggestionEngine, LoopKnobs) {
    let engine_cfg = engine_config_from(sugg);
    let knobs = LoopKnobs::from_config(sugg, &engine_cfg);
    // The safra plane: swap the registry's unconfigured placeholder for the
    // config-built adapter when the operator's safra: section declares cells.
    let mut sources_vec = sources::registry();
    if safra.enabled {
        let adapter = crate::safra::SafraSuggestionSource::from_config(safra);
        tracing::info!(cells = adapter.cell_count(), "safra plane live");
        sources_vec.retain(|s| s.kind() != SourceKind::Safra);
        sources_vec.push(Arc::new(adapter));
    }
    let engine = SuggestionEngine::start(
        sources_vec,
        Arc::clone(env),
        Arc::clone(store),
        engine_cfg,
    );
    tracing::info!(
        watchers = engine.active_watchers(),
        persist = knobs.persist,
        "mado suggestion engine live"
    );
    (engine, knobs)
}

/// Spawn the suggestion control plane on its own multi-thread tokio runtime
/// thread — mirrors the vigy runtime (the GUI thread is not async). Spawns
/// UNCONDITIONALLY: the `suggestions.enabled` gate lives INSIDE the thread
/// (a boot-disabled engine is a parked loop holding the control channel, so
/// a later config edit can hot-enable it — enable/disable/reconfigure are
/// one uniform [`EngineCommand::Swap`] path via [`engine_control`]).
/// Best-effort (a runtime build failure logs + leaves the store empty, so
/// the picker simply shows no suggestions). Parking the disabled loop also
/// keeps `praca_store::maintenance_tick` alive, so praça preset persistence
/// no longer depends on suggestions being enabled.
pub fn spawn_engine_thread(
    cfg: &crate::config::SuggestionsConfig,
    safra: &crate::safra::SafraConfig,
) {
    let sugg_cfg = cfg.clone();
    let safra_cfg = safra.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = ENGINE_CONTROL.set(EngineControl { tx });
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
                    let mut current = (sugg_cfg, safra_cfg);
                    // Config-gated (INSIDE the thread): enabled ⇒ live
                    // engine; disabled ⇒ parked loop holding the control
                    // channel so a hot-enable is one Swap away.
                    let (mut engine, mut knobs) = if current.0.enabled {
                        let (e, k) = build_engine(&current.0, &current.1, &env, &store);
                        (Some(e), k)
                    } else {
                        tracing::debug!(
                            "suggestion stream disabled (suggestions.enabled = false) — control plane parked"
                        );
                        (None, LoopKnobs::from_config(&current.0, &engine_config_from(&current.0)))
                    };
                    // Warm restart: re-surface the last-known tasks INSTANTLY
                    // (ages rebased to the snapshot's save time), then age out
                    // anything already stale AT SAVE, before the watchers
                    // re-poll. The picker is populated on the first frame.
                    // Deliberately NOT election-gated: a loser instance loads
                    // too, so a second window boots with the live board and
                    // the persisted dismissal stickiness intact. Loads even
                    // when boot-disabled (cheap; a later hot-enable then
                    // starts from the last-known board, not a blank one).
                    if knobs.persist {
                        let now_ms = env.now_unix().saturating_mul(1000);
                        store.load_file(&path, now_ms);
                        store.decay_per_source(now_ms, |k| {
                            knobs.ttl_map.get(&k).copied().unwrap_or(knobs.global_ttl_ms)
                        });
                        store.gc(knobs.max_entries, now_ms);
                    }
                    // Maintenance loop — the SINGLE owner of decay + debounced
                    // persist, off the watcher hot path. The watchers only
                    // ever touch RAM; this coalesces a startup burst of first
                    // ticks into ONE disk write, and only when the change-
                    // generation actually advanced. Keeps the runtime + engine
                    // alive for the process lifetime. The select's second arm
                    // is the hot-reload ingress: a Swap tears down the running
                    // engine (stop() aborts every watcher task) and rebuilds
                    // from the NEW config sections — same path for enable,
                    // disable, and reconfigure.
                    let mut last_gen = store.generation();
                    let mut tick = tokio::time::interval(knobs.debounce);
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                let now_ms = env.now_unix().saturating_mul(1000);
                                store.decay_per_source(now_ms, |k| {
                                    knobs.ttl_map.get(&k).copied().unwrap_or(knobs.global_ttl_ms)
                                });
                                store.gc(knobs.max_entries, now_ms);
                                let current_gen = store.generation();
                                // The writer election is RE-CHECKED every tick (a
                                // cheap non-blocking flock attempt when not already
                                // held), so a surviving instance picks up the writer
                                // role when the previous winner exits — persistence
                                // never silently dies with the first process.
                                if knobs.persist
                                    && current_gen != last_gen
                                    && crate::single_writer::is_writer()
                                {
                                    store.persist_file(&path, now_ms);
                                    last_gen = current_gen;
                                }
                                // Praça persistence rides the same maintenance tick
                                // (internally change-gated + writer-election-gated) —
                                // saved presets survive restarts with zero extra
                                // threads and zero GUI-hot-path writes.
                                crate::praca_store::maintenance_tick();
                            }
                            cmd = rx.recv() => match cmd {
                                Some(EngineCommand::Swap(pair)) => {
                                    // Idempotence gate: both config sections
                                    // derive PartialEq, so double-fire from
                                    // the two render adapters (and any
                                    // renderer-only config edit) is free.
                                    if *pair == current {
                                        continue;
                                    }
                                    if let Some(e) = engine.take() {
                                        e.stop();
                                    }
                                    current = *pair;
                                    if current.0.enabled {
                                        let (e, k) =
                                            build_engine(&current.0, &current.1, &env, &store);
                                        engine = Some(e);
                                        knobs = k;
                                    } else {
                                        knobs = LoopKnobs::from_config(
                                            &current.0,
                                            &engine_config_from(&current.0),
                                        );
                                    }
                                    tick = tokio::time::interval(knobs.debounce);
                                    tracing::info!(
                                        enabled = current.0.enabled,
                                        "suggestion engine hot-swapped from config edit"
                                    );
                                }
                                // Unreachable while ENGINE_CONTROL holds a
                                // sender for the process lifetime; park
                                // defensively rather than busy-spin on a
                                // closed channel.
                                None => {
                                    tracing::warn!(
                                        "engine control channel closed — hot-reload disabled"
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(3600))
                                        .await;
                                }
                            }
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
