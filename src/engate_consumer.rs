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

use std::sync::Arc;

use engate_attach::Consumer;
use parking_lot::RwLock;
use tear_types::engate_wrap::PaneSnapshotWrap;

use crate::terminal::Terminal;

/// engate Consumer wrapping an Arc<RwLock<Terminal>>. Clone-cheap;
/// the inner Arc clone is the only cost. Designed for a single
/// engate attach per Terminal — the engate typestate enforces this
/// at compile time.
pub struct TerminalSink {
    inner: Arc<RwLock<Terminal>>,
}

impl TerminalSink {
    #[must_use]
    pub fn new(terminal: Arc<RwLock<Terminal>>) -> Self {
        Self { inner: terminal }
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
        self.inner.write().feed(&bytes);
    }

    fn consume(&mut self, item: Self::Item) {
        // Live items are raw PTY bytes (or, in embedded mode, the
        // bytes the InProcess::subscribe_pane_bytes channel emits).
        self.inner.write().feed(&item);
    }
}
