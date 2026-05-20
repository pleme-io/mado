//! Smoke test for the embedded-tear engate pipeline.
//!
//! Verifies the types compose: build a `tear_core::InProcess`,
//! spawn a session, construct the engate Producer + Consumer +
//! Attach lifecycle, drain a few bytes, and assert the consumer
//! observed them. Same shape as `mado::gui_tear_attach::run_against_embedded_pane`
//! minus the GUI window — proves the embedded-mode wiring works at
//! the engate type level without needing a launched binary.
//!
//! Operationally meaningful: a passing test here is sufficient
//! evidence that `mado.tear.runtime = embedded` won't fail at
//! engate construction time. The remaining verification (the
//! visual paint loop) needs a real wgpu surface and is necessarily
//! manual.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use engate_attach::{Attach, Consumer, Producer};
use tear_core::engate_producer::PaneProducer;
use tear_core::InProcess;
use tear_types::engate_wrap::PaneSnapshotWrap;
use tear_types::{MultiplexerControl, SessionSource};

/// Minimal consumer that records every replay byte + live byte.
/// Mirrors mado's TerminalSink in shape but doesn't require the
/// full mado::terminal::Terminal type.
struct RecordingConsumer(Arc<Mutex<Vec<u8>>>);

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

#[test]
fn embedded_engate_attach_drains_snapshot_and_live() {
    let inproc = Arc::new(InProcess::new());
    let sid = inproc
        .new_session_with_source_and_size(
            "embedded-smoke",
            "/bin/sh",
            SessionSource::Named("mado-embedded-smoke".into()),
            (80, 24),
        )
        .expect("spawn session");
    let pane = inproc
        .with_registry(|r| {
            r.sessions
                .get(&sid)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
        .expect("session must have a pane");

    // Give the shell ~100ms to print its prompt into the pane grid.
    // Without this the snapshot replay might be empty (still valid
    // structurally — the typestate is happy — but the test would be
    // weaker since no bytes flow).
    std::thread::sleep(Duration::from_millis(200));

    let observed = Arc::new(Mutex::new(Vec::<u8>::new()));
    let producer = PaneProducer::new(Arc::clone(&inproc), pane);
    let consumer = RecordingConsumer(observed.clone());

    // Full engate typestate transition — the same shape mado runs
    // in embedded mode.
    let attach = Attach::builder().producer(producer).consumer(consumer).build();
    let (attach, history) = attach.subscribe().expect("subscribe");
    assert!(
        history.size_bytes() > 0,
        "embedded snapshot should have captured the shell's prompt"
    );
    let attach = attach.replay(history).expect("replay");
    let mut attach = attach.start_live();

    // poll_one until no more pending live items OR 100 polls (~10ms).
    for _ in 0..100 {
        if !attach.poll_one() {
            break;
        }
    }

    let observed_bytes = observed.lock().unwrap();
    // Replay alone should give us at LEAST the ANSI prelude
    // (`\x1b[0m\x1b[2J\x1b[H...`) plus per-row CSI cursor positioning
    // plus the prompt characters.
    assert!(observed_bytes.starts_with(b"\x1b[0m\x1b[2J\x1b[H"));
    let s = String::from_utf8_lossy(&observed_bytes);
    // The macOS /bin/sh prompt is "sh-3.2$ ". The replay must
    // include those characters in the byte stream.
    assert!(
        s.contains("sh-3.2$") || s.contains("$"),
        "expected shell prompt in embedded snapshot replay, got {:?}",
        &s[..s.len().min(200)]
    );
}

#[test]
fn embedded_engate_producer_snapshot_then_subscribe_ordering() {
    let inproc = Arc::new(InProcess::new());
    let sid = inproc
        .new_session_with_source_and_size(
            "embedded-ordering",
            "/bin/sh",
            SessionSource::Named("mado-embedded-ordering".into()),
            (40, 10),
        )
        .expect("spawn");
    let pane = inproc
        .with_registry(|r| {
            r.sessions
                .get(&sid)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
        .expect("pane");
    let producer = PaneProducer::new(Arc::clone(&inproc), pane);

    // Per engate contract: subscribe() must succeed even when called
    // before any output has been produced. Returns a fresh receiver
    // that will deliver every byte from now on.
    let rx = producer.subscribe().expect("subscribe is idempotent + lossless");
    // Snapshot also succeeds — captures whatever happens to be in
    // the grid at this instant (may be empty).
    let snap = producer.snapshot().expect("snapshot succeeds");
    assert_eq!(snap.0.cols, 40);
    assert_eq!(snap.0.rows, 10);

    // After subscribe, sending bytes via control plane should deliver
    // them to the rx side.
    inproc.send_keys(pane, b"X").expect("send_keys");
    let item = rx
        .recv_timeout(Duration::from_millis(500))
        .expect("live items flow after subscribe");
    // The shell echoes the X back (terminal in cooked mode).
    assert!(item.contains(&b'X'));
}
