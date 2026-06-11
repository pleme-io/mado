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
//! Since M1 (2026-06-11) every input/UX capability — keystroke
//! translation, selection + clipboard, search + dir-picker overlays,
//! full mouse forwarding, kitty CSI-u, focus events, IME, font zoom,
//! the PTY-grid⇄display reconciler — runs through the shared
//! `ux::InputEngine`, identical code to the local-PTY loop in
//! `main.rs` (the pre-M1 second copy of the UX logic this file
//! carried is gone; `tests/ux_unification.rs` pins that).
//! This file only assembles the tear transport (engate attach,
//! session lifecycle, reap) and adapts events to the engine.
//!
//! Deliberate non-goal: no multi-pane (that's tear's job — this
//! mode renders one tear pane in one mado window).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use hasami::ClipboardProvider;
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
    // somebody else created — don't kill it on our close. No
    // kanshou server runs on this path, so no injection queue.
    run_against_pane(client, pane_id, None, socket_path, config, None)
}

/// Default-launch path: try to attach to (or auto-spawn) the tear
/// daemon, create a fresh session named for this mado instance, and
/// render its first pane in a GPU window. Returns `Unavailable` if
/// tear is configured `Never` or the daemon is unreachable + spawn
/// failed AND fallback is allowed — main should then run the local-
/// PTY path. Returns `Error` for hard failures (config says
/// `Always` but daemon dead, session create failed, etc.).
pub fn try_run_default(
    config: MadoConfig,
    shell: String,
    kanshou_state: std::sync::Arc<crate::kanshou_state::MadoAppState>,
) -> TearDefaultOutcome {
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
        return try_run_default_embedded(config, shell, kanshou_state);
    }
    // Daemon path keeps a handle on the kanshou-published injection
    // queue so `simulate_chord` works in both tear runtimes.
    let injected = kanshou_state.injected.clone();
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
    match run_against_pane(client, pane_id, Some(session_id), socket_path, config, Some(injected)) {
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
    injected: Option<crate::action_injection::InjectedActions>,
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
        config.font_symbols.clone(),
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
            // A dropped VT-query answer kills reedline-based shells (fatal
            // CPR timeout) — never swallow this error silently.
            if let Err(e) = control_for_response_writer.send_keys(pane_id, bytes) {
                tracing::warn!(
                    pane = ?pane_id,
                    len = bytes.len(),
                    error = %e,
                    "VT query response write-back FAILED — shell may stall on an unanswered DSR/DA/OSC query"
                );
            }
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

    // Shell-exit detection: when the engate Producer's bytes channel
    // closes (= tear pane dropped = child PTY EOF), `attach_live.run()`
    // returns. We flip this AtomicBool so the on_event handler can
    // request a clean window close on the next tick — instead of the
    // window hanging on a dead PTY (real incident: frostmourne `exit`
    // 2026-05-21, window stayed blank until force-quit).
    use std::sync::atomic::{AtomicBool, Ordering};
    let child_exited = Arc::new(AtomicBool::new(false));
    let child_exited_engate = Arc::clone(&child_exited);
    let title_kind_owned: String = title_kind.to_owned();
    let _attach_thread = std::thread::Builder::new()
        .name(format!("mado-engate-live-{title_kind}"))
        .spawn(move || {
            let _consumer = attach_live.run();
            tracing::info!(
                kind = %title_kind_owned,
                pane = ?pane_id,
                "engate channel closed — child PTY EOF, signalling window exit"
            );
            child_exited_engate.store(true, Ordering::Release);
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
            // Both halves move together — a mirror left at snapshot
            // size answers CPR/XTWINOPS for a grid the PTY no longer
            // has (the reedline fatal-CPR class).
            terminal
                .write()
                .resize(init_cols as usize, init_rows as usize);
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

    let app_config = AppConfig {
        // Snowflake-only title — the operator-facing identifier IS the
        // window itself; pane-id + tear-kind don't need to be repeated
        // in the titlebar (they're in the seki prompt + mado MCP).
        // platform::apply_native_styling hides the text anyway via
        // NSWindowTitleVisibility::Hidden, but setting it to ❄ means
        // any path that bypasses styling (initial frame, accessibility,
        // window-menu list, screen-recordings) shows the brand mark
        // instead of a debug string.
        title: "❄".to_string(),
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
    let default_font_size_for_reset = config.font_size;
    // Register this event loop as the live drainer for kanshou-
    // injected actions (`simulate_chord`). Until attach_sink() runs,
    // the kanshou handler answers `no-injection-sink` instead of
    // queueing into the void.
    if let Some(inj) = injected.as_ref() {
        inj.attach_sink();
    }
    let child_exited_for_events = Arc::clone(&child_exited);
    // macOS window-chrome styling latch (window.macos.* +
    // appearance.background) — same shape as the local-PTY path in
    // main.rs. Without this the tear-attach window never ran
    // `apply_native_styling` at all and kept the stock opaque titlebar
    // (grey band + visible ❄ title, operator report 2026-06-11).
    let mut native_styling = crate::platform::NativeStylingLatch::from_config(&config);

    // ── M1 unified input/UX engine ────────────────────────────────
    // Every UX capability (selection + clipboard with the
    // muscle-memory copy contract, search overlay, dir-picker, full
    // mouse forwarding, kitty CSI-u, focus events, IME, font zoom,
    // the PTY-grid⇄display reconciler) lives in ux::InputEngine —
    // identical code to the local-PTY loop; this event loop is a thin
    // adapter (tests/ux_unification.rs pins that structurally). The
    // tear divergences are injected here: PTY writes →
    // control.send_keys, grid pushes → control.pane_resize_absolute,
    // DECCKM → pane_cursor_keys_mode.
    let pty_sink: Box<dyn crate::ux::PtySink> = {
        let control = Arc::clone(&control);
        Box::new(move |bytes: &[u8]| {
            let _ = control.send_keys(pane_id, bytes);
        })
    };
    let resize_sink: Box<dyn crate::ux::ResizeSink> = {
        let control = Arc::clone(&control);
        Box::new(move |cols: u16, rows: u16| {
            let _ = control.pane_resize_absolute(pane_id, cols, rows);
        })
    };
    // DECCKM (cursor-keys application mode) is queried per keystroke
    // via the typed `pane_cursor_keys_mode` accessor on
    // `MultiplexerControl` — no-alloc on the `InProcess` backend,
    // default fallback to `pane_snapshot` on other backends. When
    // vim / less / etc. enter alt-screen and set DECCKM, this returns
    // true and the engine emits `ESC O A/B/C/D` instead of
    // `ESC [ A/B/C/D` for arrow keys. Errors (`NoSuchPane` during
    // shutdown race) degrade to normal mode — the editor still
    // receives valid cursor keys.
    let cursor_keys_mode: Box<dyn Fn() -> bool + Send + Sync> = {
        let control = Arc::clone(&control);
        Box::new(move || control.pane_cursor_keys_mode(pane_id).unwrap_or(false))
    };
    // Adapter-side clipboard handle for OSC 52 sync (same precedent
    // as main.rs; the M4 typed drain subsumes both). Shared with the
    // engine so terminal-driven copies and operator copies land in
    // one place.
    let side_effect_clipboard: Arc<hasami::Clipboard> = Arc::new(
        hasami::Clipboard::new().expect("failed to initialize clipboard"),
    );
    let mut last_title: Option<String> = None;
    let terminal_for_side_effects = Arc::clone(&terminal);
    let mut engine = crate::ux::InputEngine::attach_to_renderer(
        &mut renderer,
        crate::ux::InputEngineParams {
            terminal: Arc::clone(&terminal),
            pty: pty_sink,
            resize: resize_sink,
            shared: crate::ux::SharedUxState::fresh(),
            clipboard: Arc::clone(&side_effect_clipboard) as Arc<dyn hasami::ClipboardProvider + Send + Sync>,
            // Curated default baseline + operator `keybinds.custom`
            // overrides via keybind::manager_from_config — the same
            // assembly as the local-PTY path and the kanshou
            // `simulate_chord` resolver (pre-M1 this path used the
            // bare defaults and ignored custom binds).
            keybinds: crate::keybind::manager_from_config(&config),
            behavior: crate::ux::UxBehavior::from(&config),
            cursor_keys_mode,
            default_font_size: default_font_size_for_reset,
            padding,
        },
    );

    madori::App::builder(renderer)
        .config(app_config)
        .on_event(move |event, renderer| -> EventResponse {
            // ── macOS chrome (flush titlebar etc.) ───────────────
            // The latch retries until a window exists: the first event
            // ticks can arrive before AppKit registers the window, and
            // a fire-once call would leave the stock titlebar up.
            native_styling.tick();
            // ── PTY-grid ⇄ display reconciler ────────────────────
            // Engine-owned latch on the RENDERED surface signature
            // (dims + measured cell metrics), run on every event tick
            // (pre-M1 timing preserved). Covers (a) the pre-window
            // estimate being wrong — heuristic cell metrics, and a
            // Flush titlebar insets the content view while macOS
            // sends no initial Resized to correct it (the 2026-06-11
            // "TUI overlaps stale CLI rows" report) — (b) window
            // resizes (one frame after the surface renders at the new
            // size), and (c) font-zoom metric changes. Resizes BOTH
            // halves: mado's mirror VT grid (wrap math, CPR/XTWINOPS
            // answers, mouse clamps) and tear's PaneGrid+PTY — the
            // mirror half was missing entirely in tear mode.
            engine.on_redraw_tick(renderer);
            // ── Terminal side effects (bell / title / OSC 52) ────
            // Parity with main.rs's RedrawRequested polling — before
            // this, the DEFAULT mode had a completely silent bell, a
            // never-updating window title, and dead OSC 52 copies
            // (hunt finding 2026-06-11). Loop-side until the M4 typed
            // TerminalSideEffects drain replaces both copies.
            {
                let mut term = terminal_for_side_effects.write();
                let current_title = term.title().map(String::from);
                let bell = term.take_bell();
                let osc52_clip = term.take_clipboard();
                drop(term);
                if let Some(clip_text) = osc52_clip {
                    let _ = side_effect_clipboard.copy_text(&clip_text);
                }
                if bell {
                    renderer.trigger_bell();
                }
                if current_title != last_title {
                    last_title = current_title.clone();
                    if let Some(title) = current_title {
                        return EventResponse {
                            set_title: Some(title),
                            ..Default::default()
                        };
                    }
                }
            }
            // ── Elegant child-exit close ──────────────────────
            // engate signalled the producer channel closed (shell
            // exited / PTY EOF). Request a clean window-loop exit
            // on the next event tick. Reap the owned tear session
            // too so we don't leak the multiplexer entry.
            if child_exited_for_events.load(Ordering::Acquire) {
                if let Ok(mut slot) = session_reap.lock() {
                    if let Some(sid) = slot.take() {
                        let _ = control_for_reap.kill_session(sid);
                    }
                }
                return EventResponse {
                    consumed: true,
                    exit: true,
                    ..Default::default()
                };
            }

            // ── kanshou-injected actions (`simulate_chord`) ──────
            // Drain BEFORE the event match so injected actions
            // dispatch on the very next loop tick (madori runs
            // ControlFlow::Poll and emits RedrawRequested every
            // frame — worst-case latency is one frame). Injection
            // bypasses the key-repeat gate on purpose: these are
            // deliberate typed requests, not OS auto-repeat storms,
            // and BoundedFontSize still clamps the result. The drain
            // goes through engine.apply_action — EXACTLY the dispatch
            // a physical chord hits, no parallel implementation to
            // drift.
            if let Some(inj) = injected.as_ref() {
                for action in inj.drain() {
                    if let crate::ux::ActionOutcome::FallThrough =
                        engine.apply_action(action, renderer)
                    {
                        tracing::debug!(
                            action = action.as_str(),
                            "injected action fell through (no consuming handler)"
                        );
                    }
                }
            }

            // M1 adapter: each arm translates AppEvent fields into one
            // InputEngine call and maps the typed EventOutcome back to
            // madori's EventResponse. Loop-specific concerns that stay
            // here: child-exit close + session reap (above), the
            // injection drain, NativeStylingLatch ticks, snow pulses.
            match event {
                AppEvent::Resized { .. } => {
                    // No push here: the engine reconciler (top of
                    // closure) converges on RENDERED truth one frame
                    // later. Pushing event dims raced the renderer's
                    // one-frame lag and ping-ponged tear between old
                    // and new grids (review finding 2026-06-11).
                    EventResponse::ignored()
                }
                AppEvent::Mouse(madori::MouseEvent::Button {
                    button,
                    pressed,
                    x,
                    y,
                    modifiers,
                }) => engine
                    .on_mouse_button(*button, *pressed, *x, *y, *modifiers, renderer)
                    .into(),
                AppEvent::Mouse(madori::MouseEvent::Moved { x, y }) => {
                    renderer.snow_set_cursor(*x as f32, *y as f32);
                    engine.on_mouse_moved(*x, *y, renderer).into()
                }
                AppEvent::Mouse(madori::MouseEvent::Scroll { dy, .. }) => {
                    engine.on_mouse_scroll(*dy, renderer).into()
                }
                AppEvent::Key(key_event @ KeyEvent { pressed: true, .. }) => {
                    renderer.snow_pulse_typing();
                    engine.on_key(key_event, renderer).into()
                }
                // IME commit — forward composed text (CJK, dead-key
                // accents, emoji picker) to the PTY.
                AppEvent::Ime(madori::ImeEvent::Commit(text)) => {
                    engine.on_ime_commit(text).into()
                }
                // Focus events (mode 1004) — engine emits ESC[I /
                // ESC[O when the app enabled focus reporting.
                AppEvent::Focused(focused) => {
                    // Hollow-cursor affordance — renderer-side state,
                    // adapter-appropriate (the engine owns PTY-visible
                    // focus reporting; the renderer owns the pixels).
                    renderer.set_focused(*focused);
                    engine.on_focus(*focused).into()
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
fn try_run_default_embedded(
    config: MadoConfig,
    shell: String,
    kanshou_state: std::sync::Arc<crate::kanshou_state::MadoAppState>,
) -> TearDefaultOutcome {
    use std::sync::Arc;
    use tear_core::InProcess;
    use tear_types::SessionSource;

    let inproc = Arc::new(InProcess::new());
    // Publish the live InProcess to the kanshou aggregator so the
    // `sessions` leaf reflects the GUI's actual session graph,
    // not the empty MCP-side registry that's only populated by
    // `spawn_term` in the --mcp path.
    kanshou_state.set_tear_inproc(inproc.clone());
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

    match run_against_embedded_pane(inproc, pane_id, config, Some(kanshou_state.injected.clone())) {
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
    injected: Option<crate::action_injection::InjectedActions>,
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
        injected,
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
    injected: Option<crate::action_injection::InjectedActions>,
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
        injected,
    )
}

