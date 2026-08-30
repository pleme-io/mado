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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use std::os::fd::RawFd;
use std::sync::Mutex;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::render::SharedTerminal;
use crate::search::SearchState;
use crate::selection::Selection;
use crate::terminal::{Color, Terminal};

/// Single live terminal + the channels that drive its PTY. All
/// fields are public for the main loop to access directly —
/// this is the replacement for `Arc<Mutex<WindowState>>`.
/// What is being written to the PTY, and by whom.
///
/// ── ★ WHY THE CHANNEL IS TYPED ──────────────────────────────────────────────
///
/// Operator keystrokes and VT query ANSWERS used to share one
/// `UnboundedSender<Vec<u8>>` (`response_tx = input_tx.clone()`), so the writer
/// could not tell them apart and wrote both unconditionally. They are not the
/// same thing and they have opposite preconditions:
///
/// * a keystroke is always wanted — the operator typed it;
/// * an ANSWER is wanted only while the asker is still reading. Delivered late,
///   it is not input at all: the line discipline paints it, and the operator
///   sees `^[[1;29R` in his shell. Measured on plo, where the slave's termios
///   carries `echoctl`, which is exactly what renders ESC as the two printable
///   characters `^[`.
///
/// The distinction has to be in the TYPE, because the byte strings are
/// indistinguishable — an answer is just bytes, and so is a keypress.
#[derive(Debug, Clone)]
pub enum PtyWrite {
    /// The operator typed this. Always written.
    Keys(Vec<u8>),
    /// mado's VT engine produced this in reply to a query (DSR/DA/OSC).
    /// Written only while the asker can still consume it — see the ECHO gate
    /// in the writer task.
    VtAnswer(Vec<u8>),
}

pub struct SinglePane {
    pub terminal: SharedTerminal,
    pub input_tx: UnboundedSender<PtyWrite>,
    pub resize_tx: UnboundedSender<(u16, u16)>,
    pub selection: Arc<Mutex<Selection>>,
    pub search: Arc<Mutex<SearchState>>,
    /// Reader-only directory-frecency overlay state (轍). Shared into the
    /// renderer via `TerminalRenderer::set_dir_picker`, driven by the input
    /// handler. Mirrors `search`.
    pub dir_picker: Arc<Mutex<crate::dir_picker::DirPickerState>>,
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
        let _ = self.input_tx.send(PtyWrite::Keys(data));
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

