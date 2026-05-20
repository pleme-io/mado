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

    // Compute the desired pane size up front from the operator's
    // configured window dimensions + font cell metrics, then pass
    // it to new_session_with_source_and_size so the shell spawns
    // at the right grid from t=0. Eliminates the brief 80×24
    // default + post-attach SIGWINCH re-layout.
    let cell_w_logical = config.font_size * 0.6;
    let cell_h_logical = config.font_size * 1.4;
    let pad_logical = config.window.padding as f32;
    let init_cols = (((config.window.width as f32 - 2.0 * pad_logical) / cell_w_logical)
        .floor() as u16)
        .max(1);
    let init_rows = (((config.window.height as f32 - 2.0 * pad_logical) / cell_h_logical)
        .floor() as u16)
        .max(1);
    let session_id = match client.new_session_with_source_and_size(
        &session_name,
        &shell,
        SessionSource::Named("mado".into()),
        (init_cols, init_rows),
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

    // SIGTERM / SIGINT reaper. winit's CloseRequested only fires
    // when the user closes the window — `kill mado` / `timeout
    // mado` / launchd-restart bypasses winit entirely. A signal
    // handler that holds a clone of the client + session_id reaps
    // before the process exits, so orphans don't pile up even
    // under abnormal termination.
    {
        let reap_client = Arc::clone(&client);
        let sid = session_id;
        ctrlc::set_handler(move || {
            tracing::info!(session = %sid, "signal received — reaping owned tear session");
            let _ = reap_client.kill_session(sid);
            std::process::exit(130);  // 128 + SIGINT
        })
        .ok();  // ok() — second mado in the same process would
                // double-register; the first wins. ctrlc::Error
                // here is non-fatal (reap-on-close still works).
    }

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

    // ── engate M3c: typed Attach lifecycle ──────────────────────────
    //
    // The subscribe + replay + feed dance previously lived as a
    // ~10-line closure. With engate the same shape becomes a typed
    // lifecycle that the compiler enforces:
    //
    //   builder() -> Attach<Spawned>
    //     .subscribe()  -> (Attach<Subscribed>, History)
    //     .replay(history) -> Attach<Synced>
    //     .start_live() -> Attach<Live>
    //
    // Attach<Live> is the only phase that delivers items; reaching
    // it requires consuming the History (drop-bomb otherwise) which
    // forces the snapshot-replay-before-live invariant by
    // construction. The engate M0 daemon-side snapshot replay is
    // STILL delivered as the first item in the live stream — engate
    // sees it as just another item and feeds it through the same
    // Consumer::consume path the live items use; idempotent at the
    // VT-parser level.
    let producer =
        tear_client::engate_producer::PaneProducer::new(Arc::clone(&client), pane_id);
    let consumer = crate::engate_consumer::TerminalSink::new(Arc::clone(&terminal));
    let attach_builder = engate_attach::Attach::builder()
        .producer(producer)
        .consumer(consumer)
        .build();
    let (attach_subscribed, history) = attach_builder
        .subscribe()
        .with_context(|| format!("engate.subscribe({pane_id})"))?;
    let attach_synced = attach_subscribed
        .replay(history)
        .with_context(|| format!("engate.replay({pane_id})"))?;
    let attach_live = attach_synced.start_live();
    crate::perf::log_phase("pane_subscribed");

    // Drain the live engate stream on a background thread. The
    // engate Attach<Live> owns the receiver + consumer; run()
    // blocks until the producer drops its sender (PaneClosed /
    // daemon shutdown / network drop), then returns the consumer
    // for any final-state inspection the caller wants.
    //
    // The handle is dropped on function exit, but by then the
    // attach is either still running (held by the spawned thread)
    // or has cleanly terminated. Either way the typestate guarantees
    // no half-attached state escapes into the renderer.
    let _attach_thread = std::thread::Builder::new()
        .name("mado-engate-live".into())
        .spawn(move || {
            let _consumer = attach_live.run();
        })
        .ok();

    // Initial size-sync: push pane_resize_absolute BEFORE the
    // event loop starts so tear's default 80×24 doesn't briefly
    // hold while the shell runs zshrc + renders its first prompt.
    // We estimate cols/rows from config.window.{width,height}
    // (operator-authored logical pixels) and the renderer's
    // logical cell metrics. This is close enough that the
    // shell's TIOCGWINSZ on first prompt-render gets the right
    // number, and the FIRST winit Resized event (which fires
    // shortly after window create) corrects any small drift via
    // the precise physical-pixel path below.
    {
        let logical_w = config.window.width as f32;
        let logical_h = config.window.height as f32;
        let pad = padding;
        let cell_w_logical = effective_font_size * 0.6;
        let cell_h_logical = effective_font_size * 1.4;
        let init_cols: u16 = (((logical_w - 2.0 * pad) / cell_w_logical).floor() as u16).max(1);
        let init_rows: u16 = (((logical_h - 2.0 * pad) / cell_h_logical).floor() as u16).max(1);
        if init_cols as usize != cols as usize || init_rows as usize != rows as usize {
            if let Err(e) = client.pane_resize_absolute(pane_id, init_cols, init_rows) {
                tracing::warn!(
                    error = %e,
                    init_cols, init_rows,
                    "initial pane_resize_absolute failed; tear stays at snapshot size"
                );
            } else {
                tracing::info!(
                    init_cols, init_rows,
                    "initial pane_resize_absolute pushed (mado is size authority)"
                );
            }
        }
    }

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
                    // ARCHITECTURE: mado is the size authority. The
                    // renderer knows the exact PHYSICAL cell dims +
                    // scale factor; cells_for_window_phys is the
                    // single source of truth. Push the result to
                    // tear so the daemon's pane size mirrors mado's
                    // visible grid — and the child shell sees the
                    // correct cols/rows via TIOCGWINSZ. Without
                    // this match, nvim and other TUI apps render
                    // at the wrong size.
                    let (cols, rows) = renderer.cells_for_window_phys(*width, *height);
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
    // _attach_thread is the engate worker — it owns the Attach<Live>
    // and the consumer; dropping the JoinHandle here lets the OS
    // reap when the producer's sender closes. The engate typestate
    // already guarantees there's no half-attached state to clean up.
    let _ = socket_path; // used in window title in a future iteration
    Ok(())
}
