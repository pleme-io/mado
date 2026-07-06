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
use madori::event::{KeyEvent, Modifiers, MouseButton, ScrollDelta};

use crate::dir_picker::DirPickerState;
use crate::font_size::{BoundedFontSize, FontSizeSteps};
use crate::picker::state::PickerSource;
use crate::keybind::{Action, KeybindManager};
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::search::SearchState;
use crate::selection::{CellPos, Selection};
use crate::session_picker::{SessionPickerBridge, SessionPickerState};
use crate::terminal::{Cell, MouseMode, SelectionAnchor};
use crate::ux::modes::{
    self, DragMode, Overlay, OverlayEffect, OverlayEvent, OverlayKey, OverlayRouting, OverlayStep,
    Pointer, PointerEffect, PointerEvent, PressPlan, PressRoute, SearchNav,
};
use crate::ux::mouse_report::{MouseMods, MouseReport, MouseReportButton, MouseReportKind};
use crate::ux::{
    EventOutcome, FontZoomTarget, PtySink, ResizeSink, ScrollAction, ScrollContext, ScrollGesture,
    ScrollSystem, UxBehavior,
};

/// Translate a windowing-layer [`ScrollDelta`] (madori, typed by source)
/// into the scroll system's [`ScrollGesture`]. The ONE boundary where
/// mado's scroll path touches the windowing type; everything downstream is
/// the windowing-agnostic [`ScrollSystem`]. Vertical-only — mado has no
/// horizontal scrollback (ghostty's X axis is dropped at this seam, as it
/// was when the engine read a single `dy`).
impl From<ScrollDelta> for ScrollGesture {
    fn from(delta: ScrollDelta) -> Self {
        match delta {
            ScrollDelta::Lines { y, .. } => ScrollGesture::Wheel { ticks: y },
            ScrollDelta::Pixels { y, .. } => ScrollGesture::Precise { pixels: y },
        }
    }
}

