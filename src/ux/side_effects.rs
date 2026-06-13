//! [`TerminalSideEffects`] — the typed drain payload for everything
//! the VT engine accumulates as a side effect of feeding bytes —
//! plus the typed vocabulary the Terminal queues those effects in
//! ([`PendingNotification`], [`Urgency`], [`ProgressState`]) and the
//! ONE shared consumer ([`apply_side_effects`]) both render modes
//! dispatch the drained payload through.
//!
//! M4 closes the seam M1 deferred: `Terminal::drain_side_effects()`
//! is the single producer, and both event loops (`main.rs`
//! RedrawRequested + the `gui_tear_attach` owning loop) route the
//! drained value through [`apply_side_effects`] — the per-loop
//! `take_bell` / `take_clipboard` / title-diff polling copies are
//! deleted and BANNED by `tests/ux_unification.rs`'s drain markers.
//! The drain is pure state transfer: same pre-state drains the same
//! value; an immediately repeated drain yields the default payload.

/// One typed urgency vocabulary for the whole pipeline — the OSC 99
/// `u=` parse (`0`=Low, `1`=Normal, `2`=Critical), the queue entry,
/// and the tsuuchi dispatch all share `tsuuchi::Urgency` (re-export,
/// not a mirror — per the org single-definition rule). OSC 9 and
/// OSC 777 carry no urgency and default to `Normal`; BEL routes
/// `Normal`; OSC 1337 `RequestAttention` routes `Critical`.
pub use tsuuchi::Urgency;

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

impl PendingNotification {
    /// Project the queue entry onto tsuuchi's dispatch type. A
    /// body-only OSC 9 entry titles as "mado" so the OS banner names
    /// its origin; urgency and group carry through typed.
    fn into_tsuuchi(self) -> tsuuchi::Notification {
        let title = self.title.unwrap_or_else(|| "mado".to_owned());
        let n = tsuuchi::Notification::new(title, self.body).urgency(self.urgency);
        match self.group {
            Some(g) => n.group(g),
            None => n,
        }
    }
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

/// One frame's worth of terminal side effects, drained atomically by
/// `Terminal::drain_side_effects()` and dispatched by
/// [`apply_side_effects`].
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
    /// OSC 1337 `RequestAttention` rising edge since the last drain —
    /// dock bounce + critical-urgency dispatch.
    pub attention: bool,
}

/// THE shared per-frame side-effect consumer — the single
/// implementation both event-loop adapters call with their drained
/// [`TerminalSideEffects`] (M1 ux-unification pattern; the drain
/// markers in `tests/ux_unification.rs` ban per-loop polling and
/// require this call in both adapters).
///
/// Routing:
/// - clipboard → system clipboard via the loop's `hasami` provider
/// - bell → renderer bell flash AND a Normal-urgency notification
/// - notifications → tsuuchi dispatch with their typed urgency
/// - attention edge → Critical-urgency notification + dock attention
/// - progress / cwd → typed trace (no renderer surface yet — the
///   progress UI lands with the renderer's M4 follow-up)
/// - title → returned to the adapter, which owns the
///   `EventResponse::set_title` translation (madori applies it)
pub fn apply_side_effects(
    effects: TerminalSideEffects,
    renderer: &mut crate::render::TerminalRenderer,
    clipboard: &dyn hasami::ClipboardProvider,
    notifier: &tsuuchi::NotificationDispatcher,
) -> Option<String> {
    if let Some(clip) = effects.clipboard
        && let Err(e) = clipboard.copy_text(&clip)
    {
        tracing::warn!(error = %e, "OSC 52 clipboard sync failed");
    }
    if effects.bell {
        renderer.trigger_bell();
        dispatch(
            notifier,
            &tsuuchi::Notification::new("mado", "Bell")
                .urgency(Urgency::Normal)
                .group("bell"),
        );
    }
    for pending in effects.notifications {
        dispatch(notifier, &pending.into_tsuuchi());
    }
    if effects.attention {
        dispatch(
            notifier,
            &tsuuchi::Notification::new("mado", "Attention requested")
                .urgency(Urgency::Critical)
                .group("attention"),
        );
        crate::platform::request_dock_attention();
    }
    if let Some(progress) = effects.progress {
        tracing::debug!(?progress, "ConEmu progress update (no renderer surface yet)");
    }
    if let Some(cwd) = effects.cwd {
        tracing::trace!(%cwd, "OSC 7 cwd update");
    }
    effects.title
}

/// Notification delivery is best-effort: a failed backend send is an
/// operator-observability loss, never a render-loop fault.
fn dispatch(notifier: &tsuuchi::NotificationDispatcher, n: &tsuuchi::Notification) {
    if let Err(e) = notifier.send(n) {
        tracing::warn!(error = %e, title = %n.title, "notification dispatch failed");
    }
}
