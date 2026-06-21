//! `mado e2e` — the typed L2 smoke driver (docs/INTEGRATION-TESTING.md §L2).
//!
//! A typed rmcp CLIENT that spawns `mado mcp` (this same binary) as a
//! child process over stdio and drives the `spawn_term` /
//! `get_output` / `send_keys` tool surface through a fixed smoke
//! matrix. Every row asserts a **command round-trip**, never bare
//! liveness — the verification discipline from the 2026-06-10
//! freeze-on-Enter incident ("process alive" ≠ "shell interactive").
//!
//! Matrix rows (dependency-ordered; later rows report
//! `skipped: <dependency>` instead of silently passing when an
//! earlier row fails — matrix discipline: aggregate failures, report
//! every row, then fail once via the exit code):
//!
//! 1. `spawn_term`   — session opens for the configured shell
//! 2. `prompt_visible` — non-blank grid content within the per-row
//!    ceiling (`MADO_E2E_PROMPT_TIMEOUT_SECS`, default 45s)
//! 3. `enter_fresh_prompt` — Enter → shell still interactive, a
//!    fresh prompt line appears (the E5 death point)
//! 4. `echo_marker`  — `echo E2E_MARKER` round-trips: the marker
//!    appears ≥2× (command echo + command output)
//! 5. `single_recorder` — wadachi (轍) single-recorder rule: a `cd`
//!    into a fresh nonce dir lands EXACTLY ONE visit in a hermetic
//!    `WADACHI_DB` store (the shell records at its chdir chokepoint;
//!    mado and every other reader never record). Shells without a
//!    wadachi hook (`/bin/sh`) report a typed environment-skip
//!    (`skipped: true`, non-fatal) — distinct from the fatal
//!    dependency-skips above, which keep `pass: false`.
//!
//! Rows are typed Rust constants for now. mado's shikumi plumbing
//! (`MadoConfig`) is *terminal* config, not operator-harness config —
//! rather than invent a second config surface here, a shikumi-style
//! matrix YAML is the documented follow-up (INTEGRATION-TESTING.md
//! M2/M3, alongside the nix `.#e2e-mado` app that runs this binary
//! from the built system closure).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde::Serialize;

/// Per-row wait ceiling. The L2 spec says "prompt visible within
/// 10s"; the same ceiling bounds every round-trip row (prompt_visible,
/// enter_fresh_prompt, echo_marker). Operator-tunable via
/// `MADO_E2E_PROMPT_TIMEOUT_SECS` (u64 seconds; default 45) — 10s was
/// too tight for the heavier frostmourne rc under rebuild load.
const ROW_TIMEOUT_DEFAULT_SECS: u64 = 45;

