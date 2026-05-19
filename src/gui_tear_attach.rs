//! Phase-3.1 GPU GUI mode for `mado tear-attach --gpu`, AND the
//! default-launch path that auto-attaches to a freshly-created tear
//! session when one is reachable (`try_run_default`).
//!
//! Opens a real mado GPU window backed by:
//! - A single `Terminal` (no WindowState, no PaneManager, no local PTY).
//! - A tear-client subscription that feeds PTY bytes into the
//!   Terminal as they arrive from `tear-daemon`.
//! - Mado's existing `TerminalRenderer` in its single-pane fallback
//!   path (`window: None`) — same GPU pipeline that renders a local
//!   shell today.
//! - Keystroke forwarding: KeyEvent → `client.send_keys(pane, bytes)`.
//! - Resize forwarding: window resize → cell-dim math →
//!   `client.pane_resize_absolute(pane, cols, rows)`.
//!
//! Deliberate non-goals for the MVP:
//! - No special-key translation (arrows / function / chord keys);
//!   only `KeyEvent.text` (UTF-8 char input) is forwarded. Phase
//!   3.1.1 will port mado's existing key-table.
//! - No clipboard / selection / search / Rhai scripts.
//! - No multi-pane (that's tear's job — this mode renders one
//!   tear pane in one mado window).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use tear_types::{MultiplexerControl, PaneId, SessionSource};

use crate::config::{MadoConfig, MadoTearConfig, TearMode};
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::tear_discovery::{discover, DiscoveryOutcome};
use crate::terminal::{Color as TermColor, Terminal};

/// Result of attempting to start mado in default tear-attached mode.
pub enum TearDefaultOutcome {
    /// Tear-attached event loop ran to completion (window closed).
    /// Main should exit normally.
    Ran,
    /// Daemon unavailable and config allowed fallback. Main should
    /// fall through to the local-PTY path.
    Unavailable,
    /// Hard error — propagate to the operator.
    Error(anyhow::Error),
}

/// Entry point — assembles the GUI from a connected tear-client +
/// a subscribed pane id, then runs the madori::App event loop.
/// This is the `mado tear-attach --gpu <pane>` path.
pub fn run(pane_id: PaneId, socket_path: PathBuf) -> Result<()> {
    let config = crate::config::load(&None).unwrap_or_default();

    // ── Tear control connection — discovery-driven ────────────
    let tear_cfg = MadoTearConfig {
        socket: Some(socket_path.clone()),
        // CLI invocation = explicit user intent to attach.
        mode: match config.tear.mode {
            TearMode::Never => TearMode::Auto,
            other => other,
        },
        ..config.tear.clone()
    };
    let (client, _resolved_socket) = match discover(&tear_cfg) {
        DiscoveryOutcome::Attached(c, p) => (Arc::new(c), p),
        DiscoveryOutcome::Required(msg) => {
            return Err(anyhow::anyhow!("{msg}"));
        }
        DiscoveryOutcome::Fallback => {
            return Err(anyhow::anyhow!(
                "tear-daemon not reachable at {} (tear.mode={:?}, auto_spawn={}).\n\
                 Start one with `tear daemon` or set `tear.auto_spawn = true`.",
                socket_path.display(),
                tear_cfg.mode,
                tear_cfg.auto_spawn
            ));
        }
    };

    impose_if_any(&client, &tear_cfg);
    // CLI `mado tear-attach <pane>` is attaching to a session
    // somebody else created — don't kill it on our close.
    run_against_pane(client, pane_id, None, socket_path, config)
}

