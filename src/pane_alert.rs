use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{AlertDelivery, PaneAlertsConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertReason {
    WaitingForInput,
    LongCommand,
    Idle,
    Bell,
}

impl AlertReason {
    pub const ALL: [Self; 4] = [
        Self::WaitingForInput,
        Self::LongCommand,
        Self::Idle,
        Self::Bell,
    ];

    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::WaitingForInput => "waiting_for_input",
            Self::LongCommand => "long_command",
            Self::Idle => "idle",
            Self::Bell => "bell",
        }
    }

    #[must_use]
    fn rank(self) -> u8 {
        match self {
            Self::WaitingForInput => 3,
            Self::LongCommand => 2,
            Self::Idle => 1,
            Self::Bell => 0,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::WaitingForInput => 0,
            Self::LongCommand => 1,
            Self::Idle => 2,
            Self::Bell => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptKind {
    ClaudeInput,
    ClaudePermission,
    Confirm,
    Password,
}

impl PromptKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeInput => "Claude Code is waiting for input",
            Self::ClaudePermission => "Claude Code is asking for permission",
            Self::Confirm => "Waiting for a yes/no answer",
            Self::Password => "Waiting for a password",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Tail,
    LastLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needle {
    Contains(&'static str),
    ContainsCi(&'static str),
    EndsWith(&'static str),
    LineStartsWith(&'static str),
    RuleThenLineStartsWith(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct PromptShape {
    kind: PromptKind,
    scope: Scope,
    all: &'static [Needle],
    any: &'static [Needle],
    none: &'static [Needle],
}

const PROMPT_SHAPES: &[PromptShape] = &[
    PromptShape {
        kind: PromptKind::ClaudePermission,
        scope: Scope::Tail,
        all: &[
            Needle::Contains("Do you want to"),
            Needle::LineStartsWith("❯ 1."),
        ],
        any: &[],
        none: &[],
    },
    PromptShape {
        kind: PromptKind::ClaudeInput,
        scope: Scope::Tail,
        all: &[Needle::RuleThenLineStartsWith("❯")],
        any: &[],
        none: &[Needle::ContainsCi("esc to interrupt")],
    },
    PromptShape {
        kind: PromptKind::Confirm,
        scope: Scope::LastLine,
        all: &[],
        any: &[
            Needle::ContainsCi("(y/n)"),
            Needle::ContainsCi("[y/n]"),
            Needle::ContainsCi("(yes/no)"),
            Needle::ContainsCi("[yes/no]"),
        ],
        none: &[],
    },
    PromptShape {
        kind: PromptKind::Password,
        scope: Scope::LastLine,
        all: &[Needle::EndsWith(":")],
        any: &[
            Needle::ContainsCi("password"),
            Needle::ContainsCi("passphrase"),
        ],
        none: &[],
    },
];

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 8 && t.chars().all(|c| c == '─' || c == '━')
}

impl Needle {
    fn hits(self, lines: &[&str]) -> bool {
        match self {
            Self::Contains(s) => lines.iter().any(|l| l.contains(s)),
            Self::ContainsCi(s) => {
                let s = s.to_lowercase();
                lines.iter().any(|l| l.to_lowercase().contains(&s))
            }
            Self::EndsWith(s) => lines.iter().any(|l| l.trim_end().ends_with(s)),
            Self::LineStartsWith(s) => lines.iter().any(|l| l.trim_start().starts_with(s)),
            Self::RuleThenLineStartsWith(s) => lines
                .windows(2)
                .any(|w| is_rule(w[0]) && w[1].trim_start().starts_with(s)),
        }
    }
}

impl PromptShape {
    fn matches(&self, tail: &[&str], last: &[&str]) -> bool {
        let lines = match self.scope {
            Scope::Tail => tail,
            Scope::LastLine => last,
        };
        !lines.is_empty()
            && self.all.iter().all(|n| n.hits(lines))
            && (self.any.is_empty() || self.any.iter().any(|n| n.hits(lines)))
            && !self.none.iter().any(|n| n.hits(lines))
    }
}

#[must_use]
pub fn match_prompt(tail: &str) -> Option<PromptKind> {
    let lines: Vec<&str> = tail.lines().collect();
    let last: Vec<&str> = lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .copied()
        .into_iter()
        .collect();
    PROMPT_SHAPES
        .iter()
        .find(|shape| shape.matches(&lines, &last))
        .map(|shape| shape.kind)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub reason: AlertReason,
    pub prompt: Option<PromptKind>,
    pub detail: Option<String>,
    pub raised_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Raised {
        reason: AlertReason,
        delivery: AlertDelivery,
    },
    Updated,
    Cleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input<'a> {
    Output { tail: Option<&'a str> },
    CommandFinished { qualifies: bool },
    Bell,
    Focus(bool),
    Tick,
}

#[derive(Debug, Clone)]
pub struct Detector {
    cfg: PaneAlertsConfig,
    focused: bool,
    alert: Option<Alert>,
    flashed: [bool; 4],
    last_flash: Option<Instant>,
    last_output: Option<Instant>,
    burst_start: Option<Instant>,
    burst_spent: bool,
    prompt: Option<(PromptKind, Instant)>,
    prompt_reported: bool,
}

const BURST_GAP: Duration = Duration::from_secs(5);

pub const SCREEN_TAIL_ROWS: usize = 24;

impl Detector {
    #[must_use]
    pub fn new(cfg: &PaneAlertsConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            focused: true,
            alert: None,
            flashed: [false; 4],
            last_flash: None,
            last_output: None,
            burst_start: None,
            burst_spent: false,
            prompt: None,
            prompt_reported: false,
        }
    }

    pub fn reconfigure(&mut self, cfg: &PaneAlertsConfig) {
        self.cfg = cfg.clone();
        if !cfg.enabled {
            self.alert = None;
        }
    }

    #[must_use]
    pub fn config(&self) -> &PaneAlertsConfig {
        &self.cfg
    }

    #[must_use]
    pub fn alert(&self) -> Option<&Alert> {
        self.alert.as_ref()
    }

    pub fn set_detail(&mut self, detail: String) {
        if let Some(a) = self.alert.as_mut() {
            a.detail = Some(detail);
        }
    }

    pub fn step(&mut self, now: Instant, input: Input<'_>) -> Option<Outcome> {
        if let Input::Focus(focused) = input {
            return self.on_focus(now, focused);
        }
        if !self.cfg.enabled {
            return None;
        }
        match input {
            Input::Output { tail } => {
                self.on_output(now, tail);
                self.poll(now)
            }
            Input::CommandFinished { qualifies } if qualifies && !self.focused => {
                self.raise(now, AlertReason::LongCommand, None)
            }
            Input::Bell if !self.focused => self.raise(now, AlertReason::Bell, None),
            Input::Tick => self.poll(now),
            Input::CommandFinished { .. } | Input::Bell | Input::Focus(_) => None,
        }
    }

    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        if !self.cfg.enabled || self.focused {
            return None;
        }
        let prompt = self
            .prompt
            .filter(|_| !self.prompt_reported)
            .map(|(_, since)| since + self.settle());
        let idle = self.idle_due_at();
        match (prompt, idle) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn settle(&self) -> Duration {
        Duration::from_millis(self.cfg.prompt_settle_ms)
    }

    fn quiet(&self) -> Duration {
        Duration::from_secs(self.cfg.idle_quiet_secs)
    }

    fn idle_due_at(&self) -> Option<Instant> {
        let (start, last) = (self.burst_start?, self.last_output?);
        let sustained = last.saturating_duration_since(start)
            >= Duration::from_secs(self.cfg.idle_sustain_secs);
        (sustained && !self.burst_spent).then(|| last + self.quiet())
    }

    fn on_focus(&mut self, now: Instant, focused: bool) -> Option<Outcome> {
        let was = std::mem::replace(&mut self.focused, focused);
        if focused {
            self.flashed = [false; 4];
            return self.alert.take().map(|_| Outcome::Cleared);
        }
        if was {
            if self.prompt.is_some() {
                self.prompt_reported = true;
            }
            if self
                .last_output
                .is_some_and(|t| now.saturating_duration_since(t) >= self.quiet())
            {
                self.burst_spent = true;
            }
        }
        None
    }

    fn on_output(&mut self, now: Instant, tail: Option<&str>) {
        let fresh_burst = self
            .last_output
            .is_none_or(|t| now.saturating_duration_since(t) > BURST_GAP);
        if fresh_burst {
            self.burst_start = Some(now);
            self.burst_spent = false;
        }
        self.last_output = Some(now);
        if let Some(tail) = tail {
            let seen = match_prompt(tail);
            if seen != self.prompt.map(|(k, _)| k) {
                self.prompt = seen.map(|k| (k, now));
                self.prompt_reported = self.focused;
            }
        }
    }

    fn poll(&mut self, now: Instant) -> Option<Outcome> {
        if self.focused {
            return None;
        }
        if let Some((kind, since)) = self.prompt
            && !self.prompt_reported
            && now.saturating_duration_since(since) >= self.settle()
        {
            self.prompt_reported = true;
            return self.raise(now, AlertReason::WaitingForInput, Some(kind));
        }
        if self.idle_due_at().is_some_and(|due| now >= due) {
            self.burst_spent = true;
            return self.raise(now, AlertReason::Idle, None);
        }
        None
    }

    fn raise(
        &mut self,
        now: Instant,
        reason: AlertReason,
        prompt: Option<PromptKind>,
    ) -> Option<Outcome> {
        let delivery = self.cfg.reasons.delivery(reason);
        if delivery == AlertDelivery::Off {
            return None;
        }
        let cooled = self.last_flash.is_none_or(|t| {
            now.saturating_duration_since(t) >= Duration::from_secs(self.cfg.cooldown_secs)
        });
        let changed = match self.alert.as_mut() {
            None => {
                self.flashed = [false; 4];
                self.alert = Some(Alert {
                    reason,
                    prompt,
                    detail: None,
                    raised_at: now,
                });
                true
            }
            Some(a) if reason.rank() > a.reason.rank() => {
                a.reason = reason;
                a.prompt = prompt;
                a.detail = None;
                true
            }
            Some(_) => false,
        };
        if cooled && !self.flashed[reason.index()] {
            self.flashed[reason.index()] = true;
            self.last_flash = Some(now);
            return Some(Outcome::Raised { reason, delivery });
        }
        changed.then_some(Outcome::Updated)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AlertSnapshot {
    pub reason: &'static str,
    pub prompt: Option<&'static str>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub age_ms: u64,
}

pub static LIVE: Mutex<Option<(Alert, Option<String>)>> = Mutex::new(None);

pub fn publish(alert: Option<&Alert>, title: Option<&str>) {
    if let Ok(mut slot) = LIVE.lock() {
        *slot = alert.map(|a| (a.clone(), title.map(str::to_owned)));
    }
}

#[must_use]
pub fn snapshot(now: Instant) -> Option<AlertSnapshot> {
    let slot = LIVE.lock().ok()?;
    let (alert, title) = slot.as_ref()?;
    Some(AlertSnapshot {
        reason: alert.reason.slug(),
        prompt: alert.prompt.map(PromptKind::label),
        title: title.clone(),
        detail: alert.detail.clone(),
        age_ms: u64::try_from(now.saturating_duration_since(alert.raised_at).as_millis())
            .unwrap_or(u64::MAX),
    })
}

#[must_use]
pub fn badged_title(cfg: &PaneAlertsConfig, base: &str, alerting: bool) -> String {
    if alerting {
        let mut s = cfg.badge.clone();
        s.push_str(base);
        s
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_IDLE: &str = "⏺ Done.\n\n✻ Waiting for 4 background agents to finish\n\n────────────────────────────\n❯ \n────────────────────────────\n  opus │ $3 │ 90%\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
    const CLAUDE_BUSY: &str = "⏺ Reading files\n\n✻ Thinking… (esc to interrupt · 12s)\n\n────────────────────────────\n❯ \n────────────────────────────\n  opus │ $3 │ 90%\n";
    const CLAUDE_PERMISSION: &str = " Bash command\n   cargo test\n Do you want to proceed?\n ❯ 1. Yes\n   2. No, and tell Claude what to do differently (esc)\n";

    fn cfg() -> PaneAlertsConfig {
        PaneAlertsConfig::prescribed()
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn away(t0: Instant) -> Detector {
        let mut d = Detector::new(&cfg());
        assert_eq!(d.step(t0, Input::Focus(false)), None);
        d
    }

    fn raised(reason: AlertReason, delivery: AlertDelivery) -> Option<Outcome> {
        Some(Outcome::Raised { reason, delivery })
    }

    #[test]
    fn the_prompt_table_names_each_shape() {
        assert_eq!(match_prompt(CLAUDE_IDLE), Some(PromptKind::ClaudeInput));
        assert_eq!(match_prompt(CLAUDE_BUSY), None);
        assert_eq!(
            match_prompt(CLAUDE_PERMISSION),
            Some(PromptKind::ClaudePermission)
        );
        assert_eq!(
            match_prompt("rm: remove 3 files? (y/n) "),
            Some(PromptKind::Confirm)
        );
        assert_eq!(
            match_prompt("Proceed with install? [Y/n]"),
            Some(PromptKind::Confirm)
        );
        assert_eq!(
            match_prompt("[sudo] password for luis:"),
            Some(PromptKind::Password)
        );
        assert_eq!(
            match_prompt("Enter passphrase for key '/x/id_ed25519':"),
            Some(PromptKind::Password)
        );
        assert_eq!(match_prompt("$ ls\nsrc  target\n$ "), None);
        assert_eq!(
            match_prompt("password reset done\n$ "),
            None,
            "a password word above the prompt line is not a password prompt"
        );
    }

    #[test]
    fn a_settled_prompt_raises_waiting_for_input() {
        let t0 = Instant::now();
        let mut d = away(t0);
        assert_eq!(
            d.step(
                t0,
                Input::Output {
                    tail: Some(CLAUDE_BUSY)
                }
            ),
            None
        );
        assert_eq!(
            d.step(
                at(t0, 100),
                Input::Output {
                    tail: Some(CLAUDE_IDLE)
                }
            ),
            None,
            "a prompt that has not settled does not alert"
        );
        assert_eq!(d.next_deadline(), Some(at(t0, 850)));
        assert_eq!(d.step(at(t0, 500), Input::Tick), None);
        assert_eq!(
            d.step(at(t0, 900), Input::Tick),
            raised(AlertReason::WaitingForInput, AlertDelivery::Notify)
        );
        assert_eq!(
            d.alert().and_then(|a| a.prompt),
            Some(PromptKind::ClaudeInput)
        );
        assert_eq!(d.step(at(t0, 2_000), Input::Tick), None, "once per prompt");
    }

    #[test]
    fn a_prompt_that_flickers_away_before_settling_never_alerts() {
        let t0 = Instant::now();
        let mut d = away(t0);
        d.step(
            t0,
            Input::Output {
                tail: Some(CLAUDE_IDLE),
            },
        );
        d.step(
            at(t0, 300),
            Input::Output {
                tail: Some(CLAUDE_BUSY),
            },
        );
        assert_eq!(d.step(at(t0, 2_000), Input::Tick), None);
        assert!(d.alert().is_none());
    }

    #[test]
    fn a_prompt_already_on_screen_when_focus_leaves_does_not_alert() {
        let t0 = Instant::now();
        let mut d = Detector::new(&cfg());
        d.step(
            t0,
            Input::Output {
                tail: Some(CLAUDE_IDLE),
            },
        );
        d.step(at(t0, 2_000), Input::Tick);
        d.step(at(t0, 3_000), Input::Focus(false));
        d.step(
            at(t0, 3_100),
            Input::Output {
                tail: Some(CLAUDE_IDLE),
            },
        );
        assert_eq!(d.step(at(t0, 9_000), Input::Tick), None);
    }

    #[test]
    fn a_long_command_raises_with_its_configured_delivery() {
        let t0 = Instant::now();
        let mut d = away(t0);
        assert_eq!(
            d.step(t0, Input::CommandFinished { qualifies: false }),
            None
        );
        assert_eq!(
            d.step(t0, Input::CommandFinished { qualifies: true }),
            raised(AlertReason::LongCommand, AlertDelivery::Notify)
        );
    }

    #[test]
    fn quiet_after_sustained_output_raises_idle() {
        let t0 = Instant::now();
        let mut d = away(t0);
        for s in 0..=12 {
            assert_eq!(
                d.step(at(t0, s * 1_000), Input::Output { tail: None }),
                None
            );
        }
        assert_eq!(d.next_deadline(), Some(at(t0, 42_000)));
        assert_eq!(d.step(at(t0, 30_000), Input::Tick), None);
        assert_eq!(
            d.step(at(t0, 42_000), Input::Tick),
            raised(AlertReason::Idle, AlertDelivery::Badge)
        );
        assert_eq!(d.step(at(t0, 90_000), Input::Tick), None, "once per burst");
    }

    #[test]
    fn a_short_burst_going_quiet_is_not_idle() {
        let t0 = Instant::now();
        let mut d = away(t0);
        for s in 0..3 {
            d.step(at(t0, s * 1_000), Input::Output { tail: None });
        }
        assert_eq!(d.next_deadline(), None);
        assert_eq!(d.step(at(t0, 120_000), Input::Tick), None);
    }

    #[test]
    fn output_with_gaps_restarts_the_burst() {
        let t0 = Instant::now();
        let mut d = away(t0);
        for s in [0_u64, 7, 14, 21] {
            d.step(at(t0, s * 1_000), Input::Output { tail: None });
        }
        assert_eq!(d.step(at(t0, 120_000), Input::Tick), None);
    }

    #[test]
    fn a_bell_raises_a_badge() {
        let t0 = Instant::now();
        let mut d = away(t0);
        assert_eq!(
            d.step(t0, Input::Bell),
            raised(AlertReason::Bell, AlertDelivery::Badge)
        );
    }

    #[test]
    fn nothing_alerts_while_the_pane_is_focused() {
        let t0 = Instant::now();
        let mut d = Detector::new(&cfg());
        assert_eq!(d.step(t0, Input::Bell), None);
        assert_eq!(d.step(t0, Input::CommandFinished { qualifies: true }), None);
        for s in 0..=12 {
            d.step(
                at(t0, s * 1_000),
                Input::Output {
                    tail: Some(CLAUDE_BUSY),
                },
            );
        }
        d.step(
            at(t0, 12_100),
            Input::Output {
                tail: Some(CLAUDE_IDLE),
            },
        );
        assert_eq!(d.next_deadline(), None);
        assert_eq!(d.step(at(t0, 60_000), Input::Tick), None);
        assert!(d.alert().is_none());
    }

    #[test]
    fn focusing_the_pane_clears_the_alert() {
        let t0 = Instant::now();
        let mut d = away(t0);
        d.step(t0, Input::Bell);
        assert!(d.alert().is_some());
        assert_eq!(
            d.step(at(t0, 1_000), Input::Focus(true)),
            Some(Outcome::Cleared)
        );
        assert!(d.alert().is_none());
        assert_eq!(d.step(at(t0, 2_000), Input::Focus(true)), None);
    }

    #[test]
    fn a_chatty_pane_flashes_once_per_cooldown() {
        let t0 = Instant::now();
        let mut d = away(t0);
        let mut raised_count = 0;
        for i in 0..50 {
            if let Some(Outcome::Raised { .. }) = d.step(at(t0, i * 100), Input::Bell) {
                raised_count += 1;
            }
        }
        assert_eq!(raised_count, 1, "fifty bells in five seconds flash once");
    }

    #[test]
    fn a_higher_reason_escalates_and_flashes_after_the_cooldown_only() {
        let t0 = Instant::now();
        let mut d = away(t0);
        d.step(t0, Input::Bell);
        assert_eq!(
            d.step(at(t0, 2_000), Input::CommandFinished { qualifies: true }),
            Some(Outcome::Updated),
            "inside the cooldown a new reason updates the badge silently"
        );
        assert_eq!(d.alert().map(|a| a.reason), Some(AlertReason::LongCommand));
        assert_eq!(
            d.step(at(t0, 12_000), Input::CommandFinished { qualifies: true }),
            raised(AlertReason::LongCommand, AlertDelivery::Notify),
            "a reason held back by the cooldown is delivered once it passes"
        );
        assert_eq!(
            d.step(at(t0, 25_000), Input::CommandFinished { qualifies: true }),
            None,
            "a reason already delivered for this alert does not flash again"
        );
        d.step(
            at(t0, 25_500),
            Input::Output {
                tail: Some(CLAUDE_IDLE),
            },
        );
        assert_eq!(
            d.step(at(t0, 26_500), Input::Tick),
            raised(AlertReason::WaitingForInput, AlertDelivery::Notify)
        );
    }

    #[test]
    fn the_cooldown_spans_a_focus_round_trip() {
        let t0 = Instant::now();
        let mut d = away(t0);
        d.step(t0, Input::Bell);
        d.step(at(t0, 500), Input::Focus(true));
        d.step(at(t0, 600), Input::Focus(false));
        assert_eq!(d.step(at(t0, 700), Input::Bell), Some(Outcome::Updated));
        assert!(d.alert().is_some(), "the badge still shows");
    }

    #[test]
    fn an_off_reason_never_alerts() {
        let t0 = Instant::now();
        let mut c = cfg();
        c.reasons.bell = AlertDelivery::Off;
        let mut d = Detector::new(&c);
        d.step(t0, Input::Focus(false));
        assert_eq!(d.step(t0, Input::Bell), None);
        assert!(d.alert().is_none());
    }

    #[test]
    fn a_disabled_detector_is_inert() {
        let t0 = Instant::now();
        let mut d = Detector::new(&PaneAlertsConfig::bare());
        d.step(t0, Input::Focus(false));
        assert_eq!(d.step(t0, Input::Bell), None);
        assert_eq!(d.step(t0, Input::CommandFinished { qualifies: true }), None);
        assert_eq!(d.next_deadline(), None);
    }

    #[test]
    fn every_reason_has_a_distinct_slot_and_slug() {
        let mut idx: Vec<usize> = AlertReason::ALL.iter().map(|r| r.index()).collect();
        idx.sort_unstable();
        idx.dedup();
        assert_eq!(idx.len(), AlertReason::ALL.len());
        let mut slugs: Vec<&str> = AlertReason::ALL.iter().map(|r| r.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), AlertReason::ALL.len());
    }

    #[test]
    fn the_badge_prefixes_the_title_only_while_alerting() {
        let c = cfg();
        assert_eq!(badged_title(&c, "agent", true), "● agent");
        assert_eq!(badged_title(&c, "agent", false), "agent");
    }
}
