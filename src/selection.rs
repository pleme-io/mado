//! Text selection state machine — CONTENT-anchored (M2 bridge).
//!
//! Selection endpoints are [`SelectionAnchor`]s (logical line id +
//! run + cell offset), captured at gesture time via
//! [`crate::terminal::Terminal::selection_anchor_at`] and resolved at
//! use time (render highlight, copy extraction) via
//! `resolve_selection_span` / `extract_selection_text`. Streaming
//! output sliding rows into scrollback and rewrap-on-resize both
//! leave the anchors pointing at the SAME content; viewport `(row,
//! col)` pairs — the pre-anchor representation — went stale on every
//! grid mutation under an active selection.
//!
//! Tier honesty: a dangling anchor (content evicted, RIS rebuild,
//! screen-buffer switch) is parse-time-rejected — every read path
//! goes through resolution, which returns `None` rather than stale
//! coordinates — and the engine's per-tick reconciler collapses the
//! state to `None`. It is not truly unrepresentable: the anchors can
//! dangle between eviction and the next resolution; there is simply
//! no read path that turns them into coordinates.

use crate::prompt_mark::CommandZone;
use crate::terminal::{Cell, SelectionAnchor};

/// A cell position in the VIEWPORT (row 0 = top of the current
/// view). The mouse-gesture coordinate space; anchors are captured
/// from these at gesture time and never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPos {
    pub row: usize,
    pub col: usize,
}

/// Selection state. `Selecting` = a live char-drag whose release
/// decides commit-vs-clear; `Selected` = a committed span (word/line
/// snap, select-all, shift-extend, finished drag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    None,
    Selecting {
        start: SelectionAnchor,
        end: SelectionAnchor,
    },
    Selected {
        start: SelectionAnchor,
        end: SelectionAnchor,
    },
}

/// Monotonically-bumped counter over `Selection` mutations.
///
/// The selection overlay's answer to `Terminal::seqno()`. `needs_frame`
/// compares the live epoch against `render`'s memoized last-painted
/// epoch to tell "the highlight looks like what was last drawn" from
/// "the selection state moved and owes a repaint" — without this, a
/// drag or a clear mutated the state but incremented nothing on the
/// wake predicate's checklist, so madori skipped the frame and the
/// highlight never reached the display (measured 2026-08-26, mado
/// 0.1.142 on Linux + `cursor.blink = false`: drag selected, state
/// updated, no frame painted).
///
/// Bumped on every state-mutating method — start / update /
/// set_span / finish / clear — even on a no-op transition (a drag
/// re-arriving at the same anchor). The wake predicate is deliberately
/// conservative: a redundant frame costs microseconds, a wrongly
/// skipped one is a display that stops updating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectionEpoch(u64);

impl SelectionEpoch {
    fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

/// Text selection manager. Holds anchors in CAPTURE order — reading
/// order (start ≤ end) is only knowable after resolution, so
/// normalization lives in `Terminal::resolve_selection_span`.
pub struct Selection {
    state: State,
    epoch: SelectionEpoch,
}

impl Selection {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::None,
            epoch: SelectionEpoch::default(),
        }
    }

    /// Begin a char-drag gesture at the given anchor.
    pub fn start(&mut self, pos: SelectionAnchor) {
        self.state = State::Selecting {
            start: pos,
            end: pos,
        };
        self.epoch.bump();
    }

    /// Move the gesture's end anchor. Acts in BOTH live states —
    /// drag gating belongs to the engine's pointer machine
    /// (`ux::modes::Pointer`), not to this holder (a shift-extended
    /// selection is `Selected` yet still draggable).
    pub fn update(&mut self, pos: SelectionAnchor) {
        match self.state {
            State::Selecting { start, .. } => {
                self.state = State::Selecting { start, end: pos };
                self.epoch.bump();
            }
            State::Selected { start, .. } => {
                self.state = State::Selected { start, end: pos };
                self.epoch.bump();
            }
            State::None => {}
        }
    }

    /// Replace the selection with a committed span (word/line snap,
    /// select-all, shift-click extend, word/line drag union).
    pub fn set_span(&mut self, start: SelectionAnchor, end: SelectionAnchor) {
        self.state = State::Selected { start, end };
        self.epoch.bump();
    }

    /// Finalize a char-drag (mouse released): a zero-length gesture
    /// was a click, not a selection — clear it.
    pub fn finish(&mut self) {
        if let State::Selecting { start, end } = self.state {
            if start == end {
                self.state = State::None;
            } else {
                self.state = State::Selected { start, end };
            }
            self.epoch.bump();
        }
    }

    /// Clear the selection.
    pub fn clear(&mut self) {
        self.state = State::None;
        self.epoch.bump();
    }

    /// The current mutation epoch. Read by `render::TerminalRenderer::needs_frame`
    /// to wake the loop on any selection state change (start / update / commit
    /// / clear). See [`SelectionEpoch`].
    #[must_use]
    pub fn epoch(&self) -> SelectionEpoch {
        self.epoch
    }

    /// Whether a selection is currently active (selecting or selected).
    /// Production reads flow through [`Self::anchors`] (resolution is
    /// the only honest liveness check); this stays as the test /
    /// external-consumer surface.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::None)
    }

    /// The endpoint anchors in CAPTURE order (un-normalized — feed
    /// them to `Terminal::resolve_selection_span` /
    /// `extract_selection_text`, which normalize after resolution).
    #[must_use]
    pub fn anchors(&self) -> Option<(SelectionAnchor, SelectionAnchor)> {
        match self.state {
            State::None => None,
            State::Selecting { start, end } | State::Selected { start, end } => Some((start, end)),
        }
    }
}

