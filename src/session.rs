//! Headless terminal sessions for the MCP introspection surface.
//!
//! A [`Session`] is mado's terminal core (PTY + vte-driven [`Terminal`])
//! without any GPU, winit, or window — exactly the slice an agent
//! driving mado over MCP needs. The session reads bytes from the PTY
//! into the [`Terminal`] state machine on a background tokio task; the
//! MCP tools poke it via [`SessionRegistry`].
//!
//! ## Why this exists (Constructive Substrate Engineering)
//!
//! "Same bugs" rendering reports from the operator are hard to triage
//! from screenshots. With headless sessions the renderer's *input*
//! (the cell grid) becomes a typed snapshot — `snapshot_grid` returns
//! exactly what `render::build_text_buffers` reads, so any visual
//! artifact can be traced to either:
//!
//! 1. **Terminal core** (cell grid is wrong) — grep / diff the snapshot.
//! 2. **GPU renderer** (cell grid is right, pixels are wrong) — render
//!    to an offscreen wgpu target via `snapshot_png` (M1) and inspect.
//!
//! The pattern is intended to lift into `garasu-introspect` once a
//! second GPU app needs it; fumi, kagi, hibiki, namimado all share
//! the same wgpu + glyphon + cell-grid shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use anyhow::anyhow;
use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::pty::Pty;
use crate::term_spec::TermSpec;
use crate::terminal::{Cell, Terminal, UnderlineStyle};

// The floor moved to `shell_resolve`, the only place that decides what to
// spawn. This was one of four `/bin/sh` floors that did not agree.

/// A live terminal session — PTY + state machine + reader pump.
///
/// Ownership is structured so callers can hand out `Arc<Session>` to
/// MCP tool handlers freely. The `_reader_task` field anchors the
/// reader pump to the session lifetime: when the registry drops the
/// last `Arc<Session>`, the task is cancelled via [`JoinHandle::abort`]
/// in the [`Drop`] impl, and the PTY's master-fd reader stops pulling
/// bytes — which lets the child exit (or be killed via [`Pty`]'s
/// drop, which kills the child).
pub struct Session {
    pub id: String,
    pub title: String,
    pub shell: String,
    pub created_at_unix_ms: u128,
    pub created_at_instant: Instant,
    terminal: Arc<parking_lot::RwLock<Terminal>>,
    pty: Arc<AsyncMutex<Pty>>,
    /// Lock around the PTY writer half. Held only while a write is in
    /// flight so concurrent `send_keys` calls serialize correctly.
    writer: Arc<AsyncMutex<crate::pty::PtyWriter>>,
    /// Anchored reader pump — aborted on drop.
    _reader_task: JoinHandle<()>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let term = self.terminal.read();
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("shell", &self.shell)
            .field("cols", &term.cols())
            .field("rows", &term.rows())
            .finish()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self._reader_task.abort();
    }
}

impl Session {
    /// Send raw bytes (typically keystrokes) to the PTY master. The
    /// terminal state advances when the child writes back; agents
    /// that need to wait for output should poll [`snapshot_grid`]
    /// (currently lock-step; future work: `seqno`-based wait).
    pub async fn send_input(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(bytes).await.context("pty write_all")?;
        Ok(())
    }

    /// Resize the PTY *and* the terminal grid together. Both are
    /// required: the kernel-level winsize controls SIGWINCH delivery
    /// to the child, the in-memory grid controls what `feed` reads/writes.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let pty = self.pty.lock().await;
        pty.resize(cols, rows).context("pty resize")?;
        let mut term = self.terminal.write();
        term.resize(cols as usize, rows as usize);
        Ok(())
    }

    /// Shared handle to the underlying terminal. Lets the L3 visual-
    /// golden check in `scenario.rs` build a TerminalRenderer over
    /// the live session state without going through GridSnapshot
    /// (which loses fg/bg/attrs needed for accurate rendering).
    pub fn terminal_arc(&self) -> std::sync::Arc<parking_lot::RwLock<Terminal>> {
        self.terminal.clone()
    }

    /// Take a complete snapshot of the terminal grid + cursor +
    /// metadata. This is the load-bearing introspection primitive —
    /// every visual-rendering question can be answered by diffing
    /// this against the rendered output.
    pub fn snapshot_grid(&self) -> GridSnapshot {
        let term = self.terminal.read();
        let cols = term.cols();
        let rows = term.rows();
        let cursor = *term.cursor();
        let styles = term.styles();
        let cells: Vec<Vec<CellSnapshot>> = term
            .visible_rows()
            .map(|row| {
                row.iter()
                    .take(cols)
                    .map(|c| CellSnapshot::from_cell(c, styles))
                    .collect()
            })
            .collect();
        GridSnapshot {
            cols,
            rows,
            cursor_row: cursor.row,
            cursor_col: cursor.col,
            cursor_visible: cursor.visible,
            cells,
        }
    }
}

