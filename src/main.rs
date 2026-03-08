//! Mado (窓) — GPU-rendered terminal emulator.
//!
//! Follows Ghostty's philosophy:
//! - Native GPU rendering via wgpu (Metal/Vulkan)
//! - Fast, correct VT100/xterm emulation via vte
//! - WGSL shader plugins for visual effects
//! - Hot-reloadable configuration via shikumi

mod config;
mod platform;
mod pty;
mod render;
mod selection;
mod terminal;

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::Parser;
use hasami::{Clipboard, ClipboardProvider};
use madori::event::{AppEvent, KeyEvent, MouseEvent};
use madori::EventResponse;
use tokio::io::AsyncReadExt;
use tracing_subscriber::EnvFilter;

use crate::render::{SharedTerminal, TerminalRenderer};
use crate::selection::{CellPos, Selection};
use crate::terminal::{MouseMode, Terminal};

#[derive(Parser)]
#[command(name = "mado", version, about = "GPU-rendered terminal emulator")]
struct Cli {
    /// Command to execute (default: user's shell)
    #[arg(short, long)]
    command: Option<String>,

    /// Configuration file override
    #[arg(long, env = "MADO_CONFIG")]
    config: Option<std::path::PathBuf>,
}

/// Commands sent from the main thread to the PTY thread.
enum PtyCmd {
    Resize { cols: u16, rows: u16 },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let (config, _config_store) = config::load_and_watch(&cli.config, |new_config| {
        tracing::info!("config reloaded: {:?}", new_config);
    })?;

    tracing::info!("mado starting with config: {:?}", config);

    let shell = cli
        .command
        .or(config.shell.command.clone())
        .unwrap_or_else(default_shell);

    let font_size = config.font_size;
    let padding = config.window.padding as f32;
    let cell_w = font_size * 0.6;
    let cell_h = font_size * 1.4;

    let cols = ((config.window.width as f32 - 2.0 * padding) / cell_w) as usize;
    let rows = ((config.window.height as f32 - 2.0 * padding) / cell_h) as usize;
    let cols = cols.max(10);
    let rows = rows.max(3);

    let terminal: SharedTerminal = Arc::new(Mutex::new(Terminal::new(cols, rows)));
    let terminal_for_pty = Arc::clone(&terminal);

    // Signal from PTY thread → event loop: shell has exited
    let pty_exited = Arc::new(AtomicBool::new(false));
    let pty_exited_writer = Arc::clone(&pty_exited);

    // Channel for keyboard input + terminal responses → PTY
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let response_tx = input_tx.clone();
    // Channel for resize commands → PTY thread
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<PtyCmd>();

    // Spawn PTY I/O on a background thread with its own tokio runtime
    let shell_clone = shell.clone();
    let _pty_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");

