//! [`InputEngine`] — the ONE place every input/UX capability is
//! implemented (docs/REMEDIATION-PLAN.md §M1).
//!
//! Handler bodies are step-lifted from the pre-M1 `main.rs`
//! local-PTY loop (the more complete copy), parameterized over the
//! per-mode divergences:
//!
//! * PTY writes → [`PtySink`] (local `input_tx` vs tear `send_keys`)
//! * grid pushes → [`ResizeSink`] (local `resize_tx` vs tear
//!   `pane_resize_absolute`)
//! * DECCKM query → `cursor_keys_mode` closure (local mirror-Terminal
//!   read vs tear `pane_cursor_keys_mode`)
//!
//! Everything else — selection, copy/paste (PasteGuard), search +
//! dir-picker overlay routing, full mouse forwarding, kitty CSI-u,
//! focus events, IME, font zoom, the PTY-grid⇄display reconciler —
//! is shared state inside the engine. Capabilities the two pre-M1
//! copies had each implemented partially now hold in BOTH modes:
//! the embedded default gains dir-picker / prompt-jump / scroll
//! actions / select-all; the local fallback gains the key-repeat
//! storm gate and the curated default keybind baseline.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use hasami::ClipboardProvider;
use madori::event::{KeyCode, KeyEvent, Modifiers, MouseButton};

use crate::dir_picker::DirPickerState;
use crate::font_size::{BoundedFontSize, FontSizeSteps};
use crate::keybind::{Action, KeybindManager};
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::search::SearchState;
use crate::selection::{CellPos, Selection};
use crate::terminal::{Cell, MouseMode, SelectionAnchor};
use crate::ux::mouse_report::{MouseMods, MouseReport, MouseReportButton, MouseReportKind};
use crate::ux::{EventOutcome, FontZoomTarget, PtySink, ResizeSink, UxBehavior};

/// The shared overlay/selection state the renderer highlights from
/// and the engine mutates. One value, three Arcs — constructed
/// together so a call site cannot wire half of them.
pub struct SharedUxState {
    pub selection: Arc<Mutex<Selection>>,
    pub search: Arc<Mutex<SearchState>>,
    pub dir_picker: Arc<Mutex<DirPickerState>>,
}

impl SharedUxState {
    /// Fresh state — the embedded-tear path's shape (no pre-existing
    /// Arcs to share).
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            selection: Arc::new(Mutex::new(Selection::new())),
            search: Arc::new(Mutex::new(SearchState::new())),
            dir_picker: Arc::new(Mutex::new(DirPickerState::new())),
        }
    }
}

/// Constructor parameters for [`InputEngine::attach_to_renderer`].
pub struct InputEngineParams {
    /// The mirror VT terminal (local: the PTY-fed Terminal; tear:
    /// the engate-fed mirror).
    pub terminal: SharedTerminal,
    /// Where encoded bytes go (keystrokes, mouse reports, paste).
    pub pty: Box<dyn PtySink>,
    /// Where grid pushes go (PTY winsize / pane_resize_absolute).
    pub resize: Box<dyn ResizeSink>,
    /// Selection + search + dir-picker Arcs (renderer-shared).
    pub shared: SharedUxState,
    /// System clipboard (or `hasami::MockClipboard` in tests).
    pub clipboard: Arc<dyn ClipboardProvider>,
    /// Chord → Action map (`keybind::manager_from_config`).
    pub keybinds: KeybindManager,
    /// The typed config subset the handlers read.
    pub behavior: UxBehavior,
    /// DECCKM (cursor-keys application mode) query — the one
    /// read-side divergence between the modes.
    pub cursor_keys_mode: Box<dyn Fn() -> bool + Send + Sync>,
    /// Font size `Action::FontReset` returns to.
    pub default_font_size: f32,
    /// Logical window padding (scaled by the renderer's factor for
    /// pixel→cell math).
    pub padding: f32,
}

/// Result of dispatching a keybind [`Action`].
pub enum ActionOutcome {
    /// A handler ran and the keystroke is consumed.
    Consumed(EventOutcome),
    /// The action either executed without consuming (pre-M1 `main.rs`
    /// arms with no `return` — prompt jump, select-all, clear-screen,
    /// …) or has no handler (Search* on a closed overlay — the fleet
    /// atlas binds search_close to bare Escape, and consuming it here
    /// would eat Esc for vim/helix/fzf; review finding 2026-06-11).
    /// The key continues down the pipeline (selection-clear → kitty →
    /// PTY translation).
    FallThrough,
}

/// Selection-drag granularity: what unit the live drag snaps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    /// Plain drag — endpoints are the exact cells.
    Char,
    /// Double-click drag — BOTH ends snap to word boundaries; the
    /// origin word stays fully selected (kitty/ghostty contract).
    Word,
    /// Triple-click drag — both ends snap to full physical rows.
    Line,
}

/// The selection-drag FSM state — ONE typed value, no scattered
/// bools (per the determinism+FSM directive; the upcoming engine-
/// modal-state FSM lift consumes this enum as-is). `origin` is the
/// content-anchored span captured at press time: char drags carry
/// the press cell twice; word/line drags carry the snapped origin
/// unit, which every motion-time union keeps fully selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionDrag {
    Idle,
    Drag {
        mode: DragMode,
        origin: (SelectionAnchor, SelectionAnchor),
    },
}

/// The unified input/UX engine. Owns every piece of interaction
/// state the pre-M1 loops kept as closure-captured locals.
pub struct InputEngine {
    terminal: SharedTerminal,
    pty: Box<dyn PtySink>,
    resize: Box<dyn ResizeSink>,
    selection: Arc<Mutex<Selection>>,
    search: Arc<Mutex<SearchState>>,
    dir_picker: Arc<Mutex<DirPickerState>>,
    clipboard: Arc<dyn ClipboardProvider>,
    keybinds: KeybindManager,
    pub(crate) behavior: UxBehavior,
    cursor_keys_mode: Box<dyn Fn() -> bool + Send + Sync>,
    default_font_size: f32,
    padding: f32,
    /// Per-action debouncer for OS key-repeat storms. Default 80ms
    /// window — OS key-repeat is ~30-50ms, this drops storm-ticks
    /// while allowing 12 intentional presses per second. Per the
    /// 2026-05-21 runaway-font incident (Cmd-= held → font grew
    /// 14 → 32pt in 1.5s); the gate caps that to ~19 transitions
    /// and `BoundedFontSize` caps the final value at FONT_MAX = 64.
    key_repeat_gate: awase::KeyRepeatGate<Action>,
    /// `mouse_hide_while_typing` latch: false = pointer hidden.
    mouse_visible: bool,
    // Double/triple click tracking.
    last_click_time: Instant,
    click_count: u8,
    last_click_pos: CellPos,
    /// Whether the left button is currently held — drives the SGR
    /// motion code (drag = 32 vs hover = 35) and ButtonEvent(1002)'s
    /// motion-only-while-pressed contract.
    left_button_down: bool,
    /// A shift+click bypassed mouse tracking and started a
    /// terminal-side selection — motion events update the selection
    /// instead of forwarding while this drag is live.
    shift_drag_bypass: bool,
    /// The live selection-drag FSM (`Idle` ↔ `Drag { mode, origin }`).
    /// Pressed → drag, released / typed-over / evicted → idle.
    /// Motion only mutates the selection through this state — there
    /// is no is-a-drag-happening boolean to desync from it.
    drag: SelectionDrag,
    /// Last modifier state seen on any key/button event — wheel
    /// events don't carry modifiers on the current madori pin.
    last_mods: Modifiers,
    /// Last pointer position in physical pixels — the wheel arm has
    /// no coordinates of its own (madori Scroll carries only deltas),
    /// so wheel reports use the tracked position (closes the
    /// documented fake-`1;1` wheel-coordinate gap).
    last_mouse_pos: (f64, f64),
    /// PTY-grid ⇄ display reconciler signature: (surface w, surface
    /// h, cell w bits, cell h bits) last pushed. Latching on render
    /// truth (never on what-was-pushed) is load-bearing: events
    /// dispatch before render, so event-derived bookkeeping is one
    /// frame ahead of measured metrics and ping-pongs the pane
    /// between old/new grids.
    grid_sync_sig: Option<(u32, u32, u32, u32)>,
    /// Last-seen [`Terminal::grid_generation`] — the search
    /// re-anchoring seam. A resize (rewrap or truncate) renumbers
    /// absolute grid rows, so the active search's match list goes
    /// stale; the per-tick reconciler re-runs the query when the
    /// generation moves (M2 review finding 2026-06-12).
    search_grid_gen: Option<u64>,
}

impl InputEngine {
    /// The ONLY constructor path. Wires the renderer's selection AND
    /// search AND dir-picker hooks internally so a consumer cannot
    /// forget half — the historical embedded-path bug (selection
    /// mutated but never handed to the renderer → silent invisible
    /// highlight) is unwritable by construction.
    #[must_use]
    pub fn attach_to_renderer(
        renderer: &mut TerminalRenderer,
        params: InputEngineParams,
    ) -> Self {
        renderer.set_selection(Arc::clone(&params.shared.selection));
        renderer.set_search(Arc::clone(&params.shared.search));
        renderer.set_dir_picker(Arc::clone(&params.shared.dir_picker));
        Self {
            terminal: params.terminal,
            pty: params.pty,
            resize: params.resize,
            selection: params.shared.selection,
            search: params.shared.search,
            dir_picker: params.shared.dir_picker,
            clipboard: params.clipboard,
            keybinds: params.keybinds,
            behavior: params.behavior,
            cursor_keys_mode: params.cursor_keys_mode,
            default_font_size: params.default_font_size,
            padding: params.padding,
            key_repeat_gate: awase::KeyRepeatGate::<Action>::new(),
            mouse_visible: true,
            last_click_time: Instant::now(),
            click_count: 0,
            last_click_pos: CellPos { row: 0, col: 0 },
            left_button_down: false,
            shift_drag_bypass: false,
            drag: SelectionDrag::Idle,
            last_mods: Modifiers::default(),
            last_mouse_pos: (0.0, 0.0),
            grid_sync_sig: None,
            search_grid_gen: None,
        }
    }

    /// Snapshot the visible grid rows + column count under a short
    /// read lock — the shape every lifted handler used: collect, drop
    /// the guard, operate on the copy.
    /// Rows the search scans: visible screen + up to this many of the
    /// most recent scrollback rows. Bounds the per-keystroke scan on
    /// unbounded-scrollback configs (~5k rows × 200 cols ≈ 1M cells,
    /// well under a frame at memchr speeds).
    const SEARCH_SCROLLBACK_CAP: usize = 5000;

    /// Search row source: (rows, cols, absolute index of rows[0]).
    fn search_rows(&self) -> (Vec<Vec<Cell>>, usize, usize) {
        let term = self.terminal.read();
        let (rows, first_abs) = term.search_rows(Self::SEARCH_SCROLLBACK_CAP);
        (rows, term.cols(), first_abs)
    }