/// Compact, JSON-serializable mirror of a single terminal [`Cell`].
///
/// We intentionally flatten the attribute bitfield and emit colours
/// as `[r,g,b]` triplets — the MCP wire format is what the agent
/// reads, so the schema should be obvious without a glossary.
#[derive(Debug, Clone, Serialize)]
pub struct CellSnapshot {
    /// Glyph at this cell. Wide-char continuation cells use `' '`
    /// (width == 0); the caller already skipped via `take(cols)` so
    /// the column count is correct.
    pub ch: char,
    /// Cell width in columns: 1 for normal, 2 for wide chars, 0 for
    /// continuation slots of the preceding wide char.
    pub width: u8,
    /// Foreground colour `[r, g, b]` after palette resolution.
    pub fg: [u8; 3],
    /// Background colour `[r, g, b]` after palette resolution.
    pub bg: [u8; 3],
    /// Legacy attrs bitfield as u8 — bold/italic/underline/inverse/
    /// dim/strikethrough/hidden/blink, in the historical `CellAttrs`
    /// bit positions (now produced via `Attrs::to_legacy_bits`).
    /// Kept for wire back-compat; any non-"none" underline style sets
    /// the single legacy underline bit.
    pub attrs: u8,
    /// M2 — typed underline style name: `none` / `single` / `double` /
    /// `curly` / `dotted` / `dashed` (SGR 4:N wire).
    pub underline: String,
    /// M2 — typed underline colour (SGR 58/59): `None` when the
    /// underline follows the cell fg (default), otherwise
    /// `indexed(N)` or `#rrggbb`.
    pub underline_color: Option<String>,
}

impl CellSnapshot {
    /// Legacy-shaped constructor — a cell carrying only the u8 attrs
    /// bitfield. `underline_color` has no u8 representation and stays
    /// `None`; `underline` is DERIVED from bit 2 of `attrs`.
    ///
    /// It used to be hardcoded to `"none"`, and that made this
    /// constructor lie about its own input: the legacy u8 layout
    /// carries underline at bit 2 (`CellAttrs::UNDERLINE`), so an
    /// underlined cell came back as `attrs: 4, underline: "none"` — two
    /// fields of one snapshot flatly contradicting each other.
    ///
    /// Not cosmetic, and not hypothetical. `snapshot_grid` is the tool
    /// whose whole job is triaging "the screen renders wrong", and the
    /// live embedded-pane path below is its ONLY non-zero-attrs caller.
    /// So while every cell in a tear pane was wrongly underlined (tear
    /// latched UNDERLINE on the pen from `CSI > 4 ; 2 m`, fixed in tear
    /// 3a94f7a/d117fa6), the one diagnostic built to see that reported
    /// `underline: "none"` for all of them. The bug was legible in the
    /// raw bitfield and invisible in the field named after it.
    ///
    /// The u8 can only say underlined-or-not, so `Single` is the honest
    /// projection — a `4:3` curly cell reports `"single"` here. Callers
    /// wanting the true style use [`Self::from_cell`], which reads the
    /// typed [`UnderlineStyle`] directly.
    #[must_use]
    #[allow(dead_code)] // Test-fixture surface across modules (mcp/scenario/session tests).
    pub fn legacy(ch: char, width: u8, fg: [u8; 3], bg: [u8; 3], attrs: u8) -> Self {
        let underline = if attrs & crate::terminal::CellAttrs::UNDERLINE.bits() == 0 {
            UnderlineStyle::None
        } else {
            UnderlineStyle::Single
        };
        Self {
            ch,
            width,
            fg,
            bg,
            attrs,
            underline: underline.to_string(),
            underline_color: None,
        }
    }

