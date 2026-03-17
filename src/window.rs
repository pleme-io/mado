//! Window state — manages tabs, panes, and per-pane Terminal+PTY.
//!
//! Each tab owns an independent pane tree with its own terminals.
//! Each pane has its own Terminal instance and PTY process.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::pane::{PaneId, PaneManager, PaneRect, SplitDir};
use crate::render::SharedTerminal;
use crate::search::SearchState;
use crate::selection::Selection;
use crate::tab::{TabId, TabManager};
use crate::terminal::{Color, Terminal};

/// Per-pane state: terminal + PTY I/O channels.
pub struct PaneTerminal {
    pub terminal: SharedTerminal,
    pub input_tx: UnboundedSender<Vec<u8>>,
    pub selection: Arc<Mutex<Selection>>,
    pub search: Arc<Mutex<SearchState>>,
    exited: Arc<AtomicBool>,
}

impl PaneTerminal {
    /// Check if the PTY has exited.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

/// Per-tab state: pane tree + terminal map.
struct TabState {
    panes: PaneManager,
    terminals: HashMap<PaneId, PaneTerminal>,
    resize_senders: HashMap<PaneId, UnboundedSender<(u16, u16)>>,
}

/// Manages the full window state: tabs, panes, and per-pane terminals.
pub struct WindowState {
    tabs: TabManager,
    tab_states: HashMap<TabId, TabState>,
    shell: String,
    scrollback: usize,
    default_cols: usize,
    default_rows: usize,
    /// Theme colors applied to new terminals (None = use defaults).
    theme_colors: Option<(Color, Color, [Color; 16])>,
}

impl WindowState {
    /// Create a new window state with a single tab and pane.
    pub fn new(
        shell: String,
        cols: usize,
        rows: usize,
        scrollback: usize,
    ) -> Self {
        let tabs = TabManager::new();
        let active_tab_id = tabs.active_tab().id;

        let mut tab_state = TabState {
            panes: PaneManager::new(),
            terminals: HashMap::new(),
            resize_senders: HashMap::new(),
        };

        let initial_id = PaneId(0);
        Self::spawn_pane_for_tab(&mut tab_state, initial_id, cols, rows, &shell, scrollback, None);

        let mut tab_states = HashMap::new();
        tab_states.insert(active_tab_id, tab_state);

        Self {
            tabs,
            tab_states,
            shell,
            scrollback,
            default_cols: cols,
            default_rows: rows,
            theme_colors: None,
        }
    }

    /// Store theme colors so new terminals get them automatically.
    pub fn set_theme(&mut self, fg: Color, bg: Color, ansi: [Color; 16]) {
        self.theme_colors = Some((fg, bg, ansi));
    }

    /// Get the focused pane's terminal state.
    pub fn focused_pane(&self) -> Option<&PaneTerminal> {
        let tab = self.active_tab_state()?;
        tab.terminals.get(&tab.panes.focused())
    }

    /// Get a pane's terminal by ID (in the active tab).
    pub fn pane(&self, id: &PaneId) -> Option<&PaneTerminal> {
        let tab = self.active_tab_state()?;
        tab.terminals.get(id)
    }

    /// Get the focused pane ID.
    pub fn focused_pane_id(&self) -> PaneId {
        self.active_tab_state()
            .map_or(PaneId(0), |tab| tab.panes.focused())
    }

    /// Get all visible pane rects for the current tab's layout.
    pub fn layout(&self, x: f32, y: f32, width: f32, height: f32) -> Vec<PaneRect> {
        self.active_tab_state()
            .map_or_else(Vec::new, |tab| tab.panes.layout(x, y, width, height))
    }

    /// Send input to the focused pane's PTY.
    pub fn send_input(&self, data: Vec<u8>) {
        if let Some(pt) = self.focused_pane() {
            let _ = pt.input_tx.send(data);
        }
    }

