//! `tend-repos` — workspace repos that need attention (dirty / missing /
//! unknown), surfaced as "go tidy this" suggestions. Local CLI, no auth.
//!
//! Live wiring: `tend status --json` → an array of `{name, path, state}`. A
//! repo whose state is not `clean` becomes a suggestion whose spawn drops you
//! in that repo's directory. `tend` not installed / no JSON → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct TendReposSource;

impl SuggestionSource for TendReposSource {
    fn kind(&self) -> SourceKind {
        SourceKind::TendRepos
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, _cfg: &SourceConfig) -> Vec<Suggestion> {
        let Some(out) = env.run(&Cmd::new("tend").arg("status").arg("--json")) else {
            return Vec::new();
        };
        parse(&out)
    }
}

/// Parse `tend status --json` into suggestions for non-clean repos.
fn parse(json: &str) -> Vec<Suggestion> {
    let Ok(rows) = serde_json::from_str::<Vec<RepoRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter(|r| !r.state.eq_ignore_ascii_case("clean") && !r.state.is_empty())
        .filter_map(|r| {
            // Missing repos have no path to spawn into → use the name; the
            // suggestion still points you at "clone/sync this".
            let cwd = if r.path.is_empty() { r.name.clone() } else { r.path.clone() };
            if cwd.is_empty() {
                return None;
            }
            let mut name = String::from("\u{1F9F9} "); // 🧹
            name.push_str(&r.name);
            let spawn = SpawnSpec::new(cwd, name)?;
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
        let out = TendReposSource.poll(&env, &cfg);
        assert_eq!(out.len(), 2, "clean repo excluded");
        assert!(out.iter().any(|s| s.title.contains("mado") && s.title.contains("dirty")));
        let missing = out.iter().find(|s| s.title.contains("newrepo")).unwrap();
        assert_eq!(missing.urgency, Urgency::Normal);
        // Missing repo with no path falls back to its name as the spawn target.
        assert_eq!(missing.spawn.cwd().to_str().unwrap(), "newrepo");
    }

    #[test]
    fn no_tend_yields_nothing() {
        let cfg = SourceConfig::for_kind(SourceKind::TendRepos);
        assert!(TendReposSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }
}