    /// Bring the active search match into view: matches carry
    /// ABSOLUTE rows, so when the active one is outside the current
    /// viewport, retarget the scroll offset to put it a few rows
    /// below the top edge (search-reading posture). No-op when the
    /// match is already visible.
    fn scroll_to_active_match(&mut self) {
        let target_abs = {
            let st = self.search.lock().unwrap();
            match st.current_match() {
                Some(m) => m.row,
                None => return,
            }
        };
        let mut term = self.terminal.write();
        let sb_len = term.scrollback_total();
        let visible = term.rows();
        let offset = term.scroll_offset();
        // Viewport spans absolute rows [sb_len - offset, +visible).
        let top_abs = sb_len.saturating_sub(offset);
        if target_abs >= top_abs && target_abs < top_abs + visible {
            return; // already on screen
        }
        // Put the match 2 rows below the viewport top (clamped).
        let desired_top = target_abs.saturating_sub(2);
        let new_offset = sb_len.saturating_sub(desired_top).min(sb_len);
        let cur = term.scroll_offset();
        if new_offset > cur {
            term.scroll_up(new_offset - cur);
        } else {
            term.scroll_down(cur - new_offset);
        }
    }

    fn rows_snapshot(&self) -> (Vec<Vec<Cell>>, usize) {
        let term = self.terminal.read();
        let rows: Vec<_> = term.visible_rows().map(|r| r.to_vec()).collect();
        let cols = term.cols();
        (rows, cols)
    }

    // ── Key handling ─────────────────────────────────────────────

    /// One pressed key. The full pre-M1 pipeline, in lifted order:
    /// search-overlay routing → dir-picker routing → keybind action
    /// dispatch (storm-gated for font actions) → selection clear →
    /// kitty CSI-u → key→PTY-byte translation → bare-Cmd consume /
    /// unmapped ignore.
    pub fn on_key(&mut self, event: &KeyEvent, zoom: &mut dyn FontZoomTarget) -> EventOutcome {
        let hide_cursor = self.behavior.mouse_hide_while_typing && {
            let was_visible = self.mouse_visible;
            self.mouse_visible = false;
            was_visible
        };
        let vis = hide_cursor.then_some(false);

        let action = self.keybinds.lookup_madori(event);
        let mods = event.modifiers;
        self.last_mods = mods;
        let key = &event.key;
        let text = &event.text;

        // ── Search-overlay input routing ──────────────────────────
        // While the overlay is open every keystroke belongs to it:
        // Close/Next/Prev chords act on the overlay, unmodified text
        // edits the query, everything else is consumed so it can't
        // leak to the PTY.
        {
            let search_active = self.search.lock().unwrap().active;
            if search_active {
                match action {
                    Some(Action::SearchClose) => {
                        self.search.lock().unwrap().close();
                        return EventOutcome::consumed().with_cursor_visibility(vis);
                    }
                    Some(Action::SearchNext) => {
                        self.search.lock().unwrap().next();
                        self.scroll_to_active_match();
                        return EventOutcome::consumed().with_cursor_visibility(vis);
                    }
                    Some(Action::SearchPrev) => {
                        self.search.lock().unwrap().prev();
                        self.scroll_to_active_match();
                        return EventOutcome::consumed().with_cursor_visibility(vis);
                    }
                    _ => {}
                }
                // In search mode, typing updates the query (no modifiers).
                if !mods.ctrl && !mods.meta && !mods.alt {
                    if let Some(text) = text {
                        if !text.is_empty() {
                            let (rows, cols, first_abs) = self.search_rows();
                            self.search
                                .lock()
                                .unwrap()
                                .append_query(text, &rows, cols, first_abs);
                            self.scroll_to_active_match();
                            return EventOutcome::consumed().with_cursor_visibility(vis);
                        }
                    }
                    if matches!(key, KeyCode::Backspace) {
                        let (rows, cols, first_abs) = self.search_rows();
                        self.search
                            .lock()
                            .unwrap()
                            .backspace_query(&rows, cols, first_abs);
                        self.scroll_to_active_match();
                        return EventOutcome::consumed().with_cursor_visibility(vis);
                    }
                }
                return EventOutcome::consumed().with_cursor_visibility(vis);
            }
        }

        // ── Dir-picker mode input (reader-only frecency overlay 轍) ─
        // Mirrors the search-mode block above; keys are fully consumed
        // while open. mado only READS wadachi + injects `cd` on select.
        {
            let dp_open = self.dir_picker.lock().unwrap().open;
            if dp_open {
                if matches!(key, KeyCode::Escape) {
                    self.dir_picker.lock().unwrap().close();
                    return EventOutcome::consumed().with_cursor_visibility(vis);
                }
                if matches!(key, KeyCode::Enter) {
                    let path = self.dir_picker.lock().unwrap().selected_path().cloned();
                    if let Some(p) = path {
                        let cmd = format!(
                            "cd {}\n",
                            crate::dir_picker::shell_quote_path(&p.to_string_lossy())
                        );
                        self.write_key_input(cmd.as_bytes());
                    }
                    self.dir_picker.lock().unwrap().close();
                    return EventOutcome::consumed().with_cursor_visibility(vis);
                }
                if matches!(key, KeyCode::Up) {
                    self.dir_picker.lock().unwrap().move_up();
                    return EventOutcome::consumed().with_cursor_visibility(vis);
                }
                if matches!(key, KeyCode::Down) {
                    self.dir_picker.lock().unwrap().move_down();
                    return EventOutcome::consumed().with_cursor_visibility(vis);
                }
                if !mods.ctrl && !mods.meta && !mods.alt {
                    if matches!(key, KeyCode::Backspace) {
                        self.dir_picker.lock().unwrap().backspace();
                        return EventOutcome::consumed().with_cursor_visibility(vis);
                    }
                    if let Some(text) = text {
                        if !text.is_empty() {
                            self.dir_picker.lock().unwrap().push_str(text);
                            return EventOutcome::consumed().with_cursor_visibility(vis);
                        }
                    }
                }
                // Consume everything else while the overlay is open.
                return EventOutcome::consumed().with_cursor_visibility(vis);
            }
        }

        // ── Keybind action dispatch ───────────────────────────────
        if let Some(action) = action {
            // Font scaling: rate-limit + bound by type. The gate
            // drops OS key-repeat storms; the BoundedFontSize newtype
            // saturates at FONT_MAX so even a passed storm can't
            // explode the font onscreen.
            let scale_action = matches!(
                action,
                Action::FontIncrease | Action::FontDecrease | Action::FontReset
            );
            if scale_action && !self.key_repeat_gate.try_pass(action) {
                // Storm tick — drop silently. Consume so the
                // keystroke doesn't fall through to the PTY as
                // literal text.
                return EventOutcome::consumed().with_cursor_visibility(vis);
            }
            if let ActionOutcome::Consumed(out) = self.apply_action(action, zoom) {
                return out.with_cursor_visibility(vis);
            }
        }

        // Selection-clear moved into write_key_input: clearing on ANY
        // key event killed the selection on the bare Cmd/Shift press
        // winit synthesizes for every modifier — Cmd+C of a mouse
        // selection was structurally dead (hunt finding 2026-06-11).
        // Only keys that actually SEND BYTES clear (and snap) now.

        // Kitty keyboard protocol: if active, encode keys using the
        // protocol. The mirror Terminal tracks the mode stack from
        // the byte stream, so the gating is identical in both modes.
        {
            let kitty_flags = self.terminal.read().kitty_keyboard_flags();
            if kitty_flags > 0 {
                if let Some(encoded) =
                    crate::keybind::kitty_encode_key(key, text, &mods, kitty_flags)
                {
                    self.write_key_input(&encoded);
                    return EventOutcome::consumed().with_cursor_visibility(vis);
                }
            }
        }

        // Translate the key + text + modifiers to PTY bytes through
        // the shared helper in keybind.rs — single source of truth
        // for both render modes; the `embedded_tear_flow_ctrl_r_
        // reaches_pty` regression test pins the 2026-05-26 Ctrl-R
        // bug shut, and `ctrl_r_reaches_pty_through_engine_*` pins
        // the same flow through this engine.
        //
        // DECCKM (cursor-keys application mode) is queried through
        // the injected closure — the local path reads the mirror
        // Terminal, the tear path asks `pane_cursor_keys_mode` (errors
        // during shutdown race degrade to normal mode — the editor
        // still receives valid cursor keys).
        let app_mode = (self.cursor_keys_mode)();
        if let Some(bytes) = crate::keybind::madori_key_to_pty_bytes(key, text, mods, app_mode) {
            self.write_key_input(&bytes);
            return EventOutcome::consumed().with_cursor_visibility(vis);
        }
        // Helper returned None — bare Cmd shortcut (consume so the
        // bare letter doesn't leak to the PTY) or a truly unmapped
        // key (ignored so the OS can handle media keys / F13+ / etc.).
        let out = if mods.meta {
            EventOutcome::consumed()
        } else {
            EventOutcome::ignored()
        };
        out.with_cursor_visibility(vis)
    }