    /// Split the focused pane. Spawns a new PTY for the new pane.
    pub fn split(&mut self, dir: SplitDir, cols: usize, rows: usize) -> PaneId {
        let tab_id = self.tabs.active_tab().id;
        let shell = self.shell.clone();
        let scrollback = self.scrollback;
        let theme = self.theme_colors;
        let tab = self.tab_states.get_mut(&tab_id).expect("active tab");
        let new_id = tab.panes.split(dir);
        Self::spawn_pane_for_tab(tab, new_id, cols, rows, &shell, scrollback, theme);
        new_id
    }

    /// Close the focused pane. Drops its terminal and PTY.
    pub fn close_focused_pane(&mut self) -> Option<PaneId> {
        let tab_id = self.tabs.active_tab().id;
        let tab = self.tab_states.get_mut(&tab_id)?;
        let closed = tab.panes.close_focused()?;
        tab.terminals.remove(&closed);
        tab.resize_senders.remove(&closed);
        Some(closed)
    }

    /// Focus the next pane.
    pub fn focus_next(&mut self) {
        let tab_id = self.tabs.active_tab().id;
        if let Some(tab) = self.tab_states.get_mut(&tab_id) {
            tab.panes.focus_next();
        }
    }

    /// Focus the previous pane.
    pub fn focus_prev(&mut self) {
        let tab_id = self.tabs.active_tab().id;
        if let Some(tab) = self.tab_states.get_mut(&tab_id) {
            tab.panes.focus_prev();
        }
    }

    /// Resize all panes' terminals in the active tab to match their viewport.
    pub fn resize_panes(&self, width: f32, height: f32, padding: f32, cell_w: f32, cell_h: f32) {
        let Some(tab) = self.active_tab_state() else {
            return;
        };
        let rects = tab.panes.layout(padding, padding, width - 2.0 * padding, height - 2.0 * padding);
        for rect in &rects {
            let pane_cols = ((rect.width) / cell_w) as usize;
            let pane_rows = ((rect.height) / cell_h) as usize;
            let pane_cols = pane_cols.max(10);
            let pane_rows = pane_rows.max(3);

            if let Some(pt) = tab.terminals.get(&rect.id) {
                let mut term = pt.terminal.lock().unwrap();
                term.resize(pane_cols, pane_rows);
                drop(term);
            }
            if let Some(tx) = tab.resize_senders.get(&rect.id) {
                let _ = tx.send((pane_cols as u16, pane_rows as u16));
            }
        }
    }

    /// Check if any pane has exited. Returns true if the last pane in the last tab exits.
    pub fn any_exited(&self) -> bool {
        let Some(tab) = self.active_tab_state() else {
            return true;
        };
        if self.tabs.count() == 1 && tab.terminals.len() == 1 {
            return tab.terminals.values().next().is_some_and(|pt| pt.has_exited());
        }
        false
    }

    /// Get all pane IDs in the active tab.
    #[allow(dead_code)]
    pub fn all_pane_ids(&self) -> Vec<PaneId> {
        self.active_tab_state()
            .map_or_else(Vec::new, |tab| tab.panes.all_ids())
    }

    /// Iterate over all pane terminals across all tabs.
    pub fn all_panes(&self) -> impl Iterator<Item = &PaneTerminal> {
        self.tab_states.values().flat_map(|tab| tab.terminals.values())
    }

    // ── Tab operations ──────────────────────────────────────────────

    /// Create a new tab with a single pane. Returns the new tab's ID.
    pub fn new_tab(&mut self) -> TabId {
        let tab_id = self.tabs.add();
        let cols = self.default_cols;
        let rows = self.default_rows;
        let shell = self.shell.clone();
        let scrollback = self.scrollback;

        let mut tab_state = TabState {
            panes: PaneManager::new(),
            terminals: HashMap::new(),
            resize_senders: HashMap::new(),
        };
        let initial_id = PaneId(0);
        let theme = self.theme_colors;
        Self::spawn_pane_for_tab(&mut tab_state, initial_id, cols, rows, &shell, scrollback, theme);
        self.tab_states.insert(tab_id, tab_state);

        tracing::info!(tab = tab_id.0, "new tab created");
        tab_id
    }

