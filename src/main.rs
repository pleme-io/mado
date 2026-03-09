//! Mado (窓) — GPU-rendered terminal emulator.
//!
//! Follows Ghostty's philosophy:
//! - Native GPU rendering via wgpu (Metal/Vulkan)
//! - Fast, correct VT100/xterm emulation via vte
//! - WGSL shader plugins for visual effects
//! - Hot-reloadable configuration via shikumi

mod config;
mod keybind;
mod mcp;
mod scripting;
mod pane;
mod platform;
mod pty;
mod render;
mod search;
mod selection;
mod tab;
mod terminal;
mod theme;
mod url;
mod window;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::Parser;
use hasami::{Clipboard, ClipboardProvider};
use madori::event::{AppEvent, KeyEvent, MouseEvent};
use madori::EventResponse;
use tracing_subscriber::EnvFilter;

use crate::keybind::{Action, KeybindManager};
use crate::pane::SplitDir;
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::selection::CellPos;
use crate::terminal::{Color, MouseMode};
use crate::window::WindowState;

#[derive(Parser)]
#[command(name = "mado", version, about = "GPU-rendered terminal emulator")]
struct Cli {
    /// Command to execute (default: user's shell)
    #[arg(short, long)]
    command: Option<String>,

    /// Configuration file override
    #[arg(long, env = "MADO_CONFIG")]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    subcmd: Option<SubCmd>,
}

#[derive(clap::Subcommand)]
enum SubCmd {
    /// Run as MCP server (stdio transport) for Claude Code integration.
    Mcp,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Handle MCP subcommand before loading GUI config
    if let Some(SubCmd::Mcp) = cli.subcmd {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(mcp::run())
            .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
        return Ok(());
    }

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

    let scrollback = config.behavior.scrollback_lines;

    // Create window state — manages tabs, panes, and per-pane terminals+PTYs
    let window = Arc::new(Mutex::new(WindowState::new(shell, cols, rows, scrollback)));

    // Get initial pane's terminal for the renderer
    let initial_terminal: SharedTerminal = {
        let ws = window.lock().unwrap();
        let pane = ws.focused_pane().expect("initial pane");
        Arc::clone(&pane.terminal)
    };

    let bg_color = parse_hex_color(&config.appearance.background, 0.180, 0.204, 0.251);
    let fg_hex = parse_hex_rgb(&config.appearance.foreground, 236, 239, 244);

    let mut renderer = TerminalRenderer::new(
        initial_terminal,
        font_size,
        config.font_family.clone(),
        padding,
        config.cursor.style,
        config.cursor.blink,
        config.cursor.blink_rate_ms,
        wgpu::Color {
            r: bg_color.0,
            g: bg_color.1,
            b: bg_color.2,
            a: f64::from(config.appearance.opacity),
        },
        Color::new(fg_hex.0, fg_hex.1, fg_hex.2),
    );

    // Apply accessibility settings from config
    renderer.set_colorblind_mode(config.accessibility.colorblind);

    // Set initial pane's selection/search on renderer (for single-pane fallback)
    {
        let ws = window.lock().unwrap();
        let pane = ws.focused_pane().expect("initial pane");
        renderer.set_selection(Arc::clone(&pane.selection));
        renderer.set_search(Arc::clone(&pane.search));
    }
    renderer.set_window(Arc::clone(&window));

    let window_for_events = Arc::clone(&window);
    let clipboard = Arc::new(Mutex::new(
        Clipboard::new().expect("failed to initialize clipboard"),
    ));
    let keybinds = KeybindManager::new();
    // Track last known title to detect changes
    let mut last_title: Option<String> = None;
    // Track window dimensions for split resize
    let mut last_width = config.window.width;
    let mut last_height = config.window.height;
    // Double/triple click tracking
    let default_font_size = font_size;
    let mut last_click_time = Instant::now();
    let mut click_count: u8 = 0;
    let mut last_click_pos = CellPos { row: 0, col: 0 };