    /// Compat: WindowState took pixel dims + cell metrics and
    /// resized every pane. Single-pane mado computes cells once
    /// from the same inputs + forwards to `resize`.
    ///
    /// Superseded in the event loop by `ux::InputEngine`'s grid
    /// reconciler (M1) — retained for the WindowState-compat unit
    /// tests below until tear absorbs SinglePane (Phase 5+).
    #[allow(dead_code)]
    pub fn resize_panes(&self, width: f32, height: f32, padding: f32, cell_w: f32, cell_h: f32) {
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
    reflow_on_resize: bool,
    theme_colors: Option<(Color, Color, [Color; 16])>,
    extra_env: HashMap<String, String>,
    working_directory: Option<std::path::PathBuf>,
    initial_command: Option<String>,
) -> SinglePane {
    let mut term = Terminal::with_scrollback(cols, rows, scrollback);
    term.set_reflow_on_resize(reflow_on_resize);
    if let Some((fg, bg, ansi)) = theme_colors {
        term.apply_theme(fg, bg, ansi);
    }
    let terminal: SharedTerminal = Arc::new(parking_lot::RwLock::new(term));
    let terminal_for_pty = Arc::clone(&terminal);

    let exited = Arc::new(AtomicBool::new(false));
    let exited_writer = Arc::clone(&exited);

    let (input_tx, mut input_rx) = unbounded_channel::<PtyWrite>();
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
                while let Some(item) = input_rx.recv().await {
                    let data = match item {
                        PtyWrite::Keys(d) => d,
                        // ── ★ THE ECHO GATE ─────────────────────────────────
                        //
                        // A shell that asked `ESC[6n` is in RAW mode — it has
                        // to be, or it could not read the answer byte by byte.
                        // So an answer arriving while the slave has ECHO on is
                        // arriving after the asker stopped reading, and writing
                        // it does not deliver anything: the line discipline
                        // paints it, and the operator sees `^[[1;29R` in his
                        // prompt.
                        //
                        // This is the precondition the writer never had. It was
                        // not a latency problem — measured, a 300 ms delayed
                        // answer produces ZERO echoes while a surplus one
                        // desyncs the shell permanently — it is a liveness
                        // problem about the READER.
                        //
                        // Reading termios from the MASTER reports the pair's
                        // flags (verified on plo: raw slave -> master shows
                        // ECHO false), so mado can answer this without holding
                        // the slave.
                        PtyWrite::VtAnswer(d) => {
                            if slave_is_echoing(master_raw) {
                                tracing::debug!(
                                    len = d.len(),
                                    "VT answer SUPPRESSED — slave has ECHO on, so \
                                     nobody is reading it and the line discipline \
                                     would paint it as text"
                                );
                                continue;
                            }
                            d
                        }
                    };
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
                            let _ = response_tx.send(PtyWrite::VtAnswer(response));
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
        dir_picker: Arc::new(Mutex::new(crate::dir_picker::DirPickerState::new())),
        exited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a SinglePane without a PTY — for shim unit tests.
    /// The input_tx / resize_tx receivers are dropped so any sends
    /// silently fail (matches the post-PTY-exit shape exactly).
    fn for_test(cols: usize, rows: usize) -> SinglePane {
        let terminal: SharedTerminal = Arc::new(parking_lot::RwLock::new(
            Terminal::with_scrollback(cols, rows, 100),
        ));
        let (input_tx, _input_rx) = unbounded_channel::<PtyWrite>();
        let (resize_tx, _resize_rx) = unbounded_channel::<(u16, u16)>();
        // Drop the receivers so subsequent sends return Err — exactly
        // what happens after a real PTY's writer / resize task exits.
        SinglePane {
            terminal,
            input_tx,
            resize_tx,
            selection: Arc::new(Mutex::new(Selection::new())),
            search: Arc::new(Mutex::new(SearchState::new())),
            dir_picker: Arc::new(Mutex::new(crate::dir_picker::DirPickerState::new())),
            exited: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn focused_pane_always_returns_some_self() {
        let p = for_test(80, 24);
        let f = p.focused_pane();
        assert!(f.is_some());
        // Same pointer — focused_pane returns &self.
        let f_ref = f.unwrap();
        assert!(std::ptr::eq(f_ref, &p));
    }

    #[test]
    fn any_exited_mirrors_has_exited() {
        let p = for_test(80, 24);
        assert!(!p.has_exited());
        assert!(!p.any_exited());
        p.exited.store(true, Ordering::Release);
        assert!(p.has_exited());
        assert!(p.any_exited());
    }

    #[test]
    fn send_input_is_no_op_after_receiver_dropped() {
        // for_test drops the receiver so any send errors. The shim
        // swallows the error — must not panic.
        let p = for_test(80, 24);
        p.send_input(b"keystroke".to_vec());
        p.send_input(b"another".to_vec());
        // No assertion needed — the test passes if it doesn't panic.
    }

    #[test]
    fn resize_is_no_op_after_receiver_dropped() {
        let p = for_test(80, 24);
        p.resize(120, 40);
        p.resize(0, 0);
        // No assertion needed — test passes if no panic.
    }

    #[test]
    fn resize_panes_converts_pixels_to_cells() {
        // width 800, height 600, padding 10, cell 8x16
        //   cols = (800 - 20) / 8 = 97
        //   rows = (600 - 20) / 16 = 36
        let p = for_test(80, 24);
        p.resize_panes(800.0, 600.0, 10.0, 8.0, 16.0);
        // Verify the terminal grid was resized:
        let t = p.terminal.read();
        assert_eq!(t.cols(), 97);
        assert_eq!(t.rows(), 36);
    }

    #[test]
    fn resize_panes_with_invalid_cell_metrics_is_noop() {
        // Zero or negative cell metrics shouldn't crash the
        // terminal — happens during HiDPI initialisation race
        // before the renderer measures actual cell metrics.
        let p = for_test(80, 24);
        let original_cols = p.terminal.read().cols();
        let original_rows = p.terminal.read().rows();
        p.resize_panes(800.0, 600.0, 10.0, 0.0, 16.0);
        assert_eq!(p.terminal.read().cols(), original_cols);
        assert_eq!(p.terminal.read().rows(), original_rows);
        p.resize_panes(800.0, 600.0, 10.0, 8.0, -1.0);
        assert_eq!(p.terminal.read().cols(), original_cols);
        assert_eq!(p.terminal.read().rows(), original_rows);
    }

    #[test]
    fn resize_panes_clamps_to_min_one() {
        // Width 5, padding 10 → (5 - 20) / 8 = negative → clamp to 1.
        let p = for_test(80, 24);
        p.resize_panes(5.0, 5.0, 10.0, 8.0, 16.0);
        // saturating math: cell_w_logical = 8, width - 2*pad = -15;
        // `as u16` of negative f32 saturates to 0 on Rust, then max(1).
        let t = p.terminal.read();
        assert!(t.cols() >= 1);
        assert!(t.rows() >= 1);
    }
}

/// Is the slave in a mode where the LINE DISCIPLINE would echo what we write?
///
/// Reads termios from the MASTER fd, which reports the pair's flags — verified
/// on plo: a slave put into raw mode makes the master report `ECHO: False`.
/// mado holds the master, so this needs no access to the slave.
///
/// Returns `false` when the ioctl fails. That is deliberate and it is the
/// permissive direction: if we cannot tell, we WRITE. An unanswered DSR kills a
/// reedline shell outright (a fatal CPR timeout), while a surplus answer is a
/// cosmetic echo — so the failure mode of not knowing must be the recoverable
/// one.
#[must_use]
fn slave_is_echoing(master: std::os::unix::io::RawFd) -> bool {
    // SAFETY: `termios` is a plain C struct that `tcgetattr` fully initialises
    // on success; the value is only read when the call returns 0.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(master, &raw mut t) != 0 {
            return false;
        }
        t.c_lflag & libc::ECHO != 0
    }
}

#[cfg(test)]
mod echo_gate_tests {
    use super::slave_is_echoing;

    /// ★ THE GATE'S PREDICATE, against real termios on a real pty pair.
    ///
    /// The whole fix rests on one measured claim: reading termios from the
    /// MASTER reports the *pair's* flags, so mado — which holds only the master
    /// — can tell whether the slave would echo. If that were false the gate
    /// would be reading its own fd's defaults and suppressing (or writing)
    /// arbitrarily.
    ///
    /// Both directions are asserted. A one-directional test would pass against
    /// a function that always returned `true`.
    #[test]
    fn a_raw_slave_reports_not_echoing_and_a_cooked_one_reports_echoing() {
        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;
        // ── ★ RETRY: THIS SUITE EXHAUSTS THE PTY POOL ────────────────────────
        //
        // `openpty` returned -1 here on its second run, having succeeded on the
        // first. mado's test suite spawns shells and ptys in parallel, and under
        // that load the pool runs dry. Measured the same day: `session::tests::*`
        // fail 7/7 on a CLEAN tree, and `mcp_spawn_send_snapshot_end_to_end` and
        // `single_writer::second_acquire_on_the_same_dir_loses` are both flaky —
        // all of them pty/shell-spawning tests. One cause, several symptoms.
        //
        // Retrying is honest here because pty availability is not the property
        // under test; the gate's predicate is. What is NOT done is skipping
        // silently on exhaustion — a test that quietly passes when it could not
        // run is the vacuous-guard shape this whole session has been removing,
        // so the failure below still fails, and names the real cause.
        let mut rc = -1;
        for _ in 0..10 {
            // SAFETY: openpty fills both fds on success; the return is checked.
            rc = unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            rc,
            0,
            "openpty failed 10x, errno={}",
            std::io::Error::last_os_error()
        );

        // A fresh pty is cooked: the line discipline WOULD paint what we write.
        assert!(
            slave_is_echoing(master),
            "a default pty must report echoing — if this is false the gate \
             suppresses answers a shell is genuinely waiting for, which kills \
             reedline outright"
        );

        // Put the slave in raw mode, as a shell issuing ESC[6n must be.
        // SAFETY: `t` is fully initialised by tcgetattr before use.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave, &raw mut t), 0);
            libc::cfmakeraw(&raw mut t);
            assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &raw const t), 0);
        }

        assert!(
            !slave_is_echoing(master),
            "a RAW slave must report not-echoing — this is the case where an \
             answer is genuinely wanted, and suppressing it would be the \
             regression"
        );

        // SAFETY: both fds were opened by openpty above and are still open.
        unsafe {
            libc::close(slave);
            libc::close(master);
        }
    }

    /// An unreadable fd must answer "not echoing", so the failure mode of not
    /// knowing is to WRITE. An unanswered DSR kills a reedline shell; a surplus
    /// answer is a cosmetic echo. The permissive direction is the recoverable
    /// one, and this pins which way that is.
    #[test]
    fn an_invalid_fd_fails_permissive() {
        assert!(!slave_is_echoing(-1));
    }
}
