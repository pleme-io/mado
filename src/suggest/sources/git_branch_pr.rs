//! `git-branch-pr` — your open PRs across the fleet, correlated to where you'd
//! work on them. Fully local-tooling (the `gh` CLI), no extra credential
//! beyond `gh auth`. Enter spawns a session in the PR's repo and kicks off
//! `gh pr checkout <n>`.
//!
//! Live wiring: `gh search prs --author=@me --state=open --json
//! number,title,url,repository --limit N`. `gh`'s `--json` output is stable +
//! documented. Honesty contract: a failed/unauthed `gh` run is
//! `Unavailable(Error)` — only an OBSERVED run output is `Fetched` (so a
//! network blip never reads as "all your PRs merged").

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct GitBranchPrSource;

impl SuggestionSource for GitBranchPrSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GitBranchPr
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let limit = cfg.max_items.max(1).to_string();
        let mut cmd = Cmd::new("gh")
            .arg("search")
            .arg("prs")
            .arg("--author=@me")
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
            let mut name = String::from("\u{1F33F} pr#"); // 🌿
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
                Suggestion::new(SourceKind::GitBranchPr, &key, title, spawn)
                    .correlated(crate::suggest::core::CorrKey::github(&owner, pr.number))
                    .detail(owner)
                    .urgent(Urgency::Low),
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
                "gh search prs --author=@me --state=open --json number,title,url,repository --limit 5",
                FIXTURE,
            )
    }

    #[test]
    fn produces_a_suggestion_per_pr_with_checkout_command() {
        let cfg = SourceConfig::for_kind(SourceKind::GitBranchPr);
        let PollOutcome::Fetched(out) = GitBranchPrSource.poll(&env(), &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2);
        let mado = out.iter().find(|s| s.title.contains("pr#1234")).unwrap();
        assert!(mado.title.contains("fix the parser"));
        assert_eq!(mado.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        assert_eq!(mado.spawn.initial_command(), Some("gh pr checkout 1234"));
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
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (gh missing/unauthed/
        // failed) → Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(SourceKind::GitBranchPr);
        assert_eq!(
            GitBranchPrSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