    madori::App::builder(renderer)
        .title("mado")
        .size(config.window.width, config.window.height)
        .on_event(move |event, renderer| -> EventResponse {
            // Check if PTY has exited — request window close
            {
                let ws = window_for_events.lock().unwrap();
                if ws.any_exited() {
                    return EventResponse {
                        consumed: true,
                        exit: true,
                        set_title: None,
                    };
                }
            }

            match event {
                AppEvent::Key(KeyEvent {
                    pressed: true,
                    text,
                    key,
                    modifiers,
                    ..
                }) => {
                    // Map winit modifiers to awase modifiers
                    let mut awase_mods = awase::Modifiers::NONE;
                    if modifiers.ctrl {
                        awase_mods = awase_mods | awase::Modifiers::CTRL;
                    }
                    if modifiers.alt {
                        awase_mods = awase_mods | awase::Modifiers::ALT;
                    }
                    if modifiers.shift {
                        awase_mods = awase_mods | awase::Modifiers::SHIFT;
                    }
                    if modifiers.meta {
                        awase_mods = awase_mods | awase::Modifiers::CMD;
                    }

                    // Keep track of raw modifiers for non-keybind usage
                    let mods = modifiers;

                    // Determine awase key from the event
                    let awase_key = winit_to_awase_key(key, text);

                    // Check for keybind action first
                    let action = awase_key
                        .map(|k| awase::Hotkey::new(awase_mods, k))
                        .and_then(|hk| keybinds.lookup(&hk));

                    // Handle search mode input
                    {
                        let ws = window_for_events.lock().unwrap();
                        if let Some(pane) = ws.focused_pane() {
                            let search_active = pane.search.lock().unwrap().active;
                            if search_active {
                                match action {
                                    Some(Action::SearchClose) => {
                                        pane.search.lock().unwrap().close();
                                        return EventResponse::consumed();
                                    }
                                    Some(Action::SearchNext) => {
                                        pane.search.lock().unwrap().next();
                                        return EventResponse::consumed();
                                    }
                                    Some(Action::SearchPrev) => {
                                        pane.search.lock().unwrap().prev();
                                        return EventResponse::consumed();
                                    }
                                    _ => {}
                                }
                                // In search mode, typing updates the query (no modifiers)
                                if !mods.ctrl && !mods.meta && !mods.alt {
                                    if let Some(text) = text {
                                        if !text.is_empty() {
                                            let mut s = pane.search.lock().unwrap();
                                            let mut query = s.query.clone();
                                            query.push_str(text);
                                            let term = pane.terminal.lock().unwrap();
                                            let rows: Vec<_> =
                                                term.visible_rows().map(|r| r.to_vec()).collect();
                                            let cols = term.cols();
                                            drop(term);
                                            s.set_query(&query, &rows, cols);
                                            return EventResponse::consumed();
                                        }
                                    }
                                    // Handle backspace in search
                                    if matches!(key, madori::event::KeyCode::Backspace) {
                                        let mut s = pane.search.lock().unwrap();
                                        let mut query = s.query.clone();
                                        query.pop();
                                        let term = pane.terminal.lock().unwrap();
                                        let rows: Vec<_> =
                                            term.visible_rows().map(|r| r.to_vec()).collect();
                                        let cols = term.cols();
                                        drop(term);
                                        s.set_query(&query, &rows, cols);
                                        return EventResponse::consumed();
                                    }
                                }
                                return EventResponse::consumed();
                            }
                        }
                    }

                    // Dispatch keybind action
                    if let Some(action) = action {
                        match action {
                            Action::Copy => {
                                let selected_text = {
                                    let ws = window_for_events.lock().unwrap();
                                    ws.focused_pane().and_then(|pane| {
                                        let sel = pane.selection.lock().unwrap();
                                        let term = pane.terminal.lock().unwrap();
                                        let rows: Vec<_> =
                                            term.visible_rows().map(|r| r.to_vec()).collect();
                                        let cols = term.cols();
                                        drop(term);
                                        sel.extract_text(&rows, cols)
                                    })
                                };
                                if let Some(text) = selected_text {
                                    if let Ok(cb) = clipboard.lock() {
                                        let _ = cb.copy_text(&text);
                                    }
                                }
                                return EventResponse::consumed();
                            }
                            Action::Paste => {
                                let pasted_text =
                                    clipboard.lock().ok().and_then(|cb| cb.paste_text().ok());
                                if let Some(pasted) = pasted_text {
                                    if !pasted.is_empty() {
                                        let ws = window_for_events.lock().unwrap();
                                        if let Some(pane) = ws.focused_pane() {
                                            let term = pane.terminal.lock().unwrap();
                                            let bracketed = term.bracketed_paste();
                                            drop(term);
                                            if bracketed {
                                                let _ =
                                                    pane.input_tx.send(b"\x1b[200~".to_vec());
                                            }
                                            let _ = pane.input_tx.send(pasted.into_bytes());
                                            if bracketed {
                                                let _ =
                                                    pane.input_tx.send(b"\x1b[201~".to_vec());
                                            }
                                        }
                                    }
                                }
                                return EventResponse::consumed();
                            }
                            Action::SearchOpen => {
                                let ws = window_for_events.lock().unwrap();
                                if let Some(pane) = ws.focused_pane() {
                                    pane.search.lock().unwrap().open();
                                }
                                return EventResponse::consumed();
                            }
                            Action::SearchClose | Action::SearchNext | Action::SearchPrev => {
                                // Already handled above when search is active
                                return EventResponse::consumed();
                            }
                            Action::ScrollPageUp => {
                                let ws = window_for_events.lock().unwrap();
                                if let Some(pane) = ws.focused_pane() {
                                    let mut term = pane.terminal.lock().unwrap();
                                    let page = term.rows();
                                    term.scroll_up(page);
                                }
                                return EventResponse::consumed();
                            }
                            Action::ScrollPageDown => {
                                let ws = window_for_events.lock().unwrap();
                                if let Some(pane) = ws.focused_pane() {
                                    let mut term = pane.terminal.lock().unwrap();
                                    let page = term.rows();
                                    term.scroll_down(page);
                                }
                                return EventResponse::consumed();
                            }
                            Action::SplitHorizontal => {
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let mut ws = window_for_events.lock().unwrap();
                                let new_id = ws.split(SplitDir::Horizontal, cols, rows);
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                tracing::info!(pane = new_id.0, "horizontal split");
                                return EventResponse::consumed();
                            }
                            Action::SplitVertical => {
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let mut ws = window_for_events.lock().unwrap();
                                let new_id = ws.split(SplitDir::Vertical, cols, rows);
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                tracing::info!(pane = new_id.0, "vertical split");
                                return EventResponse::consumed();
                            }
                            Action::ClosePane => {
                                let mut ws = window_for_events.lock().unwrap();
                                if let Some(closed) = ws.close_focused_pane() {
                                    tracing::info!(pane = closed.0, "closed pane");
                                }
                                return EventResponse::consumed();
                            }
                            Action::FocusNext => {
                                let mut ws = window_for_events.lock().unwrap();
                                ws.focus_next();
                                return EventResponse::consumed();
                            }
                            Action::FocusPrev => {
                                let mut ws = window_for_events.lock().unwrap();
                                ws.focus_prev();
                                return EventResponse::consumed();
                            }
                            Action::FontIncrease => {
                                let new_size = renderer.font_size() + 1.0;
                                renderer.set_font_size(new_size);
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let ws = window_for_events.lock().unwrap();
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                return EventResponse::consumed();
                            }
                            Action::FontDecrease => {
                                let new_size = renderer.font_size() - 1.0;
                                renderer.set_font_size(new_size);
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let ws = window_for_events.lock().unwrap();
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                return EventResponse::consumed();
                            }
                            Action::FontReset => {
                                renderer.set_font_size(default_font_size);
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let ws = window_for_events.lock().unwrap();
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                return EventResponse::consumed();
                            }
                            Action::ScrollToTop => {
                                let ws = window_for_events.lock().unwrap();
                                if let Some(pane) = ws.focused_pane() {
                                    pane.terminal.lock().unwrap().scroll_to_top();
                                }
                                return EventResponse::consumed();
                            }
                            Action::ScrollToBottom => {
                                let ws = window_for_events.lock().unwrap();
                                if let Some(pane) = ws.focused_pane() {
                                    pane.terminal.lock().unwrap().scroll_to_bottom();
                                }
                                return EventResponse::consumed();
                            }
                            Action::ResetTerminal => {
                                let ws = window_for_events.lock().unwrap();
                                if let Some(pane) = ws.focused_pane() {
                                    pane.terminal.lock().unwrap().reset();
                                }
                                return EventResponse::consumed();
                            }
                            Action::NewTab => {
                                let mut ws = window_for_events.lock().unwrap();
                                ws.new_tab();
                            }
                            Action::CloseTab => {
                                let mut ws = window_for_events.lock().unwrap();
                                if ws.close_tab().is_none() {
                                    return EventResponse {
                                        exit: true,
                                        ..Default::default()
                                    };
                                }
                            }
                            Action::NextTab => {
                                let mut ws = window_for_events.lock().unwrap();
                                ws.next_tab();
                            }
                            Action::PrevTab => {
                                let mut ws = window_for_events.lock().unwrap();
                                ws.prev_tab();
                            }
                            Action::ToggleFullscreen => {
                                tracing::debug!("fullscreen toggle (not yet implemented)");
                            }
                            Action::ScrollUp | Action::ScrollDown => {
                                // These are handled by scroll wheel events
                            }
                        }
                    }

                    // Clear selection on any key press
                    {
                        let ws = window_for_events.lock().unwrap();
                        if let Some(pane) = ws.focused_pane() {
                            pane.selection.lock().unwrap().clear();
                        }
                    }

                    // Kitty keyboard protocol: if active, encode keys using the protocol
                    {
                        let ws = window_for_events.lock().unwrap();
                        if let Some(pane) = ws.focused_pane() {
                            let kitty_flags =
                                pane.terminal.lock().unwrap().kitty_keyboard_flags();
                            if kitty_flags > 0 {
                                if let Some(encoded) =
                                    kitty_encode_key(key, text, modifiers, kitty_flags)
                                {
                                    let _ = pane.input_tx.send(encoded);
                                    return EventResponse::consumed();
                                }
                            }
                        }
                    }

                    // Handle text input
                    if let Some(text) = text {
                        if !text.is_empty() {
                            // Ctrl+letter → control byte (0x01..0x1A)
                            if modifiers.ctrl && text.len() == 1 {
                                let ch = text.chars().next().unwrap();
                                if ch.is_ascii_alphabetic() {
                                    let ctrl_byte =
                                        (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                                    let ws = window_for_events.lock().unwrap();
                                    ws.send_input(vec![ctrl_byte]);
                                    return EventResponse::consumed();
                                }
                            }

                            // Alt+key → ESC prefix + character
                            if modifiers.alt {
                                let mut bytes = vec![0x1b];
                                bytes.extend_from_slice(text.as_bytes());
                                let ws = window_for_events.lock().unwrap();
                                ws.send_input(bytes);
                                return EventResponse::consumed();
                            }

                            let ws = window_for_events.lock().unwrap();
                            ws.send_input(text.as_bytes().to_vec());
                            return EventResponse::consumed();
                        }
                    }

                    // Handle keys without text
                    let ws = window_for_events.lock().unwrap();
                    let app_mode = ws
                        .focused_pane()
                        .map(|p| p.terminal.lock().unwrap().cursor_keys_mode())
                        .unwrap_or(false);

                    // Ctrl+letter for Char keys without text
                    if modifiers.ctrl {
                        if let madori::event::KeyCode::Char(ch) = key {
                            if ch.is_ascii_alphabetic() {
                                let ctrl_byte = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                                ws.send_input(vec![ctrl_byte]);
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
                            ws.send_input(bytes);
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
                        ws.send_input(bytes);
                        return EventResponse::consumed();
                    }
                    EventResponse::ignored()
                }
                // IME commit — forward composed text to PTY
                AppEvent::Ime(madori::ImeEvent::Commit(text)) => {
                    if !text.is_empty() {
                        let ws = window_for_events.lock().unwrap();
                        ws.send_input(text.as_bytes().to_vec());
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

                    let ws = window_for_events.lock().unwrap();
                    let Some(pane) = ws.focused_pane() else {
                        return EventResponse::consumed();
                    };

                    let term = pane.terminal.lock().unwrap();
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
                        let cx = (col + 1).min(223) as u8;
                        let cy = (row + 1).min(223) as u8;
                        if sgr {
                            let action = if *pressed { 'M' } else { 'm' };
                            let seq = format!("\x1b[<0;{};{}{action}", col + 1, row + 1);
                            let _ = pane.input_tx.send(seq.into_bytes());
                        } else if *pressed {
                            let _ =
                                pane.input_tx.send(vec![0x1b, b'[', b'M', 32, cx + 32, cy + 32]);
                        } else {
                            let _ =
                                pane.input_tx.send(vec![0x1b, b'[', b'M', 35, cx + 32, cy + 32]);
                        }
                        return EventResponse::consumed();
                    }

                    // Text selection via left mouse button
                    if *button == madori::event::MouseButton::Left {
                        if *pressed {
                            let now = Instant::now();
                            let same_pos = last_click_pos.row == row && last_click_pos.col == col;
                            let quick = now.duration_since(last_click_time).as_millis() < 400;

                            if same_pos && quick {
                                click_count = (click_count + 1).min(3);
                            } else {
                                click_count = 1;
                            }
                            last_click_time = now;
                            last_click_pos = CellPos { row, col };

                            let mut sel = pane.selection.lock().unwrap();
                            match click_count {
                                2 => {
                                    // Double-click: select word
                                    let term = pane.terminal.lock().unwrap();
                                    let rows: Vec<_> =
                                        term.visible_rows().map(|r| r.to_vec()).collect();
                                    let cols_count = term.cols();
                                    drop(term);
                                    sel.select_word(CellPos { row, col }, &rows, cols_count);
                                }
                                3 => {
                                    // Triple-click: select entire line
                                    let term = pane.terminal.lock().unwrap();
                                    let cols_count = term.cols();
                                    drop(term);
                                    sel.select_line(row, cols_count);
                                }
                                _ => {
                                    sel.start(CellPos { row, col });
                                }
                            }
                        } else {
                            let mut sel = pane.selection.lock().unwrap();
                            if click_count == 1 {
                                sel.finish();
                            }
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

                    let ws = window_for_events.lock().unwrap();
                    let Some(pane) = ws.focused_pane() else {
                        return EventResponse::consumed();
                    };

                    let term = pane.terminal.lock().unwrap();
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
                            let _ = pane.input_tx.send(seq.into_bytes());
                        }
                        return EventResponse::consumed();
                    }

                    // Update text selection if dragging
                    let mut sel = pane.selection.lock().unwrap();
                    if sel.is_active() {
                        sel.update(CellPos { row, col });
                    }
                    EventResponse::consumed()
                }
                AppEvent::Mouse(MouseEvent::Scroll { dy, .. }) => {
                    let ws = window_for_events.lock().unwrap();
                    let Some(pane) = ws.focused_pane() else {
                        return EventResponse::consumed();
                    };

                    let mut term = pane.terminal.lock().unwrap();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();

                    // If mouse tracking is active, forward scroll as button events
                    if mouse_mode != MouseMode::Off {
                        drop(term);
                        let button = if *dy > 0.0 { 64 } else { 65 };
                        if sgr {
                            let seq = format!("\x1b[<{button};1;1M");
                            let _ = pane.input_tx.send(seq.into_bytes());
                        } else {
                            let _ =
                                pane.input_tx.send(vec![0x1b, b'[', b'M', button + 32, 33, 33]);
                        }
                        return EventResponse::consumed();
                    }

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
                    let ws = window_for_events.lock().unwrap();
                    if let Some(pane) = ws.focused_pane() {
                        let term = pane.terminal.lock().unwrap();
                        let reporting = term.focus_reporting();
                        drop(term);
                        if reporting {
                            if *focused {
                                let _ = pane.input_tx.send(b"\x1b[I".to_vec());
                            } else {
                                let _ = pane.input_tx.send(b"\x1b[O".to_vec());
                            }
                        }
                    }
                    EventResponse::consumed()
                }
                AppEvent::Resized { width, height } => {
                    last_width = *width;
                    last_height = *height;
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let ws = window_for_events.lock().unwrap();
                    ws.resize_panes(*width as f32, *height as f32, padding, cw, ch);
                    EventResponse::consumed()
                }
                // Check for title/bell/clipboard changes on every redraw
                AppEvent::RedrawRequested => {
                    let ws = window_for_events.lock().unwrap();
                    if let Some(pane) = ws.focused_pane() {
                        let mut term = pane.terminal.lock().unwrap();
                        let current_title = term.title().map(String::from);
                        let bell = term.take_bell();
                        let osc52_clip = term.take_clipboard();
                        drop(term);
                        drop(ws);

                        // OSC 52 clipboard sync — copy terminal clipboard to system
                        if let Some(clip_text) = osc52_clip {
                            if let Ok(cb) = clipboard.lock() {
                                let _ = cb.copy_text(&clip_text);
                            }
                        }

                        // Bell flash
                        if bell {
                            renderer.trigger_bell();
                        }

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

/// Encode a key event for the Kitty keyboard protocol.
/// Returns the escape sequence bytes if the protocol is active, None otherwise.
fn kitty_encode_key(
    key: &madori::event::KeyCode,
    text: &Option<String>,
    modifiers: &madori::event::Modifiers,
    flags: u32,
) -> Option<Vec<u8>> {
    if flags == 0 {
        return None;
    }

    // Build modifier value: 1 + bitmask
    let mut mod_bits: u32 = 0;
    if modifiers.shift {
        mod_bits |= 1;
    }
    if modifiers.alt {
        mod_bits |= 2;
    }
    if modifiers.ctrl {
        mod_bits |= 4;
    }
    if modifiers.meta {
        mod_bits |= 8; // super
    }
    let mod_val = 1 + mod_bits;

    // Map key to unicode codepoint or special key number
    match key {
        madori::event::KeyCode::Enter => Some(kitty_key_seq(13, mod_val, 'u')),
        madori::event::KeyCode::Tab => Some(kitty_key_seq(9, mod_val, 'u')),
        madori::event::KeyCode::Backspace => Some(kitty_key_seq(127, mod_val, 'u')),
        madori::event::KeyCode::Escape => Some(kitty_key_seq(27, mod_val, 'u')),
        madori::event::KeyCode::Space => Some(kitty_key_seq(b' ' as u32, mod_val, 'u')),
        madori::event::KeyCode::Delete => Some(kitty_tilde_seq(3, mod_val)),
        madori::event::KeyCode::Up => Some(kitty_key_seq(1, mod_val, 'A')),
        madori::event::KeyCode::Down => Some(kitty_key_seq(1, mod_val, 'B')),
        madori::event::KeyCode::Right => Some(kitty_key_seq(1, mod_val, 'C')),
        madori::event::KeyCode::Left => Some(kitty_key_seq(1, mod_val, 'D')),
        madori::event::KeyCode::Home => Some(kitty_key_seq(1, mod_val, 'H')),
        madori::event::KeyCode::End => Some(kitty_key_seq(1, mod_val, 'F')),
        madori::event::KeyCode::PageUp => Some(kitty_tilde_seq(5, mod_val)),
        madori::event::KeyCode::PageDown => Some(kitty_tilde_seq(6, mod_val)),
        madori::event::KeyCode::F(n) => {
            let (num, suffix) = match n {
                1 => (1, 'P'),
                2 => (1, 'Q'),
                3 => (1, 'R'),
                4 => (1, 'S'),
                5 => (15, '~'),
                6 => (17, '~'),
                7 => (18, '~'),
                8 => (19, '~'),
                9 => (20, '~'),
                10 => (21, '~'),
                11 => (23, '~'),
                12 => (24, '~'),
                _ => return None,
            };
            if suffix == '~' {
                Some(kitty_tilde_seq(num, mod_val))
            } else {
                Some(kitty_key_seq(num, mod_val, suffix))
            }
        }
        madori::event::KeyCode::Char(ch) => {
            // Level 1 (disambiguate): only encode with modifiers beyond shift
            if mod_bits <= 1 {
                // No modifiers or just shift — send as normal UTF-8
                return None;
            }
            let cp = *ch as u32;
            Some(kitty_key_seq(cp, mod_val, 'u'))
        }
        _ => {
            // Try text content
            if let Some(t) = text {
                if let Some(ch) = t.chars().next() {
                    if t.len() == ch.len_utf8() && mod_bits > 1 {
                        return Some(kitty_key_seq(ch as u32, mod_val, 'u'));
                    }
                }
            }
            None
        }
    }
}

/// Build CSI number ; modifiers X sequence for Kitty protocol.
fn kitty_key_seq(number: u32, modifiers: u32, suffix: char) -> Vec<u8> {
    if modifiers <= 1 && suffix == 'u' {
        // No modifiers — CSI number u
        format!("\x1b[{number}{suffix}").into_bytes()
    } else if suffix == 'u' {
        format!("\x1b[{number};{modifiers}{suffix}").into_bytes()
    } else if modifiers <= 1 {
        // Arrow/Home/End without modifiers — standard CSI X
        format!("\x1b[{suffix}").into_bytes()
    } else {
        format!("\x1b[{number};{modifiers}{suffix}").into_bytes()
    }
}

/// Build CSI number ; modifiers ~ sequence for Kitty protocol (tilde keys).
fn kitty_tilde_seq(number: u32, modifiers: u32) -> Vec<u8> {
    if modifiers <= 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{modifiers}~").into_bytes()
    }
}

/// Map madori key event to awase Key.
fn winit_to_awase_key(key: &madori::event::KeyCode, text: &Option<String>) -> Option<awase::Key> {
    match key {
        madori::event::KeyCode::Enter => Some(awase::Key::Return),
        madori::event::KeyCode::Escape => Some(awase::Key::Escape),
        madori::event::KeyCode::Tab => Some(awase::Key::Tab),
        madori::event::KeyCode::Backspace => Some(awase::Key::Backspace),
        madori::event::KeyCode::Delete => Some(awase::Key::Delete),
        madori::event::KeyCode::Home => Some(awase::Key::Home),
        madori::event::KeyCode::End => Some(awase::Key::End),
        madori::event::KeyCode::PageUp => Some(awase::Key::PageUp),
        madori::event::KeyCode::PageDown => Some(awase::Key::PageDown),
        madori::event::KeyCode::Up => Some(awase::Key::Up),
        madori::event::KeyCode::Down => Some(awase::Key::Down),
        madori::event::KeyCode::Left => Some(awase::Key::Left),
        madori::event::KeyCode::Right => Some(awase::Key::Right),
        madori::event::KeyCode::F(n) => match n {
            1 => Some(awase::Key::F1),
            2 => Some(awase::Key::F2),
            3 => Some(awase::Key::F3),
            4 => Some(awase::Key::F4),
            5 => Some(awase::Key::F5),
            6 => Some(awase::Key::F6),
            7 => Some(awase::Key::F7),
            8 => Some(awase::Key::F8),
            9 => Some(awase::Key::F9),
            10 => Some(awase::Key::F10),
            11 => Some(awase::Key::F11),
            12 => Some(awase::Key::F12),
            _ => None,
        },
        madori::event::KeyCode::Char(ch) => char_to_awase_key(*ch),
        madori::event::KeyCode::Space => Some(awase::Key::Space),
        _ => {
            // Try to extract from text
            if let Some(t) = text {
                if let Some(ch) = t.chars().next() {
                    if t.len() == ch.len_utf8() {
                        return char_to_awase_key(ch);
                    }
                }
            }
            None
        }
    }
}

/// Map a character to an awase key.
fn char_to_awase_key(ch: char) -> Option<awase::Key> {
    match ch.to_ascii_lowercase() {
        'a' => Some(awase::Key::A),
        'b' => Some(awase::Key::B),
        'c' => Some(awase::Key::C),
        'd' => Some(awase::Key::D),
        'e' => Some(awase::Key::E),
        'f' => Some(awase::Key::F),
        'g' => Some(awase::Key::G),
        'h' => Some(awase::Key::H),
        'i' => Some(awase::Key::I),
        'j' => Some(awase::Key::J),
        'k' => Some(awase::Key::K),
        'l' => Some(awase::Key::L),
        'm' => Some(awase::Key::M),
        'n' => Some(awase::Key::N),
        'o' => Some(awase::Key::O),
        'p' => Some(awase::Key::P),
        'q' => Some(awase::Key::Q),
        'r' => Some(awase::Key::R),
        's' => Some(awase::Key::S),
        't' => Some(awase::Key::T),
        'u' => Some(awase::Key::U),
        'v' => Some(awase::Key::V),
        'w' => Some(awase::Key::W),
        'x' => Some(awase::Key::X),
        'y' => Some(awase::Key::Y),
        'z' => Some(awase::Key::Z),
        '0' => Some(awase::Key::Num0),
        '1' => Some(awase::Key::Num1),
        '2' => Some(awase::Key::Num2),
        '3' => Some(awase::Key::Num3),
        '4' => Some(awase::Key::Num4),
        '5' => Some(awase::Key::Num5),
        '6' => Some(awase::Key::Num6),
        '7' => Some(awase::Key::Num7),
        '8' => Some(awase::Key::Num8),
        '9' => Some(awase::Key::Num9),
        ' ' => Some(awase::Key::Space),
        '/' => Some(awase::Key::Slash),
        '+' | '=' => Some(awase::Key::Equal),
        '-' => Some(awase::Key::Minus),
        ',' => Some(awase::Key::Comma),
        '.' => Some(awase::Key::Period),
        _ => None,
    }
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

/// Parse a hex color string like "#2e3440" into (f64, f64, f64) normalized to 0.0..1.0.
fn parse_hex_color(hex: &str, dr: f64, dg: f64, db: f64) -> (f64, f64, f64) {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return (dr, dg, db);
    }
    let Ok(r) = u8::from_str_radix(&hex[0..2], 16) else {
        return (dr, dg, db);
    };
    let Ok(g) = u8::from_str_radix(&hex[2..4], 16) else {
        return (dr, dg, db);
    };
    let Ok(b) = u8::from_str_radix(&hex[4..6], 16) else {
        return (dr, dg, db);
    };
    (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    )
}

/// Parse a hex color string like "#eceff4" into (u8, u8, u8).
fn parse_hex_rgb(hex: &str, dr: u8, dg: u8, db: u8) -> (u8, u8, u8) {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return (dr, dg, db);
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(dr);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(dg);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(db);
    (r, g, b)
}
