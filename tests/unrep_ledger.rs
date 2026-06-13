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

/// The leading backticked tier word of every main-ledger data row,
/// i.e. the honest grade in the 4th ("Tier (honest)") column. A row
/// is a main-table row iff it has ≥5 cells (the tier/histogram tables
/// have fewer) and is not the header / separator.
fn ledger_tier_counts() -> std::collections::BTreeMap<String, usize> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for line in LEDGER.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        if cells.len() < 5 {
            continue;
        }
        // 4th column (index 3) is "Tier (honest)". Header rows carry
        // the literal "Tier (honest)" prose with no leading backtick.
        let tier_cell = cells[3].trim();
        let Some(stripped) = tier_cell.strip_prefix('`') else {
            continue;
        };
        let Some(end) = stripped.find('`') else {
            continue;
        };
        let word = &stripped[..end];
        // Only the four real grades participate; any other backticked
        // type-prose leading a cell would be a table-shape change we
        // want to surface, not silently bucket.
        if matches!(
            word,
            "truly-unrepresentable" | "parse-time-rejected" | "partially" | "only-mitigated"
        ) {
            *counts.entry(word.to_owned()).or_default() += 1;
        }
    }
    counts
}

/// The `| `<tier>` | <n> |` rows of the "## Tier histogram" table,
/// parsed back into (tier-word → claimed count). The tier cell may
/// carry trailing prose after the backticked word (the
/// `truly-unrepresentable` row does), so we key on the leading
/// backticked token only.
fn ledger_histogram_claims() -> std::collections::BTreeMap<String, usize> {
    let mut claims: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut in_histogram = false;
    for line in LEDGER.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## Tier histogram") {
            in_histogram = true;
            continue;
        }
        if in_histogram && trimmed.starts_with("## ") {
            break;
        }
        if !in_histogram || !trimmed.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = trimmed.trim_matches('|').split('|').collect();
        if cells.len() != 2 {
            continue;
        }
        let tier_cell = cells[0].trim();
        let Some(stripped) = tier_cell.strip_prefix('`') else {
            continue;
        };
        let Some(end) = stripped.find('`') else {
            continue;
        };
        let word = &stripped[..end];
        let Ok(n) = cells[1].trim().parse::<usize>() else {
            continue;
        };
        claims.insert(word.to_owned(), n);
    }
    claims
}

/// The histogram is "the honesty"; a miscount silently erodes the
/// audit. This forcing-function asserts every histogram cell equals
/// the live count of main-table rows whose leading tier matches — so
/// the arithmetic cannot drift from the table again (the 2026-06-13
/// review's off-by-one would have failed here).
#[test]
fn ledger_histogram_matches_the_main_table_row_counts() {
    let counts = ledger_tier_counts();
    let claims = ledger_histogram_claims();

    assert!(
        !claims.is_empty(),
        "histogram parse found no `| `tier` | n |` rows — table shape changed?"
    );

    let mut failures: Vec<String> = Vec::new();
    for tier in [
        "truly-unrepresentable",
        "parse-time-rejected",
        "partially",
        "only-mitigated",
    ] {
        let actual = counts.get(tier).copied().unwrap_or(0);
        let claimed = claims.get(tier).copied().unwrap_or(0);
        if actual != claimed {
            failures.push(format!(
                "tier `{tier}`: histogram claims {claimed}, main table has {actual} rows"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} histogram cells disagree with the main table:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}
