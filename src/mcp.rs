//! MCP server for mado terminal emulator.
//!
//! Provides tools for inspecting and controlling terminal sessions,
//! sending keystrokes, reading output, and managing panes/tabs.

use std::sync::{Arc, Mutex};

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;

use crate::clipboard_store::{ClipboardHash, ClipboardStore};
use crate::term_spec::TermSpec;

// ── Tool input types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct SessionIdInput {
    #[schemars(description = "Session identifier (pane or tab ID).")]
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendKeysInput {
    #[schemars(description = "Session identifier (pane or tab ID). Use 'active' for the focused session.")]
    session_id: String,
    #[schemars(description = "Keystrokes to send to the session. Supports escape sequences (e.g., '\\n' for Enter).")]
    keys: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetOutputInput {
    #[schemars(description = "Session identifier (pane or tab ID). Use 'active' for the focused session.")]
    session_id: String,
    #[schemars(description = "Number of recent lines to retrieve (default: 50).")]
    lines: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SplitPaneInput {
    #[schemars(description = "Split direction: 'horizontal' or 'vertical'.")]
    direction: String,
    #[schemars(description = "Optional command to run in the new pane.")]
    command: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConfigGetInput {
    #[schemars(description = "Config key to retrieve (e.g., 'font_size', 'theme'). Omit for full config.")]
    key: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConfigSetInput {
    #[schemars(description = "Config key to set (e.g., 'font_size', 'theme').")]
    key: String,
    #[schemars(description = "Value to set (as JSON string).")]
    value: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClipboardGetInput {
    #[schemars(description = "32-char lowercase BLAKE3-128 hex hash (matches the token escriba's `defsnippet :hash \"…\"` uses).")]
    hash: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClipboardListInput {
    #[schemars(description = "Maximum number of entries to return — most recent first. Omit for the full list.")]
    limit: Option<u32>,
    #[schemars(description = "If true, include the full payload `content` in each entry. Defaults to false — only preview + hash are returned so the response stays compact.")]
    include_content: Option<bool>,
}

// ── MCP Server ──────────────────────────────────────────────────────────────

/// Shared handle on the cross-session content-addressed clipboard
/// mirror the MCP server exposes. Wrapping in `Arc<Mutex<_>>` means
/// multiple tool handlers (and the future IPC bridge that feeds this
/// from the live Terminal) can hold references without cloning the
/// store itself.
type SharedClipboard = Arc<Mutex<ClipboardStore>>;

#[derive(Debug, Clone)]
struct MadoMcp {
    tool_router: ToolRouter<Self>,
    /// Content-addressed clipboard shared with (eventually) every
    /// Terminal's OSC 52 pipe. For now the server owns the only copy
    /// — the IPC bridge will merge Terminal-side stores on demand.
    clipboard: SharedClipboard,
}

#[tool_router]
impl MadoMcp {
    fn new() -> Self {
        Self::with_clipboard(Arc::new(Mutex::new(ClipboardStore::new(128))))
    }

    /// Construct with an externally-owned clipboard store — lets the
    /// binary hand over the same store its Terminal populates, so
    /// reads see live session state.
    fn with_clipboard(clipboard: SharedClipboard) -> Self {
        Self {
            tool_router: Self::tool_router(),
            clipboard,
        }
    }

    // ── Standard tools ──────────────────────────────────────────────────────

    #[tool(description = "Get mado application status and health information. Returns JSON with running state, session count, and uptime.")]
    async fn status(&self) -> String {
        serde_json::json!({
            "status": "running",
            "app": "mado",
            "sessions": 0,
            "note": "MCP server is operational. GUI state queries require a running mado instance with IPC."
        })
        .to_string()
    }

    #[tool(description = "Get mado version information. Returns JSON with version, build, and feature details.")]
    async fn version(&self) -> String {
        serde_json::json!({
            "name": "mado",
            "version": env!("CARGO_PKG_VERSION"),
            "description": env!("CARGO_PKG_DESCRIPTION"),
            "renderer": "wgpu (Metal/Vulkan)",
            "terminal_emulation": "vte (VT100/xterm)",
        })
        .to_string()
    }

    #[tool(description = "Get a mado configuration value. Pass a key for a specific value, or omit for the full config.")]
    async fn config_get(&self, Parameters(input): Parameters<ConfigGetInput>) -> String {
        match input.key {
            Some(key) => serde_json::json!({
                "key": key,
                "value": null,
                "note": "Config queries require IPC to a running mado instance."
            })
            .to_string(),
            None => serde_json::json!({
                "note": "Full config retrieval requires IPC to a running mado instance.",
                "config_path": "~/.config/mado/mado.yaml"
            })
            .to_string(),
        }
    }

    #[tool(description = "Set a mado configuration value at runtime. Changes take effect immediately via hot-reload.")]
    async fn config_set(&self, Parameters(input): Parameters<ConfigSetInput>) -> String {
        serde_json::json!({
            "key": input.key,
            "value": input.value,
            "applied": false,
            "note": "Config mutations require IPC to a running mado instance."
        })
        .to_string()
    }

    // ── Terminal-specific tools ─────────────────────────────────────────────

    #[tool(description = "List all open terminal sessions (panes and tabs). Returns JSON array with session IDs, titles, working directories, and dimensions.")]
    async fn list_sessions(&self) -> String {
        serde_json::json!({
            "sessions": [],
            "note": "Session listing requires IPC to a running mado instance."
        })
        .to_string()
    }

    #[tool(description = "Send keystrokes to a specific terminal session. Supports escape sequences for special keys.")]
    async fn send_keys(&self, Parameters(input): Parameters<SendKeysInput>) -> String {
        serde_json::json!({
            "session_id": input.session_id,
            "keys_sent": input.keys,
            "ok": false,
            "note": "Keystroke delivery requires IPC to a running mado instance."
        })
        .to_string()
    }

    #[tool(description = "Get recent terminal output from a session. Returns the last N lines of visible and scrollback content.")]
    async fn get_output(&self, Parameters(input): Parameters<GetOutputInput>) -> String {
        let lines = input.lines.unwrap_or(50);
        serde_json::json!({
            "session_id": input.session_id,
            "lines_requested": lines,
            "output": [],
            "note": "Output retrieval requires IPC to a running mado instance."
        })
        .to_string()
    }

    #[tool(description = "Create a new split pane in the active tab. Specify horizontal or vertical split direction.")]
    async fn split_pane(&self, Parameters(input): Parameters<SplitPaneInput>) -> String {
        serde_json::json!({
            "direction": input.direction,
            "command": input.command,
            "ok": false,
            "note": "Pane creation requires IPC to a running mado instance."
        })
        .to_string()
    }

    #[tool(description = "Create a new tab in the terminal window. Optionally run a command in the new tab.")]
    async fn new_tab(&self) -> String {
        serde_json::json!({
            "ok": false,
            "note": "Tab creation requires IPC to a running mado instance."
        })
        .to_string()
    }

    // ── Typed session spawning — the escriba integration surface ─────────────
    //
    // `spawn_term` is the MCP tool escriba (and any other typed client)
    // calls when it wants mado to open a new session from a declarative
    // spec. The JSON schema advertised to clients is `TermSpec` — every
    // field has a default so the minimal payload is `{}`. This mirrors
    // the escriba-lisp `defterm` form authored in the rc.
    #[tool(description = "Spawn a terminal session from a typed TermSpec. Fields: shell, args, cwd, env, title, placement (tab/split-horizontal/split-vertical/window), attach (existing session id), effects (shader names). All optional — empty spec opens a default shell in a new tab.")]
    async fn spawn_term(&self, Parameters(spec): Parameters<TermSpec>) -> String {
        serde_json::json!({
            "ok": false,
            "placement": format!("{:?}", spec.resolved_placement()),
            "title": spec.display_title(),
            "is_attach": spec.is_attach(),
            "note": "Session spawning requires IPC to a running mado instance. Spec accepted + parsed."
        })
        .to_string()
    }

    // ── Content-addressed clipboard — the escriba snippet integration ────────
    //
    // Mado mirrors every OSC 52 payload into a BLAKE3-indexed
    // `ClipboardStore`. These tools expose that store to any typed
    // client — chief consumer is escriba's `defsnippet :hash "…"`
    // form, which resolves the body by asking mado for the payload
    // associated with the hash. No editor / terminal pair in the
    // category ships this: the hash is the API.

    #[tool(description = "Fetch a clipboard payload by its 32-char BLAKE3-128 hash. Returns `{found, hash, content, kind, set_at}` on hit; `{found: false, hash}` when the hash isn't in the session store. Used by escriba's `defsnippet :hash \"…\"` to resolve snippet bodies without copying bytes across the socket.")]
    async fn clipboard_get(&self, Parameters(input): Parameters<ClipboardGetInput>) -> String {
        let Some(hash) = ClipboardHash::from_hex(&input.hash) else {
            return serde_json::json!({
                "found": false,
                "hash": input.hash,
                "error": "malformed-hash",
                "note": "hash must be 32 lowercase hex chars (BLAKE3-128)"
            })
            .to_string();
        };
        let guard = self.clipboard.lock().expect("clipboard lock poisoned");
        match guard.get(hash) {
            Some(entry) => serde_json::json!({
                "found": true,
                "hash": entry.hash.to_hex(),
                "content": entry.content,
                "kind": entry.kind.label(),
                "set_at": entry.set_at,
            })
            .to_string(),
            None => serde_json::json!({
                "found": false,
                "hash": input.hash,
            })
            .to_string(),
        }
    }

    #[tool(description = "List clipboard payloads the session has seen, most-recent-first. Returns `{count, entries: [{hash, preview, bytes, kind, set_at}]}`. Set `include_content: true` to also pull the full payload (for scripted pipelines); default is preview-only to keep the response compact.")]
    async fn clipboard_list(&self, Parameters(input): Parameters<ClipboardListInput>) -> String {
        let include_content = input.include_content.unwrap_or(false);
        let limit = input.limit.map(|n| n as usize);
        let guard = self.clipboard.lock().expect("clipboard lock poisoned");
        let iter = guard.entries_recent_first();
        let entries: Vec<serde_json::Value> = match limit {
            Some(n) => iter.take(n).map(|e| entry_json(e, include_content)).collect(),
            None => iter.map(|e| entry_json(e, include_content)).collect(),
        };
        serde_json::json!({
            "count": entries.len(),
            "total": guard.len(),
            "entries": entries,
        })
        .to_string()
    }
}

/// Render one [`ClipboardEntry`] as the MCP wire shape. `preview`
/// is always the first 60 chars of the payload with newlines folded
/// into `⏎` so callers can eyeball an entry without pulling the full
/// body. `bytes` is the payload's byte length — lets clients decide
/// whether to request `include_content: true` on the next call.
fn entry_json(
    entry: &crate::clipboard_store::ClipboardEntry,
    include_content: bool,
) -> serde_json::Value {
    let preview = preview_from(&entry.content);
    let bytes = entry.content.len();
    if include_content {
        serde_json::json!({
            "hash": entry.hash.to_hex(),
            "preview": preview,
            "bytes": bytes,
            "content": entry.content,
            "kind": entry.kind.label(),
            "set_at": entry.set_at,
        })
    } else {
        serde_json::json!({
            "hash": entry.hash.to_hex(),
            "preview": preview,
            "bytes": bytes,
            "kind": entry.kind.label(),
            "set_at": entry.set_at,
        })
    }
}

/// Build the `preview` field — up to 60 chars, newlines rendered as
/// `⏎` so the preview stays single-line in the MCP response.
fn preview_from(content: &str) -> String {
    const MAX: usize = 60;
    let mut out = String::with_capacity(MAX + 4);
    let mut taken = 0;
    for ch in content.chars() {
        if taken >= MAX {
            out.push('…');
            break;
        }
        match ch {
            '\n' => out.push('⏎'),
            '\r' => {}
            c => out.push(c),
        }
        taken += 1;
    }
    out
}

#[tool_handler]
impl ServerHandler for MadoMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Mado GPU terminal emulator — session management, keystroke delivery, and output capture."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_server() -> MadoMcp {
        MadoMcp::new()
    }

    #[tokio::test]
    async fn mcp_status_json() {
        let server = new_server();
        let result = server.status().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "running");
        assert_eq!(parsed["app"], "mado");
        assert!(parsed["sessions"].is_number());
    }

    #[tokio::test]
    async fn mcp_version_json() {
        let server = new_server();
        let result = server.version().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["name"], "mado");
        assert!(parsed["version"].is_string());
        assert!(parsed["renderer"].is_string());
    }

    #[tokio::test]
    async fn mcp_list_sessions_json() {
        let server = new_server();
        let result = server.list_sessions().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["sessions"].is_array());
    }

    #[tokio::test]
    async fn mcp_config_get_with_key() {
        let server = new_server();
        let input = ConfigGetInput { key: Some("font_size".to_string()) };
        let result = server.config_get(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "font_size");
    }

    #[tokio::test]
    async fn mcp_config_get_without_key() {
        let server = new_server();
        let input = ConfigGetInput { key: None };
        let result = server.config_get(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["config_path"].is_string());
    }

    #[tokio::test]
    async fn mcp_send_keys_json() {
        let server = new_server();
        let input = SendKeysInput {
            session_id: "active".to_string(),
            keys: "ls\\n".to_string(),
        };
        let result = server.send_keys(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["session_id"], "active");
        assert!(parsed["keys_sent"].is_string());
    }

    #[tokio::test]
    async fn mcp_get_output_json() {
        let server = new_server();
        let input = GetOutputInput {
            session_id: "active".to_string(),
            lines: Some(10),
        };
        let result = server.get_output(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["session_id"], "active");
        assert_eq!(parsed["lines_requested"], 10);
        assert!(parsed["output"].is_array());
    }

    #[tokio::test]
    async fn mcp_split_pane_json() {
        let server = new_server();
        let input = SplitPaneInput {
            direction: "vertical".to_string(),
            command: None,
        };
        let result = server.split_pane(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["direction"], "vertical");
    }

    #[tokio::test]
    async fn mcp_new_tab_json() {
        let server = new_server();
        let result = server.new_tab().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("ok").is_some());
    }

    // ── Clipboard tools — round-trip through the shared store ────────────────

    use crate::clipboard_store::ClipboardKind;

    fn server_with_seeded_clipboard(payloads: &[(&str, ClipboardKind)]) -> (MadoMcp, Vec<String>) {
        let store = Arc::new(Mutex::new(ClipboardStore::new(64)));
        let mut hashes = Vec::new();
        {
            let mut guard = store.lock().unwrap();
            for (content, kind) in payloads {
                let hash = guard.store((*content).to_string(), *kind);
                hashes.push(hash.to_hex());
            }
        }
        (MadoMcp::with_clipboard(store), hashes)
    }

    #[tokio::test]
    async fn clipboard_get_resolves_known_hash() {
        let (server, hashes) = server_with_seeded_clipboard(&[
            ("deploy.sh --prod", ClipboardKind::System),
            ("kubectl logs -f", ClipboardKind::System),
        ]);
        let hash = hashes[0].clone();
        let input = ClipboardGetInput { hash: hash.clone() };
        let result = server.clipboard_get(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["found"], true);
        assert_eq!(parsed["content"], "deploy.sh --prod");
        assert_eq!(parsed["hash"], hash);
        assert_eq!(parsed["kind"], "c");
    }

    #[tokio::test]
    async fn clipboard_get_reports_miss_without_content() {
        let (server, _) = server_with_seeded_clipboard(&[]);
        let input = ClipboardGetInput {
            hash: "af42c0d18e9b3f4aa18b7c3ef1de93a4".into(),
        };
        let result = server.clipboard_get(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["found"], false);
        assert!(parsed.get("content").is_none());
    }

    #[tokio::test]
    async fn clipboard_get_rejects_malformed_hash() {
        let (server, _) = server_with_seeded_clipboard(&[]);
        let input = ClipboardGetInput { hash: "too-short".into() };
        let result = server.clipboard_get(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["found"], false);
        assert_eq!(parsed["error"], "malformed-hash");
    }

    #[tokio::test]
    async fn clipboard_list_returns_preview_by_default() {
        let (server, _) = server_with_seeded_clipboard(&[
            ("payload one", ClipboardKind::System),
            ("payload two", ClipboardKind::Primary),
        ]);
        let input = ClipboardListInput {
            limit: None,
            include_content: None,
        };
        let result = server.clipboard_list(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 2);
        assert_eq!(parsed["total"], 2);
        let entries = parsed["entries"].as_array().unwrap();
        // Most-recent first.
        assert_eq!(entries[0]["preview"], "payload two");
        assert_eq!(entries[0]["kind"], "p");
        assert_eq!(entries[1]["preview"], "payload one");
        // Content is NOT included by default.
        assert!(entries[0].get("content").is_none());
        // Bytes are always present.
        assert_eq!(entries[0]["bytes"], "payload two".len());
    }

    #[tokio::test]
    async fn clipboard_list_honours_limit_and_include_content() {
        let (server, _) = server_with_seeded_clipboard(&[
            ("a", ClipboardKind::System),
            ("b", ClipboardKind::System),
            ("c", ClipboardKind::System),
        ]);
        let input = ClipboardListInput {
            limit: Some(2),
            include_content: Some(true),
        };
        let result = server.clipboard_list(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 2);
        // Total reflects the underlying store, not the returned slice.
        assert_eq!(parsed["total"], 3);
        let entries = parsed["entries"].as_array().unwrap();
        assert_eq!(entries[0]["content"], "c");
        assert_eq!(entries[1]["content"], "b");
    }

    #[test]
    fn preview_from_folds_newlines_and_truncates() {
        // Newlines render as ⏎; \r drops entirely; >60 chars gets …
        // The 60-char cap is measured in *chars*, not bytes, so
        // multibyte input doesn't truncate mid-codepoint.
        let folded = preview_from("line-a\nline-b\r\nline-c");
        assert!(!folded.contains('\n'));
        assert!(!folded.contains('\r'));
        assert!(folded.contains('⏎'));

        let long = "x".repeat(80);
        let trunc = preview_from(&long);
        assert!(trunc.ends_with('…'));
        // 60 'x' chars + the ellipsis.
        assert_eq!(trunc.chars().count(), 61);
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let server = MadoMcp::new().serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
