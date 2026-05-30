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
use crate::osc_1337::UserMarkHistory;
use crate::prompt_mark::PromptHistory;
use crate::session::SessionRegistry;
use crate::term_spec::TermSpec;

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

// SplitPaneInput removed at Phase 4 — multiplexing belongs in
// tear (see theory/MADO-TEAR-M5.md). Use `tear-client::Client::
// split_pane` or invoke `tear` from the shell.

// ── tear MCP bridge input shapes ───────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearSetConfigYamlInput {
    #[schemars(description = "TearConfig YAML payload — same shape as ~/.config/tear/tear.yaml.")]
    yaml: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct TearNewSessionInput {
    #[schemars(description = "Operator-visible session name. Defaults to 'mcp-session'.")]
    #[serde(default)]
    name: Option<String>,
    #[schemars(description = "Shell command for the session's first pane. Defaults to /bin/sh.")]
    #[serde(default)]
    shell: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearSessionIdInput {
    #[schemars(description = "16-char lowercase-hex tear session id.")]
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearPaneIdInput {
    #[schemars(description = "16-char lowercase-hex tear pane id.")]
    pane_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearSendKeysInput {
    #[schemars(description = "16-char lowercase-hex tear pane id.")]
    pane_id: String,
    #[schemars(description = "Keystrokes to send. \\n = Enter, \\x1b = ESC, \\x03 = Ctrl-C, etc.")]
    keys: String,
}

/// Pane-as-block: list blocks for a pane, optionally since N.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearPaneBlocksListInput {
    #[schemars(description = "16-char lowercase-hex tear pane id.")]
    pane_id: String,
    #[schemars(description = "Filter to blocks with index >= since (default 0 = all retained).")]
    #[serde(default)]
    since: Option<u64>,
    #[schemars(description = "Max blocks to return (default 50).")]
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearPaneBlockAtInput {
    #[schemars(description = "16-char lowercase-hex tear pane id.")]
    pane_id: String,
    #[schemars(description = "Per-pane block index (0-based, stable across eviction). Use tear_pane_blocks_status to get the latest index.")]
    index: u64,
}

/// Phase-A: input policy for `tear_set_input_policy`. Maps 1:1 to
/// `tear_types::InputPolicy`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TearSetInputPolicyInput {
    #[schemars(description = "16-char lowercase-hex tear pane id.")]
    pane_id: String,
    #[schemars(description = "Either \"free\" (default; accepts every send_keys) or \"locked\" (rejects send_keys with WireError::Rejected).")]
    policy: String,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClipboardPutInput {
    #[schemars(description = "Payload to store. UTF-8 text. Hashed under BLAKE3-128 and indexed by the resulting token.")]
    content: String,
    #[schemars(description = "OSC 52 selection kind — `c` (system, default), `p` (primary), `s` (secondary). Persisted with the entry so callers can distinguish \"give me the last primary\" later.")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PromptMarksListInput {
    #[schemars(description = "Maximum number of marks to return, most-recent-first. Omit for the full history.")]
    limit: Option<u32>,
    #[schemars(description = "If true, include non-Start kinds (CommandStart/Output/End) in the result. Default false — jump-capable Start marks only.")]
    include_all_kinds: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UserMarksListInput {
    #[schemars(description = "Maximum number of user marks to return, most-recent-first. Omit for the full history.")]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AttentionSetInput {
    #[schemars(description = "True to request user attention (bounce dock / flash titlebar); false to cancel any pending request.")]
    requested: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SnapshotGridInput {
    #[schemars(description = "Session identifier (returned by `spawn_term`).")]
    session_id: String,
    #[schemars(description = "Include the `cells[][]` array in the response. Default true. Set false when you only need cursor + dimensions (much smaller payload for repeated polling).")]
    include_cells: Option<bool>,
    #[schemars(description = "How to filter cells in the response. `non_default` (default) emits only cells with non-default content — shrinks an 80×24 grid response from ~50KB to ~2KB. `non_blank` emits cells whose char isn't space OR whose bg isn't black. `all` emits every cell (use sparingly; can exceed MCP token limits).")]
    cells_filter: Option<String>,
    #[schemars(description = "If true, include a human-readable `pretty` string field — one row per line, cursor cell marked `▓`, empty cells `·`. Useful for quick visual scans in chat without parsing the cell array.")]
    pretty: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResizeSessionInput {
    #[schemars(description = "Session identifier (returned by `spawn_term`).")]
    session_id: String,
    #[schemars(description = "New column count.")]
    cols: u16,
    #[schemars(description = "New row count.")]
    rows: u16,
}

// ── MCP Server ──────────────────────────────────────────────────────────────

/// Shared handle on the cross-session content-addressed clipboard
/// mirror the MCP server exposes. Wrapping in `Arc<Mutex<_>>` means
/// multiple tool handlers (and the future IPC bridge that feeds this
/// from the live Terminal) can hold references without cloning the
/// store itself.
type SharedClipboard = Arc<Mutex<ClipboardStore>>;

/// Mirror of [`SharedClipboard`] for the typed OSC 133 prompt-mark
/// history. Same ownership model — future IPC bridge routes Terminal
/// writes into this handle so MCP readers see live state.
type SharedPromptMarks = Arc<Mutex<PromptHistory>>;

/// Mirror of [`SharedPromptMarks`] for OSC 1337 SetMark history.
/// User-emitted marks (script-echoed) live alongside shell-emitted
/// prompt marks with identical ownership semantics.
type SharedUserMarks = Arc<Mutex<UserMarkHistory>>;

/// OSC 1337 RequestAttention flag — a simple bool wrapped for
/// cross-thread sharing. When true, the platform layer bounces the
/// dock / flashes the titlebar until focus returns.
type SharedAttention = Arc<Mutex<bool>>;

/// Registry of headless terminal sessions the MCP server owns.
/// `spawn_term`, `list_sessions`, `send_keys`, `get_output`,
/// `snapshot_grid` all read/write through this handle — the
/// constructive-substrate primitive that turns "drive mado over MCP"
/// from a stub into a real surface.
type SharedSessions = Arc<SessionRegistry>;

/// Bundle of every shared-state handle the MCP server reads / writes.
///
/// Collecting the handles into one struct means adding a new shared
/// surface is a single field edit instead of threading a new
/// positional argument through every caller + test fixture. Struct
/// update syntax (`..SharedState::default()`) gives tests the
/// "override one handle, accept defaults for everything else"
/// shape naturally.
#[derive(Debug, Clone)]
struct SharedState {
    clipboard: SharedClipboard,
    prompt_marks: SharedPromptMarks,
    user_marks: SharedUserMarks,
    attention: SharedAttention,
    sessions: SharedSessions,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            clipboard: Arc::new(Mutex::new(ClipboardStore::new(128))),
            prompt_marks: Arc::new(Mutex::new(PromptHistory::default())),
            user_marks: Arc::new(Mutex::new(UserMarkHistory::default())),
            attention: Arc::new(Mutex::new(false)),
            sessions: Arc::new(SessionRegistry::default()),
        }
    }
}

#[derive(Debug, Clone)]
struct MadoMcp {
    tool_router: ToolRouter<Self>,
    /// Every shared handle the MCP tools read / write. See
    /// [`SharedState`] for field-level docs.
    state: SharedState,
}

#[tool_router]
impl MadoMcp {
    fn new() -> Self {
        Self::with_state(SharedState::default())
    }

    /// Construct with an externally-owned shared-state bundle —
    /// the future IPC bridge + the test fixtures both route through
    /// here. `new()` is the prod entrypoint and calls this with
    /// `SharedState::default()`.
    fn with_state(state: SharedState) -> Self {
        Self {
            tool_router: Self::tool_router(),
            state,
        }
    }

    /// Clipboard-only test fixture — for the scenarios that exercise
    /// only the clipboard bridge. Every other handle defaults to
    /// empty state so the server behaves as if no OSC 133 /
    /// OSC 1337 activity has occurred yet.
    #[cfg(test)]
    fn with_clipboard(clipboard: SharedClipboard) -> Self {
        Self::with_state(SharedState {
            clipboard,
            ..SharedState::default()
        })
    }

    // ── Standard tools ──────────────────────────────────────────────────────

    #[tool(description = "Get mado application status and health information. When a GUI mado is running, this forwards via kanshou to that process's live AppState; when no GUI is reachable, falls back to the MCP server's process-local count.")]
    async fn status(&self) -> String {
        match kanshou::mcp::forward_status(
            "mado",
            &kanshou::Query::field(["sessions"]),
            || {
                let count = self.state.sessions.list().len();
                Ok(serde_json::json!({ "count": count, "sessions": [] }))
            },
        )
        .await
        {
            kanshou::mcp::ForwardOutcome::Live { pid, value } => {
                serde_json::json!({
                    "status": "running",
                    "app": "mado",
                    "live_gui_pid": pid,
                    "sessions": value.get("count").cloned().unwrap_or_else(|| serde_json::json!(0)),
                })
                .to_string()
            }
            kanshou::mcp::ForwardOutcome::Fallback { value } => serde_json::json!({
                "status": "running",
                "app": "mado",
                "live_gui_pid": serde_json::Value::Null,
                "sessions": value.get("count").cloned().unwrap_or_else(|| serde_json::json!(0)),
                "note": "no live GUI mado discoverable via kanshou; reporting MCP-server-local state",
            })
            .to_string(),
            kanshou::mcp::ForwardOutcome::LiveError { pid, error } => serde_json::json!({
                "status": "running",
                "app": "mado",
                "live_gui_pid": pid,
                "kanshou_error": error.to_string(),
            })
            .to_string(),
        }
    }

    #[tool(description = "Get the most recent render-loop frame timing snapshot from the LIVE GUI mado. When a GUI is running, this forwards via kanshou to the GUI's render atomics (last_frame_us, last_frame_rects, last_frame_text, last_frame_shape_cache, total_frames, total_frames_skipped). When no GUI is reachable, returns zeros from the MCP server's process-local atomics (which are never updated by the MCP-only process).")]
    async fn frame_perf(&self) -> String {
        let value = kanshou::mcp::forward(
            "mado",
            &kanshou::Query::field(["frame_perf"]),
            || {
                use std::sync::atomic::Ordering;
                Ok(serde_json::json!({
                    "last_frame_us": crate::render::LAST_FRAME_US.load(Ordering::Relaxed),
                    "last_frame_rects": crate::render::LAST_FRAME_RECTS.load(Ordering::Relaxed),
                    "last_frame_text": crate::render::LAST_FRAME_TEXT.load(Ordering::Relaxed),
                    "last_frame_shape_cache": crate::render::LAST_FRAME_SHAPE_CACHE.load(Ordering::Relaxed),
                    "total_frames": crate::render::TOTAL_FRAMES.load(Ordering::Relaxed),
                    "total_frames_skipped": crate::render::TOTAL_FRAMES_SKIPPED.load(Ordering::Relaxed),
                }))
            },
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
        let merged = match value {
            serde_json::Value::Object(mut m) => {
                m.insert("ok".into(), serde_json::Value::Bool(true));
                serde_json::Value::Object(m)
            }
            other => other,
        };
        merged.to_string()
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

    #[tool(description = "Get a mado configuration value. Forwards through kanshou to the LIVE GUI mado's MadoConfig snapshot. Pass a key for nested access (e.g. 'shell.command', 'tear.runtime') or omit for the full config.")]
    async fn config_get(&self, Parameters(input): Parameters<ConfigGetInput>) -> String {
        let path: Vec<String> = match &input.key {
            Some(k) => std::iter::once("config".to_string())
                .chain(k.split('.').map(str::to_string))
                .collect(),
            None => vec!["config".into()],
        };
        let value = kanshou::mcp::forward(
            "mado",
            &kanshou::Query { path, args: vec![] },
            || {
                Ok(serde_json::json!({
                    "note": "no live GUI mado discoverable via kanshou",
                    "config_path": "~/.config/mado/mado.yaml",
                }))
            },
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
        value.to_string()
    }

    #[tool(description = "Set a mado configuration value at runtime. Changes take effect immediately via hot-reload.")]
    async fn config_set(&self, Parameters(input): Parameters<ConfigSetInput>) -> String {
        stub_response(
            "config_set",
            serde_json::json!({ "key": input.key, "value": input.value }),
        )
    }

    // ── Terminal-specific tools ─────────────────────────────────────────────
    //
    // Backed by `SessionRegistry`. Every tool here operates on a live
    // headless session — PTY + terminal-state-machine + reader pump.
    // The `--mcp` mado process owns the registry; `spawn_term` opens
    // sessions inside it, every subsequent tool refers to them by id.

    #[tool(description = "List every live headless terminal session. When a GUI mado is running, forwards through kanshou to the live SessionRegistry; when not, falls back to the MCP-server-local registry (sessions spawn_term created here).")]
    async fn list_sessions(&self) -> String {
        let live = kanshou::mcp::forward(
            "mado",
            &kanshou::Query::field(["sessions"]),
            || {
                let summaries = self.state.sessions.list();
                Ok(serde_json::json!({ "sessions": summaries, "count": summaries.len() }))
            },
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
        live.to_string()
    }

    #[tool(description = "Send keystrokes / raw bytes to a terminal session. The string is sent as UTF-8 bytes to the PTY master — the child's read() advances. Use `\\n` for Enter, `\\x1b` for ESC, `\\x03` for Ctrl-C, etc.")]
    async fn send_keys(&self, Parameters(input): Parameters<SendKeysInput>) -> String {
        let Some(session) = self.state.sessions.get(&input.session_id) else {
            return serde_json::json!({
                "ok": false,
                "error": "no-such-session",
                "session_id": input.session_id,
            })
            .to_string();
        };
        let bytes = decode_send_keys(&input.keys);
        match session.send_input(&bytes).await {
            Ok(()) => serde_json::json!({
                "ok": true,
                "session_id": input.session_id,
                "bytes_written": bytes.len(),
            })
            .to_string(),
            Err(e) => serde_json::json!({
                "ok": false,
                "session_id": input.session_id,
                "error": e.to_string(),
            })
            .to_string(),
        }
    }

    #[tool(description = "Get the visible terminal output as plain text (trailing whitespace stripped per row). Lighter than `snapshot_grid` when the agent only needs to grep / match output. `lines` clips to the most recent N rows; omit for the full grid.")]
    async fn get_output(&self, Parameters(input): Parameters<GetOutputInput>) -> String {
        let Some(session) = self.state.sessions.get(&input.session_id) else {
            return serde_json::json!({
                "ok": false,
                "error": "no-such-session",
                "session_id": input.session_id,
            })
            .to_string();
        };
        let snap = session.snapshot_grid();
        let text = snap.to_text();
        let output = if let Some(n) = input.lines {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(n as usize);
            lines[start..].join("\n")
        } else {
            text
        };
        serde_json::json!({
            "ok": true,
            "session_id": input.session_id,
            "cols": snap.cols,
            "rows": snap.rows,
            "output": output,
        })
        .to_string()
    }

    #[tool(description = "Take a full structured snapshot of a session's terminal grid — every cell (char + fg + bg + attrs + width) plus cursor row/col/visibility. The load-bearing introspection tool: any visual rendering bug can be triaged by diffing two snapshots or comparing against the on-screen pixels. Set `pretty: true` to also include a human-readable text grid where cells with empty bg become `·` and the cursor cell becomes `▓`. Default `cells_filter: runs` (RLE same-attrs contiguous cells into one entry — ~10–50× smaller payload than `non_default` on typical interactive grids). Alternatives: `non_default` (per-cell sparse), `non_blank` (looser sparse), `all` (full row-major grid — caller owns the size implications).")]
    async fn snapshot_grid(&self, Parameters(input): Parameters<SnapshotGridInput>) -> String {
        let Some(session) = self.state.sessions.get(&input.session_id) else {
            return serde_json::json!({
                "ok": false,
                "error": "no-such-session",
                "session_id": input.session_id,
            })
            .to_string();
        };
        let snap = session.snapshot_grid();
        let mut payload = serde_json::json!({
            "ok": true,
            "session_id": input.session_id,
            "cols": snap.cols,
            "rows": snap.rows,
            "cursor_row": snap.cursor_row,
            "cursor_col": snap.cursor_col,
            "cursor_visible": snap.cursor_visible,
        });
        if input.include_cells.unwrap_or(true) {
            // P10: default switched to `runs` — collapses every same-
            // attrs contiguous span into one RLE entry. Wire size on
            // a typical 120×40 interactive grid drops from ~300 KB
            // (non_default per-cell sparse) to ~5–25 KB. Agents that
            // need per-cell granularity can still pass
            // `cells_filter: "non_default"` explicitly.
            let filter = input.cells_filter.as_deref().unwrap_or("runs");
            let cells = filtered_cells(&snap, filter);
            payload["cells_filter"] = serde_json::Value::String(filter.to_string());
            payload["cells"] = cells;
        }
        if input.pretty.unwrap_or(false) {
            payload["pretty"] = serde_json::Value::String(snap.to_pretty());
        }
        payload.to_string()
    }

    #[tool(description = "Resize a session's PTY + terminal grid together. Both the kernel winsize (so the child gets SIGWINCH) and the in-memory grid are updated. Use to reproduce layout-sensitive bugs at specific widths.")]
    async fn resize_session(&self, Parameters(input): Parameters<ResizeSessionInput>) -> String {
        let Some(session) = self.state.sessions.get(&input.session_id) else {
            return serde_json::json!({
                "ok": false,
                "error": "no-such-session",
                "session_id": input.session_id,
            })
            .to_string();
        };
        match session.resize(input.cols, input.rows).await {
            Ok(()) => serde_json::json!({
                "ok": true,
                "session_id": input.session_id,
                "cols": input.cols,
                "rows": input.rows,
            })
            .to_string(),
            Err(e) => serde_json::json!({
                "ok": false,
                "session_id": input.session_id,
                "error": e.to_string(),
            })
            .to_string(),
        }
    }

    #[tool(description = "Close a headless terminal session. The PTY is dropped (killing the child if still alive) and the reader task is aborted. Returns `{ok, closed}` — `closed: true` when the id matched a live session, `false` when it was already gone.")]
    async fn close_session(&self, Parameters(input): Parameters<SessionIdInput>) -> String {
        match self.state.sessions.close(&input.session_id) {
            Ok(closed) => serde_json::json!({
                "ok": true,
                "session_id": input.session_id,
                "closed": closed,
            })
            .to_string(),
            Err(e) => serde_json::json!({
                "ok": false,
                "session_id": input.session_id,
                "error": e.to_string(),
            })
            .to_string(),
        }
    }

    // split_pane / new_tab removed at Phase 4 — mado is now a
    // single-pane terminal. Multi-pane / tab operations live in
    // tear (theory/MADO-TEAR-M5.md). Clients that need to drive
    // panes call tear-client (or `tear` from the shell) directly.

    // ── Typed session spawning — the escriba integration surface ─────────────
    //
    // `spawn_term` is the MCP tool escriba (and any other typed client)
    // calls when it wants mado to open a new session from a declarative
    // spec. The JSON schema advertised to clients is `TermSpec` — every
    // field has a default so the minimal payload is `{}`. This mirrors
    // the escriba-lisp `defterm` form authored in the rc.
    #[tool(description = "Spawn a headless terminal session from a typed TermSpec. Fields: shell (default $SHELL → /bin/sh), args, cwd (~/ expands), env, title (derives from shell/cwd if empty), placement (advisory — headless ignores; future window mode honors), attach (existing session id), effects (shader names — visual only), cols/rows (default 80/24). Returns `{ok, session_id, title, shell, cols, rows}`. The new session is live: the reader pump is already pulling bytes from the PTY into the terminal state machine.")]
    async fn spawn_term(&self, Parameters(spec): Parameters<TermSpec>) -> String {
        match self.state.sessions.spawn(&spec).await {
            Ok(id) => {
                let (cols, rows) = spec.resolved_dimensions();
                serde_json::json!({
                    "ok": true,
                    "session_id": id,
                    "title": spec.display_title(),
                    "shell": if spec.shell.is_empty() {
                        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
                    } else {
                        spec.shell.clone()
                    },
                    "cols": cols,
                    "rows": rows,
                    "placement": format!("{:?}", spec.resolved_placement()),
                })
                .to_string()
            }
            Err(e) => serde_json::json!({
                "ok": false,
                "error": e.to_string(),
            })
            .to_string(),
        }
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
        let guard = self.state.clipboard.lock().expect("clipboard lock poisoned");
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

    #[tool(description = "Publish a payload into the session's content-addressed clipboard store. Returns `{ok, hash, bytes, kind, duplicate}` — `duplicate: true` when the content was already indexed (the hash is stable across calls, so this is idempotent). Used by escriba workflows that yank text in the editor and want the mado side to resolve the same payload by hash later.")]
    async fn clipboard_put(&self, Parameters(input): Parameters<ClipboardPutInput>) -> String {
        let kind = input
            .kind
            .as_deref()
            .map(|s| crate::clipboard_store::ClipboardKind::from_osc52_byte(s.as_bytes()))
            .unwrap_or(crate::clipboard_store::ClipboardKind::System);
        let bytes = input.content.len();
        let mut guard = self.state.clipboard.lock().expect("clipboard lock poisoned");
        let pre_hash = ClipboardHash::of(&input.content);
        let duplicate = guard.contains(pre_hash);
        let hash = guard.store(input.content, kind);
        serde_json::json!({
            "ok": true,
            "hash": hash.to_hex(),
            "bytes": bytes,
            "kind": kind.label(),
            "duplicate": duplicate,
        })
        .to_string()
    }

    #[tool(description = "Wipe every entry in the session's clipboard store. Returns `{ok, cleared}` with the count of entries that were removed. Used when a workflow touches sensitive content and wants the session's copy history scrubbed.")]
    async fn clipboard_clear(&self) -> String {
        let mut guard = self.state.clipboard.lock().expect("clipboard lock poisoned");
        let cleared = guard.clear();
        serde_json::json!({
            "ok": true,
            "cleared": cleared,
        })
        .to_string()
    }

    // ── OSC 133 prompt-mark history — escriba "past command" picker ────────
    //
    // Mado captures every OSC 133 marker (prompt start / command start /
    // output / end) in a typed bounded history. These tools surface
    // the history over MCP so escriba's picker can render a "jump to
    // past command" list spanning every terminal pane the agent has
    // access to. No editor / terminal pair exposes this: ghostty +
    // kitty + iterm2 keep prompt-jump internal.

    #[tool(description = "List OSC 133 prompt marks the session has seen, most-recent-first. Returns `{count, total, marks: [{grid_row, kind}]}`. Default filters to `Start` kind only — the jump-capable marker. Set `include_all_kinds: true` to also surface CommandStart / CommandOutput / CommandEnd marks for finer-grained replays.")]
    async fn prompt_marks_list(
        &self,
        Parameters(input): Parameters<PromptMarksListInput>,
    ) -> String {
        use crate::prompt_mark::PromptKind;
        let include_all = input.include_all_kinds.unwrap_or(false);
        let limit = input.limit.map(|n| n as usize);
        let guard = self.state.prompt_marks.lock().expect("prompt_marks lock poisoned");
        // Most-recent-first: walk the underlying VecDeque in reverse.
        let filtered: Vec<serde_json::Value> = guard
            .iter()
            .rev()
            .filter(|m| include_all || m.kind == PromptKind::Start)
            .map(|m| {
                serde_json::json!({
                    "grid_row": m.grid_row,
                    "kind": format!("{:?}", m.kind),
                })
            })
            .collect();
        let marks: Vec<_> = match limit {
            Some(n) => filtered.into_iter().take(n).collect(),
            None => filtered,
        };
        serde_json::json!({
            "count": marks.len(),
            "total": guard.len(),
            "marks": marks,
        })
        .to_string()
    }

    #[tool(description = "Clear the OSC 133 prompt-mark history. Returns `{ok, cleared}`. Used when a session needs a fresh jump surface (e.g. after `reset`) or when sensitive shell output should no longer be jumpable-to.")]
    async fn prompt_marks_clear(&self) -> String {
        let mut guard = self.state.prompt_marks.lock().expect("prompt_marks lock poisoned");
        let cleared = guard.len();
        guard.clear();
        serde_json::json!({
            "ok": true,
            "cleared": cleared,
        })
        .to_string()
    }

    // ── OSC 1337 user-mark + attention surface ─────────────────────────────
    //
    // Complements the OSC 133 prompt-mark tools: user marks are
    // script-echoed (`echo -e "\e]1337;SetMark\e\\"`) whereas
    // prompt marks are shell-emitted. Both live in the terminal;
    // both surface over MCP so escriba's picker exposes each as a
    // separate jump surface without cross-contamination.

    #[tool(description = "List OSC 1337 SetMark user marks the session has seen, most-recent-first. Returns `{count, total, marks: [{grid_row}]}`. Unlike prompt_marks_list, no kind filter — user marks are a flat history of explicit script-echoed markers.")]
    async fn user_marks_list(
        &self,
        Parameters(input): Parameters<UserMarksListInput>,
    ) -> String {
        let limit = input.limit.map(|n| n as usize);
        let guard = self.state.user_marks.lock().expect("user_marks lock poisoned");
        let iter = guard.iter().rev().map(|m| {
            serde_json::json!({
                "grid_row": m.grid_row,
            })
        });
        let marks: Vec<_> = match limit {
            Some(n) => iter.take(n).collect(),
            None => iter.collect(),
        };
        serde_json::json!({
            "count": marks.len(),
            "total": guard.len(),
            "marks": marks,
        })
        .to_string()
    }

    #[tool(description = "Clear the OSC 1337 user-mark history. Returns `{ok, cleared}`. Paired with prompt_marks_clear for a full mark-history reset.")]
    async fn user_marks_clear(&self) -> String {
        let mut guard = self.state.user_marks.lock().expect("user_marks lock poisoned");
        let cleared = guard.len();
        guard.clear();
        serde_json::json!({
            "ok": true,
            "cleared": cleared,
        })
        .to_string()
    }

    #[tool(description = "Read the current OSC 1337 RequestAttention flag. Returns `{attention_requested}`. Used by escriba workflows that want to know whether a terminal is currently asking for user attention (e.g., long-running test signals completion).")]
    async fn attention_get(&self) -> String {
        let guard = self.state.attention.lock().expect("attention lock poisoned");
        serde_json::json!({
            "attention_requested": *guard,
        })
        .to_string()
    }

    #[tool(description = "Set the OSC 1337 RequestAttention flag. Returns `{ok, attention_requested}`. Lets escriba workflows drive the dock-bounce / titlebar-flash signal without emitting an ANSI sequence through a shell — e.g., a `defworkflow` can flash the dock when tests pass or a deployment completes.")]
    async fn attention_set(
        &self,
        Parameters(input): Parameters<AttentionSetInput>,
    ) -> String {
        let mut guard = self.state.attention.lock().expect("attention lock poisoned");
        *guard = input.requested;
        serde_json::json!({
            "ok": true,
            "attention_requested": *guard,
        })
        .to_string()
    }

    #[tool(description = "List clipboard payloads the session has seen, most-recent-first. Returns `{count, entries: [{hash, preview, bytes, kind, set_at}]}`. Set `include_content: true` to also pull the full payload (for scripted pipelines); default is preview-only to keep the response compact.")]
    async fn clipboard_list(&self, Parameters(input): Parameters<ClipboardListInput>) -> String {
        let include_content = input.include_content.unwrap_or(false);
        let limit = input.limit.map(|n| n as usize);
        let guard = self.state.clipboard.lock().expect("clipboard lock poisoned");
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

    // ── tear-multiplexer bridge (Phase 5 — full daemon surface) ────────────
    //
    // Every tool below opens a transient tear-client connection
    // via `tear_discovery::discover()` for the call. Cheap (UDS
    // local), reflects the user's `[tear]` config (mode / socket /
    // auto_spawn) on every invocation, and means tests can drive
    // mado MCP against an in-process daemon without ever wiring a
    // singleton.

    #[tool(description = "Probe tear-daemon reachability. Returns {reachable: bool, socket_path, daemon_pid?, sessions?}. Honours [tear] config from MADO_CONFIG / ~/.config/mado/mado.yaml — auto-spawns if tear.auto_spawn=true and no daemon answers.")]
    async fn tear_status(&self) -> String {
        let cfg = crate::config::load(&None).unwrap_or_default().tear;
        match crate::tear_discovery::discover(&cfg) {
            crate::tear_discovery::DiscoveryOutcome::Attached(client, path) => {
                use tear_types::MultiplexerControl;
                let sessions = client.list_sessions().map(|v| v.len()).unwrap_or(0);
                serde_json::json!({
                    "reachable": true,
                    "socket_path": path.to_string_lossy(),
                    "sessions": sessions,
                    "mode": format!("{:?}", cfg.mode),
                })
                .to_string()
            }
            crate::tear_discovery::DiscoveryOutcome::Fallback => serde_json::json!({
                "reachable": false,
                "socket_path": crate::tear_discovery::resolve_socket_path(&cfg).to_string_lossy(),
                "fallback": true,
                "mode": format!("{:?}", cfg.mode),
            })
            .to_string(),
            crate::tear_discovery::DiscoveryOutcome::Required(msg) => serde_json::json!({
                "reachable": false,
                "required": true,
                "error": msg,
            })
            .to_string(),
        }
    }

    #[tool(description = "Fetch the tear-daemon's current TearConfig as YAML. Returns {ok, yaml} on success or {ok: false, error}.")]
    async fn tear_get_config(&self) -> String {
        with_tear_client(|client| match client.get_config_yaml() {
            Ok(yaml) => ok_json(serde_json::json!({ "yaml": yaml })),
            Err(e) => err_json(e),
        })
    }

    #[tool(description = "Push a TearConfig YAML payload to the daemon — replaces the live config in-place via the same path tear.impose uses at attach. Returns {ok} or {ok: false, error}. Daemon-on-disk file is NOT touched; the next ReloadConfig reverts.")]
    async fn tear_set_config_yaml(&self, Parameters(input): Parameters<TearSetConfigYamlInput>) -> String {
        with_tear_client(|client| match client.set_config_yaml(input.yaml) {
            Ok(()) => ok_json(serde_json::Value::Null),
            Err(e) => err_json(e),
        })
    }

    #[tool(description = "Force the daemon to re-read its config file from disk. Reverts any prior SetConfig overrides. Returns {ok}.")]
    async fn tear_reload_config(&self) -> String {
        with_tear_client(|client| match client.reload_config() {
            Ok(()) => ok_json(serde_json::Value::Null),
            Err(e) => err_json(e),
        })
    }

    #[tool(description = "List every tear session. Returns {ok, sessions: [{id, name, windows, panes, state}]}.")]
    async fn tear_list_sessions(&self) -> String {
        use tear_types::MultiplexerControl;
        with_tear_client(|client| match client.list_sessions() {
            Ok(sessions) => {
                let v: Vec<_> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id.to_string(),
                            "name": s.name,
                            "windows": s.windows.len(),
                            "panes": s.panes.len(),
                            "state": format!("{:?}", s.state),
                        })
                    })
                    .collect();
                ok_json(serde_json::json!({ "sessions": v }))
            }
            Err(e) => err_json(e),
        })
    }

    #[tool(description = "Create a new tear session running `shell` (default /bin/sh). Session is tagged with SessionSource::Agent so `tear list --source agent` can audit MCP-created sessions. Returns {ok, session_id, first_pane_id}.")]
    async fn tear_new_session(&self, Parameters(input): Parameters<TearNewSessionInput>) -> String {
        use tear_types::MultiplexerControl;
        with_tear_client(|client| {
            let shell = input.shell.unwrap_or_else(|| "/bin/sh".to_string());
            let name = input.name.unwrap_or_else(|| "mcp-session".to_string());
            // Every mado-MCP-created session is provenance-tagged
            // as `Agent` so operators can `tear list --source agent`
            // to triage what an agent has spawned behind their back.
            match client.new_session_with_source(
                &name,
                &shell,
                tear_types::SessionSource::Agent,
            ) {
                Ok(sid) => {
                    let first_pane = client
                        .get_session(sid)
                        .ok()
                        .and_then(|s| s.panes.keys().next().copied());
                    ok_json(serde_json::json!({
                        "session_id": sid.to_string(),
                        "first_pane_id": first_pane.map(|p| p.to_string()),
                    }))
                }
                Err(e) => err_json(e),
            }
        })
    }

    #[tool(description = "Kill a tear session by id. Returns {ok}.")]
    async fn tear_kill_session(&self, Parameters(input): Parameters<TearSessionIdInput>) -> String {
        use tear_types::MultiplexerControl;
        with_tear_id::<tear_types::SessionId, _>(
            "session_id",
            &input.session_id,
            |client, id| match client.kill_session(id) {
                Ok(()) => ok_json(serde_json::Value::Null),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Snapshot a tear pane's rendered cell grid. Returns {ok, cols, rows, cursor_row, cursor_col, alt_screen_active, text_rows}.")]
    async fn tear_pane_snapshot(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        use tear_types::MultiplexerControl;
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.pane_snapshot(id) {
                Ok(snap) => ok_json(serde_json::json!({
                    "cols": snap.cols,
                    "rows": snap.rows,
                    "cursor_row": snap.cursor_row,
                    "cursor_col": snap.cursor_col,
                    "alt_screen_active": snap.alt_screen_active,
                    "text_rows": snap.to_text_rows(),
                })),
                Err(e) => err_json(e),
            },
        )
    }

    // ── #2 input policy ───────────────────────────────────────

    #[tool(description = "Set a tear pane's typed InputPolicy. `policy = free` (default, accepts send_keys) or `locked` (rejects send_keys; useful for observer / demo / agent-only panes). Returns {ok} or {ok: false, error}.")]
    async fn tear_set_input_policy(&self, Parameters(input): Parameters<TearSetInputPolicyInput>) -> String {
        use tear_types::MultiplexerControl;
        let policy = match input.policy.as_str() {
            "free" => tear_types::InputPolicy::Free,
            "locked" => tear_types::InputPolicy::Locked,
            other => return err_json(format!("invalid policy `{other}` — accepted: free | locked")),
        };
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            move |client, id| match client.set_input_policy(id, policy) {
                Ok(()) => ok_json(serde_json::json!({ "policy": match policy {
                    tear_types::InputPolicy::Free => "free",
                    tear_types::InputPolicy::Locked => "locked",
                    tear_types::InputPolicy::Leader { .. } => "leader",
                }})),
                Err(e) => err_json(e),
            },
        )
    }

    // ── #3 migration ergonomic ────────────────────────────────

    #[tool(description = "Subscriber count for a tear pane. Tells you whether the pane is already attached elsewhere before you open a second renderer. Returns {ok, subscribers}.")]
    async fn tear_pane_subscriber_count(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        use tear_types::MultiplexerControl;
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.pane_subscriber_count(id) {
                Ok(n) => ok_json(serde_json::json!({ "subscribers": n })),
                Err(e) => err_json(e),
            },
        )
    }

    // ── #4 recording ──────────────────────────────────────────

    #[tool(description = "Start daemon-native recording on a tear pane. Captures every PTY byte with a relative timestamp; export via `tear_pane_record_export` as asciinema v2 .cast. Returns {ok}.")]
    async fn tear_pane_record_start(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.start_pane_recording(id) {
                Ok(()) => ok_json(serde_json::Value::Null),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Stop recording on a tear pane. Captured buffer is retained for export. Returns {ok}.")]
    async fn tear_pane_record_stop(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.stop_pane_recording(id) {
                Ok(()) => ok_json(serde_json::Value::Null),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Export the captured asciinema v2 .cast (JSON-lines string) of a recorded pane. Returns {ok, cast}.")]
    async fn tear_pane_record_export(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.export_pane_recording(id) {
                Ok(cast) => ok_json(serde_json::json!({ "cast": cast })),
                Err(e) => err_json(e),
            },
        )
    }

    // ── Pane-as-block (OSC 133 prompt-mark capture) ──────────

    #[tool(description = "List captured prompt+command+output blocks for a tear pane. Blocks are extracted from OSC 133 prompt marks (powerlevel10k / starship / VS Code shell-integration emit these). Each block has {index, prompt, command, output, exit_code, started_at_unix_ms, ended_at_unix_ms}. Returns {ok, blocks: [...]}.")]
    async fn tear_pane_blocks_list(&self, Parameters(input): Parameters<TearPaneBlocksListInput>) -> String {
        let since = input.since.unwrap_or(0);
        let limit = input.limit.unwrap_or(50);
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            move |client, id| match client.pane_blocks_list(id, since, limit) {
                Ok(blocks) => ok_json(serde_json::json!({ "blocks": blocks })),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Fetch one block by per-pane index. Use tear_pane_blocks_status first to get the current total. Returns {ok, block: {...}} or {ok: false, error} if the block has been evicted or never existed.")]
    async fn tear_pane_block_at(&self, Parameters(input): Parameters<TearPaneBlockAtInput>) -> String {
        let index = input.index;
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            move |client, id| match client.pane_block_at(id, index) {
                Ok(block) => ok_json(serde_json::json!({ "block": block })),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Pane block summary — {total_completed, in_progress}. `tear top` polls this each refresh.")]
    async fn tear_pane_blocks_status(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.pane_blocks_status(id) {
                Ok((total, in_progress)) => ok_json(serde_json::json!({
                    "total_completed": total,
                    "in_progress": in_progress,
                })),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Recording status for a tear pane. Returns {ok, recording: bool, events: number}.")]
    async fn tear_pane_record_status(&self, Parameters(input): Parameters<TearPaneIdInput>) -> String {
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            |client, id| match client.pane_recording_status(id) {
                Ok((enabled, events)) => ok_json(serde_json::json!({
                    "recording": enabled,
                    "events": events,
                })),
                Err(e) => err_json(e),
            },
        )
    }

    #[tool(description = "Send keystrokes to a tear pane's PTY. `keys` is decoded via the same escape grammar as mado's `send_keys` tool (\\n = Enter, \\x03 = Ctrl-C, etc.). Returns {ok}.")]
    async fn tear_send_keys(&self, Parameters(input): Parameters<TearSendKeysInput>) -> String {
        use tear_types::MultiplexerControl;
        let bytes = decode_send_keys(&input.keys);
        with_tear_id::<tear_types::PaneId, _>(
            "pane_id",
            &input.pane_id,
            move |client, id| match client.send_keys(id, &bytes) {
                Ok(()) => ok_json(serde_json::Value::Null),
                Err(e) => err_json(e),
            },
        )
    }

}

/// Per-tool client factory — fresh discovery + connection per
/// MCP call. Lets each tool reflect the live [tear] config without
/// caching staleness, and means `tear_status` reachable=false
/// errors are honest about "right now" reachability.
fn with_tear_client<F>(f: F) -> String
where
    F: FnOnce(tear_client::Client) -> String,
{
    let cfg = crate::config::load(&None).unwrap_or_default().tear;
    match crate::tear_discovery::discover(&cfg) {
        crate::tear_discovery::DiscoveryOutcome::Attached(client, _) => f(client),
        crate::tear_discovery::DiscoveryOutcome::Fallback => serde_json::json!({
            "ok": false,
            "error": "tear-daemon not reachable; set tear.mode = \"always\" or tear.auto_spawn = true to require it",
            "fallback": true,
        })
        .to_string(),
        crate::tear_discovery::DiscoveryOutcome::Required(msg) => serde_json::json!({
            "ok": false,
            "error": msg,
            "required": true,
        })
        .to_string(),
    }
}

/// Compose `with_tear_client` + a typed-id parse step. Every tear_*
/// tool that takes a `SessionId` / `PaneId` runs the same shape:
/// (1) discover + connect, (2) parse the hex id from the input,
/// (3) call a Client method, (4) shape the response. This helper
/// folds (1)+(2) into one call so the per-tool body is just (3)+(4).
fn with_tear_id<T, F>(label: &str, raw: &str, f: F) -> String
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
    F: FnOnce(tear_client::Client, T) -> String,
{
    with_tear_client(|client| match raw.parse::<T>() {
        Ok(id) => f(client, id),
        Err(e) => err_json(format!("invalid {label}: {e}")),
    })
}

/// `{"ok": true, ...extra}` — terminal-state response for tools that
/// signal success with structured data. `extra` MUST be a JSON
/// object; non-object values are silently dropped so callers can
/// pass `serde_json::Value::Null` to mean "just ok".
fn ok_json(extra: serde_json::Value) -> String {
    let mut obj = serde_json::Map::with_capacity(4);
    obj.insert("ok".into(), serde_json::Value::Bool(true));
    if let serde_json::Value::Object(fields) = extra {
        obj.extend(fields);
    }
    serde_json::Value::Object(obj).to_string()
}

/// `{"ok": false, "error": <e>}` — terminal-state response for tool
/// failures. Uniform across every tear_* tool so MCP clients can
/// branch on `ok` exactly once.
fn err_json<E: std::fmt::Display>(error: E) -> String {
    serde_json::json!({ "ok": false, "error": error.to_string() }).to_string()
}

/// Render the "stubbed — requires IPC" response shape used by every
/// tool that can't be satisfied without a running mado instance. The
/// shape is: `{ok: false, tool: <name>, note: "<name> requires IPC
/// to a running mado instance.", …extra}`. Extracted so the 8
/// stubbed tools can't drift into slightly different phrasings — the
/// wire contract is a single predicate instead of 8 hand-written
/// JSON objects.
///
/// `extra` MUST be a JSON object; any other shape is flattened and
/// discarded (serde_json's `Map::extend` signature prevents mixing
/// keyed + unkeyed values).
fn stub_response(tool: &str, extra: serde_json::Value) -> String {
    let mut obj = serde_json::Map::with_capacity(8);
    obj.insert("ok".into(), serde_json::Value::Bool(false));
    obj.insert("tool".into(), serde_json::Value::String(tool.to_string()));
    obj.insert(
        "note".into(),
        serde_json::Value::String(format!(
            "{tool} requires IPC to a running mado instance."
        )),
    );
    if let serde_json::Value::Object(fields) = extra {
        obj.extend(fields);
    }
    serde_json::Value::Object(obj).to_string()
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

/// Per-cell snapshot entry used by the `non_default` / `non_blank`
/// MCP serialisation modes. Direct serde struct — much faster than
/// the previous `serde_json::json!` macro that built a BTreeMap per
/// cell. `ch` is a single-codepoint String because JSON has no char
/// type; everything else round-trips as small primitives or `[u8;3]`
/// arrays so the wire format is obvious without a glossary.
#[derive(serde::Serialize)]
struct SparseCellEntry {
    row: usize,
    col: usize,
    ch: String,
    fg: [u8; 3],
    bg: [u8; 3],
    attrs: u8,
    width: u8,
}

/// Run-length-encoded contiguous cells with identical styling, used
/// by the `runs` MCP serialisation mode (the default). Collapses
/// e.g. a 25-char same-colour prompt line from ~25 SparseCellEntry
/// objects (~1.2 KB) into ONE CellRun (~80 B). Typical interactive
/// grid: 4800 cells → ~30–80 runs, ~10–50× smaller payload than
/// `non_default`. Wide cells (width != 1) are emitted as solo runs
/// to keep advance semantics unambiguous on the client.
#[derive(serde::Serialize)]
struct CellRun {
    row: usize,
    col: usize,
    text: String,
    fg: [u8; 3],
    bg: [u8; 3],
    attrs: u8,
}

/// Filter cells in a [`GridSnapshot`] for MCP serialisation.
///
/// **Modes** (the `cells_filter` MCP tool parameter):
///   - `runs` — DEFAULT. RLE same-attrs contiguous cells into one
///     [`CellRun`] each. 10–50× smaller payload than `non_default`
///     on typical interactive grids. The structure agents need to
///     "what's at this position in this colour" is preserved; only
///     redundant per-cell repetition is dropped.
///   - `non_default` — one [`SparseCellEntry`] per non-default cell.
///     Drops space / WHITE-on-BLACK / no-attrs cells. Useful when an
///     agent needs per-cell granularity (e.g. computing per-cell
///     diffs across snapshots).
///   - `non_blank` — like `non_default` but only filters cells whose
///     char is space AND bg is BLACK. Keeps cells whose only
///     non-default trait is a non-WHITE fg.
///   - `all` — emit the full row-major grid. Caller owns the
///     response-size implications; useful for byte-perfect
///     deterministic snapshots in CI.
///
/// Cell::default semantics: `ch == ' ' && fg == WHITE && bg == BLACK
/// && attrs == 0 && width == 1` (the post-reset terminal cell
/// before any SGR has been applied).
fn filtered_cells(snap: &crate::session::GridSnapshot, mode: &str) -> serde_json::Value {
    let blank_ch = ' ';
    let blank_bg = [0u8, 0u8, 0u8];
    let blank_fg = [255u8, 255u8, 255u8];

    let is_default = |cell: &crate::session::CellSnapshot| -> bool {
        cell.ch == blank_ch
            && cell.bg == blank_bg
            && cell.fg == blank_fg
            && cell.attrs == 0
            && cell.width == 1
    };
    let is_blank = |cell: &crate::session::CellSnapshot| -> bool {
        cell.ch == blank_ch && cell.bg == blank_bg
    };

    if mode == "all" {
        return serde_json::to_value(&snap.cells).unwrap_or(serde_json::Value::Null);
    }

    if mode == "runs" {
        // Walk each row, batching width-1 same-style cells into one
        // CellRun. Default cells terminate any open run. Wide cells
        // flush + emit solo.
        let mut runs: Vec<CellRun> = Vec::new();
        for (r, row) in snap.cells.iter().enumerate() {
            // Open run: (start_col, accumulated text, fg, bg, attrs).
            let mut cur: Option<(usize, String, [u8; 3], [u8; 3], u8)> = None;
            let flush = |cur: &mut Option<(usize, String, [u8; 3], [u8; 3], u8)>,
                         runs: &mut Vec<CellRun>| {
                if let Some((col, text, fg, bg, attrs)) = cur.take() {
                    runs.push(CellRun { row: r, col, text, fg, bg, attrs });
                }
            };
            for (c, cell) in row.iter().enumerate() {
                if is_default(cell) {
                    flush(&mut cur, &mut runs);
                    continue;
                }
                if cell.width != 1 {
                    // Wide / continuation: flush, emit solo, no
                    // attempt to merge into a row run because column
                    // accounting would lie about the run's length.
                    flush(&mut cur, &mut runs);
                    if cell.width >= 1 {
                        runs.push(CellRun {
                            row: r,
                            col: c,
                            text: cell.ch.to_string(),
                            fg: cell.fg,
                            bg: cell.bg,
                            attrs: cell.attrs,
                        });
                    }
                    continue;
                }
                match &mut cur {
                    Some((_, text, fg, bg, attrs))
                        if *fg == cell.fg && *bg == cell.bg && *attrs == cell.attrs =>
                    {
                        text.push(cell.ch);
                    }
                    _ => {
                        flush(&mut cur, &mut runs);
                        cur = Some((
                            c,
                            cell.ch.to_string(),
                            cell.fg,
                            cell.bg,
                            cell.attrs,
                        ));
                    }
                }
            }
            flush(&mut cur, &mut runs);
        }
        return serde_json::to_value(&runs).unwrap_or(serde_json::Value::Null);
    }

    // Sparse modes (non_default / non_blank). Direct struct serde,
    // no per-cell BTreeMap allocation (the old json!{...} macro path
    // was a major share of the per-call CPU on a quiescent grid).
    let mut entries: Vec<SparseCellEntry> = Vec::new();
    for (r, row) in snap.cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let keep = match mode {
                "non_blank" => !is_blank(cell),
                _ => !is_default(cell),
            };
            if !keep {
                continue;
            }
            entries.push(SparseCellEntry {
                row: r,
                col: c,
                ch: cell.ch.to_string(),
                fg: cell.fg,
                bg: cell.bg,
                attrs: cell.attrs,
                width: cell.width,
            });
        }
    }
    serde_json::to_value(&entries).unwrap_or(serde_json::Value::Null)
}

/// Decode a `send_keys` payload into raw bytes for the PTY.
///
/// Backslash escapes (`\n`, `\r`, `\t`, `\\`, `\0`, `\x1b`, `\xHH`)
/// are honoured so agents can author "press Enter then Ctrl-C" as
/// `"foo\n\x03"` without juggling JSON-encoded literals. Anything
/// else passes through unchanged; an unrecognised escape produces
/// the literal backslash + char (best-effort, never panics on
/// malformed input).
fn decode_send_keys(input: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            None => {
                out.push(b'\\');
                break;
            }
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('\'') => out.push(b'\''),
            Some('"') => out.push(b'"'),
            Some('e') => out.push(0x1b),
            Some('x') => {
                let h1 = chars.next();
                let h2 = chars.next();
                match (h1, h2) {
                    (Some(a), Some(b)) => {
                        let hex = format!("{a}{b}");
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            out.push(byte);
                        } else {
                            out.extend_from_slice(b"\\x");
                            out.push(a as u8);
                            out.push(b as u8);
                        }
                    }
                    _ => out.extend_from_slice(b"\\x"),
                }
            }
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
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
    async fn mcp_send_keys_unknown_session_errors_cleanly() {
        let server = new_server();
        let input = SendKeysInput {
            session_id: "active".to_string(),
            keys: "ls\\n".to_string(),
        };
        let result = server.send_keys(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "no-such-session");
        assert_eq!(parsed["session_id"], "active");
    }

    #[tokio::test]
    async fn mcp_get_output_unknown_session_errors_cleanly() {
        let server = new_server();
        let input = GetOutputInput {
            session_id: "active".to_string(),
            lines: Some(10),
        };
        let result = server.get_output(Parameters(input)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "no-such-session");
    }

    #[tokio::test]
    async fn mcp_spawn_send_snapshot_end_to_end() {
        // The load-bearing test — proves the entire MCP→Session→PTY→
        // Terminal chain works end-to-end without a window or GPU.
        let server = new_server();

        // spawn a tiny shell session
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 40,
            rows: 8,
            ..TermSpec::default()
        };
        let raw = server.spawn_term(Parameters(spec)).await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);
        let id = parsed["session_id"].as_str().unwrap().to_string();

        // send a deterministic echo
        let raw = server
            .send_keys(Parameters(SendKeysInput {
                session_id: id.clone(),
                keys: "PS1=''; echo MADO_E2E\\n".into(),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);

        // poll the grid for the sentinel to land
        let mut found = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let raw = server
                .get_output(Parameters(GetOutputInput {
                    session_id: id.clone(),
                    lines: None,
                }))
                .await;
            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let text = parsed["output"].as_str().unwrap_or("");
            if text.contains("MADO_E2E") {
                found = true;
                break;
            }
        }
        assert!(found, "MCP→Session→PTY→Terminal chain did not deliver echo");

        // snapshot_grid returns full structured data
        let raw = server
            .snapshot_grid(Parameters(SnapshotGridInput {
                session_id: id.clone(),
                include_cells: Some(true),
                cells_filter: Some("non_default".into()),
                pretty: Some(true),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["cols"], 40);
        assert_eq!(parsed["rows"], 8);
        assert!(parsed["cells"].is_array());
        assert!(parsed["pretty"].is_string());

        // cleanup
        let raw = server
            .close_session(Parameters(SessionIdInput {
                session_id: id.clone(),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["closed"], true);
    }

    #[test]
    fn filtered_cells_runs_rle_same_attrs_run() {
        use crate::session::{CellSnapshot, GridSnapshot};
        // One 3-cell ASCII run with identical styling on row 0,
        // separated by a default cell, then another 2-cell run with
        // different fg. Expect: 2 runs total, both with the right
        // start col + accumulated text.
        let row: Vec<CellSnapshot> = vec![
            CellSnapshot { ch: 'a', width: 1, fg: [1, 2, 3], bg: [0, 0, 0], attrs: 0 },
            CellSnapshot { ch: 'b', width: 1, fg: [1, 2, 3], bg: [0, 0, 0], attrs: 0 },
            CellSnapshot { ch: 'c', width: 1, fg: [1, 2, 3], bg: [0, 0, 0], attrs: 0 },
            CellSnapshot { ch: ' ', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
            CellSnapshot { ch: 'x', width: 1, fg: [9, 9, 9], bg: [0, 0, 0], attrs: 0 },
            CellSnapshot { ch: 'y', width: 1, fg: [9, 9, 9], bg: [0, 0, 0], attrs: 0 },
        ];
        let snap = GridSnapshot {
            cols: 6,
            rows: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            cells: vec![row],
        };
        let runs = filtered_cells(&snap, "runs");
        let arr = runs.as_array().expect("runs returns array");
        assert_eq!(arr.len(), 2, "two RLE runs expected");
        assert_eq!(arr[0]["row"], 0);
        assert_eq!(arr[0]["col"], 0);
        assert_eq!(arr[0]["text"], "abc");
        assert_eq!(arr[0]["fg"], serde_json::json!([1, 2, 3]));
        assert_eq!(arr[1]["row"], 0);
        assert_eq!(arr[1]["col"], 4);
        assert_eq!(arr[1]["text"], "xy");
        assert_eq!(arr[1]["fg"], serde_json::json!([9, 9, 9]));
    }

    #[test]
    fn decode_send_keys_handles_common_escapes() {
        assert_eq!(decode_send_keys("ab\\n"), b"ab\n");
        assert_eq!(decode_send_keys("\\r\\t"), b"\r\t");
        assert_eq!(decode_send_keys("\\x1b[A"), b"\x1b[A");
        assert_eq!(decode_send_keys("\\x03"), &[0x03]);
        // unknown escape falls through to literal backslash + char
        assert_eq!(decode_send_keys("\\q"), b"\\q");
        // utf-8 passthrough
        assert_eq!(decode_send_keys("café"), "café".as_bytes());
    }

    // mcp_split_pane_json / mcp_new_tab_json removed at Phase 4 —
    // the tools they exercised no longer exist (multiplexing is
    // tear's domain now).

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

    #[tokio::test]
    async fn every_stubbed_tool_follows_uniform_shape() {
        // Contract: every IPC-stubbed tool returns {ok: false, tool,
        // note} at minimum. The session-aware tools (`spawn_term`,
        // `list_sessions`, `send_keys`, `get_output`, `snapshot_grid`,
        // `resize_session`, `close_session`) are real implementations
        // backed by `SessionRegistry` — they no longer go through
        // `stub_response`. Only the tools that still need a windowed
        // mado process (config_*, split_pane, new_tab) live here.
        let server = new_server();
        let responses: Vec<(&str, String)> = vec![
            (
                "config_get",
                server
                    .config_get(Parameters(ConfigGetInput { key: None }))
                    .await,
            ),
            (
                "config_set",
                server
                    .config_set(Parameters(ConfigSetInput {
                        key: "font_size".into(),
                        value: "14".into(),
                    }))
                    .await,
            ),
        ];
        for (tool_name, raw) in responses {
            let parsed: serde_json::Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("{tool_name} returned non-JSON: {e} in {raw:?}"));
            assert_eq!(parsed["ok"], false, "{tool_name}: ok should be false");
            assert_eq!(parsed["tool"], tool_name, "{tool_name}: tool field mismatch");
            let note = parsed["note"]
                .as_str()
                .unwrap_or_else(|| panic!("{tool_name}: note missing / non-string"));
            assert!(
                note.starts_with(tool_name),
                "{tool_name}: note should start with the tool name (got {note:?})",
            );
            assert!(
                note.contains("requires IPC"),
                "{tool_name}: note should mention IPC requirement",
            );
        }
    }

    #[test]
    fn stub_response_flattens_extra_fields_into_object() {
        // The helper must merge the `extra` object's fields into the
        // top-level response — callers should be able to add
        // tool-specific context without wrapping it.
        let raw = stub_response(
            "probe",
            serde_json::json!({ "hello": "world", "count": 3 }),
        );
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["tool"], "probe");
        assert_eq!(parsed["hello"], "world");
        assert_eq!(parsed["count"], 3);
    }

    #[test]
    fn stub_response_handles_empty_extra() {
        // A stub with no tool-specific context is still a valid
        // payload — used by new_tab() which has nothing to echo.
        let raw = stub_response("empty", serde_json::json!({}));
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["tool"], "empty");
        assert!(parsed["note"].is_string());
    }

    #[tokio::test]
    async fn clipboard_put_indexes_and_reports_duplicate_on_repeat() {
        // First put: stored, duplicate=false. Second identical put:
        // same hash, duplicate=true. Round-trips the hash back via
        // `clipboard_get` to prove store + MCP agree on the address.
        let store = Arc::new(Mutex::new(ClipboardStore::new(8)));
        let server = MadoMcp::with_clipboard(store);

        let raw1 = server
            .clipboard_put(Parameters(ClipboardPutInput {
                content: "deploy.sh --prod".into(),
                kind: None,
            }))
            .await;
        let first: serde_json::Value = serde_json::from_str(&raw1).unwrap();
        assert_eq!(first["ok"], true);
        assert_eq!(first["duplicate"], false);
        let hash = first["hash"].as_str().unwrap().to_string();
        assert_eq!(hash.len(), 32);
        assert_eq!(first["bytes"], "deploy.sh --prod".len());
        assert_eq!(first["kind"], "c"); // default

        let raw2 = server
            .clipboard_put(Parameters(ClipboardPutInput {
                content: "deploy.sh --prod".into(),
                kind: None,
            }))
            .await;
        let second: serde_json::Value = serde_json::from_str(&raw2).unwrap();
        assert_eq!(second["duplicate"], true);
        assert_eq!(second["hash"], hash);

        // Now fetch via clipboard_get — round-trip completes.
        let got = server
            .clipboard_get(Parameters(ClipboardGetInput { hash: hash.clone() }))
            .await;
        let got: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["content"], "deploy.sh --prod");
    }

    #[tokio::test]
    async fn clipboard_put_honours_explicit_kind() {
        let store = Arc::new(Mutex::new(ClipboardStore::new(8)));
        let server = MadoMcp::with_clipboard(store);

        let raw = server
            .clipboard_put(Parameters(ClipboardPutInput {
                content: "primary selection".into(),
                kind: Some("p".into()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["kind"], "p");

        // Unknown kind falls back to System (ghostty-permissive parse).
        let raw = server
            .clipboard_put(Parameters(ClipboardPutInput {
                content: "another".into(),
                kind: Some("zzz".into()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["kind"], "c");
    }

    #[tokio::test]
    async fn clipboard_clear_wipes_store_and_returns_count() {
        let store = Arc::new(Mutex::new(ClipboardStore::new(8)));
        {
            let mut guard = store.lock().unwrap();
            guard.store("a".into(), crate::clipboard_store::ClipboardKind::System);
            guard.store("b".into(), crate::clipboard_store::ClipboardKind::System);
            guard.store("c".into(), crate::clipboard_store::ClipboardKind::System);
        }
        let server = MadoMcp::with_clipboard(store.clone());

        let raw = server.clipboard_clear().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["cleared"], 3);

        // Store is actually empty afterwards.
        assert!(store.lock().unwrap().is_empty());

        // Clearing again is well-defined — just returns 0.
        let raw = server.clipboard_clear().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["cleared"], 0);
    }

    #[tokio::test]
    async fn clipboard_put_list_get_clear_lifecycle() {
        // End-to-end invariant: put → list sees it → get resolves it
        // → clear drops it → list is empty → get reports miss.
        let store = Arc::new(Mutex::new(ClipboardStore::new(8)));
        let server = MadoMcp::with_clipboard(store);

        let put_raw = server
            .clipboard_put(Parameters(ClipboardPutInput {
                content: "pipeline payload".into(),
                kind: None,
            }))
            .await;
        let put: serde_json::Value = serde_json::from_str(&put_raw).unwrap();
        let hash = put["hash"].as_str().unwrap().to_string();

        let list_raw = server
            .clipboard_list(Parameters(ClipboardListInput {
                limit: None,
                include_content: None,
            }))
            .await;
        let list: serde_json::Value = serde_json::from_str(&list_raw).unwrap();
        assert_eq!(list["count"], 1);

        let get_raw = server
            .clipboard_get(Parameters(ClipboardGetInput { hash: hash.clone() }))
            .await;
        let got: serde_json::Value = serde_json::from_str(&get_raw).unwrap();
        assert_eq!(got["found"], true);

        server.clipboard_clear().await;

        let list_raw = server
            .clipboard_list(Parameters(ClipboardListInput {
                limit: None,
                include_content: None,
            }))
            .await;
        let list: serde_json::Value = serde_json::from_str(&list_raw).unwrap();
        assert_eq!(list["count"], 0);

        let get_raw = server
            .clipboard_get(Parameters(ClipboardGetInput { hash }))
            .await;
        let got: serde_json::Value = serde_json::from_str(&get_raw).unwrap();
        assert_eq!(got["found"], false);
    }

    // ── Prompt-mark MCP tools ────────────────────────────────────────────

    use crate::osc_1337::UserMarkHistory;
    use crate::prompt_mark::{PromptHistory, PromptKind};

    fn server_with_seeded_prompt_marks(
        marks: &[(usize, PromptKind)],
    ) -> (MadoMcp, Arc<Mutex<PromptHistory>>) {
        let history = Arc::new(Mutex::new(PromptHistory::default()));
        {
            let mut guard = history.lock().unwrap();
            for (row, kind) in marks {
                guard.record(*row, *kind);
            }
        }
        let clipboard = Arc::new(Mutex::new(ClipboardStore::new(16)));
        let server = MadoMcp::with_state(SharedState {
            clipboard,
            prompt_marks: history.clone(),
            ..SharedState::default()
        });
        (server, history)
    }

    #[tokio::test]
    async fn prompt_marks_list_defaults_to_start_only() {
        let (server, _) = server_with_seeded_prompt_marks(&[
            (5, PromptKind::Start),
            (6, PromptKind::CommandStart),
            (10, PromptKind::Start),
            (11, PromptKind::CommandOutput),
            (20, PromptKind::Start),
        ]);
        let raw = server
            .prompt_marks_list(Parameters(PromptMarksListInput {
                limit: None,
                include_all_kinds: None,
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Only Start marks by default — 3 of them.
        assert_eq!(parsed["count"], 3);
        assert_eq!(parsed["total"], 5);
        let marks = parsed["marks"].as_array().unwrap();
        // Most-recent-first.
        assert_eq!(marks[0]["grid_row"], 20);
        assert_eq!(marks[0]["kind"], "Start");
        assert_eq!(marks[1]["grid_row"], 10);
        assert_eq!(marks[2]["grid_row"], 5);
    }

    #[tokio::test]
    async fn prompt_marks_list_honours_include_all_kinds() {
        let (server, _) = server_with_seeded_prompt_marks(&[
            (5, PromptKind::Start),
            (6, PromptKind::CommandStart),
            (7, PromptKind::CommandOutput),
            (8, PromptKind::CommandEnd),
        ]);
        let raw = server
            .prompt_marks_list(Parameters(PromptMarksListInput {
                limit: None,
                include_all_kinds: Some(true),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["count"], 4);
        let marks = parsed["marks"].as_array().unwrap();
        // Most-recent-first across every kind.
        assert_eq!(marks[0]["kind"], "CommandEnd");
        assert_eq!(marks[3]["kind"], "Start");
    }

    #[tokio::test]
    async fn prompt_marks_list_honours_limit() {
        let (server, _) = server_with_seeded_prompt_marks(&[
            (1, PromptKind::Start),
            (2, PromptKind::Start),
            (3, PromptKind::Start),
            (4, PromptKind::Start),
        ]);
        let raw = server
            .prompt_marks_list(Parameters(PromptMarksListInput {
                limit: Some(2),
                include_all_kinds: None,
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["count"], 2);
        // total reflects the full history.
        assert_eq!(parsed["total"], 4);
        let marks = parsed["marks"].as_array().unwrap();
        assert_eq!(marks[0]["grid_row"], 4);
        assert_eq!(marks[1]["grid_row"], 3);
    }

    #[tokio::test]
    async fn prompt_marks_clear_wipes_history_and_returns_prior_count() {
        let (server, history) = server_with_seeded_prompt_marks(&[
            (5, PromptKind::Start),
            (10, PromptKind::Start),
            (15, PromptKind::Start),
        ]);
        let raw = server.prompt_marks_clear().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["cleared"], 3);
        assert!(history.lock().unwrap().is_empty());

        // Clearing again reports 0.
        let raw = server.prompt_marks_clear().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["cleared"], 0);
    }

    #[tokio::test]
    async fn prompt_marks_list_on_empty_history_returns_empty_array() {
        let (server, _) = server_with_seeded_prompt_marks(&[]);
        let raw = server
            .prompt_marks_list(Parameters(PromptMarksListInput {
                limit: None,
                include_all_kinds: None,
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["count"], 0);
        assert_eq!(parsed["total"], 0);
        assert_eq!(parsed["marks"].as_array().unwrap().len(), 0);
    }

    // ── User-mark + attention MCP tools ──────────────────────────────────

    fn server_with_seeded_user_marks(
        rows: &[usize],
    ) -> (MadoMcp, Arc<Mutex<UserMarkHistory>>, Arc<Mutex<bool>>) {
        let clipboard = Arc::new(Mutex::new(ClipboardStore::new(16)));
        let prompt_marks = Arc::new(Mutex::new(PromptHistory::default()));
        let user_marks = Arc::new(Mutex::new(UserMarkHistory::default()));
        let attention = Arc::new(Mutex::new(false));
        {
            let mut guard = user_marks.lock().unwrap();
            for row in rows {
                guard.record(*row);
            }
        }
        let server = MadoMcp::with_state(SharedState {
            clipboard,
            prompt_marks,
            user_marks: user_marks.clone(),
            attention: attention.clone(),
            ..SharedState::default()
        });
        (server, user_marks, attention)
    }

    #[tokio::test]
    async fn user_marks_list_returns_most_recent_first() {
        let (server, _, _) = server_with_seeded_user_marks(&[1, 3, 7, 15]);
        let raw = server
            .user_marks_list(Parameters(UserMarksListInput { limit: None }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["count"], 4);
        assert_eq!(parsed["total"], 4);
        let marks = parsed["marks"].as_array().unwrap();
        assert_eq!(marks[0]["grid_row"], 15);
        assert_eq!(marks[1]["grid_row"], 7);
        assert_eq!(marks[2]["grid_row"], 3);
        assert_eq!(marks[3]["grid_row"], 1);
    }

    #[tokio::test]
    async fn user_marks_list_honours_limit_with_total_unchanged() {
        let (server, _, _) = server_with_seeded_user_marks(&[1, 2, 3, 4, 5]);
        let raw = server
            .user_marks_list(Parameters(UserMarksListInput { limit: Some(2) }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["count"], 2);
        // total reflects the full history regardless of limit.
        assert_eq!(parsed["total"], 5);
    }

    #[tokio::test]
    async fn user_marks_clear_wipes_history() {
        let (server, history, _) = server_with_seeded_user_marks(&[5, 10, 20]);
        let raw = server.user_marks_clear().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["cleared"], 3);
        assert!(history.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn attention_get_reflects_shared_state() {
        let (server, _, attention) = server_with_seeded_user_marks(&[]);
        // Defaults to false.
        let raw = server.attention_get().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["attention_requested"], false);

        // Flip externally (simulating a Terminal receiving OSC 1337
        // RequestAttention=1) and confirm the getter sees it.
        *attention.lock().unwrap() = true;
        let raw = server.attention_get().await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["attention_requested"], true);
    }

    #[test]
    fn shared_state_default_initializes_every_handle() {
        // Struct-update contract: SharedState::default() gives a
        // server that behaves as if no activity has occurred.
        let state = SharedState::default();
        assert!(state.clipboard.lock().unwrap().is_empty());
        assert!(state.prompt_marks.lock().unwrap().is_empty());
        assert!(state.user_marks.lock().unwrap().is_empty());
        assert!(!*state.attention.lock().unwrap());
    }

    #[test]
    fn shared_state_struct_update_preserves_overridden_handle() {
        // Tests rely on `..SharedState::default()` to override one
        // handle. Pin that the overridden handle is the same Arc
        // (ptr equality) — struct-update must forward the user's
        // handle verbatim, not clone-reset it.
        let clipboard = Arc::new(Mutex::new(ClipboardStore::new(4)));
        let state = SharedState {
            clipboard: clipboard.clone(),
            ..SharedState::default()
        };
        assert!(Arc::ptr_eq(&state.clipboard, &clipboard));
    }

    #[tokio::test]
    async fn attention_set_updates_shared_state() {
        let (server, _, attention) = server_with_seeded_user_marks(&[]);
        let raw = server
            .attention_set(Parameters(AttentionSetInput { requested: true }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["attention_requested"], true);
        // The shared handle sees the write — the Terminal reads from
        // this same handle on every frame to decide whether to
        // signal the window manager.
        assert!(*attention.lock().unwrap());

        // Cancel.
        let raw = server
            .attention_set(Parameters(AttentionSetInput { requested: false }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["attention_requested"], false);
        assert!(!*attention.lock().unwrap());
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

    // ── tear MCP-bridge integration tests ─────────────────────
    //
    // Each test below spins up an in-process tear-daemon at a
    // private socket, sets MADO_CONFIG to a temp file pointing
    // mado at that socket, then drives the relevant `tear_*`
    // tool. Tests are #[tokio::test] (not #[tokio::test(flavor =
    // "multi_thread")]) so we don't accidentally race other tests
    // that also touch MADO_CONFIG — within a single test the
    // env-var-mutation is safe.

    fn write_temp_mado_config(socket: &std::path::Path) -> tempfile::NamedTempFile {
        use std::io::Write;
        // shikumi auto-detects format by extension; a no-extension
        // temp file silently falls through to TOML and fails to parse
        // YAML, which then degrades to default config and breaks the
        // socket-override pathway. Suffix matters.
        let mut tmp = tempfile::Builder::new()
            .prefix("mado-mcp-")
            .suffix(".yaml")
            .tempfile()
            .expect("tempfile");
        let yaml = format!(
            "font_family: \"JetBrainsMono Nerd Font Mono\"\n\
             font_size: 14\n\
             theme: nord\n\
             tear:\n  \
               mode: auto\n  \
               socket: {sock}\n  \
               auto_spawn: false\n  \
               spawn_wait_ms: 100\n",
            sock = socket.display()
        );
        tmp.write_all(yaml.as_bytes()).expect("write");
        tmp
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use serde_json::Value;
    use tear_config::{LiveConfig, TearConfig};
    use tear_core::InProcess;
    use tear_daemon::DaemonHandle;

    /// Serialise every test that mutates the process-global
    /// `MADO_CONFIG`. Without this, parallel `cargo test` workers
    /// race: one test's discovery picks up another test's temp
    /// file and points at the wrong socket. The guard holds the
    /// lock for the lifetime of the test body.
    static MADO_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Monotonic harness counter — appended to PID for socket-path
    /// disambiguation across many TearMcpHarness instances in one
    /// process. Avoids "address already in use" when tests pick the
    /// same PID-suffixed socket path back-to-back.
    static HARNESS_SEQ: AtomicU32 = AtomicU32::new(0);

    /// Scope-guard restoring MADO_CONFIG on Drop. Held inside
    /// `TearMcpHarness` for the lifetime of the test. `PoisonError`
    /// is intentionally swallowed so a panic in one test doesn't
    /// permanently kill the rest.
    struct MadoConfigGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl MadoConfigGuard {
        fn set(path: &std::path::Path) -> Self {
            let lock = MADO_CONFIG_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var("MADO_CONFIG").ok();
            unsafe { std::env::set_var("MADO_CONFIG", path) };
            Self { prev, _lock: lock }
        }
    }
    impl Drop for MadoConfigGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => unsafe { std::env::set_var("MADO_CONFIG", v) },
                None => unsafe { std::env::remove_var("MADO_CONFIG") },
            }
        }
    }

    /// Per-test scaffold: spins up an in-process `tear-daemon` with a
    /// fresh `LiveConfig`, points MADO_CONFIG at a `.yaml` tempfile,
    /// and hands back a ready-to-call `MadoMcp` server.
    ///
    /// Holds the env-var lock for the lifetime of the test so
    /// parallel workers don't stomp on each other. Drop stops the
    /// daemon and unlinks the socket file. Tests that need the
    /// daemon's `LiveConfig` (e.g. to assert that a `tear_set_config`
    /// actually mutated daemon-side state) read `h.live.load()`.
    struct TearMcpHarness {
        /// The MCP server under test.
        server: MadoMcp,
        /// Daemon-side live config — read with `h.live.load()` to
        /// assert that `tear_set_config_yaml` actually flowed
        /// through.
        live: Arc<LiveConfig>,
        // ── kept alive for the test's lifetime ──────────────────
        daemon: Option<DaemonHandle>,
        _cfg_file: tempfile::NamedTempFile,
        _guard: MadoConfigGuard,
    }

    impl TearMcpHarness {
        fn new() -> Self {
            let pid = std::process::id();
            let seq = HARNESS_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut socket = std::env::temp_dir();
            socket.push(format!("mado-mcp-h-{pid}-{seq}.sock"));
            let inproc = Arc::new(InProcess::new());
            let live = Arc::new(LiveConfig::default());
            let daemon = tear_daemon::start_with_config(
                socket.clone(),
                inproc,
                live.clone(),
            )
            .expect("daemon");
            std::thread::sleep(Duration::from_millis(50));
            let cfg_file = write_temp_mado_config(&socket);
            let guard = MadoConfigGuard::set(cfg_file.path());
            Self {
                server: new_server(),
                live,
                daemon: Some(daemon),
                _cfg_file: cfg_file,
                _guard: guard,
            }
        }

        /// Helper: parse a `tear_*` JSON response. Panics on bad
        /// JSON — every tool is contracted to emit `Value::Object`,
        /// so a parse failure is a real bug worth panicking on.
        fn parse(raw: &str) -> Value {
            serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("malformed JSON: {e}\nraw: {raw}"))
        }
    }

    impl Drop for TearMcpHarness {
        fn drop(&mut self) {
            if let Some(d) = self.daemon.take() {
                d.stop();
            }
        }
    }

    // ── status / discovery paths ───────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn tear_status_reports_reachable_when_daemon_is_live() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(&h.server.tear_status().await);
        assert_eq!(v["reachable"], true);
        assert_eq!(v["sessions"], 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_status_reports_fallback_when_no_daemon() {
        // No harness — point MADO_CONFIG at a YAML that names a
        // non-existent socket so discover() returns Fallback.
        let socket = std::path::PathBuf::from("/tmp/no-such-mado-mcp-fallback.sock");
        let cfg_file = write_temp_mado_config(&socket);
        let _g = MadoConfigGuard::set(cfg_file.path());
        let server = new_server();
        let v = TearMcpHarness::parse(&server.tear_status().await);
        assert_eq!(v["reachable"], false);
        assert_eq!(v["fallback"], true);
    }

    /// `with_tear_client` Fallback branch — every non-status tool
    /// that hits a downed daemon must emit `{ok: false, fallback:
    /// true, error: …}` consistently. Pick `tear_list_sessions`
    /// as the representative tool (covers the whole helper, not
    /// the tool's specific logic).
    #[tokio::test(flavor = "current_thread")]
    async fn with_tear_client_fallback_branch_returns_uniform_shape() {
        let socket = std::path::PathBuf::from("/tmp/no-such-mado-mcp-list.sock");
        let cfg_file = write_temp_mado_config(&socket);
        let _g = MadoConfigGuard::set(cfg_file.path());
        let server = new_server();
        let v = TearMcpHarness::parse(&server.tear_list_sessions().await);
        assert_eq!(v["ok"], false);
        assert_eq!(v["fallback"], true);
        assert!(v["error"].as_str().unwrap().contains("not reachable"));
    }

    /// `with_tear_client` Required branch — `tear.mode = "always"`
    /// + auto_spawn = false + no daemon → operator-facing
    /// `{required: true}` response.
    #[tokio::test(flavor = "current_thread")]
    async fn with_tear_client_required_branch_when_mode_always_and_no_daemon() {
        use std::io::Write;
        let socket = std::path::PathBuf::from("/tmp/no-such-mado-mcp-required.sock");
        let mut tmp = tempfile::Builder::new()
            .prefix("mado-mcp-")
            .suffix(".yaml")
            .tempfile()
            .unwrap();
        writeln!(
            tmp,
            "tear:\n  mode: always\n  socket: {}\n  auto_spawn: false\n  spawn_wait_ms: 50\n",
            socket.display()
        )
        .unwrap();
        let _g = MadoConfigGuard::set(tmp.path());
        let server = new_server();
        let v = TearMcpHarness::parse(&server.tear_list_sessions().await);
        assert_eq!(v["ok"], false);
        assert_eq!(v["required"], true);
        assert!(v["error"].as_str().unwrap().contains("not reachable"));
    }

    // ── config round-trip: get / set / reload ──────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn tear_get_config_returns_parseable_yaml() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(&h.server.tear_get_config().await);
        assert_eq!(v["ok"], true);
        let yaml = v["yaml"].as_str().unwrap();
        let _: TearConfig = serde_yaml_ng::from_str(yaml).expect("yaml parses");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_set_config_yaml_mutates_daemon_live_config() {
        let h = TearMcpHarness::new();
        assert_eq!(h.live.load().prefix, TearConfig::default().prefix);

        let new_cfg = TearConfig {
            prefix: "C-Space".into(),
            ..TearConfig::default()
        };
        let yaml = serde_yaml_ng::to_string(&new_cfg).unwrap();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_set_config_yaml(Parameters(TearSetConfigYamlInput { yaml }))
                .await,
        );
        assert_eq!(v["ok"], true);
        assert_eq!(h.live.load().prefix, "C-Space");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_set_config_yaml_rejects_malformed_payload() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_set_config_yaml(Parameters(TearSetConfigYamlInput {
                    yaml: "::: not valid yaml :::".into(),
                }))
                .await,
        );
        assert_eq!(v["ok"], false);
        assert!(v["error"].is_string());
        // LiveConfig must NOT have been mutated.
        assert_eq!(h.live.load().prefix, TearConfig::default().prefix);
    }

    /// `tear_reload_config` re-reads `~/.config/tear/tear.yaml`. The
    /// daemon documents two outcomes:
    ///   - file present + parseable → swap to the on-disk shape
    ///   - file missing or unparseable → return Err, leave the
    ///     previous override in place
    ///
    /// We can't depend on the test host having (or not having) the
    /// canonical config file, so the assertion is the conjunction:
    /// `ok=true` ⇒ live.load() ≠ prior override (reverted); `ok=false`
    /// ⇒ live.load() == prior override (unchanged). Either way the
    /// override doesn't leak past reload's contract.
    #[tokio::test(flavor = "current_thread")]
    async fn tear_reload_config_either_swaps_or_preserves_override() {
        let h = TearMcpHarness::new();
        let pushed = TearConfig {
            prefix: "C-Override".into(),
            ..TearConfig::default()
        };
        let yaml = serde_yaml_ng::to_string(&pushed).unwrap();
        let _ = h
            .server
            .tear_set_config_yaml(Parameters(TearSetConfigYamlInput { yaml }))
            .await;
        assert_eq!(h.live.load().prefix, "C-Override");

        let v = TearMcpHarness::parse(&h.server.tear_reload_config().await);
        let after = h.live.load();
        match v["ok"].as_bool().unwrap() {
            true => assert_ne!(
                after.prefix, "C-Override",
                "reload reported ok but kept the override — contract violation"
            ),
            false => assert_eq!(
                after.prefix, "C-Override",
                "reload errored but mutated state — contract violation"
            ),
        }
    }

    // ── session lifecycle ──────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn tear_new_session_returns_session_and_first_pane() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_new_session(Parameters(TearNewSessionInput {
                    name: Some("lifecycle".into()),
                    shell: Some("/bin/sh".into()),
                }))
                .await,
        );
        assert_eq!(v["ok"], true);
        assert!(v["session_id"].as_str().unwrap().len() == 16);
        assert!(v["first_pane_id"].as_str().unwrap().len() == 16);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_new_session_uses_defaults_when_fields_omitted() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_new_session(Parameters(TearNewSessionInput::default()))
                .await,
        );
        assert_eq!(v["ok"], true);
        // Sanity — list_sessions sees one session named "mcp-session".
        let v2 = TearMcpHarness::parse(&h.server.tear_list_sessions().await);
        let sessions = v2["sessions"].as_array().unwrap();
        assert!(sessions.iter().any(|s| s["name"] == "mcp-session"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_kill_session_removes_session_from_list() {
        let h = TearMcpHarness::new();
        let new = TearMcpHarness::parse(
            &h.server
                .tear_new_session(Parameters(TearNewSessionInput::default()))
                .await,
        );
        let sid = new["session_id"].as_str().unwrap().to_owned();

        let v = TearMcpHarness::parse(
            &h.server
                .tear_kill_session(Parameters(TearSessionIdInput {
                    session_id: sid.clone(),
                }))
                .await,
        );
        assert_eq!(v["ok"], true);
        let v2 = TearMcpHarness::parse(&h.server.tear_list_sessions().await);
        assert!(v2["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != sid));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_kill_session_with_invalid_id_returns_error() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_kill_session(Parameters(TearSessionIdInput {
                    session_id: "not-hex".into(),
                }))
                .await,
        );
        assert_eq!(v["ok"], false);
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("invalid session_id"));
    }

    // ── pane snapshot + send_keys ──────────────────────────────────

    /// The load-bearing vertical: spawn a real shell session via
    /// MCP, drive a `printf MARKER\n` via `tear_send_keys`, poll
    /// `tear_pane_snapshot` until the marker shows up. Proves the
    /// whole mado-MCP → tear-daemon → PTY → vte pipe is alive.
    #[tokio::test(flavor = "current_thread")]
    async fn tear_send_keys_marker_appears_in_pane_snapshot() {
        let h = TearMcpHarness::new();
        let new = TearMcpHarness::parse(
            &h.server
                .tear_new_session(Parameters(TearNewSessionInput {
                    name: Some("marker".into()),
                    shell: Some("/bin/sh".into()),
                }))
                .await,
        );
        let pid_str = new["first_pane_id"].as_str().unwrap().to_owned();

        let v = TearMcpHarness::parse(
            &h.server
                .tear_send_keys(Parameters(TearSendKeysInput {
                    pane_id: pid_str.clone(),
                    keys: "printf 'MADO_MCP_TEAR_MARKER\\n'\n".into(),
                }))
                .await,
        );
        assert_eq!(v["ok"], true);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let v = TearMcpHarness::parse(
                &h.server
                    .tear_pane_snapshot(Parameters(TearPaneIdInput {
                        pane_id: pid_str.clone(),
                    }))
                    .await,
            );
            let text = v["text_rows"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.contains("MADO_MCP_TEAR_MARKER") {
                saw = true;
                break;
            }
        }
        assert!(saw, "MARKER never appeared in pane_snapshot via MCP");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_pane_snapshot_with_invalid_id_returns_error() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_pane_snapshot(Parameters(TearPaneIdInput {
                    pane_id: "not-hex".into(),
                }))
                .await,
        );
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("invalid pane_id"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tear_send_keys_with_invalid_id_returns_error() {
        let h = TearMcpHarness::new();
        let v = TearMcpHarness::parse(
            &h.server
                .tear_send_keys(Parameters(TearSendKeysInput {
                    pane_id: "not-hex".into(),
                    keys: "ignored".into(),
                }))
                .await,
        );
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("invalid pane_id"));
    }

    /// Snapshot of a real (but freshly-spawned) pane returns
    /// sensible dimensions + empty grid. Catches regressions where
    /// the daemon returns 0-sized panes or the JSON marshalling
    /// drops fields.
    #[tokio::test(flavor = "current_thread")]
    async fn tear_pane_snapshot_returns_grid_shape() {
        let h = TearMcpHarness::new();
        let new = TearMcpHarness::parse(
            &h.server
                .tear_new_session(Parameters(TearNewSessionInput::default()))
                .await,
        );
        let pid_str = new["first_pane_id"].as_str().unwrap_or_else(|| {
            panic!("first_pane_id missing from new_session response: {new}")
        }).to_owned();
        let raw = h.server
            .tear_pane_snapshot(Parameters(TearPaneIdInput {
                pane_id: pid_str,
            }))
            .await;
        let v = TearMcpHarness::parse(&raw);
        assert_eq!(v["ok"], true, "snapshot returned ok=false: {raw}");
        assert!(v["cols"].as_u64().unwrap() > 0);
        assert!(v["rows"].as_u64().unwrap() > 0);
        // Cursor must be inside the grid.
        assert!(v["cursor_col"].as_u64().unwrap() < v["cols"].as_u64().unwrap());
        assert!(v["cursor_row"].as_u64().unwrap() < v["rows"].as_u64().unwrap());
        assert!(v["alt_screen_active"].is_boolean());
        assert!(v["text_rows"].is_array());
    }

    // ── helper / refactor invariants ───────────────────────────────

    #[test]
    fn ok_json_with_null_extra_emits_just_ok_true() {
        let s = ok_json(serde_json::Value::Null);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v, serde_json::json!({ "ok": true }));
    }

    #[test]
    fn ok_json_with_object_extra_merges_fields() {
        let s = ok_json(serde_json::json!({ "a": 1, "b": "x" }));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v, serde_json::json!({ "ok": true, "a": 1, "b": "x" }));
    }

    #[test]
    fn ok_json_with_non_object_extra_silently_drops_extras() {
        // `extra` is contract-bound to be an object; non-object input
        // must not corrupt the response (silent drop is preferable to
        // a runtime panic in an MCP tool body).
        let s = ok_json(serde_json::json!([1, 2, 3]));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v, serde_json::json!({ "ok": true }));
    }

    #[test]
    fn err_json_includes_display_string() {
        let s = err_json("boom: bad input");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "boom: bad input");
    }

    #[test]
    fn err_json_uses_display_impl_for_arbitrary_errors() {
        struct E;
        impl std::fmt::Display for E {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "custom-display")
            }
        }
        let s = err_json(E);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"], "custom-display");
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let server = MadoMcp::new().serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
