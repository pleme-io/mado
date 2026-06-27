//! `project-marks` — your pinned project shortcuts, surfaced as one-keystroke
//! "jump back to this" suggestions. Fully local, no auth, no subprocess.
//!
//! Live wiring: read `~/.local/share/mado/marks` (one mark per line). Each line
//! is either `name<TAB>path` or a bare `path`. Enter spawns a session in the
//! marked directory. Missing file → no suggestions (graceful).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion};
use crate::suggest::env::SuggestionEnvironment;
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct ProjectMarksSource;

impl SuggestionSource for ProjectMarksSource {
    fn kind(&self) -> SourceKind {
        SourceKind::ProjectMarks
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let path = env.home().join(".local/share/mado/marks");
        let Some(content) = env.read_file(&path) else {
            return Vec::new();
        };
        let mut out = parse(&content);
        out.truncate(cfg.max_items.max(1));
        out
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

    const FIXTURE: &str =
        "nexus\t/code/github/pleme-io/nexus\n/code/github/pleme-io/mado\n\n";

    const MARKS_PATH: &str = "/home/op/.local/share/mado/marks";

    #[test]
    fn surfaces_named_and_bare_marks() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .file(MARKS_PATH, FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::ProjectMarks);
        let out = ProjectMarksSource.poll(&env, &cfg);
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
        assert_eq!(ProjectMarksSource.poll(&env, &cfg).len(), 1);
    }

    #[test]
    fn missing_file_yields_nothing() {
        // No file registered → read_file returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::ProjectMarks);
        assert!(ProjectMarksSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }

    #[test]
    fn blank_content_is_safe() {
        assert!(parse("\n  \n").is_empty());
    }
}
