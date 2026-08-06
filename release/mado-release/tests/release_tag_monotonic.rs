//! The exact-release tag ordinal must be MONOTONE ACROSS WORKFLOW FILES.
//!
//! `mac-app-release`'s own comment states the contract: `r<n>` is "numerically
//! orderable so a subscriber can pick newest". That property is what makes the
//! moving `-latest` pointer and any `max_by(r<n>)` subscriber correct, and it
//! is a property of the NUMBER SOURCE, not of the tag format.
//!
//! It broke once, silently. The publish job moved out of `release-binaries.yml`
//! — which had reached run 397 — into `ci.yml`, then at run 40. `run_number` is
//! a PER-WORKFLOW-FILE counter, so the lineage went r397 -> r40 and "newest"
//! inverted: after r40 shipped, `max_by(r<n>)` resolved to
//! `macos-arm64-r397-1a85f80`, the DMG built from a RED-CI commit whose ungated
//! publish is the reason the job was gated in the first place. It would have
//! kept resolving there for the ~357 merges it takes ci.yml's counter to climb
//! back past 397.
//!
//! `github.run_id` is repo-global and monotone, so it is ordered across that
//! seam and stays ordered if the job is ever moved to a third file.
//!
//! WHY THIS TEST READS THE WORKFLOW FILE. The interesting failure is not in the
//! tag builder — it is a one-line edit in CI. `run_number` is declared
//! `#[arg(long, env = "GITHUB_RUN_NUMBER")]`, and the runner ALWAYS sets that
//! variable, so DELETING the `--run-number` line does not disable the ordinal:
//! it silently reverts to the resetting one. A unit test of `from_run` cannot
//! see that. This asserts the wiring.

use std::path::PathBuf;

fn ci_yml() -> String {
    // tests/ -> mado-release/ -> release/ -> repo root
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows/ci.yml");
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

#[test]
fn ci_passes_run_id_not_run_number() {
    let ci = ci_yml();

    assert!(
        ci.contains(r#"--run-number "${{ github.run_id }}""#),
        "ci.yml must pass github.run_id as the release ordinal. \
         github.run_number is a per-workflow-file counter: moving this job \
         between files resets it and inverts `newest`, which is exactly how \
         macos-arm64-r397 outranked r40 and made a red-CI DMG the latest."
    );

    assert!(
        !ci.contains(r#"--run-number "${{ github.run_number }}""#),
        "ci.yml still passes github.run_number somewhere — that is the \
         resetting counter this test exists to keep out."
    );
}

#[test]
fn the_run_number_flag_is_present_at_all() {
    // The deletion case, which is NOT a no-op: clap falls back to
    // GITHUB_RUN_NUMBER (always set by the runner), so removing the line
    // silently restores the resetting ordinal rather than disabling it.
    let ci = ci_yml();
    assert!(
        ci.contains("--run-number"),
        "the --run-number flag is gone from ci.yml. That does NOT fall back to \
         the timestamp form: mado-release declares the arg with \
         `env = \"GITHUB_RUN_NUMBER\"`, which the runner always sets, so the \
         resetting per-file counter comes back silently."
    );
}

#[test]
fn a_run_id_ordinal_outranks_the_whole_legacy_r_lineage() {
    // The ordering property, stated numerically. Real ids observed on this
    // repo: 31072544164, 31076457408, 31083216255, 31085443094.
    let legacy_high: u64 = 397; // release-binaries.yml's last
    let run_ids: [u64; 4] = [31_072_544_164, 31_076_457_408, 31_083_216_255, 31_085_443_094];

    for id in run_ids {
        assert!(
            id > legacy_high,
            "a run_id must sort above the legacy r<n> lineage, else `newest` \
             can resolve backwards across the file move"
        );
    }

    // And monotone among themselves, in observation order.
    for w in run_ids.windows(2) {
        assert!(w[1] > w[0], "run ids must increase: {} then {}", w[0], w[1]);
    }
}

#[test]
fn exact_tag_keeps_the_sortable_r_shape() {
    // The format the subscriber's max_by(r<n>) depends on. Guarded here so a
    // change to the tag shape has to be deliberate.
    let tags = mac_app_release::tag::ReleaseTags::from_run(
        "macos",
        "arm64",
        31_085_443_094,
        "deadbee",
    );
    assert_eq!(tags.exact, "macos-arm64-r31085443094-deadbee");
    assert_eq!(tags.latest, "macos-arm64-latest");
}
