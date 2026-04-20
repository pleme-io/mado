//! OSC 133 prompt marker history — ghostty-style prompt jump state.
//!
//! Shells instrument their prompts with OSC 133 `A` (prompt start),
//! `B` (command start), `C` (command output), `D` (command end). Mado
//! already dispatches these sequences; this module turns the
//! side-effect-free stream of markers into a bounded, queryable
//! history so the user can jump between prompts with a keybind —
//! the same UX ghostty, kitty, iterm2, and wezterm ship.
//!
//! Every mark stores a **grid-internal row index** — the index into
//! the terminal's `VecDeque<Vec<Cell>>`. When the grid evicts rows
//! from the front (scrollback cap reached), callers invoke
//! [`PromptHistory::shift_on_evict`] with the eviction count and
//! marks whose rows would go negative are dropped.
//!
//! No other terminal emulator in the category exposes prompt-mark
//! state as a typed, testable record — pleme-io's Rust-owned
//! invariants make the jump API trivially verifiable at the unit
//! test layer.
//!
//! ```text
//! shell OSC 133 A @ row 12  ─┐
//! shell OSC 133 A @ row 18  ─┤   PromptHistory (cap=1000)
//! shell OSC 133 A @ row 25  ─┘── ▶ jump_prev(from=25) = 18
//!                                  jump_prev(from=18) = 12
//!                                  jump_next(from=12) = 18
//! ```
//!
//! See `src/terminal.rs::handle_osc_133_shell_integration` for the
//! single call site.

use std::collections::VecDeque;

/// Semantic prompt kind — one letter per OSC 133 ps1/ps2 lifecycle
/// state. Matches the iTerm2 / ghostty / Kitty vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptKind {
    /// `A` — prompt start (PS1 about to be drawn).
    Start,
    /// `B` — command start (prompt drawn, user began typing).
    CommandStart,
    /// `C` — command output begins (user pressed Enter).
    CommandOutput,
    /// `D` — command end (exit status available as the optional
    /// trailing parameter, ignored here for now).
    CommandEnd,
}

impl PromptKind {
    /// Parse the OSC 133 second parameter into a [`PromptKind`].
    /// Returns `None` for anything outside A/B/C/D so the OSC
    /// dispatcher's match arm stays exhaustive.
    #[must_use]
    pub fn from_osc_param(param: &[u8]) -> Option<Self> {
        match param {
            b"A" => Some(Self::Start),
            b"B" => Some(Self::CommandStart),
            b"C" => Some(Self::CommandOutput),
            b"D" => Some(Self::CommandEnd),
            _ => None,
        }
    }

    /// Inverse of [`Self::from_osc_param`] — useful for tests and
    /// MCP payloads that echo prompt state back to the caller.
    #[must_use]
    pub fn as_osc_byte(self) -> u8 {
        match self {
            Self::Start => b'A',
            Self::CommandStart => b'B',
            Self::CommandOutput => b'C',
            Self::CommandEnd => b'D',
        }
    }
}

/// One OSC 133 prompt marker.
///
/// `grid_row` is measured in the terminal's flat-grid coordinate
/// space — the same index used by `Grid::rows` in `terminal.rs`.
/// This keeps the row stable under visible-area scrolling (scrolling
/// the viewport up/down changes `scroll_offset` but does NOT renumber
/// rows). Only scrollback eviction (`pop_front`) shifts rows — and
/// that is handled by [`PromptHistory::shift_on_evict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptMark {
    pub grid_row: usize,
    pub kind: PromptKind,
}

/// Bounded FIFO of OSC 133 prompt markers. The cap defaults to the
/// grid's scrollback size so we can't outlive the rows we reference.
#[derive(Debug, Clone)]
pub struct PromptHistory {
    marks: VecDeque<PromptMark>,
    cap: usize,
}

impl PromptHistory {
    /// Fresh history with a hard cap on the mark count.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            marks: VecDeque::with_capacity(cap.min(1024)),
            cap: cap.max(1),
        }
    }

    /// Record a new mark. If the most recent mark is already the
    /// same `(grid_row, kind)`, this is a no-op — many shells
    /// re-emit OSC 133 A on prompt redraw and we don't want dupes.
    pub fn record(&mut self, grid_row: usize, kind: PromptKind) {
        if let Some(last) = self.marks.back() {
            if last.grid_row == grid_row && last.kind == kind {
                return;
            }
        }
        self.marks.push_back(PromptMark { grid_row, kind });
        while self.marks.len() > self.cap {
            self.marks.pop_front();
        }
    }

    /// Apply an eviction — called by the grid whenever rows roll
    /// off the front of the scrollback `VecDeque`. Marks whose row
    /// would go negative are dropped from the front; surviving
    /// marks decrement by `n`.
    pub fn shift_on_evict(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        while let Some(first) = self.marks.front() {
            if first.grid_row < n {
                self.marks.pop_front();
            } else {
                break;
            }
        }
        for m in self.marks.iter_mut() {
            m.grid_row -= n;
        }
    }

    /// Most recent `Start` mark strictly above `from_row`. Ignores
    /// B/C/D kinds — jump-to-prompt is spec'd in terms of the user-
    /// visible PS1 start, not the internal substates.
    #[must_use]
    pub fn prev_prompt(&self, from_row: usize) -> Option<usize> {
        self.marks
            .iter()
            .rev()
            .find(|m| m.kind == PromptKind::Start && m.grid_row < from_row)
            .map(|m| m.grid_row)
    }

    /// First `Start` mark strictly below `from_row`.
    #[must_use]
    pub fn next_prompt(&self, from_row: usize) -> Option<usize> {
        self.marks
            .iter()
            .find(|m| m.kind == PromptKind::Start && m.grid_row > from_row)
            .map(|m| m.grid_row)
    }

    /// How many marks are currently tracked (all kinds).
    #[must_use]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// True when no marks have been captured yet.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// Drop every mark — used by terminal reset (RIS).
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// Iterator over every tracked mark in insertion order.
    /// Returns a [`DoubleEndedIterator`] so callers can walk in
    /// reverse (prompt-jump backwards) without cloning.
    pub fn iter(&self) -> std::collections::vec_deque::Iter<'_, PromptMark> {
        self.marks.iter()
    }
}