    /// Dispatch a resolved keybind [`Action`]. Shared by the physical
    /// keypress path ([`Self::on_key`]) and the kanshou-injected path
    /// (`simulate_chord` → `InjectedActions` drain) so an injected
    /// action exercises EXACTLY the dispatch a real chord hits — no
    /// parallel implementation to drift. The key-repeat gate is
    /// deliberately NOT in here — storms are an input-path concern;
    /// injected actions are deliberate.
    pub fn apply_action(&mut self, action: Action, zoom: &mut dyn FontZoomTarget) -> ActionOutcome {
        match action {
            Action::Copy => {
                if let Some(text) = self.selected_text() {
                    let _ = self.clipboard.copy_text(&text);
                }
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::Paste => {
                self.paste();
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::SearchOpen => {
                self.search.lock().unwrap().open();
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::DirPickerOpen => {
                self.dir_picker.lock().unwrap().open();
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            // Close/Next/Prev are handled ONLY while the overlay is
            // open: the fleet atlas binds search_close to bare Escape,
            // and consuming it on a closed overlay would eat Esc
            // before PTY forwarding and kill vim/helix/fzf (review
            // finding 2026-06-11). With the overlay closed they fall
            // through and the key reaches the PTY.
            Action::SearchClose | Action::SearchNext | Action::SearchPrev => {
                let mut st = self.search.lock().unwrap();
                if !st.active {
                    return ActionOutcome::FallThrough;
                }
                let close = matches!(action, Action::SearchClose);
                match action {
                    Action::SearchClose => st.close(),
                    Action::SearchNext => st.next(),
                    _ => st.prev(),
                }
                drop(st);
                if !close {
                    self.scroll_to_active_match();
                }
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            // Bare PageUp/PageDown are bound to these actions in the
            // default map. On the ALTERNATE screen a full-screen TUI
            // (less, vim) owns the viewport — scrollback navigation is
            // meaningless and the app needs ESC[5~/[6~, so fall
            // through to PTY key translation (review finding
            // 2026-06-11: the lift made the engine consume them in
            // both modes where pre-M1 neither shipped path did).
            Action::ScrollPageUp => {
                let mut term = self.terminal.write();
                if term.is_alternate_screen() {
                    return ActionOutcome::FallThrough;
                }
                let page = term.rows();
                term.scroll_up(page);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::ScrollPageDown => {
                let mut term = self.terminal.write();
                if term.is_alternate_screen() {
                    return ActionOutcome::FallThrough;
                }
                let page = term.rows();
                term.scroll_down(page);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            // Font scaling — BoundedFontSize saturates at FONT_MIN /
            // FONT_MAX (the raw `+ 1.0` this replaced was the
            // 2026-05-21 runaway-font class). The pane-grid push is
            // left to the per-redraw reconciler: cell metrics
            // re-measure on the NEXT rendered frame, so any grid
            // computed here would use stale metrics.
            Action::FontIncrease => {
                let new_size = BoundedFontSize::new(zoom.font_size()).inc_step().get();
                zoom.set_font_size(new_size);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::FontDecrease => {
                let new_size = BoundedFontSize::new(zoom.font_size()).dec_step().get();
                zoom.set_font_size(new_size);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::FontReset => {
                let reset_size = BoundedFontSize::new(self.default_font_size).get();
                zoom.set_font_size(reset_size);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::ScrollToTop => {
                self.terminal.write().scroll_to_top();
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::ScrollToBottom => {
                self.terminal.write().scroll_to_bottom();
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::ResetTerminal => {
                self.terminal.write().reset();
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::ToggleFullscreen => ActionOutcome::Consumed(EventOutcome {
                consumed: true,
                toggle_fullscreen: true,
                ..Default::default()
            }),
            Action::ScrollUp | Action::ScrollDown => {
                // These are handled by scroll wheel events.
                ActionOutcome::FallThrough
            }
            // The arms below executed WITHOUT consuming in the pre-M1
            // local loop (no `return` in the lifted match) — the key
            // continues to selection-clear → kitty → PTY translation.
            // Preserved verbatim; consume-semantics review is an M2
            // cleanup candidate.
            Action::PasteFromSelection => {
                // Same PasteGuard + bracketed framing as paste() —
                // the pre-M1 local arm wrote raw clipboard bytes,
                // which re-opened the ESC[201~ injection the guard
                // exists for the moment a custom bind made this
                // reachable in tear mode (review finding 2026-06-11).
                let pasted = self.clipboard.paste_text().ok();
                if let Some(text) = pasted {
                    self.write_paste(&text);
                }
                ActionOutcome::FallThrough
            }
            Action::JumpToPrompt | Action::JumpToPromptPrev => {
                // Consumed: pre-M1 the fall-through was harmless
                // (meta-only chords translate to None), but under the
                // kitty protocol the chord encoded as a meta-arrow,
                // leaked to the app, AND write_key_input's snap undid
                // the jump (hunt finding 2026-06-11).
                // Scroll to the previous OSC 133 A mark (ghostty's
                // canonical Cmd-Up binding).
                let mut term = self.terminal.write();
                if let Some(target) = term.scroll_offset_to_prev_prompt() {
                    let delta = target.saturating_sub(term.scroll_offset());
                    if delta > 0 {
                        term.scroll_up(delta);
                    } else {
                        let back = term.scroll_offset().saturating_sub(target);
                        term.scroll_down(back);
                    }
                    tracing::trace!(target, "jumped to prev prompt");
                } else {
                    tracing::debug!("no prompt marks above viewport");
                }
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::JumpToPromptNext => {
                // Scroll to the next OSC 133 A mark forward from the
                // current view top (ghostty's Cmd-Down).
                let mut term = self.terminal.write();
                if let Some(target) = term.scroll_offset_to_next_prompt() {
                    let cur = term.scroll_offset();
                    if target < cur {
                        term.scroll_down(cur - target);
                    } else if target > cur {
                        term.scroll_up(target - cur);
                    }
                    tracing::trace!(target, "jumped to next prompt");
                } else {
                    tracing::debug!("no prompt marks below viewport");
                }
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::ClearScreen => {
                // Operator input — snaps to the live tail so the
                // redraw is visible even when scrolled into history.
                self.write_key_input(b"\x0c"); // Ctrl+L
                ActionOutcome::FallThrough
            }
            Action::SelectAll => {
                // Consumed: the chord IS the action. The pre-M1
                // FallThrough + clear-on-keypress combination made a
                // bound select_all a guaranteed silent no-op (hunt
                // finding 2026-06-11).
                let span = {
                    let term = self.terminal.read();
                    let last_row = term.rows().saturating_sub(1);
                    let last_col = term.cols().saturating_sub(1);
                    term.selection_anchor_at(0, 0)
                        .zip(term.selection_anchor_at(last_row, last_col))
                };
                if let Some((a, b)) = span {
                    self.selection.lock().unwrap().set_span(a, b);
                }
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::CopyUrlToClipboard => {
                let (row_cells, cols, cursor_row) = {
                    let term = self.terminal.read();
                    let cols = term.cols();
                    let cursor_row = term.cursor().row;
                    let row_cells: Vec<_> =
                        (0..cols).map(|c| term.cell(cursor_row, c).clone()).collect();
                    (row_cells, cols, cursor_row)
                };
                let urls = crate::url::detect_urls_in_row(&row_cells, cols, cursor_row);
                if let Some(url) = urls.first() {
                    let _ = self.clipboard.copy_text(&url.url);
                }
                ActionOutcome::FallThrough
            }
            Action::ToggleMouseReporting => {
                tracing::info!("mouse reporting toggled");
                ActionOutcome::FallThrough
            }
        }
    }

    /// Paste the system clipboard into the PTY through the M0
    /// PasteGuard (`clipboard_store::sanitize_paste`): strips bytes
    /// that would break out of (or fake) bracketed-paste framing —
    /// without this, a clipboard containing ESC[201~ executes the
    /// rest of the paste as keystrokes (classic paste injection).
    fn paste(&mut self) {
        let pasted_text = self.clipboard.paste_text().ok();
        if let Some(pasted) = pasted_text {
            self.write_paste(&pasted);
        }
    }

    /// Deliver OPERATOR INPUT bytes to the PTY: snap the viewport to
    /// the bottom first (you type, you're back at the prompt — the
    /// kitty/ghostty contract), then write. Output never moves the
    /// view (terminal.rs pins it to content); this is the ONE place
    /// the view returns to the live tail. Mouse reports and VT query
    /// answers deliberately do NOT route through here — they are not
    /// the operator asking to look at the prompt.
    fn write_key_input(&mut self, bytes: &[u8]) {
        // Typing replaces the selection (the bytes will change the
        // grid under it) and — on the PRIMARY screen — snaps the view
        // to the live tail. In a full-screen TUI the offset belongs
        // to the saved primary viewport; zeroing it there would
        // discard the operator's reading position for no reason
        // ('q' out of a pager used to land them at the prompt bottom
        // instead of where they were).
        // Typing also cancels any live drag — motion after the clear
        // must not resurrect the dead selection.
        self.drag = SelectionDrag::Idle;
        self.selection.lock().unwrap().clear();
        if !self.terminal.read().is_alternate_screen() {
            self.terminal.write().scroll_to_bottom();
        }
        self.pty.write(bytes);
    }

    /// The ONE guarded paste write: PasteGuard sanitization +
    /// bracketed framing. Every path that delivers clipboard bytes
    /// to the PTY routes through here.
    fn write_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self.terminal.read().bracketed_paste();
        let safe = crate::clipboard_store::sanitize_paste(text, bracketed);
        if safe.is_empty() {
            return;
        }
        // Paste is operator input — snap to the live tail like a
        // keystroke would.
        self.terminal.write().scroll_to_bottom();
        if bracketed {
            self.pty.write(b"\x1b[200~");
        }
        self.pty.write(&safe);
        if bracketed {
            self.pty.write(b"\x1b[201~");
        }
    }

    // ── Mouse handling ───────────────────────────────────────────

    /// Pixel coords → clamped 0-based cell coords, plus the terminal
    /// mouse state — the prologue every mouse arm shared. Mouse
    /// coords are PHYSICAL pixels; padding is a logical config value
    /// — scale before subtracting or clicks land up to half a cell
    /// off on HiDPI.
    fn mouse_cell(
        &self,
        x: f64,
        y: f64,
        metrics: &dyn FontZoomTarget,
    ) -> (usize, usize, MouseMode, bool) {
        let cw = metrics.cell_width();
        let ch = metrics.cell_height();
        let pad_phys = self.padding * metrics.scale_factor();
        let col = ((x as f32 - pad_phys) / cw).max(0.0) as usize;
        let row = ((y as f32 - pad_phys) / ch).max(0.0) as usize;
        let (mouse_mode, sgr, term_cols, term_rows) = {
            let term = self.terminal.read();
            (term.mouse_mode(), term.sgr_mouse(), term.cols(), term.rows())
        };
        (
            col.min(term_cols.saturating_sub(1)),
            row.min(term_rows.saturating_sub(1)),
            mouse_mode,
            sgr,
        )
    }

    /// Mouse button press/release: PTY forwarding when the app
    /// tracks the mouse (ALL buttons + modifier bits — M1 closes the
    /// pre-M1 left-only gap), else selection (click / double-click
    /// word / triple-click line, copy-on-select, Cmd+click URL open).
    pub fn on_mouse_button(
        &mut self,
        button: MouseButton,
        pressed: bool,
        x: f64,
        y: f64,
        modifiers: Modifiers,
        metrics: &dyn FontZoomTarget,
    ) -> EventOutcome {
        self.last_mouse_pos = (x, y);
        self.last_mods = modifiers;
        let (col, row, mouse_mode, sgr) = self.mouse_cell(x, y, metrics);

        if button == MouseButton::Left {
            self.left_button_down = pressed;
            if pressed {
                self.shift_drag_bypass = modifiers.shift
                    && !self.behavior.mouse_shift_capture
                    && mouse_mode != MouseMode::Off;
            }
        }
        let bypass_release =
            button == MouseButton::Left && !pressed && self.shift_drag_bypass;
        if bypass_release {
            // The release belongs to the bypassed drag: finish the
            // selection below (tracking-off path), then end the
            // bypass.
            self.shift_drag_bypass = false;
        }

        // Forward mouse events to the PTY via the typed MouseReport
        // emitter when tracking is active. Pre-M1 both loops forwarded
        // Left only; the unified engine forwards left/middle/right
        // with real modifier bits (Shift 4 / Meta 8 / Ctrl 16).
        //
        // Shift is the operator's escape hatch (xterm/kitty/ghostty
        // convention): shift+click bypasses tracking and selects
        // text terminal-side — without it, selection is impossible
        // inside vim/tmux/htop (hunt finding 2026-06-11).
        let shift_bypass = modifiers.shift && !self.behavior.mouse_shift_capture;
        if mouse_mode != MouseMode::Off && !shift_bypass && !bypass_release {
            let report = MouseReport {
                kind: if pressed {
                    MouseReportKind::Press
                } else {
                    MouseReportKind::Release
                },
                button: match button {
                    MouseButton::Left => MouseReportButton::Left,
                    MouseButton::Middle => MouseReportButton::Middle,
                    MouseButton::Right => MouseReportButton::Right,
                },
                col: col + 1,
                row: row + 1,
                mods: MouseMods::from(modifiers),
            };
            self.pty.write(&report.encode(sgr));
            return EventOutcome::consumed();
        }

        // Middle-click pastes (kitty wires it to paste-from-selection
        // on every platform; macOS has no primary selection so the
        // clipboard is the source). Same PasteGuard path as the
        // keyboard action.
        if button == MouseButton::Middle {
            if pressed {
                let pasted = self.clipboard.paste_text().ok();
                if let Some(text) = pasted {
                    self.write_paste(&text);
                }
            }
            return EventOutcome::consumed();
        }

        // Text selection via left mouse button. Anchors are captured
        // at gesture time (here) and resolved at use time (render /
        // extract) — the selection survives streaming output and
        // rewrap because it names content, not viewport rows.
        if button == MouseButton::Left {
            if pressed {
                // Shift+click EXTENDS an existing selection to the
                // click point (xterm/kitty/ghostty convention) and
                // keeps dragging from the surviving start anchor.
                // No existing selection → nothing to extend — fall
                // through to the plain click path below.
                let existing = self.selection.lock().unwrap().anchors();
                if let (true, Some((start, _))) = (modifiers.shift, existing) {
                    if let Some(click) =
                        self.terminal.read().selection_anchor_at(row, col)
                    {
                        self.selection.lock().unwrap().set_span(start, click);
                        self.drag = SelectionDrag::Drag {
                            mode: DragMode::Char,
                            origin: (start, start),
                        };
                    }
                    // An extension is not a multi-click: leave the
                    // cadence state (click_count / last_click_*)
                    // untouched.
                    return EventOutcome::consumed();
                }

                let now = Instant::now();
                let same_pos =
                    self.last_click_pos.row == row && self.last_click_pos.col == col;
                let quick = now.duration_since(self.last_click_time).as_millis() < 400;

                if same_pos && quick {
                    self.click_count = (self.click_count + 1).min(3);
                } else {
                    self.click_count = 1;
                }
                self.last_click_time = now;
                self.last_click_pos = CellPos { row, col };

                match self.click_count {
                    2 => {
                        // Double-click: select word; the snapped word
                        // is the drag origin every word-drag union
                        // keeps fully selected.
                        if let Some(span) = self.word_span_at(row, col) {
                            self.selection.lock().unwrap().set_span(span.0, span.1);
                            self.drag = SelectionDrag::Drag {
                                mode: DragMode::Word,
                                origin: span,
                            };
                        }
                    }
                    3 => {
                        // Triple-click: select entire (physical) line.
                        if let Some(span) = self.line_span_at(row) {
                            self.selection.lock().unwrap().set_span(span.0, span.1);
                            self.drag = SelectionDrag::Drag {
                                mode: DragMode::Line,
                                origin: span,
                            };
                        }
                    }
                    _ => {
                        if let Some(a) = self.terminal.read().selection_anchor_at(row, col)
                        {
                            self.selection.lock().unwrap().start(a);
                            self.drag = SelectionDrag::Drag {
                                mode: DragMode::Char,
                                origin: (a, a),
                            };
                        }
                    }
                }
            } else {
                // Release ends the drag FSM unconditionally.
                self.drag = SelectionDrag::Idle;
                if self.click_count == 1 {
                    self.selection.lock().unwrap().finish();
                }
                // Muscle-memory contract: the highlight goes straight
                // to the clipboard on release for EVERY selection
                // shape — drag, double-click word, triple-click line
                // (word/line used to be excluded by the click_count
                // gate).
                let release_copy = self
                    .behavior
                    .copy_on_select
                    .then(|| self.selected_text())
                    .flatten();
                if let Some(text) = release_copy {
                    let _ = self.clipboard.copy_text(&text);
                }
                if self.click_count == 1 {
                    // Cmd+click (macOS) / Ctrl+click (Linux) to open
                    // URLs — single-click release only, never
                    // word/line selection.
                    if modifiers.meta || modifiers.ctrl {
                        let (row_cells, cols) = self.rows_snapshot();
                        let detected = crate::url::detect_urls(&row_cells, cols);
                        if let Some(url) = crate::url::url_at(&detected, row, col) {
                            if let Err(e) = open::that(&url.url) {
                                tracing::warn!(
                                    error = %e,
                                    url = %url.url,
                                    "failed to open URL"
                                );
                            }
                        }
                    }
                }
            }
        }
        EventOutcome::consumed()
    }

    // ── Selection helpers (anchor capture + drag FSM) ────────────

    /// Extract the active selection's text through the soft-wrap-
    /// aware content walk. The ONE copy surface — `Action::Copy` and
    /// `copy_on_select` both route here.
    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection.lock().unwrap().anchors()?;
        self.terminal.read().extract_selection_text(a, b)
    }

    /// Capture anchors for both endpoints of a viewport span.
    fn capture_span(
        &self,
        start: CellPos,
        end: CellPos,
    ) -> Option<(SelectionAnchor, SelectionAnchor)> {
        let term = self.terminal.read();
        term.selection_anchor_at(start.row, start.col)
            .zip(term.selection_anchor_at(end.row, end.col))
    }

    /// The anchored span of the word under a viewport cell.
    fn word_span_at(
        &self,
        row: usize,
        col: usize,
    ) -> Option<(SelectionAnchor, SelectionAnchor)> {
        let (rows, cols_count) = self.rows_snapshot();
        let (c0, c1) = crate::selection::word_bounds_in_row(
            CellPos { row, col },
            &rows,
            cols_count,
            "",
        );
        self.capture_span(CellPos { row, col: c0 }, CellPos { row, col: c1 })
    }

    /// The anchored span of the full physical row under a viewport row.
    fn line_span_at(&self, row: usize) -> Option<(SelectionAnchor, SelectionAnchor)> {
        let cols_count = self.terminal.read().cols();
        self.capture_span(
            CellPos { row, col: 0 },
            CellPos {
                row,
                col: cols_count.saturating_sub(1),
            },
        )
    }

    /// One motion tick of the live drag. Char drags move the end
    /// anchor; word/line drags snap the pointer to its unit and
    /// re-span to the UNION of the origin unit and the pointer unit
    /// (both ends snapped — the kitty/ghostty extend contract).
    fn update_drag(
        &mut self,
        mode: DragMode,
        origin: (SelectionAnchor, SelectionAnchor),
        row: usize,
        col: usize,
    ) {
        match mode {
            DragMode::Char => {
                if let Some(end) = self.terminal.read().selection_anchor_at(row, col) {
                    self.selection.lock().unwrap().update(end);
                }
            }
            DragMode::Word => {
                if let Some(pointer) = self.word_span_at(row, col) {
                    self.extend_to_union(origin, pointer);
                }
            }
            DragMode::Line => {
                if let Some(pointer) = self.line_span_at(row) {
                    self.extend_to_union(origin, pointer);
                }
            }
        }
    }

    /// Re-span the selection to cover both the origin unit and the
    /// pointer unit. Anchor order is unknowable without resolution,
    /// so all four endpoints resolve against the current grid and
    /// the extremes win; an unresolvable endpoint (content evicted
    /// mid-drag) skips the tick — the reconciler decides whether the
    /// selection as a whole is dead.
    fn extend_to_union(
        &mut self,
        origin: (SelectionAnchor, SelectionAnchor),
        pointer: (SelectionAnchor, SelectionAnchor),
    ) {
        let resolved = {
            let term = self.terminal.read();
            [origin.0, origin.1, pointer.0, pointer.1]
                .into_iter()
                .map(|a| term.resolve_selection_anchor(a).map(|pos| (pos, a)))
                .collect::<Option<Vec<_>>>()
        };
        let Some(cands) = resolved else { return };
        let start = cands.iter().min_by_key(|(pos, _)| *pos).expect("4 candidates").1;
        let end = cands.iter().max_by_key(|(pos, _)| *pos).expect("4 candidates").1;
        self.selection.lock().unwrap().set_span(start, end);
    }

    /// Pointer motion: PTY motion forwarding under 1002/1003, else
    /// selection drag update. Restores the pointer hidden by
    /// `mouse_hide_while_typing`.
    pub fn on_mouse_moved(&mut self, x: f64, y: f64, metrics: &dyn FontZoomTarget) -> EventOutcome {
        self.last_mouse_pos = (x, y);
        let was_hidden = !self.mouse_visible;
        self.mouse_visible = true;
        let show_cursor = self.behavior.mouse_hide_while_typing && was_hidden;
        let vis = show_cursor.then_some(true);

        let (col, row, mouse_mode, sgr) = self.mouse_cell(x, y, metrics);

        // Forward motion with the spec-correct code: 1002
        // (ButtonEvent) only while a button is held; 1003 (AnyEvent)
        // always, hover motion carrying the no-button code (35),
        // never a fabricated left-drag (32).
        if matches!(mouse_mode, MouseMode::ButtonEvent | MouseMode::AnyEvent)
            && !self.shift_drag_bypass
        {
            let forward = self.left_button_down || mouse_mode == MouseMode::AnyEvent;
            if forward && sgr {
                let report = MouseReport {
                    kind: MouseReportKind::Motion,
                    button: if self.left_button_down {
                        MouseReportButton::Left
                    } else {
                        MouseReportButton::None
                    },
                    col: col + 1,
                    row: row + 1,
                    mods: MouseMods::NONE,
                };
                self.pty.write(&report.encode(true));
            }
            return EventOutcome::consumed().with_cursor_visibility(vis);
        }

        // Update text selection if a drag is live — gated by the
        // drag FSM, not by selection liveness (a committed shift-
        // extended span is `Selected` yet still dragging).
        if let SelectionDrag::Drag { mode, origin } = self.drag {
            self.update_drag(mode, origin, row, col);
        }
        EventOutcome::consumed().with_cursor_visibility(vis)
    }

    /// Wheel / two-finger scroll: PTY wheel-button forwarding when
    /// the app tracks the mouse — with TRUE cell coords from the
    /// tracked pointer position (pre-M1 both loops sent a fake
    /// `1;1`) — else the mado-side scrollback view scrolls.
    pub fn on_mouse_scroll(&mut self, dy: f64, metrics: &dyn FontZoomTarget) -> EventOutcome {
        // Write lock up front — mode check and scroll mutation stay
        // atomic (lifted lock shape; drop before PTY forwarding).
        let mut term = self.terminal.write();
        let mouse_mode = term.mouse_mode();
        let sgr = term.sgr_mouse();

        // Shift+wheel bypasses tracking and scrolls mado's scrollback
        // (xterm/kitty/ghostty convention) — without it, scrollback
        // is unreachable inside tmux/vim/htop. Wheel events carry no
        // modifiers on the current madori pin; `last_mods` is fed by
        // every key/button event, and pressing Shift itself emits a
        // key event, so the cache is current by the time the wheel
        // turns. (madori@972f296 adds Scroll.modifiers — switch when
        // the shikumi pin unifies.)
        if mouse_mode != MouseMode::Off
            && !(self.last_mods.shift && !self.behavior.mouse_shift_capture)
        {
            let term_cols = term.cols();
            let term_rows = term.rows();
            drop(term);
            let cw = metrics.cell_width();
            let ch = metrics.cell_height();
            let pad_phys = self.padding * metrics.scale_factor();
            let col = ((self.last_mouse_pos.0 as f32 - pad_phys) / cw).max(0.0) as usize;
            let row = ((self.last_mouse_pos.1 as f32 - pad_phys) / ch).max(0.0) as usize;
            let report = MouseReport {
                kind: MouseReportKind::Press,
                button: if dy > 0.0 {
                    MouseReportButton::WheelUp
                } else {
                    MouseReportButton::WheelDown
                },
                col: col.min(term_cols.saturating_sub(1)) + 1,
                row: row.min(term_rows.saturating_sub(1)) + 1,
                mods: MouseMods::NONE,
            };
            self.pty.write(&report.encode(sgr));
            return EventOutcome::consumed();
        }

        let lines = (dy as isize).unsigned_abs().max(1)
            * (self.behavior.mouse_scroll_multiplier as usize).max(1);

        // Alternate-scroll (xterm mode 1007 semantics, default-on like
        // kitty/ghostty): a full-screen TUI without mouse tracking has
        // no scrollback to scroll — the wheel maps to arrow keys so
        // less/vim/man scroll their CONTENT instead of the viewport
        // no-op'ing. DECCKM picks the encoding the app negotiated.
        if term.is_alternate_screen() {
            drop(term);
            let app_mode = (self.cursor_keys_mode)();
            let seq: &[u8] = match (dy > 0.0, app_mode) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut out = Vec::with_capacity(seq.len() * lines);
            for _ in 0..lines {
                out.extend_from_slice(seq);
            }
            self.pty.write(&out);
            return EventOutcome::consumed();
        }

        if dy > 0.0 {
            term.scroll_up(lines);
        } else {
            term.scroll_down(lines);
        }
        EventOutcome::consumed()
    }

    // ── IME / focus ──────────────────────────────────────────────

    /// IME commit — forward composed text (CJK, dead-key accents,
    /// emoji picker) to the PTY.
    pub fn on_ime_commit(&mut self, text: &str) -> EventOutcome {
        if !text.is_empty() {
            self.write_key_input(text.as_bytes());
        }
        EventOutcome::consumed()
    }

    /// Focus events (mode 1004) — emit ESC[I / ESC[O when the app
    /// enabled focus reporting (nvim autoread, tmux-style dim).
    pub fn on_focus(&mut self, focused: bool) -> EventOutcome {
        if self.terminal.read().focus_reporting() {
            let report: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
            self.pty.write(report);
        }
        EventOutcome::consumed()
    }

    // ── Grid reconciliation ──────────────────────────────────────

    /// Window resize event (local-PTY adapter only — the tear
    /// adapter deliberately ignores Resized and lets the per-tick
    /// reconciler converge on rendered truth one frame later;
    /// pushing event dims raced the renderer's one-frame lag and
    /// ping-ponged tear between old and new grids, review finding
    /// 2026-06-11). Records the signature so the reconciler doesn't
    /// re-fire the same dims one frame later (a duplicate
    /// Terminal::resize used to reset the app's scroll region;
    /// Terminal::resize is also same-dims-no-op now as the deeper
    /// guard).
    pub fn on_resize(&mut self, _width: u32, _height: u32, _renderer: &TerminalRenderer) -> EventOutcome {
        // No push here: the reconciler (on_redraw_tick) converges on
        // RENDERED truth one frame later. madori dispatches events
        // BEFORE rendering, so the renderer's surface dims lag the
        // Resized event by one frame — latching event dims here made
        // the next tick re-push the STALE pre-resize grid (new → old
        // → new ping-pong, review finding 2026-06-11; same class as
        // the pre-M1 tear-path bug).
        EventOutcome::consumed()
    }

    /// PTY-grid ⇄ display reconciler — the per-tick latch. The
    /// pre-window estimate can't know measured font metrics or the
    /// Flush-titlebar content inset, and macOS sends no initial
    /// Resized — converge the pane grid on display truth whenever
    /// surface dims or cell metrics change (covers startup AND the
    /// one-frame metric lag after font zoom). Latches on the
    /// RENDERED surface signature; resizes BOTH halves: mado's
    /// mirror VT grid (wrap math, CPR/XTWINOPS answers, mouse
    /// clamps) and the pane's PTY through the [`ResizeSink`].
    pub fn on_redraw_tick(&mut self, renderer: &TerminalRenderer) {
        if let Some((w, h)) = renderer.last_surface_size() {
            let cw = renderer.cell_width();
            let ch = renderer.cell_height();
            let sig = (w, h, cw.to_bits(), ch.to_bits());
            if self.grid_sync_sig != Some(sig) {
                self.grid_sync_sig = Some(sig);
                self.push_grid(renderer, w, h);
            }
        }
        // AFTER any grid push: re-anchor the active search if the
        // grid geometry moved this tick (the push above bumps the
        // generation synchronously, so the matches converge within
        // the same tick).
        self.reconcile_search();
        self.reconcile_selection();
    }

    /// Selection ⇄ content reconciler: anchors whose content is gone
    /// (logical line evicted from scrollback, RIS grid rebuild,
    /// other screen buffer active) resolve to `None` on every read
    /// path already; this seam collapses the STATE too, so
    /// `is_active()` never advertises a selection that can neither
    /// render nor extract. Resolve-at-use, clear-on-dangle — an
    /// anchored-to-nothing selection is unrepresentable in effect
    /// (tier: parse-time-rejected at resolution + reconciled state;
    /// the dangling window between eviction and the next tick is
    /// unreadable, not absent).
    fn reconcile_selection(&mut self) {
        let Some((a, b)) = self.selection.lock().unwrap().anchors() else {
            return;
        };
        if self.terminal.read().resolve_selection_span(a, b).is_none() {
            self.selection.lock().unwrap().clear();
            self.drag = SelectionDrag::Idle;
        }
    }

    /// Search ⇄ grid-geometry reconciler: matches carry ABSOLUTE
    /// grid rows, and a resize (rewrap renumbers rows wholesale;
    /// truncate shifts them too) leaves them pointing at different
    /// content. Re-run the live query against fresh rows whenever
    /// [`Terminal::grid_generation`] moves while the overlay is
    /// open. Query edits already recompute on their own; this seam
    /// covers geometry changes BETWEEN edits.
    fn reconcile_search(&mut self) {
        let generation = self.terminal.read().grid_generation();
        if self.search_grid_gen == Some(generation) {
            return;
        }
        self.search_grid_gen = Some(generation);
        let needs_rerun = {
            let st = self.search.lock().unwrap();
            st.active && !st.query.is_empty()
        };
        if !needs_rerun {
            return;
        }
        let (rows, cols, first_abs) = self.search_rows();
        let mut st = self.search.lock().unwrap();
        let query = st.query.clone();
        st.set_query(&query, &rows, cols, first_abs);
    }

    /// Resize both halves from physical surface dims: the mirror
    /// Terminal first (so CPR/XTWINOPS answers match), then the
    /// sink. Cell math goes through the renderer's
    /// `cells_for_window_phys` — ONE source of truth for the grid in
    /// both modes (the pre-M1 local loop subtracted UNSCALED logical
    /// padding here; converged on the physical-padding math the
    /// renderer actually draws with).
    pub(crate) fn push_grid(&mut self, renderer: &TerminalRenderer, width: u32, height: u32) {
        let (cols, rows) = renderer.cells_for_window_phys(width, height);
        self.terminal.write().resize(cols as usize, rows as usize);
        self.resize.resize(cols, rows);
    }
}

#[cfg(test)]
mod tests {
    //! Capability-parity headless harness (REMEDIATION-PLAN §M1
    //! acceptance): ONE harness drives the InputEngine with a
    //! recording [`PtySink`] + a seeded mirror Terminal and asserts
    //! copy / paste (bracketed + PasteGuard) / search
    //! open-query-next-close / mouse SGR all buttons + wheel +
    //! drag/hover motion codes / kitty-encoded key / plain key —
    //! with NO reference to render mode. The renderer is a real
    //! `TerminalRenderer` with no GPU device (same harness shape as
    //! `render::tests::gpu_free_renderer`); the clipboard is
    //! `hasami::MockClipboard` so tests never touch the system
    //! clipboard.

    use std::sync::Mutex as StdMutex;

    use hasami::MockClipboard;
    use parking_lot::RwLock;
    use tear_types::{
        ControlError, ControlResult, Direction, InputPolicy, MultiplexerControl, PaneId,
        SessionId, SessionSource, TearPane, TearSession, TearWindow, WindowId,
    };

    use super::*;
    use crate::config::CursorStyle;
    use crate::terminal::{Color as TermColor, Terminal};

    /// Minimal `MultiplexerControl` mock (ported from the pre-M1
    /// `gui_tear_attach` seam tests): records every `send_keys`
    /// call; every other operation is rejected. Backs the
    /// "PtySink over a mock MultiplexerControl" sink configuration —
    /// proving engine outputs reach a tear-shaped control plane
    /// byte-identically to the closure sink.
    struct RecordingControl {
        sent: StdMutex<Vec<(PaneId, Vec<u8>)>>,
    }

    impl RecordingControl {
        fn new() -> Self {
            Self {
                sent: StdMutex::new(Vec::new()),
            }
        }
    }

    impl MultiplexerControl for RecordingControl {
        fn list_sessions(&self) -> ControlResult<Vec<TearSession>> {
            Ok(Vec::new())
        }
        fn get_session(&self, id: SessionId) -> ControlResult<TearSession> {
            Err(ControlError::NoSuchSession(id))
        }
        fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)> {
            Err(ControlError::NoSuchWindow(id))
        }
        fn get_pane(&self, id: PaneId) -> ControlResult<TearPane> {
            Err(ControlError::NoSuchPane(id))
        }
        fn new_session_with_source_and_size(
            &self,
            _name: &str,
            _shell: &str,
            _source: SessionSource,
            _size_cells: (u16, u16),
        ) -> ControlResult<SessionId> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn rename_session(&self, _id: SessionId, _new_name: &str) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn kill_session(&self, _id: SessionId) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn new_window(
            &self,
            _session: SessionId,
            _name: &str,
            _shell: &str,
        ) -> ControlResult<WindowId> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn kill_window(&self, _id: WindowId) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn select_window(&self, _id: WindowId) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn split_pane(
            &self,
            _origin: PaneId,
            _direction: Direction,
            _shell: &str,
        ) -> ControlResult<PaneId> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn kill_pane(&self, _id: PaneId) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn select_pane(&self, _id: PaneId) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn resize_pane(
            &self,
            _id: PaneId,
            _direction: Direction,
            _delta_cells: i16,
        ) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
        fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()> {
            self.sent
                .lock()
                .expect("recording mutex poisoned")
                .push((id, bytes.to_vec()));
            Ok(())
        }
        fn pane_subscriber_count(&self, _id: PaneId) -> ControlResult<u32> {
            Ok(0)
        }
        fn set_input_policy(&self, _id: PaneId, _policy: InputPolicy) -> ControlResult<()> {
            Err(ControlError::Rejected("mock".into()))
        }
    }

    /// Which sink shape backs the engine — the two production
    /// configurations: a plain closure (local-PTY shape) and a
    /// mock-MultiplexerControl wrapper (tear shape).
    enum SinkKind {
        Closure,
        Control,
    }

    struct Harness {
        renderer: TerminalRenderer,
        engine: InputEngine,
        terminal: SharedTerminal,
        /// Bytes the PtySink received (both sink kinds record here;
        /// the Control kind ALSO records into `control.sent`).
        sent: Arc<StdMutex<Vec<Vec<u8>>>>,
        resized: Arc<StdMutex<Vec<(u16, u16)>>>,
        clipboard: Arc<MockClipboard>,
        control: Arc<RecordingControl>,
        pane: PaneId,
    }

    impl Harness {
        fn new(kind: SinkKind) -> Self {
            let terminal: SharedTerminal =
                Arc::new(RwLock::new(Terminal::new(80, 24)));
            let mut renderer = TerminalRenderer::new(
                Arc::clone(&terminal),
                14.0,
                "monospace".into(),
                "monospace".into(),
                "monospace".into(),
                0.0,
                CursorStyle::Block,
                false,
                500,
                wgpu::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                TermColor::WHITE,
            );
            let sent: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));
            let resized: Arc<StdMutex<Vec<(u16, u16)>>> =
                Arc::new(StdMutex::new(Vec::new()));
            let clipboard = Arc::new(MockClipboard::new());
            let control = Arc::new(RecordingControl::new());
            let pane = PaneId::from_seed("input-engine-test");
            let pty: Box<dyn PtySink> = match kind {
                SinkKind::Closure => {
                    let sent = Arc::clone(&sent);
                    Box::new(move |bytes: &[u8]| {
                        sent.lock().unwrap().push(bytes.to_vec());
                    })
                }
                SinkKind::Control => {
                    let sent = Arc::clone(&sent);
                    let control = Arc::clone(&control);
                    Box::new(move |bytes: &[u8]| {
                        let _ = control.send_keys(pane, bytes);
                        sent.lock().unwrap().push(bytes.to_vec());
                    })
                }
            };
            let resize: Box<dyn ResizeSink> = {
                let resized = Arc::clone(&resized);
                Box::new(move |c: u16, r: u16| resized.lock().unwrap().push((c, r)))
            };
            let cursor_keys_mode: Box<dyn Fn() -> bool + Send + Sync> = {
                let term = Arc::clone(&terminal);
                Box::new(move || term.read().cursor_keys_mode())
            };
            let engine = InputEngine::attach_to_renderer(
                &mut renderer,
                InputEngineParams {
                    terminal: Arc::clone(&terminal),
                    pty,
                    resize,
                    shared: SharedUxState::fresh(),
                    clipboard: clipboard.clone(),
                    keybinds: KeybindManager::with_mado_defaults(),
                    behavior: UxBehavior {
                        copy_on_select: true,
                        confirm_close: false,
                        mouse_hide_while_typing: false,
                        mouse_scroll_multiplier: 1,
                        mouse_shift_capture: false,
                    },
                    cursor_keys_mode,
                    default_font_size: 14.0,
                    padding: 0.0,
                },
            );
            Self {
                renderer,
                engine,
                terminal,
                sent,
                resized,
                clipboard,
                control,
                pane,
            }
        }

        fn clear_sent(&self) {
            self.sent.lock().unwrap().clear();
        }

        fn feed(&self, bytes: &[u8]) {
            self.terminal.write().feed(bytes);
        }

        fn key(&mut self, key: KeyCode, text: Option<&str>, modifiers: Modifiers) -> EventOutcome {
            let event = KeyEvent {
                key,
                text: text.map(str::to_owned),
                modifiers,
                pressed: true,
            };
            self.engine.on_key(&event, &mut self.renderer)
        }

        /// Center-of-cell physical pixel coords for a 0-based cell.
        fn cell_px(&self, col: usize, row: usize) -> (f64, f64) {
            let cw = self.renderer.cell_width() as f64;
            let ch = self.renderer.cell_height() as f64;
            ((col as f64 + 0.5) * cw, (row as f64 + 0.5) * ch)
        }

        fn button(&mut self, button: MouseButton, pressed: bool, col: usize, row: usize, modifiers: Modifiers) {
            let (x, y) = self.cell_px(col, row);
            self.engine
                .on_mouse_button(button, pressed, x, y, modifiers, &self.renderer);
        }

        fn moved(&mut self, col: usize, row: usize) {
            let (x, y) = self.cell_px(col, row);
            self.engine.on_mouse_moved(x, y, &self.renderer);
        }

        fn sent_bytes(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }

        fn drain_sent(&self) -> Vec<Vec<u8>> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    fn no_mods() -> Modifiers {
        Modifiers {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        }
    }

    // ── Capability parity: keys ──────────────────────────────────

    #[test]
    fn plain_key_reaches_pty() {
        let mut h = Harness::new(SinkKind::Closure);
        let out = h.key(KeyCode::Char('a'), Some("a"), no_mods());
        assert!(out.consumed);
        assert_eq!(h.sent_bytes(), vec![b"a".to_vec()]);
    }

    /// **Invariant: the operator's own keystroke snaps the view to
    /// the live tail** (2026-06-11) — the input-layer half of the
    /// scrollback-anchor contract. Output pins the view to content
    /// (terminal.rs tests); typing is the ONE thing that brings the
    /// viewport back to the prompt.
    #[test]
    fn typing_while_scrolled_snaps_view_to_bottom() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..30 {
            h.feed(b"line\r\n");
        }
        h.terminal.write().scroll_up(5);
        assert_eq!(h.terminal.read().scroll_offset(), 5);
        h.key(KeyCode::Char('a'), Some("a"), no_mods());
        assert_eq!(
            h.terminal.read().scroll_offset(),
            0,
            "a keystroke that sends bytes must snap to the live tail"
        );
        assert_eq!(h.sent_bytes(), vec![b"a".to_vec()]);
    }

    /// **Scrollback search** (2026-06-11): a query that scrolled out
    /// of the viewport must still match, and Next must bring the
    /// match into view. Pre-fix the scan covered only the visible
    /// rows — "Cmd+F for a string from two seconds ago: zero
    /// matches, silently."
    #[test]
    fn search_finds_matches_in_scrollback_and_next_scrolls_to_them() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"needle-here\r\n");
        for _ in 0..40 {
            h.feed(b"filler\r\n");
        }
        // The needle is now deep in scrollback (24-row screen).
        assert_eq!(h.terminal.read().scroll_offset(), 0);
        h.engine.apply_action(Action::SearchOpen, &mut h.renderer);
        for ch in ["n", "e", "e", "d", "l", "e"] {
            h.key(KeyCode::Char(ch.chars().next().unwrap()), Some(ch), no_mods());
        }
        let (count, target_abs) = {
            let st = h.engine.search.lock().unwrap();
            (
                st.matches.len(),
                st.current_match().map(|m| m.row).unwrap_or(usize::MAX),
            )
        };
        assert!(count >= 1, "scrollback content must match");
        // Query-edit already jumped the view to the active match.
        let term = h.terminal.read();
        let top_abs = term.scrollback_total() - term.scroll_offset();
        assert!(
            target_abs >= top_abs && target_abs < top_abs + term.rows(),
            "active match (abs row {target_abs}) must be inside the \
             viewport [{top_abs}, +{})",
            term.rows()
        );
    }

    /// Absolute match rows survive viewport scrolling — the matches
    /// list doesn't go stale when the operator wheels around with
    /// the overlay open.
    #[test]
    fn search_matches_keep_absolute_rows_across_scrolling() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"needle\r\n");
        for _ in 0..40 {
            h.feed(b"filler\r\n");
        }
        h.engine.apply_action(Action::SearchOpen, &mut h.renderer);
        for ch in ["n", "e", "e", "d", "l", "e"] {
            h.key(KeyCode::Char(ch.chars().next().unwrap()), Some(ch), no_mods());
        }
        let before: Vec<usize> = h
            .engine
            .search
            .lock()
            .unwrap()
            .matches
            .iter()
            .map(|m| m.row)
            .collect();
        h.terminal.write().scroll_down(3);
        let after: Vec<usize> = h
            .engine
            .search
            .lock()
            .unwrap()
            .matches
            .iter()
            .map(|m| m.row)
            .collect();
        assert_eq!(before, after, "absolute rows must not shift with the viewport");
    }

    /// Search matches re-anchor across a grid resize (M2 review
    /// wave): the per-tick reconciler re-runs the live query when
    /// [`Terminal::grid_generation`] moves, so absolute match rows
    /// track the rewrapped layout instead of pointing at whatever
    /// content the renumbered rows now hold.
    #[test]
    fn search_matches_reanchor_after_grid_resize() {
        let mut h = Harness::new(SinkKind::Closure);
        // A 100-char wrapped line ABOVE the needle shifts the
        // needle's absolute row when the column count changes.
        let long: String = (0..100u32)
            .map(|i| char::from_digit(i % 10, 10).unwrap())
            .collect();
        h.feed(long.as_bytes());
        h.feed(b"\r\nneedle\r\n");
        h.engine.apply_action(Action::SearchOpen, &mut h.renderer);
        for ch in ["n", "e", "e", "d", "l", "e"] {
            h.key(KeyCode::Char(ch.chars().next().unwrap()), Some(ch), no_mods());
        }
        let rows = |h: &Harness| -> Vec<usize> {
            h.engine
                .search
                .lock()
                .unwrap()
                .matches
                .iter()
                .map(|m| m.row)
                .collect()
        };
        assert_eq!(rows(&h), vec![2], "needle on absolute row 2 at 80 cols");

        // Narrow: the wrapped line grows to 3 rows; the reconciler
        // tick re-runs the query against the fresh layout.
        h.terminal.write().resize(40, 24);
        h.engine.on_redraw_tick(&h.renderer);
        assert_eq!(
            rows(&h),
            vec![3],
            "matches re-anchored to the rewrapped layout"
        );

        // Widen back: another tick converges again.
        h.terminal.write().resize(80, 24);
        h.engine.on_redraw_tick(&h.renderer);
        assert_eq!(rows(&h), vec![2], "round trip restores match rows");
    }

    /// **Bare modifier presses must NOT clear the selection** —
    /// winit synthesizes a key event for every Cmd/Shift press, and
    /// clearing there made Cmd+C of a mouse selection structurally
    /// dead (hunt finding 2026-06-11): the Cmd press wiped the
    /// selection before the C arrived.
    #[test]
    fn bare_modifier_press_preserves_selection_so_cmd_c_works() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        // Mouse-select "hello" via drag.
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        let (x1, y1) = h.cell_px(4, 0);
        h.engine.on_mouse_moved(x1, y1, &h.renderer);
        h.button(MouseButton::Left, false, 4, 0, no_mods());
        assert!(h.engine.selection.lock().unwrap().is_active());

        // The bare Cmd press (synthesized key event, no text).
        let mut cmd_only = no_mods();
        cmd_only.meta = true;
        h.key(KeyCode::Unknown, None, cmd_only);
        assert!(
            h.engine.selection.lock().unwrap().is_active(),
            "bare modifier press must not clear the selection"
        );

        // Copy still extracts it (via the action seam — the chord's
        // dispatch path).
        let out = h.engine.apply_action(Action::Copy, &mut h.renderer);
        assert!(matches!(out, ActionOutcome::Consumed(_)));
        assert_eq!(h.clipboard.paste_text().unwrap(), "hello");
    }

    /// Typing real input still clears the selection (it's about to
    /// change the grid under it).
    #[test]
    fn typing_clears_selection() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.button(MouseButton::Left, false, 0, 0, no_mods());
        h.key(KeyCode::Char('x'), Some("x"), no_mods());
        assert!(
            !h.engine.selection.lock().unwrap().is_active(),
            "byte-sending keystrokes clear the selection"
        );
    }

    /// **Shift bypasses mouse tracking** (xterm/kitty/ghostty
    /// contract): shift+click selects terminal-side and forwards
    /// NOTHING; shift+wheel scrolls mado's scrollback.
    #[test]
    fn shift_click_and_wheel_bypass_mouse_tracking() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..30 {
            h.feed(b"line\r\n");
        }
        h.feed(b"\x1b[?1000h\x1b[?1006h"); // app enables tracking
        let mut shift = no_mods();
        shift.shift = true;

        // shift+click: no report forwarded, selection starts.
        h.button(MouseButton::Left, true, 2, 2, shift);
        assert!(
            h.sent_bytes().is_empty(),
            "shift+click must not forward a mouse report"
        );
        h.button(MouseButton::Left, false, 2, 2, shift);

        // shift held (cached from the button event) + wheel: scrolls
        // scrollback instead of forwarding.
        h.clear_sent();
        let mut shift_key = no_mods();
        shift_key.shift = true;
        h.key(KeyCode::Unknown, None, shift_key); // bare Shift press updates the cache
        h.engine.on_mouse_scroll(1.0, &h.renderer);
        assert!(
            h.sent_bytes().is_empty(),
            "shift+wheel must not forward a mouse report"
        );
        assert!(
            h.terminal.read().scroll_offset() > 0,
            "shift+wheel scrolls mado's scrollback"
        );
    }

    /// With `mouse_shift_capture` on, shift-modified clicks forward
    /// to the app WITH the shift bit (4) — the operator opted the
    /// bypass off.
    #[test]
    fn shift_capture_true_forwards_shift_clicks() {
        let mut h = Harness::new(SinkKind::Closure);
        h.engine.behavior.mouse_shift_capture = true;
        h.feed(b"\x1b[?1000h\x1b[?1006h");
        let mut shift = no_mods();
        shift.shift = true;
        h.button(MouseButton::Left, true, 0, 0, shift);
        assert_eq!(
            h.sent_bytes(),
            vec![b"\x1b[<4;1;1M".to_vec()],
            "captured shift click forwards with bit 4"
        );
    }

    /// Middle-click pastes via the guarded path (PasteGuard +
    /// bracketed framing).
    #[test]
    fn middle_click_pastes_guarded() {
        let mut h = Harness::new(SinkKind::Closure);
        h.clipboard.copy_text("from-selection").unwrap();
        h.button(MouseButton::Middle, true, 1, 1, no_mods());
        let sent = h.sent_bytes().concat();
        assert_eq!(sent, b"from-selection");
    }

    /// Keystrokes inside an alt-screen TUI must NOT zero the saved
    /// primary-screen scroll position — 'q' out of a pager returns
    /// the operator to where they were reading.
    #[test]
    fn alt_screen_keystroke_preserves_primary_scroll_offset() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..30 {
            h.feed(b"line\r\n");
        }
        h.terminal.write().scroll_up(5);
        h.feed(b"\x1b[?1049h"); // pager opens
        h.key(KeyCode::Char('q'), Some("q"), no_mods());
        h.feed(b"\x1b[?1049l"); // pager exits
        assert_eq!(
            h.terminal.read().scroll_offset(),
            5,
            "typing in the alt screen must not discard the primary reading position"
        );
    }

    /// Mouse reports are NOT operator look-at-the-prompt intent —
    /// wheel forwarding in mouse-tracking mode must not move a
    /// scrolled primary viewport. (Guards the write_key_input /
    /// pty.write split.)
    #[test]
    fn mouse_report_does_not_snap_view_to_bottom() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..30 {
            h.feed(b"line\r\n");
        }
        h.terminal.write().scroll_up(7);
        // Enable mouse tracking so the wheel forwards reports.
        h.feed(b"\x1b[?1000h\x1b[?1006h");
        h.engine.on_mouse_scroll(1.0, &h.renderer);
        assert_eq!(
            h.terminal.read().scroll_offset(),
            7,
            "forwarded mouse reports must not move the operator's view"
        );
    }

    /// **Alternate-scroll** (xterm 1007 semantics, default-on like
    /// kitty/ghostty): wheel in a full-screen TUI without mouse
    /// tracking maps to arrow keys — DECCKM picks CSI vs SS3 — so
    /// less/man scroll content instead of a no-op.
    #[test]
    fn wheel_in_alt_screen_sends_arrow_keys() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"\x1b[?1049h"); // alt screen, no mouse tracking
        h.engine.on_mouse_scroll(1.0, &h.renderer);
        let sent = h.sent_bytes().concat();
        assert!(
            !sent.is_empty() && sent.chunks(3).all(|c| c == b"\x1b[A"),
            "wheel-up in alt screen must emit CSI A repeats: {sent:?}"
        );
        // Application cursor-keys mode flips the encoding to SS3.
        h.feed(b"\x1b[?1h");
        h.clear_sent();
        h.engine.on_mouse_scroll(-1.0, &h.renderer);
        let sent = h.sent_bytes().concat();
        assert!(
            !sent.is_empty() && sent.chunks(3).all(|c| c == b"\x1bOB"),
            "wheel-down under DECCKM must emit SS3 B repeats: {sent:?}"
        );
    }

