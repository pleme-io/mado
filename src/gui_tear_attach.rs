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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use tear_types::{MultiplexerControl, PaneId, SessionSource};

use crate::config::{MadoConfig, MadoTearConfig, TearMode, TearRuntime};
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::session_switch::SwitchRequests;
use crate::tear_discovery::{discover, DiscoveryOutcome};
use crate::terminal::{Color as TermColor, Terminal};

/// The pane the per-pane closures (input/resize/response/cursor-keys)
/// currently target.
///
/// `Fixed` is the legacy one-shot binding: the pane is a `Copy`
/// `PaneId`, so `get()` is a register load — byte-identical to
/// capturing `pane_id: PaneId` directly (the default `session_switching
/// = false` path). `Shared` is the switchable binding: the pane lives
/// behind an `Arc<RwLock<PaneId>>` so a runtime switch re-points every
/// closure at once by writing one cell, with no closure rebuild.
///
/// One closure body, two behaviors, zero duplication — the off-path
/// pays nothing the legacy code didn't.
#[derive(Clone)]
enum CurrentPane {
    Fixed(PaneId),
    Shared(Arc<RwLock<PaneId>>),
}

impl CurrentPane {
    /// The pane every per-pane closure should target right now.
    #[inline]
    fn get(&self) -> PaneId {
        match self {
            CurrentPane::Fixed(id) => *id,
            CurrentPane::Shared(cell) => *cell.read(),
        }
    }

    /// Re-point the shared cell at a new pane (switchable path only).
    /// A no-op on `Fixed` — that variant is never switched.
    fn set(&self, id: PaneId) {
        if let CurrentPane::Shared(cell) = self {
            *cell.write() = id;
        }
    }
}