/// Resolve the per-row wait ceiling from `MADO_E2E_PROMPT_TIMEOUT_SECS`
/// (falling back to [`ROW_TIMEOUT_DEFAULT_SECS`] when unset/unparseable).
fn row_timeout() -> Duration {
    let secs = std::env::var("MADO_E2E_PROMPT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(ROW_TIMEOUT_DEFAULT_SECS);
    Duration::from_secs(secs)
}
/// Poll cadence while waiting on grid content.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Grid dimensions for every session this driver spawns. A generous
/// fixed width keeps the round-trip rows reading a clean single-line
/// echo: frostmourne's prompt can run ~70 cols, and an 80-col default
/// left a 15-char command (`echo E2E_MARKER`) no room — it soft-wrapped
/// across the right edge and split the marker (`E2E_MARKE` ⏎ `R`),
/// reading as a missing occurrence (the 2026-06-21 ryn gate false-
/// negative). 200×50 decouples the wiring smoke test from display
/// wrapping AND keeps the failure-detail grid excerpts legible. The
/// matcher ([`marker_occurrences`]) is wrap-robust regardless — this is
/// defence in depth, not the load-bearing fix.
const E2E_COLS: u16 = 200;
const E2E_ROWS: u16 = 50;
/// Sentinel for the echo round-trip row. Must appear ≥2× in the
/// grid (counted wrap-robustly via [`marker_occurrences`]): once as
/// the echoed command line, once as command output.
const E2E_MARKER: &str = "E2E_MARKER";
/// How long the `single_recorder` row waits for the shell's chdir
/// hook to land a visit in the hermetic store before concluding the
/// shell is a non-recorder (`/bin/sh` pays this in full every run —
/// keep it well under [`row_timeout`]).
const RECORD_WAIT: Duration = Duration::from_secs(2);
/// Settle window between "first visit observed" and the final
/// exactly-one recount — long enough for a second errant recorder
/// (the bug class the row exists to catch) to betray itself.
const RECORD_SETTLE: Duration = Duration::from_millis(400);

/// One smoke-matrix row outcome. `detail` carries the human-readable
/// evidence (elapsed ms, grid excerpts, skip reason).
///
/// `skipped` is the typed *environment*-skip: the row's contract
/// cannot be exercised in this environment (e.g. a shell with no
/// wadachi hook) — non-fatal for the summary verdict. Dependency
/// skips (an earlier row failed) stay `pass: false, skipped: false`:
/// the matrix is already red and they must not soften it.
#[derive(Debug, Serialize)]
pub struct RowResult {
    pub name: &'static str,
    pub pass: bool,
    pub skipped: bool,
    pub detail: String,
}

impl RowResult {
    fn passed(name: &'static str, detail: String) -> Self {
        Self { name, pass: true, skipped: false, detail }
    }
    fn failed(name: &'static str, detail: String) -> Self {
        Self { name, pass: false, skipped: false, detail }
    }
    fn env_skipped(name: &'static str, detail: String) -> Self {
        Self { name, pass: false, skipped: true, detail }
    }
}

/// Typed summary `mado e2e` prints to stdout as JSON. `pass` is the
/// machine-readable verdict (the process exit code mirrors it).
#[derive(Debug, Serialize)]
pub struct E2eSummary {
    pub shell: String,
    pub rows: Vec<RowResult>,
    pub pass: bool,
}

/// The connected rmcp client driving the `mado mcp` child.
type Client = RunningService<RoleClient, ()>;

/// Run the full smoke matrix against a freshly-spawned `mado mcp`
/// child. Infrastructure failures (can't spawn the child, JSON-RPC
/// handshake fails) are hard errors; row failures are typed rows in
/// the summary.
pub async fn run(shell: &str) -> Result<E2eSummary> {
    // The MCP server is THIS binary — `mado e2e` always smokes the
    // exact artifact it shipped in (the nix `.#e2e-mado` app runs it
    // from the built closure, never cargo artifacts).
    let mado_bin = std::env::current_exe().context("resolve current mado binary path")?;
    let mut cmd = tokio::process::Command::new(&mado_bin);
    // Stderr stays inherited: the child's tracing can't corrupt the
    // stdio JSON-RPC framing, and failure context surfaces in CI logs.
    cmd.arg("mcp").env("RUST_LOG", "error");
    let transport = TokioChildProcess::new(cmd).context("spawn `mado mcp` child")?;
    let client = ()
        .serve(transport)
        .await
        .context("rmcp client handshake with `mado mcp` child")?;

    let mut rows: Vec<RowResult> = Vec::new();

    // ── row 1: spawn_term ────────────────────────────────────────
    let session_id = match spawn_term(&client, shell, Some(hermetic_shell_env(&[]))).await {
        Ok(id) => {
            rows.push(RowResult::passed(
                "spawn_term",
                format!("session_id={id} shell={shell}"),
            ));
            Some(id)
        }
        Err(e) => {
            rows.push(RowResult::failed("spawn_term", format!("{e:#}")));
            None
        }
    };

    // ── row 2: prompt_visible ────────────────────────────────────
    let mut prompt_ok = false;
    match &session_id {
        Some(id) => {
            let started = Instant::now();
            match wait_for_output(&client, id, |text| non_blank_lines(text) >= 1).await {
                Ok(text) => {
                    prompt_ok = true;
                    rows.push(RowResult::passed(
                        "prompt_visible",
                        format!(
                            "prompt rendered in {}ms (last line: {:?})",
                            started.elapsed().as_millis(),
                            last_non_blank_line(&text),
                        ),
                    ));
                }
                Err(e) => rows.push(RowResult::failed(
                    "prompt_visible",
                    format!("no non-blank grid content within {:?}: {e:#}", row_timeout()),
                )),
            }
        }
        None => rows.push(skipped("prompt_visible", "spawn_term failed")),
    }

    // ── row 3: enter_fresh_prompt (the E5 row) ───────────────────
    let mut enter_ok = false;
    match &session_id {
        Some(id) if prompt_ok => {
            let before = match get_output(&client, id).await {
                Ok(text) => non_blank_lines(&text),
                Err(_) => 0,
            };
            match press_enter_and_wait(&client, id, before).await {
                Ok(after) => {
                    enter_ok = true;
                    rows.push(RowResult::passed(
                        "enter_fresh_prompt",
                        format!(
                            "non-blank lines {before} → {after} after Enter (shell survived, fresh prompt)"
                        ),
                    ));
                }
                Err(e) => rows.push(RowResult::failed(
                    "enter_fresh_prompt",
                    format!(
                        "no fresh prompt within {:?} after Enter (E5 class — shell dead or frozen): {e:#}",
                        row_timeout()
                    ),
                )),
            }
        }
        Some(_) => rows.push(skipped("enter_fresh_prompt", "prompt_visible failed")),
        None => rows.push(skipped("enter_fresh_prompt", "spawn_term failed")),
    }

    // ── row 4: echo_marker round-trip ────────────────────────────
    match &session_id {
        Some(id) if enter_ok => {
            let keys = [
                "echo ",
                E2E_MARKER,
                "\n",
            ]
            .concat();
            match send_keys(&client, id, &keys).await {
                Ok(()) => {
                    match wait_for_output(&client, id, |text| {
                        marker_occurrences(text, E2E_MARKER) >= 2
                    })
                    .await
                    {
                        Ok(text) => rows.push(RowResult::passed(
                            "echo_marker",
                            format!(
                                "{} occurrences of {E2E_MARKER} (echo + output)",
                                marker_occurrences(&text, E2E_MARKER)
                            ),
                        )),
                        Err(e) => rows.push(RowResult::failed(
                            "echo_marker",
                            format!(
                                "marker did not round-trip ≥2× within {:?}: {e:#}",
                                row_timeout()
                            ),
                        )),
                    }
                }
                Err(e) => rows.push(RowResult::failed(
                    "echo_marker",
                    format!("send_keys failed: {e:#}"),
                )),
            }
        }
        Some(_) => rows.push(skipped("echo_marker", "enter_fresh_prompt failed")),
        None => rows.push(skipped("echo_marker", "spawn_term failed")),
    }

    // ── row 5: single_recorder (wadachi 轍) ──────────────────────
    // Spawns its OWN session with a hermetic WADACHI_DB so the
    // operator's real frecency store is never touched, then proves
    // the single-recorder rule end-to-end: one `cd` → exactly one
    // visit. Gated on enter_fresh_prompt — an uninteractive shell
    // can't `cd`, so the row would only re-report the E5 failure.
    if enter_ok {
        rows.push(single_recorder_row(&client, shell).await);
    } else if session_id.is_some() {
        rows.push(skipped("single_recorder", "enter_fresh_prompt failed"));
    } else {
        rows.push(skipped("single_recorder", "spawn_term failed"));
    }

    // Best-effort cleanup — the child dies with the client transport
    // anyway, but closing the session keeps the run tidy.
    if let Some(id) = &session_id {
        let _ = call_tool(
            &client,
            "close_session",
            serde_json::json!({ "session_id": id }),
        )
        .await;
    }
    client.cancel().await.ok();

    let pass = summary_pass(&rows);
    Ok(E2eSummary {
        shell: shell.to_string(),
        rows,
        pass,
    })
}

/// The matrix verdict: every row either passed or is a typed
/// environment-skip. Dependency-skips (`pass: false, skipped: false`)
/// still fail the matrix — the dependency row already did.
fn summary_pass(rows: &[RowResult]) -> bool {
    rows.iter().all(|r| r.pass || r.skipped)
}

/// Uniform "dependency failed" row — keeps the matrix complete (every
/// row reported every run) without faking a pass.
fn skipped(name: &'static str, dependency: &str) -> RowResult {
    RowResult::failed(name, format!("skipped: {dependency}"))
}

// ── row 5: wadachi single-recorder ──────────────────────────────────

/// Drive the `single_recorder` row; infrastructure errors become a
/// failed row (never a hard error — matrix discipline: every row
/// reports). Cleanup (session close, nonce dir, hermetic store) is
/// best-effort regardless of outcome.
async fn single_recorder_row(client: &Client, shell: &str) -> RowResult {
    const NAME: &str = "single_recorder";
    // Nonce keeps runs hermetic against each other AND keeps the
    // visit path unique so the count can't be polluted by the shell's
    // startup cwd or a previous run.
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let dir = format!("/tmp/e2e-sr-{nonce}");
    let db = std::path::PathBuf::from(format!("/tmp/e2e-sr-{nonce}.db"));

    // The hermetic seam: WADACHI_DB lands in the SPAWNED shell's env
    // (TermSpec.env → PTY child env), so a recording shell writes the
    // throwaway store, never the operator's real one. The driver
    // reads the same path directly via the wadachi store API — no
    // env mutation in this process.
    let env = hermetic_shell_env(&[("WADACHI_DB", db.to_string_lossy().into_owned())]);
    let row = match spawn_term(client, shell, Some(env)).await {
        Ok(id) => {
            let row = drive_single_recorder(client, &id, &dir, &db)
                .await
                .unwrap_or_else(|e| RowResult::failed(NAME, format!("{e:#}")));
            let _ = call_tool(
                client,
                "close_session",
                serde_json::json!({ "session_id": id }),
            )
            .await;
            row
        }
        Err(e) => RowResult::failed(NAME, format!("hermetic spawn_term failed: {e:#}")),
    };
    // Best-effort cleanup of the nonce dir + the store (and SQLite
    // WAL sidecars).
    let _ = std::fs::remove_dir_all(&dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    row
}

/// The row body: mkdir + cd into the nonce dir, prove the commands
/// were consumed via an echo round-trip, then count visits in the
/// hermetic store. Exactly one → pass; zero → typed environment-skip
/// (no wadachi chdir hook in this shell); two or more → the
/// single-recorder rule is violated (a reader recorded) → fail.
async fn drive_single_recorder(
    client: &Client,
    session_id: &str,
    dir: &str,
    db: &std::path::Path,
) -> Result<RowResult> {
    const NAME: &str = "single_recorder";
    // Fresh session — wait for its prompt before typing.
    let text = wait_for_output(client, session_id, |t| non_blank_lines(t) >= 1)
        .await
        .context("single_recorder: prompt never appeared in hermetic session")?;
    // mkdir first so the cd cannot fail, then cd, then a marker echo.
    // Sends are SEQUENCED — each command waits for the next prompt
    // line before the following send. Reedline-family shells
    // (frostmourne) probe the terminal between prompts (CPR) and can
    // drop type-ahead, so back-to-back sends lose commands (observed
    // live 2026-06-10: the third of three unsequenced sends vanished).
    let mut lines = non_blank_lines(&text);
    for cmd in [format!("mkdir -p {dir}"), format!("cd {dir}")] {
        send_keys(client, session_id, &format!("{cmd}\r")).await?;
        let text = wait_for_output(client, session_id, |t| non_blank_lines(t) > lines)
            .await
            .with_context(|| format!("single_recorder: no fresh prompt after {cmd:?}"))?;
        lines = non_blank_lines(&text);
    }
    // ≥2 marker occurrences (command echo + output) proves the cd
    // already executed — a recording shell records at its chdir
    // chokepoint, i.e. before the marker output ever renders.
    let marker = format!("SR_DONE_{}", std::process::id());
    send_keys(client, session_id, &format!("echo {marker}\r")).await?;
    wait_for_output(client, session_id, |t| marker_occurrences(t, &marker) >= 2)
        .await
        .context("single_recorder: cd-confirm marker did not round-trip")?;

    // Poll the hermetic store for the visit. A non-recording shell
    // (/bin/sh — no wadachi hook) never writes one: that's the typed
    // environment-skip, not a failure.
    let deadline = Instant::now() + RECORD_WAIT;
    let mut count = visits_for(db, dir)?;
    while count == 0 && Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        count = visits_for(db, dir)?;
    }
    if count == 0 {
        return Ok(RowResult::env_skipped(
            NAME,
            format!(
                "skipped: no visit recorded in the hermetic store within \
                 {RECORD_WAIT:?} — shell has no wadachi chdir hook (e.g. /bin/sh)"
            ),
        ));
    }
    // Settle, then recount — a second errant recorder (mado, a
    // reader, a doubled shell hook) lands here as count > 1.
    tokio::time::sleep(RECORD_SETTLE).await;
    let count = visits_for(db, dir)?;
    if count == 1 {
        Ok(RowResult::passed(
            NAME,
            format!("exactly 1 visit recorded for {dir} (shell is the sole recorder)"),
        ))
    } else {
        Ok(RowResult::failed(
            NAME,
            format!(
                "{count} visits recorded for {dir} — single-recorder rule \
                 violated (expected exactly 1; readers must never record)"
            ),
        ))
    }
}

/// Count visits in the hermetic store whose final path component is
/// the nonce dir. Matching on the basename keeps the count robust to
/// path canonicalization (macOS records `/private/tmp/…` for a
/// logical `/tmp/…` cwd). An absent store file means zero visits —
/// the driver never creates the store (opening would).
fn visits_for(db: &std::path::Path, dir: &str) -> Result<usize> {
    use pleme_io_wadachi::{DirFrecencyDb, DirStore};
    if !db.exists() {
        return Ok(0);
    }
    let basename = std::path::Path::new(dir)
        .file_name()
        .and_then(|n| n.to_str())
        .context("nonce dir has no basename")?
        .to_string();
    let store = DirFrecencyDb::open(db).context("open hermetic wadachi store")?;
    Ok(store
        .entries()
        .context("read hermetic wadachi entries")?
        .iter()
        .filter(|e| {
            e.path.file_name().and_then(|n| n.to_str()) == Some(basename.as_str())
        })
        .map(|e| e.visits.len())
        .sum())
}

// ── typed tool-call plumbing ────────────────────────────────────────

/// Call one MCP tool and parse its single text payload as JSON —
/// every mado tool returns exactly that shape.
async fn call_tool(
    client: &Client,
    name: &'static str,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let arguments = match args {
        serde_json::Value::Object(m) => Some(m),
        serde_json::Value::Null => None,
        other => anyhow::bail!("tool args must be a JSON object, got {other}"),
    };
    let result = client
        .call_tool(CallToolRequestParams {
            name: name.into(),
            arguments,
            meta: None,
            task: None,
        })
        .await
        .with_context(|| format!("call_tool({name})"))?;
    let text = result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .with_context(|| format!("{name} returned no text content"))?;
    serde_json::from_str(&text).with_context(|| format!("{name} returned non-JSON: {text}"))
}