impl Default for PromptHistory {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_kind_round_trips_through_osc_bytes() {
        for kind in [
            PromptKind::Start,
            PromptKind::CommandStart,
            PromptKind::CommandOutput,
            PromptKind::CommandEnd,
        ] {
            let byte = kind.as_osc_byte();
            assert_eq!(
                PromptKind::from_osc_param(&[byte]),
                Some(kind),
                "{kind:?} should round-trip",
            );
        }
        assert_eq!(PromptKind::from_osc_param(b"Z"), None);
        assert_eq!(PromptKind::from_osc_param(b""), None);
    }

    #[test]
    fn record_dedupes_identical_back_to_back_marks() {
        // Shells commonly re-emit OSC 133 A when the prompt repaints
        // (e.g., resize). The history must not treat that as two
        // separate prompts.
        let mut h = PromptHistory::default();
        h.record(5, PromptKind::Start);
        h.record(5, PromptKind::Start);
        h.record(5, PromptKind::Start);
        assert_eq!(h.len(), 1);

        // Same row but different kind is kept — that's prompt → command.
        h.record(5, PromptKind::CommandStart);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn record_enforces_capacity_by_evicting_front() {
        let mut h = PromptHistory::with_capacity(3);
        for row in 0..5 {
            h.record(row, PromptKind::Start);
        }
        assert_eq!(h.len(), 3);
        // Oldest two dropped — we keep 2, 3, 4.
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        assert_eq!(rows, vec![2, 3, 4]);
    }

    #[test]
    fn prev_prompt_walks_backwards_skipping_non_start_kinds() {
        let mut h = PromptHistory::default();
        h.record(10, PromptKind::Start);
        h.record(11, PromptKind::CommandStart);
        h.record(20, PromptKind::Start);
        h.record(21, PromptKind::CommandOutput);
        h.record(30, PromptKind::Start);

        // From just after the last prompt, prev is the 30 mark's
        // predecessor — 20. Command start/output kinds are skipped.
        assert_eq!(h.prev_prompt(30), Some(20));
        assert_eq!(h.prev_prompt(20), Some(10));
        assert_eq!(h.prev_prompt(10), None);
        // A cursor between 20 and 30 should also resolve to 20.
        assert_eq!(h.prev_prompt(25), Some(20));
    }

    #[test]
    fn next_prompt_walks_forwards_skipping_non_start_kinds() {
        let mut h = PromptHistory::default();
        h.record(10, PromptKind::Start);
        h.record(11, PromptKind::CommandOutput);
        h.record(20, PromptKind::Start);
        h.record(30, PromptKind::Start);

        assert_eq!(h.next_prompt(0), Some(10));
        assert_eq!(h.next_prompt(10), Some(20));
        assert_eq!(h.next_prompt(20), Some(30));
        assert_eq!(h.next_prompt(30), None);
        // Cursor between 10 and 20 jumps to 20.
        assert_eq!(h.next_prompt(15), Some(20));
    }

    #[test]
    fn shift_on_evict_drops_underflow_and_decrements_survivors() {
        let mut h = PromptHistory::default();
        h.record(2, PromptKind::Start);
        h.record(5, PromptKind::Start);
        h.record(10, PromptKind::Start);

        // Evict 3 rows — the mark at row 2 disappears, the others
        // decrement by 3.
        h.shift_on_evict(3);
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        assert_eq!(rows, vec![2, 7]);

        // Evicting zero is a no-op.
        h.shift_on_evict(0);
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        assert_eq!(rows, vec![2, 7]);

        // Evicting more than any remaining row clears everything.
        h.shift_on_evict(1000);
        assert!(h.is_empty());
    }

    #[test]
    fn clear_drops_every_mark() {
        let mut h = PromptHistory::default();
        h.record(3, PromptKind::Start);
        h.record(6, PromptKind::Start);
        assert_eq!(h.len(), 2);
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.prev_prompt(100), None);
        assert_eq!(h.next_prompt(0), None);
    }

    #[test]
    fn prev_next_return_none_when_no_start_marks() {
        // Only B/C/D kinds — no Start — so navigation yields nothing.
        let mut h = PromptHistory::default();
        h.record(5, PromptKind::CommandStart);
        h.record(6, PromptKind::CommandOutput);
        h.record(7, PromptKind::CommandEnd);
        assert_eq!(h.prev_prompt(100), None);
        assert_eq!(h.next_prompt(0), None);
    }
}
