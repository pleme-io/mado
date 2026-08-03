//! `recent-dirs` — directories you've recently jumped into, surfaced as
//! "drop me back here" suggestions. No tooling, no auth — just a flat list
//! file mado itself maintains.
//!
//! Live wiring: read `~/.local/share/mado/recent_dirs` — one absolute dir
//! path per line. Each non-empty line becomes a suggestion whose spawn drops
//! you in that directory. Honesty contract: this source reads mado's OWN
//! local state file, so an absent file is a legit first run — `Fetched` of an
//! empty set; there is no param, credential, or tool whose absence could make
//! it `Unavailable`.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::SuggestionEnvironment;
use crate::suggest::source::{PollOutcome, SourceConfig};

pub struct RecentDirsSource;

impl izumi::Source<SourceKind, SpawnSpec> for RecentDirsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::RecentDirs
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let path = env.home().join(".local/share/mado/recent_dirs");
        let Some(content) = env.read_file(&path) else {
            // mado's own state file — absent means a legit first run (observed
            // empty), not an unavailable upstream.
            return PollOutcome::Fetched(Vec::new());
        };
        PollOutcome::Fetched(parse(&content, cfg.max_items.max(1)))
    }
}

/// Parse the recent-dirs list into suggestions, one per non-empty line, capped
/// at `limit`. Pure — the unit the source is tested through.
fn parse(content: &str, limit: usize) -> Vec<Suggestion> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(limit)
        .filter_map(|line| {
            let base = basename(line);
            let mut name = String::from("\u{1F4C1} "); // 📁
            name.push_str(&base);
            let spawn = SpawnSpec::new(line, name)?;
            Some(
                Suggestion::new(SourceKind::RecentDirs, line, base, spawn)
                    .detail(line)
                    .urgent(Urgency::Idle),
            )
        })
        .collect()
}

use super::util::basename;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;
    use izumi::Source as _;

    const FIXTURE: &str = "/code/github/pleme-io/mado\n/code/github/pleme-io/tear\n";

    #[test]
    fn produces_a_suggestion_per_recent_dir() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .file("/home/op/.local/share/mado/recent_dirs", FIXTURE);
        let mut cfg = SourceConfig::for_kind(SourceKind::RecentDirs);
        cfg.max_items = 10;
        let PollOutcome::Fetched(out) = RecentDirsSource.poll(&env, &cfg) else {
            panic!("an observed state file is Fetched");
        };
        assert_eq!(out.len(), 2);
        let mado = out.iter().find(|s| s.title == "mado").unwrap();
        assert_eq!(
            mado.spawn.cwd().to_str().unwrap(),
            "/code/github/pleme-io/mado"
        );
        assert_eq!(mado.detail.as_deref(), Some("/code/github/pleme-io/mado"));
        assert_eq!(mado.urgency, Urgency::Idle);
        assert!(out.iter().any(|s| s.title == "tear"));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No file registered → read_file() returns None → a legit first run:
        // an observed-empty `Fetched`, never `Unavailable` — this source reads
        // mado's own state file, so there is no param/credential/tool to miss.
        let cfg = SourceConfig::for_kind(SourceKind::RecentDirs);
        assert_eq!(
            RecentDirsSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::Fetched(Vec::new())
        );
    }
}
