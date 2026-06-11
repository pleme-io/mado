//! [`TerminalSideEffects`] — the typed drain payload for everything
//! the VT engine accumulates as a side effect of feeding bytes.
//!
//! **M1 defines the TYPE only** (hosted here so it lives in the one
//! shared module both render modes consume). The implementing seam —
//! `Terminal::drain_side_effects() -> TerminalSideEffects` — wires in
//! M4 (docs/REMEDIATION-PLAN.md §M4), at which point both event
//! loops delete their inlined per-frame polling (title diff,
//! `take_bell`, `take_clipboard`, pending notifications) and become
//! ~15-line dispatchers over this struct. Until then the existing
//! per-loop polling stands.

/// One frame's worth of terminal side effects, drained atomically.
#[allow(dead_code)] // type lands in M1; the Terminal::drain_side_effects() impl + consumers wire in M4
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalSideEffects {
    /// OSC 0/2 window title, when it changed since the last drain.
    pub title: Option<String>,
    /// BEL was received (visual bell + notification urgency feed).
    pub bell: bool,
    /// OSC 52 clipboard write payload to sync to the system clipboard.
    pub clipboard: Option<String>,
    /// Pending notifications (OSC 9 / 777 / 99). Plain strings in M1;
    /// M4 replaces with the typed `PendingNotification
    /// {title, body, urgency, group}` shape when the parse lands.
    pub notifications: Vec<String>,
    /// OSC 9;4 progress (0.0..=1.0). Progress never fires a
    /// notification — it is its own typed lane.
    pub progress: Option<f32>,
    /// OSC 7 working directory, when it changed since the last drain.
    pub cwd: Option<String>,
    /// OSC 1337 RequestAttention — dock bounce / critical urgency.
    pub attention: bool,
}
