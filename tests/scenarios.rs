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

/// A scenario's `printf` may not use `\xNN` — it is not POSIX, and the
/// scenarios run under `/bin/sh`, which is a DIFFERENT SHELL per platform.
///
/// This is a forcing function, not a style rule. It exists because four
/// scenarios shipped with `\xe2\x96\x88`-style escapes, passed on every
/// developer machine for as long as they existed, and failed the first time
/// CI ever ran them (2026-08-01, run 30687417194) — the exact four, while the
/// other six were green. `/bin/sh` is bash on macOS, which implements the
/// `\xNN` bash extension; on Linux it is dash, which does not, and the bytes
/// arrive mangled. Measured, both directions:
///
///   dash  printf '\xe2\x96\x88'   ->  c3 a2 c2 96 c2 88   (latin1 mojibake)
///   dash  printf '\342\226\210'   ->  e2 96 88            (correct U+2588)
///   bash  printf '\342\226\210'   ->  e2 96 88            (correct, unchanged)
///
/// Ubuntu's dash is worse still — it emits the escape literally, which is why
/// CI reported `want '█', got '\'` and the leading backslash is the tell.
///
/// `\ooo` octal IS POSIX and was verified byte-identical across dash, bash,
/// `bash --posix` and zsh, so the fix costs no portability.
///
/// Tier-honest: this is **CI-caught**, not unrepresentable. A scenario can
/// still be WRITTEN with `\xNN`; it just cannot pass. Making it truly
/// unrepresentable would mean scenarios stopped carrying raw shell strings and
/// declared bytes in a typed form instead — the right destination, and a
/// larger change than this repair.
#[test]
fn no_scenario_uses_non_posix_hex_escapes() {
    let dir = scenarios_dir();
    if !dir.exists() {
        return;
    }
    let mut offenders: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read scenarios dir {}: {e}", dir.display()));
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let is_scenario = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".scenario.yaml"));
        if !is_scenario {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // The YAML source spells one backslash as two, so a `\xNN` escape
        // destined for the shell appears here literally as `\\x`.
        for (lineno, line) in body.lines().enumerate() {
            if line.contains("\\\\x") {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                offenders.push(format!("{name}:{}", lineno + 1));
            }
        }
    }
    let n = offenders.len();
    assert!(
        offenders.is_empty(),
        "non-POSIX `\\xNN` printf escape in {n} scenario line(s): {offenders:?}\n\
         `/bin/sh` is bash on macOS but dash on Linux, and dash does not \
         implement `\\xNN` — such a scenario passes locally and fails in CI.\n\
         Use `\\ooo` octal instead (POSIX; identical bytes in dash, bash, \
         bash --posix and zsh). U+2588 `█` is `\\342\\226\\210`.",
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
