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

use anyhow::{Context, Result};
#[cfg(test)]
use anyhow::anyhow;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::pty::Pty;
use crate::term_spec::TermSpec;
use crate::terminal::{Cell, Terminal};

/// Default shell when neither the spec nor `$SHELL` is set. `/bin/sh`
/// is the POSIX guarantee — present on every system mado runs on.
const DEFAULT_SHELL: &str = "/bin/sh";

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

    /// Take a complete snapshot of the terminal grid + cursor +
    /// metadata. This is the load-bearing introspection primitive —
    /// every visual-rendering question can be answered by diffing
    /// this against the rendered output.
    pub fn snapshot_grid(&self) -> GridSnapshot {
        let term = self.terminal.read();
        let cols = term.cols();
        let rows = term.rows();
        let cursor = *term.cursor();
        let cells: Vec<Vec<CellSnapshot>> = term
            .visible_rows()
            .map(|row| {
                row.iter()
                    .take(cols)
                    .map(CellSnapshot::from_cell)
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
    /// CellAttrs bitfield as u8 — bold/italic/underline/inverse/dim/
    /// strikethrough/hidden/blink, in the same bit positions the
    /// renderer reads from.
    pub attrs: u8,
}

impl CellSnapshot {
    fn from_cell(c: &Cell) -> Self {
        Self {
            ch: c.ch,
            width: c.width,
            fg: [c.fg.r, c.fg.g, c.fg.b],
            bg: [c.bg.r, c.bg.g, c.bg.b],
            attrs: c.attrs.bits(),
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
        out.push_str(&format!(
            "grid {}×{}  cursor=({},{}) visible={}\n",
            self.cols, self.rows, self.cursor_row, self.cursor_col, self.cursor_visible
        ));
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
        let id = format!("mado-session-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let shell = if spec.shell.is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| DEFAULT_SHELL.to_string())
        } else {
            spec.shell.clone()
        };
        let (cols, rows) = spec.resolved_dimensions();
        let working_directory: Option<PathBuf> = if spec.cwd.is_empty() {
            None
        } else if let Some(stripped) = spec.cwd.strip_prefix("~/") {
            std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(stripped))
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

        let terminal = Arc::new(parking_lot::RwLock::new(Terminal::new(cols as usize, rows as usize)));
        let terminal_for_pump = Arc::clone(&terminal);

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
                        let mut term = terminal_for_pump.write();
                        term.feed(&buf[..n]);
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
        // Most-recent-first: parse the numeric suffix and sort descending.
        summaries.sort_by(|a, b| {
            let an = a.id.rsplit('-').next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let bn = b.id.rsplit('-').next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            bn.cmp(&an)
        });
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
    pub async fn spawn_and_input(self: &Arc<Self>, spec: &TermSpec, input: &[u8]) -> Result<String> {
        let id = self.spawn(spec).await?;
        let s = self.get(&id).ok_or_else(|| anyhow!("session vanished after spawn"))?;
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
                    CellSnapshot { ch: 'a', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                    CellSnapshot { ch: 'b', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                ],
                vec![
                    CellSnapshot { ch: 'c', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [255, 255, 255], bg: [0, 0, 0], attrs: 0 },
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
                    CellSnapshot { ch: 'h', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: 'i', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                ],
                vec![
                    CellSnapshot { ch: 'b', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: 'y', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: 'e', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                    CellSnapshot { ch: ' ', width: 1, fg: [0; 3], bg: [0; 3], attrs: 0 },
                ],
            ],
        };
        let text = snap.to_text();
        assert_eq!(text, "hi\nbye");
    }

}
