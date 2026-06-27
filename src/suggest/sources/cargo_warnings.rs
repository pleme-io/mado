//! `cargo-warnings` — a nudge to clean up compiler warnings in the workspace
//! you're sitting in. Local tooling (`cargo`), no auth, no network.
//!
//! Live wiring: `cargo check --message-format short` → count the stdout lines
//! that carry a ` warning:` marker. If any remain, emit a single "go fix these"
//! suggestion that drops you in the code root. `cargo` not installed / a clean
//! build → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct CargoWarningsSource;

impl SuggestionSource for CargoWarningsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::CargoWarnings
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, _cfg: &SourceConfig) -> Vec<Suggestion> {
        let cmd = Cmd::new("cargo")
            .arg("check")
            .arg("--message-format")
            .arg("short");
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        parse(&out, env)
    }
}

/// Count ` warning:` lines in `cargo check` output and, if any, emit one
/// suggestion. Pure — the unit the source is tested through.
fn parse(output: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let count = output.lines().filter(|l| l.contains(" warning:")).count();
    if count == 0 {
        return Vec::new();
    }
    let cwd = env.code_root();
    let name = String::from("\u{1F980} cargo"); // 🦀
    let Some(spawn) = SpawnSpec::new(cwd, name) else {
        return Vec::new();
    };
    let mut title = String::from("fix ");
    title.push_str(&count.to_string());
    title.push_str(" cargo warnings");
    vec![
        Suggestion::new(SourceKind::CargoWarnings, "cargo-warnings", title, spawn)
            .detail("cargo check")
            .urgent(Urgency::Low),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = "\
src/lib.rs:3:9: warning: unused variable: `x`
src/lib.rs:7:5: warning: unused import: `std::fmt`
    Finished dev [unoptimized + debuginfo] target(s) in 0.42s
";

    const CLEAN: &str = "    Finished dev [unoptimized + debuginfo] target(s) in 0.42s\n";

    #[test]
    fn counts_warning_lines_and_emits_one_suggestion() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("cargo check --message-format short", FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::CargoWarnings);
        let out = CargoWarningsSource.poll(&env, &cfg);
        assert_eq!(out.len(), 1);
        assert!(out[0].title.contains("2"), "title carries the warning count");
        assert_eq!(out[0].detail.as_deref(), Some("cargo check"));
        assert_eq!(out[0].urgency, Urgency::Low);
        assert_eq!(out[0].spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn clean_build_yields_nothing() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("cargo check --message-format short", CLEAN);
        let cfg = SourceConfig::for_kind(SourceKind::CargoWarnings);
        assert!(CargoWarningsSource.poll(&env, &cfg).is_empty());
    }

    #[test]
    fn no_cargo_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::CargoWarnings);
        assert!(CargoWarningsSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }
}
