//! `github-review-requested` — PRs across the fleet that are waiting on *your*
//! review, correlated to where you'd review them. Fully local-tooling (the `gh`
//! CLI), no extra credential beyond `gh auth`. Enter spawns a session in the
//! PR's repo and kicks off `gh pr checkout <n>`.
//!
//! Live wiring: `gh search prs --review-requested=@me --state=open --json
//! number,title,url,repository --limit N`. `gh`'s `--json` output is stable +
//! documented; an unauthed `gh` exits non-zero → no suggestions (graceful). A
//! review request is something blocking a teammate → High urgency.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct GithubReviewRequestedSource;

impl SuggestionSource for GithubReviewRequestedSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GithubReviewRequested
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let limit = cfg.max_items.max(1).to_string();
        let cmd = Cmd::new("gh")
            .arg("search")
            .arg("prs")
            .arg("--review-requested=@me")
            .arg("--state=open")
            .arg("--json")
            .arg("number,title,url,repository")
            .arg("--limit")
            .arg(limit);
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        parse(&out, env)
    }
}

/// Parse `gh search prs --json …` output into suggestions. Pure — the unit the
/// source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<PrRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|pr| {
            let owner = pr.repository.name_with_owner();
            let cwd = super::repo_cwd(env, &owner);
            let mut name = String::from("\u{1F50D} pr#"); // 🔍
            name.push_str(&pr.number.to_string());
            let checkout = {
                let mut c = String::from("gh pr checkout ");
                c.push_str(&pr.number.to_string());
                c
            };
            let spawn = SpawnSpec::new(cwd, name)?.with_command(checkout);
            let mut key = String::new();
            key.push_str(&owner);
            key.push('#');
            key.push_str(&pr.number.to_string());
            let mut title = String::from("pr#");
            title.push_str(&pr.number.to_string());
            title.push(' ');
            title.push_str(pr.title.trim());
            Some(
                Suggestion::new(SourceKind::GithubReviewRequested, &key, title, spawn)
                    .detail(owner)
                    .urgent(Urgency::High),
            )
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct PrRow {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    repository: Repo,
}

#[derive(serde::Deserialize, Default)]
struct Repo {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "nameWithOwner")]
    name_with_owner: String,
}

impl Repo {
    fn name_with_owner(&self) -> String {
        if self.name_with_owner.is_empty() {
            self.name.clone()
        } else {
            self.name_with_owner.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"number":1234,"title":"fix the parser","url":"https://x","repository":{"name":"mado","nameWithOwner":"pleme-io/mado"}},
        {"number":12,"title":"bump deps","url":"https://y","repository":{"name":"tear","nameWithOwner":"pleme-io/tear"}}
    ]"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado")
            .cmd(
                "gh search prs --review-requested=@me --state=open --json number,title,url,repository --limit 5",
                FIXTURE,
            )
    }

    #[test]
    fn produces_a_suggestion_per_pr_with_checkout_command() {
        let cfg = SourceConfig::for_kind(SourceKind::GithubReviewRequested);
        let out = GithubReviewRequestedSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 2);
        let mado = out.iter().find(|s| s.title.contains("pr#1234")).unwrap();
        assert!(mado.title.contains("fix the parser"));
        assert_eq!(mado.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        assert_eq!(mado.spawn.initial_command(), Some("gh pr checkout 1234"));
        assert_eq!(mado.urgency, Urgency::High);
        // The repo whose dir does not exist falls back to the code root.
        // (Match by repo detail — "pr#12" is a substring of "pr#1234".)
        let tear = out
            .iter()
            .find(|s| s.detail.as_deref() == Some("pleme-io/tear"))
            .unwrap();
        assert!(tear.title.contains("bump deps"));
        assert_eq!(tear.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn unauthed_gh_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::GithubReviewRequested);
        assert!(
            GithubReviewRequestedSource
                .poll(&MockEnvironment::new(), &cfg)
                .is_empty()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
