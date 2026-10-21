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

/// A dock-progress signal — the terminal's OSC 9;4 `ProgressState` mapped
/// to what the dock badge should show. Kept decoupled from the OSC-layer
/// enum so the center owns no terminal-parse types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockProgress {
    /// Determinate progress, 0..=100 percent → badge `"<pct>%"`.
    Percent(u8),
    /// Indeterminate / busy (no percentage) → badge `"…"`.
    Busy,
    /// No active progress → revert the badge to the unread count.
    Clear,
}

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

pub const LOCAL_PANE_GROUP: &str = "local";

/// The orchestrator the event loops own: a gate + a backend dispatcher +
/// a delivered-notification history ring.
pub struct NotificationCenter {
    dispatcher: NotificationDispatcher,
    gate: NotificationGate,
    history: NotificationHistory,
    /// Full config retained for source-gating queries (bell.notify,
    /// command-completion policy) — the gate owns delivery-gating.
    cfg: NotificationsConfig,
    /// Dock-badge count: notifications delivered while unfocused, cleared
    /// on focus. The count logic is pure + testable; the dock write is a
    /// platform sink.
    badge_count: u32,
    /// Focus state at the last `on_frame` — to detect the unfocused→
    /// focused transition that clears the badge.
    last_focused: bool,
    /// Active command progress (OSC 9;4). While `Some`, it takes badge
    /// precedence over the unread count.
    progress: Option<DockProgress>,
    alerts: crate::pane_alert::Detector,
    base_title: Option<String>,
    pane_flash: bool,
    pane_group: String,
    cwd: Option<String>,
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
            badge_count: 0,
            last_focused: true,
            progress: None,
            alerts: crate::pane_alert::Detector::new(&cfg.pane_alerts),
            base_title: None,
            pane_flash: false,
            pane_group: LOCAL_PANE_GROUP.to_owned(),
            cwd: None,
        }
    }

    /// Update the dock command-progress signal (OSC 9;4). `Clear` reverts
    /// the badge to the unread count.
    pub fn set_progress(&mut self, p: DockProgress) {
        self.progress = match p {
            DockProgress::Clear => None,
            other => Some(other),
        };
        self.refresh_badge();
    }

    /// The dock badge label right now: active progress takes precedence
    /// (`"45%"` / `"…"`), else the unread count (or nothing).
    #[must_use]
    fn badge_label(&self) -> Option<String> {
        if self.cfg.progress_dock {
            match self.progress {
                Some(DockProgress::Percent(pct)) => {
                    let mut s = pct.to_string();
                    s.push('%');
                    return Some(s);
                }
                Some(DockProgress::Busy) => return Some("…".to_owned()),
                // `set_progress` maps `Clear` to `None`, so a stored
                // `Some(Clear)` is unrepresentable — but keep the match
                // exhaustive.
                Some(DockProgress::Clear) | None => {}
            }
        }
        if self.cfg.badge_unread && self.badge_count > 0 {
            Some(self.badge_count.to_string())
        } else {
            None
        }
    }

    /// Write the arbitrated badge label to the dock (progress vs unread).
    fn refresh_badge(&self) {
        crate::platform::set_badge(self.badge_label().as_deref());
    }

    /// Per-frame focus tick — the event loop calls this every frame with
    /// the current window focus. On the unfocused→focused transition it
    /// clears the dock badge (the operator has seen what was waiting).
    pub fn on_frame(&mut self, focused: bool) {
        if focused && !self.last_focused && self.badge_count != 0 {
            self.badge_count = 0;
            self.refresh_badge();
        }
        self.last_focused = focused;
    }

    /// The current unread-while-unfocused badge count (for tests + the
    /// MCP status surface).
    #[must_use]
    pub fn badge_count(&self) -> u32 {
        self.badge_count
    }

    /// Re-apply config on hot-reload.
    pub fn reconfigure(&mut self, cfg: &NotificationsConfig) {
        self.gate.reconfigure(cfg);
        self.alerts.reconfigure(&cfg.pane_alerts);
        self.cfg = cfg.clone();
    }

    #[must_use]
    pub fn pane_alerts_own_long_command(&self) -> bool {
        self.cfg.pane_alerts.owns_long_command_banner()
    }

    pub fn take_pane_flash(&mut self) -> bool {
        std::mem::take(&mut self.pane_flash)
    }

    #[must_use]
    pub fn pane_alert_deadline(&self) -> Option<Instant> {
        self.alerts.next_deadline()
    }

    pub fn set_pane_group(&mut self, group: &str) {
        if self.pane_group != group {
            group.clone_into(&mut self.pane_group);
        }
    }

    #[must_use]
    pub fn pane_group(&self) -> &str {
        &self.pane_group
    }

    pub fn note_cwd(&mut self, osc7: &str) {
        self.cwd = Some(cwd_path(osc7));
    }

    fn context(&self) -> crate::notify_card::PaneContext<'_> {
        crate::notify_card::PaneContext {
            group: &self.pane_group,
            title: self.base_title.as_deref(),
            cwd: self.cwd.as_deref(),
        }
    }

    #[must_use]
    pub fn command_notice(&self, cc: &crate::ux::CommandCompletion) -> Option<Notification> {
        let rank = self.cfg.command_completion.rank(cc)?;
        Some(if self.cfg.relevance.enabled {
            crate::notify_card::command_card(cc, rank, &self.cfg.relevance, &self.context())
        } else {
            crate::ux::side_effects::completion_notification(cc)
        })
    }

    #[must_use]
    pub fn has_pane_alert(&self) -> bool {
        self.alerts.alert().is_some()
    }

    pub fn acknowledge(&mut self, now: Instant) -> bool {
        self.pane_alert(now, crate::pane_alert::Input::Focus(true), None, true)
    }

    pub fn pane_alert(
        &mut self,
        now: Instant,
        input: crate::pane_alert::Input<'_>,
        detail: Option<&str>,
        focused: bool,
    ) -> bool {
        self.pane_alert_notice(now, input, detail, None, focused)
    }

    pub fn pane_alert_notice(
        &mut self,
        now: Instant,
        input: crate::pane_alert::Input<'_>,
        detail: Option<&str>,
        notice: Option<&Notification>,
        focused: bool,
    ) -> bool {
        use crate::pane_alert::{AlertReason, Outcome};
        let Some(outcome) = self.alerts.step(now, input) else {
            return false;
        };
        if let Outcome::Raised { reason, delivery } = outcome {
            if let Some(d) = detail {
                self.alerts.set_detail(d.to_owned());
            }
            if self.alerts.config().flash {
                self.pane_flash = true;
                crate::platform::request_dock_attention_once();
            }
            if delivery == crate::config::AlertDelivery::Notify {
                let n = match (reason, notice) {
                    (AlertReason::LongCommand, Some(card)) => card.clone(),
                    _ => self.pane_alert_notification(reason, detail),
                };
                self.notify_with(&n, NotifyWhen::Always, focused);
            } else if self.cfg.badge_unread {
                self.badge_count = self.badge_count.saturating_add(1);
                self.refresh_badge();
            }
        }
        crate::pane_alert::publish(self.alerts.alert(), self.base_title.as_deref());
        true
    }

    fn pane_alert_notification(
        &self,
        reason: crate::pane_alert::AlertReason,
        detail: Option<&str>,
    ) -> Notification {
        use crate::pane_alert::AlertReason;
        if self.cfg.relevance.enabled && reason == AlertReason::WaitingForInput {
            let awaited = detail.map(str::to_owned).unwrap_or_else(|| {
                self.alerts
                    .alert()
                    .and_then(|a| a.prompt)
                    .map_or("Waiting for input", crate::pane_alert::PromptKind::label)
                    .to_owned()
            });
            return crate::notify_card::waiting_card(
                &awaited,
                &self.cfg.relevance,
                &self.context(),
            );
        }
        let title = self.base_title.clone().unwrap_or_else(|| "mado".to_owned());
        let body = match (reason, detail) {
            (_, Some(d)) => d.to_owned(),
            (AlertReason::WaitingForInput, None) => self
                .alerts
                .alert()
                .and_then(|a| a.prompt)
                .map_or("Waiting for input", crate::pane_alert::PromptKind::label)
                .to_owned(),
            (AlertReason::LongCommand, None) => "Command finished".to_owned(),
            (AlertReason::Idle, None) => "Output went quiet".to_owned(),
            (AlertReason::Bell, None) => "Bell".to_owned(),
        };
        Notification::new(title, body)
            .urgency(tsuuchi::Urgency::Normal)
            .group(format!("pane-alert-{}", reason.slug()))
    }

    pub fn pane_title(&mut self, drained: Option<String>, dirty: bool) -> Option<String> {
        let changed = drained.is_some();
        if let Some(t) = drained {
            self.base_title = Some(t);
            if let Some(a) = self.alerts.alert() {
                crate::pane_alert::publish(Some(a), self.base_title.as_deref());
            }
        }
        (changed || dirty).then(|| {
            crate::pane_alert::badged_title(
                self.alerts.config(),
                self.base_title.as_deref().unwrap_or("mado"),
                self.alerts.alert().is_some(),
            )
        })
    }

    /// Whether a BEL should raise a desktop notification.
    #[must_use]
    pub fn bell_notify(&self) -> bool {
        self.cfg.bell.notify
    }

    /// Whether a BEL should ring the native audible bell.
    #[must_use]
    pub fn bell_audible(&self) -> bool {
        self.cfg.bell.audible
    }

    /// The `NSSound` name for the audible bell (`None` = system beep).
    #[must_use]
    pub fn bell_sound_name(&self) -> Option<&'static str> {
        self.cfg.bell.sound.sound_name()
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
        let addressed =
            crate::notify_card::address(n, &self.pane_group, self.cfg.relevance.focus_action);
        let n = &addressed;
        match self.gate.admit(n, when, focused, Instant::now()) {
            GateDecision::Deliver => {
                if let Err(e) = self.dispatcher.send(n) {
                    tracing::warn!(error = %e, title = %n.title, "notification dispatch failed");
                } else {
                    self.history.push(n.clone());
                    // Badge the dock with the unread-while-away count
                    // (arbitrated with any active progress).
                    if self.cfg.badge_unread && !focused {
                        self.badge_count = self.badge_count.saturating_add(1);
                        self.refresh_badge();
                    }
                }
            }
            GateDecision::Suppressed(reason) => {
                tracing::trace!(?reason, title = %n.title, "notification suppressed by gate");
            }
        }
    }
}

