//! Typed action-injection queue — the bridge between the kanshou
//! introspection plane and the GUI event loop.
//!
//! `simulate_chord` (the MCP tool) forwards a chord string to the
//! live GUI via kanshou; the GUI-side handler in
//! [`crate::kanshou_state::MadoAppState`] resolves it against the
//! keybinding table and pushes the bound [`Action`] here. The GUI
//! event loop ([`crate::gui_tear_attach::run_against_pane_unified`])
//! drains the queue on every tick (madori runs `ControlFlow::Poll` +
//! emits a `RedrawRequested` per frame, so worst-case dispatch
//! latency is one frame) and feeds each action through EXACTLY the
//! dispatch path a physical keypress takes after key→action
//! resolution.
//!
//! The `sink_attached` flag keeps the surface honest: the kanshou
//! handler refuses to queue (typed `no-injection-sink` answer)
//! unless an event loop has registered itself as the drainer —
//! a queue nobody drains would let `simulate_chord` report
//! `queued: true` while nothing ever happens.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::keybind::Action;

/// Cross-thread FIFO of keybind [`Action`]s queued for the GUI event
/// loop. Cloning shares the underlying queue (Arc semantics) — the
/// kanshou query thread pushes, the GUI event loop drains.
#[derive(Debug, Clone, Default)]
pub struct InjectedActions {
    queue: Arc<Mutex<VecDeque<Action>>>,
    sink_attached: Arc<AtomicBool>,
}

impl InjectedActions {
    /// Queue an action for the GUI event loop. Called from the
    /// kanshou query thread (`simulate_chord` handler).
    pub fn push(&self, action: Action) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(action);
    }

    /// Take every queued action, FIFO order. Called from the GUI
    /// event loop once per tick.
    #[must_use]
    pub fn drain(&self) -> Vec<Action> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// Number of actions currently waiting. Surfaced in the
    /// `simulate_chord` response so agents can observe queue depth.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[must_use]
    #[allow(dead_code)] // clippy len-without-is-empty convention
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Register the GUI event loop as the live drainer. Until this
    /// is called the kanshou `simulate_chord` handler answers with a
    /// typed `no-injection-sink` error instead of queueing into the
    /// void (e.g. the local-PTY fallback loop, which doesn't drain
    /// injected actions yet).
    pub fn attach_sink(&self) {
        self.sink_attached.store(true, Ordering::Release);
    }

    /// Whether a GUI event loop has registered as the drainer.
    #[must_use]
    pub fn sink_attached(&self) -> bool {
        self.sink_attached.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_drain_fifo_roundtrip() {
        let inj = InjectedActions::default();
        inj.push(Action::FontIncrease);
        inj.push(Action::FontReset);
        assert_eq!(inj.len(), 2);
        assert_eq!(inj.drain(), vec![Action::FontIncrease, Action::FontReset]);
        assert!(inj.is_empty());
        // Drain on empty is well-defined.
        assert_eq!(inj.drain(), Vec::<Action>::new());
    }

    #[test]
    fn clones_share_the_queue_and_sink_flag() {
        // The kanshou thread holds one clone, the event loop another —
        // both must observe the same state (Arc semantics).
        let a = InjectedActions::default();
        let b = a.clone();
        assert!(!a.sink_attached());
        b.attach_sink();
        assert!(a.sink_attached());
        a.push(Action::DirPickerOpen);
        assert_eq!(b.drain(), vec![Action::DirPickerOpen]);
        assert!(a.is_empty());
    }
}