/// The shared overlay/selection state the renderer highlights from
/// and the engine mutates. One value, three Arcs — constructed
/// together so a call site cannot wire half of them.
pub struct SharedUxState {
    pub selection: Arc<Mutex<Selection>>,
    pub search: Arc<Mutex<SearchState>>,
    pub dir_picker: Arc<Mutex<DirPickerState>>,
    pub session_picker: Arc<Mutex<SessionPickerState>>,
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
            session_picker: Arc::new(Mutex::new(SessionPickerState::new())),
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
    /// Clickable-link config (hover cursor + click-to-open gates). The
    /// highlight half lives on the renderer; this is the interaction half.
    pub links: crate::config::MadoLinksConfig,
    /// DECCKM (cursor-keys application mode) query — the one
    /// read-side divergence between the modes.
    pub cursor_keys_mode: Box<dyn Fn() -> bool + Send + Sync>,
    /// Font size `Action::FontReset` returns to.
    pub default_font_size: f32,
    /// Logical window padding (scaled by the renderer's factor for
    /// pixel→cell math).
    pub padding: f32,
    /// The Ctrl-S session picker bridge — praça index + live registry +
    /// switch channel. `None` ⇒ session-switching disabled: Ctrl-S
    /// opens an inert picker with a "switching disabled" hint, mirroring
    /// the `switch_session` MCP tool. `Some` only on the embedded
    /// switchable path (the same gating as auto-attach).
    pub session_picker_bridge: Option<Box<dyn SessionPickerBridge>>,
    /// `suggestions.attention_on_critical` — a NEW Critical suggestion
    /// requests platform attention (dock bounce) even with the board closed.
    pub suggest_attention: bool,
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

/// The unified input/UX engine. Owns every piece of interaction
/// state the pre-M1 loops kept as closure-captured locals.
pub struct InputEngine {
    terminal: SharedTerminal,
    pty: Box<dyn PtySink>,
    resize: Box<dyn ResizeSink>,
    selection: Arc<Mutex<Selection>>,
    search: Arc<Mutex<SearchState>>,
    dir_picker: Arc<Mutex<DirPickerState>>,
    /// Renderer-shared Ctrl-S session picker state (praça browse +
    /// switch). Written ONLY by `apply_overlay_step`, read by the
    /// renderer's Pass-6 overlay — same mirror discipline as
    /// `dir_picker`.
    session_picker: Arc<Mutex<SessionPickerState>>,
    /// The praça bridge backing the session picker: list + switch.
    /// `None` ⇒ session-switching disabled (the picker is inert). The
    /// engine never reaches praca / `InProcess` / the switch channel
    /// directly — only through this seam.
    session_picker_bridge: Option<Box<dyn SessionPickerBridge>>,
    clipboard: Arc<dyn ClipboardProvider>,
    keybinds: KeybindManager,
    pub(crate) behavior: UxBehavior,
    /// Clickable-link interaction gates (hover pointer cursor +
    /// plain-click-to-open). The highlight half is the renderer's.
    links: crate::config::MadoLinksConfig,
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
    /// The pointer modal machine (`ux::modes::Pointer`) — drag
    /// lifecycle + shift-capture bypass in ONE typed state. Replaced
    /// the three pre-FSM sibling cells `left_button_down: bool` +
    /// `shift_drag_bypass: bool` + `drag: SelectionDrag` (FSM lift
    /// 2026-06-12): the button-held fact is now DERIVED from the
    /// state (`Pointer::left_button_down`), so button/drag desync is
    /// unrepresentable, and a forwarded press structurally cannot
    /// carry the bypass.
    pointer: Pointer,
    /// The overlay modal machine — which overlay owns the keyboard.
    /// Authoritative for ROUTING and for every engine-side mode
    /// decision (`reconcile_search` gates on it, M3 review
    /// 2026-06-12).
    overlay: Overlay,
    /// The renderer's SINGLE source of truth for which overlay to draw
    /// — a faithful 1:1 mirror of [`Self::overlay`], written on the SAME
    /// line the FSM state changes ([`Self::apply_overlay_step`]) and
    /// read by the renderer's Pass 6. This replaces the per-overlay
    /// `.open`/`.active` render bools as the *render gate*: the renderer
    /// matches on this one enum and draws exactly the overlay the FSM
    /// says owns the keyboard, so "two overlays visible at once" is
    /// unrepresentable at the render layer — not a priority heuristic
    /// (theory §VI, docs/THEORY.md §VIII row 1). The picker `.open`
    /// bools remain for each picker's own data lifecycle, but no longer
    /// decide what paints.
    overlay_focus: Arc<Mutex<Overlay>>,
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
    /// The scroll system (`ux::scroll`) — the ONE typed place that decides
    /// what every scroll gesture does. Owns the momentum kinetics + the
    /// precise pixel accumulator + the typed policy; `on_mouse_scroll` feeds
    /// it gestures and `on_redraw_tick` ticks its momentum. A typed sub-state
    /// advanced by pure functions (the mado FSM idiom), never free bool-flag
    /// modes.
    scroll: ScrollSystem,
    /// Wall-clock of the previous `on_redraw_tick`, for the kinetics
    /// `dt`. `None` until the first tick → first dt is treated as 0
    /// (a no-op kinetics frame), which keeps the L1/L2 determinism
    /// ladders byte-stable.
    last_scroll_tick: Option<Instant>,
    /// Subscription to the suggestion store's change broadcast (stage 3 of the
    /// live-stream substrate). Obtained from the session-picker bridge at
    /// construction. While the Ctrl-S board is open and resting, the tick
    /// re-lists the moment this fires — an event-driven subscription, never a
    /// fixed timer — so newly-watched task suggestions appear on the open board
    /// without reopening. `None` when the stream is off (or no bridge).
    suggest_rx: Option<tokio::sync::watch::Receiver<u64>>,
    /// Wall-clock of the last WHOLE-board refresh (registry reconcile +
    /// re-list) while the picker sat open — drives the coarse liveness tick
    /// that keeps the session half of the board and the age/aging labels
    /// current even when the suggestion store is silent.
    last_board_tick: Option<Instant>,
    /// When an AUTONOMOUS re-list last changed the top row while the operator
    /// was resting — the positional-stability stamp. An Enter landing within
    /// the grace window of such a shift is swallowed (the row may have moved
    /// under the cursor between the painted frame and the key event).
    board_shift_at: Option<Instant>,
    /// `suggestions.attention_on_critical` (from params).
    suggest_attention: bool,
    /// Critical suggestion ids already announced (once-latch per issue).
    seen_criticals: std::collections::HashSet<crate::suggest::SuggestionId>,
    /// Whether the initial critical set has been seeded — the first
    /// observation only populates the latch (a warm restart must not bounce
    /// the dock for old news).
    criticals_seeded: bool,
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
        renderer.set_session_picker(Arc::clone(&params.shared.session_picker));
        // The single render source of truth for overlay ownership — the
        // engine writes it on every FSM transition, the renderer matches
        // on it in Pass 6 (one overlay drawn, never two).
        let overlay_focus = Arc::new(Mutex::new(Overlay::None));
        renderer.set_overlay_focus(Arc::clone(&overlay_focus));
        // Build the scroll system from the typed scroll policy BEFORE the
        // behavior value moves into the struct.
        let scroll = ScrollSystem::new(params.behavior.scroll_config());
        // Subscribe to the suggestion store's change broadcast BEFORE the bridge
        // moves into the struct — the engine re-lists the open board on the fact
        // of a change (stage 3), not a timer.
        let suggest_rx = params
            .session_picker_bridge
            .as_ref()
            .and_then(|bridge| bridge.suggestion_subscribe());
        Self {
            terminal: params.terminal,
            pty: params.pty,
            resize: params.resize,
            selection: params.shared.selection,
            search: params.shared.search,
            dir_picker: params.shared.dir_picker,
            session_picker: params.shared.session_picker,
            session_picker_bridge: params.session_picker_bridge,
            clipboard: params.clipboard,
            keybinds: params.keybinds,
            behavior: params.behavior,
            links: params.links,
            cursor_keys_mode: params.cursor_keys_mode,
            default_font_size: params.default_font_size,
            padding: params.padding,
            key_repeat_gate: awase::KeyRepeatGate::<Action>::new(),
            mouse_visible: true,
            last_click_time: Instant::now(),
            click_count: 0,
            last_click_pos: CellPos { row: 0, col: 0 },
            pointer: Pointer::Up,
            overlay: Overlay::None,
            overlay_focus,
            last_mods: Modifiers::default(),
            last_mouse_pos: (0.0, 0.0),
            grid_sync_sig: None,
            search_grid_gen: None,
            scroll,
            last_scroll_tick: None,
            suggest_rx,
            last_board_tick: None,
            board_shift_at: None,
            suggest_attention: params.suggest_attention,
            seen_criticals: std::collections::HashSet::new(),
            criticals_seeded: false,
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

    // Scroll feel/tuning constants (wheel-impulse gain, auto-scroll velocity
    // + overshoot cap) moved into the scroll system (`ux::scroll`) and the
    // typed config knobs — the engine no longer owns scroll policy.

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

    /// Resolve the link at a viewport cell: an OSC 8 hyperlink
    /// (`cell.link_id` → the link table) takes precedence, falling back to
    /// an auto-detected bare URL in that row. `None` when the cell is not a
    /// link. Shared by the hover (pointer-cursor) and click (open) paths so
    /// they agree on what a link is.
    fn link_url_at(&self, row: usize, col: usize) -> Option<String> {
        let term = self.terminal.read();
        // OSC 8 first — `hyperlink` resolves `NO_LINK_ID` (0) to `None`.
        if let Some(uri) = term.cell(row, col).hyperlink(term.links()) {
            return Some(uri.to_string());
        }
        let cols = term.cols();
        let row_cells: Vec<Cell> = term.visible_rows().nth(row)?.to_vec();
        drop(term);
        let urls = crate::url::detect_urls_in_row(&row_cells, cols, row);
        crate::url::url_at(&urls, row, col).map(|u| u.url.clone())
    }

    /// Open the link at a viewport cell, if any, with a typed
    /// `std::process::Command` (see [`crate::url::open_link`] — a URL via
    /// the OS opener, a `file://path:line` via `$VISUAL`/`$EDITOR`).
    /// Failures are logged; never panics.
    fn try_open_link_at(&self, row: usize, col: usize) {
        if let Some(url) = self.link_url_at(row, col) {
            if let Err(e) = crate::url::open_link(&url) {
                tracing::warn!(error = %e, url = %url, "failed to open link");
            }
        }
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

        // ── Overlay input routing (typed FSM — ux/modes.rs) ───────
        // While an overlay owns the keyboard every keystroke belongs
        // to it: search-nav chords act on the search bar, raw nav
        // keys drive the dir picker, plain text edits the query,
        // everything else is consumed so it can't leak to the PTY.
        // With no overlay open the machine's `Overlay::None` arm is
        // a structural fall-through — no Search-nav consuming arm
        // exists there, so the Esc-eating LAW (review finding
        // 2026-06-11) holds by construction, not by guard.
        {
            let lowered = OverlayKey::lower(action, *key, text.as_deref(), mods);
            if matches!(
                self.dispatch_overlay(OverlayEvent::Key(lowered)),
                OverlayRouting::Consumed
            ) {
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
                self.dispatch_overlay(OverlayEvent::OpenSearch);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::DirPickerOpen => {
                self.dispatch_overlay(OverlayEvent::OpenDirPicker);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::SessionPickerOpen => {
                self.dispatch_overlay(OverlayEvent::OpenSessionPicker);
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            Action::SaveSessionAsPreset => {
                // Save the highlighted live session-picker row as a preset —
                // the keybind companion to the `save_session_as_preset` MCP
                // verb. Only ACTS (+ consumes the chord) when the picker is
                // open with a live `Switch` row highlighted; otherwise it
                // FALLS THROUGH, so the chord is never swallowed in the
                // normal flow (minimal-impact default).
                // Resolve the highlighted live session (immutable borrow,
                // released before the mutable recompute below).
                let session = {
                    use crate::session_picker::RowKind;
                    let sp = self.session_picker.lock().unwrap();
                    sp.selected_row().and_then(|r| match r.kind {
                        RowKind::Switch(id) => Some(id),
                        // Latent presets, suggestions, and create rows are not
                        // live sessions — nothing to save as a preset.
                        RowKind::Instantiate(_)
                        | RowKind::Suggestion(_)
                        | RowKind::Create(_) => None,
                    })
                };
                let saved = match (self.session_picker_bridge.as_ref(), session) {
                    (Some(bridge), Some(session)) => {
                        let now = crate::auto_attach::now_unix_seconds();
                        bridge.save_as_preset(session, now)
                    }
                    _ => false,
                };
                if saved {
                    // Re-rank so the freshly-saved preset is reflected.
                    self.session_picker_recompute();
                    ActionOutcome::Consumed(EventOutcome::consumed())
                } else {
                    ActionOutcome::FallThrough
                }
            }
            Action::LayoutPickerOpen => {
                // RESERVED no-op. Ctrl-L claims the chord for the future
                // layout picker (the Ctrl-S-for-layouts analog); for now
                // it simply SWALLOWS the key so it does NOT fall through
                // to the PTY as 0x0c — which the shell would render as a
                // clear-screen. No overlay, no byte, nothing visible.
                // When the layout picker lands, swap this for a
                // `self.dispatch_overlay(OverlayEvent::OpenLayoutPicker)`.
                ActionOutcome::Consumed(EventOutcome::consumed())
            }
            // Close/Next/Prev are handled ONLY while the search
            // overlay is open: the fleet atlas binds search_close to
            // bare Escape, and consuming it on a closed overlay would
            // eat Esc before PTY forwarding and kill vim/helix/fzf
            // (review finding 2026-06-11). The LAW is structural in
            // the overlay machine — `Overlay::None` has NO nav arm,
            // so the chord falls through and the key reaches the PTY.
            Action::SearchClose | Action::SearchNext | Action::SearchPrev => {
                let nav = match action {
                    Action::SearchClose => SearchNav::Close,
                    Action::SearchNext => SearchNav::Next,
                    _ => SearchNav::Prev,
                };
                match self.dispatch_overlay(OverlayEvent::Key(OverlayKey::nav_only(nav))) {
                    OverlayRouting::Consumed => ActionOutcome::Consumed(EventOutcome::consumed()),
                    OverlayRouting::FallThrough => ActionOutcome::FallThrough,
                }
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

    // ── Overlay machine seam ─────────────────────────────────────

    /// Dispatch one event to the overlay machine, latch the new
    /// state, execute the typed effects, and hand back the routing.
    /// EVERY overlay interaction (keystroke routing in `on_key`,
    /// chord/injected actions in `apply_action`) funnels through
    /// here — there is no second path that mutates overlay state.
    fn dispatch_overlay(&mut self, event: OverlayEvent) -> OverlayRouting {
        let step = self.overlay.on_event(event);
        let routing = step.routing;
        self.apply_overlay_step(step);
        routing
    }

    /// Execute one overlay-machine step. The machine is pure; this
    /// is the I/O edge: the renderer-shared mirror cells
    /// (`SearchState` / `DirPickerState` — read by the render passes,
    /// written ONLY here), the scroll-to-match seam, and the `cd`
    /// injection through the operator-input path.
    fn apply_overlay_step(&mut self, step: OverlayStep) {
        self.overlay = step.state;
        // Keep the renderer's single source of truth in lock-step with the
        // FSM — written on the SAME line the state changes, so the render
        // gate can never disagree with which overlay owns the keyboard.
        *self.overlay_focus.lock().unwrap() = step.state;
        for effect in step.effects {
            match effect {
                OverlayEffect::SearchOpen => self.search.lock().unwrap().open(),
                OverlayEffect::SearchClose => self.search.lock().unwrap().close(),
                OverlayEffect::SearchNext => {
                    self.search.lock().unwrap().next();
                    self.scroll_to_active_match();
                }
                OverlayEffect::SearchPrev => {
                    self.search.lock().unwrap().prev();
                    self.scroll_to_active_match();
                }
                OverlayEffect::SearchAppend(text) => {
                    let (rows, cols, first_abs) = self.search_rows();
                    self.search
                        .lock()
                        .unwrap()
                        .append_query(&text, &rows, cols, first_abs);
                    self.scroll_to_active_match();
                }
                OverlayEffect::SearchBackspace => {
                    let (rows, cols, first_abs) = self.search_rows();
                    self.search
                        .lock()
                        .unwrap()
                        .backspace_query(&rows, cols, first_abs);
                    self.scroll_to_active_match();
                }
                OverlayEffect::DirPickerOpen => self.dir_picker_open(),
                OverlayEffect::DirPickerClose => self.dir_picker.lock().unwrap().close(),
                OverlayEffect::DirPickerAccept => {
                    let path = self
                        .dir_picker
                        .lock()
                        .unwrap()
                        .selected_row()
                        .map(|(p, _)| p.clone());
                    if let Some(p) = path {
                        // `cd '<path>'\n` composed from typed pieces
                        // (shell_quote_path owns the quoting rule).
                        let mut cmd = String::from("cd ");
                        cmd.push_str(&crate::dir_picker::shell_quote_path(&p.to_string_lossy()));
                        cmd.push('\n');
                        self.write_key_input(cmd.as_bytes());
                    }
                    self.dir_picker.lock().unwrap().close();
                }
                OverlayEffect::DirPickerMoveUp => self.dir_picker.lock().unwrap().move_up(),
                OverlayEffect::DirPickerMoveDown => self.dir_picker.lock().unwrap().move_down(),
                OverlayEffect::DirPickerBackspace => self.dir_picker_backspace(),
                OverlayEffect::DirPickerPush(text) => self.dir_picker_push(&text),
                OverlayEffect::SessionPickerOpen => self.session_picker_open(),
                OverlayEffect::SessionPickerClose => {
                    self.session_picker.lock().unwrap().close();
                }
                OverlayEffect::SessionPickerAccept => self.session_picker_accept(),
                OverlayEffect::SessionPickerMoveUp => self.session_picker.lock().unwrap().move_up(),
                OverlayEffect::SessionPickerMoveDown => {
                    self.session_picker.lock().unwrap().move_down();
                }
                OverlayEffect::SessionPickerBackspace => self.session_picker_backspace(),
                OverlayEffect::SessionPickerPush(text) => self.session_picker_push(&text),
                OverlayEffect::SessionPickerRenameBegin => self.session_picker_rename_begin(),
                OverlayEffect::SessionPickerRenamePush(text) => {
                    self.session_picker_rename_push(&text);
                }
                OverlayEffect::SessionPickerRenameBackspace => {
                    self.session_picker_rename_backspace();
                }
                OverlayEffect::SessionPickerRenameCommit => self.session_picker_rename_commit(),
                OverlayEffect::SessionPickerRenameCancel => self.session_picker_rename_cancel(),
            }
        }
    }

    /// Open the Ctrl-T dir picker: seed the list from the wadachi reader
    /// (top frecency dirs, empty query). Mirrors the session-picker open
    /// path so both pickers drive the shared [`crate::picker::state::FuzzyPicker`]
    /// identically — only the source differs.
    fn dir_picker_open(&mut self) {
        let rows = crate::dir_picker::DirPickerSource.list("", 0);
        self.dir_picker.lock().unwrap().open(rows, false);
    }

    /// Re-rank the dir list after a query edit through the wadachi reader.
    fn dir_picker_recompute(&mut self) {
        let query = self.dir_picker.lock().unwrap().query.clone();
        let rows = crate::dir_picker::DirPickerSource.list(&query, 0);
        self.dir_picker.lock().unwrap().set_results(rows);
    }

    /// Append typed text to the dir-picker needle + re-rank.
    fn dir_picker_push(&mut self, text: &str) {
        self.dir_picker.lock().unwrap().query.push_str(text);
        self.dir_picker_recompute();
    }

    /// Delete the last needle char + re-rank.
    fn dir_picker_backspace(&mut self) {
        self.dir_picker.lock().unwrap().query.pop();
        self.dir_picker_recompute();
    }

    /// Open the Ctrl-S session picker: seed the list from the praça
    /// bridge (frecency-ranked, empty query). With no bridge
    /// (session-switching disabled) open it in the typed `disabled`
    /// state so the renderer shows a "switching disabled" hint and Enter
    /// is inert — mirroring the `switch_session` MCP tool's
    /// `switching-disabled` answer.
    fn session_picker_open(&mut self) {
        match self.session_picker_bridge.as_ref() {
            Some(bridge) => {
                let now = crate::auto_attach::now_unix_seconds();
                // Freshness nudge: ask every suggestion watcher whose data is
                // older than its pacing gap to re-poll RIGHT NOW — the board
                // the operator opens onto is being re-verified at this moment,
                // not whenever the next interval lands. Paced per-watcher, so
                // rapid Ctrl-S taps can never hammer an API.
                crate::suggest::request_board_refresh();
                // Sync the index to the live session set first, so a
                // session spawned out-of-band (MCP / tear) shows up the
                // moment the switcher opens — "always tracking + curating".
                bridge.refresh(now);
                let rows = bridge.list("", now);
                let footer = bridge.health_footer();
                {
                    let mut sp = self.session_picker.lock().unwrap();
                    sp.open(rows, false);
                    sp.footer = footer;
                }
                self.last_board_tick = Some(Instant::now());
                self.board_shift_at = None;
            }
            None => {
                self.session_picker.lock().unwrap().open(Vec::new(), true);
            }
        }
    }

    /// Re-rank the session list after a query edit through the praça
    /// bridge. No-op (beyond keeping the typed empty list) when
    /// switching is disabled. Operator-initiated — clears the
    /// positional-stability stamp (they are looking at what they typed).
    fn session_picker_recompute(&mut self) {
        self.board_shift_at = None;
        self.session_picker_relist();
    }

    /// Re-list through the bridge without touching the stability stamp —
    /// the shared body of the operator-initiated recompute and the
    /// autonomous refresh.
    fn session_picker_relist(&mut self) {
        let Some(bridge) = self.session_picker_bridge.as_ref() else {
            return;
        };
        let query = self.session_picker.lock().unwrap().query.clone();
        let now = crate::auto_attach::now_unix_seconds();
        let rows = bridge.list(&query, now);
        let footer = bridge.health_footer();
        let mut sp = self.session_picker.lock().unwrap();
        sp.set_results(rows);
        sp.footer = footer;
    }

    /// AUTONOMOUS whole-board refresh while the picker sits open + resting:
    /// reconcile the session registry (out-of-band spawns/deaths appear and
    /// vanish live, not just suggestions), re-list, and stamp the
    /// positional-stability window if the TOP row's identity changed —
    /// an Enter within the grace of such a shift is swallowed rather than
    /// firing at a row the operator never saw.
    fn session_picker_autorefresh(&mut self) {
        let Some(bridge) = self.session_picker_bridge.as_ref() else {
            return;
        };
        let now = crate::auto_attach::now_unix_seconds();
        bridge.refresh(now);
        let query = self.session_picker.lock().unwrap().query.clone();
        let rows = bridge.list(&query, now);
        let footer = bridge.health_footer();
        let mut sp = self.session_picker.lock().unwrap();
        let prev_top = sp.results.first().map(|r| r.kind.clone());
        // Keep the cursor on its row by identity so the board can KEEP FLOWING
        // while the operator navigates the filtered set (drive across problems
        // live) — never yanking the highlight to the top on every re-list.
        let vanished = sp.set_results_preserving(rows, |r| r.kind.clone());
        sp.footer = footer;
        let new_top = sp.results.first().map(|r| r.kind.clone());
        drop(sp);
        // Stamp the positional-stability window when the operator's highlighted
        // row vanished (cursor clamped to a row they did not choose) or the top
        // row's identity changed — so a same-instant Enter is swallowed.
        if vanished || prev_top != new_top {
            self.board_shift_at = Some(Instant::now());
        }
    }

    /// Ambient attention: on a store change, bounce the dock ONCE per newly-
    /// arrived Critical suggestion (an incident reaches the operator with the
    /// board closed). The first observation only seeds the latch — a warm
    /// restart's re-surfaced rows are old news, never a bounce. Accepted /
    /// snoozed / dismissed rows never alert (the offerability gate already
    /// filtered them out of the ranked read).
    fn notice_new_criticals(&mut self) {
        if !self.suggest_attention {
            return;
        }
        let store = crate::suggest::store();
        let now_ms = crate::auto_attach::now_unix_seconds().saturating_mul(1000);
        let criticals: Vec<crate::suggest::SuggestionId> = store
            .ranked_stored(64, now_ms)
            .into_iter()
            .filter(|st| {
                st.item.urgency == crate::suggest::Urgency::Critical
                    && matches!(st.state, crate::suggest::SuggestionState::Offered)
            })
            .map(|st| st.item.id)
            .collect();
        if !self.criticals_seeded {
            self.criticals_seeded = true;
            self.seen_criticals.extend(criticals);
            return;
        }
        let mut fresh = false;
        for id in criticals {
            if self.seen_criticals.insert(id) {
                fresh = true;
            }
        }
        if fresh {
            crate::platform::request_dock_attention();
        }
        // Latch hygiene: ids decay out of the store but accumulate here; keep
        // the set bounded without ever re-alerting a live id.
        if self.seen_criticals.len() > 4096 {
            let live: std::collections::HashSet<_> = store
                .ranked_stored(usize::MAX, now_ms)
                .into_iter()
                .map(|st| st.item.id)
                .collect();
            self.seen_criticals.retain(|id| live.contains(id));
        }
    }

    /// Append typed text to the session-picker needle + re-rank.
    fn session_picker_push(&mut self, text: &str) {
        {
            let mut sp = self.session_picker.lock().unwrap();
            sp.query.push_str(text);
            sp.notice = None;
        }
        // Refining is a freshness signal: nudge every watcher whose data is
        // past its pacing gap to re-poll now, so the set the operator is
        // driving across stays live as they type — not a stale pool filtered
        // in place. Paced per-watcher, so per-keystroke calls can't hammer.
        crate::suggest::request_board_refresh();
        self.session_picker_recompute();
    }

    /// Delete the last needle char + re-rank.
    fn session_picker_backspace(&mut self) {
        {
            let mut sp = self.session_picker.lock().unwrap();
            sp.query.pop();
            sp.notice = None;
        }
        crate::suggest::request_board_refresh();
        self.session_picker_recompute();
    }

    /// The one-line rename echo shown on the picker notice line. Built by
    /// plain String composition (typed-emission house rule: no `format!()`).
    fn rename_echo(buf: &str) -> String {
        let mut s = String::from("✎ rename → ");
        s.push_str(buf);
        s.push('▏'); // inline cursor bar
        s
    }

    /// Ctrl-E: begin the inline rename. Seeds an empty buffer (an empty
    /// commit clears the custom name → reverts to the emoji identity) and
    /// echoes it on the notice line; the picker board stays visible. The
    /// rename target is resolved from the highlighted row at COMMIT (nav is
    /// inert in `Overlay::SessionRename`, so it stays put), keeping the
    /// generic `FuzzyPicker` free of any session id.
    fn session_picker_rename_begin(&mut self) {
        let mut sp = self.session_picker.lock().unwrap();
        sp.rename_buffer = Some(String::new());
        sp.notice = Some(Self::rename_echo(""));
    }

    /// Append typed text to the live rename buffer + refresh the echo.
    fn session_picker_rename_push(&mut self, text: &str) {
        let mut sp = self.session_picker.lock().unwrap();
        if let Some(buf) = sp.rename_buffer.as_mut() {
            buf.push_str(text);
        }
        sp.notice = sp.rename_buffer.as_deref().map(Self::rename_echo);
    }

    /// Delete the last char of the rename buffer + refresh the echo.
    fn session_picker_rename_backspace(&mut self) {
        let mut sp = self.session_picker.lock().unwrap();
        if let Some(buf) = sp.rename_buffer.as_mut() {
            buf.pop();
        }
        sp.notice = sp.rename_buffer.as_deref().map(Self::rename_echo);
    }

    /// Enter: commit the rename buffer to the highlighted live session. The
    /// new name flows to the tear owner + the praça custom_name via the
    /// bridge; then drop the buffer + re-list so `display_name()` reflects it.
    /// Only `RowKind::Switch` rows (live sessions) are renamable — presets /
    /// suggestions are inert here.
    fn session_picker_rename_commit(&mut self) {
        use crate::session_picker::RowKind;
        let (target, name) = {
            let mut sp = self.session_picker.lock().unwrap();
            let name = sp.rename_buffer.take();
            let target = sp.selected_row().and_then(|r| match &r.kind {
                RowKind::Switch(id) => Some(*id),
                _ => None,
            });
            sp.notice = None;
            (target, name)
        };
        if let (Some(session), Some(name), Some(bridge)) =
            (target, name, self.session_picker_bridge.as_ref())
        {
            let now = crate::auto_attach::now_unix_seconds();
            bridge.rename_session(session, name.trim(), now);
        }
        self.session_picker_recompute();
    }

    /// Escape: discard the rename buffer, target unchanged.
    fn session_picker_rename_cancel(&mut self) {
        let mut sp = self.session_picker.lock().unwrap();
        sp.rename_buffer = None;
        sp.notice = None;
    }

    /// Accept the highlighted session-picker row: **switch** to it if it's
    /// an existing live session, or **create + switch** if it's an emoji
    /// preset / a typed name. If nothing is highlighted but the operator
    /// typed a non-empty query, create a session named by that query
    /// (create-on-miss). Then close. Inert + just-closes when switching is
    /// disabled.
    fn session_picker_accept(&mut self) {
        use crate::session_picker::{CreateSpec, RowKind};

        // Positional stability: if an autonomous re-list swapped the TOP row
        // within the grace window and the operator is still resting there,
        // this Enter may be aimed at a row they never saw — swallow it (the
        // board is now stable; the next Enter fires normally).
        let resting = {
            let sp = self.session_picker.lock().unwrap();
            sp.open && sp.selected == 0
        };
        if resting
            && self
                .board_shift_at
                .take()
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }

        let (chosen, query) = {
            let sp = self.session_picker.lock().unwrap();
            (sp.selected_row().map(|r| r.kind.clone()), sp.query.clone())
        };
        if let Some(bridge) = self.session_picker_bridge.as_ref() {
            let now = crate::auto_attach::now_unix_seconds();
            let ok = match chosen {
                Some(RowKind::Switch(session)) => bridge.switch_to(session),
                Some(RowKind::Instantiate(def_id)) => bridge.instantiate_and_switch(def_id, now),
                Some(RowKind::Suggestion(id)) => bridge.spawn_suggestion(id, now),
                Some(RowKind::Create(spec)) => bridge.create_and_switch(spec, now),
                // Nothing highlighted but a non-empty needle → create a
                // session named literally by the query (create-on-miss).
                None => {
                    let q = query.trim();
                    if q.is_empty() {
                        true
                    } else {
                        bridge.create_and_switch(CreateSpec::Named { name: q.to_owned() }, now)
                    }
                }
            };
            if !ok {
                // The target vanished between listing and Enter (resolved
                // upstream, session reaped, spawn failed). Tell the operator
                // ON the board and keep it open with a fresh list — a silent
                // close would read as "it worked".
                tracing::warn!(
                    "session picker: accept produced no switch (session reaped or spawn failed)"
                );
                // Reset-to-top re-list here (NOT the identity-preserving
                // autonomous refresh): a failed accept deliberately teleports
                // the cursor to row 0 and stamps the stability window below, so
                // a rapid second Enter can't fire at a row the operator never
                // aimed at. The live flow's cursor-preservation is for the
                // autonomous tick, not this operator-initiated failure path.
                self.session_picker_relist();
                // The re-list teleported the highlight back to row 0. If the
                // operator was mid-list, a rapid second Enter would fire at
                // the top row they never aimed at — stamp the stability
                // window so that Enter is swallowed (autorefresh only stamps
                // when the TOP row's identity changed, not a cursor move).
                if !resting {
                    self.board_shift_at = Some(Instant::now());
                }
                let mut sp = self.session_picker.lock().unwrap();
                if sp.open {
                    sp.notice =
                        Some(String::from("could not start that — it may have just resolved"));
                }
                return;
            }
        }
        self.session_picker.lock().unwrap().close();
    }

    /// Paste the system clipboard into the PTY through the M0
    /// PasteGuard (`clipboard_store::sanitize_paste`): strips bytes
    /// that would break out of (or fake) bracketed-paste framing —
    /// without this, a clipboard containing ESC[201~ executes the
    /// rest of the paste as keystrokes (classic paste injection).
    fn paste(&mut self) {
        // ghostty parity: a clipboard IMAGE becomes a temp PNG whose
        // path we paste, so a file-loading TUI (Claude Code, $EDITOR)
        // receives the image — OSC 52 can't carry image bytes, the path
        // bridges it. Probe the image first: a copied screenshot carries
        // no text, and when a source puts both on the clipboard the
        // image is what the operator means to paste.
        if let Ok(img) = self.clipboard.paste_image() {
            match crate::clipboard_store::write_clipboard_png(
                img.width as u32,
                img.height as u32,
                &img.rgba,
            ) {
                Ok(path) => {
                    self.write_paste(&path.to_string_lossy());
                    return;
                }
                // Encode/IO failure — fall back to text so a paste is
                // never silently dropped.
                Err(e) => {
                    tracing::warn!(error = %e, "clipboard image paste failed; falling back to text");
                }
            }
        }
        if let Ok(pasted) = self.clipboard.paste_text() {
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
        // must not resurrect the dead selection. (Machine contract:
        // `TypedInput` is effect-free; the matrix test pins it.)
        let step = self.pointer.on_event(PointerEvent::TypedInput);
        debug_assert!(step.effects.is_empty(), "TypedInput is effect-free by contract");
        self.pointer = step.state;
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
    ///
    /// Left-button routing is decided by the pointer machine
    /// (`ux::modes::Pointer`): `route_left_press` is the ONE place
    /// the forward-vs-terminal-side split lives, and the shift
    /// bypass rides INSIDE the machine state — release/motion arms
    /// consume it as a transition guard, not as a flag read here.
    /// Shift is the operator's escape hatch (xterm/kitty/ghostty
    /// convention): shift+click bypasses tracking and selects text
    /// terminal-side — without it, selection is impossible inside
    /// vim/tmux/htop (hunt finding 2026-06-11).
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
        let tracking_on = mouse_mode != MouseMode::Off;
        let shift_local = modifiers.shift && !self.behavior.mouse_shift_capture;

        if button == MouseButton::Left {
            let event = if pressed {
                PointerEvent::LeftPress(match modes::route_left_press(tracking_on, shift_local) {
                    modes::PressRouting::Forward => PressRoute::Forward,
                    modes::PressRouting::Select { bypass } => PressRoute::Select {
                        bypass,
                        plan: self.classify_selection_press(row, col, modifiers),
                    },
                })
            } else {
                // Release routing is STATE-DERIVED in the pointer FSM
                // now (copy-on-release regression, 2026-06-12) — the
                // event carries no tracking facts. A forwarded press
                // releases to the app (ForwardedPress state); every
                // drag-ending state commits the selection.
                PointerEvent::LeftRelease
            };
            let step = self.pointer.on_event(event);
            self.pointer = step.state;
            for effect in step.effects {
                self.run_pointer_effect(effect, col, row, modifiers, sgr);
            }
            return EventOutcome::consumed();
        }

        // Middle/right forwarding when tracking is active (same
        // guard the machine's press routing applies to Left). The
        // unified engine forwards all three buttons with real
        // modifier bits (Shift 4 / Meta 8 / Ctrl 16).
        if tracking_on && !shift_local {
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
        if button == MouseButton::Middle && pressed {
            let pasted = self.clipboard.paste_text().ok();
            if let Some(text) = pasted {
                self.write_paste(&text);
            }
        }
        EventOutcome::consumed()
    }

    /// The impure half of a terminal-side left press: multi-click
    /// cadence + content-anchor capture, resolved into a typed
    /// [`PressPlan`] the pointer machine consumes. Runs ONLY when
    /// the press routed terminal-side — a forwarded press must not
    /// advance the click cadence (pre-FSM contract: the forward
    /// block returned before the cadence update).
    fn classify_selection_press(
        &mut self,
        row: usize,
        col: usize,
        modifiers: Modifiers,
    ) -> PressPlan {
        // Shift+click EXTENDS an existing selection to the click
        // point (xterm/kitty/ghostty convention) and keeps dragging
        // from the surviving start anchor. An extension is not a
        // multi-click: the cadence state stays untouched. No
        // existing selection → nothing to extend — fall through to
        // the plain click path below.
        if modifiers.shift {
            let existing = self.selection.lock().unwrap().anchors();
            if let Some((start, _)) = existing {
                if let Some(click) = self.terminal.read().selection_anchor_at(row, col) {
                    return PressPlan::Extend { start, click };
                }
                return PressPlan::Unanchored;
            }
        }

        let now = Instant::now();
        let same_pos = self.last_click_pos.row == row && self.last_click_pos.col == col;
        let quick = now.duration_since(self.last_click_time).as_millis() < 400;
        if same_pos && quick {
            self.click_count = (self.click_count + 1).min(3);
        } else {
            self.click_count = 1;
        }
        self.last_click_time = now;
        self.last_click_pos = CellPos { row, col };

        match self.click_count {
            // Double-click: select word; the snapped word is the drag
            // origin every word-drag union keeps fully selected.
            2 => self
                .word_span_at(row, col)
                .map_or(PressPlan::Unanchored, |span| PressPlan::Word { span }),
            // Triple-click: select entire (physical) line.
            3 => self
                .line_span_at(row)
                .map_or(PressPlan::Unanchored, |span| PressPlan::Line { span }),
            _ => self
                .terminal
                .read()
                .selection_anchor_at(row, col)
                .map_or(PressPlan::Unanchored, |anchor| PressPlan::Char { anchor }),
        }
    }

    /// Execute one typed pointer effect at the event's cell coords.
    /// Pure decisions live in the machine (`ux::modes`); this is the
    /// I/O edge: PTY reports through the [`PtySink`], selection
    /// mutation, the release ritual (finish / copy-on-select / URL
    /// open).
    fn run_pointer_effect(
        &mut self,
        effect: PointerEffect,
        col: usize,
        row: usize,
        modifiers: Modifiers,
        sgr: bool,
    ) {
        match effect {
            PointerEffect::ForwardPress | PointerEffect::ForwardRelease => {
                let report = MouseReport {
                    kind: if matches!(effect, PointerEffect::ForwardPress) {
                        MouseReportKind::Press
                    } else {
                        MouseReportKind::Release
                    },
                    button: MouseReportButton::Left,
                    col: col + 1,
                    row: row + 1,
                    mods: MouseMods::from(modifiers),
                };
                self.pty.write(&report.encode(sgr));
            }
            // The machine emits ForwardMotion only under SGR (the
            // pre-FSM motion path never encoded non-SGR motion).
            PointerEffect::ForwardMotion { button_down } => {
                let report = MouseReport {
                    kind: MouseReportKind::Motion,
                    button: if button_down {
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
            PointerEffect::StartSelection(anchor) => {
                self.selection.lock().unwrap().start(anchor);
            }
            PointerEffect::SetSpan(a, b) => {
                self.selection.lock().unwrap().set_span(a, b);
            }
            PointerEffect::UpdateDrag { mode, origin } => {
                self.update_drag(mode, origin, row, col);
            }
            PointerEffect::CompleteOrphanedDrag => {
                // Copy-on-release RECOVERY (operator report 2026-06-12):
                // a release the adapter dropped (e.g. an early-return-
                // on-title drain) left this drag's highlight live. A new
                // press is landing — commit the orphaned selection to
                // the clipboard FIRST (same muscle-memory copy the
                // release would have done), then let the press's own
                // effects run. No `finish()` (the drag is abandoned by
                // the incoming press), no URL-open (that's release-only
                // single-click semantics).
                self.copy_live_selection_if_enabled();
            }
            PointerEffect::SelectionRelease => {
                // A zero-length single click leaves the selection empty
                // (`finish()` → None); a drag leaves a committed span. The
                // emptiness AFTER finish is how we tell a plain click (which
                // may open a link) from a text-selecting drag (which must
                // NOT open one — that would hijack selection).
                let mut plain_click = false;
                if self.click_count == 1 {
                    self.selection.lock().unwrap().finish();
                    plain_click = self.selection.lock().unwrap().anchors().is_none();
                }
                // Muscle-memory contract: the highlight goes straight
                // to the clipboard on release for EVERY selection
                // shape — drag, double-click word, triple-click line
                // (word/line used to be excluded by the click_count
                // gate).
                self.copy_live_selection_if_enabled();
                // Lift-to-copy: the highlight is now on the clipboard, so drop
                // it — lifting the mouse both COPIES and UNHIGHLIGHTS, and no
                // click is ever needed to copy. Gated on `copy_on_select`
                // (something was auto-copied) AND `deselect_on_copy` (default
                // on), so the copy-keeps-highlight behavior stays available via
                // config, and the bare/copy-off tier keeps the selection live
                // for a manual Cmd+C. `plain_click` was already read above, so
                // clearing here never affects the link-open decision below; an
                // empty (plain-click) selection clears as a harmless no-op.
                if self.behavior.copy_on_select && self.behavior.deselect_on_copy {
                    self.selection.lock().unwrap().clear();
                }
                if self.click_count == 1 {
                    // Open a clickable link on release — single-click only
                    // (word/line selection has click_count != 1, skipped).
                    // A plain (no-drag) click opens when `links.open_on_click`
                    // is set; the legacy Cmd+click (macOS) / Ctrl+click
                    // (Linux) affordance still works whenever links are
                    // enabled. Never on a drag (would hijack text selection).
                    let modified = modifiers.meta || modifiers.ctrl;
                    let want_open = (plain_click && self.links.open_on_click)
                        || (modified && self.links.enabled);
                    if want_open {
                        self.try_open_link_at(row, col);
                    }
                }
            }
        }
    }

    // ── Selection helpers (anchor capture + drag FSM) ────────────

    /// Extract the active selection's text through the soft-wrap-
    /// aware content walk. The ONE copy surface — `Action::Copy` and
    /// `copy_on_select` both route here.
    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection.lock().unwrap().anchors()?;
        self.terminal.read().extract_selection_text(a, b)
    }

    /// Commit the currently-live selection to the system clipboard
    /// when `copy_on_select` is enabled. The shared muscle-memory copy
    /// the `SelectionRelease` ritual and the `CompleteOrphanedDrag`
    /// recovery both route through, so a release dropped upstream still
    /// lands the highlight on the clipboard at the next press.
    fn copy_live_selection_if_enabled(&self) {
        let release_copy = self
            .behavior
            .copy_on_select
            .then(|| self.selected_text())
            .flatten();
        if let Some(text) = release_copy {
            let _ = self.clipboard.copy_text(&text);
        }
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
            // The operator knob, not "" — a hard-coded empty set left
            // config.selection.word_chars dead (M3 review 2026-06-12).
            &self.behavior.word_chars,
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

        // Motion routing is structural in the pointer machine:
        // forwarding with the spec-correct code — 1002 (ButtonEvent)
        // only while a button is held; 1003 (AnyEvent) always, hover
        // motion carrying the no-button code (35), never a
        // fabricated left-drag (32) — from tracked, un-bypassed
        // states; one drag tick ONLY from `Selecting` (gated by the
        // machine, not by selection liveness — a committed shift-
        // extended span is `Selected` yet still dragging). The
        // button bit derives from the state, so it cannot desync
        // from the drag lifecycle.
        let step = self.pointer.on_event(PointerEvent::Motion {
            mode: mouse_mode,
            sgr,
        });
        self.pointer = step.state;
        for effect in step.effects {
            self.run_pointer_effect(effect, col, row, self.last_mods, sgr);
        }

        // Clickable-link hover: request the hand/`Pointer` cursor over a link
        // cell when `links.pointer_cursor` is set, else the platform default.
        // `None` leaves the cursor unchanged. The typed shape rides on the
        // outcome; OS application awaits a madori cursor-icon channel (see
        // `ux::outcome` + the OSC 22 pointer-shape precedent).
        let pointer_shape = self.links.pointer_cursor.then(|| {
            if self.link_url_at(row, col).is_some() {
                crate::pointer_shape::PointerShape::Pointer
            } else {
                crate::pointer_shape::PointerShape::Default
            }
        });

        let mut outcome = EventOutcome::consumed().with_cursor_visibility(vis);
        outcome.set_pointer_shape = pointer_shape;
        outcome
    }

    /// Wheel / trackpad scroll. Snapshots the live terminal context, asks the
    /// scroll system ([`ScrollSystem`]) what to do, and executes the typed
    /// [`ScrollAction`]. The decision (wheel-vs-trackpad, momentum, the
    /// precise pixel accumulator, forwarding, alt-scroll) lives entirely in
    /// the system; this is the I/O edge — PTY writes and the viewport scroll.
    pub fn on_mouse_scroll(
        &mut self,
        gesture: ScrollGesture,
        metrics: &dyn FontZoomTarget,
    ) -> EventOutcome {
        // Re-feed the policy so `behavior` stays the single source of truth
        // (cheap — `ScrollConfig` is `Copy`); in-flight kinetic/accumulator
        // state is preserved.
        self.scroll.set_config(self.behavior.scroll_config());
        // Snapshot context under a short read lock, then decide + execute
        // lock-free. `shift_bypass` reserves shift as the operator's
        // scrollback escape from a mouse-tracking app (the
        // xterm/kitty/ghostty convention); wheel events carry no modifiers on
        // the current madori pin, so `last_mods` (fed by every key/button
        // event, and Shift itself emits a key event) is the source.
        let (mouse_tracking, alt_screen, sgr) = {
            let term = self.terminal.read();
            (
                term.mouse_mode() != MouseMode::Off,
                term.is_alternate_screen(),
                term.sgr_mouse(),
            )
        };
        let ctx = ScrollContext {
            mouse_tracking,
            shift_bypass: self.last_mods.shift && !self.behavior.mouse_shift_capture,
            alt_screen,
            cell_height: metrics.cell_height(),
        };
        let action = self.scroll.on_gesture(gesture, &ctx);
        self.execute_scroll_action(action, sgr, metrics);
        EventOutcome::consumed()
    }

    /// Execute a [`ScrollAction`] the scroll system produced — the impure
    /// half of the gesture path: viewport scroll under the terminal lock,
    /// wheel-button reports / cursor-key alt-scroll to the PTY.
    fn execute_scroll_action(
        &mut self,
        action: ScrollAction,
        sgr: bool,
        metrics: &dyn FontZoomTarget,
    ) {
        match action {
            ScrollAction::None => {}
            ScrollAction::Viewport { cells } => {
                let n = cells.unsigned_abs() as usize;
                let mut term = self.terminal.write();
                if cells > 0 {
                    term.scroll_up(n);
                } else if cells < 0 {
                    term.scroll_down(n);
                }
            }
            ScrollAction::ForwardWheel { up, count } => {
                // Forward to a mouse-tracking app with TRUE cell coords from
                // the tracked pointer (wheel events carry none of their own).
                let (col, row) = self.wheel_cell(metrics);
                let report = MouseReport {
                    kind: MouseReportKind::Press,
                    button: if up {
                        MouseReportButton::WheelUp
                    } else {
                        MouseReportButton::WheelDown
                    },
                    col: col + 1,
                    row: row + 1,
                    mods: MouseMods::NONE,
                };
                let bytes = report.encode(sgr);
                for _ in 0..count {
                    self.pty.write(&bytes);
                }
            }
            ScrollAction::ForwardArrows { up, count } => {
                // Alt-screen alt-scroll (xterm 1007): map to arrow keys so
                // less/man/vim scroll their CONTENT. DECCKM picks the
                // encoding the app negotiated.
                let app_mode = (self.cursor_keys_mode)();
                let seq: &[u8] = match (up, app_mode) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                let mut out = Vec::with_capacity(seq.len() * count);
                for _ in 0..count {
                    out.extend_from_slice(seq);
                }
                self.pty.write(&out);
            }
        }
    }

    /// Clamped (col, row) for the tracked pointer — the cell a forwarded
    /// wheel report names. Wheel events carry no coordinates, so they use
    /// `last_mouse_pos` (closing the pre-M1 fake-`1;1` gap). Cell metrics are
    /// guarded with `.max(1.0)` so a scroll before the first measured frame
    /// can't divide by zero.
    fn wheel_cell(&self, metrics: &dyn FontZoomTarget) -> (usize, usize) {
        let cw = metrics.cell_width().max(1.0);
        let ch = metrics.cell_height().max(1.0);
        let pad_phys = self.padding * metrics.scale_factor();
        let col = ((self.last_mouse_pos.0 as f32 - pad_phys) / cw).max(0.0) as usize;
        let row = ((self.last_mouse_pos.1 as f32 - pad_phys) / ch).max(0.0) as usize;
        let (term_cols, term_rows) = {
            let term = self.terminal.read();
            (term.cols(), term.rows())
        };
        (
            col.min(term_cols.saturating_sub(1)),
            row.min(term_rows.saturating_sub(1)),
        )
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

    /// A file was drag-and-dropped onto the window: insert its
    /// shell-quoted path into the PTY as a bracketed paste — the
    /// ghostty contract, so a dragged screenshot becomes a path a TUI
    /// (Claude Code, `$EDITOR`) or the shell can open. Routed through
    /// the ONE guarded paste write (`write_paste`) so it gets the same
    /// PasteGuard sanitization + bracketed framing as any other paste,
    /// with a trailing space so successive drops stay separated.
    pub fn drop_file(&mut self, path: &std::path::Path) -> EventOutcome {
        let mut s = crate::dir_picker::shell_quote_path(&path.to_string_lossy());
        s.push(' ');
        self.write_paste(&s);
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
        // The living Ctrl-S board (stage 3). Two intertwined signals per tick:
        //
        // 1. EVENT-DRIVEN — the store broadcast (`watch::Receiver::has_changed`,
        //    one cheap atomic read per frame when idle). Consumed picker open
        //    or closed: closed it feeds the ambient Critical attention latch;
        //    open + resting it re-lists the board immediately.
        // 2. COARSE WHOLE-BOARD TICK (~3s, open + resting only) — reconciles
        //    the session registry (out-of-band spawns/deaths appear live, not
        //    just suggestions) and advances the age/aging labels even when the
        //    store is silent. Wall-clock drift is part of the living board.
        //
        // The resting gate means a refresh never yanks the cursor out from
        // under someone mid-navigation (changes are consumed on the next rest);
        // the positional-stability stamp inside autorefresh guards the
        // Enter-vs-shift race at the top row.
        if self.session_picker_bridge.is_some() {
            let changed = self
                .suggest_rx
                .as_mut()
                .is_some_and(|rx| rx.has_changed().unwrap_or(false));
            if changed {
                if let Some(rx) = self.suggest_rx.as_mut() {
                    rx.mark_unchanged();
                }
                self.notice_new_criticals();
            }
            // Keep the board FLOWING whenever it is open — even while the
            // operator navigates or searches. `session_picker_autorefresh`
            // preserves the highlighted row by identity, so the live re-list
            // no longer yanks the cursor (the old `resting`/selected==0 gate
            // is gone); it just keeps the set the operator is driving across
            // fresh. The positional-stability stamp still guards the Enter race.
            let open = self.session_picker.lock().unwrap().open;
            if open {
                let coarse_due = self
                    .last_board_tick
                    .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(3));
                if changed || coarse_due {
                    self.last_board_tick = Some(Instant::now());
                    self.session_picker_autorefresh();
                }
            }
        }
        // Momentum + selection auto-scroll: advance the kinetic
        // sub-state and apply the resulting viewport delta. Runs AFTER
        // selection reconcile so a dangled drag is already cleared. The
        // wall-clock → dt step lives here so the driver itself is pure
        // in `dt` (deterministic + unit-testable).
        let selecting = matches!(self.pointer, Pointer::Selecting { .. });
        if !self.scroll.is_active() && !selecting {
            // Fast path: a still kinetics with no live drag means no
            // momentum and no auto-scroll. Skip the wall-clock read and
            // reset the marker so the FIRST frame after the next impulse
            // sees a fresh (≈0) dt — keeping that first kinetics frame a
            // no-op (the L1/L2 determinism contract).
            self.last_scroll_tick = None;
            return;
        }
        let now = Instant::now();
        let dt = self
            .last_scroll_tick
            .map_or(0.0, |prev| now.duration_since(prev).as_secs_f32());
        self.last_scroll_tick = Some(now);
        self.tick_scroll_kinetics(dt, renderer);
    }

    /// Per-frame momentum + selection auto-scroll driver. Computes the
    /// real frame `dt`, lets an active selection drag past a viewport
    /// edge drive a sustained velocity, advances the kinetics, applies
    /// the whole-line delta through `scroll_up`/`scroll_down` with wall
    /// clamping, and re-extends the highlight to the revealed edge.
    ///
    /// `dt == 0.0` (the very first tick) is a strict no-op inside
    /// `ScrollKinetics::tick`, keeping the L1/L2 determinism ladders
    /// byte-stable when the engine is driven headless.
    fn tick_scroll_kinetics(&mut self, dt: f32, metrics: &dyn FontZoomTarget) {
        // Keep the policy in sync with `behavior` (the source of truth) so an
        // auto-scroll speed/overshoot edit — or a momentum-tuning change —
        // takes effect; `ScrollConfig` is `Copy`, so this is trivial.
        self.scroll.set_config(self.behavior.scroll_config());

        // ── Selection auto-scroll: a live drag past a viewport edge drives a
        // sustained velocity through the scroll system. The system owns the
        // speed/overshoot policy (config knobs); the engine owns the
        // pointer-state gate + the raw overshoot measurement.
        let selecting = matches!(self.pointer, Pointer::Selecting { .. });
        if self.behavior.selection_autoscroll && selecting {
            // RAW pointer row (NOT the clamped cell — we need to SEE the
            // past-edge overshoot). Pixel → row via the same physical-padding
            // math the renderer draws with; cell height guarded for the
            // pre-first-frame case.
            let ch = metrics.cell_height().max(1.0);
            let pad_phys = self.padding * metrics.scale_factor();
            let raw_row = (self.last_mouse_pos.1 as f32 - pad_phys) / ch;
            let rows = self.terminal.read().rows() as f32;
            self.scroll.update_autoscroll(raw_row, rows);
        } else {
            // No live drag (or auto-scroll disabled) → a sustained drive must
            // not outlive its drag.
            self.scroll.end_autoscroll();
        }

        // ── Advance momentum and apply the whole-cell delta.
        let delta = self.scroll.tick(dt);
        if delta != 0 {
            let mut term = self.terminal.write();
            // Wall clamp: at the scrollback top (offset == total) or the live
            // bottom (offset == 0) halt the drive so momentum doesn't grind
            // against the bound for the rest of its decay.
            let offset = term.scroll_offset();
            let total = term.scrollback_total();
            if delta > 0 {
                if offset >= total {
                    self.scroll.stop();
                } else {
                    term.scroll_up(delta.unsigned_abs() as usize);
                    if term.scroll_offset() >= term.scrollback_total() {
                        self.scroll.stop();
                    }
                }
            } else if offset == 0 {
                self.scroll.stop();
            } else {
                term.scroll_down(delta.unsigned_abs() as usize);
                if term.scroll_offset() == 0 {
                    self.scroll.stop();
                }
            }
            drop(term);

            // ── Re-extend the selection to the revealed edge so the
            // highlight grows with the scroll. Resolve the clamped edge
            // cell (top row for an up-scroll, bottom row for a down-
            // scroll) and route it through the SAME drag-update path a
            // motion event would.
            if self.behavior.selection_autoscroll {
                if let Pointer::Selecting { mode, origin, .. } = self.pointer {
                    let (rows, cols) = {
                        let term = self.terminal.read();
                        (term.rows(), term.cols())
                    };
                    if rows > 0 && cols > 0 {
                        // Pointer is past an edge → clamp the column to
                        // the raw pointer column, the row to the edge.
                        let cw = metrics.cell_width();
                        let pad_phys = self.padding * metrics.scale_factor();
                        let raw_col = ((self.last_mouse_pos.0 as f32 - pad_phys) / cw).max(0.0);
                        let col = (raw_col as usize).min(cols - 1);
                        let edge_row = if delta > 0 { 0 } else { rows - 1 };
                        self.update_drag(mode, origin, edge_row, col);
                    }
                }
            }
        }
    }

    /// Test-only deterministic frame driver: ticks the scroll kinetics
    /// with an EXPLICIT `dt` (bypassing the wall clock) so momentum +
    /// auto-scroll behavior is asserted frame-exactly. Production drives
    /// `tick_scroll_kinetics` from the real per-frame `dt` in
    /// `on_redraw_tick`.
    #[cfg(test)]
    fn tick_scroll_dt(&mut self, dt: f32, metrics: &dyn FontZoomTarget) {
        self.tick_scroll_kinetics(dt, metrics);
    }

    /// Test-only read of the kinetic velocity (lines/sec). `+` up into
    /// history, `-` down toward the tail.
    #[cfg(test)]
    fn scroll_velocity(&self) -> f32 {
        self.scroll.velocity()
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
            // Machine contract: `SelectionDangled` is effect-free;
            // the matrix test pins it.
            let step = self.pointer.on_event(PointerEvent::SelectionDangled);
            debug_assert!(step.effects.is_empty(), "SelectionDangled is effect-free by contract");
            self.pointer = step.state;
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
        // Mode gate reads the MACHINE, not the write-only render
        // mirror `SearchState.active` (M3 review 2026-06-12): an
        // engine decision consuming the mirror made the only-
        // mitigated mirror axis load-bearing, widening any future
        // mirror desync from render glitch to search-reconciler
        // misbehavior. The query emptiness check is data, not mode —
        // the cell stays the right source for it.
        if self.overlay != Overlay::Search {
            return;
        }
        let needs_rerun = !self.search.lock().unwrap().query.is_empty();
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
    use madori::event::KeyCode;
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
        fn apply_layout(
            &self,
            _window: tear_types::WindowId,
            _kind: tear_types::LayoutKind,
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
            Self::new_with_bridge(kind, None)
        }

        /// Build a harness with an optional session-picker bridge —
        /// the seam the Ctrl-S picker tests inject a recording bridge
        /// into.
        fn new_with_bridge(
            kind: SinkKind,
            bridge: Option<Box<dyn crate::session_picker::SessionPickerBridge>>,
        ) -> Self {
            let terminal: SharedTerminal =
                Arc::new(RwLock::new(Terminal::new(80, 24)));
            let mut renderer = TerminalRenderer::new(
                Arc::clone(&terminal),
                14.0,
                1.4,
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
                        // Deselect-on-copy OFF in the harness so the existing
                        // selection tests exercise the copy-keeps-highlight
                        // path; the default-behavior test flips it on via
                        // `harness.behavior`. (Production prescribes it ON —
                        // asserted by the config prescribed-default test.)
                        deselect_on_copy: false,
                        confirm_close: false,
                        mouse_hide_while_typing: false,
                        mouse_scroll_multiplier: 1,
                        mouse_shift_capture: false,
                        word_chars: String::new(),
                        // Momentum + auto-scroll default OFF in the
                        // harness so the existing wheel/selection tests
                        // exercise the direct path; momentum tests flip
                        // these on explicitly via `harness.behavior`.
                        scroll_momentum: false,
                        scroll_friction: 3.0,
                        scroll_max_velocity: 120.0,
                        selection_autoscroll: false,
                        // Precise path at a literal 1:1 gain so harness math is
                        // "cell_height px ⇒ 1 cell"; autoscroll tuning present
                        // but gated off by selection_autoscroll: false. Tests
                        // that exercise these flip them via `harness.behavior`.
                        precise_scroll_mode: crate::config::PreciseScrollMode::Pixels,
                        precise_scroll_multiplier: 1.0,
                        selection_autoscroll_speed: 18.0,
                        selection_autoscroll_max_overshoot: 6.0,
                    },
                    // Links on in the harness so the click/hover paths are exercised.
                    links: crate::config::MadoLinksConfig::prescribed(),
                    cursor_keys_mode,
                    default_font_size: 14.0,
                    padding: 0.0,
                    session_picker_bridge: bridge,
                    // Attention OFF in the harness: tests must never bounce
                    // the real dock (platform call) from the tick path.
                    suggest_attention: false,
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

        /// Move the pointer to RAW physical pixels (may be off-grid /
        /// negative) — drives the selection-autoscroll edge detection,
        /// which reads `last_mouse_pos` directly, not the clamped cell.
        fn moved_px(&mut self, x: f64, y: f64) {
            self.engine.on_mouse_moved(x, y, &self.renderer);
        }

        /// One discrete wheel notch (`ticks > 0` = up into history).
        fn scroll(&mut self, ticks: f64) {
            self.engine
                .on_mouse_scroll(ScrollGesture::Wheel { ticks }, &self.renderer);
        }

        /// One precise (trackpad) gesture of `pixels` physical px
        /// (`> 0` = up into history).
        #[allow(dead_code)]
        fn scroll_precise(&mut self, pixels: f64) {
            self.engine
                .on_mouse_scroll(ScrollGesture::Precise { pixels }, &self.renderer);
        }

        /// One per-frame redraw tick — advances the scroll kinetics.
        /// (Momentum tests drive `tick_dt` for determinism; kept for
        /// wall-clock-shaped tests.)
        #[allow(dead_code)]
        fn tick(&mut self) {
            self.engine.on_redraw_tick(&self.renderer);
        }

        /// Deterministic frame tick with an EXPLICIT `dt` (seconds) —
        /// advances the scroll kinetics frame-exactly, bypassing the
        /// wall clock so momentum/auto-scroll assertions are stable.
        fn tick_dt(&mut self, dt: f32) {
            self.engine.tick_scroll_dt(dt, &self.renderer);
        }

        fn scroll_offset(&self) -> usize {
            self.terminal.read().scroll_offset()
        }

        fn velocity(&self) -> f32 {
            self.engine.scroll_velocity()
        }

        /// Pixel y for the top of viewport row `row` (no clamp).
        fn row_top_px(&self, row: f64) -> f64 {
            row * self.renderer.cell_height() as f64
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
        h.scroll(1.0);
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
        h.scroll(1.0);
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
        h.scroll(1.0);
        let sent = h.sent_bytes().concat();
        assert!(
            !sent.is_empty() && sent.chunks(3).all(|c| c == b"\x1b[A"),
            "wheel-up in alt screen must emit CSI A repeats: {sent:?}"
        );
        // Application cursor-keys mode flips the encoding to SS3.
        h.feed(b"\x1b[?1h");
        h.clear_sent();
        h.scroll(-1.0);
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

    /// **Copy-on-release totality** (operator report 2026-06-12): a
    /// plain drag-select followed by a release ALWAYS lands the
    /// highlight on the clipboard — the release routing is now
    /// state-derived, so there is no event-time path that forwards a
    /// drag's release to the app instead of copying. The pre-fix
    /// `release_routing(tracking_on, shift_local)` branch could route a
    /// drag's release to the app when tracking flipped on mid-drag.
    #[test]
    fn drag_release_always_copies_the_selection() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(4, 0);
        // Mouse tracking arms MID-DRAG (a TUI enabling SGR mouse while
        // the operator is dragging) — the pre-fix event-time release
        // routing would have forwarded this release. State-derived
        // routing copies regardless.
        h.feed(b"\x1b[?1006h\x1b[?1000h");
        h.button(MouseButton::Left, false, 4, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "hello",
            "a drag's release must commit the selection even if tracking armed mid-drag"
        );
    }

    /// **Lift-to-copy default** (operator directive 2026-07-06): with
    /// `deselect_on_copy` on (the production default), a drag-select's
    /// release BOTH copies the highlight AND clears it — so lifting the
    /// mouse copies and unhighlights, and no click is ever needed to copy.
    #[test]
    fn deselect_on_copy_lift_copies_and_unhighlights() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        h.engine.behavior.deselect_on_copy = true; // the production default
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(4, 0);
        h.button(MouseButton::Left, false, 4, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "hello",
            "the lift must copy the highlight"
        );
        assert!(
            !h.engine.selection.lock().unwrap().is_active(),
            "the lift must clear the highlight when deselect_on_copy is on"
        );
    }

    /// **Config opt-out** (expand-not-replace, 2026-07-06): with
    /// `deselect_on_copy` OFF the release still auto-copies but KEEPS the
    /// highlight live — the pre-2026-07-06 copy-without-deselect behavior,
    /// preserved as a config, not coded away.
    #[test]
    fn deselect_off_keeps_the_highlight_after_the_release_copy() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        h.engine.behavior.deselect_on_copy = false;
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(4, 0);
        h.button(MouseButton::Left, false, 4, 0, no_mods());
        assert_eq!(h.clipboard.paste_text().unwrap(), "hello");
        assert!(
            h.engine.selection.lock().unwrap().is_active(),
            "deselect_on_copy=false must keep the highlight live after the copy"
        );
    }

    /// **Orphaned-drag recovery** (operator report 2026-06-12, the
    /// copy-on-release regression's safety net): if a release is
    /// DROPPED upstream (the early-return-on-title drain bug ate it),
    /// the highlight stays live; the operator's NEXT press commits the
    /// stranded selection before starting the new gesture. This is the
    /// `Selecting × LeftPress → CompleteOrphanedDrag` arm, exercised
    /// through the real engine: a press with no intervening release.
    #[test]
    fn next_press_recovers_an_orphaned_drag_highlight() {
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"hello world");
        // Drag-select "hello" — but NEVER send the release (simulating
        // a release the adapter dropped on the title-OSC tick).
        h.button(MouseButton::Left, true, 0, 0, no_mods());
        h.moved(4, 0);
        // Nothing copied yet — the release that would have copied was
        // lost.
        assert!(
            h.clipboard.paste_text().unwrap_or_default().is_empty(),
            "no copy should have happened without a release"
        );
        // The operator clicks elsewhere — the orphaned highlight lands
        // on the clipboard at this press.
        h.button(MouseButton::Left, true, 8, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "hello",
            "a dropped release's highlight must reach the clipboard at the next press"
        );
    }

    /// Config round-trip for `selection.word_chars` (M3 review
    /// 2026-06-12): a custom boundary set must change double-click
    /// bounds through the PRODUCTION call path (engine `word_span_at` →
    /// `selection::word_bounds_in_row`). The rule-level matrix in
    /// selection.rs already proves the snap rule honors the set; this
    /// pins the wiring that used to hard-code `""`.
    #[test]
    fn double_click_word_snap_honors_configured_word_chars() {
        // Default rule (empty set): ':' is a boundary, the click
        // grabs only "path".
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"path:to:file next");
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        assert_eq!(h.clipboard.paste_text().unwrap(), "path");

        // Configured boundary set " " (space only): ':' becomes
        // word-interior, the same double-click grabs the whole
        // colon-joined token — proof the knob reaches the snap rule.
        let mut h = Harness::new(SinkKind::Closure);
        h.engine.behavior.word_chars = " ".into();
        h.feed(b"path:to:file next");
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        h.button(MouseButton::Left, true, 2, 0, no_mods());
        h.button(MouseButton::Left, false, 2, 0, no_mods());
        assert_eq!(
            h.clipboard.paste_text().unwrap(),
            "path:to:file",
            "configured boundary set must keep ':' word-interior on double-click"
        );
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

    #[test]
    fn paste_image_writes_and_pastes_a_temp_png_path() {
        use hasami::ClipboardImage;
        for kind in [SinkKind::Closure, SinkKind::Control] {
            let mut h = Harness::new(kind);
            h.feed(b"\x1b[?2004h"); // arm bracketed paste
            // A 1×1 opaque-red image on the clipboard, no text — the
            // copied-screenshot shape.
            h.clipboard
                .set_image(ClipboardImage { width: 1, height: 1, rgba: vec![255, 0, 0, 255] });
            let out = h.engine.apply_action(Action::Paste, &mut h.renderer);
            assert!(matches!(out, ActionOutcome::Consumed(_)));
            let sent = h.sent_bytes();
            assert_eq!(sent.len(), 3, "the pasted path is bracketed-framed");
            assert_eq!(sent[0], b"\x1b[200~".to_vec());
            assert_eq!(sent[2], b"\x1b[201~".to_vec());
            let path = String::from_utf8(sent[1].clone()).expect("path is utf8");
            assert!(
                path.contains("mado-clip-") && path.ends_with(".png"),
                "pasted a mado clipboard-png path, got {path:?}"
            );
            // The path points at a REAL decodable PNG of the right size —
            // exactly what a file-loading TUI reads back.
            let bytes = std::fs::read(&path).expect("temp png exists on disk");
            let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                .expect("pasted file is a valid png");
            assert_eq!((img.width(), img.height()), (1, 1));
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn paste_falls_back_to_text_when_no_image() {
        // No image on the clipboard ⇒ the existing text paste path is
        // unchanged (the image probe returns Empty and we fall through).
        let mut h = Harness::new(SinkKind::Closure);
        h.feed(b"\x1b[?2004h");
        h.clipboard.copy_text("plain text").unwrap();
        h.engine.apply_action(Action::Paste, &mut h.renderer);
        assert_eq!(
            h.sent_bytes(),
            vec![b"\x1b[200~".to_vec(), b"plain text".to_vec(), b"\x1b[201~".to_vec()],
            "no image ⇒ ordinary bracketed text paste"
        );
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

    /// The overlay machine is authoritative for routing; the
    /// renderer-shared `SearchState.active` / `DirPickerState.open`
    /// cells are write-only mirrors maintained by
    /// `apply_overlay_step`. This pins the mirror in sync across
    /// open / switch / close (tier-honest: the mirror pair is
    /// only-mitigated — one writer + this test, not a type).
    #[test]
    fn overlay_machine_state_mirrors_shared_cells() {
        let mut h = Harness::new(SinkKind::Closure);
        let mut failures: Vec<String> = Vec::new();
        let mut check = |h: &Harness, want: Overlay, when: &str| {
            let search_active = h.engine.search.lock().unwrap().active;
            let dp_open = h.engine.dir_picker.lock().unwrap().open;
            let sp_open = h.engine.session_picker.lock().unwrap().open;
            // (search_active, dir_picker_open, session_picker_open).
            let want_cells = match want {
                Overlay::None => (false, false, false),
                Overlay::Search => (true, false, false),
                Overlay::DirPicker => (false, true, false),
                // The rename sub-mode keeps the session-picker board open.
                Overlay::SessionPicker | Overlay::SessionRename => (false, false, true),
            };
            if h.engine.overlay != want || (search_active, dp_open, sp_open) != want_cells {
                failures.push(format!(
                    "{when}: machine={:?} (want {want:?}), mirror=(search {search_active}, \
                     dir {dp_open}, session {sp_open}) want {want_cells:?}",
                    h.engine.overlay
                ));
            }
        };
        check(&h, Overlay::None, "fresh engine");
        h.engine.apply_action(Action::SearchOpen, &mut h.renderer);
        check(&h, Overlay::Search, "after SearchOpen");
        // Injected switch: the single-enum machine closes the search
        // bar when the picker opens over it (decision 2026-06-12).
        h.engine.apply_action(Action::DirPickerOpen, &mut h.renderer);
        check(&h, Overlay::DirPicker, "after DirPickerOpen over Search");
        // Session picker switches over the dir picker (same single-enum
        // switch semantics — dir picker closes, session picker opens).
        h.engine.apply_action(Action::SessionPickerOpen, &mut h.renderer);
        check(&h, Overlay::SessionPicker, "after SessionPickerOpen over DirPicker");
        // Raw Esc closes the picker (raw-key class, not the atlas).
        h.key(KeyCode::Escape, None, no_mods());
        check(&h, Overlay::None, "after Esc closed the picker");
        assert!(
            failures.is_empty(),
            "{} mirror desyncs:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    // ── Ctrl-S session picker (praça browse + switch) ────────────

    use crate::session_picker::{CreateSpec, RowKind, SessionPickerBridge, SessionPickerRow};

    /// A recording session-picker bridge: returns a fixed frecency-
    /// ranked roster (filtered by a simple substring on `label` so the
    /// engine's filter wiring is exercised) and records every
    /// `switch_to` so a test can assert the SAME-channel post happened.
    struct RecordingBridge {
        roster: Vec<SessionPickerRow>,
        switched: Arc<StdMutex<Vec<SessionId>>>,
    }

    impl RecordingBridge {
        fn new(roster: Vec<(SessionId, &str)>) -> (Self, Arc<StdMutex<Vec<SessionId>>>) {
            let switched = Arc::new(StdMutex::new(Vec::new()));
            let roster = roster
                .into_iter()
                .map(|(id, label)| SessionPickerRow {
                    label: label.to_owned(),
                    kind: RowKind::Switch(id),
                    urgency: None,
                })
                .collect();
            (
                Self {
                    roster,
                    switched: Arc::clone(&switched),
                },
                switched,
            )
        }
    }

    impl SessionPickerBridge for RecordingBridge {
        fn list(&self, query: &str, _now: u64) -> Vec<SessionPickerRow> {
            // Empty query → full frecency-ordered roster (the bridge
            // owns the ranking; mirror praca's empty-query semantics).
            // Non-empty → substring filter on the label (the engine just
            // forwards the typed query; the real bridge fuzzy-matches).
            if query.trim().is_empty() {
                self.roster.clone()
            } else {
                self.roster
                    .iter()
                    .filter(|r| r.label.contains(query))
                    .cloned()
                    .collect()
            }
        }

        fn switch_to(&self, session: SessionId) -> bool {
            self.switched.lock().unwrap().push(session);
            true
        }

        fn create_and_switch(&self, _spec: CreateSpec, _now: u64) -> bool {
            // Create is not exercised by the switch-path tests; record
            // nothing and report no-op.
            false
        }
    }

    /// The `SessionId` a row switches to, for test assertions.
    fn row_switch_id(row: &SessionPickerRow) -> Option<SessionId> {
        match row.kind {
            RowKind::Switch(id) => Some(id),
            RowKind::Instantiate(_) | RowKind::Suggestion(_) | RowKind::Create(_) => None,
        }
    }

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    /// Ctrl-S opens the picker, the bridge's frecency-ordered roster is
    /// listed verbatim (first row highlighted), typing filters the list
    /// through the bridge, Enter switches to the highlighted session
    /// (posting it through the bridge → the SAME switch channel), and
    /// the overlay closes.
    #[test]
    fn session_picker_lists_filters_and_enter_switches() {
        let (bridge, switched) = RecordingBridge::new(vec![
            (sid("tide"), "\u{1f30a} tide  mado"),
            (sid("frost"), "\u{2744} frost  nix"),
            (sid("flow"), "\u{1f343} flow  tear"),
        ]);
        let mut h = Harness::new_with_bridge(SinkKind::Closure, Some(Box::new(bridge)));

        // Ctrl-S opens the picker (the default chord → SessionPickerOpen
        // → OverlayEffect::SessionPickerOpen → bridge.list("")).
        h.engine
            .apply_action(Action::SessionPickerOpen, &mut h.renderer);
        {
            let sp = h.engine.session_picker.lock().unwrap();
            assert!(sp.open, "Ctrl-S opens the picker");
            assert!(!sp.disabled, "a bridge means switching is enabled");
            // Frecency order is the bridge's roster order, verbatim.
            assert_eq!(sp.results.len(), 3);
            assert_eq!(row_switch_id(&sp.results[0]), Some(sid("tide")), "first roster row first");
            assert_eq!(row_switch_id(&sp.results[2]), Some(sid("flow")));
            assert_eq!(sp.selected, 0, "top row highlighted on open");
        }

        // Type "nix" — the filter narrows to the frost row through the
        // bridge (label substring), and the highlight resets to the top.
        for ch in ['n', 'i', 'x'] {
            h.key(KeyCode::Char(ch), Some(&ch.to_string()), no_mods());
        }
        {
            let sp = h.engine.session_picker.lock().unwrap();
            assert_eq!(sp.query, "nix");
            assert_eq!(sp.results.len(), 1, "fuzzy filter narrowed to one");
            assert_eq!(row_switch_id(&sp.results[0]), Some(sid("frost")));
            assert_eq!(sp.selected, 0);
        }

        // Enter switches to the highlighted (only) session — the bridge
        // records the SAME-channel post — and the overlay closes.
        h.key(KeyCode::Enter, None, no_mods());
        assert_eq!(
            switched.lock().unwrap().as_slice(),
            &[sid("frost")],
            "Enter posted the highlighted session through the bridge"
        );
        assert!(
            !h.engine.session_picker.lock().unwrap().open,
            "Enter closes the picker"
        );
        assert_eq!(
            h.engine.overlay,
            Overlay::None,
            "the FSM returns to None after accept"
        );
    }

    /// A bridge whose suggestion rows are driven by a shared cell + a real
    /// `watch` broadcast — the seam the live-stream subscription test drives.
    struct StreamBridge {
        rows: Arc<StdMutex<Vec<SessionPickerRow>>>,
        list_count: Arc<std::sync::atomic::AtomicUsize>,
        tx: tokio::sync::watch::Sender<u64>,
    }

    impl StreamBridge {
        fn new() -> (
            Self,
            Arc<StdMutex<Vec<SessionPickerRow>>>,
            Arc<std::sync::atomic::AtomicUsize>,
            tokio::sync::watch::Sender<u64>,
        ) {
            let (tx, _rx) = tokio::sync::watch::channel(0u64);
            let rows = Arc::new(StdMutex::new(Vec::new()));
            let list_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    rows: Arc::clone(&rows),
                    list_count: Arc::clone(&list_count),
                    tx: tx.clone(),
                },
                rows,
                list_count,
                tx,
            )
        }
    }

    impl SessionPickerBridge for StreamBridge {
        fn list(&self, _query: &str, _now: u64) -> Vec<SessionPickerRow> {
            self.list_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.rows.lock().unwrap().clone()
        }
        fn switch_to(&self, _session: SessionId) -> bool {
            true
        }
        fn create_and_switch(&self, _spec: CreateSpec, _now: u64) -> bool {
            false
        }
        fn suggestion_subscribe(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
            Some(self.tx.subscribe())
        }
    }

    /// The Ctrl-S board CONSUMES the store's change subscription (stage 3):
    /// while the board is open and resting, a tick re-lists ONLY when the store
    /// broadcasts a change — never on a fixed timer. Proves the store→GUI path
    /// is an event-driven subscription, not a one-shot fetch / poll.
    #[test]
    fn session_picker_relists_on_store_broadcast_not_on_timer() {
        use std::sync::atomic::Ordering::SeqCst;
        let (bridge, rows, list_count, tx) = StreamBridge::new();
        let mut h = Harness::new_with_bridge(SinkKind::Closure, Some(Box::new(bridge)));

        // Open the board — the initial list is a single fetch (count == 1).
        h.engine
            .apply_action(Action::SessionPickerOpen, &mut h.renderer);
        assert!(h.engine.session_picker.lock().unwrap().is_resting());
        assert_eq!(list_count.load(SeqCst), 1, "open lists once");

        // Ticks with NO broadcast do NOT re-list — no timer, purely event-driven.
        for _ in 0..5 {
            h.engine.on_redraw_tick(&h.renderer);
        }
        assert_eq!(
            list_count.load(SeqCst),
            1,
            "no store change → no re-list (not a timer)"
        );

        // A source watcher writes fresh rows + broadcasts (what an ingest does).
        rows.lock().unwrap().push(SessionPickerRow {
            label: "\u{25cb} new task".into(),
            kind: RowKind::Switch(sid("task")),
            urgency: None,
        });
        tx.send(1).unwrap();

        // The next tick observes the broadcast → re-lists once → the new row
        // appears on the OPEN board without reopening.
        h.engine.on_redraw_tick(&h.renderer);
        assert_eq!(list_count.load(SeqCst), 2, "the broadcast drove exactly one re-list");
        {
            let sp = h.engine.session_picker.lock().unwrap();
            assert_eq!(sp.results.len(), 1, "the newly-streamed row is on the open board");
            assert_eq!(row_switch_id(&sp.results[0]), Some(sid("task")));
        }

        // Further ticks with no new broadcast → no further re-list (the
        // subscription was consumed; it is not a per-frame poll of the bridge).
        for _ in 0..5 {
            h.engine.on_redraw_tick(&h.renderer);
        }
        assert_eq!(
            list_count.load(SeqCst),
            2,
            "the consumed broadcast does not re-fire every frame"
        );
    }

    /// Arrow / Ctrl-N navigation moves the highlight; Enter switches to
    /// the NEW highlighted session.
    #[test]
    fn session_picker_navigates_then_switches_the_selected() {
        let (bridge, switched) = RecordingBridge::new(vec![
            (sid("a"), "alpha"),
            (sid("b"), "bravo"),
            (sid("c"), "charlie"),
        ]);
        let mut h = Harness::new_with_bridge(SinkKind::Closure, Some(Box::new(bridge)));
        h.engine
            .apply_action(Action::SessionPickerOpen, &mut h.renderer);

        // Down arrow → row 1; Ctrl-N → row 2.
        h.key(KeyCode::Down, None, no_mods());
        h.key(
            KeyCode::Char('n'),
            Some("n"),
            Modifiers {
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            },
        );
        assert_eq!(h.engine.session_picker.lock().unwrap().selected, 2);

        h.key(KeyCode::Enter, None, no_mods());
        assert_eq!(
            switched.lock().unwrap().as_slice(),
            &[sid("c")],
            "Enter switched to the navigated-to session"
        );
    }

    /// A FAILED accept mid-list resets the cursor to row 0 (set_results) —
    /// the positional-stability window must swallow a rapid second Enter so
    /// it can't fire at the top row the operator never aimed at, and the
    /// board stays open with the typed notice.
    #[test]
    fn failed_accept_mid_list_stamps_stability_and_swallows_the_next_enter() {
        /// Every switch fails (session reaped between list and Enter).
        struct FailingBridge {
            attempts: Arc<StdMutex<u32>>,
        }
        impl SessionPickerBridge for FailingBridge {
            fn list(&self, _q: &str, _now: u64) -> Vec<SessionPickerRow> {
                vec![
                    SessionPickerRow {
                        label: "alpha".into(),
                        kind: RowKind::Switch(sid("a")),
                        urgency: None,
                    },
                    SessionPickerRow {
                        label: "bravo".into(),
                        kind: RowKind::Switch(sid("b")),
                        urgency: None,
                    },
                ]
            }
            fn switch_to(&self, _s: SessionId) -> bool {
                *self.attempts.lock().unwrap() += 1;
                false
            }
            fn create_and_switch(&self, _spec: CreateSpec, _now: u64) -> bool {
                false
            }
        }
        let attempts = Arc::new(StdMutex::new(0u32));
        let bridge = FailingBridge {
            attempts: Arc::clone(&attempts),
        };
        let mut h = Harness::new_with_bridge(SinkKind::Closure, Some(Box::new(bridge)));
        h.engine
            .apply_action(Action::SessionPickerOpen, &mut h.renderer);
        h.key(KeyCode::Down, None, no_mods());
        assert_eq!(h.engine.session_picker.lock().unwrap().selected, 1);

        // Enter on row 1 → switch fails → board stays open with the notice,
        // cursor teleported to row 0 by the autorefresh re-list.
        h.key(KeyCode::Enter, None, no_mods());
        assert_eq!(*attempts.lock().unwrap(), 1, "the failed switch was attempted");
        {
            let sp = h.engine.session_picker.lock().unwrap();
            assert!(sp.open, "failed accept keeps the board open");
            assert!(sp.notice.is_some(), "the operator sees why");
            assert_eq!(sp.selected, 0, "re-list reset the cursor");
        }

        // A rapid second Enter is aimed at a row the operator never chose —
        // the stability window swallows it (no second switch attempt).
        h.key(KeyCode::Enter, None, no_mods());
        assert_eq!(
            *attempts.lock().unwrap(),
            1,
            "the teleported-cursor Enter is swallowed by the stability window"
        );
        assert!(
            h.engine.session_picker.lock().unwrap().open,
            "the swallowed Enter leaves the board open"
        );
    }

    /// Esc cancels: no switch is posted + the overlay closes.
    #[test]
    fn session_picker_esc_cancels_without_switching() {
        let (bridge, switched) = RecordingBridge::new(vec![(sid("a"), "alpha")]);
        let mut h = Harness::new_with_bridge(SinkKind::Closure, Some(Box::new(bridge)));
        h.engine
            .apply_action(Action::SessionPickerOpen, &mut h.renderer);
        assert!(h.engine.session_picker.lock().unwrap().open);

        h.key(KeyCode::Escape, None, no_mods());
        assert!(
            !h.engine.session_picker.lock().unwrap().open,
            "Esc closes the picker"
        );
        assert!(
            switched.lock().unwrap().is_empty(),
            "Esc must not post any switch"
        );
        assert_eq!(h.engine.overlay, Overlay::None);
    }

    /// Session-switching disabled (no bridge): Ctrl-S opens an inert
    /// picker flagged `disabled` (the "switching disabled" hint), and
    /// Enter posts nothing — mirroring the `switch_session` MCP tool's
    /// `switching-disabled` answer.
    #[test]
    fn session_picker_inert_when_switching_disabled() {
        // The default harness has NO bridge.
        let mut h = Harness::new(SinkKind::Closure);
        h.engine
            .apply_action(Action::SessionPickerOpen, &mut h.renderer);
        {
            let sp = h.engine.session_picker.lock().unwrap();
            assert!(sp.open, "Ctrl-S still opens the overlay");
            assert!(sp.disabled, "no bridge ⇒ disabled hint");
            assert!(sp.results.is_empty());
        }
        // Enter is inert (no panic, no switch) and closes the overlay.
        h.key(KeyCode::Enter, None, no_mods());
        assert!(
            !h.engine.session_picker.lock().unwrap().open,
            "Enter closes the inert picker"
        );
        assert_eq!(h.engine.overlay, Overlay::None);
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
        h.scroll(1.0);
        assert_eq!(h.sent_bytes(), vec![b"\x1b[<64;11;6M".to_vec()]);
        h.drain_sent();
        h.scroll(-1.0);
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

    // ── Ctrl-S is exclusive to session management ────────────────
    // The INVERSE of the Ctrl-R reaches-PTY tests. Ctrl-S resolves to
    // Action::SessionPickerOpen, which `on_key` dispatches + CONSUMES
    // (engine.rs ~404) BEFORE the `madori_key_to_pty_bytes`
    // translation (~443) ever runs — so 0x13 (XOFF) NEVER reaches the
    // shell. The chord is owned by session management, not the PTY.
    // This holds even in the default no-bridge harness (the picker
    // opens inert) and even over a full-screen TUI. A regression that
    // turned the SessionPickerOpen arm into a FallThrough would leak
    // XOFF and freeze the terminal; these two tests forbid that.
    #[test]
    fn ctrl_s_is_exclusive_to_sessions_never_reaches_pty_closure_sink() {
        let mut h = Harness::new(SinkKind::Closure);
        let out = h.key(
            KeyCode::Char('s'),
            None,
            Modifiers { ctrl: true, alt: false, shift: false, meta: false },
        );
        // Routed to session management (the picker opened)…
        assert!(
            h.engine.session_picker.lock().unwrap().open,
            "Ctrl-S opens the session picker",
        );
        // …and CONSUMED, so no XOFF (0x13) ever reaches the shell.
        assert!(out.consumed, "Ctrl-S is consumed, not forwarded");
        assert!(
            h.sent_bytes().is_empty(),
            "Ctrl-S must NOT leak any byte to the PTY (no 0x13/XOFF); got {:?}",
            h.sent_bytes(),
        );
    }

    #[test]
    fn ctrl_s_is_exclusive_to_sessions_never_reaches_pty_control_sink() {
        let mut h = Harness::new(SinkKind::Control);
        let out = h.key(
            KeyCode::Char('s'),
            None,
            Modifiers { ctrl: true, alt: false, shift: false, meta: false },
        );
        assert!(out.consumed, "Ctrl-S is consumed, not forwarded");
        assert!(
            h.control.sent.lock().unwrap().is_empty(),
            "Ctrl-S must NOT leak any byte to the PTY (no 0x13/XOFF)",
        );
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

    // ── Momentum scrolling ───────────────────────────────────────

    /// A wheel notch with momentum ON does NOT jump the viewport
    /// instantly — it injects velocity into the kinetic sub-state,
    /// which the per-frame tick then integrates into a decelerating
    /// glide. The behavior-preserving opt-out (momentum OFF) keeps the
    /// direct line-for-line scroll.
    #[test]
    fn wheel_injects_velocity_with_momentum_on() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..200 {
            h.feed(b"line\r\n");
        }
        h.engine.behavior.scroll_momentum = true;
        assert_eq!(h.scroll_offset(), 0, "starts at the live tail");

        // Wheel up: velocity injected, NO immediate offset change.
        h.scroll(1.0);
        assert!(h.velocity() > 0.0, "up-wheel injects positive velocity");
        assert_eq!(
            h.scroll_offset(),
            0,
            "momentum defers the scroll to the per-frame tick"
        );

        // Drive frames at 60Hz — the glide moves the viewport up into
        // history, then decelerates to a clean stop.
        let mut frames = 0;
        while h.engine.scroll_velocity() != 0.0 && frames < 600 {
            h.tick_dt(1.0 / 60.0);
            frames += 1;
        }
        assert!(h.scroll_offset() > 0, "the glide scrolled into history");
        assert_eq!(h.velocity(), 0.0, "the glide decelerated to a stop");

        // Opt-out: momentum OFF scrolls line-for-line, immediately.
        h.engine.behavior.scroll_momentum = false;
        let before = h.scroll_offset();
        h.scroll(1.0);
        assert!(
            h.scroll_offset() > before,
            "momentum off scrolls immediately (behavior-preserving)"
        );
        assert_eq!(h.velocity(), 0.0, "no velocity injected when momentum is off");
    }

    /// `dt == 0.0` ticks move nothing even with velocity pending — the
    /// determinism contract the L1/L2 render ladders rely on.
    #[test]
    fn momentum_tick_at_dt_zero_is_a_noop() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..50 {
            h.feed(b"line\r\n");
        }
        h.engine.behavior.scroll_momentum = true;
        h.scroll(1.0);
        let off = h.scroll_offset();
        let vel = h.velocity();
        h.tick_dt(0.0);
        assert_eq!(h.scroll_offset(), off, "dt=0 moves no lines");
        assert_eq!(h.velocity(), vel, "dt=0 doesn't decay velocity");
    }

    /// Momentum doesn't grind against the scrollback wall: scrolling
    /// up into a small history caps at the top and zeroes velocity.
    #[test]
    fn momentum_clamps_at_scrollback_top() {
        let mut h = Harness::new(SinkKind::Closure);
        // Just a few rows of scrollback.
        for _ in 0..5 {
            h.feed(b"line\r\n");
        }
        h.engine.behavior.scroll_momentum = true;
        // A big up-impulse that would overshoot the small history.
        h.scroll(50.0);
        let total = h.terminal.read().scrollback_total();
        let mut frames = 0;
        while h.engine.scroll_velocity() != 0.0 && frames < 600 {
            h.tick_dt(1.0 / 60.0);
            frames += 1;
        }
        assert_eq!(
            h.scroll_offset(),
            total,
            "momentum stops exactly at the scrollback top"
        );
        assert_eq!(h.velocity(), 0.0, "velocity zeroed at the wall (no grind)");
    }

    // ── Selection auto-scroll ────────────────────────────────────

    /// Dragging a selection above the top edge scrolls the viewport UP
    /// into history and extends the highlight to the revealed lines —
    /// so a drag can select more than one screen.
    #[test]
    fn selection_drag_past_top_edge_autoscrolls_and_extends() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..200 {
            h.feed(b"content line\r\n");
        }
        h.engine.behavior.selection_autoscroll = true;
        // Scroll up a bit so there's room to reveal MORE history above.
        h.terminal.write().scroll_up(20);
        let start_offset = h.scroll_offset();

        // Begin a selection drag mid-screen, then move the pointer ABOVE
        // the top edge (negative y) without releasing.
        h.button(MouseButton::Left, true, 5, 10, no_mods());
        // Raw pointer 3 rows above the top edge.
        h.moved_px(40.0, h.row_top_px(-3.0));
        assert!(
            matches!(h.engine.pointer, Pointer::Selecting { .. }),
            "drag is live (Selecting)"
        );

        // Drive frames: the sustained auto-scroll velocity reveals more
        // history (scroll_offset grows past the start).
        for _ in 0..30 {
            h.tick_dt(1.0 / 60.0);
        }
        assert!(
            h.scroll_offset() > start_offset,
            "dragging past the top edge scrolled UP into history \
             ({} → {})",
            start_offset,
            h.scroll_offset()
        );
        // The highlight extended to the newly revealed top rows.
        assert!(
            h.engine.selection.lock().unwrap().is_active(),
            "the selection extended to the revealed lines"
        );

        // Pointer back inside the viewport → auto-scroll stops.
        h.moved_px(40.0, h.row_top_px(5.0));
        h.tick_dt(1.0 / 60.0);
        let settled = h.scroll_offset();
        h.tick_dt(1.0 / 60.0);
        assert_eq!(
            h.scroll_offset(),
            settled,
            "auto-scroll halts when the pointer re-enters the viewport"
        );
    }

    /// Dragging below the bottom edge scrolls the viewport DOWN toward
    /// the live tail.
    #[test]
    fn selection_drag_past_bottom_edge_autoscrolls_down() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..200 {
            h.feed(b"content line\r\n");
        }
        h.engine.behavior.selection_autoscroll = true;
        // Start well up in history so there's room to scroll DOWN.
        h.terminal.write().scroll_up(50);
        let start_offset = h.scroll_offset();
        assert!(start_offset > 0);

        let rows = h.terminal.read().rows();
        h.button(MouseButton::Left, true, 5, 10, no_mods());
        // Raw pointer 3 rows below the bottom edge.
        h.moved_px(40.0, h.row_top_px(rows as f64 + 3.0));
        for _ in 0..30 {
            h.tick_dt(1.0 / 60.0);
        }
        assert!(
            h.scroll_offset() < start_offset,
            "dragging past the bottom edge scrolled DOWN toward the tail \
             ({} → {})",
            start_offset,
            h.scroll_offset()
        );
    }

    /// With selection_autoscroll OFF, a past-edge drag does NOT scroll.
    #[test]
    fn selection_autoscroll_opt_out() {
        let mut h = Harness::new(SinkKind::Closure);
        for _ in 0..200 {
            h.feed(b"content line\r\n");
        }
        h.engine.behavior.selection_autoscroll = false;
        h.terminal.write().scroll_up(20);
        let start_offset = h.scroll_offset();
        h.button(MouseButton::Left, true, 5, 10, no_mods());
        h.moved_px(40.0, h.row_top_px(-3.0));
        for _ in 0..30 {
            h.tick_dt(1.0 / 60.0);
        }
        assert_eq!(
            h.scroll_offset(),
            start_offset,
            "auto-scroll off: a past-edge drag leaves the viewport frozen"
        );
    }
}
