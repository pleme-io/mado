//! `MadoAppState` — the aggregator the kanshou server exposes.
//!
//! Reads the live atomics in `crate::render` and the session
//! registry the GUI mado actually populated. The MCP server in
//! `mado mcp` connects to this socket and forwards every
//! introspection query through it — closing the "MCP returns
//! process-local zeros while the GUI renders" class structurally.

use std::sync::Arc;
use std::sync::Mutex;
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
    /// The shared praça orchestrator (index + binding + latent-preset
    /// catalog), the SAME `Arc<Mutex<_>>` the picker bridge + auto-attach
    /// driver hold. Set once at GUI startup; lets the
    /// `save_session_as_preset` MCP verb capture a session into the catalog.
    pub praca: OnceLock<Arc<Mutex<praca::Praca>>>,
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
    /// Typed session-switch channel feeding the switchable attach. The
    /// `switch_session` leaf posts; `run_against_pane_unified` polls
    /// (only when `tear.session_switching = true`). See
    /// [`crate::session_switch::SwitchRequests`].
    pub switch: crate::session_switch::SwitchRequests,
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
            praca: OnceLock::new(),
            keybinds,
            injected: crate::action_injection::InjectedActions::default(),
            switch: crate::session_switch::SwitchRequests::default(),
        }
    }

    /// Plumb the live `InProcess` in once tear-attach has constructed
    /// it. Best-effort: second-set is silently ignored (we trust the
    /// first attach path).
    pub fn set_tear_inproc(&self, inproc: Arc<tear_core::InProcess>) {
        let _ = self.tear_inproc.set(inproc);
    }

    /// Plumb the shared praça orchestrator in (the same `Arc` the picker
    /// bridge holds), so the `save_session_as_preset` MCP verb can capture
    /// into the latent catalog. Best-effort, set-once.
    pub fn set_praca(&self, praca: Arc<Mutex<praca::Praca>>) {
        let _ = self.praca.set(praca);
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
            // Method-call leaf — args: [pane_id: String (16-char hex)].
            // Posts a runtime re-attach request to the switchable
            // event loop: the displayed pane re-binds to the requested
            // live in-process tear pane (same window + renderer, fresh
            // terminal). The MCP `switch_session` tool forwards here;
            // doing the resolution GUI-side means the answer reflects
            // the live window's session graph + switch state.
            "switch_session" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let Some(pane_str) = q.args[0].as_str() else {
                    return Err(QueryError::TypeMismatch {
                        path: "switch_session".to_string(),
                        expected: "string".to_string(),
                        actual: format!("{:?}", q.args[0]),
                    });
                };
                let pane = match pane_str.parse::<tear_types::PaneId>() {
                    Ok(p) => p,
                    Err(e) => {
                        return Err(QueryError::internal(format!(
                            "invalid pane id {pane_str:?}: {e}"
                        )));
                    }
                };
                // Honest-surface gate: refuse when no switchable loop
                // has registered as the drainer. The legacy one-shot
                // loop never attaches this sink, so `switched: true`
                // with nothing polling would be a silent lie. This is
                // the runtime mirror of `tear.session_switching = false`.
                if !self.switch.sink_attached() {
                    return Ok(serde_json::json!({
                        "switched": false,
                        "error": "switching-disabled",
                        "pane_id": pane_str,
                        "note": "this mado is not running the switchable event loop (tear.session_switching = false, or local-PTY fallback)",
                    }));
                }
                // Validate the target pane exists in the live tear
                // registry — posting a non-existent pane would land the
                // event loop on an empty subscribe + replay. The
                // switchable loop is embedded-only, so the live graph
                // is `tear_inproc`.
                let known = self
                    .tear_inproc
                    .get()
                    .map(|inproc| {
                        inproc.with_registry(|r| r.sessions.values().any(|s| s.panes.contains_key(&pane)))
                    })
                    .unwrap_or(false);
                if !known {
                    return Ok(serde_json::json!({
                        "switched": false,
                        "error": "no-such-pane",
                        "pane_id": pane_str,
                        "note": "no live in-process tear pane with this id; create one with tear_new_session first",
                    }));
                }
                self.switch.post(pane);
                Ok(serde_json::json!({
                    "switched": true,
                    "pane_id": pane_str,
                }))
            }
            // Method-call leaf — args: [pane_id]. Captures the pane's
            // session as a reusable latent preset (layout + per-pane spawn
            // specs, via the shipped `from_live`), so it appears as a ○
            // Instantiate row in Ctrl-S once not running. The save-as-preset
            // gesture that feeds the union picker's latent catalog.
            "save_session_as_preset" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let Some(pane_str) = q.args[0].as_str() else {
                    return Err(QueryError::TypeMismatch {
                        path: "save_session_as_preset".to_string(),
                        expected: "string".to_string(),
                        actual: format!("{:?}", q.args[0]),
                    });
                };
                let pane = match pane_str.parse::<tear_types::PaneId>() {
                    Ok(p) => p,
                    Err(e) => {
                        return Err(QueryError::internal(format!(
                            "invalid pane id {pane_str:?}: {e}"
                        )));
                    }
                };
                let Some(inproc) = self.tear_inproc.get() else {
                    return Ok(serde_json::json!({
                        "saved": false,
                        "error": "no-live-backend",
                        "pane_id": pane_str,
                    }));
                };
                let Some(praca) = self.praca.get() else {
                    return Ok(serde_json::json!({
                        "saved": false,
                        "error": "no-praca",
                        "pane_id": pane_str,
                    }));
                };
                // Resolve the pane to its session in the live registry.
                let session = inproc.with_registry(|r| {
                    r.sessions
                        .values()
                        .find(|s| s.panes.contains_key(&pane))
                        .map(|s| s.id)
                });
                let Some(session) = session else {
                    return Ok(serde_json::json!({
                        "saved": false,
                        "error": "no-such-pane",
                        "pane_id": pane_str,
                    }));
                };
                let now = crate::auto_attach::now_unix_seconds();
                let saved = crate::session_picker::capture_preset(inproc, praca, session, now);
                Ok(serde_json::json!({
                    "saved": saved,
                    "pane_id": pane_str,
                    "session_id": session.to_string(),
                }))
            }
            // Method-call leaf — args: [SpawnTermParams object]. Spawns a
            // session INTO THIS GUI's embedded tear registry (session-world
            // union phase 1: the MCP `spawn_term` tool forwards here so an
            // agent-spawned session lands in the world the Ctrl-S picker
            // actually reads — the InProcessSessionReconciler absorbs it
            // into praca on the next picker refresh, so it shows as a ●
            // row and anchors live-dedup with zero picker changes).
            // Deliberately does NOT touch the InProcess spawn env: the
            // picker's set_spawn_env→spawn two-step runs on the GUI thread
            // and a cwd override from this thread could interleave between
            // them. cwd support waits on an atomic spawn-with-env upstream
            // API in tear-core (ledgered).
            "spawn_term" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let params: SpawnTermParams = serde_json::from_value(q.args[0].clone())
                    .map_err(|e| QueryError::internal(format!("invalid spawn params: {e}")))?;
                let Some(inproc) = self.tear_inproc.get() else {
                    return Ok(serde_json::json!({
                        "spawned": false,
                        "error": "no-live-backend",
                        "note": "this GUI runs without an embedded tear registry (daemon mode)",
                    }));
                };
                let shell = params
                    .shell
                    .filter(|s| !s.is_empty())
                    .or_else(|| self.config.shell.command.clone())
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "/bin/sh".to_string());
                use tear_types::{MultiplexerControl, SessionSource};
                let size = (params.cols.unwrap_or(80), params.rows.unwrap_or(24));
                match inproc.new_session_with_source_and_size(
                    &params.name,
                    &shell,
                    SessionSource::Agent,
                    size,
                ) {
                    Ok(sid) => Ok(serde_json::json!({
                        "spawned": true,
                        "session_id": sid.to_string(),
                        "name": params.name,
                        "shell": shell,
                        "world": "embedded",
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "spawned": false,
                        "error": e.to_string(),
                    })),
                }
            }
            // Method-call leaf — args: [session_id: String]. Closes an
            // AGENT-spawned session in THIS GUI's embedded tear registry
            // — the teardown half of the `spawn_term` leaf above
            // (session-world union). Without it, a session an agent
            // spawned into the embedded world was unkillable over MCP
            // (`close_session` only knew the MCP-process-local headless
            // registry), so agent/e2e sessions accumulated as ghost
            // "sh" rows in the operator's Ctrl-S picker (operator
            // report 2026-07-06). Two conservative typed guards:
            //   * only `SessionSource::Agent` sessions are closable
            //     here — the operator's own sessions (embedded window,
            //     picker-named, human) are structurally out of reach of
            //     this path;
            //   * a session with live pane subscribers (a mado window
            //     or recorder is attached) is refused — never yank a
            //     session a client is watching.
            //
            // The guards live in ONE place —
            // `crate::janitors::guarded_close_agent_session` — shared with
            // the ghost-session janitor's remediation path (solve-once);
            // this leaf only parses args + renders the outcome to JSON.
            "close_session" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let Some(sid_str) = q.args[0].as_str() else {
                    return Err(QueryError::TypeMismatch {
                        path: "close_session".to_string(),
                        expected: "string".to_string(),
                        actual: format!("{:?}", q.args[0]),
                    });
                };
                let Ok(sid) = sid_str.parse::<tear_types::SessionId>() else {
                    return Ok(serde_json::json!({
                        "closed": false,
                        "error": "invalid-session-id",
                        "session_id": sid_str,
                    }));
                };
                let Some(inproc) = self.tear_inproc.get() else {
                    return Ok(serde_json::json!({
                        "closed": false,
                        "error": "no-live-backend",
                        "session_id": sid_str,
                        "note": "this GUI runs without an embedded tear registry (daemon mode)",
                    }));
                };
                use crate::janitors::GuardedClose;
                match crate::janitors::guarded_close_agent_session(inproc, sid) {
                    GuardedClose::Closed => Ok(serde_json::json!({
                        "closed": true,
                        "session_id": sid_str,
                        "world": "embedded",
                    })),
                    GuardedClose::NoSuchSession => Ok(serde_json::json!({
                        "closed": false,
                        "error": "no-such-session",
                        "session_id": sid_str,
                    })),
                    GuardedClose::NotAgentOwned { source } => Ok(serde_json::json!({
                        "closed": false,
                        "error": "not-agent-owned",
                        "session_id": sid_str,
                        "source": source,
                        "note": "only SessionSource::Agent sessions are closable over MCP; operator sessions close from the GUI",
                    })),
                    GuardedClose::Attached { subscribers } => Ok(serde_json::json!({
                        "closed": false,
                        "error": "attached",
                        "session_id": sid_str,
                        "subscribers": subscribers,
                        "note": "a client is attached to this session's byte stream; detach it first",
                    })),
                    GuardedClose::Error(e) => Ok(serde_json::json!({
                        "closed": false,
                        "session_id": sid_str,
                        "error": e,
                    })),
                }
            }
            // Method-call leaf — args: [target: String, text: String].
            // Writes bytes into an EMBEDDED pane (session-world union:
            // the agent-I/O half of the spawn_term leaf — an agent that
            // spawned into the GUI's registry can now type into it).
            // `target` resolves as a session id (its first pane) first,
            // then as a bare pane id. Text bytes are written verbatim;
            // the MCP side decodes its keys grammar before forwarding.
            // inproc.send_keys carries the Leader-policy gate, so a
            // locked pane refuses here exactly like the daemon path.
            "send_keys_embedded" => {
                if q.args.len() != 2 {
                    return Err(QueryError::BadArity {
                        expected: 2,
                        actual: q.args.len(),
                    });
                }
                let (Some(target), Some(text)) = (q.args[0].as_str(), q.args[1].as_str())
                else {
                    return Err(QueryError::TypeMismatch {
                        path: "send_keys_embedded".to_string(),
                        expected: "[string, string]".to_string(),
                        actual: format!("{:?}", q.args),
                    });
                };
                let Some(inproc) = self.tear_inproc.get() else {
                    return Ok(serde_json::json!({
                        "sent": false,
                        "error": "no-live-backend",
                    }));
                };
                let pane = inproc
                    .with_registry(|r| {
                        r.sessions
                            .values()
                            .find(|s| s.id.to_string() == target)
                            .and_then(|s| s.panes.keys().next().copied())
                    })
                    .or_else(|| {
                        let p = target.parse::<tear_types::PaneId>().ok()?;
                        inproc
                            .with_registry(|r| {
                                r.sessions.values().any(|s| s.panes.contains_key(&p))
                            })
                            .then_some(p)
                    });
                let Some(pane) = pane else {
                    return Ok(serde_json::json!({
                        "sent": false,
                        "error": "no-such-target",
                        "target": target,
                    }));
                };
                use tear_types::MultiplexerControl;
                match inproc.send_keys(pane, text.as_bytes()) {
                    Ok(()) => Ok(serde_json::json!({
                        "sent": true,
                        "pane_id": pane.to_string(),
                        "bytes_written": text.len(),
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "sent": false,
                        "pane_id": pane.to_string(),
                        "error": e.to_string(),
                    })),
                }
            }
            // Method-call leaf — args: [target: String]. Read-side twin of
            // `send_keys_embedded`: a text snapshot of an EMBEDDED pane so
            // an agent can OBSERVE the session it spawned/typed into.
            // Same target resolution (session id → first pane, else pane
            // id). Read-only.
            "pane_snapshot_embedded" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let Some(target) = q.args[0].as_str() else {
                    return Err(QueryError::TypeMismatch {
                        path: "pane_snapshot_embedded".to_string(),
                        expected: "string".to_string(),
                        actual: format!("{:?}", q.args[0]),
                    });
                };
                let Some(inproc) = self.tear_inproc.get() else {
                    return Ok(serde_json::json!({
                        "found": false,
                        "error": "no-live-backend",
                    }));
                };
                let pane = inproc
                    .with_registry(|r| {
                        r.sessions
                            .values()
                            .find(|s| s.id.to_string() == target)
                            .and_then(|s| s.panes.keys().next().copied())
                    })
                    .or_else(|| {
                        let p = target.parse::<tear_types::PaneId>().ok()?;
                        inproc
                            .with_registry(|r| {
                                r.sessions.values().any(|s| s.panes.contains_key(&p))
                            })
                            .then_some(p)
                    });
                let Some(pane) = pane else {
                    return Ok(serde_json::json!({
                        "found": false,
                        "error": "no-such-target",
                        "target": target,
                    }));
                };
                match inproc.pane_snapshot(pane) {
                    Ok(snap) => Ok(serde_json::json!({
                        "found": true,
                        "pane_id": pane.to_string(),
                        "cols": snap.cols,
                        "rows": snap.rows,
                        "cursor_row": snap.cursor_row,
                        "cursor_col": snap.cursor_col,
                        "alt_screen_active": snap.alt_screen_active,
                        "text_rows": snap.to_text_rows(),
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "found": false,
                        "pane_id": pane.to_string(),
                        "error": e.to_string(),
                    })),
                }
            }
            // The living Ctrl-S board, read from THIS GUI process's store —
            // the truth the MCP `suggest_list` tool forwards to (its own
            // process-global store is a separate world). Optional arg: [max].
            "suggest" => {
                let max = q
                    .args
                    .first()
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|m| usize::try_from(m).ok())
                    .unwrap_or(50);
                Ok(crate::suggest::board_json(max))
            }
            // Method-call leaf — args: [InjectParams object]. Pushes a task
            // onto THIS GUI's live board (the 🤝 agent lane), so an agent's
            // injection lands where the operator's Ctrl-S actually reads.
            "suggest_inject" => {
                if q.args.len() != 1 {
                    return Err(QueryError::BadArity {
                        expected: 1,
                        actual: q.args.len(),
                    });
                }
                let params: crate::suggest::InjectParams =
                    serde_json::from_value(q.args[0].clone()).map_err(|e| {
                        QueryError::internal(format!("invalid inject params: {e}"))
                    })?;
                crate::suggest::inject(params).map_err(QueryError::internal)
            }
            // Method-call leaf — args: [id: String, snooze_secs: u64|null].
            // Dismisses/snoozes a row on THIS GUI's live board.
            "suggest_dismiss" => {
                if q.args.is_empty() || q.args.len() > 2 {
                    return Err(QueryError::BadArity {
                        expected: 2,
                        actual: q.args.len(),
                    });
                }
                let Some(id_str) = q.args[0].as_str() else {
                    return Err(QueryError::TypeMismatch {
                        path: "suggest_dismiss".to_string(),
                        expected: "string".to_string(),
                        actual: format!("{:?}", q.args[0]),
                    });
                };
                let snooze = q.args.get(1).and_then(serde_json::Value::as_u64);
                crate::suggest::dismiss(id_str, snooze).map_err(QueryError::internal)
            }
            other => Err(QueryError::unknown_field(other.to_string())),
        }
    }

    fn schema(&self) -> &'static [&'static str] {
        &[
            "frame_perf",
            "sessions",
            "config",
            "process",
            "simulate_chord",
            "switch_session",
            "save_session_as_preset",
            "suggest",
            "suggest_inject",
            "suggest_dismiss",
            "spawn_term",
            "close_session",
            "send_keys_embedded",
            "pane_snapshot_embedded",
        ]
    }
}

