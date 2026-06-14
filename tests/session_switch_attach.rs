//! Proof: the switchable-attach teardown + rebuild + terminal-swap
//! seam (`tear.session_switching = true`).
//!
//! mado is a binary-only crate, so `tests/` can't import the GUI's
//! internal `Terminal` / `TerminalSink` / `run_against_pane_unified`.
//! This test instead exercises the EXACT engate mechanism the
//! switchable event loop performs, against a headless
//! `tear_core::InProcess` holding TWO live panes:
//!
//!   1. Attach to pane A (subscribe → replay → live). Assert the
//!      consumer's model reflects A's marker.
//!   2. Perform the switch: DROP the attach against A (unsubscribes A
//!      WITHOUT EOF-ing its PTY — A stays alive), RESET the shared
//!      consumer model (mirrors `terminal.write().reset()`), then build
//!      a FRESH attach against pane B + replay.
//!   3. Assert the model now reflects pane B's marker and NOT pane A's.
//!
//! This is the headless analogue of the GPU proof: with switching ON,
//! the displayed terminal re-binds from session A to a different
//! in-process session B, same `InProcess` control plane, same
//! consumer-visible model object — pane B's content replaces pane A's.
//!
//! The `RecordingConsumer` here mirrors mado's `TerminalSink` in
//! shape (replay + live both append bytes; `reset` clears) without
//! requiring the full VT `Terminal` type — the seam being proved is
//! the engate attach lifecycle + the reset-on-switch, not VT parsing
//! (which `embedded_engate_smoke.rs` + the unit suite already cover).

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use engate_attach::{Attach, Consumer};
use tear_core::engate_producer::PaneProducer;
use tear_core::InProcess;
use tear_types::engate_wrap::PaneSnapshotWrap;
use tear_types::{MultiplexerControl, PaneId, SessionSource};

/// Consumer mirroring mado's `TerminalSink` shape: replay + live both
/// append into a shared model; `reset` clears it (the switch's
/// `terminal.write().reset()`).
#[derive(Clone)]
struct RecordingConsumer(Arc<Mutex<Vec<u8>>>);

impl RecordingConsumer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    /// The switch's terminal reset — clear the model so the prior
    /// pane's grid can't bleed under the new pane's replay.
    fn reset(&self) {
        self.0.lock().unwrap().clear();
    }
}

impl Consumer for RecordingConsumer {
    type Item = Vec<u8>;
    type Snap = PaneSnapshotWrap;
    fn replay(&mut self, snapshot: Self::Snap) {
        self.0.lock().unwrap().extend(snapshot.to_ansi());
    }
    fn consume(&mut self, item: Self::Item) {
        self.0.lock().unwrap().extend(item);
    }
}

