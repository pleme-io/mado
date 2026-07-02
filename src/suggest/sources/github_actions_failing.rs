//! `github-actions-failing` — failed CI runs in the current repo, surfaced as
//! "go look at this red build" suggestions. Local CLI, no auth beyond `gh
//! auth`. Enter spawns a session and tails the failed logs via `gh run view
//! <id> --log-failed`.
//!
//! Live wiring: `gh run list --status=failure --json
//! databaseId,displayTitle,workflowName,headBranch --limit N`. Note `gh run
//! list` is cwd-repo-scoped by default — it reports the runs of whatever repo
//! mado is sitting in; the optional `repos` param (comma-separated
//! `owner/name` list) widens the poll to a fixed fleet via `--repo`. A red
//! build on `main`/`master` is fleet-blocking → Critical; other branches stay
//! High. Honesty contract: a failed/unauthed `gh` run is `Unavailable(Error)`
//! (in fleet mode: only if EVERY repo's run failed) — only an OBSERVED run
//! output is `Fetched` (so a network blip never reads as "CI is green").

use crate::suggest::core::{Rank, SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct GithubActionsFailingSource;

impl SuggestionSource for GithubActionsFailingSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GithubActionsFailing
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let limit = cfg.max_items.max(1).to_string();
        let Some(repos) = cfg.param("repos") else {
            // Default: cwd-repo-scoped, today's behavior.
            let Some(out) = env.run(&run_list_cmd(env, &limit, None)) else {
                return PollOutcome::error();
            };
            return PollOutcome::Fetched(parse(&out, env, cfg));
        };
        // Fleet mode: poll each named repo and merge. The upstream counts as
        // observed if at least ONE repo answered; all-failed = Error (keep
        // last rows) so a gh outage never reads as "every repo green".
        let names: Vec<&str> = repos
            .split(',')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .collect();
        if names.is_empty() {
            // A present-but-empty `repos` param is a config mistake, not a
            // fetch failure.
            return PollOutcome::unconfigured();
        }
        let mut merged = Vec::new();
        let mut observed = false;
        for repo in names {
            if let Some(out) = env.run(&run_list_cmd(env, &limit, Some(repo))) {
                observed = true;
                merged.extend(parse(&out, env, cfg));
            }
        }
        if observed {
            PollOutcome::Fetched(merged)
        } else {
            PollOutcome::error()
        }
    }
}

/// Build the `gh run list` invocation — cwd-repo-scoped by default,
/// `--repo`-scoped in fleet mode.
fn run_list_cmd(env: &dyn SuggestionEnvironment, limit: &str, repo: Option<&str>) -> Cmd {
    let mut c = Cmd::new("gh").arg("run").arg("list");
    if let Some(r) = repo {
        c = c.arg("--repo").arg(r);
    }
    c = c
        .arg("--status=failure")
        .arg("--json")
        .arg("databaseId,displayTitle,workflowName,headBranch")
        .arg("--limit")
        .arg(limit);
    // A Dock-launched mado carries no shell env, so the sops-rendered token
    // authenticates gh. The mock key is unchanged (envs are excluded from
    // Cmd::key).
    if let Some(tok) = env.secret("github/token") {
        c = c.env("GH_TOKEN", tok);
    }
    c
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
            // A red build on the default branch blocks everyone → Critical;
            // a feature-branch failure keeps the should-look-soon High tier.
            let s = Suggestion::new(SourceKind::GithubActionsFailing, &key, title, spawn);
            let s = if run.head_branch == "main" || run.head_branch == "master" {
                s.ranked(Rank::critical())
            } else {
                s.urgent(Urgency::High)
            };
            Some(s.detail(run.head_branch))
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
        let PollOutcome::Fetched(out) = GithubActionsFailingSource.poll(&env(), &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2);
        let first = out.iter().find(|s| s.title.contains("fix the parser")).unwrap();
        assert!(first.title.contains("CI failed:"));
        assert_eq!(first.detail.as_deref(), Some("main"));
        // A red build on the default branch blocks everyone → Critical.
        assert_eq!(first.urgency, Urgency::Critical);
        // `gh run list` is cwd-repo-scoped → spawn into the code root.
        assert_eq!(first.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(
            first.spawn.initial_command(),
            Some("gh run view 98765 --log-failed")
        );
        let second = out.iter().find(|s| s.title.contains("bump deps")).unwrap();
        assert!(second.title.contains("release failed:"));
        assert_eq!(second.detail.as_deref(), Some("feat/x"));
        // A feature-branch failure keeps the High tier.
        assert_eq!(second.urgency, Urgency::High);
        assert!(
            first.rank_key() > second.rank_key(),
            "the default-branch failure must rank above the feature-branch one"
        );
    }

    #[test]
    fn repos_param_polls_each_repo_and_merges() {
        // Fleet mode: one repo answers, the other's run fails — at least one
        // observation → Fetched(merged), never a wipe.
        let mut cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        cfg.params.insert(
            "repos".to_string(),
            "pleme-io/mado,pleme-io/tear".to_string(),
        );
        let env = MockEnvironment::new().roots("/code", "/home/op").cmd(
            "gh run list --repo pleme-io/mado --status=failure --json databaseId,displayTitle,workflowName,headBranch --limit 5",
            FIXTURE,
        );
        let PollOutcome::Fetched(out) = GithubActionsFailingSource.poll(&env, &cfg) else {
            panic!("one observed repo is enough for Fetched");
        };
        assert_eq!(out.len(), 2, "the answering repo's runs are merged");
        // EVERY repo's run failing → Error (the fleet was not observed).
        let dead = MockEnvironment::new();
        assert_eq!(
            GithubActionsFailingSource.poll(&dead, &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (gh missing/unauthed/
        // failed) → Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        assert_eq!(
            GithubActionsFailingSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
        // A present-but-empty `repos` param is a config mistake, not a fetch
        // failure → Unconfigured.
        let mut cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        cfg.params.insert("repos".to_string(), " , ".to_string());
        assert_eq!(
            GithubActionsFailingSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::unconfigured()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        let cfg = SourceConfig::for_kind(SourceKind::GithubActionsFailing);
        assert!(parse("not json", &MockEnvironment::new(), &cfg).is_empty());
    }
}
