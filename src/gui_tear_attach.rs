//! Phase-3.1 GPU GUI mode for `mado tear-attach --gpu`.
//!
//! Opens a real mado GPU window backed by:
//! - A single `Terminal` (no WindowState, no PaneManager, no local PTY).
//! - A tear-client subscription that feeds PTY bytes into the
//!   Terminal as they arrive from `tear-daemon`.
//! - Mado's existing `TerminalRenderer` in its single-pane fallback
//!   path (`window: None`) — same GPU pipeline that renders a local
//!   shell today.
//! - Keystroke forwarding: KeyEvent → `client.send_keys(pane, bytes)`.
//! - Resize forwarding: window resize → cell-dim math →
//!   `client.pane_resize_absolute(pane, cols, rows)`.
//!
//! Deliberate non-goals for the MVP:
//! - No special-key translation (arrows / function / chord keys);
//!   only `KeyEvent.text` (UTF-8 char input) is forwarded. Phase
//!   3.1.1 will port mado's existing key-table.
//! - No clipboard / selection / search / Rhai scripts.
//! - No multi-pane (that's tear's job — this mode renders one
//!   tear pane in one mado window).
//!
//! The point of the MVP is to prove the GPU vertical end-to-end:
//! `mado tear-attach --gpu <pane>` opens a window that shows
//! exactly what the same pane would show in `tear snapshot`,
//! updated live as the child shell produces output, with a real
//! cursor + scrollback + colors. Phase 3.1.x slices add the
//! missing features.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::RwLock;

use crate::render::{SharedTerminal, TerminalRenderer};
use crate::terminal::{Color as TermColor, Terminal};

/// Entry point — assembles the GUI from a connected tear-client +
/// a subscribed pane id, then runs the madori::App event loop.
pub fn run(pane_id: tear_types::PaneId, socket_path: PathBuf) -> Result<()> {
    use madori::{AppConfig, AppEvent, EventResponse, KeyEvent};

    let config = crate::config::load(&None).unwrap_or_default();

    // ── Tear control connection ───────────────────────────────
    let client = Arc::new(
        tear_client::Client::connect(&socket_path)
            .with_context(|| format!(
                "tear-daemon not reachable at {}: start it with `tear daemon`",
                socket_path.display()
            ))?,
    );

    // ── Pull the pane's current size so the Terminal starts with
    // ── the right dimensions (the daemon's PaneGrid already knows
    // ── them; we read via pane_snapshot rather than guessing).
    use tear_types::MultiplexerControl;
    let snapshot = client
        .pane_snapshot(pane_id)
        .with_context(|| format!("pane_snapshot({pane_id})"))?;
    let cols = snapshot.cols.max(1);
    let rows = snapshot.rows.max(1);

    // ── Single Terminal — no WindowState ──────────────────────
    let terminal: SharedTerminal = Arc::new(RwLock::new(
        Terminal::with_scrollback(cols, rows, 10_000),
    ));

    // ── Renderer construction (mirrors main.rs single-pane path) ─
    let effective_font_size = config.font_size;
    let padding = config.window.padding as f32;
    let bg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.background)
        .unwrap_or(ishou_tokens::Srgb::new(0x2e, 0x34, 0x40));
    let bg_color: wgpu::Color = bg_srgb
        .to_linear()
        .with_alpha(config.appearance.opacity)
        .into();
    let fg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.foreground)
        .unwrap_or(ishou_tokens::Srgb::new(0xec, 0xef, 0xf4));
    let cursor_blink = config.cursor.blink && !config.accessibility.reduce_motion;
    let renderer = TerminalRenderer::new(
        Arc::clone(&terminal),
        effective_font_size,
        config.font_family.clone(),
        config.font_italic.clone(),
        padding,
        config.cursor.style,
        cursor_blink,
        config.cursor.blink_rate_ms,
        bg_color,
        TermColor::new(fg_srgb.r, fg_srgb.g, fg_srgb.b),
    );

    // ── Subscribe to the pane's PTY byte stream ────────────────
    // Feed bytes into the local Terminal's vte parser. Mado's
    // existing render pipeline reads the same Terminal each frame.
    let terminal_for_sub = Arc::clone(&terminal);
    let _subscribe = client
        .subscribe_pane_bytes(pane_id, move |bytes| {
            terminal_for_sub.write().feed(bytes);
        })
        .with_context(|| format!("subscribe_pane_bytes({pane_id})"))?;

    // ── App event handlers ─────────────────────────────────────
    let client_for_events = Arc::clone(&client);
    let app_config = AppConfig {
        title: format!("mado tear-attach {pane_id}"),
        width: config.window.width,
        height: config.window.height,
        resizable: true,
        vsync: config.performance.vsync,
        transparent: false,
    };
    // Same cell-metric formula TerminalRenderer::new uses internally.
    // The HiDPI scale factor multiplies in at first render; for the
    // resize math here we use logical-pixel metrics — good enough
    // for the MVP, exact-cell match comes when the renderer
    // exposes cell_size_px().
    let cell_w_logical = effective_font_size * 0.6;
    let cell_h_logical = effective_font_size * 1.4;
    madori::App::builder(renderer)
        .config(app_config)
        .on_event(move |event, _renderer| -> EventResponse {
            match event {
                AppEvent::Resized { width, height } => {
                    let cols = ((*width as f32 / cell_w_logical) as u16).max(1);
                    let rows = ((*height as f32 / cell_h_logical) as u16).max(1);
                    let _ = client_for_events
                        .pane_resize_absolute(pane_id, cols, rows);
                    EventResponse::ignored()
                }
                AppEvent::Key(KeyEvent {
                    pressed: true,
                    text,
                    ..
                }) => {
                    // MVP: only forward text-producing keys. Special
                    // keys (arrows, ctrl chords, function keys)
                    // need the mado main.rs key-table port —
                    // tracked as a Phase 3.1.x slice.
                    if let Some(t) = text {
                        if !t.is_empty() {
                            let _ = client_for_events
                                .send_keys(pane_id, t.as_bytes());
                        }
                    }
                    EventResponse::consumed()
                }
                AppEvent::CloseRequested => {
                    // Drop the SubscribeHandle (happens when the
                    // closure / run loop returns) — daemon prunes
                    // the dead sender on next chunk.
                    EventResponse::ignored()
                }
                _ => EventResponse::ignored(),
            }
        })
        .run()
        .map_err(|e| anyhow::anyhow!("madori::App run: {e}"))?;
    // Keep the subscribe alive for the lifetime of the run loop.
    drop(_subscribe);
    Ok(())
}