    #[test]
    fn kitty_encoded_key_reaches_pty_when_mode_active() {
        let mut h = Harness::new(SinkKind::Closure);
        // Push kitty flags=1 (disambiguate) — the mirror Terminal
        // tracks the stack from the byte stream.
        h.feed(b"\x1b[>1u");
        let out = h.key(KeyCode::Escape, None, no_mods());
        assert!(out.consumed);
        let expected = crate::keybind::kitty_encode_key(
            &KeyCode::Escape,
            &None,
            &no_mods(),
            1,
        )
        .expect("escape must kitty-encode under flags=1");
        assert_eq!(h.sent_bytes(), vec![expected]);
    }

    // ── Capability parity: copy / paste ──────────────────────────

    #[test]
    fn double_click_word_copy_on_select_then_copy_action() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        // Double-click on "hello": press/release/press/release at the
        // same cell within the 400ms multi-click window.
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "hello",
            "copy_on_select must copy the word selection on release"
        );
        // Explicit Copy action over the same live selection.
        let out = h.engine.apply_action(Action::Copy, &mut h.renderer);
        assert!(matches!(out, ActionOutcome::Consumed(_)));
        assert_eq!(h.clipboard.paste_text().unwrap(), "hello");
        // No mouse tracking armed — selection traffic never reaches
        // the PTY.
        assert!(h.sent_bytes().is_empty());
    }

    // ── Extend gestures (competitive queue 2026-06-12) ───────────

    /// Shift+left-click extends the existing selection to the click
    /// point (xterm/kitty/ghostty convention); release lands the
    /// extended text on the clipboard via `copy_on_select`.
    #[test]
    fn shift_click_extends_existing_selection() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world wide");
        // Drag-select "hello".
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(4, 0);
        h.button(MouseButton::Left, false, 4, 0, no_mods());
        assert_eq!(h.clipboard.paste_text().unwrap(), "hello");

        // Shift+click at col 10 extends to "hello world".
        let mut shift = no_mods();
        shift.shift = true;
        h.button(MouseButton::Left, true, 10, 0, shift);
        h.button(MouseButton::Left, false, 10, 0, shift);
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "hello world",
            "shift+click must extend the selection to the click point"
        );
        // Continuing to drag after the shift-click keeps extending.
        h.button(MouseButton::Left, true, 10, 0, shift);
        h.moved(15, 0);
        h.button(MouseButton::Left, false, 15, 0, shift);
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "hello world wide",
            "shift-drag after the extend click keeps extending"
        );
    }

    /// Shift+click with NO existing selection creates none — it
    /// falls through to the plain click path, and a motionless click
    /// is not a selection.
    #[test]
    fn shift_click_without_selection_creates_none() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        let mut shift = no_mods();
        shift.shift = true;
        h.button(MouseButton::Left, true, 3, 0, shift);
        h.button(MouseButton::Left, false, 3, 0, shift);
        assert!(
            !h.engine.selection.lock().unwrap().is_active(),
            "shift+click with nothing to extend must not create a selection"
        );
    }

    /// Double-click-drag extends by WORDS: both ends snap to word
    /// boundaries and the origin word stays fully selected.
    #[test]
    fn double_click_drag_extends_by_words() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"alpha bravo charlie");
        // Double-click lands on "alpha"; button stays down.
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        // Drag into the middle of "charlie" (col 14) — both ends
        // word-snap: alpha's start, charlie's end.
        h.moved(14, 0);
        h.button(MouseButton::Left, false, 14, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "alpha bravo charlie",
            "word drag must snap BOTH ends to word boundaries"
        );
        // Reverse word-drag: double-click "charlie", drag back into
        // "alpha" — origin word stays fully selected.
        h.feed(b"\x1b[H"); // cursor home, content unchanged
        h.button(MouseButton::Left, true, 14, 0, no_mods());
        h.button(MouseButton::Left, false, 14, 0, no_mods());
        h.button(MouseButton::Left, true, 14, 0, no_mods());
        h.moved(2, 0);
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "alpha bravo charlie",
            "reverse word drag must keep the origin word fully selected"
        );
    }

    /// Triple-click-drag extends by LINES: every motion re-spans to
    /// full rows covering origin line through pointer line.
    #[test]
    fn triple_click_drag_extends_by_lines() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"one\r\ntwo\r\nthree");
        // Triple-click row 0; button stays down after press #3.
        h.button(MouseButton::Left, true, 1, 0, no_mods());
        h.button(MouseButton::Left, false, 1, 0, no_mods());
        h.button(MouseButton::Left, true, 1, 0, no_mods());
        h.button(MouseButton::Left, false, 1, 0, no_mods());
        h.button(MouseButton::Left, true, 1, 0, no_mods());
        // Drag down to row 2.
        h.moved(1, 2);
        h.button(MouseButton::Left, false, 1, 2, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "one\ntwo\nthree",
            "line drag must span full rows from origin line to pointer line"
        );
    }

    /// The per-tick reconciler collapses a dangling selection
    /// (anchors that no longer resolve — RIS rebuild here, scrollback
    /// eviction in the terminal-level tests) to `None`: `is_active()`
    /// never advertises a highlight that cannot render or extract.
    #[test]
    fn reconciler_clears_dangling_selection() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(4, 0);
        h.button(MouseButton::Left, false, 4, 0, no_mods());
        assert!(h.engine.selection.lock().unwrap().is_active());
        h.feed(b"\x1bc"); // RIS — grids rebuilt, anchors dangle
        h.engine.on_redraw_tick(&h.renderer);
        assert!(
            !h.engine.selection.lock().unwrap().is_active(),
            "dangling selection must collapse to None on the next tick"
        );
    }

    /// Scrollback eviction through the engine: select, stream past
    /// the cap, tick — the selection state collapses instead of
    /// holding garbage.
    #[test]
    fn eviction_clears_selection_through_engine_tick() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"doomed");
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(5, 0);
        h.button(MouseButton::Left, false, 5, 0, no_mods());
        assert!(h.engine.selection.lock().unwrap().is_active());
        // Default cap is 10_000 — stream past it so "doomed" evicts.
        for _ in 0..10_060 {
            h.feed(b"\r\nf");
        }
        h.engine.on_redraw_tick(&h.renderer);
        assert!(
            !h.engine.selection.lock().unwrap().is_active(),
            "evicted selection must collapse to None, not stale coordinates"
        );
    }

    #[test]
    fn paste_is_bracketed_and_pasteguard_sanitized() {
        for kind in [SinkKind::Closure, SinkKind::Control] {
            let mut h = Harness::new(kind);
            // Bracketed paste armed by the application.
            h.feed(b"\x1b[?2004h");
            // Clipboard payload embeds the bracketed-paste terminator
            // — the classic paste-injection vector.
            h.clipboard
                .copy_text("echo hi\x1b[201~rm -rf /")
                .unwrap();
            let out = h.engine.apply_action(Action::Paste, &mut h.renderer);
            assert!(matches!(out, ActionOutcome::Consumed(_)));
            assert_eq!(
                h.sent_bytes(),
                vec![
                    b"\x1b[200~".to_vec(),
                    b"echo hirm -rf /".to_vec(),
                    b"\x1b[201~".to_vec(),
                ],
                "paste must be framed AND PasteGuard-sanitized in every sink config"
            );
        }
    }

    // ── Capability parity: search overlay ────────────────────────

    #[test]
    fn search_open_query_next_close_through_engine() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world hello");
        let out = h.engine.apply_action(Action::SearchOpen, &mut h.renderer);
        assert!(matches!(out, ActionOutcome::Consumed(_)));
        // Typed query characters route to the overlay, never the PTY.
        h.key(KeyCode::Char('h'), Some("h"), no_mods());
        h.key(KeyCode::Char('e'), Some("e"), no_mods());
        {
            let st = h.engine.search.lock().unwrap();
            assert!(st.active);
            assert_eq!(st.query, "he");
            assert!(st.match_count() >= 2, "two 'he' occurrences seeded");
        }
        let before = h.engine.search.lock().unwrap().current;
        assert!(matches!(
            h.engine.apply_action(Action::SearchNext, &mut h.renderer),
            ActionOutcome::Consumed(_)
        ));
        let after = h.engine.search.lock().unwrap().current;
        assert_ne!(before, after, "SearchNext must advance the match cursor");
        assert!(matches!(
            h.engine.apply_action(Action::SearchClose, &mut h.renderer),
            ActionOutcome::Consumed(_)
        ));
        assert!(!h.engine.search.lock().unwrap().active);
        assert!(
            h.sent_bytes().is_empty(),
            "overlay input must never leak to the PTY"
        );
    }

    // ── Capability parity: mouse ─────────────────────────────────

    #[test]
    fn mouse_sgr_all_buttons_with_modifier_bits() {
        let mut h = Harness::new(SinkKind::Closure);
        // Normal tracking (1000) + SGR (1006).
        h.feed(b"\x1b[?1000h\x1b[?1006h");
        // Ctrl-only (bit 16): shift-modified clicks are the operator
        // bypass by default (mouse_shift_capture=false) and never
        // reach the app — shift-bit coverage lives in
        // `shift_capture_true_forwards_shift_clicks`.
        let mods = Modifiers {
            ctrl: true,
            alt: false,
            shift: false,
            meta: false,
        };
        // middle+ctrl at 0-based cell (40,12) → button 1 + 16 = 17,
        // coords 41;13. Left → 16, Right → 18.
        let mut failures: Vec<String> = Vec::new();
        for (button, press, release) in [
            (MouseButton::Left, &b"\x1b[<16;41;13M"[..], &b"\x1b[<16;41;13m"[..]),
            (MouseButton::Middle, &b"\x1b[<17;41;13M"[..], &b"\x1b[<17;41;13m"[..]),
            (MouseButton::Right, &b"\x1b[<18;41;13M"[..], &b"\x1b[<18;41;13m"[..]),
        ] {
            h.drain_sent();
            h.button(button, true, 40, 12, mods);
            h.button(button, false, 40, 12, mods);
            let got = h.sent_bytes();
            if got != vec![press.to_vec(), release.to_vec()] {
                failures.push(format!("{button:?}: got {got:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} button encodings diverged:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn mouse_wheel_uses_true_cell_coords() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"\x1b[?1000h\x1b[?1006h");
        // Track the pointer to 0-based cell (10,5); Normal mode (1000)
        // forwards no motion, but the engine latches the position.
        h.moved(10, 5);
        h.drain_sent();
        // The spec row: wheel-up at (10,5) → ESC[<64;11;6M — true
        // coords, not the pre-M1 fake 1;1.
        h.engine.on_mouse_scroll(1.0, &h.renderer);
        assert_eq!(h.sent_bytes(), vec![b"\x1b[<64;11;6M".to_vec()]);
        h.drain_sent();
        h.engine.on_mouse_scroll(-1.0, &h.renderer);
        assert_eq!(h.sent_bytes(), vec![b"\x1b[<65;11;6M".to_vec()]);
    }

    #[test]
    fn mouse_drag_and_hover_motion_codes() {
        let mut h = Harness::new(SinkKind::Closure);
        // AnyEvent tracking (1003) + SGR.
        h.feed(b"\x1b[?1003h\x1b[?1006h");
        // Hover motion (no button): code 3 + 32 = 35.
        h.moved(2, 3);
        assert_eq!(h.drain_sent(), vec![b"\x1b[<35;3;4M".to_vec()]);
        // Press left, drag: code 0 + 32 = 32.
        h.button(MouseButton::Left, true, 2, 3, no_mods());
        assert_eq!(h.drain_sent(), vec![b"\x1b[<0;3;4M".to_vec()]);
        h.moved(3, 3);
        assert_eq!(h.drain_sent(), vec![b"\x1b[<32;4;4M".to_vec()]);
        // Release, hover again: back to 35.
        h.button(MouseButton::Left, false, 3, 3, no_mods());
        assert_eq!(h.drain_sent(), vec![b"\x1b[<0;4;4m".to_vec()]);
        h.moved(4, 3);
        assert_eq!(h.drain_sent(), vec![b"\x1b[<35;5;4M".to_vec()]);
    }

    // ── Ctrl-R flow through the engine, both sink configs ────────
    //
    // Sibling of `keybind::tests::embedded_tear_flow_ctrl_r_reaches_
    // pty` — the identical flow driven THROUGH InputEngine::on_key,
    // asserting 0x12 reaches the recording PtySink under BOTH sink
    // configurations (closure over Vec, and a mock
    // MultiplexerControl-backed sink).

    #[test]
    fn ctrl_r_reaches_pty_through_engine_closure_sink() {
        let mut h = Harness::new(SinkKind::Closure);
        let out = h.key(
            KeyCode::Char('r'),
            None,
            Modifiers {
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            },
        );
        assert!(out.consumed);
        assert_eq!(h.sent_bytes(), vec![vec![0x12u8]]);
    }

    #[test]
    fn ctrl_r_reaches_pty_through_engine_control_sink() {
        let mut h = Harness::new(SinkKind::Control);
        h.key(
            KeyCode::Char('r'),
            None,
            Modifiers {
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            },
        );
        let recorded = h.control.sent.lock().unwrap().clone();
        assert_eq!(recorded, vec![(h.pane, vec![0x12u8])]);
    }

    // ── Font zoom bounded by construction, both sink configs ─────

    #[test]
    fn font_zoom_1000_increments_saturates_at_font_max_in_both_sink_configs() {
        for kind in [SinkKind::Closure, SinkKind::Control] {
            let mut h = Harness::new(kind);
            for _ in 0..1000 {
                h.engine.apply_action(Action::FontIncrease, &mut h.renderer);
            }
            assert_eq!(
                crate::ux::FontZoomTarget::font_size(&h.renderer),
                crate::font_size::FONT_MAX,
                "1000 increments through the engine must saturate at FONT_MAX"
            );
        }
    }

    // ── Grid reconciler ──────────────────────────────────────────

    #[test]
    fn on_resize_defers_to_reconciler_and_push_grid_resizes_both_halves() {
        let mut h = Harness::new(SinkKind::Closure);
        let (w, h_px) = (800u32, 600u32);
        // on_resize must NOT push — events dispatch before render, so
        // any grid computed here is one frame stale (the new→old→new
        // ping-pong class). Convergence belongs to on_redraw_tick.
        let out = h.engine.on_resize(w, h_px, &h.renderer);
        assert!(out.consumed);
        assert!(
            h.resized.lock().unwrap().is_empty(),
            "on_resize must defer the grid push to the reconciler"
        );
        // The reconciler's push primitive resizes BOTH halves: the
        // mirror VT grid and the sink.
        h.engine.push_grid(&h.renderer, w, h_px);
        let (cols, rows) = h.renderer.cells_for_window_phys(w, h_px);
        assert_eq!(h.resized.lock().unwrap().as_slice(), &[(cols, rows)]);
        let term = h.terminal.read();
        assert_eq!((term.cols(), term.rows()), (cols as usize, rows as usize));
    }

    // ── Ported `apply_tear_action` seam invariants ───────────────
    //
    // Same asserted behavior as the pre-M1 gui_tear_attach tests;
    // the seam is now engine.apply_action.

    /// **Invariant: Search Close/Next/Prev on an INACTIVE overlay are
    /// NOT handled** (review finding 2026-06-11 — the Esc-eating
    /// regression). The fleet atlas binds `search_close` to bare
    /// Escape; if the engine consumed it while the overlay was
    /// closed, Esc would never reach the PTY and vim/helix/fzf would
    /// die. The contract: with `SearchState.active == false`, each of
    /// the three actions must (a) fall through so the caller forwards
    /// the original key bytes, (b) leave the search state inactive,
    /// and (c) write NOTHING to the sink itself. Failures aggregate
    /// matrix-style — one run reports every divergent action.
    #[test]
    fn inactive_search_actions_fall_through_to_pty_forwarding() {
        let mut failures: Vec<String> = Vec::new();
        for action in [Action::SearchClose, Action::SearchNext, Action::SearchPrev] {
            let mut h = Harness::new(SinkKind::Closure);
            assert!(
                !h.engine.search.lock().unwrap().active,
                "precondition: fresh SearchState must be inactive"
            );
            let handled = matches!(
                h.engine.apply_action(action, &mut h.renderer),
                ActionOutcome::Consumed(_)
            );
            if handled {
                failures.push(format!(
                    "{action:?}: handled an INACTIVE search overlay — eats the \
                     key (bare Esc) before PTY forwarding"
                ));
            }
            if h.engine.search.lock().unwrap().active {
                failures.push(format!(
                    "{action:?}: flipped an inactive SearchState to active"
                ));
            }
            if !h.sent_bytes().is_empty() {
                failures.push(format!(
                    "{action:?}: synthesized PTY bytes on the fall-through path"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} fall-through violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Control row for the invariant above: with the overlay OPEN,
    /// the same three actions ARE handled (consume the key) and
    /// `SearchClose` actually closes the overlay — proving the
    /// fall-through test exercises the active/inactive seam, not a
    /// dead handler.
    #[test]
    fn active_search_actions_are_consumed_by_the_overlay() {
        let mut failures: Vec<String> = Vec::new();
        for action in [Action::SearchNext, Action::SearchPrev, Action::SearchClose] {
            let mut h = Harness::new(SinkKind::Closure);
            assert!(
                matches!(
                    h.engine.apply_action(Action::SearchOpen, &mut h.renderer),
                    ActionOutcome::Consumed(_)
                ),
                "SearchOpen must be handled"
            );
            assert!(
                h.engine.search.lock().unwrap().active,
                "SearchOpen must arm the overlay"
            );
            let handled = matches!(
                h.engine.apply_action(action, &mut h.renderer),
                ActionOutcome::Consumed(_)
            );
            if !handled {
                failures.push(format!(
                    "{action:?}: not handled while the overlay is ACTIVE"
                ));
            }
            let active_after = h.engine.search.lock().unwrap().active;
            let want_active_after = !matches!(action, Action::SearchClose);
            if active_after != want_active_after {
                failures.push(format!(
                    "{action:?}: overlay active={active_after} after dispatch, \
                     want {want_active_after}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} active-overlay violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}