/// Params for the `spawn_term` leaf — the session-world-union spawn
/// ingress. Shared by the leaf handler and the MCP tool's forward
/// call so the two sides can't drift. `shell = None/""` resolves to
/// the GUI's configured shell (then `$SHELL`, then `/bin/sh`); size
/// defaults to 80×24 (the picker's own spawn size).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpawnTermParams {
    pub name: String,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
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

    // ── spawn_term leaf (session-world union phase 1) ─────────────

    #[test]
    fn spawn_term_leaf_spawns_into_embedded_registry() {
        let s = state();
        let inproc = Arc::new(tear_core::InProcess::new());
        s.set_tear_inproc(Arc::clone(&inproc));
        let v = s
            .query(&Query::call(
                ["spawn_term"],
                [serde_json::json!({
                    "name": "spawn-leaf-test",
                    "shell": "/bin/sh",
                    "cols": 40,
                    "rows": 8
                })],
            ))
            .expect("query ok");
        assert_eq!(v["spawned"], true);
        assert_eq!(v["world"], "embedded");
        let sid = v["session_id"].as_str().expect("session id").to_string();
        // The session exists in the live registry, tagged as the
        // agent lane (SessionSource::Agent) so the picker can style it.
        let found = inproc.with_registry(|r| {
            r.sessions.values().any(|sess| {
                sess.id.to_string() == sid
                    && matches!(sess.source, tear_types::SessionSource::Agent)
            })
        });
        assert!(found, "spawned session must be in the embedded registry");
    }

    #[test]
    fn spawn_term_leaf_without_backend_reports_no_live_backend() {
        let s = state();
        let v = s
            .query(&Query::call(
                ["spawn_term"],
                [serde_json::json!({"name": "x"})],
            ))
            .expect("query ok");
        assert_eq!(v["spawned"], false);
        assert_eq!(v["error"], "no-live-backend");
    }

    #[test]
    fn spawn_term_leaf_bad_params_is_typed_error() {
        let s = state();
        let err = s
            .query(&Query::call(["spawn_term"], []))
            .expect_err("zero args must be BadArity");
        assert!(
            matches!(err, QueryError::BadArity { expected: 1, actual: 0 }),
            "got {err:?}"
        );
        let err = s
            .query(&Query::call(["spawn_term"], [serde_json::json!(42)]))
            .expect_err("non-object params must be a typed error");
        assert!(
            format!("{err:?}").contains("invalid spawn params"),
            "got {err:?}"
        );
    }

    #[test]
    fn schema_advertises_spawn_term() {
        assert!(state().schema().contains(&"spawn_term"));
    }

    // ── close_session leaf (session-world union teardown) ─────────

    /// Spawn an agent session via the leaf, then return `(state,
    /// inproc, session_id)` for the close-leaf tests.
    fn state_with_agent_session() -> (MadoAppState, Arc<tear_core::InProcess>, String) {
        let s = state();
        let inproc = Arc::new(tear_core::InProcess::new());
        s.set_tear_inproc(Arc::clone(&inproc));
        let v = s
            .query(&Query::call(
                ["spawn_term"],
                [serde_json::json!({
                    "name": "close-leaf-test",
                    "shell": "/bin/sh",
                    "cols": 40,
                    "rows": 8
                })],
            ))
            .expect("spawn query ok");
        let sid = v["session_id"].as_str().expect("session id").to_string();
        (s, inproc, sid)
    }

    #[test]
    fn close_session_leaf_closes_agent_session() {
        let (s, inproc, sid) = state_with_agent_session();
        let v = s
            .query(&Query::call(["close_session"], [serde_json::json!(sid)]))
            .expect("close query ok");
        assert_eq!(v["closed"], true, "got {v}");
        assert_eq!(v["world"], "embedded");
        let still_there =
            inproc.with_registry(|r| r.sessions.values().any(|sess| sess.id.to_string() == sid));
        assert!(!still_there, "closed session must leave the registry");
    }

    #[test]
    fn close_session_leaf_refuses_non_agent_session() {
        use tear_types::MultiplexerControl;
        let s = state();
        let inproc = Arc::new(tear_core::InProcess::new());
        s.set_tear_inproc(Arc::clone(&inproc));
        // `new_session` defaults to SessionSource::Human — the
        // operator-owned shape the leaf must refuse to touch.
        let sid = inproc
            .new_session("operator-session", "/bin/sh")
            .expect("new_session");
        let v = s
            .query(&Query::call(
                ["close_session"],
                [serde_json::json!(sid.to_string())],
            ))
            .expect("close query ok");
        assert_eq!(v["closed"], false, "got {v}");
        assert_eq!(v["error"], "not-agent-owned");
        let still_there = inproc.with_registry(|r| r.sessions.contains_key(&sid));
        assert!(still_there, "refused session must stay in the registry");
    }

    #[test]
    fn close_session_leaf_refuses_attached_session() {
        let (s, inproc, sid) = state_with_agent_session();
        // Attach a live subscriber (a mado window watching the pane).
        let pane = inproc.with_registry(|r| {
            r.sessions
                .values()
                .find(|sess| sess.id.to_string() == sid)
                .and_then(|sess| sess.panes.keys().next().copied())
                .expect("agent session has a pane")
        });
        let _rx = inproc.subscribe_pane_bytes(pane).expect("subscribe");
        let v = s
            .query(&Query::call(["close_session"], [serde_json::json!(sid)]))
            .expect("close query ok");
        assert_eq!(v["closed"], false, "got {v}");
        assert_eq!(v["error"], "attached");
        let still_there =
            inproc.with_registry(|r| r.sessions.values().any(|sess| sess.id.to_string() == sid));
        assert!(still_there, "watched session must never be yanked");
    }

    #[test]
    fn close_session_leaf_unknown_session_reports_no_such_session() {
        let s = state();
        s.set_tear_inproc(Arc::new(tear_core::InProcess::new()));
        let v = s
            .query(&Query::call(
                ["close_session"],
                [serde_json::json!("00000000deadbeef")],
            ))
            .expect("close query ok");
        assert_eq!(v["closed"], false);
        assert_eq!(v["error"], "no-such-session");
    }

    #[test]
    fn close_session_leaf_invalid_id_is_refused() {
        let s = state();
        s.set_tear_inproc(Arc::new(tear_core::InProcess::new()));
        let v = s
            .query(&Query::call(
                ["close_session"],
                [serde_json::json!("not-a-session-id")],
            ))
            .expect("close query ok");
        assert_eq!(v["closed"], false);
        assert_eq!(v["error"], "invalid-session-id");
    }

    #[test]
    fn close_session_leaf_without_backend_reports_no_live_backend() {
        let s = state();
        let v = s
            .query(&Query::call(
                ["close_session"],
                [serde_json::json!("00000000deadbeef")],
            ))
            .expect("close query ok");
        assert_eq!(v["closed"], false);
        assert_eq!(v["error"], "no-live-backend");
    }

    #[test]
    fn schema_advertises_close_session() {
        assert!(state().schema().contains(&"close_session"));
    }

    // ── send_keys_embedded leaf (agent I/O into embedded panes) ───

    #[test]
    fn send_keys_embedded_resolves_session_id_to_first_pane() {
        let (s, _pane) = state_with_live_pane();
        // Resolve via the SESSION id (not the pane id).
        let sid = s
            .tear_inproc
            .get()
            .unwrap()
            .with_registry(|r| r.sessions.keys().next().copied())
            .expect("live session")
            .to_string();
        let v = s
            .query(&Query::call(
                ["send_keys_embedded"],
                [serde_json::json!(sid), serde_json::json!("echo hi\n")],
            ))
            .expect("query ok");
        assert_eq!(v["sent"], true, "got {v}");
        assert_eq!(v["bytes_written"], 8);
    }

    #[test]
    fn send_keys_embedded_unknown_target_is_typed_refusal() {
        let (s, _pane) = state_with_live_pane();
        let v = s
            .query(&Query::call(
                ["send_keys_embedded"],
                [serde_json::json!("feedfacefeedface"), serde_json::json!("x")],
            ))
            .expect("query ok");
        assert_eq!(v["sent"], false);
        assert_eq!(v["error"], "no-such-target");
    }

    #[test]
    fn send_keys_embedded_without_backend_reports_no_live_backend() {
        let s = state();
        let v = s
            .query(&Query::call(
                ["send_keys_embedded"],
                [serde_json::json!("abc"), serde_json::json!("x")],
            ))
            .expect("query ok");
        assert_eq!(v["sent"], false);
        assert_eq!(v["error"], "no-live-backend");
    }

    #[test]
    fn schema_advertises_send_keys_embedded() {
        assert!(state().schema().contains(&"send_keys_embedded"));
    }

    // ── pane_snapshot_embedded leaf (read-side twin) ──────────────

    #[test]
    fn pane_snapshot_embedded_resolves_session_id() {
        let (s, pane) = state_with_live_pane();
        let sid = s
            .tear_inproc
            .get()
            .unwrap()
            .with_registry(|r| r.sessions.keys().next().copied())
            .expect("live session")
            .to_string();
        let v = s
            .query(&Query::call(
                ["pane_snapshot_embedded"],
                [serde_json::json!(sid)],
            ))
            .expect("query ok");
        assert_eq!(v["found"], true, "got {v}");
        assert_eq!(v["pane_id"], pane);
        assert!(v["text_rows"].is_array());
        assert!(v["cols"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn pane_snapshot_embedded_unknown_target_refuses() {
        let (s, _pane) = state_with_live_pane();
        let v = s
            .query(&Query::call(
                ["pane_snapshot_embedded"],
                [serde_json::json!("feedfacefeedface")],
            ))
            .expect("query ok");
        assert_eq!(v["found"], false);
        assert_eq!(v["error"], "no-such-target");
    }

    #[test]
    fn schema_advertises_pane_snapshot_embedded() {
        assert!(state().schema().contains(&"pane_snapshot_embedded"));
    }

    // ── switch_session leaf ───────────────────────────────────────

    fn switch_query(pane_id: &str) -> Query {
        Query::call(["switch_session"], [serde_json::json!(pane_id)])
    }

    /// A live in-process session + the id of its first pane, plumbed
    /// into `tear_inproc` so the leaf's pane-existence check passes.
    fn state_with_live_pane() -> (MadoAppState, String) {
        use tear_types::{MultiplexerControl, SessionSource};
        let s = state();
        let inproc = Arc::new(tear_core::InProcess::new());
        let sid = inproc
            .new_session_with_source_and_size(
                "switch-leaf-test",
                "/bin/sh",
                SessionSource::Named("switch-leaf-test".into()),
                (80, 24),
            )
            .expect("spawn session");
        let pane = inproc
            .with_registry(|r| {
                r.sessions
                    .get(&sid)
                    .and_then(|sess| sess.panes.keys().next().copied())
            })
            .expect("session has a pane");
        s.set_tear_inproc(inproc);
        (s, pane.to_string())
    }

    #[test]
    fn switch_session_without_sink_reports_switching_disabled() {
        // The legacy one-shot loop never attaches the switch sink —
        // the leaf must refuse rather than post into the void. This is
        // the runtime mirror of tear.session_switching = false.
        let (s, pane) = state_with_live_pane();
        let v = s.query(&switch_query(&pane)).expect("query ok");
        assert_eq!(v["switched"], false);
        assert_eq!(v["error"], "switching-disabled");
        // Nothing posted.
        assert!(s.switch.take().is_none());
    }

    #[test]
    fn switch_session_posts_request_for_a_live_pane_when_enabled() {
        let (s, pane) = state_with_live_pane();
        s.switch.attach_sink();
        let v = s.query(&switch_query(&pane)).expect("query ok");
        assert_eq!(v["switched"], true);
        assert_eq!(v["pane_id"], pane);
        // The request landed in the channel for the event loop.
        let posted = s.switch.take().expect("a switch must be pending");
        assert_eq!(posted.to_string(), pane);
    }

    #[test]
    fn switch_session_unknown_pane_reports_no_such_pane() {
        // A well-formed but non-existent pane id: refuse rather than
        // land the loop on an empty subscribe.
        let (s, _live) = state_with_live_pane();
        s.switch.attach_sink();
        let ghost = tear_types::PaneId::from_seed("never-created").to_string();
        let v = s.query(&switch_query(&ghost)).expect("query ok");
        assert_eq!(v["switched"], false);
        assert_eq!(v["error"], "no-such-pane");
        assert!(s.switch.take().is_none());
    }

    #[test]
    fn switch_session_malformed_pane_is_typed_error() {
        let s = state();
        s.switch.attach_sink();
        let err = s
            .query(&switch_query("not-hex-zzz"))
            .expect_err("malformed pane id must not post");
        assert!(matches!(err, QueryError::Internal { .. }), "got {err:?}");
        assert!(s.switch.take().is_none());
    }

    #[test]
    fn switch_session_bad_arity_and_type_are_typed_errors() {
        let s = state();
        let err = s
            .query(&Query::call(["switch_session"], []))
            .expect_err("zero args must be BadArity");
        assert!(
            matches!(err, QueryError::BadArity { expected: 1, actual: 0 }),
            "got {err:?}"
        );
        let err = s
            .query(&Query::call(["switch_session"], [serde_json::json!(42)]))
            .expect_err("non-string arg must be TypeMismatch");
        assert!(matches!(err, QueryError::TypeMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn schema_advertises_switch_session() {
        assert!(state().schema().contains(&"switch_session"));
    }

    #[test]
    fn save_session_as_preset_captures_the_panes_session() {
        use tear_types::{MultiplexerControl, SessionSource};
        let s = state();
        let inproc = Arc::new(tear_core::InProcess::new());
        let sid = inproc
            .new_session_with_source_and_size(
                "save-test",
                "/bin/sh",
                SessionSource::Named("save-test".into()),
                (80, 24),
            )
            .expect("spawn session");
        let pane = inproc
            .with_registry(|r| {
                r.sessions
                    .get(&sid)
                    .and_then(|sess| sess.panes.keys().next().copied())
            })
            .expect("session has a pane");
        // praça with the session indexed → capture finds its project root.
        let mut praca = praca::Praca::new();
        praca.index.upsert(praca::SessionRecord::for_project(
            sid,
            std::path::PathBuf::from("/code/pleme-io/mado"),
            ishou_tokens::SessionNameStyle::Emoji,
            1000,
        ));
        s.set_tear_inproc(Arc::clone(&inproc));
        s.set_praca(Arc::new(Mutex::new(praca)));

        let v = s
            .query(&Query::call(
                ["save_session_as_preset"],
                [serde_json::json!(pane.to_string())],
            ))
            .expect("query ok");
        assert_eq!(v["saved"], true);
        assert_eq!(v["pane_id"], pane.to_string());
    }

    #[test]
    fn save_session_as_preset_unknown_pane_reports_no_such_pane() {
        let (s, _live) = state_with_live_pane();
        s.set_praca(Arc::new(Mutex::new(praca::Praca::new())));
        let ghost = tear_types::PaneId::from_seed("never-created").to_string();
        let v = s
            .query(&Query::call(
                ["save_session_as_preset"],
                [serde_json::json!(ghost)],
            ))
            .expect("query ok");
        assert_eq!(v["saved"], false);
        assert_eq!(v["error"], "no-such-pane");
    }

    #[test]
    fn schema_advertises_save_session_as_preset() {
        assert!(state().schema().contains(&"save_session_as_preset"));
    }
}
