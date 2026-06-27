//! `github-actions-failing` — failed CI runs in the current repo, surfaced as
//! "go look at this red build" suggestions. Local CLI, no auth beyond `gh
//! auth`. Enter spawns a session and tails the failed logs via `gh run view
//! <id> --log-failed`.
//!
//! Live wiring: `gh run list --status=failure --json
//! databaseId,displayTitle,workflowName,headBranch --limit N`. Note `gh run
//! list` is cwd-repo-scoped — it reports the runs of whatever repo mado is
//! sitting in. `gh` not installed / unauthed / no JSON → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct GithubActionsFailingSource;

impl SuggestionSource for GithubActionsFailingSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GithubActionsFailing
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let limit = cfg.max_items.max(1).to_string();
        let cmd = Cmd::new("gh")
            .arg("run")
            .arg("list")
            .arg("--status=failure")
            .arg("--json")
            .arg("databaseId,displayTitle,workflowName,headBranch")
            .arg("--limit")
            .arg(limit);
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        parse(&out, env, cfg)
    }
}

/// Parse `gh run list --json …` output into suggestions. Pure — the unit the
/// source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<RunRow>>(json) else {
        return Vec::new();
    };
    let cwd = env.code_root();
    rows.into_iter()
        .take(cfg.max_items.max(1))
        .filter_map(|run| {
            let spawn = {
                let mut view = String::from("gh run view ");
                view.push_str(&run.database_id.to_string());
                view.push_str(" --log-failed");
                SpawnSpec::new(cwd.clone(), "\u{1F6A8} CI")?.with_command(view) // 🚨
            };
            let mut title = run.workflow_name.clone();
            title.push_str(" failed: ");
            let display: String = run.display_title.chars().take(120).collect();
            title.push_str(display.trim());
            let key = run.database_id.to_string();
            Some(
                Suggestion::new(SourceKind::GithubActionsFailing, &key, title, spawn)
                    .detail(run.head_branch)
                    .urgent(Urgency::High),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct RunRow {
    #[serde(default, rename = "databaseId")]
    database_id: u64,
    #[serde(default, rename = "displayTitle")]
    display_title: String,
    #[serde(default, rename = "workflowName")]
    workflow_name: String,
    #[serde(default, rename = "headBranch")]
    head_branch: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"databaseId":98765,"displayTitle":"fix the parser","workflowName":"CI","headBranch":"main"},
        {"databaseId":98766,"displayTitle":"bump deps","workflowName":"release","headBranch":"feat/x"}
    ]"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new().roots("/code", "/home/op").cmd(
            "gh run list --status=failure --json databaseId,displayTitle,workflowName,headBranch --limit 5",
            FIXTURE,
        )
    }

    #[test]
    fn produces_a_suggestion_per_failing_run_with_log_command() {
        let cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        let out = GithubActionsFailingSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 2);
        let first = out.iter().find(|s| s.title.contains("fix the parser")).unwrap();
        assert!(first.title.contains("CI failed:"));
        assert_eq!(first.detail.as_deref(), Some("main"));
        assert_eq!(first.urgency, Urgency::High);
        // `gh run list` is cwd-repo-scoped → spawn into the code root.
        assert_eq!(first.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(
            first.spawn.initial_command(),
            Some("gh run view 98765 --log-failed")
        );
        let second = out.iter().find(|s| s.title.contains("bump deps")).unwrap();
        assert!(second.title.contains("release failed:"));
        assert_eq!(second.detail.as_deref(), Some("feat/x"));
    }

    #[test]
    fn no_gh_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        assert!(GithubActionsFailingSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        let cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        assert!(parse("not json", &MockEnvironment::new(), &cfg).is_empty());
    }
}
