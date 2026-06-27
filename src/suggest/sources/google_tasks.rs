//! `google-tasks` — your incomplete Google Tasks (the default list), surfaced
//! as "go knock this out" suggestions. HTTP against the Tasks API with a bearer
//! token from `cofre` (secret `google/tasks-token`). Enter spawns a session in
//! your code root.
//!
//! Live wiring: `GET https://tasks.googleapis.com/tasks/v1/lists/@default/tasks
//! ?showCompleted=false&maxResults=N` with `Authorization: Bearer <token>`. No
//! token / a non-200 (unauthed) → no suggestions (graceful).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct GoogleTasksSource;

impl SuggestionSource for GoogleTasksSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GoogleTasks
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let max = cfg.max_items.max(1);
        // A missing token is not fatal: we still issue the request (bearer
        // empty) and let the unauthed non-200 collapse to an empty Vec below.
        let token = env.secret("google/tasks-token").unwrap_or_default();
        let mut url = String::from(
            "https://tasks.googleapis.com/tasks/v1/lists/@default/tasks?showCompleted=false&maxResults=",
        );
        url.push_str(&max.to_string());
        let req = HttpReq::new(url).bearer(&token);
        let Some(out) = env.http_get(&req) else {
            return Vec::new();
        };
        let mut suggestions = parse(&out, env);
        suggestions.truncate(max);
        suggestions
    }
}

/// Parse the Tasks API `{items:[…]}` body into suggestions for incomplete
/// tasks. Pure — the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(resp) = serde_json::from_str::<TasksResponse>(json) else {
        return Vec::new();
    };
    resp.items
        .into_iter()
        .filter(|t| !t.title.trim().is_empty())
        .filter_map(|t| {
            let cwd = env.code_root();
            let name = String::from("\u{2705} task"); // ✅
            let spawn = SpawnSpec::new(cwd, name)?;
            let title = t.title.trim().to_string();
            Some(
                Suggestion::new(SourceKind::GoogleTasks, &t.id, title, spawn)
                    .detail(t.due)
                    .urgent(Urgency::Low),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct TasksResponse {
    #[serde(default)]
    items: Vec<TaskRow>,
}

#[derive(serde::Deserialize, Default)]
struct TaskRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    due: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const URL: &str =
        "https://tasks.googleapis.com/tasks/v1/lists/@default/tasks?showCompleted=false&maxResults=5";

    const FIXTURE: &str = r#"{"items":[
        {"id":"abc","title":"buy milk","due":"2026-07-01"},
        {"id":"def","title":"call mom","due":""},
        {"id":"ghi","title":"","due":""}
    ]}"#;

    #[test]
    fn produces_a_suggestion_per_incomplete_task() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("google/tasks-token", "tok")
            .http(URL, FIXTURE);
        let mut cfg = SourceConfig::for_kind(SourceKind::GoogleTasks);
        cfg.max_items = 5;
        let out = GoogleTasksSource.poll(&env, &cfg);
        assert_eq!(out.len(), 2, "empty-title task excluded");
        let milk = out.iter().find(|s| s.title == "buy milk").unwrap();
        assert_eq!(milk.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(milk.urgency, Urgency::Low);
        assert_eq!(milk.detail.as_deref(), Some("2026-07-01"));
        // A task with no due date still surfaces (detail just goes empty).
        assert!(out.iter().any(|s| s.title == "call mom"));
    }

    #[test]
    fn unauthed_yields_nothing() {
        // No http fixture registered → http_get returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::GoogleTasks);
        assert!(GoogleTasksSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
