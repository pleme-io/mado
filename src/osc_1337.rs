//! OSC 1337 — iTerm2 proprietary extensions.
//!
//! iTerm2 ships a proprietary family of OSC sequences under the
//! shared prefix `ESC ] 1337 ; …`. Ghostty, kitty, foot, wezterm,
//! and alacritty all implement at least the two that matter for
//! shell ergonomics:
//!
//! | Argument                    | Semantics                              |
//! |-----------------------------|----------------------------------------|
//! | `SetMark`                   | Plant an explicit user mark at cursor  |
//! | `RequestAttention=<0 / 1>`  | Bounce dock / flash titlebar (on/off)  |
//!
//! Mado previously ignored the whole family. This module adds a
//! typed parameter parser + the two marker / attention lifecycle
//! pieces, with room for the longer-tail keys (`CopyToClipboard`,
//! `File=…`) to land in subsequent ticks without reshaping the
//! dispatch.
//!
//! ## User marks vs. OSC 133 prompt marks
//!
//! `OSC 133 A` is shell-emitted — the prompt itself records its
//! location. `OSC 1337 SetMark` is user-emitted, typed by a
//! `\e]1337;SetMark\e\\` echoed from a script or an inline key
//! action. Both land in the grid at the current cursor row; they
//! differ in provenance and in what jumps them in the UI. Mado
//! tracks them in separate typed histories so a "jump between
//! prompts" binding doesn't cross-contaminate "jump between user
//! marks".
//!
//! ## Parser shape
//!
//! The argument after `1337;` is either a bare identifier
//! (`SetMark`) or `key=value`. Unknown keys decode to
//! [`Osc1337Param::Unknown`] so the dispatcher can trace them
//! without corrupting typed state.

/// Typed OSC 1337 parameter — output of [`parse_osc_1337`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc1337Param {
    /// `SetMark` — user requests a mark at the current cursor row.
    SetMark,
    /// `RequestAttention=<0|1>` — flip the attention request flag.
    /// `true` means "get the user's attention" (bounce dock);
    /// `false` cancels a pending request.
    RequestAttention(bool),
    /// Unknown key — carries the raw text for tracing. The
    /// dispatcher logs and ignores these.
    Unknown(String),
}

/// Parse the argument portion of an OSC 1337 sequence. The caller
/// has already stripped the leading `1337;` and split into param
/// segments by the OSC-level parser; this function takes the first
/// (and usually only) parameter byte slice.
///
/// Returns [`Osc1337Param::Unknown`] for unparseable UTF-8 — we
/// don't error, so a weird sequence can't corrupt the terminal
/// state.
#[must_use]
pub fn parse_osc_1337(arg: &[u8]) -> Osc1337Param {
    let Ok(s) = std::str::from_utf8(arg) else {
        return Osc1337Param::Unknown(String::from_utf8_lossy(arg).into_owned());
    };
    if s == "SetMark" {
        return Osc1337Param::SetMark;
    }
    if let Some(value) = s.strip_prefix("RequestAttention=") {
        return Osc1337Param::RequestAttention(match value {
            "0" | "false" | "no" => false,
            _ => true,
        });
    }
    Osc1337Param::Unknown(s.to_string())
}

/// One user-emitted mark from `OSC 1337 SetMark`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserMark {
    /// Grid-internal row index — same coord space OSC 133
    /// [`PromptHistory`](crate::prompt_mark::PromptHistory) uses.
    pub grid_row: usize,
}

/// Bounded FIFO of user marks. Shape mirrors `PromptHistory` so
/// the scrollback-eviction bookkeeping stays consistent.
#[derive(Debug, Clone)]
pub struct UserMarkHistory {
    marks: std::collections::VecDeque<UserMark>,
    cap: usize,
}

