//! `cargo-warnings` — a nudge to clean up compiler warnings in the workspace
//! you're sitting in. Local tooling (`cargo`), no auth, no network.
//!
//! Live wiring: `cargo check --message-format json` → count the
//! `compiler-message` records whose `level` is `warning` AND that carry at
//! least one span. The non-empty-span filter is what makes the count exact:
//! cargo also emits a summary record (`"... generated N warnings"`) with an
//! empty `spans` array, and the old `--message-format short` substring scan
//! double-counted it (plus the per-line ` warning:` markers). If any real
//! warnings remain, emit a single "go fix these" suggestion that drops you in
//! the code root. Honesty contract: a failed `cargo` run is
//! `Unavailable(Error)`; a clean OBSERVED build is `Fetched` of an empty set
//! (the nudge genuinely resolves).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct CargoWarningsSource;

impl SuggestionSource for CargoWarningsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::CargoWarnings
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, _cfg: &SourceConfig) -> PollOutcome {
        let cmd = Cmd::new("cargo")
            .arg("check")
            .arg("--message-format")
            .arg("json");
        let Some(out) = env.run(&cmd) else {
            // A non-zero cargo exit (compile errors, missing cargo) is
            // unobservable through this seam — Error keeps the last-known
            // warnings on the board until the TTL ages them out.
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, env))
    }
}

/// One `cargo --message-format json` record (only the fields we discriminate on).
#[derive(serde::Deserialize)]
struct CargoRecord {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: Option<CargoDiagnostic>,
}

#[derive(serde::Deserialize)]
struct CargoDiagnostic {
    #[serde(default)]
    level: String,
    /// Real diagnostics point at source; the aggregate summary has `spans: []`.
    #[serde(default)]
    spans: Vec<serde_json::Value>,
}

/// Count real warning diagnostics in `cargo check --message-format json` output
/// and, if any, emit one suggestion. Pure — the unit the source is tested
/// through. Lines that aren't JSON (cargo's human progress) are skipped.
fn parse(output: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let count = output
        .lines()
        .filter_map(|l| serde_json::from_str::<CargoRecord>(l).ok())
        .filter(|r| r.reason == "compiler-message")
        .filter_map(|r| r.message)
        .filter(|m| m.level == "warning" && !m.spans.is_empty())
        .count();
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

    // Two real warnings (non-empty spans) + the aggregate summary record
    // (empty spans, `level":"warning"`) that the old substring scan miscounted.
    // The non-empty-span filter must yield exactly 2.
    const FIXTURE: &str = concat!(
        r#"{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/lib.rs"}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/lib.rs"}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","message":"`mado` (lib) generated 2 warnings","spans":[]}}"#,
        "\n",
        r#"{"reason":"compiler-artifact","target":{"name":"mado"}}"#,
        "\n",
        r#"{"reason":"build-finished","success":true}"#,
        "\n",
    );

    const CLEAN: &str = concat!(
        r#"{"reason":"compiler-artifact","target":{"name":"mado"}}"#,
        "\n",
        r#"{"reason":"build-finished","success":true}"#,
        "\n",
    );

    #[test]
    fn counts_warning_diagnostics_and_emits_one_suggestion() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("cargo check --message-format json", FIXTURE);
        let cfg = SourceConfig::for_kind(SourceKind::CargoWarnings);
        let PollOutcome::Fetched(out) = CargoWarningsSource.poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 1);
        assert!(
            out[0].title.contains('2'),
            "title carries the warning count, summary record excluded: {}",
            out[0].title
        );
        assert_eq!(out[0].detail.as_deref(), Some("cargo check"));
        assert_eq!(out[0].urgency, Urgency::Low);
        assert_eq!(out[0].spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn clean_build_yields_observed_empty() {
        // A clean OBSERVED build is Fetched of an empty set — the nudge
        // genuinely resolves, unlike a failed run.
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("cargo check --message-format json", CLEAN);
        let cfg = SourceConfig::for_kind(SourceKind::CargoWarnings);
        let PollOutcome::Fetched(out) = CargoWarningsSource.poll(&env, &cfg) else {
            panic!("an observed clean build is Fetched");
        };
        assert!(out.is_empty());
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (compile errors exit
        // non-zero through this seam) → Error, never an empty Fetched — the
        // last-known warnings stay and age out by TTL.
        let cfg = SourceConfig::for_kind(SourceKind::CargoWarnings);
        assert_eq!(
            CargoWarningsSource.poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }
}
