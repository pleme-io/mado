//! `github-assigned-issues` — open issues assigned to you across the fleet,
//! surfaced where you'd work on them. Fully local-tooling (the `gh` CLI), no
//! extra credential beyond `gh auth`. Enter spawns a session in the issue's
//! repo so you can start on it.
//!
//! Live wiring: `gh search issues --assignee=@me --state=open --json
//! number,title,url,repository --limit N`. `gh`'s `--json` output is stable +
//! documented. Honesty contract: a failed/unauthed `gh` run is
//! `Unavailable(Error)` — only an OBSERVED run output is `Fetched` (so a
//! network blip never reads as "all your issues closed").

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct GithubAssignedIssuesSource;

impl SuggestionSource for GithubAssignedIssuesSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GithubAssignedIssues
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let limit = cfg.max_items.max(1).to_string();
        let mut cmd = Cmd::new("gh")
            .arg("search")
            .arg("issues")
            .arg("--assignee=@me")
            .arg("--state=open")
            .arg("--json")
            .arg("number,title,url,repository")
            .arg("--limit")
            .arg(limit);
        // A Dock-launched mado carries no shell env, so the sops-rendered
        // token authenticates gh. The mock key is unchanged (envs are
        // excluded from Cmd::key).
        if let Some(tok) = env.secret("github/token") {
            cmd = cmd.env("GH_TOKEN", tok);
        }
        let Some(out) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env))
    }
}

/// Parse `gh search issues --json …` output into suggestions. Pure — the unit
/// the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<IssueRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|issue| {
            let owner = issue.repository.name_with_owner();
            let cwd = super::repo_cwd(env, &owner);
            let mut name = String::from("\u{1F41B} #"); // 🐛
            name.push_str(&issue.number.to_string());
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut key = String::new();
            key.push_str(&owner);
            key.push('#');
            key.push_str(&issue.number.to_string());
            let mut title = String::from("#");
            title.push_str(&issue.number.to_string());
            title.push(' ');
            let short: String = issue.title.trim().chars().take(120).collect();
            title.push_str(&short);
            Some(
                Suggestion::new(SourceKind::GithubAssignedIssues, &key, title, spawn)
                    .detail(owner)
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct IssueRow {
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
        {"number":42,"title":"login button is misaligned","url":"https://x","repository":{"name":"mado","nameWithOwner":"pleme-io/mado"}},
        {"number":7,"title":"crash on startup","url":"https://y","repository":{"name":"tear","nameWithOwner":"pleme-io/tear"}}
    ]"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado")
            .cmd(
                "gh search issues --assignee=@me --state=open --json number,title,url,repository --limit 5",
                FIXTURE,
            )
    }

    #[test]
    fn produces_a_suggestion_per_assigned_issue() {
        let cfg = SourceConfig::for_kind(SourceKind::GithubAssignedIssues);
        let PollOutcome::Fetched(out) = GithubAssignedIssuesSource.poll(&env(), &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2);
        let mado = out.iter().find(|s| s.title.contains("#42")).unwrap();
        assert!(mado.title.contains("login button is misaligned"));
        assert_eq!(mado.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        assert_eq!(mado.detail.as_deref(), Some("pleme-io/mado"));
        assert_eq!(mado.urgency, Urgency::Normal);
        // The repo whose dir does not exist falls back to the code root.
        let tear = out
            .iter()
            .find(|s| s.detail.as_deref() == Some("pleme-io/tear"))
            .unwrap();
        assert!(tear.title.contains("crash on startup"));
        assert_eq!(tear.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (gh missing/unauthed/
        // failed) → Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(SourceKind::GithubAssignedIssues);
        assert_eq!(
            GithubAssignedIssuesSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