/// Default-launch path: try to attach to (or auto-spawn) the tear
/// daemon, create a fresh session named for this mado instance, and
/// render its first pane in a GPU window. Returns `Unavailable` if
/// tear is configured `Never` or the daemon is unreachable + spawn
/// failed AND fallback is allowed — main should then run the local-
/// PTY path. Returns `Error` for hard failures (config says
/// `Always` but daemon dead, session create failed, etc.).
pub fn try_run_default(config: MadoConfig, shell: String) -> TearDefaultOutcome {
    if matches!(config.tear.mode, TearMode::Never) {
        return TearDefaultOutcome::Unavailable;
    }
    let (client, socket_path) = match discover(&config.tear) {
        DiscoveryOutcome::Attached(c, p) => (Arc::new(c), p),
        DiscoveryOutcome::Fallback => return TearDefaultOutcome::Unavailable,
        DiscoveryOutcome::Required(msg) => {
            return TearDefaultOutcome::Error(anyhow::anyhow!("{msg}"));
        }
    };
    crate::perf::log_phase("tear_daemon_discovered");

    // Impose ASAP — before the session exists, so the new pane
    // inherits prefix/shell/scrollback knobs the operator declared.
    impose_if_any(&client, &config.tear);

    // Session name: explicit override OR auto-generated tag that
    // distinguishes per-process while staying human-readable.
    let session_name = config
        .tear
        .session_name
        .clone()
        .unwrap_or_else(default_session_name);

    let session_id = match client.new_session_with_source(
        &session_name,
        &shell,
        SessionSource::Named("mado".into()),
    ) {
        Ok(sid) => sid,
        Err(e) => {
            tracing::warn!(error = %e, "tear new_session failed; falling back to local PTY");
            return TearDefaultOutcome::Unavailable;
        }
    };

    let session = match client.get_session(session_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "tear get_session failed; falling back");
            return TearDefaultOutcome::Unavailable;
        }
    };

    let pane_id = match session
        .windows
        .values()
        .next()
        .map(|w| w.active_pane)
    {
        Some(id) => id,
        None => {
            tracing::warn!(
                session = %session_id,
                "tear session created without any windows; falling back"
            );
            return TearDefaultOutcome::Unavailable;
        }
    };

    tracing::info!(
        session = %session_id,
        pane = %pane_id,
        name = %session_name,
        socket = %socket_path.display(),
        "mado default: tear session created + attached"
    );
    crate::perf::log_phase("tear_session_created");

    // We own this session — kill it when our window closes so
    // it doesn't accumulate as an orphan in the daemon. The
    // `mado tear-attach <existing-pane>` CLI path does NOT pass
    // session_id (it's attaching to someone else's session,
    // shouldn't reap it on close).
    match run_against_pane(client, pane_id, Some(session_id), socket_path, config) {
        Ok(()) => TearDefaultOutcome::Ran,
        Err(e) => TearDefaultOutcome::Error(e),
    }
}

/// Default session name: `mado-<unix-seconds>-<pid>`. Stable
/// per-process; sortable by creation time in `tear list`.
fn default_session_name() -> String {
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("mado-{ts}-{pid}")
}

/// If the operator declared `tear.impose.*` overrides, fetch the
/// daemon's current TearConfig, merge in the overrides, and push
/// the result back via SetConfig. Errors are logged + non-fatal —
/// failing to impose shouldn't break attach.
fn impose_if_any(client: &Arc<tear_client::Client>, tear_cfg: &MadoTearConfig) {
    let Some(impose) = tear_cfg.impose.as_ref() else { return };
    if !impose.has_any_override() {
        return;
    }
    match client.get_config() {
        Ok(mut current) => {
            impose.apply_to(&mut current);
            if let Err(e) = client.set_config(&current) {
                tracing::warn!(error = %e, "set_config (impose) failed");
            } else {
                tracing::info!("imposed mado-authored TearConfig overrides on daemon");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "get_config failed; skipping impose");
        }
    }
}

