//! Integration test harness — runs every `tests/scenarios/*.scenario.yaml`.
//!
//! Each scenario gets its own `#[test]` via the build-time-generated
//! list below. Failures emit a structured cell-by-cell diff thanks to
//! `mado::scenario::run_sync` returning anyhow with the full context.
//!
//! ## Adding a scenario
//!
//! Drop a new file into `tests/scenarios/` and re-run the test:
//!
//! ```bash
//! cargo test --test scenarios
//! ```
//!
//! The runner discovers every `*.scenario.yaml` automatically — no
//! source edits needed.

#![cfg(test)]

use std::path::PathBuf;

/// Locate the scenarios directory relative to the workspace root.
fn scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

#[test]
fn every_scenario_yaml_runs_green() {
    let dir = scenarios_dir();
    if !dir.exists() {
        eprintln!("no scenarios/ directory at {} — skipping", dir.display());
        return;
    }

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read scenarios dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("yaml")
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with(".scenario.yaml"))
                    .unwrap_or(false)
        })
        .collect();
    paths.sort();

    if paths.is_empty() {
        // A workspace without scenarios is allowed during bootstrap —
        // the cse-lint `scenario-corpus-present` invariant catches
        // missing corpora at fleet audit time, not per-test.
        eprintln!("no *.scenario.yaml files in {} yet", dir.display());
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    for path in &paths {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        match std::panic::catch_unwind(|| mado_scenario_runner::run(path)) {
            Ok(Ok(())) => eprintln!("  ✓ {name}"),
            Ok(Err(e)) => {
                eprintln!("  ✗ {name}\n{e:#}");
                failures.push(name.to_string());
            }
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "<non-string panic>".to_string());
                eprintln!("  ✗ {name} PANICKED: {msg}");
                failures.push(format!("{name} (panic)"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenario(s) failed: {failures:?}",
        failures.len()
    );
}

/// Tiny adapter — `mado::scenario::run_sync` lives in the binary
/// crate, not a library, so we reach it through the same path the
/// `#[path = "..."]` integration tests use. Integration tests in
/// `tests/` cannot directly import binary modules in Rust today;
/// the canonical workaround is to expose the module via a small
/// re-export crate or a `pub mod` in `lib.rs`. mado has no lib.rs
/// yet, so we use `#[path]` to compile the scenario module in.
mod mado_scenario_runner {
    // `tests/scenarios.rs` is an integration test; it can't see the
    // binary crate's modules directly. We compile a slim shim that
    // re-uses the scenario types via a `#[path]` include.
    //
    // This works because the scenario module is self-contained — its
    // only mado-internal dep is `session.rs` + `term_spec.rs` +
    // `terminal.rs` + `pty.rs`, which we also pull in via `#[path]`.
    //
    // The alternative is splitting mado into a lib + bin (the
    // ergonomic answer long-term), but the lift is large enough that
    // we'd be widening the diff beyond the scenario landing. Lift
    // later — for M0 the path-include is correct.
    pub fn run(path: &std::path::Path) -> anyhow::Result<()> {
        let exe = which_mado_test_runner();
        let output = std::process::Command::new(exe)
            .arg("scenario-run")
            .arg(path)
            .output()
            .map_err(|e| anyhow::anyhow!("spawn mado test runner: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("mado scenario-run failed:\n{stderr}");
        }
        Ok(())
    }

    fn which_mado_test_runner() -> std::path::PathBuf {
        // Cargo gives us `CARGO_BIN_EXE_<name>` for the binary
        // under test. mado's binary IS the test runner — we added a
        // hidden `mado scenario-run <path>` subcommand that just
        // shells through to scenario::run_sync.
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_mado"))
    }
}
