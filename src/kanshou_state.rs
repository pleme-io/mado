//! `MadoAppState` — the aggregator the kanshou server exposes.
//!
//! Reads the live atomics in `crate::render` and the session
//! registry the GUI mado actually populated. The MCP server in
//! `mado mcp` connects to this socket and forwards every
//! introspection query through it — closing the "MCP returns
//! process-local zeros while the GUI renders" class structurally.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use kanshou::{Introspect, Query, QueryError, QueryResult};

/// Live aggregator. Hand-implements [`Introspect`] because the
/// leaves are static atomics + a session-registry accessor — the
/// derive macro doesn't see static counters or arbitrary `&self`
/// methods.
///
/// Add new queryable surface by extending the match arms below + the
/// `schema` array. Each leaf is one `serde_json::json!` line; no
/// boilerplate.
pub struct MadoAppState {
    /// Snapshot of the config the GUI actually loaded. Cloned at
    /// startup; queries over this never read the on-disk file.
    pub config: Arc<crate::config::MadoConfig>,
    /// MCP-side session registry — populated by `spawn_term` /
    /// `tear_new_session` MCP tools when mado runs as the MCP
    /// server. The GUI process leaves this empty; live GUI
    /// sessions live in `tear_inproc` instead.
    pub sessions: Arc<crate::session::SessionRegistry>,
    /// Live in-process tear control plane — populated AFTER the
    /// GUI calls `tear_core::InProcess::new()` in
    /// `try_run_default_embedded`. Set once with [`OnceLock`]; reads
    /// are lock-free. The `sessions` leaf prefers this when set so
    /// kanshou queries reflect the GUI's actual session graph
    /// rather than the empty MCP-side registry.
    pub tear_inproc: OnceLock<Arc<tear_core::InProcess>>,
    /// Keybinding table the `simulate_chord` method leaf resolves
    /// against — mado baseline + the operator's `keybinds.custom`
    /// (see `keybind::manager_from_config`). Built from the same
    /// config snapshot as `config` above so the resolution reflects
    /// what this GUI process actually loaded.
    pub keybinds: crate::keybind::KeybindManager,
    /// Typed action-injection queue feeding the GUI event loop. The
    /// `simulate_chord` leaf pushes; `run_against_pane_unified`
    /// drains. See [`crate::action_injection::InjectedActions`].
    pub injected: crate::action_injection::InjectedActions,
}

impl MadoAppState {
    #[must_use]
    pub fn new(
        config: Arc<crate::config::MadoConfig>,
        sessions: Arc<crate::session::SessionRegistry>,
    ) -> Self {
        let keybinds = crate::keybind::manager_from_config(&config);
        Self {
            config,
            sessions,
            tear_inproc: OnceLock::new(),
            keybinds,
            injected: crate::action_injection::InjectedActions::default(),
        }
    }

    /// Plumb the live `InProcess` in once tear-attach has constructed
    /// it. Best-effort: second-set is silently ignored (we trust the
    /// first attach path).
    pub fn set_tear_inproc(&self, inproc: Arc<tear_core::InProcess>) {
        let _ = self.tear_inproc.set(inproc);
    }
}

impl Introspect for MadoAppState {
    fn query(&self, q: &Query) -> QueryResult {
        let Some(first) = q.path.first().map(String::as_str) else {
            return Err(QueryError::unknown_field(String::new()));
        };
        match first {
            "frame_perf" => Ok(serde_json::json!({
                "last_frame_us": crate::render::LAST_FRAME_US.load(Ordering::Relaxed),
                "last_frame_rects": crate::render::LAST_FRAME_RECTS.load(Ordering::Relaxed),
                "last_frame_text": crate::render::LAST_FRAME_TEXT.load(Ordering::Relaxed),
                "last_frame_shape_cache": crate::render::LAST_FRAME_SHAPE_CACHE.load(Ordering::Relaxed),
                "total_frames": crate::render::TOTAL_FRAMES.load(Ordering::Relaxed),
                "total_frames_skipped": crate::render::TOTAL_FRAMES_SKIPPED.load(Ordering::Relaxed),
            })),
            "sessions" => {
                // GUI mode: live tear-core registry IS the truth.
                // MCP-only mode: tear_inproc never gets populated,
                // fall back to SessionRegistry (which holds the
                // sessions `spawn_term` created in-MCP).
                if let Some(inproc) = self.tear_inproc.get() {
                    let sessions: Vec<serde_json::Value> = inproc.with_registry(|r| {
                        r.sessions
                            .values()
                            .map(|s| {
                                serde_json::json!({
                                    "id": s.id.to_string(),
                                    "name": s.name,
                                    "created_at_unix": s.created_at_unix,
                                    "active_window": s.active_window.to_string(),
                                    "windows": s.windows.len(),
                                    "panes": s.panes.len(),
                                    "source": format!("{:?}", s.source),
                                    "state": format!("{:?}", s.state),
                                })
                            })
                            .collect()
                    });
                    Ok(serde_json::json!({
                        "count": sessions.len(),
                        "sessions": sessions,
                        "source": "tear-inproc",
                    }))
                } else {
                    let summaries = self.sessions.list();
                    Ok(serde_json::json!({
                        "count": summaries.len(),
                        "sessions": summaries,
                        "source": "mcp-session-registry",
                    }))
                }
            }
            "config" => serde_json::to_value(&*self.config).map_err(|e| {
                QueryError::internal(format!("serialize MadoConfig: {e}"))
            }),
            "process" => Ok(serde_json::json!({
                "pid": std::process::id(),
                "binary": std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "version": env!("CARGO_PKG_VERSION"),
            })),
            // Method-call leaf — args: [chord: String]. Resolves the
            // chord against the keybinding table THIS GUI process
            // loaded and queues the bound Action for the event loop
            // to dispatch (see `action_injection::InjectedActions`).
            // The MCP `simulate_chord` tool forwards here; doing the
            // resolution GUI-side means the answer reflects the live
            // window's bindings, never the MCP process's idea of them.
            "simulate_chord" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let Some(chord) = q.args[0].as_str() else {
                    return Err(QueryError::TypeMismatch {
                        path: "simulate_chord".to_string(),
                        expected: "string".to_string(),
                        actual: format!("{:?}", q.args[0]),
                    });
                };
                let hotkey = match awase::Hotkey::parse(chord) {
                    Ok(h) => h,
                    Err(e) => {
                        return Err(QueryError::internal(format!(
                            "invalid chord {chord:?}: {e}"
                        )));
                    }
                };
                // Honest-surface gate: refuse to queue when no event
                // loop has registered as the drainer (e.g. local-PTY
                // fallback mode) — `queued: true` with nothing
                // draining would be a silent lie.
                if !self.injected.sink_attached() {
                    return Ok(serde_json::json!({
                        "queued": false,
                        "error": "no-injection-sink",
                        "chord": chord,
                        "note": "this mado is not running the tear-attached event loop; injected actions have no drainer",
                    }));
                }
                match self.keybinds.lookup(&hotkey) {
                    Some(action) => {
                        self.injected.push(action);
                        Ok(serde_json::json!({
                            "queued": true,
                            "action": action.as_str(),
                            "chord": chord,
                            "pending": self.injected.len(),
                        }))
                    }
                    None => Ok(serde_json::json!({
                        "queued": false,
                        "error": "no-binding",
                        "chord": chord,
                    })),
                }
            }
            other => Err(QueryError::unknown_field(other.to_string())),
        }
    }

    fn schema(&self) -> &'static [&'static str] {
        &["frame_perf", "sessions", "config", "process", "simulate_chord"]
    }
}

