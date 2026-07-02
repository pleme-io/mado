//! `tend-repos` — workspace repos that need attention (dirty / missing /
//! unknown), surfaced as "go tidy this" suggestions. Local CLI, no auth.
//!
//! Live wiring: `tend status --json` → an array of `{name, path, state}`. A
//! repo whose state is not `clean` becomes a suggestion whose spawn drops you
//! in that repo's directory. Honesty contract: a failed/absent `tend` run is
//! `Unavailable(Error)` — only an OBSERVED run output is `Fetched` (so a
//! tooling blip never reads as "every repo clean").

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct TendReposSource;

impl SuggestionSource for TendReposSource {
    fn kind(&self) -> SourceKind {
        SourceKind::TendRepos
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, _cfg: &SourceConfig) -> PollOutcome {
        let Some(out) = env.run(&Cmd::new("tend").arg("status").arg("--json")) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env))
    }
}

/// Parse `tend status --json` into suggestions for non-clean repos.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<RepoRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter(|r| !r.state.eq_ignore_ascii_case("clean") && !r.state.is_empty())
        .filter_map(|r| {
            let missing = r.state.eq_ignore_ascii_case("missing") || r.path.is_empty();
            let mut name = String::from("\u{1F9F9} "); // 🧹
            name.push_str(&r.name);
            let spawn = if missing {
                if r.name.is_empty() {
                    return None;
                }
                // A missing repo has no directory yet — the bare name is not
                // a cwd. Seat the session at the code root and kick off the
                // clone via tend.
                let mut sync = String::from("tend sync ");
                sync.push_str(&r.name);
                SpawnSpec::new(env.code_root(), name)?.with_command(sync)
            } else {
                SpawnSpec::new(r.path.clone(), name)?
            };
            let urgency = match r.state.to_ascii_lowercase().as_str() {
                "missing" => Urgency::Normal,
                _ => Urgency::Low,
            };
            let mut title = r.name.clone();
            title.push_str(" — ");
            title.push_str(&r.state);
            Some(
                Suggestion::new(SourceKind::TendRepos, &r.name, title, spawn)
                    .detail(r.state)
                    .urgent(urgency),
            )
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct RepoRow {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"},
        {"name":"tear","path":"/code/github/pleme-io/tear","state":"clean"},
        {"name":"newrepo","path":"","state":"missing"}
    ]"#;

    #[test]
    fn surfaces_only_non_clean_repos() {
        let env = MockEnvironment::new().cmd("tend status --json", FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        let PollOutcome::Fetched(out) = TendReposSource.poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2, "clean repo excluded");
        let dirty = out.iter().find(|s| s.title.contains("mado")).unwrap();
        assert!(dirty.title.contains("dirty"));
        // A dirty repo keeps its real directory as the spawn target.
        assert_eq!(dirty.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        let missing = out.iter().find(|s| s.title.contains("newrepo")).unwrap();
        assert_eq!(missing.urgency, Urgency::Normal);
        // A missing repo seats you at the code root and kicks off the clone
        // (the bare name is not a cwd).
        assert_eq!(missing.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(missing.spawn.initial_command(), Some("tend sync newrepo"));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (tend missing/failed) →
        // Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        assert_eq!(
            TendReposSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }
}
