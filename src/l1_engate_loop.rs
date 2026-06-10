//! L1 — headless engate loop against a REAL shell (no GPU).
//!
//! docs/INTEGRATION-TESTING.md §L1, M1 brick. The production seam from
//! `gui_tear_attach::run_against_pane_unified` minus the renderer +
//! window: real `tear_core::InProcess` PTY + real engate Producer +
//! the REAL `TerminalSink` with the production-shaped `ResponseWriter`
//! (VT answers forwarded back via `control.send_keys`) + the typed
//! `ProbeCounters`. The recording-consumer smoke test
//! (`tests/embedded_engate_smoke.rs`) proves the types compose; this
//! module proves the *loop* — query answered, Enter survives (the
//! 2026-06-10 E5 death point), and a command round-trips into the
//! grid. "Process alive" ≠ "shell interactive": every phase below
//! asserts forward progress, never liveness.
//!
//! Lives as a unit-test module (not `tests/`) because mado is a
//! binary-only crate — only unit tests can reach the real
//! `crate::engate_consumer::TerminalSink`.
//!
//! Local-only by construction (boot_harness pattern): resolves the
//! shell from `MADO_L1_SHELL`, else the per-user Nix profile
//! frostmourne, else SKIPs with an eprintln. CI runners without a
//! deployed frostmourne skip cleanly; cid-class workstations run the
//! real thing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use engate_attach::Attach;
use parking_lot::RwLock;
use tear_core::engate_producer::PaneProducer;
use tear_core::InProcess;
use tear_types::{MultiplexerControl, SessionSource};

use crate::engate_consumer::{ProbeCounters, ResponseWriter, TerminalSink};
use crate::terminal::Terminal;

/// Per-phase deadline. Generous vs the <100ms expectation so a cold
/// frostmourne boot (rc load, wadachi open) never flakes the row.
const PHASE_DEADLINE: Duration = Duration::from_secs(5);

/// Resolve the real shell binary for the L1 row.
///
/// Order mirrors the boot_harness local-only pattern:
/// 1. `MADO_L1_SHELL` env override (any shell — bash/zsh work too),
/// 2. the per-user Nix profile frostmourne (the deployed fleet shell),
/// 3. `None` → the test SKIPs.
fn resolve_l1_shell() -> Option<String> {
    if let Ok(sh) = std::env::var("MADO_L1_SHELL") {
        if !sh.trim().is_empty() {
            return Some(sh);
        }
    }
    let user = std::env::var("USER").ok()?;
    let candidate = format!("/etc/profiles/per-user/{user}/bin/frostmourne");
    std::path::Path::new(&candidate)
        .exists()
        .then_some(candidate)
}

/// Poll `cond` every 20ms until it holds or `deadline` elapses.
/// Returns the final evaluation so callers assert on the result.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

/// Render the terminal's visible grid as plain text — same accessor
/// chain the renderer + MCP `get_output` use (`visible_rows()` +
/// `Cell::write_to`), rows joined with `\n`.
fn visible_text(term: &Arc<RwLock<Terminal>>) -> String {
    let t = term.read();
    let mut out = String::new();
    for row in t.visible_rows() {
        for cell in row {
            cell.write_to(&mut out);
        }
        out.push('\n');
    }
    out
}