    fn from_cell(c: &Cell, styles: &crate::terminal::StyleTable) -> Self {
        let style = c.style(styles);
        let attrs = style.attrs;
        Self {
            ch: c.ch,
            width: c.width,
            fg: [style.fg.r, style.fg.g, style.fg.b],
            bg: [style.bg.r, style.bg.g, style.bg.b],
            attrs: attrs.to_legacy_bits(),
            underline: attrs.underline.to_string(),
            underline_color: match attrs.underline_color {
                crate::terminal::UnderlineColor::Default => None,
                other => Some(other.to_string()),
            },
        }
    }
}

/// Full grid snapshot — emitted as JSON by `snapshot_grid`.
///
/// The shape mirrors `render::Snapshot` but is owned-only and
/// serde-friendly. Agents can diff two snapshots field-by-field; the
/// `cells` vector is row-major so position `[row][col]` matches the
/// terminal coordinate system.
#[derive(Debug, Clone, Serialize)]
pub struct GridSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    pub cells: Vec<Vec<CellSnapshot>>,
}

impl GridSnapshot {
    /// Compact text representation — one row per line, trailing
    /// whitespace stripped. Useful when the agent only needs to
    /// pattern-match the prompt or read command output.
    ///
    /// P16 — trim per-row in a scratch buffer, not on the cumulative
    /// `out`. The previous implementation trimmed the cumulative
    /// String after each row, which was correct in most cases but
    /// fragile around (a) all-blank rows, (b) cells with `ch == '\0'`
    /// inside a row (legitimate when a CSI hard-resets the buffer
    /// mid-row), and (c) preserving the "newline per row" contract
    /// the agent baseline depends on. Per-row trim is the obviously-
    /// correct shape, and the cost is one small String per row.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(self.cols * self.rows);
        let mut row_buf = String::with_capacity(self.cols);
        for (idx, row) in self.cells.iter().enumerate() {
            row_buf.clear();
            for cell in row {
                if cell.width == 0 {
                    continue;
                }
                row_buf.push(cell.ch);
            }
            let trimmed = row_buf.trim_end_matches([' ', '\0']);
            out.push_str(trimmed);
            if idx + 1 < self.cells.len() {
                out.push('\n');
            }
        }
        out
    }

    /// Render the cell grid as a Unicode-decorated string showing
    /// per-cell attributes. Used by the `snapshot_grid` MCP tool's
    /// `pretty: true` mode — a quick visual scan of what mado thinks
    /// is on screen. Cursor position is marked with `▓`.
    #[must_use]
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "grid {}×{}  cursor=({},{}) visible={}",
            self.cols, self.rows, self.cursor_row, self.cursor_col, self.cursor_visible
        );
        for (r, row) in self.cells.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if cell.width == 0 {
                    continue;
                }
                if r == self.cursor_row && c == self.cursor_col {
                    out.push('▓');
                } else if cell.ch == ' ' && cell.bg == [0, 0, 0] {
                    out.push('·');
                } else {
                    out.push(cell.ch);
                }
            }
            out.push('\n');
        }
        out
    }
}

/// The monotonic spawn sequence parsed out of a session id's numeric
/// suffix (`mado-session-<n>` → `n`). Both the "most-recent" ordering
/// in [`SessionRegistry::list`] and the focused-session pick in
/// [`SessionRegistry::focused_cwd`] order by this — one definition so
/// the two can never diverge into a silent focus mismatch (review
/// 2026-06-12, mechanical-audit-1). Unparseable ids sort as `0`.
fn session_seq(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Typed registry of live headless sessions.
///
/// One process owns one registry; the MCP tools read/write via a
/// shared `Arc<SessionRegistry>` plumbed through `SharedState`. The
/// registry runs entirely inside the MCP-server tokio runtime — the
/// reader pump tasks live there too.
#[derive(Debug)]
pub struct SessionRegistry {
    inner: Mutex<HashMap<String, Arc<Session>>>,
    next_id: AtomicU64,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }
}

