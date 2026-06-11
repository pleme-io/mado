//! engate::Consumer impl over mado's `Arc<RwLock<Terminal>>`.
//!
//! Mado's `Terminal` is a VT-parser-backed terminal model held under
//! `Arc<RwLock<Terminal>>` so the renderer can `.read()` snapshot
//! while the subscribe pump `.write()`-feeds bytes. The engate
//! Consumer trait wants `&mut self`, so we wrap the Arc/RwLock in
//! a `TerminalSink` that owns the locking — the engate `Attach`
//! builder gets a clean Consumer impl + the locking stays where it
//! belongs.
//!
//! After this lands, mado's gui_tear_attach call site collapses
//! from ~30 lines of subscribe-callback-then-feed plumbing to:
//!
//! ```rust,ignore
//! let attach = engate_attach::Attach::builder()
//!     .producer(producer)
//!     .consumer(TerminalSink::new(terminal))
//!     .build();
//! let (attach, history) = attach.subscribe()?;
//! let attach = attach.replay(history)?.start_live();
//! attach.poll_one();  // inside the render-loop tick
//! ```
//!
//! Whether `producer` is `tear_core::engate_producer::PaneProducer`
//! (embedded mode) or `tear_client::engate_producer::PaneProducer`
//! (daemon mode) is a one-line config branch in `gui_tear_attach`;
//! the Consumer impl below is identical for both.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use engate_attach::Consumer;
use parking_lot::RwLock;
use tear_types::engate_wrap::PaneSnapshotWrap;

use crate::terminal::Terminal;

/// Typed VT-response writeback callback — promoted to
/// `ux::sinks::ResponseWriter` at M1 (the `PtySink` trait is the
/// generalized shape); re-exported here for back-compat. The full
/// contract (incl. the 2026-05-21 frostmourne CPR-timeout incident
/// it exists for) is documented at the definition.
pub use crate::ux::ResponseWriter;

/// Typed probe counters for the VT query↔answer loop — the
/// 2026-06-10 freeze-on-Enter incident probes made permanent
/// (docs/INTEGRATION-TESTING.md §L1). Shared via `Arc` so a test
/// (or future kanshou leaf) can observe the live attach loop from
/// outside the consumer thread.
///
/// Invariant the pair encodes: after the loop quiesces,
/// `responses_written == queries_seen` — every DSR/DA/OSC query the
/// VT engine answered actually made it back through the
/// `ResponseWriter`. A persistent gap means answers are being
/// generated and then dropped (the exact failure shape that killed
/// reedline-based shells).
#[derive(Debug, Default)]
pub struct ProbeCounters {
    /// Queries the VT engine answered: incremented once per feed
    /// whose `take_response()` yielded bytes. (One feed carrying two
    /// queries drains as one response batch — counted once.)
    queries_seen: AtomicU64,
    /// Responses pushed back through the `ResponseWriter` callback:
    /// incremented after the writer returns.
    responses_written: AtomicU64,
}

impl ProbeCounters {
    /// Queries answered by the VT engine so far.
    #[must_use]
    pub fn queries_seen(&self) -> u64 {
        self.queries_seen.load(Ordering::Relaxed)
    }

    /// Responses written back through the `ResponseWriter` so far.
    #[must_use]
    pub fn responses_written(&self) -> u64 {
        self.responses_written.load(Ordering::Relaxed)
    }
}

/// engate Consumer wrapping an Arc<RwLock<Terminal>>. Clone-cheap;
/// the inner Arc clone is the only cost. Designed for a single
/// engate attach per Terminal — the engate typestate enforces this
/// at compile time.
///
/// Now also owns a `ResponseWriter` callback: after every `feed`
/// the VT response queue is drained and pushed back through the
/// writer. Shells that send DSR/DA/OSC queries get answers.
pub struct TerminalSink {
    inner: Arc<RwLock<Terminal>>,
    writer: ResponseWriter,
    probes: Arc<ProbeCounters>,
}

impl TerminalSink {
    /// Construct a sink with an explicit VT-response writeback path.
    /// Pass a closure that forwards the response bytes to the
    /// pane's PTY (typically wraps `control.send_keys(pane_id, ..)`).
    /// Probe counters default to a fresh (private) set; use
    /// `Self::new_with_probes` when an observer needs them.
    #[must_use]
    pub fn new(terminal: Arc<RwLock<Terminal>>, writer: ResponseWriter) -> Self {
        Self::new_with_probes(terminal, writer, Arc::new(ProbeCounters::default()))
    }

    /// `Self::new` with caller-supplied probe counters. The caller
    /// keeps a clone of the `Arc` and observes the query↔answer loop
    /// while the sink runs on the attach thread (L1 harness shape).
    #[must_use]
    pub fn new_with_probes(
        terminal: Arc<RwLock<Terminal>>,
        writer: ResponseWriter,
        probes: Arc<ProbeCounters>,
    ) -> Self {
        Self { inner: terminal, writer, probes }
    }

    /// The sink's probe counters (clone of the shared handle).
    #[must_use]
    pub fn probes(&self) -> Arc<ProbeCounters> {
        Arc::clone(&self.probes)
    }

    /// Backwards-compat constructor that drops VT responses on the
    /// floor. Equivalent to the pre-2026-05-21 behaviour; useful for
    /// tests that don't care about query handshakes. Prefer
    /// `Self::new` with a real writer for production paths.
    #[must_use]
    pub fn new_without_writeback(terminal: Arc<RwLock<Terminal>>) -> Self {
        Self::new(terminal, Arc::new(|_: &[u8]| {}))
    }

