//! Cross-process smoke for the `mado e2e` subcommand (L2 driver,
//! docs/INTEGRATION-TESTING.md §L2).
//!
//! Runs the real `mado` binary as `mado e2e --shell /bin/sh` — which
//! itself spawns a second `mado mcp` child and drives it over rmcp —
//! and asserts the typed JSON summary contract:
//! `{shell, rows: [{name, pass, skipped, detail}], pass}` with all
//! five matrix rows present, exit code 0.
//!
//! `/bin/sh` (not frostmourne) keeps the test hermetic: it must pass
//! on any macOS/Linux box regardless of what's on PATH. The
//! frostmourne default is exercised by the nix `.#e2e-mado` app
//! against the built closure (M2 follow-up). `/bin/sh` has no wadachi
//! chdir hook, so the `single_recorder` row must report the typed
//! environment-skip (`skipped: true`, non-fatal) — never a fake pass,
//! never a failure.

#[test]
fn mado_e2e_smoke_matrix_passes_with_bin_sh() {
    let bin = env!("CARGO_BIN_EXE_mado");
    let out = std::process::Command::new(bin)
        .args(["e2e", "--shell", "/bin/sh"])
        .env("RUST_LOG", "error")
        .output()
        .expect("run `mado e2e --shell /bin/sh`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("`mado e2e` stdout was not a single JSON summary: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });

    // Typed summary contract.
    assert_eq!(v["shell"], "/bin/sh");
    let rows = v["rows"].as_array().expect("rows array");
    let names: Vec<&str> = rows
        .iter()
        .map(|r| r["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        [
            "spawn_term",
            "prompt_visible",
            "enter_fresh_prompt",
            "echo_marker",
            "single_recorder"
        ],
        "matrix must report every row, in order"
    );
    for row in rows {
        assert!(row["detail"].is_string(), "row missing detail: {row}");
    }

    // The interactive rows all pass outright with /bin/sh.
    for row in &rows[..4] {
        assert_eq!(row["pass"], true, "row failed:\n{row}\nstderr:\n{stderr}");
    }
    // single_recorder: /bin/sh never records → honest environment-
    // skip, not a fake pass, not a failure.
    let sr = &rows[4];
    assert_eq!(
        sr["pass"], false,
        "sh must not fake a recording pass:\n{sr}"
    );
    assert_eq!(sr["skipped"], true, "sh must env-skip, not fail:\n{sr}");
    assert!(
        sr["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("skipped"),
        "skip detail must say so:\n{sr}"
    );

    // Verdict: env-skip is non-fatal — summary pass + exit 0 agree.
    assert_eq!(
        v["pass"], true,
        "matrix failed:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "exit code must mirror the pass verdict; got {:?}\n{stdout}",
        out.status
    );
}

#[test]
fn mado_e2e_reports_all_rows_and_fails_when_shell_is_bogus() {
    // Matrix discipline: a dead row 1 must not hide rows 2–5 — they
    // report `skipped: …` as dependency-FAILURES (skipped: false,
    // unlike single_recorder's environment-skip) and the process
    // exits nonzero.
    let bin = env!("CARGO_BIN_EXE_mado");
    let out = std::process::Command::new(bin)
        .args(["e2e", "--shell", "/nonexistent/e2e-bogus-shell"])
        .env("RUST_LOG", "error")
        .output()
        .expect("run `mado e2e` with bogus shell");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("non-JSON summary: {e}\n{stdout}"));

    assert_eq!(v["pass"], false);
    assert!(!out.status.success(), "bogus shell must exit nonzero");
    let rows = v["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 5, "every row reported even on early failure");
    assert!(rows.iter().all(|r| r["pass"] == false), "{stdout}");
    assert!(
        rows.iter().all(|r| r["skipped"] == false),
        "dependency-skips must not soften the verdict:\n{stdout}"
    );
}
