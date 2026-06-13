//! Spawn-env projection matrix (operator report 2026-06-12: "vim grey
//! and the wrong font in the embedded-tear window"). One row per spawn
//! entry point, asserting how each gets the typed `caps::EnvProjection`
//! (the `TERM` / `TERMINFO` / `COLORTERM` / `TERM_PROGRAM` capability
//! env) onto a child shell. The end-to-end env-on-child assertions live
//! where they can actually spawn a PTY (`tear-core::inproc` tests and
//! `caps::env_projection_*`); THIS matrix is the structural
//! forcing-function that fails the build if a spawn entry point stops
//! routing through the shared projection — re-diverging the two render
//! modes the way the regression did.
//!
//! Grep-style over comment-stripped source (same hardening as
//! `ux_unification.rs`): a required token can't hide behind a `//` line,
//! and a required call can't be satisfied by a doc comment after the
//! real one was deleted. Failures aggregate matrix-style.

static PTY_RS: &str = include_str!("../src/pty.rs");
static TEAR_RS: &str = include_str!("../src/gui_tear_attach.rs");

fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One spawn entry point + how it must acquire the env projection.
struct SpawnRow {
    /// Human name of the entry point.
    entry: &'static str,
    /// Source file the entry point lives in.
    src: &'static str,
    /// Tokens that MUST appear in `src` (the projection consumption).
    required: &'static [&'static str],
}

/// The matrix. local-PTY + embedded-tear both consume the typed
/// projection, so both are rows. daemon-tear is the documented gap
/// (its children spawn in the DAEMON's process env, not mado's — the
/// `SpawnEnv` seam lives on `InProcess` only), so it is deliberately
/// NOT a row: it is not a mado in-process spawn path.
const MATRIX: &[SpawnRow] = &[
    SpawnRow {
        entry: "local-PTY (src/pty.rs)",
        src: "src/pty.rs",
        // The local path builds the projection + stamps every pair.
        required: &["EnvProjection::prescribed(", ".pairs()"],
    },
    SpawnRow {
        entry: "embedded-tear (src/gui_tear_attach.rs)",
        src: "src/gui_tear_attach.rs",
        // The embedded path hands the projection to tear via SpawnEnv.
        required: &[
            "EnvProjection::prescribed(",
            "SpawnEnv::from_overrides(",
            "set_spawn_env(",
        ],
    },
];

fn src_for(name: &str) -> String {
    match name {
        "src/pty.rs" => strip_line_comments(PTY_RS),
        "src/gui_tear_attach.rs" => strip_line_comments(TEAR_RS),
        other => panic!("unknown matrix source {other}"),
    }
}

#[test]
fn every_spawn_entry_point_routes_through_the_env_projection() {
    let mut failures: Vec<String> = Vec::new();
    for row in MATRIX {
        let src = src_for(row.src);
        for token in row.required {
            if !src.contains(token) {
                failures.push(format!(
                    "{}: missing `{token}` — this spawn entry point no longer routes \
                     through caps::EnvProjection (the vim-grey regression class)",
                    row.entry
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} spawn-env matrix violations:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

/// The matrix covers every CONSUMING spawn entry point (the two render
/// modes mado itself spawns through). A new consuming entry point that
/// lands without a matrix row is the drift this guards: bump the
/// matrix when adding a third in-process spawn path.
#[test]
fn matrix_covers_every_consuming_spawn_entry_point() {
    assert!(
        MATRIX.len() >= 2,
        "the spawn-env matrix must cover at least the local-PTY + embedded-tear paths"
    );
}

/// FIX 3 cwd-handshake forcing function: the local-PTY path must stamp
/// `PWD` to match the cwd (and strip a stale one when there is no
/// cwd), and the embedded path threads its cwd through the typed
/// `SpawnEnv` seam — so a child shell can never inherit a stale parent
/// `PWD`. The end-to-end PWD assertions live in the tear-core +
/// pty-spawn tests; this pins the wiring.
#[test]
fn spawn_paths_stamp_pwd_for_the_cwd_handshake() {
    let mut failures: Vec<String> = Vec::new();
    let pty = strip_line_comments(PTY_RS);
    // Local-PTY: stamps PWD on a cwd, strips it otherwise.
    if !pty.contains("cmd.env(\"PWD\"") {
        failures.push("src/pty.rs no longer stamps PWD for a set cwd".into());
    }
    if !pty.contains("cmd.env_remove(\"PWD\")") {
        failures.push("src/pty.rs no longer strips a stale inherited PWD".into());
    }
    // Embedded: the cwd rides the typed SpawnEnv seam.
    let tear = strip_line_comments(TEAR_RS);
    if !tear.contains(".with_cwd(") {
        failures.push(
            "src/gui_tear_attach.rs no longer threads the boot cwd through SpawnEnv::with_cwd"
                .into(),
        );
    }
    assert!(
        failures.is_empty(),
        "{} cwd-handshake wiring violations:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

