//! Typed session-switch request channel — the bridge between the
//! kanshou introspection plane and the GUI event loop's switchable
//! attach.
//!
//! `switch_session` (the MCP tool) forwards a target pane id to the
//! live GUI via kanshou; the GUI-side handler in
//! [`crate::kanshou_state::MadoAppState`] parses it and posts the
//! request here. The GUI event loop
//! ([`crate::gui_tear_attach::run_against_pane_unified`]) polls this
//! channel on every tick (only when `tear.session_switching = true`)
//! and, when a request is pending, tears down the current engate
//! attach + rebuilds it against the requested pane while the madori
//! window + renderer persist.
//!
//! Mirrors [`crate::action_injection::InjectedActions`] in shape +
//! discipline:
//!
//! * The `sink_attached` flag keeps the surface honest — the kanshou
//!   handler refuses to accept a request (typed `switching-disabled`
//!   answer) unless an event loop has registered itself as the
//!   drainer. A request nobody drains would let `switch_session`
//!   report success while nothing ever happens.
//! * The pending slot is `Option<PaneId>` (single-slot, last-write-
//!   wins) rather than a queue: a switch is a level, not an edge —
//!   posting B then C before the loop ticks should land on C, not
//!   replay B→C. The event loop takes the latest target.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tear_types::PaneId;

/// Cross-thread single-slot switch-request channel. Cloning shares
/// the underlying slot (Arc semantics) — the kanshou query thread
/// posts, the GUI event loop takes. Last write wins: posting a new
/// target before the loop drains the previous one replaces it.
#[derive(Debug, Clone, Default)]
pub struct SwitchRequests {
    /// The pending switch target, if any. `None` = no switch pending.
    pending: Arc<Mutex<Option<PaneId>>>,
    /// Whether a switchable event loop has registered as the drainer.
    sink_attached: Arc<AtomicBool>,
}

impl SwitchRequests {
    /// Post a switch request for the GUI event loop. Called from the
    /// kanshou query thread (`switch_session` handler). Last write
    /// wins — a pending target not yet taken is overwritten.
    pub fn post(&self, pane: PaneId) {
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pane);
    }

    /// Take the pending switch target, clearing the slot. Called from
    /// the GUI event loop once per tick. Returns `None` when no switch
    /// is pending.
    #[must_use]
    pub fn take(&self) -> Option<PaneId> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Whether a switch is currently pending (not yet taken).
    #[must_use]
    #[allow(dead_code)] // observability surface for the kanshou response
    pub fn is_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Register the GUI event loop as the live drainer. Until this is
    /// called the kanshou `switch_session` handler answers with a
    /// typed `switching-disabled` error instead of posting into the
    /// void (the legacy one-shot loop, which never polls this).
    pub fn attach_sink(&self) {
        self.sink_attached.store(true, Ordering::Release);
    }

    /// Whether a switchable event loop has registered as the drainer.
    /// Mirrors the `tear.session_switching` flag's runtime effect: a
    /// loop only attaches the sink when the flag is on.
    #[must_use]
    pub fn sink_attached(&self) -> bool {
        self.sink_attached.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(seed: &str) -> PaneId {
        PaneId::from_seed(seed)
    }

    #[test]
    fn post_take_roundtrip() {
        let sw = SwitchRequests::default();
        assert!(sw.take().is_none(), "fresh channel has nothing pending");
        let target = pane("pane-b");
        sw.post(target);
        assert!(sw.is_pending());
        assert_eq!(sw.take(), Some(target));
        assert!(!sw.is_pending(), "take clears the slot");
        assert!(sw.take().is_none(), "take on empty is well-defined");
    }

    #[test]
    fn last_write_wins() {
        // A switch is a level, not an edge: posting B then C before
        // the loop ticks lands on C, never replays B→C.
        let sw = SwitchRequests::default();
        let b = pane("pane-b");
        let c = pane("pane-c");
        sw.post(b);
        sw.post(c);
        assert_eq!(sw.take(), Some(c), "the latest target wins");
        assert!(sw.take().is_none());
    }

    #[test]
    fn clones_share_the_slot_and_sink_flag() {
        // The kanshou thread holds one clone, the event loop another —
        // both must observe the same state (Arc semantics).
        let a = SwitchRequests::default();
        let b = a.clone();
        assert!(!a.sink_attached());
        b.attach_sink();
        assert!(a.sink_attached(), "sink flag is shared across clones");
        let target = pane("pane-x");
        a.post(target);
        assert_eq!(b.take(), Some(target), "the slot is shared across clones");
        assert!(a.take().is_none());
    }

    #[test]
    fn sink_starts_detached() {
        // The legacy one-shot loop never attaches the sink — the
        // kanshou handler must see it detached so it answers
        // `switching-disabled` rather than posting into the void.
        let sw = SwitchRequests::default();
        assert!(!sw.sink_attached());
    }
}
