//! Notification orchestration — the mado-side layer between the drained
//! terminal side-effects and the `tsuuchi` backend.
//!
//! The [`NotificationGate`] is the pure, clock-injected decision core:
//! focus policy, coalescing, rate-limiting, mute, and the master switch.
//! It is fully testable without a real backend or wall clock. The
//! [`NotificationCenter`] wraps a `tsuuchi::NotificationDispatcher` with a
//! gate and a history ring; it is what the event loops own and drive.
//!
//! See `docs/NOTIFICATIONS.md` (layer L4).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::config::{CommandCompletionConfig, NotificationsConfig, NotifyWhen};
use tsuuchi::{HistoryEntry, Notification, NotificationDispatcher, NotificationHistory};

/// Outcome of the gate's admission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Deliver the notification now.
    Deliver,
    /// Suppressed, with the reason (for tracing / observability).
    Suppressed(SuppressReason),
}

/// Why the gate suppressed a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// The whole subsystem is disabled (`notifications.enabled = false`).
    Disabled,
    /// Muted at runtime (do-not-disturb toggle).
    Muted,
    /// Focus policy is `unfocused` and mado is currently focused.
    Focused,
    /// A same-(title, group) notification fired within the coalesce window.
    Coalesced,
    /// The rolling-minute rate limit is exhausted.
    RateLimited,
}

/// Pure admission gate. Injecting `now` keeps every decision testable
/// with a fake clock.
#[derive(Debug)]
pub struct NotificationGate {
    enabled: bool,
    rate_limit_per_min: u32,
    coalesce_window: Duration,
    muted: bool,
    /// Delivery timestamps within the trailing minute (rate limiter).
    recent: VecDeque<Instant>,
    /// Last delivery time per (title, group) key (coalescer).
    last_seen: HashMap<(String, Option<String>), Instant>,
}

impl NotificationGate {
    /// Build a gate from the notifications config.
    #[must_use]
    pub fn new(cfg: &NotificationsConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            rate_limit_per_min: cfg.rate_limit_per_min,
            coalesce_window: Duration::from_millis(cfg.coalesce_window_ms),
            muted: false,
            recent: VecDeque::new(),
            last_seen: HashMap::new(),
        }
    }

    /// Re-apply config (hot-reload) without dropping runtime state.
    pub fn reconfigure(&mut self, cfg: &NotificationsConfig) {
        self.enabled = cfg.enabled;
        self.rate_limit_per_min = cfg.rate_limit_per_min;
        self.coalesce_window = Duration::from_millis(cfg.coalesce_window_ms);
    }

    /// Toggle do-not-disturb.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Whether currently muted.
    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Decide whether to deliver `n`, recording state (rate + coalesce)
    /// only when the decision is [`GateDecision::Deliver`]. `when` is the
    /// effective focus policy for this notification (a source may tighten
    /// the global default). `focused` is whether mado is the key window.
    pub fn admit(
        &mut self,
        n: &Notification,
        when: NotifyWhen,
        focused: bool,
        now: Instant,
    ) -> GateDecision {
        if !self.enabled {
            return GateDecision::Suppressed(SuppressReason::Disabled);
        }
        if self.muted {
            return GateDecision::Suppressed(SuppressReason::Muted);
        }
        if when == NotifyWhen::Unfocused && focused {
            return GateDecision::Suppressed(SuppressReason::Focused);
        }

        // Coalesce: same (title, group) within the window collapses.
        if !self.coalesce_window.is_zero() {
            let key = (n.title.clone(), n.group.clone());
            if let Some(prev) = self.last_seen.get(&key) {
                if now.duration_since(*prev) < self.coalesce_window {
                    return GateDecision::Suppressed(SuppressReason::Coalesced);
                }
            }
        }

        // Rate limit: evict entries older than a minute, then check.
        if self.rate_limit_per_min > 0 {
            let minute_ago = now.checked_sub(Duration::from_secs(60));
            while let Some(front) = self.recent.front() {
                match minute_ago {
                    Some(cutoff) if *front <= cutoff => {
                        self.recent.pop_front();
                    }
                    _ => break,
                }
            }
            if self.recent.len() as u32 >= self.rate_limit_per_min {
                return GateDecision::Suppressed(SuppressReason::RateLimited);
            }
        }

        // Admitted — record for future coalesce + rate decisions.
        if !self.coalesce_window.is_zero() {
            self.last_seen
                .insert((n.title.clone(), n.group.clone()), now);
        }
        if self.rate_limit_per_min > 0 {
            self.recent.push_back(now);
        }
        GateDecision::Deliver
    }
}

