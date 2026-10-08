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
use crate::tear_discovery::{DiscoveryOutcome, discover};
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
/// A switch rebuilds the producer→terminal pump against a fresh pane
/// with the same producer constructor the initial attach used.
struct SwitchDriver {
    /// The shared switch-request channel the kanshou `switch_session`
    /// leaf posts to. Serviced on every wake and redraw.
    requests: SwitchRequests,
    /// The shared current-pane cell the closures read. A switch writes
    /// the new pane here so input/resize/response/cursor-keys re-target
    /// without rebuilding the closures.
    current: Arc<RwLock<PaneId>>,
}

type ProducerFor<P> = Box<dyn Fn(PaneId, std::task::Waker) -> P + Send>;

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
    let ui = crate::perf::UiThread::mark();

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
    let (tear, _resolved_socket) = match discover(&tear_cfg) {
        DiscoveryOutcome::Attached(c, p) => (crate::perf::Counted::new(Arc::new(c)), p),
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

    impose_if_any(&tear, &tear_cfg);
    // CLI `mado tear-attach <pane>` is attaching to a session
    // somebody else created — don't kill it on our close. No
    // kanshou server runs on this path, so no injection queue and
    // no config watcher (config was a one-shot load above).
    run_against_pane(
        tear,
        pane_id,
        None,
        socket_path,
        config,
        None,
        None,
        None,
        None,
        &ui,
    )
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
    ui: &crate::perf::UiThread,
) -> TearDefaultOutcome {
    if matches!(config.tear.mode, TearMode::Never) {
        return TearDefaultOutcome::Unavailable;
    }
    // M3c.1 — branch on TearRuntime. Embedded skips the daemon
    // entirely; tear's PTY+grid live in-process inside mado (no Unix
    // socket hop, no inter-process rwlock contention). It still parses
    // every byte twice: tear's `PaneGrid` feeds each chunk before
    // fanning it out, and mado's mirror `Terminal` parses it again
    // (tear PERFORMANCE R38 removes the second parse). Multi-attach
    // scenarios (ayatsuri overlay, namimado debug, remote ssh)
    // need the daemon; embedded is for the default
    // single-window case the operator opens 99% of the time.
    if matches!(config.tear.runtime, TearRuntime::Embedded) {
        return try_run_default_embedded(config, shell, kanshou_state, reload, ui);
    }
    // Daemon path keeps a handle on the kanshou-published injection
    // queue so `simulate_chord` works in both tear runtimes.
    let injected = kanshou_state.injected.clone();
    // Program + argv as ONE value (the operator's `shell.args`, paired
    // with the program they declared it for). Minted once here; the
    // refusal cases log themselves at mint time.
    let spawn = config.shell_spawn(&shell);
    let (tear, socket_path) = match discover(&config.tear) {
        DiscoveryOutcome::Attached(c, p) => (crate::perf::Counted::new(Arc::new(c)), p),
        DiscoveryOutcome::Fallback => return TearDefaultOutcome::Unavailable,
        DiscoveryOutcome::Required(msg) => {
            return TearDefaultOutcome::Error(anyhow::anyhow!("{msg}"));
        }
    };
    crate::perf::log_phase("tear_daemon_discovered");

    // Impose ASAP — before the session exists, so the new pane
    // inherits prefix/shell/scrollback knobs the operator declared.
    impose_if_any(&tear, &config.tear);

    // Session name: the ONE boot naming decision (fleet authority),
    // rendered as the single-width GLYPH form — this is the tear registry
    // name tear-core stamps into TEAR_SESSION_NAME, so the text prompt
    // stays cursor-aligned (no wide emoji in the grid). The daemon path
    // has no praça picker, so only the glyph projection is needed here.
    let session_name = resolve_boot_name(
        config.tear.session_name.as_deref(),
        config.boot_spawn_cwd().as_deref(),
    )
    .render(ishou_tokens::SessionNameStyle::Glyph);

    // Compute the desired pane size up front from the operator's
    // configured window dimensions + font cell metrics, then pass
    // it to new_session_with_source_and_size so the shell spawns
    // at the right grid from t=0. Eliminates the brief 80×24
    // default + post-attach SIGWINCH re-layout.
    let cell_w_logical = config.font_size * 0.6;
    let cell_h_logical = config.font_size * config.line_height;
    let pad_logical = config.window.padding as f32;
    let init_cols =
        (((config.window.width as f32 - 2.0 * pad_logical) / cell_w_logical).floor() as u16).max(1);
    let init_rows = (((config.window.height as f32 - 2.0 * pad_logical) / cell_h_logical).floor()
        as u16)
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
    // Resident sessions outlive this window (TearRuntime::Resident): the
    // window only views them, so it never reaps them on close or SIGTERM.
    let resident = config.tear.runtime.sessions_outlive_windows();

    // The operator's `shell.args`, carried as the child's argv[1..].
    // On this path an OLD daemon refuses non-empty args rather than
    // dropping them (`Capability::SpawnArgs` — tear-client's
    // `require_spawn_args`), so a stale daemon surfaces as the typed
    // error below, never as a login shell that quietly isn't one.
    let created = if resident {
        // The env rides ON the request: a resident daemon serves every
        // window, so a global env would let two windows swap directories.
        // A daemon without `spawn-env` refuses here, typed, before sending.
        tear.new_session_in(
            &session_name,
            spawn.program(),
            spawn.args(),
            SessionSource::Named("mado".into()),
            (init_cols, init_rows),
            &spawn_env,
        )
    } else {
        if let Err(e) = tear.set_spawn_env(&spawn_env) {
            tracing::warn!(error = %e, "tear set_spawn_env failed; daemon child may lack truecolor env");
        }
        tear.new_session_with_source_and_size(
            &session_name,
            spawn.program(),
            spawn.args(),
            SessionSource::Named("mado".into()),
            (init_cols, init_rows),
        )
    };
    let session_id = match created {
        Ok(sid) => sid,
        Err(e) if resident => {
            // No silent fallback to a local PTY: the operator asked for
            // sessions that survive the window, and a local PTY cannot.
            return TearDefaultOutcome::Error(anyhow::anyhow!(
                "tear.runtime = resident, but the tear daemon could not create a session: {e}. \
                 A daemon older than tear 0.1.27 cannot carry a per-session env — restart the \
                 tear daemon on the new build."
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "tear new_session failed; falling back to local PTY");
            return TearDefaultOutcome::Unavailable;
        }
    };

    let session = match tear.get_session(session_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "tear get_session failed; falling back");
            return TearDefaultOutcome::Unavailable;
        }
    };

    let pane_id = match session.windows.values().next().map(|w| w.active_pane) {
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

    // SIGTERM / SIGINT reaper — ONLY for a window that owns its session
    // (`TearRuntime::Daemon`). winit's CloseRequested only fires when the
    // user closes the window — `kill mado` / `timeout mado` /
    // launchd-restart bypasses winit entirely — so a signal handler holding
    // a clone of the client + session_id reaps before the process exits.
    //
    // A RESIDENT window registers nothing: its session is meant to outlive
    // the window, so a signal simply ends mado and the session keeps
    // running in the daemon, one Ctrl-S away from the next window.
    if !resident {
        let reap_tear = tear.clone();
        let sid = session_id;
        ctrlc::set_handler(move || {
            tracing::info!(session = %sid, "signal received — reaping owned tear session");
            let _ = reap_tear.kill_session(sid);
            std::process::exit(130); // 128 + SIGINT
        })
        .ok(); // ok() — second mado in the same process would
        // double-register; the first wins. ctrlc::Error
        // here is non-fatal (reap-on-close still works).
    }

    // Owned (`Daemon`): kill the session when the window closes so it doesn't
    // accumulate as an orphan. Resident: the window is a VIEW — no owned
    // session, nothing reaped — and it gets the same switch channel + Ctrl-S
    // picker the embedded runtime has, over the daemon's session list. The
    // `mado tear-attach <existing-pane>` CLI path passes no session either
    // (it attaches to someone else's session).
    let (owned_session, switch_requests, session_picker_bridge) = if resident {
        let switch = kanshou_state.switch.clone();
        let boot_name = resolve_boot_name(
            config.tear.session_name.as_deref(),
            config.boot_spawn_cwd().as_deref(),
        );
        let picker = config.tear.session_switching.then(|| {
            build_session_picker_bridge(
                &config,
                &kanshou_state,
                tear.as_dyn(),
                switch.clone(),
                None,
                session_id,
                &boot_name,
                spawn.clone(),
                spawn_env.clone(),
            )
        });
        (
            None,
            config.tear.session_switching.then_some(switch),
            picker,
        )
    } else {
        (Some(session_id), None, None)
    };

    match run_against_pane(
        tear,
        pane_id,
        owned_session,
        socket_path,
        config,
        Some(injected),
        reload,
        switch_requests,
        session_picker_bridge,
        ui,
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
/// The session that owns `pane`. mado's session-switcher creates
/// one-pane sessions, so reaping this session reaps the pane (removing
/// it from the registry → gone from the Ctrl-S list). `None` if no
/// session contains the pane (already reaped / unknown id).
fn tear_pty_sink<C>(control: Arc<C>, current: CurrentPane) -> Box<dyn crate::ux::PtySink>
where
    C: tear_types::MultiplexerControl + ?Sized + 'static,
{
    Box::new(move |bytes: &[u8]| {
        let pane = current.get();
        if let Err(e) = control.send_keys(pane, bytes) {
            crate::perf::tear_write_failed(crate::perf::TearWrite::Keys, pane, bytes.len(), &e);
        }
    })
}

fn tear_resize_sink<C>(control: Arc<C>, current: CurrentPane) -> Box<dyn crate::ux::ResizeSink>
where
    C: tear_types::MultiplexerControl + ?Sized + 'static,
{
    Box::new(move |cols: u16, rows: u16| {
        let pane = current.get();
        if let Err(e) = control.pane_resize_absolute(pane, cols, rows) {
            crate::perf::tear_write_failed(crate::perf::TearWrite::Resize, pane, 0, &e);
        }
    })
}

fn session_of_pane(
    sessions: &[tear_types::TearSession],
    pane: PaneId,
) -> Option<tear_types::SessionId> {
    sessions
        .iter()
        .find(|s| s.panes.contains_key(&pane))
        .map(|s| s.id)
}

/// The first still-`Running` pane that isn't `exclude` — the auto-switch
/// target when the displayed pane exits. `None` if every other pane is
/// also dead (the caller then closes the window). Deterministic:
/// `list_sessions` is ordered, and `panes` is a `BTreeMap`.
fn next_live_pane(sessions: &[tear_types::TearSession], exclude: PaneId) -> Option<PaneId> {
    sessions
        .iter()
        .flat_map(|s| s.panes.values())
        .find(|p| p.id != exclude && matches!(p.state, tear_types::PaneState::Running))
        .map(|p| p.id)
}

fn stream_lost_but_pane_runs(control: &dyn tear_types::MultiplexerControl, cur: PaneId) -> bool {
    matches!(
        control.get_pane(cur),
        Ok(p) if matches!(p.state, tear_types::PaneState::Running)
    )
}

/// What an idle tick must do about the pane the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayedPaneFate {
    /// Still running — or the backend could not be read, which is BLIND and
    /// never "gone": a daemon RPC that fails for a moment must not close the
    /// window.
    Keep,
    /// The displayed pane's session has ENDED. Move to `next`, or close the
    /// window when `next` is `None`. `reap` is the session still registered
    /// that must be killed first (an exited pane's — tear keeps it,
    /// remain-on-exit); `None` when it is already gone from the registry.
    Leave {
        reap: Option<tear_types::SessionId>,
        next: Option<PaneId>,
    },
}

/// Decide [`DisplayedPaneFate`] for the displayed pane `cur`.
///
/// A session ends in two shapes, and the window must treat them alike:
///
/// * its shell EXITED — the pane is still registered, state `Exited`;
/// * it was DELETED out from under the window — Ctrl-S Ctrl-D on the session
///   being shown, `tear_kill_session` over MCP, another resident window. The
///   pane is simply absent. Before this arm the absence read as "keep", so
///   the window sat on a dead pane with nothing re-attaching it.
///
/// Absence is a negative, so it is confirmed by a second probe of a different
/// shape before the window acts on it: `get_pane` must answer `NoSuchPane`
/// (a transport error is blind) AND a successful `list_sessions` must hold
/// no session owning `cur`.
fn displayed_pane_fate(
    control: &dyn tear_types::MultiplexerControl,
    cur: PaneId,
) -> DisplayedPaneFate {
    // `true` = the pane EXITED, `false` = it is ABSENT; anything else keeps.
    let exited = match control.get_pane(cur) {
        Ok(p) if matches!(p.state, tear_types::PaneState::Exited { .. }) => true,
        Err(tear_types::ControlError::NoSuchPane(_)) => false,
        // Running, or blind.
        Ok(_) | Err(_) => return DisplayedPaneFate::Keep,
    };
    let Ok(sessions) = control.list_sessions() else {
        return DisplayedPaneFate::Keep;
    };
    let owner = session_of_pane(&sessions, cur);
    let reap = if exited {
        owner
    } else if owner.is_some() {
        // The two probes disagree — `get_pane` said absent, the session list
        // still holds the pane. Absence is not confirmed, so keep, and above
        // all never reap a session the list says is there.
        return DisplayedPaneFate::Keep;
    } else {
        None
    };
    DisplayedPaneFate::Leave {
        reap,
        next: next_live_pane(&sessions, cur),
    }
}

fn pump_displayed<P>(
    stream: &mut crate::pane_stream::PaneStream<P>,
    fate: &mut crate::pane_stream::FateWatch,
    control: &dyn tear_types::MultiplexerControl,
    cur: PaneId,
    now: std::time::Instant,
) -> DisplayedPaneFate
where
    P: engate_attach::Producer<Item = Vec<u8>, Snap = tear_types::engate_wrap::PaneSnapshotWrap>,
{
    let drained = stream.drain(crate::pane_stream::DRAIN_BUDGET);
    if fate.due(drained, now) {
        displayed_pane_fate(control, cur)
    } else {
        DisplayedPaneFate::Keep
    }
}

fn run_against_pane_unified<P, C>(
    producer_for: ProducerFor<P>,
    control: Arc<crate::perf::Counted<C>>,
    pane_id: PaneId,
    snapshot_cols: usize,
    snapshot_rows: usize,
    config: MadoConfig,
    owned_session_id: Option<tear_types::SessionId>,
    title_kind: &str,
    injected: Option<crate::action_injection::InjectedActions>,
    reload: Option<crate::ux::ConfigReloadSource>,
    // Runtime re-attach machinery. `None` (`tear.session_switching =
    // false`, or a window that owns its daemon session) = the legacy
    // one-shot path: the engate pump runs on its own thread, the
    // closures target one fixed pane, no switch poll. `Some` = the
    // switchable path: the pump is drained in the event loop on every
    // wake and redraw so it can be torn down + rebuilt against a new
    // pane on demand.
    switch: Option<SwitchDriver>,
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
    _ui: &crate::perf::UiThread,
) -> Result<()>
where
    P: engate_attach::Producer<Item = Vec<u8>, Snap = tear_types::engate_wrap::PaneSnapshotWrap>
        + 'static,
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
        // ★ THE CONFIGURED DEPTH, NOT A LITERAL. This was `10_000` while the
        // very next statement read `config.behavior.reflow_on_resize` off the
        // same value — the config was in scope and `scrollback_lines` was
        // simply never consulted. Its default is `usize::MAX`, documented as
        // "never lose anything. Host RAM is the only ceiling", and it IS
        // honoured on the local-PTY path (`single_pane::spawn`). This is the
        // DEFAULT path, so on plo the operator's scrollback silently stopped
        // at 10 000 lines while `config-show` echoed back what they had set.
        //
        // ★ AND HONOURING IT DOES NOT RE-OPEN THE 90 GB INCIDENT. The default
        // is `usize::MAX`, and on 2026-09-06 a mado reached ~90 GB on exactly
        // that contract — but the fix for it was a BYTE budget, not a line
        // cap: `Grid::new` sets `max_scrollback_bytes` to
        // `DEFAULT_SCROLLBACK_MAX_BYTES` (1 GiB) by construction, so whichever
        // of the two binds first wins and an unlimited LINE count is bounded
        // in bytes. The literal here was accidental protection against a
        // hazard that is now handled where it belongs; keeping it would mean
        // this path silently disagreeing with the local-PTY path forever.
        debug_assert!(
            crate::terminal::DEFAULT_SCROLLBACK_MAX_BYTES > 0,
            "the byte budget is what bounds an unlimited line cap"
        );
        let mut term = Terminal::with_scrollback(cols, rows, config.behavior.scrollback_lines);
        // M2 — behavior.reflow_on_resize: rewrap-on-resize knob,
        // same wiring the local-PTY path gets via single_pane::spawn.
        term.set_reflow_on_resize(config.behavior.reflow_on_resize);
        term
    }));

    // ★ THE ACCESSIBILITY SCALE, which THIS path dropped. `main.rs` computes
    // `config.font_size * config.accessibility.font_scale` for the local-PTY
    // path; this one — which `main.rs` itself calls "the default render mode"
    // — took the bare size, so `accessibility: { font_scale: 1.5 }` rendered
    // at 1.0x for every normal launch and at 1.5x only on the fallback. The
    // shared `apply_effects_and_accessibility` called just below touches
    // bold_is_bright, reduce_motion, links, feedback, motion, seam and font
    // features, and never the font size.
    let effective_font_size = config.font_size * config.accessibility.font_scale;
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
    // and local render modes scale aurora quality against the SAME frame
    // budget. No madori posture is available here yet (winit owns monitor
    // enumeration), so it resolves against `None` like main.rs: the
    // operator's `performance.target_fps`, else `FALLBACK_FPS` (60). With
    // `target_fps` null this path therefore runs `Capped(60)` whatever
    // the panel's refresh rate.
    let effective_fps = config.performance.resolve_target_fps(None);
    renderer.set_ambience_budget_fps(effective_fps);
    renderer.set_histograms(config.performance.histograms);
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
    let mut hot_reload = reload.map(|src| crate::ux::ConfigHotReload::new(src, config.clone()));

    // engate typed Attach lifecycle — same shape both backends.
    // The TerminalSink writeback path closes the DSR/DA/OSC query
    // loop: when the shell (frost, frostmourne, bash, zsh) sends
    // `\x1b[6n` (cursor position query) etc., mado's VT engine
    // generates the response, and this writer forwards it back to
    // the tear pane's PTY. Without this, every cursor report a
    // reedline-based shell (frost) asks for times out after
    // crossterm's 2 s.
    let control_for_response_writer = Arc::clone(&control);
    let current_pane_for_response = current_pane.clone();
    let response_writer: crate::engate_consumer::ResponseWriter =
        Arc::new(move |bytes: &[u8]| {
            // A dropped VT-query answer stalls the asking shell — never
            // swallow this error silently. frost builds against pleme-io's
            // reedline fork, whose painter falls back after a failed cursor
            // report, so there a dropped answer costs a 2 s stall per cursor
            // report (crossterm's timeout), not the shell; upstream reedline
            // treats the same timeout as fatal. After a runtime switch this
            // re-targets the new pane automatically (Fixed → register load
            // on the legacy path).
            let pane_id = current_pane_for_response.get();
            if let Err(e) = control_for_response_writer.send_keys(pane_id, bytes) {
                crate::perf::tear_write_failed(
                    crate::perf::TearWrite::QueryAnswer,
                    pane_id,
                    bytes.len(),
                    &e,
                );
            }
        });
    let mut window = madori::App::builder(renderer);
    let bell = crate::perf::window_bell(window.waker());
    // The engate Live-attach builder, factored so the switchable path
    // can rebuild it against a fresh pane. `response_writer` is an
    // `Arc` (clone-cheap) so each rebuilt consumer shares the same
    // re-targeting writeback. The terminal Arc is shared too — a
    // rebuilt attach feeds the SAME renderer-visible terminal (after a
    // `reset()` clears the prior pane's grid).
    let terminal_for_attach = Arc::clone(&terminal);
    let bell_for_attach = bell.clone();
    let attach_stream = move |producer_for: &ProducerFor<P>,
                              pane: PaneId,
                              response_writer: crate::engate_consumer::ResponseWriter|
          -> Result<crate::pane_stream::PaneStream<P>> {
        crate::pane_stream::PaneStream::attach(
            |waker| producer_for(pane, waker),
            &crate::perf::UI_TEAR_CALLS,
            Arc::clone(&terminal_for_attach),
            response_writer,
            &bell_for_attach,
        )
        .context("engate attach")
    };

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
    //     thread with the blocking `run()`, and rings the window after
    //     each chunk it feeds and when the stream ends.
    //   * switchable (switch.is_some()): the pump lives in a cell the
    //     event loop drains on every wake and redraw, so a switch can
    //     drop it + rebuild it against a new pane WITHOUT killing the
    //     old pane's subscriber (dropping the cell unsubscribes; it does
    //     not EOF the PTY).
    let mut live_cell: Option<crate::pane_stream::PaneStream<P>> = None;
    if switch.is_none() {
        crate::pane_stream::spawn_feeder(
            producer_for(pane_id, std::task::Waker::noop().clone()),
            &crate::perf::UI_TEAR_CALLS,
            Arc::clone(&terminal),
            Arc::clone(&response_writer),
            bell.clone(),
            Arc::clone(&child_exited),
            format!("mado-engate-live-{title_kind}"),
        )?;
    } else {
        live_cell = Some(attach_stream(
            &producer_for,
            pane_id,
            Arc::clone(&response_writer),
        )?);
    }
    crate::perf::log_phase("pane_subscribed");
    let mut fate = crate::pane_stream::FateWatch::new(
        config.tear.pane_fate,
        config.tear.fate_backstop_secs,
        std::time::Instant::now(),
    );

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
        let init_cols: u16 = (((logical_w - 2.0 * pad) / cell_w_logical).floor() as u16).max(1);
        let init_rows: u16 = (((logical_h - 2.0 * pad) / cell_h_logical).floor() as u16).max(1);
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
        // Same reasoning as the local-PTY path in main.rs: mado owns its
        // menubar so winit's Services submenu never ships.
        menu_policy: madori::MenuPolicy::AppOwned,
    };
    crate::platform::install_app_menu();
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
    let pty_sink = tear_pty_sink(Arc::clone(&control), current_pane.clone());
    let resize_sink = tear_resize_sink(Arc::clone(&control), current_pane.clone());
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
        Box::new(move || {
            control
                .pane_cursor_keys_mode(current.get())
                .unwrap_or(false)
        })
    };
    // Adapter-side clipboard handle for OSC 52 sync (same precedent
    // as main.rs; the M4 drain consumer reads it). Shared with the
    // engine so terminal-driven copies and operator copies land in
    // one place.
    let side_effect_clipboard: Arc<dyn hasami::ClipboardProvider> =
        crate::clipboard_store::open_or_memory();
    // Boot-time notification center (M4 drain consumer): focus-aware,
    // coalescing, rate-limiting orchestrator over the chosen backend. See
    // docs/NOTIFICATIONS.md.
    let mut notify_center = crate::notify_center::NotificationCenter::new(
        crate::platform::notification_dispatcher(config.notifications.backend),
        &config.notifications,
    );
    let terminal_for_side_effects = Arc::clone(&terminal);
    let mut engine = crate::ux::InputEngine::attach_to_renderer(
        window.renderer_mut(),
        crate::ux::InputEngineParams {
            terminal: Arc::clone(&terminal),
            pty: pty_sink,
            resize: resize_sink,
            shared: crate::ux::SharedUxState::fresh(),
            clipboard: Arc::clone(&side_effect_clipboard),
            // Curated default baseline + operator `keybinds.custom`
            // overrides via keybind::manager_from_config — the same
            // assembly as the local-PTY path and the kanshou
            // `simulate_chord` resolver (pre-M1 this path used the
            // bare defaults and ignored custom binds).
            keybinds: crate::keybind::manager_from_config(&config),
            behavior: crate::ux::UxBehavior::from(&config),
            links: config.links.clone(),
            cursor_keys_mode,
            default_font_size: default_font_size_for_reset,
            padding,
            session_picker_bridge,
            suggest_attention: config.suggestions.attention_on_critical,
        },
    );

    // ── Switchable-attach loop state ──────────────────────────────
    // Destructure the switch driver into the pieces the event loop
    // owns: the request channel (serviced on every wake and redraw),
    // the current-pane cell (re-pointed on switch), the live attach
    // cell (`live_cell`, drained on every wake and redraw + replaced on
    // switch), and the shared terminal (reset on switch to clear the
    // prior pane's grid). `None` on the legacy path — the closure
    // branches on nothing.
    let switch_requests = switch.as_ref().map(|sw| sw.requests.clone());
    let current_pane_for_switch = switch.map(|sw| CurrentPane::Shared(sw.current));
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
    let mut last_reattach = std::time::Instant::now();

    // Frame pacing — and THIS is the site that actually costs the operator:
    // the embedded-tear window is the default render mode. Full reasoning and
    // the madori-side API at the twin builder in main.rs; `effective_fps` is
    // resolved above via `config.performance.resolve_target_fps(None)`.
    window
        .target_fps(effective_fps)
        .config(app_config)
        // Wayland `app_id` / X11 `WM_CLASS` — see the twin builder in
        // main.rs. Same string on both paths so the embedded-tear window
        // (default render mode) matches the launcher too.
        .app_id("mado")
        .on_event(crate::perf::ui_dispatch(move |event, renderer| -> EventResponse {
            let woke = matches!(event, AppEvent::RedrawRequested);
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
            if woke && let Some(driver) = auto_attach.as_mut() {
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
            // Service the switch channel + drain the switchable attach.
            // The whole block is gated on `switch_requests.is_some()`
            // — the legacy one-shot loop never enters it (the engate
            // pump runs on its own thread there). Order matters:
            // service a pending switch FIRST (so the rest of this tick
            // already reads the new pane), then drain the new pane's
            // bytes.
            if woke && let Some(reqs) = switch_requests.as_ref() {
                let reattach = if live_cell
                    .as_ref()
                    .is_some_and(crate::pane_stream::PaneStream::ended)
                    && last_reattach.elapsed() >= crate::pane_stream::REATTACH_BACKOFF
                {
                    last_reattach = std::time::Instant::now();
                    current_pane_for_switch
                        .as_ref()
                        .map(CurrentPane::get)
                        .filter(|cur| stream_lost_but_pane_runs(&*control_for_switch, *cur))
                } else {
                    None
                };
                let pending = reqs
                    .take()
                    .map(|t| (t, false))
                    .or(reattach.map(|t| (t, true)));
                if let Some((target, forced)) = pending {
                    let from = current_pane_for_switch
                        .as_ref()
                        .map(CurrentPane::get)
                        .unwrap_or(target);
                    if target != from || forced {
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
                        match attach_stream(
                            &producer_for,
                            target,
                            Arc::clone(&response_writer_for_switch),
                        ) {
                            Ok(stream) => {
                                live_cell = Some(stream);
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
                                if let Err(e) = control_for_switch
                                    .pane_resize_absolute(target, c.max(1), r.max(1))
                                {
                                    crate::perf::tear_write_failed(
                                        crate::perf::TearWrite::Resize,
                                        target,
                                        0,
                                        &e,
                                    );
                                }
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
                                // The cell is emptied; closures already
                                // re-pointed — input still reaches the
                                // target pane, just no live render pump.
                                live_cell = None;
                            }
                        }
                    }
                }
                // Drain the switchable attach: whatever the current pane
                // produced since the last wake. Bounded so a noisy pane
                // can't starve the event loop; a drain that hits the
                // bound rings the window again for the rest.
                if let Some(live) = live_cell.as_mut() {
                    // ── Reap-on-exit + auto-switch ───────────────────
                    // tear marks an exited pane `Exited` but KEEPS it
                    // (tmux remain-on-exit), so a shell that exits would
                    // otherwise STICK on screen and linger in the Ctrl-S
                    // list. The fate is read when the pane's byte stream
                    // ends — tear ends it on exit and on kill — plus a
                    // backstop every `tear.fate_backstop_secs`, so an
                    // idle window makes no registry read;
                    // `tear.pane_fate: poll` reads it on every idle tick
                    // as before (`FateWatch`). When the displayed pane's
                    // session ENDS — its shell exited, or it was deleted out
                    // from under the window (Ctrl-S Ctrl-D, MCP) — reap what
                    // is left of it (→ gone from the picker) and auto-switch
                    // to another live pane, or close the window if none
                    // remain. The decision is `displayed_pane_fate`.
                    if let (Some(cp), Some(reqs)) = (
                        current_pane_for_switch.as_ref(),
                        switch_requests.as_ref(),
                    ) {
                        let cur = cp.get();
                        if let DisplayedPaneFate::Leave { reap, next } = pump_displayed(
                            live,
                            &mut fate,
                            &*control_for_switch,
                            cur,
                            std::time::Instant::now(),
                        ) {
                            if let Some(sid) = reap {
                                let _ = control_for_switch.kill_session(sid);
                                tracing::info!(
                                    pane = ?cur,
                                    session = %sid,
                                    "displayed pane exited — reaped its session"
                                );
                            } else {
                                tracing::info!(
                                    pane = ?cur,
                                    "displayed pane's session was deleted — leaving it"
                                );
                            }
                            match next {
                                Some(target) => {
                                    tracing::info!(to = ?target, "auto-switching away from ended pane");
                                    reqs.post(target);
                                }
                                None => {
                                    return EventResponse {
                                        consumed: true,
                                        exit: true,
                                        ..Default::default()
                                    };
                                }
                            }
                        }
                    }
                }
            }
            // ── macOS chrome (flush titlebar etc.) ───────────────
            // The latch retries until a window exists: the first event
            // ticks can arrive before AppKit registers the window, and
            // a fire-once call would leave the stock titlebar up.
            if woke {
                native_styling.tick();
            }
            // ── PTY-grid ⇄ display reconciler ────────────────────
            // Engine-owned latch on the RENDERED surface signature
            // (dims + measured cell metrics), run on every wake and
            // redraw. Covers (a) the pre-window
            // estimate being wrong — heuristic cell metrics, and a
            // Flush titlebar insets the content view while macOS
            // sends no initial Resized to correct it (the 2026-06-11
            // "TUI overlaps stale CLI rows" report) — (b) window
            // resizes (one frame after the surface renders at the new
            // size), and (c) font-zoom metric changes. Resizes BOTH
            // halves: mado's mirror VT grid (wrap math, CPR/XTWINOPS
            // answers, mouse clamps) and tear's PaneGrid+PTY — the
            // mirror half was missing entirely in tear mode.
            if woke {
                engine.on_redraw_tick(renderer);
            }
            // ── Watched-config delta (M4 stage 2) ────────────────
            // Same per-frame poll the local-PTY adapter runs: dirty
            // flag → typed SetterCall diff → only the changed
            // renderer setters fire.
            if woke && let Some(hr) = hot_reload.as_mut() {
                // A reload touching titlebar/appearance chrome
                // un-latches `native_styling` so the next tick moves
                // the NSWindow backing too, not just the canvas —
                // parity with the local-PTY loop (main.rs).
                if let Some(new_config) = hr.poll_config_reload(renderer) {
                    native_styling.refresh(&new_config);
                }
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
            let drained_title = if woke {
                let effects = terminal_for_side_effects.write().drain_side_effects();
                crate::ux::apply_side_effects(
                    effects,
                    renderer,
                    &*side_effect_clipboard,
                    &mut notify_center,
                )
            } else {
                None
            };
            // ── Elegant child-exit close ──────────────────────
            // engate signalled the producer channel closed (shell
            // exited / PTY EOF) and rang the window. Request a clean
            // window-loop exit on that wake. Reap the owned tear session
            // too so we don't leak the multiplexer entry.
            if woke && child_exited_for_events.load(Ordering::Acquire) {
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
            // dispatch on the very next redraw (the tear path's loop
            // runs `Capped` and redraws at least once per frame
            // interval — worst-case latency is one frame). Injection
            // bypasses the key-repeat gate on purpose: these are
            // deliberate typed requests, not OS auto-repeat storms,
            // and BoundedFontSize still clamps the result. The drain
            // goes through engine.apply_action — EXACTLY the dispatch
            // a physical chord hits, no parallel implementation to
            // drift.
            if woke && let Some(inj) = injected.as_ref() {
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
                AppEvent::Mouse(madori::MouseEvent::Scroll { delta, .. }) => {
                    engine.on_mouse_scroll((*delta).into(), renderer).into()
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
                // Drag-and-drop — a dropped file's shell-quoted path is
                // bracket-pasted into the PTY (ghostty parity: a dragged
                // screenshot becomes a path a TUI / $EDITOR can open).
                AppEvent::DroppedFile(path) => engine.drop_file(path).into(),
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
        }))
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
/// It is not a single parse: tear's `PaneGrid` parses every chunk
/// before handing it on, and mado's mirror `Terminal` parses it
/// again on the main thread, so each byte is parsed twice until tear
/// PERFORMANCE R38 deletes the mirror.
///
/// Trade-off: single-attach only. ayatsuri overlays, namimado-debug,
/// and remote ssh-mux scenarios require the daemon. Operator opts
/// into Daemon via `mado.tear.runtime = "daemon"` for those.
fn try_run_default_embedded(
    config: MadoConfig,
    shell: String,
    kanshou_state: std::sync::Arc<crate::kanshou_state::MadoAppState>,
    reload: Option<crate::ux::ConfigReloadSource>,
    ui: &crate::perf::UiThread,
) -> TearDefaultOutcome {
    use std::sync::Arc;
    use tear_core::InProcess;
    use tear_types::SessionSource;

    // Program + argv as ONE value — same mint as the daemon path, so both
    // entry points spawn the child with an identical argv (and log an
    // identical refusal when the operator's args don't apply).
    let spawn = config.shell_spawn(&shell);

    let inproc = Arc::new(InProcess::new());
    // Publish the live InProcess to the kanshou aggregator so the
    // `sessions` leaf reflects the GUI's actual session graph,
    // not the empty MCP-side registry that's only populated by
    // `spawn_term` in the --mcp path.
    kanshou_state.set_tear_inproc(inproc.clone());
    // …and to the janitor plane, so the ghost-session sweeper observes
    // the SAME live registry (see crate::janitors).
    crate::janitors::set_tear_inproc(inproc.clone());
    let tear = crate::perf::Counted::new(inproc);
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
    tear.set_spawn_env(spawn_env.clone());

    // The ONE boot naming decision (fleet authority), minted ONCE here.
    // `session_name` is the single-width GLYPH projection — the tear
    // registry name → TEAR_SESSION_NAME → the cursor-aligned prompt.
    // `boot_name` is kept so the praça picker seeds below project the
    // EMOJI form of the SAME identity (vivid in the GUI). One identity,
    // two surface renderings — picker and prompt cannot disagree.
    let boot_name = resolve_boot_name(
        config.tear.session_name.as_deref(),
        config.boot_spawn_cwd().as_deref(),
    );
    let session_name = boot_name.render(ishou_tokens::SessionNameStyle::Glyph);

    let cell_w_logical = config.font_size * 0.6;
    let cell_h_logical = config.font_size * config.line_height;
    let pad_logical = config.window.padding as f32;
    let init_cols =
        (((config.window.width as f32 - 2.0 * pad_logical) / cell_w_logical).floor() as u16).max(1);
    let init_rows = (((config.window.height as f32 - 2.0 * pad_logical) / cell_h_logical).floor()
        as u16)
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
    let session_id = match tear.new_session_with_source_and_size(
        &session_name,
        spawn.program(),
        // The operator's `shell.args`, as the child's argv[1..] — the same
        // typed value the daemon path passes, so both runtimes spawn the
        // identical command line.
        spawn.args(),
        SessionSource::Named("mado-embedded".into()),
        (init_cols, init_rows),
    ) {
        Ok(sid) => sid,
        Err(e) => {
            tracing::warn!(error = %e, "InProcess::new_session_with_source_and_size failed in embedded mode");
            return TearDefaultOutcome::Unavailable;
        }
    };
    let pane_id = match tear.with_registry(|r| {
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
    // The session-picker bridge can CREATE sessions (create-on-miss /
    // emoji presets), so it needs the spawn shell + env — the same the
    // auto-attach driver spawns with. Capture a clone before the driver
    // construction below moves the original `spawn_env`.
    let picker_spawn_env = spawn_env.clone();
    let picker_spawn = spawn.clone();

    let auto_attach: Option<crate::auto_attach::AutoAttachDriver> =
        if config.tear.auto_attach.is_active() {
            if config.tear.session_switching {
                let now = crate::auto_attach::now_unix_seconds();
                let boot_root = praca::project::project_root(
                    config
                        .boot_spawn_cwd()
                        .as_deref()
                        .unwrap_or_else(|| std::path::Path::new(".")),
                );
                Some(crate::auto_attach::AutoAttachDriver::new(
                    tear.clone(),
                    kanshou_state.switch.clone(),
                    config.tear.auto_attach.policy(),
                    ishou_tokens::SessionNameStyle::Emoji,
                    session_id,
                    pane_id,
                    boot_root,
                    // The EMOJI projection of the ONE minted identity — the
                    // picker label, matching the glyph form the prompt shows.
                    boot_name.render(ishou_tokens::SessionNameStyle::Emoji),
                    // Program only: `AutoAttachDriver::new` takes a bare
                    // `String` and spawns with `&[]`, so an auto-attach-on-cd
                    // session does NOT get `shell.args`. That is a gap in
                    // `src/auto_attach.rs`, not a decision made here — it
                    // closes by giving the driver a `ShellSpawn` instead of a
                    // `String`, the same swap this file just made.
                    spawn.program().to_owned(),
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
            build_session_picker_bridge(
                &config,
                &kanshou_state,
                tear.as_dyn(),
                switch.clone(),
                auto_attach.as_ref().map(|driver| driver.shared_praca()),
                session_id,
                &boot_name,
                picker_spawn.clone(),
                picker_spawn_env.clone(),
            )
        });

    match run_against_embedded_pane(
        tear,
        pane_id,
        config,
        Some(kanshou_state.injected.clone()),
        reload,
        switch_requests,
        auto_attach,
        session_picker_bridge,
        ui,
    ) {
        Ok(()) => TearDefaultOutcome::Ran,
        Err(e) => TearDefaultOutcome::Error(e),
    }
}

/// Build the Ctrl-S session-picker bridge over ANY tear backend — the ONE
/// construction both the embedded and the resident launch paths use, so the
/// picker a resident window gets is the same picker an embedded window gets.
///
/// `shared_praca` is the auto-attach driver's live index when one exists;
/// otherwise a fresh index is seeded with the boot session so the picker has
/// the current seat, and the reconciler (run on every picker refresh) absorbs
/// every other session the backend holds.
#[allow(clippy::too_many_arguments)]
fn build_session_picker_bridge(
    config: &MadoConfig,
    kanshou_state: &crate::kanshou_state::MadoAppState,
    control: crate::perf::Counted<dyn tear_types::MultiplexerControl>,
    switch: SwitchRequests,
    shared_praca: Option<Arc<std::sync::Mutex<praca::Praca>>>,
    boot_session: tear_types::SessionId,
    boot_name: &ishou_tokens::ResolvedName,
    spawn: crate::config::ShellSpawn,
    spawn_env: tear_types::SpawnEnv,
) -> Box<dyn crate::session_picker::SessionPickerBridge> {
    let shared_praca = shared_praca.unwrap_or_else(|| {
        let now = crate::auto_attach::now_unix_seconds();
        let boot_root = praca::project::project_root(
            config
                .boot_spawn_cwd()
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new(".")),
        );
        let mut index = praca::SessionIndex::new();
        // Label the boot record by the EMOJI projection of the minted
        // identity (the same identity the registry/prompt glyph-projects) —
        // NOT a re-derivation from boot_root — so the picker shows exactly
        // what the prompt names.
        let mut boot_rec = praca::SessionRecord::for_project(
            boot_session,
            boot_root.clone(),
            ishou_tokens::SessionNameStyle::Emoji,
            now,
        );
        boot_rec.rename(boot_name.render(ishou_tokens::SessionNameStyle::Emoji));
        index.upsert(boot_rec);
        let mut binding = praca::ProjectBinding::new();
        binding.bind(boot_root, boot_session);
        Arc::new(std::sync::Mutex::new(praca::Praca::with(
            index,
            binding,
            config.tear.auto_attach.policy(),
            ishou_tokens::SessionNameStyle::Emoji,
        )))
    });
    // Presets survive restarts: restore the persisted definitions catalog into
    // the freshly-built praça (boot index/binding/policy untouched — dead-
    // session state must not steer auto-attach) and register it for the
    // maintenance loop's debounced persist.
    crate::praca_store::load_and_register(&shared_praca);
    // Give the MCP surface the SAME catalog the picker reads, so
    // `save_session_as_preset` writes where Ctrl-S reads its ○ rows.
    kanshou_state.set_praca(Arc::clone(&shared_praca));
    // The Ctrl-S picker shares the global suggestion store the watcher engine
    // fills (see `crate::suggest`), so the continuously-refreshing ○ task rows
    // appear beneath sessions + presets when the stream is enabled.
    let (suggest_store, suggest_max, suggest_cap) = if config.suggestions.enabled {
        (
            Some(crate::suggest::store()),
            config.suggestions.max_visible,
            config.suggestions.per_source_cap,
        )
    } else {
        (None, 0, 0)
    };
    Box::new(crate::session_picker::PracaPickerBridge::new(
        shared_praca,
        control,
        switch,
        spawn,
        spawn_env,
        config.tear.session_picker_surface_presets,
        config.tear.session_picker_badges,
        suggest_store,
        suggest_max,
        suggest_cap,
        config.suggestions.reserved_rows,
    ))
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
    tear: crate::perf::Counted<tear_core::InProcess>,
    pane_id: PaneId,
    config: MadoConfig,
    injected: Option<crate::action_injection::InjectedActions>,
    reload: Option<crate::ux::ConfigReloadSource>,
    switch_requests: Option<SwitchRequests>,
    auto_attach: Option<crate::auto_attach::AutoAttachDriver>,
    session_picker_bridge: Option<Box<dyn crate::session_picker::SessionPickerBridge>>,
    ui: &crate::perf::UiThread,
) -> Result<()> {
    let snapshot = tear
        .pane_snapshot(pane_id)
        .with_context(|| format!("inproc.pane_snapshot({pane_id})"))?;
    let tear_for_factory = tear.clone();
    let producer_for: ProducerFor<tear_core::engate_producer::PaneProducer> =
        Box::new(move |pane, waker| tear_for_factory.producer(pane, waker));
    let switch = switch_requests.map(|requests| SwitchDriver {
        requests,
        current: Arc::new(RwLock::new(pane_id)),
    });
    run_against_pane_unified(
        producer_for,
        Arc::new(tear),
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
        ui,
    )
}

/// THE single boot-session naming decision, via the fleet authority
/// [`ishou_tokens::FleetSessionNames::resolve`]. Both the daemon and the
/// embedded launch paths call this with the same inputs, so they can
/// never name the same context two different ways.
///
/// Returns the resolved identity; the caller renders it PER SURFACE —
/// [`ishou_tokens::SessionNameStyle::Glyph`] for the tear registry name
/// (→ `TEAR_SESSION_NAME` → the cursor-aligned text prompt) and
/// [`ishou_tokens::SessionNameStyle::Emoji`] for the praça picker record
/// (the vivid GUI surface). One identity, two projections: the picker
/// and the prompt can never disagree on the session's word.
///
/// Precedence: an explicit operator override (`tear.session_name`) wins;
/// else the project root of the spawn cwd yields the deterministic,
/// path-stable identity; else (no spawn cwd — a Finder/launchd launch)
/// the branded fleet default `🌑 rime`, replacing the old opaque
/// `mado-<unix-seconds>-<pid>` tag.
fn resolve_boot_name(
    override_name: Option<&str>,
    spawn_cwd: Option<&Path>,
) -> ishou_tokens::ResolvedName {
    use ishou_tokens::{FleetSessionNames, NameContext};
    if let Some(explicit) = override_name {
        return FleetSessionNames::resolve(NameContext::Override(explicit));
    }
    match spawn_cwd {
        Some(cwd) => {
            let root = praca::project::project_root(cwd);
            FleetSessionNames::resolve(NameContext::Project(&root))
        }
        None => FleetSessionNames::resolve(NameContext::Default),
    }
}

/// If the operator declared `tear.impose.*` overrides, fetch the
/// daemon's current TearConfig, merge in the overrides, and push
/// the result back via SetConfig. Errors are logged + non-fatal —
/// failing to impose shouldn't break attach.
fn impose_if_any(tear: &crate::perf::Counted<tear_client::Client>, tear_cfg: &MadoTearConfig) {
    let Some(impose) = tear_cfg.impose.as_ref() else {
        return;
    };
    if !impose.has_any_override() {
        return;
    }
    match tear.get_config() {
        Ok(mut current) => {
            impose.apply_to(&mut current);
            if let Err(e) = tear.set_config(&current) {
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
#[allow(clippy::too_many_arguments)]
fn run_against_pane(
    tear: crate::perf::Counted<tear_client::Client>,
    pane_id: PaneId,
    owned_session_id: Option<tear_types::SessionId>,
    _socket_path: PathBuf,
    config: MadoConfig,
    injected: Option<crate::action_injection::InjectedActions>,
    reload: Option<crate::ux::ConfigReloadSource>,
    switch_requests: Option<SwitchRequests>,
    session_picker_bridge: Option<Box<dyn crate::session_picker::SessionPickerBridge>>,
    ui: &crate::perf::UiThread,
) -> Result<()> {
    let snapshot = tear
        .pane_snapshot(pane_id)
        .with_context(|| format!("pane_snapshot({pane_id})"))?;
    let tear_for_factory = tear.clone();
    let producer_for: ProducerFor<tear_client::engate_producer::PaneProducer> =
        Box::new(move |pane, waker| tear_for_factory.producer(pane, waker));
    // Runtime re-attach over the daemon: the same SwitchDriver the embedded
    // runtime uses, with the pump rebuilt from the CLIENT's producer — so a
    // switch re-subscribes this window to any pane the daemon holds.
    let switch = switch_requests.map(|requests| SwitchDriver {
        requests,
        current: Arc::new(RwLock::new(pane_id)),
    });
    run_against_pane_unified(
        producer_for,
        Arc::new(tear),
        pane_id,
        snapshot.cols,
        snapshot.rows,
        config,
        owned_session_id,
        "tear",
        injected,
        reload,
        // Switching + the Ctrl-S picker: present for a RESIDENT window
        // (sessions outlive it, so moving between them is the point), absent
        // for an owned `Daemon` window and for `mado tear-attach`.
        switch,
        // Auto-attach stays embedded-only: its driver spawns into and reads
        // the GUI's own InProcess registry.
        None,
        session_picker_bridge,
        ui,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CurrentPane, DisplayedPaneFate, displayed_pane_fate, pump_displayed, resolve_boot_name,
        tear_pty_sink, tear_resize_sink, with_title,
    };
    use crate::perf::{TEAR_WRITE_FAILURES, TearWrite};
    use madori::EventResponse;
    use std::sync::Arc;
    use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

    #[test]
    fn the_tear_sinks_count_every_write_their_control_refuses() {
        let control = Arc::new(tear_core::InProcess::new());
        let gone = CurrentPane::Fixed(PaneId(u64::MAX));
        let keys = TEAR_WRITE_FAILURES.get(TearWrite::Keys);
        let resizes = TEAR_WRITE_FAILURES.get(TearWrite::Resize);
        tear_pty_sink(Arc::clone(&control), gone.clone()).write(b"x");
        tear_resize_sink(control, gone).resize(80, 24);
        assert!(TEAR_WRITE_FAILURES.get(TearWrite::Keys) > keys);
        assert!(TEAR_WRITE_FAILURES.get(TearWrite::Resize) > resizes);
    }

    /// A live one-pane session on `inproc`, and its pane.
    fn live_session(inproc: &tear_core::InProcess, name: &str) -> (SessionId, PaneId) {
        let sid = inproc
            .new_session_with_source_and_size(
                name,
                "/bin/sh",
                &[],
                SessionSource::Named("fate-test".into()),
                (80, 24),
            )
            .expect("spawn");
        let pane = inproc
            .get_session(sid)
            .ok()
            .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
            .expect("a live session has a pane");
        (sid, pane)
    }

    /// The window is showing session A's pane and A is DELETED out from under
    /// it (Ctrl-S Ctrl-D on the session being shown). The window must leave
    /// the dead pane exactly as it does when a shell exits: to the next live
    /// pane, or close once nothing is left — never sit on a pane that no
    /// longer exists. Nothing is left to reap: the delete already removed it.
    ///
    /// Red-run: with the absent arm treated as "keep" (the pre-change
    /// behaviour, which only looked for `PaneState::Exited`), both `Leave`
    /// assertions fail with `Keep`.
    #[test]
    fn a_deleted_displayed_session_moves_the_window_on_like_an_ended_one() {
        let inproc = tear_core::InProcess::new();
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let (a, a_pane) = live_session(&inproc, "shown");
        let (b, b_pane) = live_session(&inproc, "other");

        assert_eq!(
            displayed_pane_fate(&inproc, a_pane),
            DisplayedPaneFate::Keep,
            "a running pane is kept"
        );

        inproc.kill_session(a).expect("delete the shown session");
        assert_eq!(
            displayed_pane_fate(&inproc, a_pane),
            DisplayedPaneFate::Leave {
                reap: None,
                next: Some(b_pane)
            },
            "deleted → move to the next live pane"
        );

        inproc.kill_session(b).expect("delete the last session");
        assert_eq!(
            displayed_pane_fate(&inproc, a_pane),
            DisplayedPaneFate::Leave {
                reap: None,
                next: None
            },
            "nothing left → the window closes, as it does for a last exited shell"
        );
    }

    /// The pre-existing arm, preserved: a displayed pane whose shell EXITED is
    /// still registered — tear keeps a WATCHED session (remain-on-exit), and
    /// the window's byte subscription is what makes it watched — so its
    /// session is reaped first.
    #[test]
    fn an_exited_displayed_pane_is_reaped_then_left() {
        let inproc = tear_core::InProcess::new();
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let (sid, pane) = live_session(&inproc, "exits");
        // The window's attach: without a live subscriber tear reaps the
        // session itself on exit, which is the OTHER (absent) shape.
        let _window = inproc.subscribe_pane_bytes(pane).expect("subscribe");
        inproc.send_keys(pane, b"exit\n").expect("type exit");
        // Bounded wait for the child to exit and tear to mark the pane.
        let mut fate = DisplayedPaneFate::Keep;
        for _ in 0..200 {
            fate = displayed_pane_fate(&inproc, pane);
            if fate != DisplayedPaneFate::Keep {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(
            fate,
            DisplayedPaneFate::Leave {
                reap: Some(sid),
                next: None
            }
        );
    }

    #[derive(Default)]
    struct Rings(std::sync::Mutex<usize>, std::sync::Condvar);

    impl std::task::Wake for Rings {
        fn wake(self: std::sync::Arc<Self>) {
            *self.0.lock().unwrap() += 1;
            self.1.notify_all();
        }
    }

    impl Rings {
        fn count(&self) -> usize {
            *self.0.lock().unwrap()
        }

        fn past(&self, n: usize) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut got = self.0.lock().unwrap();
            while *got <= n {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                got = self.1.wait_timeout(got, left).unwrap().0;
            }
            true
        }
    }

    struct IdleRun {
        get_pane: u64,
        wakes: u64,
        rang_for_output: bool,
        end: DisplayedPaneFate,
        end_get_pane: u64,
    }

    fn idle_window(policy: crate::config::PaneFate) -> IdleRun {
        use crate::perf::TearCall;
        use std::time::{Duration, Instant};
        let inproc = std::sync::Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let (sid, pane) = live_session(&inproc, "idle");
        let calls: &'static crate::perf::TearCalls =
            Box::leak(Box::new(kanshou::metrics::Family::new()));
        let control = crate::perf::Counted::counting_into(std::sync::Arc::clone(&inproc), calls);
        let terminal: crate::render::SharedTerminal = std::sync::Arc::new(
            parking_lot::RwLock::new(crate::terminal::Terminal::with_scrollback(80, 24, 100)),
        );
        let rings = std::sync::Arc::new(Rings::default());
        let bell = std::task::Waker::from(std::sync::Arc::clone(&rings));
        let mut stream = crate::pane_stream::PaneStream::attach(
            |waker| control.producer(pane, waker),
            calls,
            terminal,
            std::sync::Arc::new(|_: &[u8]| {}),
            &bell,
        )
        .expect("attach");
        let quiet = rings.count();
        inproc.send_keys(pane, b"echo idle\n").expect("type");
        let rang_for_output = rings.past(quiet);
        let _ui = crate::perf::UiThread::mark();
        let start = Instant::now();
        let mut fate = crate::pane_stream::FateWatch::new(policy, 30, start);
        std::thread::sleep(Duration::from_millis(300));
        let _ = stream.drain(crate::pane_stream::DRAIN_BUDGET);
        let before = calls.get(TearCall::GetPane);
        let mut wakes = 0;
        for tick in 0..240u64 {
            let now = start + Duration::from_micros(tick * 16_667);
            assert_eq!(
                pump_displayed(&mut stream, &mut fate, &control, pane, now),
                DisplayedPaneFate::Keep,
                "a running pane stays"
            );
            wakes += 1;
        }
        let get_pane = calls.get(TearCall::GetPane) - before;
        let rung = rings.count();
        inproc.kill_session(sid).expect("kill the shown session");
        assert!(rings.past(rung), "the stream's end rang the window");
        let before = calls.get(TearCall::GetPane);
        let end = pump_displayed(
            &mut stream,
            &mut fate,
            &control,
            pane,
            start + Duration::from_secs(5),
        );
        IdleRun {
            get_pane,
            wakes,
            rang_for_output,
            end,
            end_get_pane: calls.get(TearCall::GetPane) - before,
        }
    }

    #[test]
    fn an_idle_window_reads_no_pane_fate_and_reads_it_once_when_the_stream_ends() {
        let idle = idle_window(crate::config::PaneFate::Edge);
        assert!(idle.rang_for_output, "output rang the window's bell");
        assert_eq!(
            idle.get_pane, 0,
            "UI-thread get_pane over {} idle wakes",
            idle.wakes
        );
        assert_eq!(
            idle.end,
            DisplayedPaneFate::Leave {
                reap: None,
                next: None
            }
        );
        assert_eq!(idle.end_get_pane, 1, "the end is read once");
    }

    #[test]
    fn the_poll_control_reads_the_fate_on_every_idle_wake() {
        let idle = idle_window(crate::config::PaneFate::Poll);
        assert_eq!(
            idle.get_pane, idle.wakes,
            "pane_fate: poll is today's one get_pane per idle tick"
        );
        assert_eq!(
            idle.end,
            DisplayedPaneFate::Leave {
                reap: None,
                next: None
            }
        );
    }

    /// A backend that cannot be read is BLIND: the window keeps its pane. A
    /// daemon RPC failing for a moment must never close the operator's window.
    #[test]
    fn an_unreadable_backend_keeps_the_displayed_pane() {
        assert_eq!(
            displayed_pane_fate(&crate::control_stub::Unreachable, PaneId::from_seed("p")),
            DisplayedPaneFate::Keep
        );
    }

    /// The boot naming authority: a cwd inside a git project resolves to
    /// the deterministic identity of the PROJECT ROOT (not the cwd), and
    /// is stable across calls. The EMOJI projection (the picker) is
    /// byte-identical to the direct `FleetSessionNames::from_project_path`,
    /// and the GLYPH projection (the prompt) carries the SAME word — so the
    /// helper is a thin caller of the one authority, and the two surface
    /// renderings can never disagree.
    #[test]
    fn resolve_boot_name_project_is_deterministic_and_split_consistent() {
        use ishou_tokens::SessionNameStyle::{Emoji, Glyph};
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).expect("mk .git");
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("mk nested");

        let project_root = praca::project::project_root(&nested);
        assert_eq!(project_root, root, "project_root walks up to the .git root");

        let expected_emoji =
            ishou_tokens::FleetSessionNames::from_project_path(&project_root, Emoji).to_string();

        let a = resolve_boot_name(None, Some(nested.as_path()));
        let b = resolve_boot_name(None, Some(nested.as_path()));
        assert_eq!(
            a.render(Emoji),
            expected_emoji,
            "emoji form = the project identity"
        );
        assert_eq!(
            a.render(Emoji),
            b.render(Emoji),
            "same project → same name, always"
        );
        // Split projection: glyph (prompt) and emoji (picker) share the word.
        assert_eq!(a.word(), b.word());
        assert!(
            a.render(Glyph).ends_with(a.word()),
            "glyph form carries the same word"
        );
        assert!(!a.render(Emoji).is_empty());
    }

    /// No spawn cwd (Finder/launchd launch) → the branded fleet default
    /// `🌑 rime` (NOT an opaque `mado-<ts>-<pid>` tag). The prompt gets the
    /// grid-safe glyph `◉ rime`; the picker gets the emoji; same word.
    #[test]
    fn resolve_boot_name_none_is_the_branded_default() {
        use ishou_tokens::SessionNameStyle::{Emoji, Glyph};
        let name = resolve_boot_name(None, None);
        assert_eq!(name.render(Emoji), "🌑 rime", "picker projection");
        assert_eq!(name.render(Glyph), "◉ rime", "grid-safe prompt projection");
        assert_eq!(name.word(), "rime");
    }

    /// An explicit operator override (`tear.session_name`) wins over both
    /// project + default and is used verbatim (no curated mark) in BOTH
    /// projections — so the picker and the prompt still agree.
    #[test]
    fn resolve_boot_name_override_wins_verbatim() {
        use ishou_tokens::SessionNameStyle::{Emoji, Glyph};
        let name = resolve_boot_name(Some("billing-stack"), Some(std::path::Path::new("/code/x")));
        assert_eq!(name.render(Emoji), "billing-stack");
        assert_eq!(name.render(Glyph), "billing-stack");
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
