//! `google-calendar` — your upcoming primary-calendar events, surfaced so you
//! can drop into a working session keyed to "the next thing on your schedule".
//! HTTP source: a Google Calendar OAuth token (`google/calendar-token`,
//! overridable via the `secret` param) gets you the events list.
//!
//! Live wiring: `GET
//! https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=N`
//! with a bearer token → `{items:[{id, summary, start:{dateTime}}]}`. Each
//! event becomes a suggestion whose spawn drops you in the code root.
//! Honesty contract: a missing token is `Unavailable(AuthMissing)` (no request
//! is fired), a failed fetch `Unavailable(Error)` — only an OBSERVED response
//! is `Fetched` (so an auth outage never reads as "an empty calendar").

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct GoogleCalendarSource;

impl SuggestionSource for GoogleCalendarSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GoogleCalendar
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let max = cfg.max_items.max(1);
        let secret_key = cfg.param("secret").unwrap_or("google/calendar-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
        let mut url = String::from(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=",
        );
        url.push_str(&max.to_string());
        // timeMin = now, so we only surface UPCOMING events. Without it the API
        // returns the whole history (past meetings as stale suggestions).
        url.push_str("&timeMin=");
        url.push_str(&super::util::pct(&super::util::rfc3339_utc(env.now_unix())));
        let req = HttpReq::new(url).bearer(&token);
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env, max))
    }
}

/// Parse a Google Calendar `events` response into suggestions. Pure — the unit
/// the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(resp) = serde_json::from_str::<EventsResp>(json) else {
        return Vec::new();
    };
    resp.items
        .into_iter()
        .take(max)
        .filter_map(|ev| {
            let cwd = env.code_root();
            let mut name = String::from("\u{1F4C5} "); // 📅
            name.push_str(&ev.summary.chars().take(24).collect::<String>());
            let spawn = SpawnSpec::new(cwd, name)?;
            let key = ev.id;
            Some(
                Suggestion::new(SourceKind::GoogleCalendar, &key, ev.summary, spawn)
                    .detail(ev.start.date_time)
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct EventsResp {
    #[serde(default)]
    items: Vec<EventRow>,
}

#[derive(serde::Deserialize, Default)]
struct EventRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    start: Start,
}

#[derive(serde::Deserialize, Default)]
struct Start {
    #[serde(default, rename = "dateTime")]
    date_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    // The mock's default clock is 1_000_000 (1970-01-12T13:46:40Z); the URL
    // carries the pct-encoded timeMin cursor the source now appends.
    const URL: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=5&timeMin=1970-01-12T13%3A46%3A40Z";

    const FIXTURE: &str = r#"{
        "items": [
            {"id":"evt-1","summary":"Standup with the team","start":{"dateTime":"2026-06-27T09:00:00Z"}},
            {"id":"evt-2","summary":"This is a really long meeting title that exceeds the cap","start":{"dateTime":"2026-06-27T14:30:00Z"}}
        ]
    }"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("google/calendar-token", "tok-123")
            .http(URL, FIXTURE)
    }

    #[test]
    fn produces_a_suggestion_per_event() {
        let mut cfg = SourceConfig::for_kind(SourceKind::GoogleCalendar);
        cfg.max_items = 5;
        let PollOutcome::Fetched(out) = GoogleCalendarSource.poll(&env(), &cfg) else {
            panic!("an observed response is Fetched");
        };
        assert_eq!(out.len(), 2);
        let standup = out.iter().find(|s| s.title.contains("Standup")).unwrap();
        // Title is plain text — the picker prepends the source emoji.
        assert_eq!(standup.title, "Standup with the team");
        assert_eq!(standup.detail.as_deref(), Some("2026-06-27T09:00:00Z"));
        assert_eq!(standup.urgency, Urgency::Normal);
        // Spawn drops you in the code root.
        assert_eq!(standup.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No token secret → AuthMissing (needs auth, not "an empty calendar")
        // — and no request is ever fired unauthenticated.
        let cfg = SourceConfig::for_kind(SourceKind::GoogleCalendar);
        assert_eq!(
            GoogleCalendarSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::auth_missing()
        );
        // Token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("google/calendar-token", "tok");
        assert_eq!(GoogleCalendarSource.poll(&env, &cfg), PollOutcome::error());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
