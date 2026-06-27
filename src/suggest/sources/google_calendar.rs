//! `google-calendar` — your upcoming primary-calendar events, surfaced so you
//! can drop into a working session keyed to "the next thing on your schedule".
//! HTTP source: a Google Calendar OAuth token (`google/calendar-token`) gets
//! you the events list; no token / no response → graceful empty.
//!
//! Live wiring: `GET
//! https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=N`
//! with a bearer token → `{items:[{id, summary, start:{dateTime}}]}`. Each
//! event becomes a suggestion whose spawn drops you in the code root.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct GoogleCalendarSource;

impl SuggestionSource for GoogleCalendarSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GoogleCalendar
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let max = cfg.max_items.max(1);
        let token = env.secret("google/calendar-token").unwrap_or_default();
        let mut url = String::from(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=",
        );
        url.push_str(&max.to_string());
        let req = HttpReq::new(url).bearer(&token);
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        parse(&out, env, max)
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

    const URL: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=5";

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
        let out = GoogleCalendarSource.poll(&env(), &cfg);
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
    fn no_token_or_response_yields_nothing() {
        // No secret + no http fixture registered → http_get returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::GoogleCalendar);
        assert!(
            GoogleCalendarSource
                .poll(&MockEnvironment::new(), &cfg)
                .is_empty()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new(), 5).is_empty());
    }
}
