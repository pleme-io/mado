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
//! 2. `prompt_visible` — non-blank grid content within 10s
//! 3. `enter_fresh_prompt` — Enter → shell still interactive, a
//!    fresh prompt line appears (the E5 death point)
//! 4. `echo_marker`  — `echo E2E_MARKER` round-trips: the marker
//!    appears ≥2× (command echo + command output)
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
/// 10s"; the same ceiling bounds every round-trip row.
const ROW_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll cadence while waiting on grid content.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Sentinel for the echo round-trip row. Must appear ≥2× in the
/// grid: once as the echoed command line, once as command output.
const E2E_MARKER: &str = "E2E_MARKER";

/// One smoke-matrix row outcome. `detail` carries the human-readable
/// evidence (elapsed ms, grid excerpts, skip reason).
#[derive(Debug, Serialize)]
pub struct RowResult {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
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
    let session_id = match spawn_term(&client, shell).await {
        Ok(id) => {
            rows.push(RowResult {
                name: "spawn_term",
                pass: true,
                detail: format!("session_id={id} shell={shell}"),
            });
            Some(id)
        }
        Err(e) => {
            rows.push(RowResult {
                name: "spawn_term",
                pass: false,
                detail: format!("{e:#}"),
            });
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
                    rows.push(RowResult {
                        name: "prompt_visible",
                        pass: true,
                        detail: format!(
                            "prompt rendered in {}ms (last line: {:?})",
                            started.elapsed().as_millis(),
                            last_non_blank_line(&text),
                        ),
                    });
                }
                Err(e) => rows.push(RowResult {
                    name: "prompt_visible",
                    pass: false,
                    detail: format!("no non-blank grid content within {ROW_TIMEOUT:?}: {e:#}"),
                }),
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
                    rows.push(RowResult {
                        name: "enter_fresh_prompt",
                        pass: true,
                        detail: format!(
                            "non-blank lines {before} → {after} after Enter (shell survived, fresh prompt)"
                        ),
                    });
                }
                Err(e) => rows.push(RowResult {
                    name: "enter_fresh_prompt",
                    pass: false,
                    detail: format!(
                        "no fresh prompt within {ROW_TIMEOUT:?} after Enter (E5 class — shell dead or frozen): {e:#}"
                    ),
                }),
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
                        text.matches(E2E_MARKER).count() >= 2
                    })
                    .await
                    {
                        Ok(text) => rows.push(RowResult {
                            name: "echo_marker",
                            pass: true,
                            detail: format!(
                                "{} occurrences of {E2E_MARKER} (echo + output)",
                                text.matches(E2E_MARKER).count()
                            ),
                        }),
                        Err(e) => rows.push(RowResult {
                            name: "echo_marker",
                            pass: false,
                            detail: format!(
                                "marker did not round-trip ≥2× within {ROW_TIMEOUT:?}: {e:#}"
                            ),
                        }),
                    }
                }
                Err(e) => rows.push(RowResult {
                    name: "echo_marker",
                    pass: false,
                    detail: format!("send_keys failed: {e:#}"),
                }),
            }
        }
        Some(_) => rows.push(skipped("echo_marker", "enter_fresh_prompt failed")),
        None => rows.push(skipped("echo_marker", "spawn_term failed")),
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

    let pass = rows.iter().all(|r| r.pass);
    Ok(E2eSummary {
        shell: shell.to_string(),
        rows,
        pass,
    })
}

/// Uniform "dependency failed" row — keeps the matrix complete (every
/// row reported every run) without faking a pass.
fn skipped(name: &'static str, dependency: &str) -> RowResult {
    RowResult {
        name,
        pass: false,
        detail: format!("skipped: {dependency}"),
    }
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

/// `spawn_term` with the matrix shell; returns the session id.
async fn spawn_term(client: &Client, shell: &str) -> Result<String> {
    let v = call_tool(client, "spawn_term", serde_json::json!({ "shell": shell })).await?;
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

/// Poll `get_output` until `accept` holds or [`ROW_TIMEOUT`] elapses.
/// Returns the accepted grid text; errors with the LAST observed grid
/// so a failed row's detail shows what the terminal actually rendered.
async fn wait_for_output<F>(client: &Client, session_id: &str, accept: F) -> Result<String>
where
    F: Fn(&str) -> bool,
{
    let deadline = Instant::now() + ROW_TIMEOUT;
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
    use super::*;

    #[test]
    fn non_blank_lines_counts_content_rows_only() {
        assert_eq!(non_blank_lines(""), 0);
        assert_eq!(non_blank_lines("\n\n   \n"), 0);
        assert_eq!(non_blank_lines("$ \n\nhello\n   \n"), 2);
    }

    #[test]
    fn last_non_blank_line_finds_the_prompt() {
        assert_eq!(last_non_blank_line("a\n$ \n  \n"), "$ ");
        assert_eq!(last_non_blank_line("   \n"), "");
    }

    #[test]
    fn skipped_rows_fail_with_reason() {
        let row = skipped("echo_marker", "spawn_term failed");
        assert!(!row.pass);
        assert_eq!(row.detail, "skipped: spawn_term failed");
    }

    #[test]
    fn summary_serializes_to_the_documented_shape() {
        // The wire contract `{shell, rows: [{name, pass, detail}], pass}`
        // is what the nix `.#e2e-mado` app + CI parse — pin it.
        let summary = E2eSummary {
            shell: "/bin/sh".into(),
            rows: vec![RowResult {
                name: "spawn_term",
                pass: true,
                detail: "session_id=abc".into(),
            }],
            pass: true,
        };
        let v = serde_json::to_value(&summary).unwrap();
        assert_eq!(v["shell"], "/bin/sh");
        assert_eq!(v["pass"], true);
        assert_eq!(v["rows"][0]["name"], "spawn_term");
        assert_eq!(v["rows"][0]["pass"], true);
        assert!(v["rows"][0]["detail"].is_string());
    }
}