/// `spawn_term` with the matrix shell; returns the session id. `env`
/// (a JSON object) merges onto the spawned shell's environment via
/// `TermSpec.env` — the single_recorder row uses it to point the
/// shell at a hermetic `WADACHI_DB`.

/// Hermetic shell environment for every session this driver spawns.
///
/// 2026-06-10 incident: live e2e runs spawned real frostmourne with the
/// operator's HOME — the shell recorded every test command (escape-bomb
/// printfs, fake `git push`/`nix run .#rebuild` lines, marker echoes)
/// into the REAL `~/.local/state/zsh/history`, which Ctrl-R then served
/// back to the operator. The driver now gives each spawned shell a
/// throwaway HOME + HISTFILE + XDG trio, so history/state isolation
/// holds BY CONSTRUCTION for every row (WADACHI_DB merges on top for
/// the single-recorder row).
fn hermetic_shell_env(extra: &[(&str, String)]) -> serde_json::Value {
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let home = std::env::temp_dir().join(format!("mado-e2e-home-{nonce}"));
    let _ = std::fs::create_dir_all(&home);
    let h = home.to_string_lossy().into_owned();
    let mut map = serde_json::Map::new();
    map.insert("HOME".into(), serde_json::Value::String(h.clone()));
    map.insert(
        "HISTFILE".into(),
        serde_json::Value::String(format!("{h}/.zsh_history")),
    );
    for k in ["XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_CONFIG_HOME"] {
        map.insert(
            k.into(),
            serde_json::Value::String(format!("{h}/{}", k.to_lowercase())),
        );
    }
    for (k, v) in extra {
        map.insert((*k).into(), serde_json::Value::String(v.clone()));
    }
    serde_json::Value::Object(map)
}

