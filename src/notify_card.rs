use crate::config::{CommandRank, RelevanceConfig};
use crate::ux::CommandCompletion;
use crate::ux::side_effects::humanize_duration;
use tsuuchi::{Notification, NotificationAction};

pub const CATEGORY_COMMAND_FINISHED: &str = "command-finished";
pub const CATEGORY_WAITING_FOR_INPUT: &str = "waiting-for-input";
pub const FOCUS_ACTION_ID: &str = "focus_pane";

#[derive(Debug, Clone, Copy)]
pub struct PaneContext<'a> {
    pub group: &'a str,
    pub title: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

impl PaneContext<'_> {
    fn subtitle(&self) -> Option<&str> {
        [self.title, self.cwd]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
    }
}

#[must_use]
pub fn one_line(s: &str) -> Option<String> {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!joined.is_empty()).then_some(joined)
}

#[must_use]
pub fn truncate(s: &str, max_chars: usize) -> String {
    let line = one_line(s).unwrap_or_default();
    let max = max_chars.max(1);
    if line.chars().count() <= max {
        return line;
    }
    let mut out: String = line.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[must_use]
pub fn stable_id(group: &str) -> String {
    format!("pane-{group}")
}

fn finish(
    n: Notification,
    relevance: &RelevanceConfig,
    ctx: &PaneContext<'_>,
    category: &str,
) -> Notification {
    let n = n
        .category(category)
        .group(ctx.group)
        .id(stable_id(ctx.group));
    let n = match ctx.subtitle() {
        Some(s) => n.subtitle(s),
        None => n,
    };
    if relevance.focus_action {
        n.action(NotificationAction::foreground(FOCUS_ACTION_ID, "Focus"))
    } else {
        n
    }
}

#[must_use]
pub fn address(n: &Notification, group: &str, focus_action: bool) -> Notification {
    let mut out = n.clone();
    out.group = Some(group.to_owned());
    if focus_action && !out.actions.iter().any(|a| a.id == FOCUS_ACTION_ID) {
        out.actions
            .push(NotificationAction::foreground(FOCUS_ACTION_ID, "Focus"));
    }
    out
}

#[must_use]
pub fn command_card(
    cc: &CommandCompletion,
    rank: CommandRank,
    relevance: &RelevanceConfig,
    ctx: &PaneContext<'_>,
) -> Notification {
    let title = match cc.command.as_deref() {
        Some(c) => truncate(c, relevance.title_max_chars),
        None if cc.succeeded() => "Command finished".to_owned(),
        None => "Command failed".to_owned(),
    };
    let mark = if cc.succeeded() { "✅" } else { "❌" };
    let body = format!(
        "{mark} exit {} · {}",
        cc.exit_code,
        humanize_duration(cc.duration_ms)
    );
    let n = Notification::new(title, body).urgency(relevance.command_urgency(rank).into());
    finish(n, relevance, ctx, CATEGORY_COMMAND_FINISHED)
}

#[must_use]
pub fn waiting_card(
    awaited: &str,
    relevance: &RelevanceConfig,
    ctx: &PaneContext<'_>,
) -> Notification {
    let n = Notification::new("Waiting for input", format!("⏳ {awaited}"))
        .urgency(relevance.waiting_for_input.into());
    finish(n, relevance, ctx, CATEGORY_WAITING_FOR_INPUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsuuchi::Urgency;

    fn cc(exit_code: i32, duration_ms: u64, command: Option<&str>) -> CommandCompletion {
        CommandCompletion {
            exit_code,
            duration_ms,
            used_alt_screen: false,
            command: command.map(str::to_owned),
        }
    }

    fn ctx<'a>() -> PaneContext<'a> {
        PaneContext {
            group: "ab12",
            title: Some("build"),
            cwd: Some("/src"),
        }
    }

    fn rel() -> RelevanceConfig {
        RelevanceConfig::prescribed()
    }

    #[test]
    fn failure_card_carries_the_real_fact() {
        let n = command_card(
            &cc(1, 94_000, Some("cargo test")),
            CommandRank::Failure,
            &rel(),
            &ctx(),
        );
        assert_eq!(n.title, "cargo test");
        assert_eq!(n.subtitle.as_deref(), Some("build"));
        assert_eq!(n.body, "❌ exit 1 · 1m 34s");
        assert_eq!(n.category.as_deref(), Some("command-finished"));
        assert_eq!(n.group.as_deref(), Some("ab12"));
        assert_eq!(n.urgency, Urgency::Normal);
    }

    #[test]
    fn slow_success_card_is_low_and_green() {
        let n = command_card(
            &cc(0, 134_000, Some("make")),
            CommandRank::SlowSuccess,
            &rel(),
            &ctx(),
        );
        assert_eq!(n.body, "✅ exit 0 · 2m 14s");
        assert_eq!(n.urgency, Urgency::Low);
    }

    #[test]
    fn urgency_follows_the_config_not_a_constant() {
        let mut r = rel();
        r.failure = crate::config::NotifyUrgency::Critical;
        r.slow_success = crate::config::NotifyUrgency::Normal;
        r.waiting_for_input = crate::config::NotifyUrgency::Low;
        let f = command_card(&cc(2, 1, Some("x")), CommandRank::Failure, &r, &ctx());
        let s = command_card(&cc(0, 1, Some("x")), CommandRank::SlowSuccess, &r, &ctx());
        let w = waiting_card("Waiting for a password", &r, &ctx());
        assert_eq!(
            (f.urgency, s.urgency, w.urgency),
            (Urgency::Critical, Urgency::Normal, Urgency::Low)
        );
    }

    #[test]
    fn id_is_stable_per_pane_so_a_newer_card_replaces_the_older() {
        let a = command_card(&cc(1, 1, Some("a")), CommandRank::Failure, &rel(), &ctx());
        let b = command_card(
            &cc(0, 20_000, Some("b")),
            CommandRank::SlowSuccess,
            &rel(),
            &ctx(),
        );
        let w = waiting_card("Waiting for a password", &rel(), &ctx());
        assert_eq!(a.id, b.id);
        assert_eq!(a.id, w.id);
        assert_eq!(a.id.as_deref(), Some("pane-ab12"));
        let other = PaneContext {
            group: "cd34",
            ..ctx()
        };
        let c = command_card(&cc(1, 1, Some("a")), CommandRank::Failure, &rel(), &other);
        assert_ne!(a.id, c.id);
    }

    #[test]
    fn title_is_one_line_and_truncated() {
        let long = format!("echo {}\n  second", "x".repeat(200));
        let n = command_card(&cc(1, 1, Some(&long)), CommandRank::Failure, &rel(), &ctx());
        assert_eq!(n.title.chars().count(), 80);
        assert!(n.title.ends_with('…'));
        assert!(!n.title.contains('\n'));
        assert_eq!(truncate("a   b\tc", 10), "a b c");
        assert_eq!(truncate("abcdef", 3), "ab…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn missing_command_text_falls_back_to_the_outcome() {
        let ok = command_card(&cc(0, 1, None), CommandRank::SlowSuccess, &rel(), &ctx());
        let bad = command_card(&cc(3, 1, None), CommandRank::Failure, &rel(), &ctx());
        assert_eq!(ok.title, "Command finished");
        assert_eq!(bad.title, "Command failed");
    }

    #[test]
    fn subtitle_prefers_the_pane_title_then_the_working_dir() {
        let no_title = PaneContext {
            title: None,
            ..ctx()
        };
        let blank = PaneContext {
            title: Some("  "),
            ..ctx()
        };
        let none = PaneContext {
            title: None,
            cwd: None,
            ..ctx()
        };
        let n = |c: &PaneContext<'_>| waiting_card("x", &rel(), c).subtitle;
        assert_eq!(n(&no_title).as_deref(), Some("/src"));
        assert_eq!(n(&blank).as_deref(), Some("/src"));
        assert_eq!(n(&none), None);
    }

    #[test]
    fn waiting_card_names_what_is_awaited() {
        let n = waiting_card("Waiting for a password", &rel(), &ctx());
        assert_eq!(n.category.as_deref(), Some("waiting-for-input"));
        assert_eq!(n.body, "⏳ Waiting for a password");
    }

    #[test]
    fn focus_action_is_a_knob() {
        let on = waiting_card("x", &rel(), &ctx());
        assert_eq!(on.actions.len(), 1);
        assert_eq!(on.actions[0].id, "focus_pane");
        let mut r = rel();
        r.focus_action = false;
        assert!(waiting_card("x", &r, &ctx()).actions.is_empty());
    }
}