        rt.block_on(async move {
            let pty_result = pty::Pty::spawn(&shell_clone, cols as u16, rows as u16).await;
            let pty = match pty_result {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("failed to spawn PTY: {e}");
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

            // We need the master fd to call resize ioctl.
            let master_raw: RawFd = pty.master_raw_fd();

            // Writer task: keyboard input → PTY
            let writer_task = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(data) = input_rx.recv().await {
                    if let Err(e) = writer.write_all(&data).await {
                        tracing::warn!("PTY write error: {e}");
                        break;
                    }
                }
            });

            // Resize listener task
            let resize_task = tokio::spawn(async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        PtyCmd::Resize { cols, rows } => {
                            let ws = libc::winsize {
                                ws_row: rows,
                                ws_col: cols,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            // SAFETY: TIOCSWINSZ on a valid PTY master fd
                            unsafe {
                                libc::ioctl(master_raw, libc::TIOCSWINSZ, &ws);
                            }
                            tracing::debug!(cols, rows, "PTY resized via ioctl");
                        }
                    }
                }
            });

            // Reader loop: PTY output → terminal
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        tracing::info!("PTY closed (EOF)");
                        break;
                    }
                    Ok(n) => {
                        let mut term = terminal_for_pty.lock().unwrap();
                        term.feed(&buf[..n]);
                        // Relay any terminal responses (DSR, DA) back to PTY
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

    let mut renderer = TerminalRenderer::new(
        Arc::clone(&terminal),
        font_size,
        padding,
        config.cursor.style,
        config.cursor.blink,
        config.cursor.blink_rate_ms,
    );
    let terminal_for_events = Arc::clone(&terminal);
    let clipboard = Arc::new(Mutex::new(
        Clipboard::new().expect("failed to initialize clipboard"),
    ));
    let selection = Arc::new(Mutex::new(Selection::new()));
    renderer.set_selection(Arc::clone(&selection));
    // Track last known title to detect changes
    let mut last_title: Option<String> = None;

    madori::App::builder(renderer)
        .title("mado")
        .size(config.window.width, config.window.height)
        .on_event(move |event, renderer| -> EventResponse {
            // Check if PTY has exited — request window close
            if pty_exited.load(Ordering::Acquire) {
                return EventResponse {
                    consumed: true,
                    exit: true,
                    set_title: None,
                };
            }

            match event {
                AppEvent::Key(KeyEvent {
                    pressed: true,
                    text: Some(text),
                    modifiers,
                    ..
                }) => {
                    // Cmd+C — copy selection to clipboard
                    if modifiers.meta && text == "c" {
                        let sel = selection.lock().unwrap();
                        let term = terminal_for_events.lock().unwrap();
                        let rows: Vec<_> = term.visible_rows().map(|r| r.to_vec()).collect();
                        let cols = term.cols();
                        drop(term);
                        if let Some(selected_text) = sel.extract_text(&rows, cols) {
                            if let Ok(cb) = clipboard.lock() {
                                let _ = cb.copy_text(&selected_text);
                            }
                        }
                        return EventResponse::consumed();
                    }

                    // Cmd+V — paste from clipboard
                    if modifiers.meta && text == "v" {
                        if let Ok(cb) = clipboard.lock() {
                            if let Ok(pasted) = cb.paste_text() {
                                if !pasted.is_empty() {
                                    let term = terminal_for_events.lock().unwrap();
                                    let bracketed = term.bracketed_paste();
                                    drop(term);
                                    if bracketed {
                                        let _ = input_tx.send(b"\x1b[200~".to_vec());
                                    }
                                    let _ = input_tx.send(pasted.into_bytes());
                                    if bracketed {
                                        let _ = input_tx.send(b"\x1b[201~".to_vec());
                                    }
                                }
                            }
                        }
                        return EventResponse::consumed();
                    }

                    // Clear selection on any key press
                    if let Ok(mut sel) = selection.lock() {
                        sel.clear();
                    }

                    // Ctrl+letter → control byte (0x01..0x1A)
                    if modifiers.ctrl && text.len() == 1 {
                        let ch = text.chars().next().unwrap();
                        if ch.is_ascii_alphabetic() {
                            let ctrl_byte = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                            let _ = input_tx.send(vec![ctrl_byte]);
                            return EventResponse::consumed();
                        }
                    }

                    // Alt+key → ESC prefix + character
                    if modifiers.alt && !text.is_empty() {
                        let mut bytes = vec![0x1b];
                        bytes.extend_from_slice(text.as_bytes());
                        let _ = input_tx.send(bytes);
                        return EventResponse::consumed();
                    }

                    let _ = input_tx.send(text.as_bytes().to_vec());
                    EventResponse::consumed()
                }
                AppEvent::Key(KeyEvent {
                    pressed: true,
                    key,
                    modifiers,
                    ..
                }) => {
                    let term = terminal_for_events.lock().unwrap();
                    let app_mode = term.cursor_keys_mode();
                    drop(term);

                    // Ctrl+letter for Char keys without text
                    if modifiers.ctrl {
                        if let madori::event::KeyCode::Char(ch) = key {
                            if ch.is_ascii_alphabetic() {
                                let ctrl_byte = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                                let _ = input_tx.send(vec![ctrl_byte]);
                                return EventResponse::consumed();
                            }
                        }
                    }

                    // Alt+char for Char keys without text
                    if modifiers.alt {
                        if let madori::event::KeyCode::Char(ch) = key {
                            let mut bytes = vec![0x1b];
                            let mut buf = [0u8; 4];
                            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            let _ = input_tx.send(bytes);
                            return EventResponse::consumed();
                        }
                    }

                    let bytes: Option<Vec<u8>> = match key {
                        madori::event::KeyCode::Enter => Some(b"\r".to_vec()),
                        madori::event::KeyCode::Backspace => Some(b"\x7f".to_vec()),
                        madori::event::KeyCode::Tab => Some(b"\t".to_vec()),
                        madori::event::KeyCode::Escape => Some(b"\x1b".to_vec()),
                        madori::event::KeyCode::Space => Some(b" ".to_vec()),
                        // Cursor keys: application mode (ESC O x) vs normal (ESC [ x)
                        madori::event::KeyCode::Up => Some(cursor_key(b'A', app_mode)),
                        madori::event::KeyCode::Down => Some(cursor_key(b'B', app_mode)),
                        madori::event::KeyCode::Right => Some(cursor_key(b'C', app_mode)),
                        madori::event::KeyCode::Left => Some(cursor_key(b'D', app_mode)),
                        madori::event::KeyCode::Home => Some(cursor_key(b'H', app_mode)),
                        madori::event::KeyCode::End => Some(cursor_key(b'F', app_mode)),
                        madori::event::KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
                        madori::event::KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
                        madori::event::KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
                        // F1-F12
                        madori::event::KeyCode::F(n) => Some(f_key_escape(*n)),
                        _ => None,
                    };
                    if let Some(bytes) = bytes {
                        let _ = input_tx.send(bytes);
                        return EventResponse::consumed();
                    }
                    EventResponse::ignored()
                }
                // IME commit — forward composed text to PTY
                AppEvent::Ime(madori::ImeEvent::Commit(text)) => {
                    if !text.is_empty() {
                        let _ = input_tx.send(text.as_bytes().to_vec());
                    }
                    EventResponse::consumed()
                }
                // Mouse button events — selection or forward to PTY
                AppEvent::Mouse(MouseEvent::Button {
                    button,
                    pressed,
                    x,
                    y,
                }) => {
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let col = ((*x as f32 - padding) / cw).max(0.0) as usize;
                    let row = ((*y as f32 - padding) / ch).max(0.0) as usize;

                    let term = terminal_for_events.lock().unwrap();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();
                    let term_cols = term.cols();
                    let term_rows = term.rows();
                    drop(term);

                    let col = col.min(term_cols.saturating_sub(1));
                    let row = row.min(term_rows.saturating_sub(1));

                    // Forward mouse events to PTY if mouse tracking is active
                    if mouse_mode != MouseMode::Off
                        && *button == madori::event::MouseButton::Left
                    {
                        // 1-based coordinates for terminal mouse protocol
                        let cx = (col + 1).min(223) as u8;
                        let cy = (row + 1).min(223) as u8;
                        if sgr {
                            let action = if *pressed { 'M' } else { 'm' };
                            let seq = format!("\x1b[<0;{};{}{action}", col + 1, row + 1);
                            let _ = input_tx.send(seq.into_bytes());
                        } else if *pressed {
                            let _ = input_tx.send(vec![0x1b, b'[', b'M', 32, cx + 32, cy + 32]);
                        } else {
                            let _ = input_tx.send(vec![0x1b, b'[', b'M', 35, cx + 32, cy + 32]);
                        }
                        return EventResponse::consumed();
                    }

                    // Text selection via left mouse button
                    if *button == madori::event::MouseButton::Left {
                        let mut sel = selection.lock().unwrap();
                        if *pressed {
                            sel.start(CellPos { row, col });
                        } else {
                            sel.finish();
                        }
                    }
                    EventResponse::consumed()
                }
                // Mouse move — update selection drag or forward to PTY
                AppEvent::Mouse(MouseEvent::Moved { x, y }) => {
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let col = ((*x as f32 - padding) / cw).max(0.0) as usize;
                    let row = ((*y as f32 - padding) / ch).max(0.0) as usize;

                    let term = terminal_for_events.lock().unwrap();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();
                    let term_cols = term.cols();
                    let term_rows = term.rows();
                    drop(term);

                    let col = col.min(term_cols.saturating_sub(1));
                    let row = row.min(term_rows.saturating_sub(1));

                    // Forward mouse motion to PTY if button-event or any-event mode
                    if matches!(mouse_mode, MouseMode::ButtonEvent | MouseMode::AnyEvent) {
                        if sgr {
                            let seq = format!("\x1b[<32;{};{}M", col + 1, row + 1);
                            let _ = input_tx.send(seq.into_bytes());
                        }
                        return EventResponse::consumed();
                    }

                    // Update text selection if dragging
                    let mut sel = selection.lock().unwrap();
                    if sel.is_active() {
                        sel.update(CellPos { row, col });
                    }
                    EventResponse::consumed()
                }
                AppEvent::Mouse(MouseEvent::Scroll { dy, .. }) => {
                    let term = terminal_for_events.lock().unwrap();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();
                    drop(term);

                    // If mouse tracking is active, forward scroll as button events
                    if mouse_mode != MouseMode::Off {
                        // Scroll up = button 64, scroll down = button 65
                        let button = if *dy > 0.0 { 64 } else { 65 };
                        if sgr {
                            let seq = format!("\x1b[<{button};1;1M");
                            let _ = input_tx.send(seq.into_bytes());
                        } else {
                            let _ =
                                input_tx.send(vec![0x1b, b'[', b'M', button + 32, 33, 33]);
                        }
                        return EventResponse::consumed();
                    }

                    let mut term = terminal_for_events.lock().unwrap();
                    let lines = (*dy as isize).unsigned_abs().max(1);
                    if *dy > 0.0 {
                        term.scroll_up(lines);
                    } else {
                        term.scroll_down(lines);
                    }
                    EventResponse::consumed()
                }
                // Focus events → send to PTY if focus reporting is enabled
                AppEvent::Focused(focused) => {
                    let term = terminal_for_events.lock().unwrap();
                    let reporting = term.focus_reporting();
                    drop(term);
                    if reporting {
                        if *focused {
                            let _ = input_tx.send(b"\x1b[I".to_vec());
                        } else {
                            let _ = input_tx.send(b"\x1b[O".to_vec());
                        }
                    }
                    EventResponse::consumed()
                }
                AppEvent::Resized { width, height } => {
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let new_cols = ((*width as f32 - 2.0 * padding) / cw) as usize;
                    let new_rows = ((*height as f32 - 2.0 * padding) / ch) as usize;
                    let new_cols = new_cols.max(10);
                    let new_rows = new_rows.max(3);

                    let mut term = terminal_for_events.lock().unwrap();
                    term.resize(new_cols, new_rows);
                    drop(term);

                    // Resize the PTY too
                    let _ = cmd_tx.send(PtyCmd::Resize {
                        cols: new_cols as u16,
                        rows: new_rows as u16,
                    });
                    EventResponse::consumed()
                }
                // Check for title changes from OSC on every redraw
                AppEvent::RedrawRequested => {
                    // Check for title changes from terminal OSC sequences
                    let term = terminal_for_events.lock().unwrap();
                    let current_title = term.title().map(String::from);
                    drop(term);

                    if current_title != last_title {
                        last_title = current_title.clone();
                        if let Some(title) = current_title {
                            return EventResponse {
                                consumed: false,
                                exit: false,
                                set_title: Some(title),
                            };
                        }
                    }
                    EventResponse::ignored()
                }
                _ => EventResponse::ignored(),
            }
        })
        .run()
        .map_err(|e| anyhow::anyhow!("madori error: {e}"))?;

    Ok(())
}

/// Generate cursor key escape sequence based on mode.
fn cursor_key(ch: u8, application_mode: bool) -> Vec<u8> {
    if application_mode {
        vec![0x1b, b'O', ch]
    } else {
        vec![0x1b, b'[', ch]
    }
}

/// Generate F-key escape sequences.
fn f_key_escape(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => vec![],
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}
