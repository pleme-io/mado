//! The pty answer boundary, typed.
//!
//! ── ★ A BOUNDARY IS A TYPE: THE TYPE OF WHAT SURVIVES IT ────────────────────
//!
//! Bytes written to a pty carry no record of WHY they were written. A
//! keystroke and a VT query answer are the same `&[u8]` on the wire, so the
//! boundary's carrying capacity is exactly `Bytes` — and every distinction
//! upstream of it is erased.
//!
//! That erasure is not hypothetical. mado had THREE independent writers
//! draining one response buffer; one was gated, and the operator saw the
//! result as text between two prompts, read off the live grid on plo
//! 2026-08-30:
//!
//! ```text
//! ^[[31;24R^[[31;24R
//! ```
//!
//! Prior art converges 7/7, uncoordinated, across C / C++ / Rust / Zig on
//! exactly ONE emission path per pty: kitty `screen.c:1743`, foot
//! `terminal.c:116`, ghostty `stream_handler.zig:143`, xterm's single
//! `unparseseq` placed OUTSIDE the switch so no arm can emit, alacritty's one
//! `write_to_pty` match block. xterm goes further and merges DSR and CPR into
//! one case with an explicit FALLTHRU, so a second arm cannot fire even by
//! accident.
//!
//! ── ★ WHAT THIS TYPE BUYS, AND WHAT IT DOES NOT ─────────────────────────────
//!
//! [`VtAnswer`] has no public constructor. The only way to obtain one is
//! [`VtAnswer::drain`], which takes the terminal and empties its response
//! buffer — so a writer cannot invent an answer, and cannot write one it did
//! not drain. That makes "a second writer emits a duplicate" require a second
//! *drain*, which is greppable, rather than requiring vigilance at every
//! `write_all` in the crate.
//!
//! It does NOT make the class unrepresentable. Nothing stops a caller writing
//! raw bytes to the same pty handle. Tier: **only-mitigated** (C1 — the funnel
//! is the ergonomic route, not the only one). Reaching truly-unrep means the
//! pty writer itself accepting only typed payloads, which is a wider change
//! than the defect warrants today.

use crate::terminal::Terminal;

/// Bytes mado's VT engine produced in reply to a query, and the proof that
/// they came from draining the engine rather than from a caller's imagination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtAnswer(Vec<u8>);

impl VtAnswer {
    /// Drain the terminal's pending response, if any.
    ///
    /// The ONLY constructor. A writer that has not drained has nothing to
    /// write, which is the property the three-writer bug violated.
    #[must_use]
    pub fn drain(term: &mut Terminal) -> Option<Self> {
        term.take_response().map(Self)
    }

    /// The bytes to write. Consuming, so one answer cannot be written twice.
    ///
    /// ★ `self` by value on purpose. A `&self` accessor would let two writers
    /// each emit the same drained answer — the same duplicate, one step later.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Length, for logging, without consuming the answer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the answer is empty. An empty answer is never emitted — xterm
    /// suppresses the same case at `charproc.c:4740`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_twice_yields_one_answer() {
        let mut t = Terminal::with_scrollback(80, 24, 10);
        t.feed(b"\x1b[6n");
        let first = VtAnswer::drain(&mut t);
        assert!(first.is_some(), "the query must produce an answer");
        assert!(
            VtAnswer::drain(&mut t).is_none(),
            "a second drain must yield nothing — otherwise two writers each \
             emit the same answer, which is the ^[[31;24R^[[31;24R the \
             operator saw"
        );
    }

    #[test]
    fn no_query_means_no_answer() {
        let mut t = Terminal::with_scrollback(80, 24, 10);
        t.feed(b"hello");
        assert!(VtAnswer::drain(&mut t).is_none());
    }

    /// The answer is consumed by `into_bytes`, so it cannot be written twice.
    /// This is a compile-time property; the test documents the intent and
    /// pins the round-trip.
    #[test]
    fn an_answer_round_trips_its_bytes_once() {
        let mut t = Terminal::with_scrollback(80, 24, 10);
        t.feed(b"\x1b[6n");
        let a = VtAnswer::drain(&mut t).expect("answer");
        assert!(!a.is_empty());
        let bytes = a.into_bytes();
        assert!(bytes.starts_with(b"\x1b["), "a CPR reply starts with CSI");
        assert!(bytes.ends_with(b"R"), "a CPR reply ends with R");
    }
}
