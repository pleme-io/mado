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

    #[tool(description = "Get mado application status and health information. Returns JSON with running state, session count, and uptime.")]
    async fn status(&self) -> String {
        // P15 — read the live count from the session registry rather
        // than the hardcoded 0 the field had before. The mismatch
        // (`status` reporting 0 while `list_sessions` returned ≥2)
        // was visible in the 2026-05-13 MCP baseline.
        let sessions = self.state.sessions.list().len();
        serde_json::json!({
            "status": "running",
            "app": "mado",
            "sessions": sessions,
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
            Some(key) => stub_response(
                "config_get",
                serde_json::json!({ "key": key, "value": null }),
            ),
            None => stub_response(
                "config_get",
                serde_json::json!({ "config_path": "~/.config/mado/mado.yaml" }),
            ),
        }
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

    #[tool(description = "List every live headless terminal session. Each entry has `id`, `title`, `shell`, `cols`, `rows`, `created_at_unix_ms`, `uptime_ms`. Sorted most-recently-spawned first.")]
    async fn list_sessions(&self) -> String {
        let summaries = self.state.sessions.list();
        serde_json::json!({ "sessions": summaries }).to_string()
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

    #[tool(description = "Create a new split pane in the active tab. Specify horizontal or vertical split direction.")]
    async fn split_pane(&self, Parameters(input): Parameters<SplitPaneInput>) -> String {
        stub_response(
            "split_pane",
            serde_json::json!({
                "direction": input.direction,
                "command": input.command,
            }),
        )
    }

    #[tool(description = "Create a new tab in the terminal window. Optionally run a command in the new tab.")]
    async fn new_tab(&self) -> String {
        stub_response("new_tab", serde_json::json!({}))
    }

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
            (
                "split_pane",
                server
                    .split_pane(Parameters(SplitPaneInput {
                        direction: "vertical".into(),
                        command: None,
                    }))
                    .await,
            ),
            ("new_tab", server.new_tab().await),
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
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let server = MadoMcp::new().serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
