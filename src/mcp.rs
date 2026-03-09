//! MCP server for mado terminal emulator.
//!
//! Provides tools for inspecting and controlling terminal sessions,
//! sending keystrokes, reading output, and managing panes/tabs.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;

// ── Tool input types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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

// ── MCP Server ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct MadoMcp {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MadoMcp {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
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

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let server = MadoMcp::new().serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