/// The shared render loop: snapshot pane size, build Terminal +
/// Renderer, subscribe to pane bytes, run the madori App loop with
/// key + resize forwarding via tear-client. Called by both `run()`
/// (CLI tear-attach, no owned session) and `try_run_default()`
/// (default launch, owns the session and will reap it on close).
fn run_against_pane(
    client: Arc<tear_client::Client>,
    pane_id: PaneId,
    owned_session_id: Option<tear_types::SessionId>,
    socket_path: PathBuf,
    config: MadoConfig,
) -> Result<()> {
    use madori::{AppConfig, AppEvent, EventResponse, KeyEvent};

    let snapshot = client
        .pane_snapshot(pane_id)
        .with_context(|| format!("pane_snapshot({pane_id})"))?;
    let cols = snapshot.cols.max(1);
    let rows = snapshot.rows.max(1);

    let terminal: SharedTerminal = Arc::new(RwLock::new(
        Terminal::with_scrollback(cols, rows, 10_000),
    ));

    let effective_font_size = config.font_size;
    let padding = config.window.padding as f32;
    let bg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.background)
        .unwrap_or(ishou_tokens::Srgb::new(0x2e, 0x34, 0x40));
    let bg_color: wgpu::Color = bg_srgb
        .to_linear()
        .with_alpha(config.appearance.opacity)
        .into();
    let fg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.foreground)
        .unwrap_or(ishou_tokens::Srgb::new(0xec, 0xef, 0xf4));
    let cursor_blink = config.cursor.blink && !config.accessibility.reduce_motion;
    let mut renderer = TerminalRenderer::new(
        Arc::clone(&terminal),
        effective_font_size,
        config.font_family.clone(),
        config.font_italic.clone(),
        padding,
        config.cursor.style,
        cursor_blink,
        config.cursor.blink_rate_ms,
        bg_color,
        TermColor::new(fg_srgb.r, fg_srgb.g, fg_srgb.b),
    );
    // Snow overlay is enabled by default in MadoConfig — flow it
    // through here too so the tear-attached window is visually
    // consistent with the local-PTY default.
    renderer.set_snow_config(config.effects.snow.clone());

    let terminal_for_sub = Arc::clone(&terminal);
    let _subscribe = client
        .subscribe_pane_bytes(pane_id, move |bytes| {
            terminal_for_sub.write().feed(bytes);
        })
        .with_context(|| format!("subscribe_pane_bytes({pane_id})"))?;
    crate::perf::log_phase("pane_subscribed");

    let client_for_events = Arc::clone(&client);
    let app_config = AppConfig {
        title: format!("mado · tear {pane_id}"),
        width: config.window.width,
        height: config.window.height,
        resizable: true,
        vsync: config.performance.vsync,
        transparent: false,
        decorations: config.window.decorations,
    };
    let cell_w_logical = effective_font_size * 0.6;
    let cell_h_logical = effective_font_size * 1.4;
    crate::perf::log_phase("event_loop_entering");
    // Wrap the owned session id so we can move it into the
    // event closure (CloseRequested reaps).
    let session_reap_target = std::sync::Arc::new(std::sync::Mutex::new(owned_session_id));
    let client_for_reap = Arc::clone(&client);
    let session_reap = Arc::clone(&session_reap_target);
    madori::App::builder(renderer)
        .config(app_config)
        .on_event(move |event, renderer| -> EventResponse {
            match event {
                AppEvent::Resized { width, height } => {
                    let cols = ((*width as f32 / cell_w_logical) as u16).max(1);
                    let rows = ((*height as f32 / cell_h_logical) as u16).max(1);
                    let _ = client_for_events.pane_resize_absolute(pane_id, cols, rows);
                    EventResponse::ignored()
                }
                AppEvent::Mouse(madori::MouseEvent::Moved { x, y }) => {
                    // Snow cursor deflection — applies in tear mode too.
                    renderer.snow_set_cursor(*x as f32, *y as f32);
                    EventResponse::ignored()
                }
                AppEvent::Key(KeyEvent {
                    pressed: true,
                    text,
                    ..
                }) => {
                    // Visual feedback: pulse the snow shimmer on every keystroke.
                    renderer.snow_pulse_typing();
                    // MVP: only forward text-producing keys. Special
                    // keys (arrows, ctrl chords, function keys) need
                    // the mado main.rs key-table port.
                    if let Some(t) = text {
                        if !t.is_empty() {
                            let _ = client_for_events.send_keys(pane_id, t.as_bytes());
                        }
                    }
                    EventResponse::consumed()
                }
                AppEvent::CloseRequested => {
                    // Reap our owned tear session so it doesn't
                    // accumulate as an orphan in the daemon. The
                    // take() makes this idempotent — second
                    // CloseRequested is a no-op.
                    if let Ok(mut slot) = session_reap.lock() {
                        if let Some(sid) = slot.take() {
                            match client_for_reap.kill_session(sid) {
                                Ok(()) => tracing::info!(
                                    session = %sid,
                                    "reaped owned tear session on window close"
                                ),
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    session = %sid,
                                    "kill_session on close failed"
                                ),
                            }
                        }
                    }
                    EventResponse::ignored()
                }
                _ => EventResponse::ignored(),
            }
        })
        .run()
        .map_err(|e| anyhow::anyhow!("madori::App run: {e}"))?;
    drop(_subscribe);
    let _ = socket_path; // used in window title in a future iteration
    Ok(())
}