impl Default for Selection {
    fn default() -> Self {
        Self::new()
    }
}

/// The INCLUSIVE absolute grid-row span one command's output occupies
/// — the shape [`command_output_rows`] hands to the anchor capture.
///
/// Absolute (scrollback origin 0), the same coordinate space
/// [`crate::prompt_mark::PromptMark::grid_row`] lives in — NOT the
/// viewport space [`CellPos`] uses. The two are only a subtraction
/// apart, but a zone that scrolled off the top of the view has no
/// viewport row at all, which is exactly why the conversion happens at
/// the anchor-capture seam and not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRowSpan {
    pub first: usize,
    pub last: usize,
}

/// The rows a [`CommandZone`]'s output occupies, or `None` when the
/// command produced no output at all.
///
/// The zone is half-open in the marks' own terms: `start` is the row
/// of the OSC 133 `C` mark (the first row of output) and `end` is the
/// row of the `A` mark that opens the NEXT prompt — one past the last
/// output row. Two edge cases are decided here rather than at the call
/// site, because both are indistinguishable from a healthy zone once
/// they have been turned into coordinates:
///
/// * **No output** (`cd /tmp`, a command whose next prompt draws on the
///   very row the output would have started) collapses to `end ==
///   start`, and answering `Some(first == last == start)` would select
///   the next prompt line and copy it as if it were output. `None`.
/// * **Still running** — the shell has not drawn the next prompt, so
///   `zone.end` is `None` and there is no recorded bound. The caller
///   supplies `live_row`, the absolute row the cursor sits on right
///   now, as the exclusive end: everything printed so far, nothing
///   beyond it. (A zone that has not printed a full row yet again
///   collapses to empty and yields `None`.)
///
/// A defensive `end < start` (marks recorded out of order) is treated
/// as empty rather than clamped — a backwards span is not a selection.
#[must_use]
pub fn command_output_rows(zone: &CommandZone, live_row: usize) -> Option<OutputRowSpan> {
    let end_exclusive = zone.end.unwrap_or(live_row);
    if end_exclusive <= zone.start {
        return None;
    }
    Some(OutputRowSpan {
        first: zone.start,
        last: end_exclusive - 1,
    })
}