/// The orchestrator the event loops own: a gate + a backend dispatcher +
/// a delivered-notification history ring.
pub struct NotificationCenter {
    dispatcher: NotificationDispatcher,
    gate: NotificationGate,
    history: NotificationHistory,
    /// Full config retained for source-gating queries (bell.notify,
    /// command-completion policy) — the gate owns delivery-gating.
    cfg: NotificationsConfig,
}

impl NotificationCenter {
    /// Build the center from a dispatcher and the operator's config.
    #[must_use]
    pub fn new(dispatcher: NotificationDispatcher, cfg: &NotificationsConfig) -> Self {
        Self {
            dispatcher,
            gate: NotificationGate::new(cfg),
            history: NotificationHistory::new(cfg.history_capacity.max(1)),
            cfg: cfg.clone(),
        }
    }

    /// Re-apply config on hot-reload.
    pub fn reconfigure(&mut self, cfg: &NotificationsConfig) {
        self.gate.reconfigure(cfg);
        self.cfg = cfg.clone();
    }

    /// Whether a BEL should raise a desktop notification.
    #[must_use]
    pub fn bell_notify(&self) -> bool {
        self.cfg.bell.notify
    }

    /// The command-completion notification policy.
    #[must_use]
    pub fn command_completion(&self) -> &CommandCompletionConfig {
        &self.cfg.command_completion
    }

    /// The backend's actual capabilities (honest degradation surface).
    #[must_use]
    pub fn capabilities(&self) -> tsuuchi::Capabilities {
        self.dispatcher.capabilities()
    }

    /// Toggle do-not-disturb; returns the new muted state.
    pub fn toggle_mute(&mut self) -> bool {
        let next = !self.gate.is_muted();
        self.gate.set_muted(next);
        next
    }