impl SessionRegistry {
    /// Spawn a new headless session from a [`TermSpec`].
    ///
    /// The spec's `shell` falls back through `$SHELL` → `/bin/sh`;
    /// `cols`/`rows` are resolved via [`TermSpec::resolved_dimensions`]
    /// (default 80×24 for headless). Returns the new session id;
    /// callers refer to the session by id thereafter.
    pub async fn spawn(self: &Arc<Self>, spec: &TermSpec) -> Result<String> {
        let id = format!(
            "mado-session-{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        );
        // ★ Through the one ladder. This branch read only $SHELL and never
        // consulted the configured shell at all, so a headless pane ignored
        // the operator's choice while a GUI pane honoured it -- same machine,
        // same config, two answers.
        let shell = crate::shell_resolve::resolve(Some(spec.shell.as_str()));
        let (cols, rows) = spec.resolved_dimensions();
        let working_directory: Option<PathBuf> = if spec.cwd.is_empty() {
            None
        } else if let Some(stripped) = spec.cwd.strip_prefix("~/") {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(stripped))
        } else if spec.cwd == "~" {
            std::env::var("HOME").ok().map(PathBuf::from)
        } else {
            Some(PathBuf::from(&spec.cwd))
        };

        let pty = Pty::spawn(
            &shell,
            cols,
            rows,
            &spec.env,
            working_directory.as_deref(),
            None,
        )
        .await
        .context("pty spawn")?;

        let mut reader = pty.reader().context("pty reader")?;
        let writer = pty.writer().context("pty writer")?;
        let pty = Arc::new(AsyncMutex::new(pty));
        let writer = Arc::new(AsyncMutex::new(writer));

        let terminal = Arc::new(parking_lot::RwLock::new(Terminal::new(
            cols as usize,
            rows as usize,
        )));
        let terminal_for_pump = Arc::clone(&terminal);
        let writer_for_pump = Arc::clone(&writer);

        let reader_task = tokio::spawn(async move {
            // 64 KiB read buffer (was 8 KiB). High-throughput PTY
            // streams (`cat large.bin`, `journalctl -f` bursts, build
            // logs) saturate the kernel-side pipe; a 64 KiB read
            // drains ~8× more bytes per syscall + per Tokio scheduler
            // wake-up. Matches refterm's read-chunk + Ghostty's
            // io-thread buffer sizing. The cost is 56 KiB extra
            // resident per session, which is negligible at any
            // realistic session count.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        tracing::debug!("pty reader hit EOF");
                        break;
                    }
                    Ok(n) => {
                        // Feed + drain VT query answers under one
                        // lock scope, then write back outside the
                        // terminal lock (parking_lot guards must not
                        // cross an await). Without the write-back,
                        // a headless session never answers DSR/DA/
                        // OSC queries and reedline-based shells
                        // (frost, frostmourne) stall waiting for the
                        // CPR reply — found by the `mado e2e`
                        // frostmourne matrix (E2/E5 class; the GUI
                        // paths route the same answers through
                        // engate_consumer::ResponseWriter).
                        // ★ ONE FUNNEL. Drained through `VtAnswer`, which has
                        // no other constructor, so this writer cannot invent an
                        // answer and cannot emit one it did not drain. Three
                        // independent drainers is what produced the doubled
                        // `^[[31;24R` the operator saw.
                        let response = {
                            let mut term = terminal_for_pump.write();
                            term.feed(&buf[..n]);
                            crate::vt_answer::VtAnswer::drain(&mut term)
                        };
                        if let Some(resp) = response.filter(|a| !a.is_empty()) {
                            let len = resp.len();
                            let mut w = writer_for_pump.lock().await;
                            if let Err(e) = w.write_all(&resp.into_bytes()).await {
                                tracing::warn!(
                                    error = %e,
                                    len,
                                    "VT query response write-back FAILED — shell may stall on an unanswered DSR/DA/OSC query"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "pty reader error");
                        break;
                    }
                }
            }
        });

        let session = Arc::new(Session {
            id: id.clone(),
            title: if spec.title.is_empty() {
                spec.display_title()
            } else {
                spec.title.clone()
            },
            shell,
            created_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            created_at_instant: Instant::now(),
            terminal,
            pty,
            writer,
            _reader_task: reader_task,
        });

        self.inner
            .lock()
            .expect("registry lock poisoned")
            .insert(id.clone(), session);

