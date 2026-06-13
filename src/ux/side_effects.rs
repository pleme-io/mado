//! [`TerminalSideEffects`] — the typed drain payload for everything
//! the VT engine accumulates as a side effect of feeding bytes —
//! plus the typed vocabulary the Terminal queues those effects in
//! ([`PendingNotification`], [`Urgency`], [`ProgressState`]).
//!
//! **M1 defined the TYPE only** (hosted here so it lives in the one
//! shared module both render modes consume). M4 stage 1 lands the
//! typed notification/progress vocabulary; the implementing seam —
//! `Terminal::drain_side_effects() -> TerminalSideEffects` — wires in
//! M4 stage 2 (docs/REMEDIATION-PLAN.md §M4), at which point both
//! event loops delete their inlined per-frame polling (title diff,
//! `take_bell`, `take_clipboard`, pending notifications) and become
//! ~15-line dispatchers over this struct. Until then the existing
//! per-loop polling stands.

/// Desktop-notification urgency — the OSC 99 `u=` axis (kitty
/// protocol: `0`=low, `1`=normal, `2`=critical). OSC 9 and OSC 777
/// carry no urgency and default to [`Urgency::Normal`]; BEL routes
/// `Normal`; OSC 1337 `RequestAttention` routes `Critical`.
///
/// Field-for-field mirror of `tsuuchi::Urgency` — M4 stage 2 (which
/// adds the tsuuchi dependency) replaces this definition with a
/// re-export so the parse layer and the dispatch layer share ONE
/// type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// Informational, non-intrusive (OSC 99 `u=0`).
    Low,
    /// Standard notification (OSC 99 `u=1`; the OSC 9 / 777 default).
    #[default]
    Normal,
    /// Requires immediate attention (OSC 99 `u=2`).
    Critical,
}

/// One typed desktop notification queued by the terminal (OSC 9 /
/// OSC 777;notify / OSC 99). The queue carries ONLY notifications —
/// `ConEmu` progress (OSC 9;4) lives in its own typed lane
/// ([`ProgressState`]) and cannot be represented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotification {
    /// Title line. OSC 9 carries body-only (`None`); OSC 777 carries
    /// an explicit title param; OSC 99 fills this from `p=title`
    /// payload parts.
    pub title: Option<String>,
    /// Body text. OSC 9's whole payload; OSC 777's body param;
    /// OSC 99's `p=body` payload parts.
    pub body: String,
    /// Urgency — only OSC 99 (`u=`) can express non-Normal.
    pub urgency: Urgency,
    /// Collapse/replace group. Only OSC 99's `i=<id>` populates this
    /// (re-notifying with the same id is replace semantics); OSC 9 /
    /// OSC 777 have no group vocabulary.
    pub group: Option<String>,
}

/// `ConEmu` OSC 9;4 progress state (`ESC ] 9 ; 4 ; st ; pr ST`).
/// `st`: 0=remove, 1=set value, 2=error, 3=indeterminate, 4=paused.
/// `pr` is an integer percentage 0..=100 (clamped at parse).
///
/// Progress is NOT a notification: it lives in this separate typed
/// lane, so a progress update firing a desktop notification is
/// unrepresentable — there is no constructor from one to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// `st=0` — remove the progress indicator.
    Remove,
    /// `st=1` — determinate progress at `pct` percent.
    Set {
        /// 0..=100, clamped at the parse boundary.
        pct: u8,
    },
    /// `st=2` — error state; `pct` retains the last value when sent.
    Error {
        /// Optional 0..=100 percentage accompanying the error.
        pct: Option<u8>,
    },
    /// `st=3` — indeterminate (busy spinner, no percentage).
    Indeterminate,
    /// `st=4` — paused; `pct` retains the last value when sent.
    Paused {
        /// Optional 0..=100 percentage at which progress paused.
        pct: Option<u8>,
    },
}

/// One frame's worth of terminal side effects, drained atomically.
#[allow(dead_code)] // M4 stage 1 types the payload; Terminal::drain_side_effects() + consumers wire in stage 2
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalSideEffects {
    /// OSC 0/2 window title, when it changed since the last drain.
    pub title: Option<String>,
    /// BEL was received (visual bell + notification urgency feed).
    pub bell: bool,
    /// OSC 52 clipboard write payload to sync to the system clipboard.
    pub clipboard: Option<String>,
    /// Pending notifications (OSC 9 / 777 / 99), typed.
    pub notifications: Vec<PendingNotification>,
    /// `ConEmu` OSC 9;4 progress — its own typed lane, never a
    /// notification (different field, not a filtered queue entry).
    pub progress: Option<ProgressState>,
    /// OSC 7 working directory, when it changed since the last drain.
    pub cwd: Option<String>,
    /// OSC 1337 RequestAttention — dock bounce / critical urgency.
    pub attention: bool,
}