    /// Whether currently muted.
    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.gate.is_muted()
    }

    /// Recent delivered notifications (newest last), up to `n`.
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<HistoryEntry> {
        self.history.recent(n).into_iter().cloned().collect()
    }

    /// Deliver `n` subject to the gate, using the global focus policy.
    pub fn notify(&mut self, n: &Notification, focused: bool) {
        self.notify_with(n, self.cfg.when, focused);
    }

    /// Deliver `n` subject to the gate, with an explicit focus policy
    /// (a source that must always deliver — e.g. an explicit attention
    /// request — passes [`NotifyWhen::Always`]).
    pub fn notify_with(&mut self, n: &Notification, when: NotifyWhen, focused: bool) {
        match self.gate.admit(n, when, focused, Instant::now()) {
            GateDecision::Deliver => {
                if let Err(e) = self.dispatcher.send(n) {
                    tracing::warn!(error = %e, title = %n.title, "notification dispatch failed");
                } else {
                    self.history.push(n.clone());
                }
            }
            GateDecision::Suppressed(reason) => {
                tracing::trace!(?reason, title = %n.title, "notification suppressed by gate");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> NotificationsConfig {
        NotificationsConfig::prescribed()
    }

    fn note(title: &str) -> Notification {
        Notification::new(title, "body")
    }

    #[test]
    fn disabled_suppresses_everything() {
        let mut c = NotificationsConfig::prescribed();
        c.enabled = false;
        let mut g = NotificationGate::new(&c);
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Always, false, Instant::now()),
            GateDecision::Suppressed(SuppressReason::Disabled)
        );
    }

    #[test]
    fn unfocused_policy_suppresses_when_focused() {
        let mut g = NotificationGate::new(&cfg());
        let now = Instant::now();
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Unfocused, true, now),
            GateDecision::Suppressed(SuppressReason::Focused)
        );
        // Same notification while unfocused is delivered.
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Unfocused, false, now),
            GateDecision::Deliver
        );
    }

    #[test]
    fn always_policy_delivers_even_when_focused() {
        let mut g = NotificationGate::new(&cfg());
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Always, true, Instant::now()),
            GateDecision::Deliver
        );
    }

    #[test]
    fn coalesces_same_title_group_within_window() {
        let mut c = cfg();
        c.coalesce_window_ms = 1000;
        let mut g = NotificationGate::new(&c);
        let t0 = Instant::now();
        assert_eq!(g.admit(&note("bell"), NotifyWhen::Always, false, t0), GateDecision::Deliver);
        // 500ms later — within window → coalesced.
        let t1 = t0 + Duration::from_millis(500);
        assert_eq!(
            g.admit(&note("bell"), NotifyWhen::Always, false, t1),
            GateDecision::Suppressed(SuppressReason::Coalesced)
        );
        // 1200ms after the *first* delivery → outside window → delivered.
        let t2 = t0 + Duration::from_millis(1200);
        assert_eq!(g.admit(&note("bell"), NotifyWhen::Always, false, t2), GateDecision::Deliver);
    }

    #[test]
    fn different_titles_do_not_coalesce() {
        let mut c = cfg();
        c.coalesce_window_ms = 5000;
        let mut g = NotificationGate::new(&c);
        let t = Instant::now();
        assert_eq!(g.admit(&note("a"), NotifyWhen::Always, false, t), GateDecision::Deliver);
        assert_eq!(g.admit(&note("b"), NotifyWhen::Always, false, t), GateDecision::Deliver);
    }

    #[test]
    fn rate_limit_caps_per_minute_then_recovers() {
        let mut c = cfg();
        c.rate_limit_per_min = 3;
        c.coalesce_window_ms = 0; // isolate the rate limiter
        let mut g = NotificationGate::new(&c);
        let t0 = Instant::now();
        // Distinct titles so coalescing (off anyway) is irrelevant.
        for i in 0..3 {
            assert_eq!(
                g.admit(&note(&i.to_string()), NotifyWhen::Always, false, t0),
                GateDecision::Deliver,
                "delivery {i} should pass"
            );
        }
        // Fourth within the minute → rate limited.
        assert_eq!(
            g.admit(&note("x"), NotifyWhen::Always, false, t0),
            GateDecision::Suppressed(SuppressReason::RateLimited)
        );
        // 61s later the window has slid → delivered again.
        let t1 = t0 + Duration::from_secs(61);
        assert_eq!(g.admit(&note("y"), NotifyWhen::Always, false, t1), GateDecision::Deliver);
    }

    #[test]
    fn rate_limit_zero_is_unlimited() {
        let mut c = cfg();
        c.rate_limit_per_min = 0;
        c.coalesce_window_ms = 0;
        let mut g = NotificationGate::new(&c);
        let t = Instant::now();
        for i in 0..1000 {
            assert_eq!(
                g.admit(&note(&i.to_string()), NotifyWhen::Always, false, t),
                GateDecision::Deliver
            );
        }
    }

    #[test]
    fn mute_suppresses_until_unmuted() {
        let mut g = NotificationGate::new(&cfg());
        g.set_muted(true);
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Always, false, Instant::now()),
            GateDecision::Suppressed(SuppressReason::Muted)
        );
        g.set_muted(false);
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Always, false, Instant::now()),
            GateDecision::Deliver
        );
    }

    #[test]
    fn center_records_history_on_delivery() {
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg());
        center.notify_with(&note("hello"), NotifyWhen::Always, false);
        let recent = center.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].notification.title, "hello");
    }

    #[test]
    fn center_history_respects_focus_suppression() {
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg());
        // Default policy is Unfocused; focused → suppressed → no history.
        center.notify(&note("suppressed"), true);
        assert_eq!(center.recent(10).len(), 0);
    }

    #[test]
    fn center_mute_toggle_roundtrips() {
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg());
        assert!(!center.is_muted());
        assert!(center.toggle_mute());
        assert!(center.is_muted());
        assert!(!center.toggle_mute());
    }
}