async fn spawn_term(
    client: &Client,
    shell: &str,
    env: Option<serde_json::Value>,
) -> Result<String> {
    let mut args = serde_json::json!({
        "shell": shell,
        "cols": E2E_COLS,
        "rows": E2E_ROWS,
    });
    if let Some(env) = env {
        args["env"] = env;
    }
    let v = call_tool(client, "spawn_term", args).await?;
    if v["ok"] != true {
        anyhow::bail!("spawn_term not ok: {v}");
    }
    v["session_id"]
        .as_str()
        .map(str::to_string)
        .context("spawn_term response missing session_id")
}

/// One `get_output` snapshot as plain text.
async fn get_output(client: &Client, session_id: &str) -> Result<String> {
    let v = call_tool(
        client,
        "get_output",
        serde_json::json!({ "session_id": session_id }),
    )
    .await?;
    if v["ok"] != true {
        anyhow::bail!("get_output not ok: {v}");
    }
    Ok(v["output"].as_str().unwrap_or_default().to_string())
}

/// `send_keys` — raw bytes to the session's PTY.
async fn send_keys(client: &Client, session_id: &str, keys: &str) -> Result<()> {
    let v = call_tool(
        client,
        "send_keys",
        serde_json::json!({ "session_id": session_id, "keys": keys }),
    )
    .await?;
    if v["ok"] != true {
        anyhow::bail!("send_keys not ok: {v}");
    }
    Ok(())
}