fn cwd_path(osc7: &str) -> String {
    let path = osc7
        .strip_prefix("file://")
        .map_or(osc7, |rest| rest.find('/').map_or(rest, |i| &rest[i..]));
    crate::links::percent_decode(path)
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
        assert_eq!(
            g.admit(&note("bell"), NotifyWhen::Always, false, t0),
            GateDecision::Deliver
        );
        // 500ms later — within window → coalesced.
        let t1 = t0 + Duration::from_millis(500);
        assert_eq!(
            g.admit(&note("bell"), NotifyWhen::Always, false, t1),
            GateDecision::Suppressed(SuppressReason::Coalesced)
        );
        // 1200ms after the *first* delivery → outside window → delivered.
        let t2 = t0 + Duration::from_millis(1200);
        assert_eq!(
            g.admit(&note("bell"), NotifyWhen::Always, false, t2),
            GateDecision::Deliver
        );
    }

    #[test]
    fn different_titles_do_not_coalesce() {
        let mut c = cfg();
        c.coalesce_window_ms = 5000;
        let mut g = NotificationGate::new(&c);
        let t = Instant::now();
        assert_eq!(
            g.admit(&note("a"), NotifyWhen::Always, false, t),
            GateDecision::Deliver
        );
        assert_eq!(
            g.admit(&note("b"), NotifyWhen::Always, false, t),
            GateDecision::Deliver
        );
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
        assert_eq!(
            g.admit(&note("y"), NotifyWhen::Always, false, t1),
            GateDecision::Deliver
        );
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

    #[test]
    fn badge_counts_unfocused_deliveries_and_clears_on_focus() {
        // prescribed() has badge_unread = true.
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg());
        assert_eq!(center.badge_count(), 0);
        // Two deliveries while unfocused → badge counts them (Always policy
        // so the focus gate doesn't suppress the delivery itself).
        center.notify_with(&note("a"), NotifyWhen::Always, false);
        center.notify_with(&note("b"), NotifyWhen::Always, false);
        assert_eq!(center.badge_count(), 2);
        // A delivery while focused does not badge.
        center.notify_with(&note("c"), NotifyWhen::Always, true);
        assert_eq!(center.badge_count(), 2);
        // The unfocused→focused transition clears the badge.
        center.on_frame(false);
        center.on_frame(true);
        assert_eq!(center.badge_count(), 0);
    }

    #[test]
    fn badge_off_when_disabled() {
        let mut c = cfg();
        c.badge_unread = false;
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &c);
        center.notify_with(&note("a"), NotifyWhen::Always, false);
        assert_eq!(center.badge_count(), 0, "badge disabled → never counts");
    }

    #[test]
    fn progress_badge_takes_precedence_over_unread_then_reverts() {
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg());
        // One unread-while-unfocused notification → badge "1".
        center.notify_with(&note("a"), NotifyWhen::Always, false);
        assert_eq!(center.badge_label().as_deref(), Some("1"));
        // Active determinate progress overrides the count.
        center.set_progress(DockProgress::Percent(45));
        assert_eq!(center.badge_label().as_deref(), Some("45%"));
        // Busy (indeterminate) shows the ellipsis.
        center.set_progress(DockProgress::Busy);
        assert_eq!(center.badge_label().as_deref(), Some("…"));
        // Clearing progress reverts to the unread count.
        center.set_progress(DockProgress::Clear);
        assert_eq!(center.badge_label().as_deref(), Some("1"));
    }

    fn alert_center() -> NotificationCenter {
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        NotificationCenter::new(dispatcher, &cfg())
    }

    const CLAUDE_WAITING: &str = "────────────────\n❯ \n────────────────\n";

    #[test]
    fn a_waiting_pane_badges_its_title_and_notifies_with_it() {
        use crate::pane_alert::Input;
        let mut center = alert_center();
        let t0 = Instant::now();
        assert_eq!(
            center
                .pane_title(Some("agent run".to_owned()), false)
                .as_deref(),
            Some("agent run")
        );
        center.pane_alert(t0, Input::Focus(false), None, false);
        assert!(
            center.pane_alert(
                t0,
                Input::Output {
                    tail: Some(CLAUDE_WAITING)
                },
                None,
                false
            ) == false
        );
        assert!(center.pane_alert(t0 + Duration::from_secs(1), Input::Tick, None, false));
        assert_eq!(
            center.pane_title(None, true).as_deref(),
            Some("● agent run")
        );
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].notification.title, "Waiting for input");
        assert_eq!(
            delivered[0].notification.subtitle.as_deref(),
            Some("agent run")
        );
        assert_eq!(
            delivered[0].notification.body,
            "⏳ Claude Code is waiting for input"
        );
        assert!(center.pane_alert(t0 + Duration::from_secs(2), Input::Focus(true), None, true));
        assert_eq!(center.pane_title(None, true).as_deref(), Some("agent run"));
    }

    #[test]
    fn a_badge_only_reason_never_reaches_the_backend() {
        use crate::pane_alert::Input;
        let mut center = alert_center();
        let t0 = Instant::now();
        center.pane_alert(t0, Input::Focus(false), None, false);
        assert!(center.pane_alert(t0, Input::Bell, None, false));
        assert!(center.recent(10).is_empty());
        assert_eq!(
            center.badge_count(),
            1,
            "a badge-only alert still counts on the dock"
        );
        assert!(center.take_pane_flash());
        assert!(!center.take_pane_flash(), "the in-window flash fires once");
        assert_eq!(center.pane_title(None, true).as_deref(), Some("● mado"));
        center.on_frame(false);
        center.on_frame(true);
        assert_eq!(center.badge_count(), 0);
    }

    #[test]
    fn a_long_command_notifies_once_through_the_pane_alert() {
        use crate::pane_alert::Input;
        let mut center = alert_center();
        assert!(center.pane_alerts_own_long_command());
        let t0 = Instant::now();
        center.pane_alert(t0, Input::Focus(false), None, false);
        center.pane_alert(
            t0,
            Input::CommandFinished { qualifies: true },
            Some("✓ Command finished · Done in 12.0s"),
            false,
        );
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            delivered[0].notification.body,
            "✓ Command finished · Done in 12.0s"
        );
    }

    #[test]
    fn disabled_pane_alerts_leave_the_legacy_banner_in_charge() {
        let mut c = cfg();
        c.pane_alerts.enabled = false;
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let center = NotificationCenter::new(dispatcher, &c);
        assert!(!center.pane_alerts_own_long_command());
    }

    fn completion(exit_code: i32, duration_ms: u64, command: &str) -> crate::ux::CommandCompletion {
        crate::ux::CommandCompletion {
            exit_code,
            duration_ms,
            used_alt_screen: false,
            command: Some(command.to_owned()),
        }
    }

    fn finish_command(center: &mut NotificationCenter, cc: &crate::ux::CommandCompletion) {
        use crate::pane_alert::Input;
        let notice = center.command_notice(cc);
        center.pane_alert_notice(
            Instant::now(),
            Input::CommandFinished {
                qualifies: notice.is_some(),
            },
            None,
            notice.as_ref(),
            false,
        );
    }

    #[test]
    fn a_fast_failure_beats_the_duration_threshold() {
        let mut center = alert_center();
        center.pane_alert(
            Instant::now(),
            crate::pane_alert::Input::Focus(false),
            None,
            false,
        );
        finish_command(&mut center, &completion(1, 40, "false"));
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].notification.title, "false");
        assert_eq!(delivered[0].notification.urgency, tsuuchi::Urgency::Normal);
    }

    #[test]
    fn a_fast_success_sends_nothing() {
        let mut center = alert_center();
        center.pane_alert(
            Instant::now(),
            crate::pane_alert::Input::Focus(false),
            None,
            false,
        );
        assert!(center.command_notice(&completion(0, 300, "ls")).is_none());
        finish_command(&mut center, &completion(0, 300, "ls"));
        assert!(center.recent(10).is_empty());
    }

    #[test]
    fn a_slow_success_is_low_urgency() {
        let mut center = alert_center();
        center.pane_alert(
            Instant::now(),
            crate::pane_alert::Input::Focus(false),
            None,
            false,
        );
        finish_command(&mut center, &completion(0, 134_000, "make"));
        let delivered = center.recent(10);
        assert_eq!(delivered[0].notification.urgency, tsuuchi::Urgency::Low);
        assert_eq!(delivered[0].notification.body, "✅ exit 0 · 2m 14s");
    }

    #[test]
    fn the_duration_gate_returns_for_failures_when_the_knob_is_off() {
        let mut c = cfg();
        c.command_completion.failure_ignores_min_duration = false;
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let center = NotificationCenter::new(dispatcher, &c);
        assert!(center.command_notice(&completion(1, 40, "false")).is_none());
        assert!(
            center
                .command_notice(&completion(1, 20_000, "false"))
                .is_some()
        );
    }

    #[test]
    fn a_newer_card_for_the_pane_carries_the_same_id() {
        let mut center = alert_center();
        center.set_pane_group("p1");
        let a = center.command_notice(&completion(1, 40, "first")).unwrap();
        let b = center
            .command_notice(&completion(0, 40_000, "second"))
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(a.group.as_deref(), Some("p1"));
        center.set_pane_group("p2");
        let c = center.command_notice(&completion(1, 40, "first")).unwrap();
        assert_ne!(a.id, c.id);
    }

    #[test]
    fn the_cwd_becomes_the_subtitle_when_the_pane_has_no_title() {
        let mut center = alert_center();
        center.note_cwd("file://host/Users/me/my%20proj");
        let n = center.command_notice(&completion(1, 40, "x")).unwrap();
        assert_eq!(n.subtitle.as_deref(), Some("/Users/me/my proj"));
    }

    #[test]
    fn the_generic_banner_returns_when_relevance_is_off() {
        let mut c = cfg();
        c.relevance.enabled = false;
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let center = NotificationCenter::new(dispatcher, &c);
        let n = center.command_notice(&completion(1, 40, "x")).unwrap();
        assert_eq!(n.title, "✗ Command failed");
    }

    #[test]
    fn waiting_for_input_is_a_card_with_the_prompt_shape() {
        use crate::pane_alert::Input;
        let mut center = alert_center();
        let t0 = Instant::now();
        center.pane_alert(t0, Input::Focus(false), None, false);
        center.pane_alert(
            t0,
            Input::Output {
                tail: Some("Password: "),
            },
            None,
            false,
        );
        center.pane_alert(t0 + Duration::from_secs(2), Input::Tick, None, false);
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        let n = &delivered[0].notification;
        assert_eq!(n.category.as_deref(), Some("waiting-for-input"));
        assert_eq!(n.body, "⏳ Waiting for a password");
        assert_eq!(n.urgency, tsuuchi::Urgency::Normal);
    }

    fn assert_addressed(n: &Notification, pane: &str) {
        assert_eq!(n.group.as_deref(), Some(pane), "group is the pane: {n:?}");
        let focus: Vec<_> = n
            .actions
            .iter()
            .filter(|a| a.id == crate::notify_card::FOCUS_ACTION_ID)
            .collect();
        assert_eq!(focus.len(), 1, "exactly one focus_pane action: {n:?}");
    }

    fn unfocused_center(mutate: impl FnOnce(&mut NotificationsConfig)) -> NotificationCenter {
        let mut c = cfg();
        c.pane_alerts.reasons.bell = crate::config::AlertDelivery::Notify;
        c.pane_alerts.reasons.long_command = crate::config::AlertDelivery::Notify;
        c.pane_alerts.reasons.waiting_for_input = crate::config::AlertDelivery::Notify;
        mutate(&mut c);
        let dispatcher = NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &c);
        center.set_pane_group("pane-x");
        center.pane_alert(
            Instant::now(),
            crate::pane_alert::Input::Focus(false),
            None,
            false,
        );
        center
    }

    #[test]
    fn emit_site_command_finished_card_is_addressed() {
        let mut center = unfocused_center(|_| {});
        finish_command(&mut center, &completion(1, 40, "make"));
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_addressed(&delivered[0].notification, "pane-x");
    }

    #[test]
    fn emit_site_command_finished_generic_banner_is_addressed() {
        let mut center = unfocused_center(|c| c.relevance.enabled = false);
        finish_command(&mut center, &completion(1, 40, "make"));
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].notification.title, "✗ Command failed");
        assert_addressed(&delivered[0].notification, "pane-x");
    }

    #[test]
    fn emit_site_command_completion_banner_is_addressed() {
        let mut center = unfocused_center(|c| c.relevance.enabled = false);
        let card = center.command_notice(&completion(1, 40, "make")).unwrap();
        center.notify_with(&card, NotifyWhen::Always, false);
        assert_addressed(&center.recent(1)[0].notification, "pane-x");
    }

    #[test]
    fn emit_site_waiting_for_input_card_is_addressed() {
        use crate::pane_alert::Input;
        let mut center = unfocused_center(|_| {});
        let t0 = Instant::now();
        center.pane_alert(
            t0,
            Input::Output {
                tail: Some("Password: "),
            },
            None,
            false,
        );
        center.pane_alert(t0 + Duration::from_secs(2), Input::Tick, None, false);
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].notification.title, "Waiting for input");
        assert_addressed(&delivered[0].notification, "pane-x");
    }

    #[test]
    fn emit_site_waiting_for_input_generic_banner_is_addressed() {
        use crate::pane_alert::Input;
        let mut center = unfocused_center(|c| c.relevance.enabled = false);
        let t0 = Instant::now();
        center.pane_alert(
            t0,
            Input::Output {
                tail: Some("Password: "),
            },
            None,
            false,
        );
        center.pane_alert(t0 + Duration::from_secs(2), Input::Tick, None, false);
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_addressed(&delivered[0].notification, "pane-x");
    }

    #[test]
    fn emit_site_bell_pane_alert_banner_is_addressed() {
        let mut center = unfocused_center(|_| {});
        center.pane_alert(Instant::now(), crate::pane_alert::Input::Bell, None, false);
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].notification.body, "Bell");
        assert_addressed(&delivered[0].notification, "pane-x");
    }

    #[test]
    fn emit_site_long_command_pane_alert_without_a_card_is_addressed() {
        let mut center = unfocused_center(|_| {});
        center.pane_alert_notice(
            Instant::now(),
            crate::pane_alert::Input::CommandFinished { qualifies: true },
            Some("make · done"),
            None,
            false,
        );
        let delivered = center.recent(10);
        assert_eq!(delivered.len(), 1);
        assert_addressed(&delivered[0].notification, "pane-x");
    }

    #[test]
    fn the_focus_action_stays_a_knob() {
        let mut center = unfocused_center(|c| c.relevance.focus_action = false);
        center.pane_alert(Instant::now(), crate::pane_alert::Input::Bell, None, false);
        let n = &center.recent(1)[0].notification;
        assert_eq!(n.group.as_deref(), Some("pane-x"));
        assert!(n.actions.is_empty());
    }
}
