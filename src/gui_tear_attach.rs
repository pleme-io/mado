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

use crate::config::{MadoConfig, MadoTearConfig, TearMode, TearRuntime};
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
    // M3c.1 — branch on TearRuntime. Embedded skips the daemon
    // entirely; tear's PTY+grid live in-process inside mado.
    // ghostty-class latency (no Unix socket hop, no second VT
    // parser, no inter-process rwlock contention). Multi-attach
    // scenarios (ayatsuri overlay, namimado debug, remote ssh)
    // need the daemon; embedded is for the default
    // single-window case the operator opens 99% of the time.
    if matches!(config.tear.runtime, TearRuntime::Embedded) {
        return try_run_default_embedded(config, shell);
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

/// Refactored unified renderer + event loop for both daemon-mode
/// and embedded-mode tear backends. Generic over:
///
///   * `P` — the engate Producer (tear_client or tear_core impl)
///   * `C` — the control plane (Client or InProcess, both impl
///           MultiplexerControl for resize/send_keys/kill_session)
///
/// `owned_session_id`:
///   * `Some(sid)` — daemon mode: mado owns the session, reaps on
///     CloseRequested + on SIGTERM via ctrlc handler
///   * `None` — embedded mode: the session lives in mado's process,
///     dies naturally on process exit; no reap needed
///
/// This collapses what was previously 320 lines of duplicated daemon
/// + embedded paths into one ~150-line function. Prime directive
/// duplication-is-a-bug satisfied. Embedded mode also picks up
/// features the daemon path had (mouse-cursor snow deflection, snow
/// pulse on keypress) for free.
fn run_against_pane_unified<P, C>(
    producer: P,
    control: Arc<C>,
    pane_id: PaneId,
    snapshot_cols: usize,
    snapshot_rows: usize,
    config: MadoConfig,
    owned_session_id: Option<tear_types::SessionId>,
    title_kind: &str,
) -> Result<()>
where
    P: engate_attach::Producer<
            Item = Vec<u8>,
            Snap = tear_types::engate_wrap::PaneSnapshotWrap,
        > + 'static,
    C: tear_types::MultiplexerControl + Send + Sync + 'static,
{
    use madori::{AppConfig, AppEvent, EventResponse, KeyEvent};

    let cols = snapshot_cols.max(1);
    let rows = snapshot_rows.max(1);

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
    renderer.set_snow_config(config.effects.snow.clone());

    // engate typed Attach lifecycle — same shape both backends.
    // The TerminalSink writeback path closes the DSR/DA/OSC query
    // loop: when the shell (frost, frostmourne, bash, zsh) sends
    // `\x1b[6n` (cursor position query) etc., mado's VT engine
    // generates the response, and this writer forwards it back to
    // the tear pane's PTY. Without this, reedline-based shells
    // (frost) time out with "cursor position could not be read".
    let control_for_response_writer = Arc::clone(&control);
    let response_writer: crate::engate_consumer::ResponseWriter = Arc::new(
        move |bytes: &[u8]| {
            let _ = control_for_response_writer.send_keys(pane_id, bytes);
        },
    );
    let consumer = crate::engate_consumer::TerminalSink::new(
        Arc::clone(&terminal),
        response_writer,
    );
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

    let _attach_thread = std::thread::Builder::new()
        .name(format!("mado-engate-live-{title_kind}"))
        .spawn(move || {
            let _consumer = attach_live.run();
        })
        .ok();

    // Initial size-sync: push pane_resize_absolute BEFORE the event
    // loop so tear's default 80×24 doesn't briefly hold while the
    // shell runs zshrc + renders its first prompt. Mado is the size
    // authority.
    {
        let logical_w = config.window.width as f32;
        let logical_h = config.window.height as f32;
        let pad = padding;
        let cell_w_logical = effective_font_size * 0.6;
        let cell_h_logical = effective_font_size * 1.4;
        let init_cols: u16 =
            (((logical_w - 2.0 * pad) / cell_w_logical).floor() as u16).max(1);
        let init_rows: u16 =
            (((logical_h - 2.0 * pad) / cell_h_logical).floor() as u16).max(1);
        if init_cols as usize != cols || init_rows as usize != rows {
            if let Err(e) = control.pane_resize_absolute(pane_id, init_cols, init_rows) {
                tracing::warn!(error = %e, init_cols, init_rows, "initial pane_resize_absolute failed");
            }
        }
    }

    // SIGTERM/SIGINT reaper — only when mado owns the session.
    // Embedded mode doesn't need this; the in-process tear runtime
    // dies with mado.
    if let Some(sid) = owned_session_id {
        let reap_control = Arc::clone(&control);
        ctrlc::set_handler(move || {
            tracing::info!(session = %sid, "signal received — reaping owned tear session");
            let _ = reap_control.kill_session(sid);
            std::process::exit(130);
        })
        .ok();
    }

    let control_for_events = Arc::clone(&control);
    let app_config = AppConfig {
        title: format!("mado · {title_kind} {pane_id}"),
        width: config.window.width,
        height: config.window.height,
        resizable: true,
        vsync: config.performance.vsync,
        transparent: false,
        decorations: config.window.decorations,
    };
    crate::perf::log_phase("event_loop_entering");
    let session_reap_target = std::sync::Arc::new(std::sync::Mutex::new(owned_session_id));
    let control_for_reap = Arc::clone(&control);
    let session_reap = Arc::clone(&session_reap_target);
    // Tear-mode keybindings. Use the operator-prescribed default
    // baseline (with_mado_defaults). Per the three-tier model in
    // MadoConfig: bare → bare+discovered → bare+defaults+discovered;
    // here we're in the third tier. Operator overrides via
    // mado.yaml are loaded into the typed keybinds config but the
    // tear-mode event loop only dispatches the action-firing
    // subset relevant in single-pane tear mode (font zoom +
    // fullscreen + future). Other actions like Copy/Paste need
    // mado-side state we don't have here yet — they fall through
    // to PTY forwarding (terminal apps handle them).
    let keybinds = crate::keybind::KeybindManager::with_mado_defaults();
    // Per-action debouncer for OS key-repeat storms. Default 80ms
    // window — OS key-repeat is ~30-50ms, this drops storm-ticks
    // while allowing 12 intentional presses per second. Per the
    // 2026-05-21 runaway-font incident (Cmd-= held → font grew
    // 14 → 32pt in 1.5s); the gate caps that to ~19 transitions
    // and `BoundedFontSize` caps the final value at FONT_MAX = 64.
    let mut key_repeat_gate =
        awase::KeyRepeatGate::<crate::keybind::Action>::new();
    let default_font_size_for_reset = config.font_size;
    madori::App::builder(renderer)
        .config(app_config)
        .on_event(move |event, renderer| -> EventResponse {
            match event {
                AppEvent::Resized { width, height } => {
                    let (cols, rows) = renderer.cells_for_window_phys(*width, *height);
                    let _ = control_for_events.pane_resize_absolute(pane_id, cols, rows);
                    EventResponse::ignored()
                }
                AppEvent::Mouse(madori::MouseEvent::Moved { x, y }) => {
                    renderer.snow_set_cursor(*x as f32, *y as f32);
                    EventResponse::ignored()
                }
                AppEvent::Key(key_event @ KeyEvent { pressed: true, .. }) => {
                    renderer.snow_pulse_typing();
                    // First: consult the prescribed keybinding map.
                    // Cmd-= / Cmd-- / Cmd-0 → font zoom in/out/reset
                    // BEFORE the text falls through to the PTY.
                    if let Some(action) = keybinds.lookup_madori(key_event) {
                        use crate::font_size::{BoundedFontSize, FontSizeSteps};
                        use crate::keybind::Action;
                        // Font scaling: rate-limit + bound by type.
                        // The gate drops OS key-repeat storms; the
                        // BoundedFontSize newtype saturates at
                        // FONT_MAX so even a passed storm can't
                        // explode the font onscreen.
                        let scale_action = matches!(
                            action,
                            Action::FontIncrease | Action::FontDecrease | Action::FontReset
                        );
                        if scale_action && !key_repeat_gate.try_pass(action) {
                            // Storm tick — drop silently. Consume so
                            // the keystroke doesn't fall through to
                            // the PTY as literal text.
                            return EventResponse::consumed();
                        }
                        match action {
                            Action::FontIncrease => {
                                let new_size = BoundedFontSize::new(renderer.font_size())
                                    .inc_step()
                                    .get();
                                renderer.set_font_size(new_size);
                                // Push the new grid dims to tear so the
                                // shell sees the right cols/rows after
                                // the cell-size change.
                                let (cols, rows) = renderer
                                    .cells_for_window_phys(config.window.width, config.window.height);
                                let _ = control_for_events
                                    .pane_resize_absolute(pane_id, cols, rows);
                                return EventResponse::consumed();
                            }
                            Action::FontDecrease => {
                                let new_size = BoundedFontSize::new(renderer.font_size())
                                    .dec_step()
                                    .get();
                                renderer.set_font_size(new_size);
                                let (cols, rows) = renderer
                                    .cells_for_window_phys(config.window.width, config.window.height);
                                let _ = control_for_events
                                    .pane_resize_absolute(pane_id, cols, rows);
                                return EventResponse::consumed();
                            }
                            Action::FontReset => {
                                let reset_size = BoundedFontSize::new(default_font_size_for_reset).get();
                                renderer.set_font_size(reset_size);
                                let (cols, rows) = renderer
                                    .cells_for_window_phys(config.window.width, config.window.height);
                                let _ = control_for_events
                                    .pane_resize_absolute(pane_id, cols, rows);
                                return EventResponse::consumed();
                            }
                            // Other actions (Copy, Paste, search,
                            // scroll, fullscreen, etc.) fall through
                            // to PTY forwarding for now; terminal
                            // apps + mado-side state wiring lands in
                            // a follow-up.
                            _ => {}
                        }
                    }
                    // No bound action AND no platform modifier is held
                    // (avoid forwarding `=` to PTY when the operator
                    // typed Cmd-+ but the binding wasn't recognized).
                    let mods = key_event.modifiers;
                    let has_platform_mod = mods.meta || mods.ctrl;
                    if has_platform_mod {
                        return EventResponse::consumed();
                    }
                    if let Some(t) = &key_event.text {
                        if !t.is_empty() {
                            let _ = control_for_events.send_keys(pane_id, t.as_bytes());
                        }
                    }
                    EventResponse::consumed()
                }
                AppEvent::CloseRequested => {
                    if let Ok(mut slot) = session_reap.lock() {
                        if let Some(sid) = slot.take() {
                            match control_for_reap.kill_session(sid) {
                                Ok(()) => tracing::info!(session = %sid, "reaped owned tear session on window close"),
                                Err(e) => tracing::warn!(error = %e, session = %sid, "kill_session on close failed"),
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
    Ok(())
}

/// M3c.1 — embedded-tear default-launch path.
///
/// Identical operator-visible contract to `try_run_default` but
/// runs tear's PTY + grid + VT parser IN-PROCESS via
/// `tear_core::InProcess`. No daemon spawn, no Unix socket, no
/// inter-process IPC. The engate Producer impl on InProcess
/// delivers PTY bytes directly to mado's TerminalSink Consumer
/// without crossing a process boundary; latency drops from
/// ~25-45ms (daemon path) to ~16ms (ghostty-class single-process).
///
/// Trade-off: single-attach only. ayatsuri overlays, namimado-debug,
/// and remote ssh-mux scenarios require the daemon. Operator opts
/// into Daemon via `mado.tear.runtime = "daemon"` for those.
fn try_run_default_embedded(config: MadoConfig, shell: String) -> TearDefaultOutcome {
    use std::sync::Arc;
    use tear_core::engate_producer::PaneProducer as EmbeddedProducer;
    use tear_core::InProcess;
    use tear_types::SessionSource;

    let inproc = Arc::new(InProcess::new());
    crate::perf::log_phase("tear_inproc_constructed");

    let session_name = config
        .tear
        .session_name
        .clone()
        .unwrap_or_else(default_session_name);

    let cell_w_logical = config.font_size * 0.6;
    let cell_h_logical = config.font_size * 1.4;
    let pad_logical = config.window.padding as f32;
    let init_cols = (((config.window.width as f32 - 2.0 * pad_logical) / cell_w_logical)
        .floor() as u16)
        .max(1);
    let init_rows = (((config.window.height as f32 - 2.0 * pad_logical) / cell_h_logical)
        .floor() as u16)
        .max(1);

    let session_id = match inproc.new_session_with_source_and_size(
        &session_name,
        &shell,
        SessionSource::Named("mado-embedded".into()),
        (init_cols, init_rows),
    ) {
        Ok(sid) => sid,
        Err(e) => {
            tracing::warn!(error = %e, "InProcess::new_session_with_source_and_size failed in embedded mode");
            return TearDefaultOutcome::Unavailable;
        }
    };
    let pane_id = match inproc.with_registry(|r| {
        r.sessions
            .get(&session_id)
            .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
    }) {
        Some(id) => id,
        None => {
            tracing::warn!("embedded session has no pane");
            return TearDefaultOutcome::Unavailable;
        }
    };
    tracing::info!(
        session = %session_id,
        pane = %pane_id,
        name = %session_name,
        mode = "embedded",
        "mado embedded: tear InProcess session created"
    );
    crate::perf::log_phase("tear_session_created");

    match run_against_embedded_pane(inproc, pane_id, config) {
        Ok(()) => TearDefaultOutcome::Ran,
        Err(e) => TearDefaultOutcome::Error(e),
    }
}

/// Embedded-mode renderer + event loop — thin wrapper around
/// `run_against_pane_unified`. Constructs the `tear_core` engate
/// Producer + uses `Arc<InProcess>` as the control plane; the
/// unified function handles the rest.
fn run_against_embedded_pane(
    inproc: std::sync::Arc<tear_core::InProcess>,
    pane_id: PaneId,
    config: MadoConfig,
) -> Result<()> {
    let snapshot = inproc
        .pane_snapshot(pane_id)
        .with_context(|| format!("inproc.pane_snapshot({pane_id})"))?;
    let producer =
        tear_core::engate_producer::PaneProducer::new(Arc::clone(&inproc), pane_id);
    run_against_pane_unified(
        producer,
        inproc,
        pane_id,
        snapshot.cols,
        snapshot.rows,
        config,
        None, // embedded session dies with mado; no reap needed
        "tear[embedded]",
    )
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
/// Daemon-mode renderer + event loop — thin wrapper around
/// `run_against_pane_unified`. Constructs the `tear_client` engate
/// Producer + uses `Arc<Client>` as the control plane; the unified
/// function handles the rest including the SIGTERM/CloseRequested
/// reap path for the owned session.
fn run_against_pane(
    client: Arc<tear_client::Client>,
    pane_id: PaneId,
    owned_session_id: Option<tear_types::SessionId>,
    _socket_path: PathBuf,
    config: MadoConfig,
) -> Result<()> {
    let snapshot = client
        .pane_snapshot(pane_id)
        .with_context(|| format!("pane_snapshot({pane_id})"))?;
    let producer =
        tear_client::engate_producer::PaneProducer::new(Arc::clone(&client), pane_id);
    run_against_pane_unified(
        producer,
        client,
        pane_id,
        snapshot.cols,
        snapshot.rows,
        config,
        owned_session_id,
        "tear",
    )
}