/// The runtime machinery a switchable attach needs, assembled once
/// when `tear.session_switching = true`. `None` everywhere else, so
/// the legacy path constructs + branches on nothing new.
///
/// Generic over the engate Producer `P` so a switch can rebuild the
/// producer→terminal pump against a fresh pane: `build_producer` is
/// the same constructor the initial attach used (`PaneProducer::new`
/// closed over the control plane), parameterized by pane id.
struct SwitchDriver<P> {
    /// The shared switch-request channel the kanshou `switch_session`
    /// leaf posts to. Polled once per event-loop tick.
    requests: SwitchRequests,
    /// The shared current-pane cell the closures read. A switch writes
    /// the new pane here so input/resize/response/cursor-keys re-target
    /// without rebuilding the closures.
    current: Arc<RwLock<PaneId>>,
    /// Rebuilds the engate Producer for a given pane (the same
    /// `PaneProducer::new(control, pane)` shape the first attach used).
    build_producer: Box<dyn Fn(PaneId) -> P + Send>,
}

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
    // kanshou server runs on this path, so no injection queue and
    // no config watcher (config was a one-shot load above).
    run_against_pane(client, pane_id, None, socket_path, config, None, None)
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
    reload: Option<crate::ux::ConfigReloadSource>,
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
        return try_run_default_embedded(config, shell, kanshou_state, reload);
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

    // Session name: explicit override OR the deterministic emoji name
    // derived from the project root of the spawn cwd (praça naming
    // slice). Same cwd the SpawnEnv handshake uses below, so the name
    // matches the directory the shell actually starts in.
    let session_name = config.tear.session_name.clone().unwrap_or_else(|| {
        session_name_for_cwd(config.boot_spawn_cwd().as_deref())
    });

    // Compute the desired pane size up front from the operator's
    // configured window dimensions + font cell metrics, then pass
    // it to new_session_with_source_and_size so the shell spawns
    // at the right grid from t=0. Eliminates the brief 80×24
    // default + post-attach SIGWINCH re-layout.
    let cell_w_logical = config.font_size * 0.6;
    let cell_h_logical = config.font_size * config.line_height;
    let pad_logical = config.window.padding as f32;
    let init_cols = (((config.window.width as f32 - 2.0 * pad_logical) / cell_w_logical)
        .floor() as u16)
        .max(1);
    let init_rows = (((config.window.height as f32 - 2.0 * pad_logical) / cell_h_logical)
        .floor() as u16)
        .max(1);

    // Project the full capability env to the DAEMON before spawning the
    // session — the daemon-runtime mirror of the embedded path's
    // `inproc.set_spawn_env`. Without this a daemon-spawned shell sees no
    // COLORTERM=truecolor and the wrong terminfo (vim renders grey). The
    // override applies only to SUBSEQUENT spawns, so it must precede
    // new_session. Same typed `caps::EnvProjection` → `SpawnEnv` seam, so
    // the child env is identical across the embedded + daemon entry points.
    let spawn_env = tear_types::SpawnEnv::from_overrides(
        crate::caps::EnvProjection::prescribed(env!("CARGO_PKG_VERSION"))
            .pairs()
            .to_vec(),
    )
    .with_cwd(
        config
            .boot_spawn_cwd()
            .map(|d| d.to_string_lossy().into_owned()),
    );
    if let Err(e) = client.set_spawn_env(&spawn_env) {
        tracing::warn!(error = %e, "tear set_spawn_env failed; daemon child may lack truecolor env");
    }

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
    match run_against_pane(
        client,
        pane_id,
        Some(session_id),
        socket_path,
        config,
        Some(injected),
        reload,
    ) {
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
///
/// `switch`: `Some` iff `tear.session_switching = true` — the runtime
/// re-attach machinery (request channel + current-pane cell + producer
/// factory). `None` is the byte-identical legacy one-shot path.
#[allow(clippy::too_many_arguments)]
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
    reload: Option<crate::ux::ConfigReloadSource>,
    // Runtime re-attach machinery. `None` (default,
    // `tear.session_switching = false`) = the legacy one-shot path:
    // the engate pump runs on its own thread, the closures target one
    // fixed pane, no switch poll. `Some` = the switchable path: the
    // pump is polled in the event loop so it can be torn down + rebuilt
    // against a new pane on demand.
    switch: Option<SwitchDriver<P>>,
    // Auto-attach-on-cd driver. `None` (default, `tear.auto_attach =
    // Off` OR `session_switching = false`) = no auto-attach; the
    // displayed pane's cwd is never observed and nothing moves. `Some`
    // = the praça automation: each tick reads the displayed terminal's
    // OSC-7 cwd and, on a cross-project change, posts a switch (to an
    // existing or freshly-spawned session) into the SAME switch channel
    // the `switch` driver above drains. Only meaningful when `switch`
    // is also `Some` (auto-attach requires session_switching).
    mut auto_attach: Option<crate::auto_attach::AutoAttachDriver>,
    // Ctrl-S session picker bridge — praça browse + switch. `Some`
    // (embedded + session_switching) drives the same switch channel as
    // auto-attach; `None` keeps Ctrl-S an inert "switching disabled"
    // hint (mirrors the `switch_session` MCP tool).
    session_picker_bridge: Option<Box<dyn crate::session_picker::SessionPickerBridge>>,
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

    // The pane the per-pane closures target. `Fixed` on the legacy
    // path (one binding for the window's lifetime, a Copy read);
    // `Shared` on the switchable path (a switch re-points all closures
    // by writing one cell). `switch.is_some()` ⇔
    // `tear.session_switching = true`.
    let current_pane: CurrentPane = match &switch {
        Some(sw) => CurrentPane::Shared(Arc::clone(&sw.current)),
        None => CurrentPane::Fixed(pane_id),
    };

    let terminal: SharedTerminal = Arc::new(RwLock::new({
        let mut term = Terminal::with_scrollback(cols, rows, 10_000);
        // M2 — behavior.reflow_on_resize: rewrap-on-resize knob,
        // same wiring the local-PTY path gets via single_pane::spawn.
        term.set_reflow_on_resize(config.behavior.reflow_on_resize);
        term
    }));

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
        config.line_height,
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
    // Effects + accessibility through the SAME application point
    // main.rs uses (M3 review 2026-06-12) — this path previously
    // called only set_effects_config, so effects.colorblind.mode,
    // the accessibility.colorblind alias, reduce_motion's animated-
    // effect gating, and bold_is_bright were all dead in tear-attach
    // windows while working in local-PTY ones.
    renderer.apply_effects_and_accessibility(&config);
    // Budget the ambience governor against the resolved effective frame
    // rate — identical to the local-PTY path (main.rs), so the embedded
    // and local render modes scale aurora quality against the SAME real
    // frame budget instead of the hardcoded 60 Hz floor. No madori
    // posture is available here yet (winit owns monitor enumeration), so
    // it resolves against `None` like main.rs.
    let effective_fps = config.performance.resolve_target_fps(None);
    renderer.set_ambience_budget_fps(effective_fps);
    // Theme parity (FIX 2, operator report 2026-06-12): the SAME
    // shared theme-application point main.rs uses. This path previously
    // applied NO theme — it never called `Terminal::apply_theme`, so
    // the mirror ANSI palette + the OSC 11 background-query answer
    // stayed at the default and vim/the operator's configured theme
    // never reached an embedded-tear window. Routing through
    // `crate::theme::apply_config_theme` makes the palette identical to
    // the local-PTY path (pinned by the entry-point parity test).
    crate::theme::apply_config_theme(
        &mut renderer,
        &terminal,
        &config.theme,
        config.appearance.opacity,
    );
    // Watched-config delta driver (M4 stage 2) — same shared
    // ux::ConfigHotReload the local-PTY loop polls; None on the CLI
    // `mado tear-attach` path, which loads config one-shot and runs
    // no watcher.
    let mut hot_reload = reload
        .map(|src| crate::ux::ConfigHotReload::new(src, config.clone()));

    // engate typed Attach lifecycle — same shape both backends.
    // The TerminalSink writeback path closes the DSR/DA/OSC query
    // loop: when the shell (frost, frostmourne, bash, zsh) sends
    // `\x1b[6n` (cursor position query) etc., mado's VT engine
    // generates the response, and this writer forwards it back to
    // the tear pane's PTY. Without this, reedline-based shells
    // (frost) time out with "cursor position could not be read".
    let control_for_response_writer = Arc::clone(&control);
    let current_pane_for_response = current_pane.clone();
    let response_writer: crate::engate_consumer::ResponseWriter = Arc::new(
        move |bytes: &[u8]| {
            // A dropped VT-query answer kills reedline-based shells (fatal
            // CPR timeout) — never swallow this error silently. After a
            // runtime switch this re-targets the new pane automatically
            // (Fixed → register load on the legacy path).
            let pane_id = current_pane_for_response.get();
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
    // The engate Live-attach builder, factored so the switchable path
    // can rebuild it against a fresh pane. `response_writer` is an
    // `Arc` (clone-cheap) so each rebuilt consumer shares the same
    // re-targeting writeback. The terminal Arc is shared too — a
    // rebuilt attach feeds the SAME renderer-visible terminal (after a
    // `reset()` clears the prior pane's grid).
    let terminal_for_attach = Arc::clone(&terminal);
    let build_attach_live = move |producer: P,
                                  response_writer: crate::engate_consumer::ResponseWriter|
          -> Result<engate_attach::Attach<engate_types::Live, P, crate::engate_consumer::TerminalSink>> {
        let consumer = crate::engate_consumer::TerminalSink::new(
            Arc::clone(&terminal_for_attach),
            response_writer,
        );
        let attach_builder = engate_attach::Attach::builder()
            .producer(producer)
            .consumer(consumer)
            .build();
        let (attach_subscribed, history) = attach_builder
            .subscribe()
            .context("engate.subscribe")?;
        let attach_synced = attach_subscribed.replay(history).context("engate.replay")?;
        Ok(attach_synced.start_live())
    };

    let attach_live = build_attach_live(producer, Arc::clone(&response_writer))?;
    crate::perf::log_phase("pane_subscribed");

    // Shell-exit detection: when the engate Producer's bytes channel
    // closes (= tear pane dropped = child PTY EOF), `attach_live.run()`
    // returns. We flip this AtomicBool so the on_event handler can
    // request a clean window close on the next tick — instead of the
    // window hanging on a dead PTY (real incident: frostmourne `exit`
    // 2026-05-21, window stayed blank until force-quit).
    use std::sync::atomic::{AtomicBool, Ordering};
    let child_exited = Arc::new(AtomicBool::new(false));

    // ── Attach drive mode ─────────────────────────────────────────
    //   * legacy (switch.is_none()): the engate pump runs on its own
    //     thread with the blocking `run()` — byte-identical to today.
    //   * switchable (switch.is_some()): the pump lives in a cell the
    //     event loop polls via `poll_one()`, so a switch can drop it +
    //     rebuild it against a new pane WITHOUT killing the old pane's
    //     subscriber (dropping the cell unsubscribes; it does not EOF
    //     the PTY).
    let mut live_cell: Option<
        engate_attach::Attach<engate_types::Live, P, crate::engate_consumer::TerminalSink>,
    > = None;
    if switch.is_none() {
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
    } else {
        // Switchable path: keep the attach in the cell; the event loop
        // pumps it with `poll_one()` every tick.
        live_cell = Some(attach_live);
    }

    // Initial size-sync: push pane_resize_absolute BEFORE the event
    // loop so tear's default 80×24 doesn't briefly hold while the
    // shell runs zshrc + renders its first prompt. Mado is the size
    // authority.
    {
        let logical_w = config.window.width as f32;
        let logical_h = config.window.height as f32;
        let pad = padding;
        let cell_w_logical = effective_font_size * 0.6;
        let cell_h_logical = effective_font_size * config.line_height;
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
        let current = current_pane.clone();
        Box::new(move |bytes: &[u8]| {
            let _ = control.send_keys(current.get(), bytes);
        })
    };
    let resize_sink: Box<dyn crate::ux::ResizeSink> = {
        let control = Arc::clone(&control);
        let current = current_pane.clone();
        Box::new(move |cols: u16, rows: u16| {
            let _ = control.pane_resize_absolute(current.get(), cols, rows);
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
        let current = current_pane.clone();
        Box::new(move || control.pane_cursor_keys_mode(current.get()).unwrap_or(false))
    };
    // Adapter-side clipboard handle for OSC 52 sync (same precedent
    // as main.rs; the M4 drain consumer reads it). Shared with the
    // engine so terminal-driven copies and operator copies land in
    // one place.
    let side_effect_clipboard: Arc<hasami::Clipboard> = Arc::new(
        hasami::Clipboard::new().expect("failed to initialize clipboard"),
    );
    // Boot-time notification dispatcher (M4 drain consumer): macOS
    // osascript backend on Darwin, tsuuchi LogBackend elsewhere.
    let notifier = crate::platform::notification_dispatcher();
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
            session_picker_bridge,
        },
    );

    // ── Switchable-attach loop state ──────────────────────────────
    // Destructure the switch driver into the pieces the event loop
    // owns: the request channel (polled per tick), the current-pane
    // cell (re-pointed on switch), the producer factory (rebuilds the
    // pump for the new pane), the live attach cell (`live_cell`,
    // pumped via poll_one + replaced on switch), and the shared
    // terminal (reset on switch to clear the prior pane's grid).
    // `None` on the legacy path — the closure branches on nothing.
    let switch_requests = switch.as_ref().map(|sw| sw.requests.clone());
    let (current_pane_for_switch, build_producer) = match switch {
        Some(sw) => (Some(CurrentPane::Shared(sw.current)), Some(sw.build_producer)),
        None => (None, None),
    };
    // Register THIS loop as the switch drainer iff switching is on, so
    // the kanshou `switch_session` leaf answers `switching-disabled`
    // anywhere else (mirrors the InjectedActions::attach_sink gate).
    if let Some(req) = switch_requests.as_ref() {
        req.attach_sink();
    }
    let control_for_switch = Arc::clone(&control);
    let response_writer_for_switch = Arc::clone(&response_writer);
    let terminal_for_switch = Arc::clone(&terminal);
    let mut live_cell = live_cell;

    madori::App::builder(renderer)
        .config(app_config)
        .on_event(move |event, renderer| -> EventResponse {
            // ── Auto-attach-on-cd (the headline praça automation) ──
            // BEFORE servicing the switch channel: observe the displayed
            // terminal's OSC-7 cwd. On an actual cross-project change the
            // driver decides + (for AutoSwitch) POSTS a switch into the
            // SAME channel the block below drains — so the resulting
            // teardown/rebuild happens on this very tick. Gated on
            // `auto_attach.is_some()` (⇔ tear.auto_attach != Off AND
            // session_switching = true); the default path constructs no
            // driver and branches on nothing. Reading the cwd is a cheap
            // RwLock read + (when unchanged) one string compare — no
            // praca/registry/fs work on the common no-change tick.
            if let Some(driver) = auto_attach.as_mut() {
                let displayed_cwd =
                    terminal_for_switch.read().cwd().map(str::to_owned);
                let now = crate::auto_attach::now_unix_seconds();
                if let Some(outcome) =
                    driver.on_displayed_cwd(displayed_cwd.as_deref(), now)
                {
                    match outcome {
                        crate::auto_attach::AutoAttachOutcome::Stay => {}
                        crate::auto_attach::AutoAttachOutcome::Switched {
                            session,
                            pane,
                        } => tracing::info!(
                            ?session,
                            ?pane,
                            "auto-attach: cd switched displayed pane to a bound session"
                        ),
                        crate::auto_attach::AutoAttachOutcome::Spawned {
                            session,
                            pane,
                            ref root,
                            ref name,
                        } => tracing::info!(
                            ?session,
                            ?pane,
                            root = %root.display(),
                            name = %name,
                            "auto-attach: cd spawned + switched to a fresh session"
                        ),
                        crate::auto_attach::AutoAttachOutcome::Suggested {
                            ref hint,
                        } => tracing::info!(
                            hint = %hint,
                            "auto-attach: cd suggests an attach (Suggest mode — pane unchanged)"
                        ),
                        crate::auto_attach::AutoAttachOutcome::Skipped {
                            ref reason,
                        } => tracing::warn!(
                            ?reason,
                            "auto-attach: cd decision skipped"
                        ),
                    }
                }
            }
            // ── Runtime session switch (tear.session_switching) ──
            // Poll the switch channel + pump the switchable attach.
            // The whole block is gated on `switch_requests.is_some()`
            // — the legacy one-shot loop never enters it (the engate
            // pump runs on its own thread there). Order matters:
            // service a pending switch FIRST (so the rest of this tick
            // already reads the new pane), then drain the new pane's
            // bytes via poll_one.
            if let Some(reqs) = switch_requests.as_ref() {
                if let Some(target) = reqs.take() {
                    let from = current_pane_for_switch
                        .as_ref()
                        .map(CurrentPane::get)
                        .unwrap_or(target);
                    if target != from {
                        // 1. Drop the old attach — unsubscribes the old
                        //    pane's byte channel WITHOUT EOF-ing its PTY
                        //    (the pane stays alive for a later switch
                        //    back). Done by replacing the cell below.
                        // 2. Clear the renderer-visible terminal so the
                        //    old pane's grid doesn't bleed under the new
                        //    pane's replay (same reset() the RIS path +
                        //    config hot-reload use).
                        terminal_for_switch.write().reset();
                        // 3. Re-point every per-pane closure at the new
                        //    pane (one cell write).
                        if let Some(cp) = current_pane_for_switch.as_ref() {
                            cp.set(target);
                        }
                        // 4. Build a fresh engate pump for the new pane
                        //    + replay its current grid into the cleared
                        //    terminal, then keep pumping it.
                        match build_producer.as_ref() {
                            Some(factory) => {
                                let producer = factory(target);
                                match build_attach_live(
                                    producer,
                                    Arc::clone(&response_writer_for_switch),
                                ) {
                                    Ok(new_live) => {
                                        live_cell = Some(new_live);
                                        // Size-sync the new pane to the
                                        // window's current grid so it
                                        // doesn't briefly hold tear's
                                        // 80×24 default (the reconciler
                                        // below also converges, but this
                                        // makes the first frame correct).
                                        let (c, r) = {
                                            let t = terminal_for_switch.read();
                                            (t.cols() as u16, t.rows() as u16)
                                        };
                                        let _ = control_for_switch
                                            .pane_resize_absolute(target, c.max(1), r.max(1));
                                        tracing::info!(
                                            from = ?from,
                                            to = ?target,
                                            "session switch: re-attached displayed pane"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            to = ?target,
                                            "session switch: rebuild attach failed; staying on prior pane state"
                                        );
                                        // Leave live_cell as-is (now
                                        // None after the move attempt);
                                        // closures already re-pointed —
                                        // input still reaches the target
                                        // pane, just no live render pump.
                                    }
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "session switch requested but no producer factory; ignoring"
                                );
                            }
                        }
                    }
                }
                // Pump the switchable attach: drain whatever the current
                // pane produced since the last tick. Bounded so a noisy
                // pane can't starve the event loop (matches the
                // embedded_engate_smoke bounded-poll shape).
                if let Some(live) = live_cell.as_mut() {
                    let mut drained = 0u32;
                    while drained < 4096 && live.poll_one() {
                        drained += 1;
                    }
                }
            }
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
            // ── Watched-config delta (M4 stage 2) ────────────────
            // Same per-frame poll the local-PTY adapter runs: dirty
            // flag → typed SetterCall diff → only the changed
            // renderer setters fire.
            if let Some(hr) = hot_reload.as_mut() {
                hr.poll_config_reload(renderer);
            }
            // ── Terminal side effects (M4 drain) ─────────────────
            // ONE typed drain + ONE shared consumer — parity with
            // main.rs by construction now (tests/ux_unification.rs
            // drain markers ban per-loop polling; the 2026-06-11
            // silent-bell/dead-title/dead-OSC52 hunt class cannot
            // re-diverge between the loops).
            //
            // COPY-ON-RELEASE LAW (operator report 2026-06-12): the
            // drain MUST NOT early-return — doing so DROPPED whatever
            // input event rode this same tick. When frost's
            // shell-integration title OSC landed on the mouse-release
            // tick, the early `return EventResponse{ set_title }` ate
            // the LeftRelease, the pointer FSM stayed Selecting, and no
            // copy fired ("I have to click to copy"). The title is now
            // a deferred side-channel: drain it, then ALWAYS run the
            // `match event` below, and fold the title into the event's
            // own response (the event wins on consumed/exit; the title
            // rides along on an otherwise-untouched field).
            let drained_title = {
                let effects = terminal_for_side_effects.write().drain_side_effects();
                crate::ux::apply_side_effects(
                    effects,
                    renderer,
                    &*side_effect_clipboard,
                    &notifier,
                )
            };
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
            //
            // The event's own response is computed FIRST (so no input
            // event is ever dropped to deliver a title), then the
            // drained title is folded in via `with_title` — the event
            // keeps its consumed/exit decision; the title only fills an
            // otherwise-empty `set_title` slot.
            let response = match event {
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
            };
            // Fold the deferred drained title into the event's own
            // response. The event always ran (no drop); the title only
            // fills an empty `set_title` slot — an exit/title set by
            // the event itself wins.
            with_title(response, drained_title)
        })
        .run()
        .map_err(|e| anyhow::anyhow!("madori::App run: {e}"))?;
    Ok(())
}

/// Fold a drained side-effect title into an event's own response
/// WITHOUT ever dropping the event. The event keeps its
/// consumed/exit/cursor decisions; the title only fills an
/// otherwise-empty `set_title` slot (a title the event set itself —
/// the confirm-close prompt, say — wins). This is the structural fix
/// for the copy-on-release regression: the title is a side-channel
/// merged onto the response, never a reason to short-circuit the
/// `match event`.
fn with_title(
    mut response: madori::EventResponse,
    drained_title: Option<String>,
) -> madori::EventResponse {
    if response.set_title.is_none()
        && let Some(title) = drained_title
    {
        response.set_title = Some(title);
    }
    response
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
    reload: Option<crate::ux::ConfigReloadSource>,
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

    // ── Spawn-env projection (FIX 2 + FIX 3) ──────────────────────
    // Stamp mado's typed capability env (TERM=xterm-ghostty + vendored
    // TERMINFO + COLORTERM + TERM_PROGRAM) AND the boot cwd's PWD onto
    // every child PTY tear spawns IN-PROCESS — applied AFTER tear's
    // inherited + xterm-256color-fallback env so mado's richer set
    // wins. Without this, vim in the operator-default (embedded) window
    // got no truecolor (grey) + the wrong terminfo, and a child shell
    // could inherit a stale parent PWD. This is the typed seam
    // (`tear_types::SpawnEnv`) the local-PTY path's env projection also
    // routes through (`caps::EnvProjection`), so the child env is
    // identical across spawn entry points.
    let spawn_env = tear_types::SpawnEnv::from_overrides(
        crate::caps::EnvProjection::prescribed(env!("CARGO_PKG_VERSION"))
            .pairs()
            .to_vec(),
    )
    .with_cwd(
        config
            .boot_spawn_cwd()
            .map(|d| d.to_string_lossy().into_owned()),
    );
    inproc.set_spawn_env(spawn_env.clone());

    // Deterministic emoji name from the spawn cwd's project root
    // (praça naming slice) unless the operator pinned an explicit
    // session_name. Same cwd as the SpawnEnv handshake above.
    let session_name = config.tear.session_name.clone().unwrap_or_else(|| {
        session_name_for_cwd(config.boot_spawn_cwd().as_deref())
    });

    let cell_w_logical = config.font_size * 0.6;
    let cell_h_logical = config.font_size * config.line_height;
    let pad_logical = config.window.padding as f32;
    let init_cols = (((config.window.width as f32 - 2.0 * pad_logical) / cell_w_logical)
        .floor() as u16)
        .max(1);
    let init_rows = (((config.window.height as f32 - 2.0 * pad_logical) / cell_h_logical)
        .floor() as u16)
        .max(1);

    // window.inherit_working_directory threading (M4 stage 2). LAW:
    // the boot cwd now flows through the typed `SpawnEnv.cwd` seam set
    // above — `PtyHandle::spawn` receives it as the child cwd and
    // stamps a matching `PWD` (FIX 3 cwd handshake). `boot_spawn_cwd`
    // returns Some only when the operator pinned
    // environment.working_directory or turned the knob OFF ($HOME);
    // knob ON returns None and the child inherits the launch-shell cwd
    // (and `PtyHandle::spawn` strips any stale inherited PWD). The
    // former `std::env::set_current_dir` process-cwd hack is gone — the
    // cwd is a typed per-spawn arg now, not a mutation of the whole
    // process. (Daemon mode spawns children in the DAEMON's cwd — same
    // documented gap; the SpawnEnv seam lives on InProcess only.)
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

    // Runtime re-attach is embedded-only (the switch target is a pane
    // in the GUI's own InProcess). Wire the shared switch channel iff
    // `tear.session_switching = true`; `None` keeps the byte-identical
    // legacy one-shot path.
    let switch_requests = config
        .tear
        .session_switching
        .then(|| kanshou_state.switch.clone());

    // ── Auto-attach-on-cd (the headline praça automation) ─────────
    // Construct the driver ONLY when `tear.auto_attach != Off` AND
    // `tear.session_switching = true` — auto-attach drives the switch
    // channel, so without switching there is no drainer to post into.
    // When the operator enabled auto_attach but NOT session_switching,
    // log a one-time warning and behave as Off (no driver, byte-
    // identical to today). `None` everywhere else keeps the default
    // path pristine.
    let auto_attach: Option<crate::auto_attach::AutoAttachDriver> = if config
        .tear
        .auto_attach
        .is_active()
    {
        if config.tear.session_switching {
            let now = crate::auto_attach::now_unix_seconds();
            let boot_root = praca::project::project_root(
                config
                    .boot_spawn_cwd()
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new(".")),
            );
            Some(crate::auto_attach::AutoAttachDriver::new(
                Arc::clone(&inproc),
                kanshou_state.switch.clone(),
                config.tear.auto_attach.policy(),
                ishou_tokens::SessionNameStyle::Emoji,
                session_id,
                pane_id,
                boot_root,
                shell.clone(),
                spawn_env,
                now,
            ))
        } else {
            tracing::warn!(
                auto_attach = ?config.tear.auto_attach,
                "tear.auto_attach is set but tear.session_switching = false; \
                 auto-attach drives the switch channel and is INERT without it. \
                 Set tear.session_switching = true to enable auto-attach-on-cd."
            );
            None
        }
    } else {
        None
    };

    // ── Ctrl-S session picker bridge (praça browse + switch) ──────
    // Built whenever session-switching is on (the picker drives the
    // switch channel). It shares the auto-attach driver's LIVE praça
    // index when auto-attach is active, so a spawned/visited session
    // appears in the picker immediately; with auto-attach off
    // (session-switching on), a fresh praça seeded with the boot
    // session gives the picker at least the current seat to browse.
    // `None` keeps the picker inert (Ctrl-S shows the "switching
    // disabled" hint) on the byte-identical legacy path.
    let session_picker_bridge: Option<Box<dyn crate::session_picker::SessionPickerBridge>> =
        switch_requests.as_ref().map(|switch| {
            let shared_praca = match auto_attach.as_ref() {
                Some(driver) => driver.shared_praca(),
                None => {
                    // No auto-attach driver to share with — seed a fresh
                    // index with the boot session so the picker has the
                    // current seat (the picker is read-only here; nothing
                    // mutates this index, so the lone boot row is enough
                    // to demonstrate browse + switch-to-self).
                    let now = crate::auto_attach::now_unix_seconds();
                    let boot_root = praca::project::project_root(
                        config
                            .boot_spawn_cwd()
                            .as_deref()
                            .unwrap_or_else(|| std::path::Path::new(".")),
                    );
                    let mut index = praca::SessionIndex::new();
                    index.upsert(praca::SessionRecord::for_project(
                        session_id,
                        boot_root.clone(),
                        ishou_tokens::SessionNameStyle::Emoji,
                        now,
                    ));
                    let mut binding = praca::ProjectBinding::new();
                    binding.bind(boot_root, session_id);
                    std::sync::Arc::new(std::sync::Mutex::new(praca::Praca::with(
                        index,
                        binding,
                        config.tear.auto_attach.policy(),
                        ishou_tokens::SessionNameStyle::Emoji,
                    )))
                }
            };
            Box::new(crate::session_picker::PracaPickerBridge::new(
                shared_praca,
                Arc::clone(&inproc),
                switch.clone(),
            )) as Box<dyn crate::session_picker::SessionPickerBridge>
        });

    match run_against_embedded_pane(
        inproc,
        pane_id,
        config,
        Some(kanshou_state.injected.clone()),
        reload,
        switch_requests,
        auto_attach,
        session_picker_bridge,
    ) {
        Ok(()) => TearDefaultOutcome::Ran,
        Err(e) => TearDefaultOutcome::Error(e),
    }
}

/// Embedded-mode renderer + event loop — thin wrapper around
/// `run_against_pane_unified`. Constructs the `tear_core` engate
/// Producer + uses `Arc<InProcess>` as the control plane; the
/// unified function handles the rest.
///
/// `switch_requests`: `Some` iff `tear.session_switching = true`. When
/// present, builds the [`SwitchDriver`] so the unified loop can
/// re-attach the displayed pane to a different in-process pane at
/// runtime (same window, fresh terminal). The producer factory closes
/// over the SAME `Arc<InProcess>` so a rebuilt pump addresses any pane
/// the control plane already knows.
#[allow(clippy::too_many_arguments)]
fn run_against_embedded_pane(
    inproc: std::sync::Arc<tear_core::InProcess>,
    pane_id: PaneId,
    config: MadoConfig,
    injected: Option<crate::action_injection::InjectedActions>,
    reload: Option<crate::ux::ConfigReloadSource>,
    switch_requests: Option<SwitchRequests>,
    auto_attach: Option<crate::auto_attach::AutoAttachDriver>,
    session_picker_bridge: Option<Box<dyn crate::session_picker::SessionPickerBridge>>,
) -> Result<()> {
    let snapshot = inproc
        .pane_snapshot(pane_id)
        .with_context(|| format!("inproc.pane_snapshot({pane_id})"))?;
    let producer =
        tear_core::engate_producer::PaneProducer::new(Arc::clone(&inproc), pane_id);
    let switch = switch_requests.map(|requests| {
        let inproc_for_factory = Arc::clone(&inproc);
        SwitchDriver {
            requests,
            current: Arc::new(RwLock::new(pane_id)),
            build_producer: Box::new(move |pane| {
                tear_core::engate_producer::PaneProducer::new(
                    Arc::clone(&inproc_for_factory),
                    pane,
                )
            }),
        }
    });
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
        reload,
        switch,
        auto_attach,
        session_picker_bridge,
    )
}