        Ok(id)
    }

    /// The focused session's working directory, as reported by the
    /// shell via OSC 7 ([`Terminal::cwd`]). "Focused" in the headless
    /// registry is the most recently spawned live session — the same
    /// most-recent-first ordering [`Self::list`] presents (a headless
    /// registry has no window-system focus to consult). `None` when
    /// no session is live or the focused one never emitted OSC 7 —
    /// callers fall back to their spawn default.
    ///
    /// Consumer: `spawn_term`'s `window.inherit_working_directory`
    /// resolution ([`TermSpec::with_inherited_cwd`]).
    #[must_use]
    pub fn focused_cwd(&self) -> Option<String> {
        let guard = self.inner.lock().expect("registry lock poisoned");
        guard
            .values()
            .max_by_key(|s| session_seq(&s.id))
            .and_then(|s| s.terminal.read().cwd().map(str::to_owned))
    }

    /// Fetch a session by id. Returns `None` if no such session.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.inner
            .lock()
            .expect("registry lock poisoned")
            .get(id)
            .cloned()
    }

    /// List every live session — most-recently-spawned first by id.
    /// Each entry is a compact summary; the agent fetches the grid
    /// separately via `snapshot_grid` when needed.
    #[must_use]
    pub fn list(&self) -> Vec<SessionSummary> {
        let guard = self.inner.lock().expect("registry lock poisoned");
        let mut summaries: Vec<SessionSummary> = guard
            .values()
            .map(|s| {
                let term = s.terminal.read();
                SessionSummary {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    shell: s.shell.clone(),
                    cols: term.cols(),
                    rows: term.rows(),
                    created_at_unix_ms: s.created_at_unix_ms,
                    uptime_ms: s.created_at_instant.elapsed().as_millis(),
                }
            })
            .collect();
        // Most-recent-first: order by the shared spawn-sequence helper,
        // descending — identical ordering to `focused_cwd`'s pick.
        summaries.sort_by(|a, b| session_seq(&b.id).cmp(&session_seq(&a.id)));
        summaries
    }

    /// Remove a session and drop its reader task. The session's PTY
    /// drops too — which kills the child if the kernel hasn't reaped
    /// it already. Returns `Ok(true)` if the id matched a live session.
    pub fn close(&self, id: &str) -> Result<bool> {
        let removed = self
            .inner
            .lock()
            .expect("registry lock poisoned")
            .remove(id);
        Ok(removed.is_some())
    }

    /// Convenience: spawn a session, send `input` to it, return its id.
    /// Used in tests and quick agent-driven flows where the full
    /// spawn → write → snapshot loop is one step.
    #[cfg(test)]
    pub async fn spawn_and_input(
        self: &Arc<Self>,
        spec: &TermSpec,
        input: &[u8],
    ) -> Result<String> {
        let id = self.spawn(spec).await?;
        let s = self
            .get(&id)
            .ok_or_else(|| anyhow!("session vanished after spawn"))?;
        s.send_input(input).await?;
        Ok(id)
    }
}