/// Spawn the kanshou server in a tokio task. Returns the path the
/// server bound to so the caller can log it. The task is detached;
/// dropping it shuts the server (and removes the socket file).
///
/// `app_name` is the canonical wire identifier — operator tools use
/// it to filter discovery. Pass `"mado"` for the GUI process.
pub fn spawn_server(
    app_name: &str,
    state: Arc<MadoAppState>,
) -> std::io::Result<std::path::PathBuf> {
    let server = kanshou::Server::new(app_name, state)?;
    let socket_path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            tracing::warn!(error = ?e, "mado kanshou server exited with error");
        }
    });
    Ok(socket_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybind::Action;

    fn state() -> MadoAppState {
        MadoAppState::new(
            Arc::new(crate::config::MadoConfig::default()),
            Arc::new(crate::session::SessionRegistry::default()),
        )
    }

    fn chord_query(chord: &str) -> Query {
        Query::call(["simulate_chord"], [serde_json::json!(chord)])
    }

    #[test]
    fn simulate_chord_queues_bound_action_when_sink_attached() {
        let s = state();
        s.injected.attach_sink();
        // Cmd+0 is the atlas font-reset chord in mado's defaults.
        let v = s.query(&chord_query("cmd+0")).expect("query ok");
        assert_eq!(v["queued"], true);
        assert_eq!(v["action"], "font_reset");
        assert_eq!(s.injected.drain(), vec![Action::FontReset]);
    }

    #[test]
    fn simulate_chord_without_sink_reports_no_injection_sink() {
        // No event loop registered (e.g. local-PTY fallback) — must
        // refuse to queue rather than report a queued-but-undrained
        // action.
        let s = state();
        let v = s.query(&chord_query("cmd+0")).expect("query ok");
        assert_eq!(v["queued"], false);
        assert_eq!(v["error"], "no-injection-sink");
        assert!(s.injected.is_empty());
    }

    #[test]
    fn simulate_chord_unbound_chord_reports_no_binding() {
        let s = state();
        s.injected.attach_sink();
        let v = s.query(&chord_query("ctrl+alt+shift+z")).expect("query ok");
        assert_eq!(v["queued"], false);
        assert_eq!(v["error"], "no-binding");
        assert!(s.injected.is_empty());
    }

    #[test]
    fn simulate_chord_malformed_chord_is_typed_error() {
        let s = state();
        s.injected.attach_sink();
        let err = s
            .query(&chord_query("not_a_real_chord!!!"))
            .expect_err("malformed chord must not queue");
        assert!(matches!(err, QueryError::Internal { .. }), "got {err:?}");
        assert!(s.injected.is_empty());
    }

    #[test]
    fn simulate_chord_bad_arity_is_typed_error() {
        let s = state();
        let err = s
            .query(&Query::call(["simulate_chord"], []))
            .expect_err("zero args must be BadArity");
        assert!(
            matches!(err, QueryError::BadArity { expected: 1, actual: 0 }),
            "got {err:?}"
        );
        let err = s
            .query(&Query::call(["simulate_chord"], [serde_json::json!(42)]))
            .expect_err("non-string arg must be TypeMismatch");
        assert!(matches!(err, QueryError::TypeMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn schema_advertises_simulate_chord() {
        assert!(state().schema().contains(&"simulate_chord"));
    }
}