/// Deterministic, project-derived session name when a spawn cwd is
/// known; otherwise the stable per-process fallback.
///
/// First praça integration slice (naming only — no auto-attach, no
/// picker, no facade). When `cwd` is `Some`, the project root is found
/// via [`praca::project::project_root`] (walks up to `.git` /
/// `Cargo.toml` / `flake.nix` / … markers; falls back to the cwd
/// itself), and the name is the emoji identity of that root via
/// [`ishou_tokens::FleetSessionNames::from_project_path`] in
/// [`ishou_tokens::SessionNameStyle::Emoji`] (e.g. `🌊 tide`). The
/// seed is a stable hash of the project path, so the SAME project
/// always yields the SAME name — across daemon restarts and process
/// re-launches. When `cwd` is `None`, we keep the legacy
/// `mado-<unix-seconds>-<pid>` per-process tag.
fn session_name_for_cwd(cwd: Option<&Path>) -> String {
    match cwd {
        Some(cwd) => {
            let root = praca::project::project_root(cwd);
            ishou_tokens::FleetSessionNames::from_project_path(
                &root,
                ishou_tokens::SessionNameStyle::Emoji,
            )
            .to_string()
        }
        None => default_session_name(),
    }
}

/// Legacy fallback session name: `mado-<unix-seconds>-<pid>`. Stable
/// per-process; sortable by creation time in `tear list`. Used only
/// when no spawn cwd is known (`session_name_for_cwd(None)`).
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
    reload: Option<crate::ux::ConfigReloadSource>,
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
        reload,
        // Daemon-mode runtime switching is a later phase (it needs the
        // multi-attach + persistence story). Embedded only for now.
        None,
        // Auto-attach is embedded-only too (it drives the embedded
        // switch channel + reads the GUI's own InProcess registry).
        None,
        // The session picker is embedded-only as well (its bridge reads
        // the GUI's own InProcess registry + posts to the embedded
        // switch channel). Daemon mode keeps Ctrl-S an inert hint.
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::{default_session_name, session_name_for_cwd, with_title};
    use madori::EventResponse;

    /// praça naming slice: a cwd inside a git project yields the
    /// deterministic emoji name of the PROJECT ROOT (not the cwd), and
    /// that name is stable across repeated calls. Asserts byte-equality
    /// against the direct `FleetSessionNames::from_project_path(root,
    /// Emoji)` so the helper is a thin, correct adapter over the typed
    /// surface — not its own naming scheme.
    #[test]
    fn session_name_for_cwd_is_deterministic_project_emoji() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Mark the tempdir as a project root (git marker).
        std::fs::create_dir(root.join(".git")).expect("mk .git");
        // A nested cwd under the project root — the name must derive
        // from the ROOT, so a subdir produces the same name.
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("mk nested");

        let project_root = praca::project::project_root(&nested);
        assert_eq!(
            project_root, root,
            "project_root must walk up to the .git-marked root"
        );

        let expected = ishou_tokens::FleetSessionNames::from_project_path(
            &project_root,
            ishou_tokens::SessionNameStyle::Emoji,
        )
        .to_string();

        let a = session_name_for_cwd(Some(nested.as_path()));
        let b = session_name_for_cwd(Some(nested.as_path()));
        assert_eq!(a, expected, "helper must emit the ishou emoji name of the project root");
        assert_eq!(a, b, "same project → same name, always (stable seed)");
        assert!(!a.is_empty(), "emoji name is non-empty");
    }

    /// No cwd → the legacy per-process fallback tag, not an emoji name.
    #[test]
    fn session_name_for_cwd_none_is_the_fallback() {
        let name = session_name_for_cwd(None);
        // mado-<unix-seconds>-<pid>: three dash-joined segments, the
        // first literal `mado`, the trailing two numeric.
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 3, "fallback shape is mado-<ts>-<pid>, got {name:?}");
        assert_eq!(parts[0], "mado");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "ts numeric");
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()), "pid numeric");
        // Sanity: the fallback is consistent with default_session_name's shape.
        assert!(default_session_name().starts_with("mado-"));
    }

    /// THE copy-on-release adapter regression, pinned (operator report
    /// 2026-06-12): when a side-effect title is drained ON THE SAME
    /// tick as an input event, the event's own response MUST survive —
    /// the title may never short-circuit the `match event` (the old
    /// `return EventResponse{ set_title }` ate the `LeftRelease`, leaving
    /// the FSM Selecting and no copy). Here the event is a consumed
    /// mouse release (consumed, no title of its own); folding a drained
    /// title MUST keep `consumed` true so the release is delivered to
    /// the FSM, and carry the title along on the side.
    #[test]
    fn drained_title_never_drops_the_events_own_response() {
        // A consumed input event (e.g. the LeftRelease that copies).
        let release_response = EventResponse {
            consumed: true,
            ..Default::default()
        };
        let folded = with_title(release_response, Some("frost ❄ ~/code".into()));
        assert!(
            folded.consumed,
            "the input event's consumed flag must survive a same-tick title drain — \
             dropping it is the copy-on-release bug"
        );
        assert!(!folded.exit, "a title drain must not synthesize an exit");
        assert_eq!(
            folded.set_title.as_deref(),
            Some("frost ❄ ~/code"),
            "the drained title rides along on the otherwise-empty slot"
        );
    }

    /// The title fills an EMPTY response (the common idle-redraw tick),
    /// so the title path is not regressed by the fix.
    #[test]
    fn drained_title_fills_an_empty_response() {
        let folded = with_title(EventResponse::ignored(), Some("title".into()));
        assert_eq!(folded.set_title.as_deref(), Some("title"));
        assert!(!folded.consumed);
    }

    /// An event that set its OWN title (the confirm-close prompt) wins
    /// over a same-tick drained title — the event's intent is never
    /// clobbered by a side-channel title.
    #[test]
    fn events_own_title_wins_over_a_drained_title() {
        let prompt = EventResponse {
            consumed: true,
            set_title: Some("mado — press close again to exit".into()),
            ..Default::default()
        };
        let folded = with_title(prompt, Some("background OSC title".into()));
        assert_eq!(
            folded.set_title.as_deref(),
            Some("mado — press close again to exit"),
            "the event's own title must not be clobbered by a drained title"
        );
    }

    /// No drained title → the response passes through untouched (the
    /// fold is a no-op when nothing was drained).
    #[test]
    fn no_drained_title_passes_response_through() {
        let resp = EventResponse {
            consumed: true,
            exit: true,
            ..Default::default()
        };
        let folded = with_title(resp.clone(), None);
        assert_eq!(folded.consumed, resp.consumed);
        assert_eq!(folded.exit, resp.exit);
        assert_eq!(folded.set_title, None);
    }
}