/// Summary row returned by `list_sessions` — every field a debug
/// session needs at a glance. The grid itself is fetched separately
/// to keep list responses small even with many sessions.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub shell: String,
    pub cols: usize,
    pub rows: usize,
    pub created_at_unix_ms: u128,
    pub uptime_ms: u128,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Spawn an `/bin/sh`, echo a sentinel string, snapshot, assert
    /// the sentinel made it through the PTY → vte → terminal chain.
    /// Proves the headless wiring is load-bearing — the rest of the
    /// MCP surface composes from this single primitive.
    #[tokio::test]
    async fn spawn_send_snapshot_roundtrip() {
        let registry = Arc::new(SessionRegistry::default());
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 40,
            rows: 8,
            ..TermSpec::default()
        };
        let id = registry.spawn(&spec).await.unwrap();
        let s = registry.get(&id).unwrap();

        // Disable shell prompt + history so output is deterministic.
        s.send_input(b"PS1=''; echo MADO_SENTINEL\n").await.unwrap();

        // The reader pump runs asynchronously — poll up to 1s for
        // the sentinel to materialise. CI is the worst case here;
        // local runs settle in <50ms.
        let mut found = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let text = s.snapshot_grid().to_text();
            if text.contains("MADO_SENTINEL") {
                found = true;
                break;
            }
        }
        assert!(found, "sentinel did not propagate through PTY → terminal");
    }

    /// REGRESSION GUARD — headless sessions must ANSWER VT queries,
    /// not just parse them. The child raw-modes its tty, emits a DSR
    /// (`ESC[6n`), and blocks a 1-byte read on the reply; the marker
    /// only prints if the reader pump wrote the CPR answer back to
    /// the PTY. Before the write-back landed, reedline shells
    /// (frost / frostmourne) hung at startup under `spawn_term` —
    /// found by the `mado e2e` frostmourne matrix (E2/E5 class).
    #[tokio::test]
    async fn reader_pump_answers_dsr_so_raw_mode_readers_unblock() {
        let registry = Arc::new(SessionRegistry::default());
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 40,
            rows: 8,
            ..TermSpec::default()
        };
        let id = registry.spawn(&spec).await.unwrap();
        let s = registry.get(&id).unwrap();

        // Raw mode so the reply's lack of a newline doesn't wedge the
        // canonical-mode line discipline; dd blocks until ≥1 reply
        // byte arrives. No reply ⇒ no CPR_ROUNDTRIP ⇒ test fails.
        s.send_input(
            b"PS1=''; stty raw -echo; printf '\\033[6n'; dd bs=1 count=1 >/dev/null 2>&1; stty sane; echo CPR_ROUNDTRIP\n",
        )
        .await
        .unwrap();

        let mut found = false;
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let text = s.snapshot_grid().to_text();
            if text.contains("CPR_ROUNDTRIP") {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "DSR reply never reached the child — reader pump is not writing take_response() back to the PTY"
        );
    }

    #[tokio::test]
    async fn list_sessions_returns_most_recent_first() {
        let registry = Arc::new(SessionRegistry::default());
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 20,
            rows: 4,
            ..TermSpec::default()
        };
        let _a = registry.spawn(&spec).await.unwrap();
        let _b = registry.spawn(&spec).await.unwrap();
        let _c = registry.spawn(&spec).await.unwrap();
        let list = registry.list();
        assert_eq!(list.len(), 3);
        // most-recent-first ordering
        let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert!(ids[0].ends_with("-3"));
        assert!(ids[2].ends_with("-1"));
    }

    /// `focused_cwd` reads the OSC-7-reported cwd of the most
    /// recently spawned live session (the registry's notion of
    /// focus). The OSC 7 bytes are fed straight into the session's
    /// Terminal — the same path a real shell's
    /// `printf '\e]7;file://host/dir\a'` takes through the reader
    /// pump, minus the PTY round-trip flakiness.
    #[tokio::test]
    async fn focused_cwd_tracks_most_recent_sessions_osc7() {
        let registry = Arc::new(SessionRegistry::default());
        assert!(
            registry.focused_cwd().is_none(),
            "empty registry has no focused cwd"
        );
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 20,
            rows: 4,
            ..TermSpec::default()
        };
        let a = registry.spawn(&spec).await.unwrap();
        assert!(
            registry.focused_cwd().is_none(),
            "no OSC 7 emitted yet → fall back cleanly"
        );
        registry
            .get(&a)
            .unwrap()
            .terminal_arc()
            .write()
            .feed(b"\x1b]7;file://host/tmp/proj-a\x07");
        assert_eq!(registry.focused_cwd().as_deref(), Some("/tmp/proj-a"));

        // A newer session takes focus — even before it reports a
        // cwd, the OLDER session's cwd must no longer leak through.
        let b = registry.spawn(&spec).await.unwrap();
        assert!(
            registry.focused_cwd().is_none(),
            "focus moved to the new cwd-less session"
        );
        registry
            .get(&b)
            .unwrap()
            .terminal_arc()
            .write()
            .feed(b"\x1b]7;file://host/tmp/proj-b\x07");
        assert_eq!(registry.focused_cwd().as_deref(), Some("/tmp/proj-b"));

        // Closing the focused session falls back to the previous one.
        registry.close(&b).unwrap();
        assert_eq!(registry.focused_cwd().as_deref(), Some("/tmp/proj-a"));
    }

    #[tokio::test]
    async fn snapshot_dimensions_match_spec() {
        let registry = Arc::new(SessionRegistry::default());
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 100,
            rows: 30,
            ..TermSpec::default()
        };
        let id = registry.spawn(&spec).await.unwrap();
        let snap = registry.get(&id).unwrap().snapshot_grid();
        assert_eq!(snap.cols, 100);
        assert_eq!(snap.rows, 30);
        assert_eq!(snap.cells.len(), 30);
        for row in &snap.cells {
            // each row visible_rows yields exactly `cols` cells
            assert_eq!(row.len(), 100);
        }
    }

    #[tokio::test]
    async fn resize_updates_grid() {
        let registry = Arc::new(SessionRegistry::default());
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 80,
            rows: 24,
            ..TermSpec::default()
        };
        let id = registry.spawn(&spec).await.unwrap();
        let s = registry.get(&id).unwrap();
        s.resize(132, 40).await.unwrap();
        let snap = s.snapshot_grid();
        assert_eq!(snap.cols, 132);
        assert_eq!(snap.rows, 40);
    }

    #[tokio::test]
    async fn close_removes_session() {
        let registry = Arc::new(SessionRegistry::default());
        let spec = TermSpec {
            shell: "/bin/sh".into(),
            cols: 20,
            rows: 4,
            ..TermSpec::default()
        };
        let id = registry.spawn(&spec).await.unwrap();
        assert!(registry.get(&id).is_some());
        let closed = registry.close(&id).unwrap();
        assert!(closed);
        assert!(registry.get(&id).is_none());
    }

    /// `CellSnapshot::legacy` must DERIVE `underline` from bit 2 of the
    /// legacy u8, never hardcode `"none"`.
    ///
    /// The regression this pins is a diagnostic that contradicted
    /// itself: a live tear pane had `CellAttrs::UNDERLINE` latched on
    /// every cell (tear's `CSI > 4 ; 2 m` leak), and `snapshot_grid` —
    /// the tool for triaging exactly that — reported `attrs: 4` beside
    /// `underline: "none"` for all of them. A snapshot field may be
    /// lossy; it may not disagree with the bitfield it is projected
    /// from. Asserted BOTH directions so hardcoding either constant
    /// fails.
    #[test]
    fn legacy_derives_underline_from_the_attrs_bit() {
        let underline_bit = crate::terminal::CellAttrs::UNDERLINE.bits();
        let plain = CellSnapshot::legacy('x', 1, [255; 3], [0; 3], 0);
        assert_eq!(plain.underline, "none", "no bit set must report none");

        let underlined = CellSnapshot::legacy('x', 1, [255; 3], [0; 3], underline_bit);
        assert_ne!(
            underlined.underline, "none",
            "attrs carries UNDERLINE ({underline_bit:#04b}) — the underline \
             field must not report none; that contradiction is the bug"
        );

        // The bit must be read positionally, not "any non-zero attrs".
        // BOLD alone shares the field and must NOT read as underlined.
        let bold = CellSnapshot::legacy(
            'x',
            1,
            [255; 3],
            [0; 3],
            crate::terminal::CellAttrs::BOLD.bits(),
        );
        assert_eq!(bold.underline, "none", "BOLD must not read as underline");
    }

    #[test]
    fn pretty_marks_cursor_and_background() {
        let snap = GridSnapshot {
            cols: 4,
            rows: 2,
            cursor_row: 0,
            cursor_col: 1,
            cursor_visible: true,
            cells: vec![
                vec![
                    CellSnapshot::legacy('a', 1, [255, 255, 255], [0, 0, 0], 0),
                    CellSnapshot::legacy('b', 1, [255, 255, 255], [0, 0, 0], 0),
                    CellSnapshot::legacy(' ', 1, [255, 255, 255], [0, 0, 0], 0),
                    CellSnapshot::legacy(' ', 1, [255, 255, 255], [0, 0, 0], 0),
                ],
                vec![
                    CellSnapshot::legacy('c', 1, [255, 255, 255], [0, 0, 0], 0),
                    CellSnapshot::legacy(' ', 1, [255, 255, 255], [0, 0, 0], 0),
                    CellSnapshot::legacy(' ', 1, [255, 255, 255], [0, 0, 0], 0),
                    CellSnapshot::legacy(' ', 1, [255, 255, 255], [0, 0, 0], 0),
                ],
            ],
        };
        let pretty = snap.to_pretty();
        // cursor at (0,1) marked
        assert!(pretty.contains("a▓"));
        // empty cells become ·
        assert!(pretty.contains("c···"));
    }

    #[test]
    fn to_text_strips_trailing_whitespace_per_row() {
        let snap = GridSnapshot {
            cols: 6,
            rows: 2,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            cells: vec![
                vec![
                    CellSnapshot::legacy('h', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy('i', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                ],
                vec![
                    CellSnapshot::legacy('b', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy('y', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy('e', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                    CellSnapshot::legacy(' ', 1, [0; 3], [0; 3], 0),
                ],
            ],
        };
        let text = snap.to_text();
        assert_eq!(text, "hi\nbye");
    }
}