    /// Helper: feed bytes + drain any VT response back through
    /// the writer in one atomic step. Used by `consume` + `replay`.
    fn feed_and_drain(&self, bytes: &[u8]) {
        let mut term = self.inner.write();
        term.feed(bytes);
        // Drain inside the write-lock so we don't race with another
        // feed populating response_bytes between drop and re-grab.
        if let Some(resp) = term.take_response() {
            drop(term);
            // Probe ordering matters: queries_seen first, writer,
            // then responses_written — an observer polling the pair
            // sees `responses_written <= queries_seen` converge to
            // equality once the loop quiesces. Relaxed is enough:
            // the counters are diagnostics, not synchronization.
            self.probes.queries_seen.fetch_add(1, Ordering::Relaxed);
            (self.writer)(&resp);
            self.probes.responses_written.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Consumer for TerminalSink {
    type Item = Vec<u8>;
    type Snap = PaneSnapshotWrap;

    fn replay(&mut self, snapshot: Self::Snap) {
        // Replay = feed the ANSI serialization of the producer's
        // current grid through mado's VT parser. Idempotent at the
        // parser level — same bytes the daemon-mode M0 path delivers
        // as the first PaneBytes frame.
        let bytes = snapshot.to_ansi();
        self.feed_and_drain(&bytes);
    }

    fn consume(&mut self, item: Self::Item) {
        // Live items are raw PTY bytes (or, in embedded mode, the
        // bytes the InProcess::subscribe_pane_bytes channel emits).
        self.feed_and_drain(&item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn collecting_writer() -> (ResponseWriter, Arc<Mutex<Vec<Vec<u8>>>>) {
        let collected: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let writer_buf = Arc::clone(&collected);
        let w: ResponseWriter = Arc::new(move |bytes: &[u8]| {
            writer_buf.lock().unwrap().push(bytes.to_vec());
        });
        (w, collected)
    }

    #[test]
    fn dsr_cursor_position_response_flows_back_through_writer() {
        // The runaway-frostmourne incident: shell sends DSR-6
        // (cursor position), expects \x1b[<row>;<col>R back.
        // Before the writer wiring, that answer was generated by
        // the VT engine and then silently dropped.
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let (writer, collected) = collecting_writer();
        let mut sink = TerminalSink::new(Arc::clone(&term), writer);
        // Drive the DSR query through `consume`.
        sink.consume(b"\x1b[6n".to_vec());
        let captured = collected.lock().unwrap();
        assert_eq!(captured.len(), 1, "exactly one writeback expected");
        let resp = &captured[0];
        // Cursor starts at 1;1 (1-based) in a fresh terminal.
        assert_eq!(resp, b"\x1b[1;1R");
    }

    #[test]
    fn dsr_5_status_report_flows_back_too() {
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let (writer, collected) = collecting_writer();
        let mut sink = TerminalSink::new(Arc::clone(&term), writer);
        sink.consume(b"\x1b[5n".to_vec());
        let captured = collected.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(&captured[0], b"\x1b[0n");
    }

    #[test]
    fn plain_text_does_not_trigger_writer() {
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let (writer, collected) = collecting_writer();
        let mut sink = TerminalSink::new(Arc::clone(&term), writer);
        sink.consume(b"hello".to_vec());
        assert!(collected.lock().unwrap().is_empty(),
                "plain text should not generate any VT response");
    }

    #[test]
    fn new_without_writeback_drops_responses_silently() {
        // Tests that don't care about query handshakes can still
        // get the old behaviour via the explicit no-op constructor.
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let mut sink = TerminalSink::new_without_writeback(Arc::clone(&term));
        // No panic, no observable side effect.
        sink.consume(b"\x1b[6n".to_vec());
    }

    #[test]
    fn probe_counters_track_answered_queries() {
        // The incident probes made permanent: every answered query
        // bumps queries_seen, every completed writeback bumps
        // responses_written, and after the loop quiesces the pair
        // is equal.
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let (writer, collected) = collecting_writer();
        let probes = Arc::new(ProbeCounters::default());
        let mut sink =
            TerminalSink::new_with_probes(Arc::clone(&term), writer, Arc::clone(&probes));
        assert_eq!(sink.probes().queries_seen(), 0);
        assert_eq!(sink.probes().responses_written(), 0);

        sink.consume(b"\x1b[6n".to_vec()); // DSR-6 → CPR answer
        sink.consume(b"\x1b[5n".to_vec()); // DSR-5 → status report
        assert_eq!(probes.queries_seen(), 2);
        assert_eq!(probes.responses_written(), 2);
        assert_eq!(collected.lock().unwrap().len(), 2);
    }

    #[test]
    fn probe_counters_ignore_plain_text() {
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let (writer, _collected) = collecting_writer();
        let probes = Arc::new(ProbeCounters::default());
        let mut sink =
            TerminalSink::new_with_probes(Arc::clone(&term), writer, Arc::clone(&probes));
        sink.consume(b"hello world".to_vec());
        assert_eq!(probes.queries_seen(), 0);
        assert_eq!(probes.responses_written(), 0);
    }

    #[test]
    fn default_constructor_keeps_private_probes_observable() {
        // `new` allocates a fresh counter set; `probes()` exposes it
        // so even call sites that didn't pre-share counters can be
        // observed after construction.
        let term = Arc::new(RwLock::new(Terminal::with_scrollback(80, 24, 100)));
        let (writer, _collected) = collecting_writer();
        let mut sink = TerminalSink::new(Arc::clone(&term), writer);
        let probes = sink.probes();
        sink.consume(b"\x1b[6n".to_vec());
        assert_eq!(probes.queries_seen(), 1);
        assert_eq!(probes.responses_written(), 1);
    }
}
