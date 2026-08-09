//! `recent-dirs` — directories you've recently worked in, surfaced as
//! "drop me back here" suggestions.
//!
//! **Redirected to wadachi (2026-08-09).** This source used to read a flat
//! `~/.local/share/mado/recent_dirs` file, one absolute path per line. Nothing
//! in `mado/src` — nor anywhere else in the fleet — has ever WRITTEN that file,
//! and the directory it lives in does not exist. So every poll took the
//! `read_file → None` branch and returned `Fetched(vec![])`, which the board
//! cannot tell apart from a legitimately empty upstream: a structurally dead
//! lane wearing a healthy source's clothes, enabled by default
//! (`config.rs`, `SuggestionSourceConfig::enable(SourceKind::RecentDirs)`),
//! reporting `Ok` forever.
//!
//! Per the fleet's ★★ MODULARIZE, DON'T DELETE doctrine the module is kept and
//! REDIRECTED rather than retired: mado already answers this exact question
//! correctly elsewhere — `mcp.rs`'s `recent_dirs_list` / `jump_to_recent_dir`
//! read `pleme_io_wadachi`, the fleet's SQLite directory-frecency store that
//! frost records into at its `chdir` chokepoint. Pointing the suggestion source
//! at the same store makes the lane live with real data instead of turning a
//! knob off, and it removes the duplicate answer: MCP and the Ctrl-S board now
//! rank recent directories from ONE source of truth.
//!
//! **Honesty contract (now typed, previously unrepresentable).** wadachi is an
//! in-process reader, so its two outcomes are genuinely distinguishable and are
//! reported as such: a readable store yields `Fetched` (empty only when the
//! operator really has no history yet — a fresh machine), while a store that
//! cannot be opened or queried yields `Unavailable(SourceStatus::Error)`. The
//! old flat-file path could only ever produce the first, which is precisely how
//! it stayed dead unnoticed.
//!
//! **Why this source does not read through the `Environment` seam.** Every
//! other provider does its I/O via `&dyn SuggestionEnvironment` so it can be
//! driven by `MockEnvironment` in tests. wadachi is a SQLite store behind a
//! typed in-process facade — there is no `Environment` method that can express
//! it (`izumi::Environment` offers `run` / `http_get` / `read_file` /
//! `path_exists` / `secret`, and a `.db` is none of those), and adding one is a
//! change to izumi, a third repo. Rather than fake the seam, the projection is
//! split out as the pure [`to_suggestions`] and unit-tested directly, and the
//! ranking path is exercised end-to-end against wadachi's own in-memory store —
//! so the tests touch neither the operator's real database nor the filesystem.

// Reached through wadachi's own re-export — `wadachi-spec` is not (and need
// not be) a direct dependency; the facade crate is the one border mado names.
use pleme_io_wadachi::wadachi_spec::RankedDir;

use crate::suggest::core::{SourceKind, SourceStatus, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::SuggestionEnvironment;
use crate::suggest::source::{PollOutcome, SourceConfig};

pub struct RecentDirsSource;

impl izumi::Source<SourceKind, SpawnSpec> for RecentDirsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::RecentDirs
    }

    fn poll(&self, _env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        // An empty needle matches everything (wadachi `query::top_n`), so this
        // is "the operator's top N directories by frecency" — the same read
        // `mcp.rs::recent_dirs_list` performs, bounded by the source's own cap
        // instead of a tool argument.
        match pleme_io_wadachi::top_n("", cfg.max_items.max(1)) {
            Ok(ranked) => PollOutcome::Fetched(to_suggestions(&ranked)),
            Err(e) => {
                tracing::debug!(error = %e, "wadachi frecency store unreadable");
                PollOutcome::Unavailable(SourceStatus::Error)
            }
        }
    }
}

/// Project wadachi's frecency ranking into board suggestions, best-ranked
/// first. Pure — the unit the source is tested through.
///
/// A row that cannot become a spawn target — a non-UTF-8 path, or an empty one
/// (the case `SpawnSpec::new` refuses) — is skipped rather than faked; the rest
/// keep wadachi's order, which IS the ranking.
fn to_suggestions(ranked: &[RankedDir]) -> Vec<Suggestion> {
    ranked
        .iter()
        .filter_map(|dir| {
            let path = dir.path.to_str()?;
            let base = basename(path);
            let mut name = String::from("\u{1F4C1} "); // 📁
            name.push_str(&base);
            let spawn = SpawnSpec::new(path, name)?;
            Some(
                Suggestion::new(SourceKind::RecentDirs, path, base, spawn)
                    .detail(path)
                    .urgent(Urgency::Idle),
            )
        })
        .collect()
}

use super::util::basename;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ranked(path: &str, score: f64) -> RankedDir {
        RankedDir {
            path: PathBuf::from(path),
            score,
        }
    }

    #[test]
    fn produces_a_suggestion_per_ranked_dir() {
        let out = to_suggestions(&[
            ranked("/code/github/pleme-io/mado", 12.0),
            ranked("/code/github/pleme-io/tear", 4.5),
        ]);
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
    fn frecency_order_is_preserved() {
        // wadachi hands back best-first; the board's per-source cap then takes
        // a PREFIX, so re-ordering here would silently drop the top hits.
        let out = to_suggestions(&[
            ranked("/code/github/pleme-io/tear", 99.0),
            ranked("/code/github/pleme-io/mado", 1.0),
        ]);
        assert_eq!(out[0].title, "tear");
        assert_eq!(out[1].title, "mado");
    }

    #[test]
    fn an_unspawnable_entry_is_skipped_not_faked() {
        // `SpawnSpec::new` refuses an empty cwd — a degenerate store row must
        // not reach the board as a suggestion that cannot start.
        let out = to_suggestions(&[ranked("", 1.0), ranked("/code/real", 1.0)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "real");
    }

    #[test]
    fn reads_the_same_store_the_mcp_tools_read() {
        // The redirect's whole point: this source and `mcp.rs::recent_dirs_list`
        // now rank from ONE store. Driven against wadachi's in-memory store so
        // the test touches neither the operator's database nor the filesystem.
        use pleme_io_wadachi::store::{DirStore, MemDirStore};
        use pleme_io_wadachi::wadachi_spec::FrecencyRankingSpec;

        let store = MemDirStore::new();
        store.record("/code/github/pleme-io/mado").unwrap();
        store.record("/code/github/pleme-io/tear").unwrap();
        let ranked =
            pleme_io_wadachi::query::top_n(&store, &FrecencyRankingSpec::skimtab_parity(), "", 10)
                .unwrap();

        let out = to_suggestions(&ranked);
        assert_eq!(out.len(), 2, "an empty needle matches every recorded dir");
        assert!(out.iter().all(|s| s.source == SourceKind::RecentDirs));
        assert!(out.iter().any(|s| s.title == "mado"));
        assert!(out.iter().any(|s| s.title == "tear"));
    }

    #[test]
    fn an_empty_store_is_fetched_empty_not_unavailable() {
        // A fresh machine has no history — that is an OBSERVED empty set, and
        // it must stay distinguishable from the unreadable-store case above.
        assert!(to_suggestions(&[]).is_empty());
    }
}