impl UserMarkHistory {
    /// Fresh history with a hard cap on the mark count.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            marks: std::collections::VecDeque::with_capacity(cap.min(1024)),
            cap: cap.max(1),
        }
    }

    /// Record a new mark at `grid_row`. Back-to-back duplicates are
    /// dropped so a script that echoes `SetMark` twice doesn't
    /// double-pin the same row.
    pub fn record(&mut self, grid_row: usize) {
        if let Some(last) = self.marks.back() {
            if last.grid_row == grid_row {
                return;
            }
        }
        self.marks.push_back(UserMark { grid_row });
        while self.marks.len() > self.cap {
            self.marks.pop_front();
        }
    }

    /// Shift every mark when the grid evicts `n` rows from the front.
    /// Marks that would go negative are dropped. Same contract
    /// [`PromptHistory::shift_on_evict`](crate::prompt_mark::PromptHistory::shift_on_evict)
    /// already uses.
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

    /// Count of tracked marks.
    #[must_use]
    #[allow(dead_code)] // Surface for pending MCP prompt-history-style tooling.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// True when empty.
    #[must_use]
    #[allow(dead_code)] // Idiomatic alongside `len()`; consumed by tests.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// Drop every mark — used by terminal reset (RIS).
    #[allow(dead_code)] // Pairs with PromptHistory::clear for reset paths.
    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// Iterator over marks in insertion order.
    pub fn iter(
        &self,
    ) -> std::collections::vec_deque::Iter<'_, UserMark> {
        self.marks.iter()
    }
}

impl Default for UserMarkHistory {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_recognizes_set_mark() {
        assert_eq!(parse_osc_1337(b"SetMark"), Osc1337Param::SetMark);
    }

    #[test]
    fn parser_decodes_request_attention_boolean() {
        // Ghostty / iTerm2 both accept `0`/`false`/`no` as off; any
        // other value is treated as "request attention". Pin the
        // off-vocab so adding a new "falsy" literal is a conscious
        // edit.
        assert_eq!(
            parse_osc_1337(b"RequestAttention=0"),
            Osc1337Param::RequestAttention(false),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=false"),
            Osc1337Param::RequestAttention(false),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=no"),
            Osc1337Param::RequestAttention(false),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=1"),
            Osc1337Param::RequestAttention(true),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=true"),
            Osc1337Param::RequestAttention(true),
        );
    }

    #[test]
    fn parser_returns_unknown_for_unrecognized_keys() {
        match parse_osc_1337(b"CopyToClipboard") {
            Osc1337Param::Unknown(s) => assert_eq!(s, "CopyToClipboard"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        // key=value shape with unknown key.
        match parse_osc_1337(b"File=abc") {
            Osc1337Param::Unknown(s) => assert_eq!(s, "File=abc"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parser_handles_invalid_utf8_gracefully() {
        // Malformed UTF-8 — must not panic or error. Falls through
        // to Unknown with a lossy rendering.
        let bad = &[0xff, 0xfe, b'a'];
        match parse_osc_1337(bad) {
            Osc1337Param::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn mark_history_dedupes_identical_back_to_back_marks() {
        let mut h = UserMarkHistory::default();
        h.record(5);
        h.record(5);
        h.record(5);
        assert_eq!(h.len(), 1);
        // Different row → new entry.
        h.record(6);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn mark_history_enforces_capacity() {
        let mut h = UserMarkHistory::with_capacity(3);
        for row in 0..5 {
            h.record(row);
        }
        assert_eq!(h.len(), 3);
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        assert_eq!(rows, vec![2, 3, 4]);
    }

    #[test]
    fn mark_history_shift_on_evict_drops_underflow() {
        let mut h = UserMarkHistory::default();
        h.record(2);
        h.record(5);
        h.record(10);
        h.shift_on_evict(3);
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        // Row 2 dropped (would go negative); 5-3=2, 10-3=7.
        assert_eq!(rows, vec![2, 7]);

        // Huge eviction wipes everything.
        h.shift_on_evict(1000);
        assert!(h.is_empty());
    }

    #[test]
    fn mark_history_shift_on_evict_zero_is_noop() {
        let mut h = UserMarkHistory::default();
        h.record(5);
        h.shift_on_evict(0);
        assert_eq!(h.len(), 1);
    }
}