/// Poll `get_output` until `accept` holds or [`row_timeout`] elapses.
/// Returns the accepted grid text; errors with the LAST observed grid
/// so a failed row's detail shows what the terminal actually rendered.
async fn wait_for_output<F>(client: &Client, session_id: &str, accept: F) -> Result<String>
where
    F: Fn(&str) -> bool,
{
    let deadline = Instant::now() + row_timeout();
    let mut last = String::new();
    while Instant::now() < deadline {
        last = get_output(client, session_id).await?;
        if accept(&last) {
            return Ok(last);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    anyhow::bail!(
        "condition not met before deadline; last grid (non-blank lines only): {:?}",
        last.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>(),
    )
}

/// Press Enter and wait until the grid grows a fresh non-blank line
/// (a new prompt) beyond `before`. Proves both "shell still alive"
/// and "fresh prompt" in one observation — a shell that dies on
/// Enter (E5) never paints another prompt line.
async fn press_enter_and_wait(client: &Client, session_id: &str, before: usize) -> Result<usize> {
    send_keys(client, session_id, "\n").await?;
    let text = wait_for_output(client, session_id, |t| non_blank_lines(t) > before).await?;
    Ok(non_blank_lines(&text))
}

/// Count rows with any non-whitespace content.
fn non_blank_lines(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Count `marker` occurrences in a grid snapshot, robust to terminal
/// soft-wrap. [`Session::to_text`] emits one physical grid row per line
/// and right-trims each, so a marker the terminal wrapped across the
/// right edge is split by a `\n` (`E2E_MARKE` ⏎ `R`) and a naive
/// per-line `str::matches` under-counts it. A command round-trip is a
/// LOGICAL event — the marker is echoed on the input line and again as
/// output — so a wrap at the display layer must not read as a missing
/// occurrence. Counting in the de-wrapped view (physical row breaks
/// removed) rejoins a split marker; because the wrapped row is full by
/// construction (no trailing space to trim) the two halves abut exactly.
/// De-wrapping can only RECONNECT a marker the grid physically split —
/// it never fabricates the second (output) occurrence the row requires,
/// so a shell that echoes but never executes still counts 1 and fails.
fn marker_occurrences(text: &str, marker: &str) -> usize {
    text.replace('\n', "").matches(marker).count()
}

/// Last non-blank line, for row-detail evidence.
fn last_non_blank_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    /// 2026-06-10 regression: the driver must NEVER hand a spawned shell
    /// the operator's real HOME/HISTFILE — live e2e runs polluted the real
    /// shell history with test commands (incl. escape-sequence printf
    /// bombs served back via Ctrl-R).
    #[test]
    fn hermetic_env_never_points_at_real_home() {
        let real_home = std::env::var("HOME").unwrap_or_default();
        let env = super::hermetic_shell_env(&[("WADACHI_DB", "/tmp/x.db".into())]);
        let map = env.as_object().expect("object env");
        let home = map["HOME"].as_str().expect("HOME set");
        assert!(!home.is_empty() && home != real_home, "hermetic HOME must differ from real");
        assert!(std::path::Path::new(home).is_dir(), "hermetic HOME is created");
        let hist = map["HISTFILE"].as_str().expect("HISTFILE set");
        assert!(hist.starts_with(home), "HISTFILE lives under the hermetic HOME");
        for k in ["XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_CONFIG_HOME"] {
            assert!(map[k].as_str().unwrap().starts_with(home), "{k} under hermetic HOME");
        }
        assert_eq!(map["WADACHI_DB"].as_str().unwrap(), "/tmp/x.db", "extras merge");
    }

    /// Two calls must not share a HOME (parallel rows stay isolated).
    #[test]
    fn hermetic_env_is_unique_per_call() {
        let a = super::hermetic_shell_env(&[]);
        let b = super::hermetic_shell_env(&[]);
        assert_ne!(a["HOME"], b["HOME"]);
    }

    use super::*;

    #[test]
    fn non_blank_lines_counts_content_rows_only() {
        assert_eq!(non_blank_lines(""), 0);
        assert_eq!(non_blank_lines("\n\n   \n"), 0);
        assert_eq!(non_blank_lines("$ \n\nhello\n   \n"), 2);
    }

    #[test]
    fn marker_occurrences_counts_across_soft_wrap() {
        // The 2026-06-21 ryn gate false-negative, verbatim: a ~66-col
        // frostmourne prompt in an 80-col session soft-wrapped the
        // echoed `echo E2E_MARKER`, splitting the marker across a grid-
        // row break (`E2E_MARKE` ⏎ `R`) while the command OUTPUT line
        // kept it intact. A per-line count sees 1; the LOGICAL round-
        // trip (input echo + output) is 2.
        let grid = "~ mado-1782083362-41220 · ryn · github/pleme-io/nix main ❄ echo E2E_MARKE\n\
                    R\n\
                    E2E_MARKER\n\
                    ~ mado-1782083362-41220 · ryn · github/pleme-io/nix main ❄";
        assert_eq!(
            grid.matches("E2E_MARKER").count(),
            1,
            "physical grid under-counts the wrapped echo"
        );
        assert_eq!(
            marker_occurrences(grid, "E2E_MARKER"),
            2,
            "de-wrapped count restores the logical round-trip"
        );
    }

    #[test]
    fn marker_occurrences_never_fabricates_a_round_trip() {
        // A shell that echoes the command but never executes it (wedged
        // before output) leaves the marker exactly once — wrapped or
        // not — so the row must still fail. De-wrapping reconnects a
        // split occurrence; it never invents the second (output) one.
        let typed_then_wedged = "prompt ❄ echo E2E_MARKE\nR";
        assert_eq!(marker_occurrences(typed_then_wedged, "E2E_MARKER"), 1);
        // A dead shell paints no marker at all.
        let dead = "prompt ❄";
        assert_eq!(marker_occurrences(dead, "E2E_MARKER"), 0);
        // The clean (un-wrapped) case still counts both occurrences.
        let clean = "prompt ❄ echo E2E_MARKER\nE2E_MARKER\nprompt ❄";
        assert_eq!(marker_occurrences(clean, "E2E_MARKER"), 2);
    }

    #[test]
    fn last_non_blank_line_finds_the_prompt() {
        assert_eq!(last_non_blank_line("a\n$ \n  \n"), "$ ");
        assert_eq!(last_non_blank_line("   \n"), "");
    }

    #[test]
    fn skipped_rows_fail_with_reason() {
        // Dependency-skips are FAILURES (skipped: false) — the matrix
        // is red from the dependency row and must stay red.
        let row = skipped("echo_marker", "spawn_term failed");
        assert!(!row.pass);
        assert!(!row.skipped);
        assert_eq!(row.detail, "skipped: spawn_term failed");
    }

    #[test]
    fn environment_skips_are_non_fatal_but_dependency_skips_fail() {
        // Typed environment-skip (no wadachi hook) keeps the summary
        // green; a dependency-skip in the same matrix kills it.
        let green = [
            RowResult::passed("spawn_term", "ok".into()),
            RowResult::env_skipped("single_recorder", "skipped: no hook".into()),
        ];
        assert!(summary_pass(&green));
        let red = [
            RowResult::failed("spawn_term", "boom".into()),
            skipped("single_recorder", "spawn_term failed"),
        ];
        assert!(!summary_pass(&red));
    }

    #[test]
    fn summary_serializes_to_the_documented_shape() {
        // The wire contract `{shell, rows: [{name, pass, skipped,
        // detail}], pass}` is what the nix `.#e2e-mado` app + CI
        // parse — pin it.
        let summary = E2eSummary {
            shell: "/bin/sh".into(),
            rows: vec![RowResult::passed("spawn_term", "session_id=abc".into())],
            pass: true,
        };
        let v = serde_json::to_value(&summary).unwrap();
        assert_eq!(v["shell"], "/bin/sh");
        assert_eq!(v["pass"], true);
        assert_eq!(v["rows"][0]["name"], "spawn_term");
        assert_eq!(v["rows"][0]["pass"], true);
        assert_eq!(v["rows"][0]["skipped"], false);
        assert!(v["rows"][0]["detail"].is_string());
    }

    #[test]
    fn visits_for_counts_only_the_nonce_dir() {
        // Real schema, throwaway store: two visits to the nonce dir
        // under DIFFERENT canonicalizations (/tmp vs /private/tmp)
        // both count; an unrelated dir doesn't.
        use pleme_io_wadachi::{DirFrecencyDb, DirStore};
        let dir = std::env::temp_dir().join(format!(
            "mado-e2e-visits-for-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("wadachi.db");
        {
            let store = DirFrecencyDb::open(&db).unwrap();
            store.record("/tmp/e2e-sr-nonce").unwrap();
            store.record("/private/tmp/e2e-sr-nonce").unwrap();
            store.record("/tmp/unrelated").unwrap();
        }
        assert_eq!(visits_for(&db, "/tmp/e2e-sr-nonce").unwrap(), 2);
        assert_eq!(visits_for(&db, "/tmp/never-visited").unwrap(), 0);
        // Absent store = zero visits, and the read must NOT create it.
        let missing = dir.join("missing.db");
        assert_eq!(visits_for(&missing, "/tmp/e2e-sr-nonce").unwrap(), 0);
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
