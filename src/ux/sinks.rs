//! Typed sinks — the two seams where the unified engine's output
//! diverges per render mode.
//!
//! * Local-PTY mode: bytes go to `SinglePane::input_tx`, grid pushes
//!   go to `SinglePane::resize_tx` (PTY `TIOCSWINSZ`).
//! * Embedded/daemon tear mode: bytes go to
//!   `MultiplexerControl::send_keys(pane_id, ..)`, grid pushes go to
//!   `MultiplexerControl::pane_resize_absolute(pane_id, ..)`.
//!
//! Both shapes are closures over their channel/control handle; the
//! blanket impls below let the adapters pass plain closures.

use std::sync::Arc;

/// Write already-encoded bytes (keystrokes, mouse reports, paste
/// payloads, VT-query answers) to the pane's PTY.
///
/// Promotion of the `engate_consumer::ResponseWriter` shape — the
/// proven sink the VT query↔answer loop already used. The engine
/// never knows which transport it writes to; "proven once holds in
/// every mode" is structural.
pub trait PtySink: Send + Sync {
    /// Deliver `bytes` to the PTY. Best-effort: a closed channel /
    /// dead pane drops the bytes exactly as the pre-M1 loops did
    /// (`let _ = input_tx.send(..)` / `let _ = control.send_keys(..)`).
    fn write(&self, bytes: &[u8]);
}

impl<F> PtySink for F
where
    F: Fn(&[u8]) + Send + Sync,
{
    fn write(&self, bytes: &[u8]) {
        self(bytes);
    }
}

/// Push a new cell grid (cols × rows) to the pane.
///
/// Sibling of [`PtySink`]: local mode forwards to the PTY winsize
/// channel (SIGWINCH at the child); tear mode forwards to
/// `pane_resize_absolute` (tear's PaneGrid + PTY). The engine
/// resizes its mirror `Terminal` itself before invoking the sink —
/// both halves always move together (a mirror left at a stale size
/// answers CPR/XTWINOPS for a grid the PTY no longer has — the
/// reedline fatal-CPR class).
pub trait ResizeSink: Send + Sync {
    /// Push the new grid. Best-effort, same drop semantics as
    /// [`PtySink::write`].
    fn resize(&self, cols: u16, rows: u16);
}

impl<F> ResizeSink for F
where
    F: Fn(u16, u16) + Send + Sync,
{
    fn resize(&self, cols: u16, rows: u16) {
        self(cols, rows);
    }
}

/// Typed VT-response writeback callback. Invoked with the bytes
/// mado's VT engine generated in response to a DSR / DA / OSC
/// query embedded in the producer's stream. The caller wires this
/// to the tear pane's `send_keys` channel so the bytes flow back
/// into the shell's PTY as the answer it's blocking on.
///
/// Without this, queries like `reedline`'s `\x1b[6n` (DSR-6,
/// cursor-position request) time out — the shell errors out with
/// "The cursor position could not be read within a normal
/// duration" (real incident, frostmourne under mado, 2026-05-21).
///
/// Re-exported from `engate_consumer` for back-compat; the [`PtySink`]
/// trait above is the generalized M1 shape.
pub type ResponseWriter = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn closure_impls_pty_sink_and_boxes() {
        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let sink: Box<dyn PtySink> =
            Box::new(move |bytes: &[u8]| seen2.lock().unwrap().push(bytes.to_vec()));
        sink.write(b"\x12");
        assert_eq!(seen.lock().unwrap().as_slice(), &[b"\x12".to_vec()]);
    }

    #[test]
    fn closure_impls_resize_sink_and_boxes() {
        let seen: Arc<Mutex<Vec<(u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let sink: Box<dyn ResizeSink> =
            Box::new(move |c: u16, r: u16| seen2.lock().unwrap().push((c, r)));
        sink.resize(120, 40);
        assert_eq!(seen.lock().unwrap().as_slice(), &[(120, 40)]);
    }
}
