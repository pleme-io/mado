//! `project-marks` — your pinned project shortcuts, surfaced as one-keystroke
//! "jump back to this" suggestions. Fully local, no auth, no subprocess.
//!
//! Live wiring: read `~/.local/share/mado/marks` (one mark per line). Each line
//! is either `name<TAB>path` or a bare `path`. Enter spawns a session in the
//! marked directory. Honesty contract: this source reads mado's OWN local
//! state file, so an absent file is a legit first run — `Fetched` of an empty
//! set; there is no param, credential, or tool whose absence could make it
//! `Unavailable`.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::SuggestionEnvironment;
use crate::suggest::source::{PollOutcome, SourceConfig};

pub struct ProjectMarksSource;

impl izumi::Source<SourceKind, SpawnSpec> for ProjectMarksSource {
    fn kind(&self) -> SourceKind {
        SourceKind::ProjectMarks
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let path = env.home().join(".local/share/mado/marks");
        let Some(content) = env.read_file(&path) else {
            // mado's own marks file — absent means a legit first run (observed
            // empty), not an unavailable upstream.
            return PollOutcome::Fetched(Vec::new());
        };
        let mut out = parse(&content);
        out.truncate(cfg.max_items.max(1));
        PollOutcome::Fetched(out)
    }
}

/// Parse the `marks` file into suggestions. Pure — the unit the source is tested
/// through. Each non-blank line is `name<TAB>path` or a bare `path`; the mark
/// falls back to the path's basename when no name is given.
fn parse(content: &str) -> Vec<Suggestion> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (mark, path) = match line.split_once('\t') {
                Some((n, p)) => (n.trim().to_string(), p.trim().to_string()),
                None => (String::new(), line.to_string()),
            };
            if path.is_empty() {
                return None;
            }
            let mark = if mark.is_empty() {
                basename(&path)
            } else {
                mark
            };
            let mut name = String::from("\u{1F4CC} "); // 📌
            name.push_str(&mark);
            let spawn = SpawnSpec::new(path.clone(), name)?;
            Some(Suggestion::new(SourceKind::ProjectMarks, &path, mark, spawn).detail(path))
        })
        .collect()
}

use super::util::basename;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;
    use izumi::Source as _;

    const FIXTURE: &str =
        "nexus\t/code/github/pleme-io/nexus\n/code/github/pleme-io/mado\n\n";

    const MARKS_PATH: &str = "/home/op/.local/share/mado/marks";

    #[test]
    fn surfaces_named_and_bare_marks() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .file(MARKS_PATH, FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::ProjectMarks);
        let PollOutcome::Fetched(out) = ProjectMarksSource.poll(&env, &cfg) else {
            panic!("an observed marks file is Fetched");
        };
        assert_eq!(out.len(), 2, "blank line excluded");
        // Named mark keeps its name as the title.
        let nexus = out.iter().find(|s| s.title == "nexus").unwrap();
        assert_eq!(
            nexus.spawn.cwd().to_str().unwrap(),
            "/code/github/pleme-io/nexus"
        );
        assert_eq!(nexus.detail.as_deref(), Some("/code/github/pleme-io/nexus"));
        // Bare path falls back to the basename as the mark.
        let mado = out.iter().find(|s| s.title == "mado").unwrap();
        assert_eq!(
            mado.spawn.cwd().to_str().unwrap(),
            "/code/github/pleme-io/mado"
        );
    }

    #[test]
    fn respects_max_items() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .file(MARKS_PATH, FIXTURE);
        let mut cfg = SourceConfig::for_kind(SourceKind::ProjectMarks);
        cfg.max_items = 1;
        let PollOutcome::Fetched(out) = ProjectMarksSource.poll(&env, &cfg) else {
            panic!("an observed marks file is Fetched");
        };
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No file registered → read_file returns None → a legit first run:
        // an observed-empty `Fetched`, never `Unavailable` — this source reads
        // mado's own state file, so there is no param/credential/tool to miss.
        let cfg = SourceConfig::for_kind(SourceKind::ProjectMarks);
        assert_eq!(
            ProjectMarksSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::Fetched(Vec::new())
        );
    }

    #[test]
    fn blank_content_is_safe() {
        assert!(parse("\n  \n").is_empty());
    }
}