/// Word bounds on a single viewport row: the inclusive `(start_col,
/// end_col)` of the word containing `pos`, or `(col, col)` when the
/// cell under `pos` is itself a boundary character.
///
/// `boundary_chars` defines characters that act as word boundaries
/// (matching Ghostty's `selection-word-chars`). If empty, falls back
/// to the default: any character that is not alphanumeric or
/// underscore.
///
/// Pure snap rule over a rows snapshot — the engine captures anchors
/// for the returned columns; this function never touches selection
/// state.
#[must_use]
pub fn word_bounds_in_row(
    pos: CellPos,
    rows: &[Vec<Cell>],
    cols: usize,
    boundary_chars: &str,
) -> (usize, usize) {
    let col = pos.col.min(cols.saturating_sub(1));
    let Some(row) = rows.get(pos.row) else {
        return (col, col);
    };

    let is_boundary = |c: char| -> bool {
        if boundary_chars.is_empty() {
            !c.is_alphanumeric() && c != '_'
        } else {
            boundary_chars.contains(c)
        }
    };
    let is_word = |c: char| !is_boundary(c);

    let ch = if col < row.len() { row[col].ch } else { ' ' };
    if !is_word(ch) {
        return (col, col);
    }

    let mut start = col;
    while start > 0 && start - 1 < row.len() && is_word(row[start - 1].ch) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols && end + 1 < row.len() && is_word(row[end + 1].ch) {
        end += 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    fn term_with(text: &[u8]) -> Terminal {
        let mut t = Terminal::new(40, 5);
        t.feed(text);
        t
    }

    fn anchor(t: &Terminal, row: usize, col: usize) -> SelectionAnchor {
        t.selection_anchor_at(row, col)
            .expect("viewport cell must anchor")
    }

    #[test]
    fn empty_selection() {
        let sel = Selection::new();
        assert!(!sel.is_active());
        assert!(sel.anchors().is_none());
    }

    #[test]
    fn single_cell_click_clears() {
        let t = term_with(b"hello world");
        let mut sel = Selection::new();
        sel.start(anchor(&t, 0, 3));
        sel.finish();
        assert!(!sel.is_active());
    }

    #[test]
    fn drag_commits_on_finish() {
        let t = term_with(b"hello world");
        let mut sel = Selection::new();
        sel.start(anchor(&t, 0, 2));
        sel.update(anchor(&t, 0, 5));
        assert!(sel.is_active(), "live drag is active before finish");
        sel.finish();
        assert!(sel.is_active());
        let (a, b) = sel.anchors().expect("committed span");
        assert_eq!(
            t.resolve_selection_anchor(a).unwrap(),
            (0, 2),
            "start anchor resolves to the capture cell"
        );
        assert_eq!(t.resolve_selection_anchor(b).unwrap(), (0, 5));
    }

    #[test]
    fn reverse_drag_normalizes_at_resolution() {
        let t = term_with(b"one\r\ntwo\r\nthree");
        let mut sel = Selection::new();
        sel.start(anchor(&t, 2, 4));
        sel.update(anchor(&t, 0, 1));
        sel.finish();
        let (a, b) = sel.anchors().unwrap();
        let (start, end) = t.resolve_selection_span(a, b).unwrap();
        assert_eq!(start, (0, 1), "span resolves in reading order");
        assert_eq!(end, (2, 4));
    }

    #[test]
    fn set_span_is_committed_and_finish_is_a_noop_on_it() {
        let t = term_with(b"hello world");
        let mut sel = Selection::new();
        sel.set_span(anchor(&t, 0, 0), anchor(&t, 0, 4));
        assert!(sel.is_active());
        sel.finish();
        assert!(sel.is_active(), "finish must not clear a committed span");
    }

    #[test]
    fn update_moves_end_while_selected() {
        // The shift-extend contract: a committed span keeps
        // following the pointer while the engine's drag FSM routes
        // motion here.
        let t = term_with(b"hello world wide");
        let mut sel = Selection::new();
        sel.set_span(anchor(&t, 0, 0), anchor(&t, 0, 4));
        sel.update(anchor(&t, 0, 10));
        let (_, b) = sel.anchors().unwrap();
        assert_eq!(t.resolve_selection_anchor(b).unwrap(), (0, 10));
    }

    #[test]
    fn clear_selection() {
        let t = term_with(b"hello");
        let mut sel = Selection::new();
        sel.start(anchor(&t, 0, 0));
        sel.update(anchor(&t, 0, 4));
        sel.finish();
        assert!(sel.is_active());
        sel.clear();
        assert!(!sel.is_active());
    }

    /// Word-bounds snap matrix — every variant exercised, failures
    /// aggregated before the assert.
    #[test]
    fn word_bounds_matrix() {
        use std::fmt::Write as _;
        fn make_row(text: &str) -> Vec<Cell> {
            text.chars()
                .map(|ch| Cell {
                    ch,
                    ..Cell::default()
                })
                .collect()
        }
        struct Row {
            name: &'static str,
            text: &'static str,
            col: usize,
            boundary: &'static str,
            want: (usize, usize),
        }
        let rows = [
            Row {
                name: "mid-word with underscore",
                text: "hello world_test foo",
                col: 7,
                boundary: "",
                want: (6, 15),
            },
            Row {
                name: "boundary char snaps to itself",
                text: "hello world",
                col: 5,
                boundary: "",
                want: (5, 5),
            },
            Row {
                name: "word at row start",
                text: "hello world",
                col: 0,
                boundary: "",
                want: (0, 4),
            },
            Row {
                name: "word at row end",
                text: "hello world",
                col: 10,
                boundary: "",
                want: (6, 10),
            },
            Row {
                name: "default boundary splits on colon",
                text: "hello:world test",
                col: 0,
                boundary: "",
                want: (0, 4),
            },
            Row {
                name: "custom boundary keeps colon in word",
                text: "hello:world test",
                col: 0,
                boundary: " \t",
                want: (0, 10),
            },
            Row {
                name: "custom boundary splits on semicolon",
                text: "foo;bar baz",
                col: 0,
                boundary: ";, \t",
                want: (0, 2),
            },
        ];
        let mut failures = Vec::new();
        for r in &rows {
            let grid = vec![make_row(r.text)];
            let cols = r.text.len();
            let got = word_bounds_in_row(CellPos { row: 0, col: r.col }, &grid, cols, r.boundary);
            if got != r.want {
                let mut msg = String::new();
                let _ = write!(msg, "{}: want {:?}, got {got:?}", r.name, r.want);
                failures.push(msg);
            }
        }
        assert!(
            failures.is_empty(),
            "{} word-bounds variants failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn word_bounds_out_of_range_row_snaps_to_cell() {
        let got = word_bounds_in_row(CellPos { row: 5, col: 3 }, &[], 10, "");
        assert_eq!(got, (3, 3));
    }

    fn zone(start: usize, end: Option<usize>) -> CommandZone {
        CommandZone {
            start,
            end,
            exit_status: None,
        }
    }

    #[test]
    fn command_output_rows_spans_c_mark_to_the_row_before_the_next_prompt() {
        // C at 2, next prompt A at 8 ⇒ output is rows 2..=7. The `A`
        // row itself is the NEXT prompt and must never be copied.
        let got = command_output_rows(&zone(2, Some(8)), 99).expect("a six-row zone has output");
        assert_eq!(got, OutputRowSpan { first: 2, last: 7 });
    }

    #[test]
    fn command_output_rows_single_row_of_output() {
        let got = command_output_rows(&zone(4, Some(5)), 99).expect("one row is still output");
        assert_eq!(got, OutputRowSpan { first: 4, last: 4 });
    }

    #[test]
    fn command_output_rows_rejects_the_no_output_command() {
        // `cd /tmp`: the next prompt draws on the very row output
        // would have started. Answering `Some(4..=4)` here would copy
        // the next PROMPT line and present it as command output.
        assert_eq!(command_output_rows(&zone(4, Some(4)), 99), None);
    }

    #[test]
    fn command_output_rows_uses_the_live_cursor_row_while_a_command_runs() {
        // Still running (no closing `A` yet) — bound by where the
        // cursor is right now, so a mid-run copy gets exactly what has
        // been printed and nothing past it.
        let got = command_output_rows(&zone(3, None), 7).expect("partial output is still output");
        assert_eq!(got, OutputRowSpan { first: 3, last: 6 });
        // …and a run that has not yet completed a row is empty, not a
        // one-row span over the cursor's own (blank) line.
        assert_eq!(command_output_rows(&zone(3, None), 3), None);
    }

    #[test]
    fn command_output_rows_treats_a_backwards_span_as_empty() {
        // Defensive: marks recorded out of order must not clamp into a
        // plausible-looking selection.
        assert_eq!(command_output_rows(&zone(9, Some(4)), 99), None);
        assert_eq!(command_output_rows(&zone(9, None), 4), None);
    }
}
