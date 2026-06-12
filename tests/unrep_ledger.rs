//! Ledger ⇄ tree contract for
//! `docs/UNREPRESENTABILITY-VERIFICATION.md`: every pinning test the
//! ledger names must exist as a `fn <name>(` somewhere under `src/`
//! or `tests/` — a ledger row naming a nonexistent test fails the
//! build, so the doc cannot rot. Grep-style over comment-stripped
//! source (same hardening as `tests/ux_unification.rs`: a deleted
//! test cannot be satisfied by its own doc comment); failures
//! aggregate matrix-style.

use std::fs;
use std::path::{Path, PathBuf};

static LEDGER: &str = include_str!("../docs/UNREPRESENTABILITY-VERIFICATION.md");

/// Drop `//`-style comment tails so a pinning name can't be
/// satisfied by prose after the real test was deleted.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Backticked identifier-shaped tokens from the LAST cell of every
/// ledger table row — the "Pinned by" column. Other columns carry
/// backticked type/path prose; only the last cell is the contract.
fn pinned_test_names() -> Vec<String> {
    let mut names = Vec::new();
    for line in LEDGER.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        // Only the main ledger table (5 columns) participates; the
        // tier tables and histogram have fewer cells.
        if cells.len() < 5 {
            continue;
        }
        let last = cells.last().expect("split yields at least one cell");
        let mut rest = *last;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            let token = &after[..end];
            if !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                names.push(token.to_owned());
            }
            rest = &after[end + 1..];
        }
    }
    names
}

#[test]
fn every_ledger_pinning_test_exists_in_the_tree() {
    let pinned = pinned_test_names();
    // 8 ledger rows, each pinning at least one test — a parser
    // regression that silently extracts nothing must fail loudly,
    // not pass vacuously.
    assert!(
        pinned.len() >= 8,
        "ledger parse found only {} pinning names — table shape changed?",
        pinned.len()
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("tests"), &mut files);
    let corpus: String = files
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .map(|src| strip_line_comments(&src))
        .collect::<Vec<_>>()
        .join("\n");

    let mut failures: Vec<String> = Vec::new();
    for name in &pinned {
        let needle = format!("fn {name}(");
        if !corpus.contains(&needle) {
            failures.push(format!(
                "ledger pins `{name}` but no `fn {name}(` exists under src/ or tests/"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} rotted ledger rows:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}