#[test]
fn l1_real_shell_queries_answered_and_command_round_trips() {
    let Some(shell) = resolve_l1_shell() else {
        eprintln!(
            "SKIP: l1_real_shell_queries_answered_and_command_round_trips — \
             no MADO_L1_SHELL set and no per-user frostmourne profile binary \
             (local-only row, mirrors the boot_harness pattern)"
        );
        return;
    };

    // ── tear InProcess session — production embedded-mode shape ──
    let inproc = Arc::new(InProcess::new());
    let sid = inproc
        .new_session_with_source_and_size(
            "mado-l1-engate-loop",
            &shell,
            SessionSource::Named("mado-l1".into()),
            (80, 24),
        )
        .expect("spawn real shell session");
    let pane = inproc
        .with_registry(|r| {
            r.sessions
                .get(&sid)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
        .expect("session must have a pane");

    // ── REAL TerminalSink + production-shaped ResponseWriter ──
    // Same wiring as gui_tear_attach.rs (run_against_pane_unified):
    // VT answers flow back to the pane's PTY via control.send_keys.
    // NO startup sleep before attach — the consumer must be live
    // before the shell paints its first prompt so the startup CPR
    // flows through the sink, exactly as in production.
    let terminal: Arc<RwLock<Terminal>> =
        Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 10_000)));
    let control_for_response_writer = Arc::clone(&inproc);
    let response_writer: ResponseWriter = Arc::new(move |bytes: &[u8]| {
        // Production uses tracing::warn! here; eprintln keeps the
        // failure visible in test output without a subscriber.
        if let Err(e) = control_for_response_writer.send_keys(pane, bytes) {
            eprintln!("L1: VT query response write-back FAILED: {e}");
        }
    });
    let probes = Arc::new(ProbeCounters::default());
    let consumer = TerminalSink::new_with_probes(
        Arc::clone(&terminal),
        response_writer,
        Arc::clone(&probes),
    );

    // ── engate typed Attach lifecycle + live pump thread ──
    // Identical shape to production: subscribe → replay → start_live,
    // then run() on a named thread until the pane's channel closes.
    let attach = Attach::builder()
        .producer(PaneProducer::new(Arc::clone(&inproc), pane))
        .consumer(consumer)
        .build();
    let (attach, history) = attach.subscribe().expect("engate.subscribe");
    let attach = attach.replay(history).expect("engate.replay");
    let attach_live = attach.start_live();
    let attach_thread = std::thread::Builder::new()
        .name("mado-l1-engate-live".into())
        .spawn(move || {
            let _consumer = attach_live.run();
        })
        .expect("spawn engate live thread");

    // ── Phase 1: startup query answered ──
    // The shell's first prompt paint sends DSR-6 (CPR); the sink must
    // answer it. Poll the PAIR to convergence — between the two
    // fetch_adds an observer can see responses lag by one.
    let answered = wait_until(PHASE_DEADLINE, || {
        probes.queries_seen() >= 1 && probes.responses_written() == probes.queries_seen()
    });
    assert!(
        answered,
        "startup VT query not answered within {PHASE_DEADLINE:?}: \
         queries_seen={} responses_written={}\n--- grid ---\n{}",
        probes.queries_seen(),
        probes.responses_written(),
        visible_text(&terminal)
    );

    // ── Phase 2: post-Enter query answered — the E5 death point ──
    // 2026-06-10: startup CPR worked, the one after accept-line was
    // the answer that got lost. A NEW query must arrive after Enter
    // and be answered.
    let baseline = probes.queries_seen();
    inproc.send_keys(pane, b"\r").expect("send Enter");
    let post_enter = wait_until(PHASE_DEADLINE, || {
        probes.queries_seen() > baseline
            && probes.responses_written() == probes.queries_seen()
    });
    assert!(
        post_enter,
        "no NEW answered VT query within {PHASE_DEADLINE:?} after Enter \
         (the 2026-06-10 death point): baseline={baseline} queries_seen={} \
         responses_written={}\n--- grid ---\n{}",
        probes.queries_seen(),
        probes.responses_written(),
        visible_text(&terminal)
    );

    // ── Phase 3: command round-trip — never just aliveness ──
    // The marker must land in the grid twice: once echoed on the
    // prompt line (`echo L1_MARKER`), once as command output.
    inproc
        .send_keys(pane, b"echo L1_MARKER\r")
        .expect("send echo command");
    let round_tripped = wait_until(PHASE_DEADLINE, || {
        visible_text(&terminal).matches("L1_MARKER").count() >= 2
    });
    let grid = visible_text(&terminal);
    assert!(
        round_tripped,
        "echo round-trip failed: expected L1_MARKER twice (echo + output) \
         within {PHASE_DEADLINE:?}, found {} occurrence(s). \
         queries_seen={} responses_written={}\n--- grid ---\n{grid}",
        grid.matches("L1_MARKER").count(),
        probes.queries_seen(),
        probes.responses_written(),
    );

    // Invariant held end-to-end: every answered query was written back.
    assert_eq!(
        probes.responses_written(),
        probes.queries_seen(),
        "query↔answer gap at end of run — answers were generated then dropped"
    );

    // Teardown mirrors production embedded mode: NO kill_session —
    // the embedded session dies with the process (gui_tear_attach
    // passes owned_session_id=None for exactly this reason). Calling
    // kill_session here deadlocks: InProcess holds its registry lock
    // while PtyHandle::Drop does a blocking Child::wait(), and the
    // tear-pty-reader thread blocks on that same lock (observed
    // 2026-06-10, this test's first run). Instead: ask the shell to
    // exit; when it does, the PTY EOFs, the engate channel closes,
    // and the live thread's run() returns. Bounded, best-effort —
    // the harness's process exit collects anything left.
    let _ = inproc.send_keys(pane, b"exit\r");
    let _ = wait_until(PHASE_DEADLINE, || attach_thread.is_finished());
}
