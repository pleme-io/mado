//! Cross-process MCP integration test: spawn `mado mcp` as a child
//! process, drive it via the rmcp client + child-process transport,
//! and exercise the full tear_* tool surface end-to-end.
//!
//! Proves the live wire path:
//!   rmcp client (in this test)
//!     → JSON-RPC over stdio
//!       → mado bin's stdio MCP server
//!         → mcp.rs tear_* tool handlers
//!           → tear-client::Client
//!             → UDS to tear-daemon (also in this test process)
//!               → InProcess multiplexer
//!
//! The in-process mado MCP unit tests in `src/mcp.rs::tests` cover
//! the tear_* tool bodies; this test proves the entire transport
//! chain — tool registration, JSON-RPC encoding, MADO_CONFIG
//! propagation, child process lifecycle — works end-to-end.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::process::Command;

/// Extract the first text-shaped Content payload from a tool result
/// — every tear_* tool emits a single text payload (a JSON string).
fn first_text(result: &rmcp::model::CallToolResult) -> String {
    use rmcp::model::RawContent;
    for c in &result.content {
        if let RawContent::Text(t) = &c.raw {
            return t.text.clone();
        }
    }
    panic!("tool result had no text content: {result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mado_mcp_child_exposes_tear_tools_and_drives_a_real_daemon() {
    // ── 1. in-process tear-daemon ────────────────────────────────
    let socket = {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        p.push(format!("mado-mcp-xproc-{pid}.sock"));
        p
    };
    let _ = std::fs::remove_file(&socket);
    let inproc = Arc::new(tear_core::InProcess::new());
    let live = Arc::new(tear_config::LiveConfig::default());
    let daemon =
        tear_daemon::start_with_config(socket.clone(), inproc, live.clone())
            .expect("tear-daemon should start");
    // Tiny settle for the accept loop.
    std::thread::sleep(Duration::from_millis(50));

    // ── 2. temp MADO_CONFIG pointing at the daemon ───────────────
    // `.yaml` suffix is load-bearing — shikumi auto-detects format
    // from the extension; without it the file is parsed as TOML
    // and discovery silently falls back to defaults.
    use std::io::Write;
    let mut cfg = tempfile::Builder::new()
        .prefix("mado-mcp-xproc-")
        .suffix(".yaml")
        .tempfile()
        .unwrap();
    writeln!(
        cfg,
        "tear:\n  mode: auto\n  socket: {}\n  auto_spawn: false\n",
        socket.display(),
    )
    .unwrap();
    cfg.flush().unwrap();

    // ── 3. spawn `mado mcp` as a subprocess ──────────────────────
    let mado_bin = env!("CARGO_BIN_EXE_mado");
    let mut cmd = Command::new(mado_bin);
    cmd.arg("mcp")
        .env("MADO_CONFIG", cfg.path())
        // The mado bin's tracing-subscriber writes to stderr; the
        // rmcp transport only owns stdin/stdout, so stderr noise
        // can't corrupt the JSON-RPC framing. Pin RUST_LOG so a
        // shell with a debug-level default doesn't drown the test
        // output if the test fails for an unrelated reason.
        .env("RUST_LOG", "error");
    let transport = TokioChildProcess::new(cmd).expect("spawn mado mcp");
    let client = ().serve(transport).await.expect("rmcp client serve");

    // ── 4. list_all_tools — every tear_* tool must be registered ─
    let tools = client.list_all_tools().await.expect("list_tools");
    let names: std::collections::HashSet<String> =
        tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "tear_status",
        "tear_get_config",
        "tear_set_config_yaml",
        "tear_reload_config",
        "tear_list_sessions",
        "tear_new_session",
        "tear_kill_session",
        "tear_pane_snapshot",
        "tear_send_keys",
    ] {
        assert!(
            names.contains(expected),
            "expected MCP tool {expected} not registered; \
             registered tools were {names:?}"
        );
    }

    // ── 5. call tear_status — daemon should be reachable ─────────
    let result = client
        .call_tool(CallToolRequestParams {
            name: "tear_status".into(),
            arguments: None,
            meta: None,
            task: None,
        })
        .await
        .expect("call_tool(tear_status)");
    let v: serde_json::Value = serde_json::from_str(&first_text(&result)).unwrap();
    assert_eq!(
        v["reachable"], true,
        "tear_status via MCP should see the daemon: {v}"
    );

    // ── 6. tear_new_session via MCP — daemon-side LiveConfig
    //      should see it via direct Arc<LiveConfig> read ──────────
    let mut args = serde_json::Map::new();
    args.insert("name".into(), serde_json::json!("mcp-xproc-session"));
    args.insert("shell".into(), serde_json::json!("/bin/sh"));
    let result = client
        .call_tool(CallToolRequestParams {
            name: "tear_new_session".into(),
            arguments: Some(args),
            meta: None,
            task: None,
        })
        .await
        .expect("call_tool(tear_new_session)");
    let v: serde_json::Value = serde_json::from_str(&first_text(&result)).unwrap();
    assert_eq!(v["ok"], true, "tear_new_session via MCP failed: {v}");
    let session_id = v["session_id"].as_str().unwrap().to_owned();
    let pane_id = v["first_pane_id"].as_str().unwrap().to_owned();

    // ── 7. tear_send_keys + tear_pane_snapshot end-to-end ────────
    let mut send_args = serde_json::Map::new();
    send_args.insert("pane_id".into(), serde_json::json!(pane_id));
    send_args.insert(
        "keys".into(),
        serde_json::json!("printf 'MCP_XPROC_MARKER\\n'\n"),
    );
    let _ = client
        .call_tool(CallToolRequestParams {
            name: "tear_send_keys".into(),
            arguments: Some(send_args),
            meta: None,
            task: None,
        })
        .await
        .expect("call_tool(tear_send_keys)");

    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    let mut saw_marker = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut snap_args = serde_json::Map::new();
        snap_args.insert("pane_id".into(), serde_json::json!(pane_id));
        let snap = client
            .call_tool(CallToolRequestParams {
                name: "tear_pane_snapshot".into(),
                arguments: Some(snap_args),
                meta: None,
                task: None,
            })
            .await
            .expect("call_tool(tear_pane_snapshot)");
        let v: serde_json::Value = serde_json::from_str(&first_text(&snap)).unwrap();
        if let Some(rows) = v["text_rows"].as_array() {
            let text = rows
                .iter()
                .filter_map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.contains("MCP_XPROC_MARKER") {
                saw_marker = true;
                break;
            }
        }
    }
    assert!(
        saw_marker,
        "MCP_XPROC_MARKER never appeared in tear_pane_snapshot via cross-process MCP"
    );

    // ── 8. tear_kill_session via MCP — clean up ──────────────────
    let mut kill_args = serde_json::Map::new();
    kill_args.insert("session_id".into(), serde_json::json!(session_id));
    let result = client
        .call_tool(CallToolRequestParams {
            name: "tear_kill_session".into(),
            arguments: Some(kill_args),
            meta: None,
            task: None,
        })
        .await
        .expect("call_tool(tear_kill_session)");
    let v: serde_json::Value = serde_json::from_str(&first_text(&result)).unwrap();
    assert_eq!(v["ok"], true, "tear_kill_session via MCP failed: {v}");

    // ── 9. shutdown ──────────────────────────────────────────────
    client.cancel().await.ok();
    daemon.stop();
}