/// Spawn a `/bin/sh` session in `inproc`, return the id of its first
/// pane.
fn spawn_pane(inproc: &Arc<InProcess>, name: &str) -> PaneId {
    let sid = inproc
        .new_session_with_source_and_size(
            name,
            "/bin/sh",
            SessionSource::Named(name.into()),
            (80, 24),
        )
        .expect("spawn session");
    inproc
        .with_registry(|r| {
            r.sessions
                .get(&sid)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
        .expect("session must have a pane")
}

/// Write a marker into a pane + wait until its snapshot shows it (the
/// shell echoes + runs `printf`). Bounded poll so a slow CI box
/// doesn't flake.
fn write_marker_and_wait(inproc: &Arc<InProcess>, pane: PaneId, marker: &str) {
    let cmd = format!("printf '{marker}\\n'\n");
    inproc.send_keys(pane, cmd.as_bytes()).expect("send_keys");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let snap = inproc.pane_snapshot(pane).expect("snapshot");
        let text = String::from_utf8_lossy(&snap.to_ansi()).into_owned();
        if text.contains(marker) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("marker {marker:?} never appeared in pane {pane}");
}

#[test]
fn switch_tears_down_pane_a_and_rebuilds_against_pane_b() {
    // ── Setup: one InProcess, two live panes with distinct markers ──
    let inproc = Arc::new(InProcess::new());
    let pane_a = spawn_pane(&inproc, "switch-proof-A");
    let pane_b = spawn_pane(&inproc, "switch-proof-B");
    assert_ne!(pane_a, pane_b, "two distinct in-process panes");

    let marker_a = "SWITCH_PROOF_PANE_A_4F1A";
    let marker_b = "SWITCH_PROOF_PANE_B_9C3E";
    write_marker_and_wait(&inproc, pane_a, marker_a);
    write_marker_and_wait(&inproc, pane_b, marker_b);

    // ── Step 1: attach to pane A (the displayed pane) ──────────────
    // The shared model is the renderer-visible terminal in the real
    // loop; here it's the RecordingConsumer's buffer.
    let model = RecordingConsumer::new();
    let producer_a = PaneProducer::new(Arc::clone(&inproc), pane_a);
    let attach_a = Attach::builder()
        .producer(producer_a)
        .consumer(model.clone())
        .build();
    let (attach_a, history_a) = attach_a.subscribe().expect("subscribe A");
    let attach_a = attach_a.replay(history_a).expect("replay A");
    let mut attach_a = attach_a.start_live();
    for _ in 0..256 {
        if !attach_a.poll_one() {
            break;
        }
    }
    assert!(
        model.text().contains(marker_a),
        "displayed model must show pane A's marker after attaching to A; got {:?}",
        &model.text()[..model.text().len().min(400)]
    );
    assert!(
        !model.text().contains(marker_b),
        "pane B's marker must NOT be visible while attached to A"
    );

    // ── Step 2: the switch — exactly what the event loop does ──────
    //   (a) drop the attach against A. This unsubscribes A's byte
    //       channel but does NOT EOF its PTY — pane A stays alive.
    drop(attach_a);
    //   (b) pane A is still alive (the drop did not kill it).
    assert!(
        inproc.pane_snapshot(pane_a).is_ok(),
        "dropping the attach must not kill pane A's PTY"
    );
    //   (c) reset the shared model (terminal.write().reset()) so A's
    //       grid can't bleed under B's replay.
    model.reset();
    assert!(model.text().is_empty(), "reset clears the displayed model");
    //   (d) build a FRESH attach against pane B + replay. Same
    //       InProcess control plane, same model object.
    let producer_b = PaneProducer::new(Arc::clone(&inproc), pane_b);
    let attach_b = Attach::builder()
        .producer(producer_b)
        .consumer(model.clone())
        .build();
    let (attach_b, history_b) = attach_b.subscribe().expect("subscribe B");
    let attach_b = attach_b.replay(history_b).expect("replay B");
    let mut attach_b = attach_b.start_live();
    for _ in 0..256 {
        if !attach_b.poll_one() {
            break;
        }
    }

    // ── Step 3: the displayed model now reflects B, not A ──────────
    let after = model.text();
    assert!(
        after.contains(marker_b),
        "after the switch the displayed model must show pane B's marker; got {:?}",
        &after[..after.len().min(400)]
    );
    assert!(
        !after.contains(marker_a),
        "pane A's marker must be GONE after switching to B (reset + B-only replay); got {:?}",
        &after[..after.len().min(400)]
    );

    // ── And input now reaches B (re-targeting proof) ──────────────
    // The real loop re-points its pty_sink at B via the shared
    // CurrentPane cell; here we drive the control plane directly to
    // prove B is live + writable post-switch.
    let echo_marker = "SWITCH_PROOF_ECHO_B_77AA";
    write_marker_and_wait(&inproc, pane_b, echo_marker);
    for _ in 0..256 {
        if !attach_b.poll_one() {
            break;
        }
    }
    assert!(
        model.text().contains(echo_marker),
        "live bytes from pane B must flow into the displayed model after the switch"
    );
}
