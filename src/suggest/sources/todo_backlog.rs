//! `todo-backlog` — `TODO` / `FIXME` markers scattered across the code root,
//! surfaced as "go finish this" suggestions. Local CLI (ripgrep), no auth.
//!
//! Live wiring: `rg --no-heading -n -e TODO -e FIXME <code-root>` → lines of
//! `path:line:text`. Each match becomes a suggestion whose spawn drops you in
//! the file's directory. `rg` not installed / no output → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};
use std::path::Path;

/// Titles longer than this are truncated (the marker line is the title).
const MAX_TITLE: usize = 120;

pub struct TodoBacklogSource;

impl SuggestionSource for TodoBacklogSource {
    fn kind(&self) -> SourceKind {
        SourceKind::TodoBacklog
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let root = env.code_root().to_string_lossy().into_owned();
        let cmd = Cmd::new("rg")
            .arg("--no-heading")
            .arg("-n")
            .arg("-e")
            .arg("TODO")
            .arg("-e")
            .arg("FIXME")
            .arg(root);
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        parse(&out, env, cfg.max_items)
    }
}

/// Parse `rg --no-heading -n …` output (`path:line:text` per line) into
/// suggestions. Pure — the unit the source is tested through.
fn parse(out: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    out.lines()
        .filter_map(|raw| {
            let mut parts = raw.splitn(3, ':');
            let path = parts.next()?;
            let line = parts.next()?;
            let text = parts.next()?;
            if path.is_empty() || line.is_empty() {
                return None;
            }
            let p = Path::new(path);
            let cwd = match p.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
                _ => env.code_root(),
            };
            let basename = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string());
            let mut name = String::from("\u{1F4DD} "); // 📝
            name.push_str(&basename);
            let spawn = SpawnSpec::new(cwd, name)?;
            let title: String = text.trim().chars().take(MAX_TITLE).collect();
            if title.is_empty() {
                return None;
            }
            let mut detail = String::new();
            detail.push_str(&basename);
            detail.push(':');
            detail.push_str(line);
            let mut key = String::new();
            key.push_str(path);
            key.push(':');
            key.push_str(line);
            Some(
                Suggestion::new(SourceKind::TodoBacklog, &key, title, spawn)
                    .detail(detail)
                    .urgent(Urgency::Idle),
            )
        })
        .take(max.max(1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = "/code/github/pleme-io/mado/src/main.rs:42:    // TODO: wire the picker into the FSM\n/code/github/pleme-io/tear/src/lib.rs:7:    # FIXME handle the empty case\nnot-a-match-line\n";

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("rg --no-heading -n -e TODO -e FIXME /code", FIXTURE)
    }

    #[test]
    fn surfaces_one_suggestion_per_marker_line() {
        let cfg = SourceConfig::for_kind(SourceKind::TodoBacklog);
        let out = TodoBacklogSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 2, "malformed line skipped");
        let todo = out
            .iter()
            .find(|s| s.title.contains("wire the picker"))
            .unwrap();
        // Spawn drops you in the file's directory; detail names file:line.
        assert_eq!(
            todo.spawn.cwd().to_str().unwrap(),
            "/code/github/pleme-io/mado/src"
        );
        assert_eq!(todo.detail.as_deref(), Some("main.rs:42"));
        assert_eq!(todo.urgency, Urgency::Idle);
        let fixme = out
            .iter()
            .find(|s| s.title.contains("handle the empty case"))
            .unwrap();
        assert_eq!(fixme.detail.as_deref(), Some("lib.rs:7"));
    }

    #[test]
    fn caps_at_max_items() {
        let mut cfg = SourceConfig::for_kind(SourceKind::TodoBacklog);
        cfg.max_items = 1;
        let out = TodoBacklogSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn missing_rg_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::TodoBacklog);
        assert!(
            TodoBacklogSource
                .poll(&MockEnvironment::new(), &cfg)
                .is_empty()
        );
    }
}