    /// Close the active tab. Returns the closed tab's ID, or None if it's the last tab.
    pub fn close_tab(&mut self) -> Option<TabId> {
        let closed = self.tabs.close_active()?;
        self.tab_states.remove(&closed);
        tracing::info!(tab = closed.0, "tab closed");
        Some(closed)
    }

    /// Switch to the next tab.
    pub fn next_tab(&mut self) {
        self.tabs.next();
    }

    /// Switch to the previous tab.
    pub fn prev_tab(&mut self) {
        self.tabs.prev();
    }

    /// Number of tabs.
    #[allow(dead_code)]
    pub fn tab_count(&self) -> usize {
        self.tabs.count()
    }

    /// Active tab index (for rendering tab bar).
    #[allow(dead_code)]
    pub fn active_tab_index(&self) -> usize {
        self.tabs.active_index()
    }

    /// All tab titles (for rendering tab bar).
    #[allow(dead_code)]
    pub fn tab_titles(&self) -> Vec<&str> {
        self.tabs.tabs().iter().map(|t| t.title.as_str()).collect()
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn active_tab_state(&self) -> Option<&TabState> {
        let tab_id = self.tabs.active_tab().id;
        self.tab_states.get(&tab_id)
    }

    /// Spawn a PTY for a pane within a tab.
    fn spawn_pane_for_tab(
        tab: &mut TabState,
        pane_id: PaneId,
        cols: usize,
        rows: usize,
        shell: &str,
        scrollback: usize,
        theme_colors: Option<(Color, Color, [Color; 16])>,
    ) {
        let mut term = Terminal::with_scrollback(cols, rows, scrollback);
        if let Some((fg, bg, ansi)) = theme_colors {
            term.apply_theme(fg, bg, ansi);
        }
        let terminal: SharedTerminal = Arc::new(Mutex::new(term));
        let terminal_for_pty = Arc::clone(&terminal);

        let pty_exited = Arc::new(AtomicBool::new(false));
        let pty_exited_writer = Arc::clone(&pty_exited);

        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        // Terminal responses (DSR/DA) go back to this pane's PTY stdin
        let response_tx = input_tx.clone();

        let shell = shell.to_string();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create tokio runtime");

            rt.block_on(async move {
                let pty = match crate::pty::Pty::spawn(&shell, cols as u16, rows as u16).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("failed to spawn PTY for pane {}: {e}", pane_id.0);
                        pty_exited_writer.store(true, Ordering::Release);
                        return;
                    }
                };

                let mut reader = match pty.reader() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("failed to create PTY reader: {e}");
                        pty_exited_writer.store(true, Ordering::Release);
                        return;
                    }
                };

                let mut writer = match pty.writer() {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::error!("failed to create PTY writer: {e}");
                        pty_exited_writer.store(true, Ordering::Release);
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
                let mut buf = [0u8; 65536];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut term = terminal_for_pty.lock().unwrap();
                            term.feed(&buf[..n]);
                            if let Some(response) = term.take_response() {
                                drop(term);
                                let _ = response_tx.send(response);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("PTY read error: {e}");
                            break;
                        }
                    }
                }

                writer_task.abort();
                resize_task.abort();
                pty_exited_writer.store(true, Ordering::Release);
            });
        });

        let pt = PaneTerminal {
            terminal,
            input_tx,
            selection: Arc::new(Mutex::new(Selection::new())),
            search: Arc::new(Mutex::new(SearchState::new())),
            exited: pty_exited,
        };

        tab.terminals.insert(pane_id, pt);
        tab.resize_senders.insert(pane_id, resize_tx);
    }
}
