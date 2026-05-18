//! Single-pane terminal orchestration — the post-Phase-4 shape.
//!
//! Replaces the `WindowState` / `PaneManager` / `TabManager`
//! triple. Mado now runs exactly one terminal with one PTY; if a
//! user wants splits / tabs they run `tear` (see
//! `theory/MADO-TEAR-M5.md`).
//!
//! Lifts `PaneTerminal` + the PTY spawn / reader / writer / resize
//! orchestration out of `window.rs` so the legacy modules can be
//! deleted. The reader thread + writer task + resize task design
//! is unchanged from the original WindowState implementation —
//! only the wrapping (single pane, not a tab/pane tree) differs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::sync::Mutex;
use std::os::fd::RawFd;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::render::SharedTerminal;
use crate::search::SearchState;
use crate::selection::Selection;
use crate::terminal::{Color, Terminal};

/// Single live terminal + the channels that drive its PTY. All
/// fields are public for the main loop to access directly —
/// this is the replacement for `Arc<Mutex<WindowState>>`.
pub struct SinglePane {
    pub terminal: SharedTerminal,
    pub input_tx: UnboundedSender<Vec<u8>>,
    pub resize_tx: UnboundedSender<(u16, u16)>,
    pub selection: Arc<Mutex<Selection>>,
    pub search: Arc<Mutex<SearchState>>,
    exited: Arc<AtomicBool>,
}

impl SinglePane {
    /// Has the PTY's child process exited?
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Send raw bytes (already encoded keystrokes / responses)
    /// to the PTY. Non-blocking — drops the bytes if the writer
    /// task has already exited.
    pub fn send_input(&self, data: Vec<u8>) {
        let _ = self.input_tx.send(data);
    }

    /// Resize the PTY's winsize. Triggers SIGWINCH at the child.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize_tx.send((cols, rows));
    }

    // ── WindowState-compat shims for the Phase-4.3 main.rs refactor ─
    //
    // Mado's keybind handler was authored against
    // `WindowState::focused_pane() -> Option<&PaneTerminal>` etc.
    // SinglePane is structurally the same as the old PaneTerminal
    // (same field names + types), so a tiny compat layer lets the
    // 30+ callsites keep their shape unchanged. When tear absorbs
    // the rest of mado's terminal logic (Phase 5+) these shims go.

    /// Single pane — always present. Returns `Some(self)`.
    pub fn focused_pane(&self) -> Option<&Self> {
        Some(self)
    }

    /// Compat alias for WindowState's `any_exited`.
    pub fn any_exited(&self) -> bool {
        self.has_exited()
    }

    /// Compat alias for WindowState's iterator API.
    pub fn all_panes(&self) -> impl Iterator<Item = &Self> {
        std::iter::once(self)
    }

    /// Compat: WindowState took pixel dims + cell metrics and
    /// resized every pane. Single-pane mado computes cells once
    /// from the same inputs + forwards to `resize`.
    pub fn resize_panes(
        &self,
        width: f32,
        height: f32,
        padding: f32,
        cell_w: f32,
        cell_h: f32,
    ) {
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        let cols = (((width - 2.0 * padding) / cell_w) as u16).max(1);
        let rows = (((height - 2.0 * padding) / cell_h) as u16).max(1);
        self.resize(cols, rows);
        // Resize the terminal grid too — without this, the renderer
        // would read stale cell counts.
        self.terminal.write().resize(cols as usize, rows as usize);
    }
}

/// Spawn the one pane mado renders. Same orchestration shape
/// the deleted `WindowState::spawn_pane_for_tab` had — terminal
/// allocation + reader thread + writer task + resize task —
/// minus the TabState / PaneManager bookkeeping.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    shell: String,
    cols: usize,
    rows: usize,
    scrollback: usize,
    theme_colors: Option<(Color, Color, [Color; 16])>,
    extra_env: HashMap<String, String>,
    working_directory: Option<std::path::PathBuf>,
    initial_command: Option<String>,
) -> SinglePane {
    let mut term = Terminal::with_scrollback(cols, rows, scrollback);
    if let Some((fg, bg, ansi)) = theme_colors {
        term.apply_theme(fg, bg, ansi);
    }
    let terminal: SharedTerminal = Arc::new(parking_lot::RwLock::new(term));
    let terminal_for_pty = Arc::clone(&terminal);

    let exited = Arc::new(AtomicBool::new(false));
    let exited_writer = Arc::clone(&exited);

    let (input_tx, mut input_rx) = unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = unbounded_channel::<(u16, u16)>();
    let response_tx = input_tx.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");

        rt.block_on(async move {
            let pty = match crate::pty::Pty::spawn(
                &shell,
                cols as u16,
                rows as u16,
                &extra_env,
                working_directory.as_deref(),
                initial_command.as_deref(),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("failed to spawn PTY: {e}");
                    exited_writer.store(true, Ordering::Release);
                    return;
                }
            };

            let mut reader = match pty.reader() {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("failed to create PTY reader: {e}");
                    exited_writer.store(true, Ordering::Release);
                    return;
                }
            };
            let mut writer = match pty.writer() {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!("failed to create PTY writer: {e}");
                    exited_writer.store(true, Ordering::Release);
                    return;
                }
            };
            let master_raw: RawFd = pty.master_raw_fd();

            // Writer task
            let writer_task = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(data) = input_rx.recv().await {
                    if let Err(e) = writer.write_all(&data).await {
                        tracing::warn!("PTY write error: {e}");
                        break;
                    }
                }
            });

            // Resize task
            let resize_task = tokio::spawn(async move {
                while let Some((cols, rows)) = resize_rx.recv().await {
                    let ws = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        libc::ioctl(master_raw, libc::TIOCSWINSZ, &ws);
                    }
                }
            });

            // Reader loop
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut t = terminal_for_pty.write();
                        t.feed(&buf[..n]);
                        if let Some(response) = t.take_response() {
                            drop(t);
                            let _ = response_tx.send(response);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("PTY read error: {e}");
                        break;
                    }
                }
            }
            // PTY closed — signal the renderer to exit.
            exited_writer.store(true, Ordering::Release);
            writer_task.abort();
            resize_task.abort();
        });
    });

    SinglePane {
        terminal,
        input_tx,
        resize_tx,
        selection: Arc::new(Mutex::new(Selection::new())),
        search: Arc::new(Mutex::new(SearchState::new())),
        exited,
    }
}
