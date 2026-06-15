//! Typed modal FSMs for [`InputEngine`](super::InputEngine) — the
//! operator FSM directive (2026-06-12) made structural.
//!
//! # The inventory (derived from the pre-FSM engine, not from a spec)
//!
//! The pre-FSM engine encoded its modal facts as scattered cells:
//!
//! | pre-FSM cell | lifted into |
//! |---|---|
//! | `SearchState.active` (shared bool) | [`Overlay::Search`] |
//! | `DirPickerState.open` (shared bool) | [`Overlay::DirPicker`] |
//! | `drag: SelectionDrag` (engine enum) | [`Pointer::Selecting`] |
//! | `shift_drag_bypass: bool` | the `bypass` field of [`Pointer::Selecting`] / [`Pointer::HeldUnanchored`] |
//! | `left_button_down: bool` | DERIVED: [`Pointer::left_button_down`] |
//!
//! Deliberately NOT machines (caches and latches, not modes — no
//! routing decision branches on them across call sites):
//! `click_count`/`last_click_time`/`last_click_pos` (multi-click
//! cadence timing), `last_mods` + `last_mouse_pos` (event-payload
//! caches for the modifier-less madori wheel pin), `mouse_visible`
//! (hide-while-typing redraw hint), `grid_sync_sig` +
//! `search_grid_gen` (per-tick reconciler latches).
//!
//! # The two machines are orthogonal BY DESIGN
//!
//! [`Overlay`] (which overlay owns the keyboard) and [`Pointer`]
//! (the left-button drag lifecycle) are separate machines, not a
//! product enum: a live drag MAY coexist with an open overlay —
//! the pre-FSM mouse path never consulted the overlay flags
//! (selecting match text under an open search bar is a real
//! workflow), and producting the axes would fabricate illegal
//! states that were never illegal. Coupled facts (bypass ↔ drag)
//! live INSIDE one machine instead of as sibling bools.
//!
//! # The forcing function
//!
//! Both transition functions match on the state enum with NO
//! wildcard arm: adding a state variant is a compile error in
//! `on_event` until every (state, event) transition is explicitly
//! decided. The transition functions are PURE — no I/O, no locks;
//! they return typed effects the engine executes through its
//! existing seams (`PtySink`, the shared selection/search/picker
//! cells, the clipboard).

use crate::keybind::Action;
use crate::terminal::{MouseMode, SelectionAnchor};
use madori::event::{KeyCode, Modifiers};

// ════════════════════════════════════════════════════════════════
// Overlay machine — which modal overlay owns the keyboard
// ════════════════════════════════════════════════════════════════

/// Which overlay owns the keyboard. Single enum, so "search AND
/// dir-picker both open" is unrepresentable (pre-FSM the two shared
/// bools could both be set via injected actions; search routing won
/// the keyboard and the picker drew unreachable — collapsing to one
/// state makes the dead arrangement unwritable; opening one overlay
/// over the other SWITCHES, decision noted 2026-06-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(pleme_allvariants_derive::AllVariants))]
pub(crate) enum Overlay {
    /// No overlay — every keystroke falls through to the keybind /
    /// kitty / PTY pipeline.
    None,
    /// Cmd+F search bar owns the keyboard.
    Search,
    /// Directory-frecency picker (轍 wadachi) owns the keyboard.
    DirPicker,
    /// Ctrl-S praça session picker owns the keyboard — fuzzy-browse +
    /// switch to a frecency-ranked session. Navigates by RAW key class
    /// (like [`Overlay::DirPicker`]); on Enter the highlighted
    /// session's pane is posted to the switch channel.
    SessionPicker,
}

/// Search-nav chords resolved from the keybind atlas (`search_close`
/// / `search_next` / `search_prev`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchNav {
    Close,
    Next,
    Prev,
}

/// Raw key class — the mode-independent fact of the keystroke the
/// dir picker navigates by (the picker matches RAW keys, the search
/// overlay matches atlas-resolved chords; both facets ride on one
/// [`OverlayKey`] so the lowering never needs to know which overlay
/// is open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayKeyClass {
    Escape,
    Enter,
    Up,
    Down,
    Backspace,
    /// Ctrl-N — emacs/readline "next" navigation (picker move down).
    CtrlN,
    /// Ctrl-P — emacs/readline "prev" navigation (picker move up).
    CtrlP,
    Other,
}

/// The mode-independent facts of one keystroke, lowered once in
/// `on_key` and interpreted per-state by the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlayKey {
    /// Atlas-resolved search-nav chord, if the pressed chord maps to
    /// one. The ONLY consumer arms live under [`Overlay::Search`].
    pub nav: Option<SearchNav>,
    /// Raw key class (dir-picker navigation).
    pub key: OverlayKeyClass,
    /// Non-empty committed text, if any.
    pub text: Option<String>,
    /// No ctrl/meta/alt held — shift is allowed (capitals). Gates
    /// query editing in both overlays.
    pub plain: bool,
}

impl OverlayKey {
    /// Lower a keystroke's facts. Mode-independent on purpose: the
    /// machine's state arm decides which facet matters.
    pub(crate) fn lower(
        action: Option<Action>,
        key: KeyCode,
        text: Option<&str>,
        mods: Modifiers,
    ) -> Self {
        let nav = match action {
            Some(Action::SearchClose) => Some(SearchNav::Close),
            Some(Action::SearchNext) => Some(SearchNav::Next),
            Some(Action::SearchPrev) => Some(SearchNav::Prev),
            _ => None,
        };
        // Ctrl-N / Ctrl-P resolve to picker-nav classes (emacs/readline
        // "next"/"prev") — checked BEFORE the raw KeyCode match so the
        // ctrl-held `Char('n')`/`Char('p')` becomes a nav class the
        // picker arms navigate by, rather than falling to `Other` (where
        // `plain` is false and they'd be inert consumes). Only the
        // ctrl-only combo qualifies; an unmodified `n`/`p` stays `Other`
        // (a query keystroke).
        let ctrl_only = mods.ctrl && !mods.meta && !mods.alt;
        let class = match key {
            KeyCode::Char('n' | 'N') if ctrl_only => OverlayKeyClass::CtrlN,
            KeyCode::Char('p' | 'P') if ctrl_only => OverlayKeyClass::CtrlP,
            KeyCode::Escape => OverlayKeyClass::Escape,
            KeyCode::Enter => OverlayKeyClass::Enter,
            KeyCode::Up => OverlayKeyClass::Up,
            KeyCode::Down => OverlayKeyClass::Down,
            KeyCode::Backspace => OverlayKeyClass::Backspace,
            _ => OverlayKeyClass::Other,
        };
        Self {
            nav,
            key: class,
            text: text.filter(|t| !t.is_empty()).map(str::to_owned),
            plain: !mods.ctrl && !mods.meta && !mods.alt,
        }
    }

    /// A nav-chord-only key — the `apply_action` (kanshou-injected)
    /// entry, where no raw key event exists.
    pub(crate) fn nav_only(nav: SearchNav) -> Self {
        Self {
            nav: Some(nav),
            key: OverlayKeyClass::Other,
            text: None,
            plain: false,
        }
    }
}

/// One overlay-machine input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OverlayEvent {
    /// `Action::SearchOpen` dispatched (chord or injection).
    OpenSearch,
    /// `Action::DirPickerOpen` dispatched.
    OpenDirPicker,
    /// `Action::SessionPickerOpen` dispatched (Ctrl-S).
    OpenSessionPicker,
    /// A keystroke routed while the machine decides ownership.
    Key(OverlayKey),
}

/// Typed overlay side effects — executed by
/// `InputEngine::apply_overlay_step` against the renderer-shared
/// mirror cells and the scroll/PTY seams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OverlayEffect {
    SearchOpen,
    SearchClose,
    SearchNext,
    SearchPrev,
    SearchAppend(String),
    SearchBackspace,
    DirPickerOpen,
    DirPickerClose,
    /// Inject `cd <selected>\n` (if a row is highlighted) and close.
    DirPickerAccept,
    DirPickerMoveUp,
    DirPickerMoveDown,
    DirPickerBackspace,
    DirPickerPush(String),
    SessionPickerOpen,
    SessionPickerClose,
    /// Switch to the highlighted session (post its pane to the switch
    /// channel via the bridge) and close. Inert when switching is
    /// disabled (no bridge) — the close still runs.
    SessionPickerAccept,
    SessionPickerMoveUp,
    SessionPickerMoveDown,
    SessionPickerBackspace,
    SessionPickerPush(String),
}

/// Whether the keystroke was consumed by the overlay or continues
/// down the pipeline (keybind dispatch → kitty → PTY translation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayRouting {
    Consumed,
    FallThrough,
}

/// One overlay transition's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlayStep {
    pub state: Overlay,
    pub routing: OverlayRouting,
    pub effects: Vec<OverlayEffect>,
}

fn overlay_step(state: Overlay, routing: OverlayRouting, effects: Vec<OverlayEffect>) -> OverlayStep {
    OverlayStep {
        state,
        routing,
        effects,
    }
}

impl Overlay {
    /// Total registry size — DERIVED from the AllVariants-emitted
    /// `ALL`, never a hand const (M3 review 2026-06-12: a hand const
    /// let a new variant land with a fresh `ordinal` arm but no
    /// const bump, leaving the registry tests green while the matrix
    /// never exercised the state). Test-gated: the forcing function
    /// still binds in CI because `cargo test` / `clippy
    /// --all-targets` compile the test cfg.
    #[cfg(test)]
    pub(crate) const STATE_VARIANTS: usize = Self::ALL.len();

    /// Position in the derive-emitted `ALL` — a new variant grows
    /// `ALL` mechanically, so registry coverage cannot be satisfied
    /// by a fudged hand ordinal.
    #[cfg(test)]
    pub(crate) fn ordinal(self) -> usize {
        Self::ALL
            .iter()
            .position(|s| *s == self)
            .expect("every Overlay variant is in the derive-emitted ALL")
    }

    /// The pure, total transition: `(state, event) -> (state,
    /// routing, effects)`. No I/O, no locks. The outer match carries
    /// NO wildcard arm on the state enum — that is the forcing
    /// function (a new state is a compile error until every
    /// transition is decided).
    // Length is the design: every (state, event) pair is written out
    // — collapsing arms to shorten the fn would erase the forcing
    // function.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn on_event(self, event: OverlayEvent) -> OverlayStep {
        use OverlayRouting::{Consumed, FallThrough};
        match self {
            // ── No overlay: the keyboard belongs to the pipeline ──
            // THE Esc-eating LAW (review finding 2026-06-11), now
            // structural: there is NO SearchNav arm here — a
            // Close/Next/Prev chord on a closed overlay cannot be
            // consumed because the consuming arm does not exist in
            // this state. Bare Esc reaches vim/helix/fzf by
            // construction, not by guard.
            Overlay::None => match event {
                OverlayEvent::OpenSearch => {
                    overlay_step(Overlay::Search, Consumed, vec![OverlayEffect::SearchOpen])
                }
                OverlayEvent::OpenDirPicker => overlay_step(
                    Overlay::DirPicker,
                    Consumed,
                    vec![OverlayEffect::DirPickerOpen],
                ),
                OverlayEvent::OpenSessionPicker => overlay_step(
                    Overlay::SessionPicker,
                    Consumed,
                    vec![OverlayEffect::SessionPickerOpen],
                ),
                OverlayEvent::Key(_) => overlay_step(Overlay::None, FallThrough, vec![]),
            },

            // ── Search bar owns the keyboard ─────────────────────
            Overlay::Search => match event {
                OverlayEvent::OpenSearch => {
                    overlay_step(Overlay::Search, Consumed, vec![OverlayEffect::SearchOpen])
                }
                // Switch semantics (decision 2026-06-12): pre-FSM an
                // injected DirPickerOpen set both shared bools; the
                // search bar kept the keyboard and the picker drew
                // unreachable. Reachable only via injection (on_key
                // consumed the chord before action dispatch). The
                // single-enum machine cannot represent both-open, so
                // the dead arrangement becomes a switch.
                OverlayEvent::OpenDirPicker => overlay_step(
                    Overlay::DirPicker,
                    Consumed,
                    vec![OverlayEffect::SearchClose, OverlayEffect::DirPickerOpen],
                ),
                OverlayEvent::OpenSessionPicker => overlay_step(
                    Overlay::SessionPicker,
                    Consumed,
                    vec![OverlayEffect::SearchClose, OverlayEffect::SessionPickerOpen],
                ),
                OverlayEvent::Key(k) => match k.nav {
                    Some(SearchNav::Close) => {
                        overlay_step(Overlay::None, Consumed, vec![OverlayEffect::SearchClose])
                    }
                    Some(SearchNav::Next) => {
                        overlay_step(Overlay::Search, Consumed, vec![OverlayEffect::SearchNext])
                    }
                    Some(SearchNav::Prev) => {
                        overlay_step(Overlay::Search, Consumed, vec![OverlayEffect::SearchPrev])
                    }
                    None => {
                        // Typing edits the query (no chord modifiers);
                        // text-before-backspace mirrors the pre-FSM
                        // routing order. Everything else is consumed
                        // so it cannot leak to the PTY.
                        if k.plain && let Some(t) = k.text {
                            return overlay_step(
                                Overlay::Search,
                                Consumed,
                                vec![OverlayEffect::SearchAppend(t)],
                            );
                        }
                        if k.plain && matches!(k.key, OverlayKeyClass::Backspace) {
                            return overlay_step(
                                Overlay::Search,
                                Consumed,
                                vec![OverlayEffect::SearchBackspace],
                            );
                        }
                        overlay_step(Overlay::Search, Consumed, vec![])
                    }
                },
            },

            // ── Dir picker owns the keyboard ─────────────────────
            // The picker navigates by RAW key class (Esc closes no
            // matter what chord the atlas maps Esc to) — k.nav is
            // deliberately unread in this arm, mirroring the pre-FSM
            // block which matched `KeyCode` directly.
            Overlay::DirPicker => match event {
                // Switch semantics — same decision note as the
                // Search → DirPicker arm above.
                OverlayEvent::OpenSearch => overlay_step(
                    Overlay::Search,
                    Consumed,
                    vec![OverlayEffect::DirPickerClose, OverlayEffect::SearchOpen],
                ),
                OverlayEvent::OpenDirPicker => overlay_step(
                    Overlay::DirPicker,
                    Consumed,
                    vec![OverlayEffect::DirPickerOpen],
                ),
                OverlayEvent::OpenSessionPicker => overlay_step(
                    Overlay::SessionPicker,
                    Consumed,
                    vec![OverlayEffect::DirPickerClose, OverlayEffect::SessionPickerOpen],
                ),
                OverlayEvent::Key(k) => match k.key {
                    OverlayKeyClass::Escape => {
                        overlay_step(Overlay::None, Consumed, vec![OverlayEffect::DirPickerClose])
                    }
                    OverlayKeyClass::Enter => {
                        overlay_step(Overlay::None, Consumed, vec![OverlayEffect::DirPickerAccept])
                    }
                    OverlayKeyClass::Up | OverlayKeyClass::CtrlP => overlay_step(
                        Overlay::DirPicker,
                        Consumed,
                        vec![OverlayEffect::DirPickerMoveUp],
                    ),
                    OverlayKeyClass::Down | OverlayKeyClass::CtrlN => overlay_step(
                        Overlay::DirPicker,
                        Consumed,
                        vec![OverlayEffect::DirPickerMoveDown],
                    ),
                    OverlayKeyClass::Backspace => {
                        if k.plain {
                            overlay_step(
                                Overlay::DirPicker,
                                Consumed,
                                vec![OverlayEffect::DirPickerBackspace],
                            )
                        } else {
                            overlay_step(Overlay::DirPicker, Consumed, vec![])
                        }
                    }
                    OverlayKeyClass::Other => {
                        if k.plain && let Some(t) = k.text {
                            return overlay_step(
                                Overlay::DirPicker,
                                Consumed,
                                vec![OverlayEffect::DirPickerPush(t)],
                            );
                        }
                        overlay_step(Overlay::DirPicker, Consumed, vec![])
                    }
                },
            },

            // ── Session picker owns the keyboard ─────────────────
            // The praça Ctrl-S picker — fuzzy-browse + switch. Like the
            // dir picker it navigates by RAW key class (Esc closes
            // regardless of atlas mapping; k.nav is unread), plus
            // Ctrl-N / Ctrl-P as emacs-style next/prev. Enter switches
            // to the highlighted session (the SessionPickerAccept effect
            // posts its pane to the switch channel via the bridge).
            Overlay::SessionPicker => match event {
                // Switch semantics — same decision note as the
                // Search ⇄ DirPicker arms above.
                OverlayEvent::OpenSearch => overlay_step(
                    Overlay::Search,
                    Consumed,
                    vec![OverlayEffect::SessionPickerClose, OverlayEffect::SearchOpen],
                ),
                OverlayEvent::OpenDirPicker => overlay_step(
                    Overlay::DirPicker,
                    Consumed,
                    vec![OverlayEffect::SessionPickerClose, OverlayEffect::DirPickerOpen],
                ),
                OverlayEvent::OpenSessionPicker => overlay_step(
                    Overlay::SessionPicker,
                    Consumed,
                    vec![OverlayEffect::SessionPickerOpen],
                ),
                OverlayEvent::Key(k) => match k.key {
                    OverlayKeyClass::Escape => overlay_step(
                        Overlay::None,
                        Consumed,
                        vec![OverlayEffect::SessionPickerClose],
                    ),
                    OverlayKeyClass::Enter => overlay_step(
                        Overlay::None,
                        Consumed,
                        vec![OverlayEffect::SessionPickerAccept],
                    ),
                    OverlayKeyClass::Up | OverlayKeyClass::CtrlP => overlay_step(
                        Overlay::SessionPicker,
                        Consumed,
                        vec![OverlayEffect::SessionPickerMoveUp],
                    ),
                    OverlayKeyClass::Down | OverlayKeyClass::CtrlN => overlay_step(
                        Overlay::SessionPicker,
                        Consumed,
                        vec![OverlayEffect::SessionPickerMoveDown],
                    ),
                    OverlayKeyClass::Backspace => {
                        if k.plain {
                            overlay_step(
                                Overlay::SessionPicker,
                                Consumed,
                                vec![OverlayEffect::SessionPickerBackspace],
                            )
                        } else {
                            overlay_step(Overlay::SessionPicker, Consumed, vec![])
                        }
                    }
                    OverlayKeyClass::Other => {
                        if k.plain && let Some(t) = k.text {
                            return overlay_step(
                                Overlay::SessionPicker,
                                Consumed,
                                vec![OverlayEffect::SessionPickerPush(t)],
                            );
                        }
                        overlay_step(Overlay::SessionPicker, Consumed, vec![])
                    }
                },
            },
        }
    }
}

// ════════════════════════════════════════════════════════════════
// Pointer machine — left-button drag lifecycle + shift bypass
// ════════════════════════════════════════════════════════════════

/// Selection-drag granularity: what unit the live drag snaps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragMode {
    /// Plain drag — endpoints are the exact cells.
    Char,
    /// Double-click drag — BOTH ends snap to word boundaries; the
    /// origin word stays fully selected (kitty/ghostty contract).
    Word,
    /// Triple-click drag — both ends snap to full physical rows.
    Line,
}

/// The pointer modal state. Replaces three pre-FSM sibling cells
/// (`drag: SelectionDrag`, `shift_drag_bypass: bool`,
/// `left_button_down: bool`); the button-held fact is now DERIVED
/// ([`Self::left_button_down`]) so button-state desync is
/// unrepresentable, and [`Pointer::ForwardedPress`] structurally
/// carries no `bypass` field — a forwarded press with the shift
/// bypass armed cannot be written (the press routing sends
/// shift-bypassed presses terminal-side, never to the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pointer {
    /// Left button not held.
    Up,
    /// Left press was forwarded to the application (mouse tracking
    /// owned it). No drag, no bypass — by construction.
    ForwardedPress,
    /// Left button held, terminal-side selection drag live. `origin`
    /// is the content-anchored span captured at press time: char
    /// drags carry the press cell twice; word/line drags carry the
    /// snapped origin unit, which every motion-time union keeps
    /// fully selected.
    Selecting {
        mode: DragMode,
        origin: (SelectionAnchor, SelectionAnchor),
        /// Shift+click bypassed mouse tracking (xterm/kitty/ghostty
        /// escape hatch, hunt finding 2026-06-11) — motion must
        /// never forward while this drag lives.
        bypass: bool,
    },
    /// Left button held but no drag is live: anchor capture failed
    /// at press, or typing / a dangle-reconcile killed the drag
    /// mid-hold. `bypass` survives until release (pre-FSM contract:
    /// `write_key_input` never cleared the bypass flag).
    HeldUnanchored { bypass: bool },
}

/// Routing decision for a left press — the ONE place the
/// forward-vs-terminal-side split is decided (pre-FSM the same
/// boolean expression was inlined at the press site).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PressRouting {
    /// Tracking owns the press: forward it to the app.
    Forward,
    /// Terminal-side selection press; `bypass` is what the pre-FSM
    /// `shift_drag_bypass` latch would have stored (shift bypass is
    /// only an *override* when tracking is actually on).
    Select { bypass: bool },
}

/// Decide where a left press goes. `shift_local` = shift held and
/// `mouse_shift_capture` off (the operator's escape hatch).
pub(crate) fn route_left_press(tracking_on: bool, shift_local: bool) -> PressRouting {
    if tracking_on && !shift_local {
        PressRouting::Forward
    } else {
        PressRouting::Select {
            bypass: tracking_on && shift_local,
        }
    }
}

/// What the engine's impure press classifier (multi-click cadence +
/// content-anchor capture) resolved for a terminal-side press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PressPlan {
    /// Shift+click with an existing selection: extend to the click
    /// point, keep dragging from the surviving start anchor.
    Extend {
        start: SelectionAnchor,
        click: SelectionAnchor,
    },
    /// Double-click: the snapped word span is the drag origin.
    Word {
        span: (SelectionAnchor, SelectionAnchor),
    },
    /// Triple-click: the full physical row is the drag origin.
    Line {
        span: (SelectionAnchor, SelectionAnchor),
    },
    /// Single click at an anchorable cell.
    Char { anchor: SelectionAnchor },
    /// Anchor capture failed (content unreachable) — button is held
    /// but no drag can live.
    Unanchored,
}

/// One pointer-machine input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerEvent {
    /// Left press, pre-routed + pre-classified by the engine.
    LeftPress(PressRoute),
    /// Left release — PAYLOAD-FREE. The release routing is now
    /// STATE-DERIVED, not event-time (copy-on-release regression,
    /// operator report 2026-06-12): a forwarded press releases to the
    /// app (`ForwardedPress` state), and EVERY drag-ending state
    /// (`Selecting` / `HeldUnanchored`) — plus a stray release with no
    /// live press (`Up`) — runs the selection-release ritual that
    /// commits the highlight to the clipboard. The old event-time
    /// `{tracking_on, shift_local}` payload let a release that arrived
    /// while tracking flipped mid-drag forward to the app instead of
    /// copying; making the payload absent makes that miss
    /// unrepresentable (there is no event-time fact left to read).
    LeftRelease,
    /// Pointer motion with the event-time terminal mouse mode.
    Motion { mode: MouseMode, sgr: bool },
    /// Operator input bytes are about to hit the PTY
    /// (`write_key_input`) — kills any live drag, never emits
    /// effects.
    TypedInput,
    /// The per-tick reconciler found the selection's anchors
    /// dangling — kills any live drag, never emits effects.
    SelectionDangled,
}

/// A routed left press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PressRoute {
    Forward,
    Select { bypass: bool, plan: PressPlan },
}

/// Typed pointer side effects — executed by
/// `InputEngine::run_pointer_effect` at the event's cell coords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerEffect {
    /// Forward the left press to the app (SGR/X10 per event).
    ForwardPress,
    /// Forward the left release to the app.
    ForwardRelease,
    /// Forward a motion report (only ever emitted when SGR is on —
    /// the pre-FSM motion path never encoded non-SGR motion).
    ForwardMotion { button_down: bool },
    /// `Selection::start` at the anchor (plain click).
    StartSelection(SelectionAnchor),
    /// `Selection::set_span` (extend / word / line presses).
    SetSpan(SelectionAnchor, SelectionAnchor),
    /// One motion tick of the live drag (`InputEngine::update_drag`).
    UpdateDrag {
        mode: DragMode,
        origin: (SelectionAnchor, SelectionAnchor),
    },
    /// The terminal-side release ritual: finish single-click
    /// selections, copy-on-select, Cmd/Ctrl+click URL open.
    SelectionRelease,
    /// Recover a release that the adapter dropped (the copy-on-release
    /// regression, operator report 2026-06-12): if a prior release was
    /// eaten — e.g. by an early-return-on-title drain — the highlight
    /// is still live when the next press arrives. Emitted by
    /// `Selecting × LeftPress` BEFORE the new press's own effects, so
    /// the orphaned selection lands on the clipboard at the next press
    /// even when its release was lost. Runs the same copy ritual as
    /// `SelectionRelease` but does NOT consume the press (the new press
    /// proceeds normally after it).
    CompleteOrphanedDrag,
}

/// One pointer transition's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PointerStep {
    pub state: Pointer,
    pub effects: Vec<PointerEffect>,
}

fn pointer_step(state: Pointer, effects: Vec<PointerEffect>) -> PointerStep {
    PointerStep { state, effects }
}

/// A press re-classifies the pointer wholesale: any stale held
/// state is replaced (presses without a paired release are an OS
/// edge — focus loss — and the fresh press wins). Shared by every
/// state's `LeftPress` arm.
fn enter_press(route: PressRoute) -> PointerStep {
    match route {
        PressRoute::Forward => pointer_step(Pointer::ForwardedPress, vec![PointerEffect::ForwardPress]),
        PressRoute::Select { bypass, plan } => match plan {
            PressPlan::Extend { start, click } => pointer_step(
                Pointer::Selecting {
                    mode: DragMode::Char,
                    origin: (start, start),
                    bypass,
                },
                vec![PointerEffect::SetSpan(start, click)],
            ),
            PressPlan::Word { span } => pointer_step(
                Pointer::Selecting {
                    mode: DragMode::Word,
                    origin: span,
                    bypass,
                },
                vec![PointerEffect::SetSpan(span.0, span.1)],
            ),
            PressPlan::Line { span } => pointer_step(
                Pointer::Selecting {
                    mode: DragMode::Line,
                    origin: span,
                    bypass,
                },
                vec![PointerEffect::SetSpan(span.0, span.1)],
            ),
            PressPlan::Char { anchor } => pointer_step(
                Pointer::Selecting {
                    mode: DragMode::Char,
                    origin: (anchor, anchor),
                    bypass,
                },
                vec![PointerEffect::StartSelection(anchor)],
            ),
            PressPlan::Unanchored => pointer_step(Pointer::HeldUnanchored { bypass }, vec![]),
        },
    }
}

/// A press that lands ON a live `Selecting` drag re-classifies the
/// pointer (via [`enter_press`]) but FIRST commits the orphaned
/// highlight: a release the adapter dropped (the copy-on-release
/// regression) left the selection live, and the operator's next
/// click would otherwise discard it. Prepending
/// [`PointerEffect::CompleteOrphanedDrag`] copies the stranded
/// selection before the new press's own effects run — so a lost
/// release still lands the highlight on the clipboard.
fn enter_press_completing_orphan(route: PressRoute) -> PointerStep {
    let mut step = enter_press(route);
    step.effects.insert(0, PointerEffect::CompleteOrphanedDrag);
    step
}

/// Payload-free discriminant mirror of [`Pointer`] — `ALL` is
/// derive-emitted, so the state-variant count is mechanical (M3
/// review 2026-06-12: the former hand const `STATE_VARIANTS = 4`
/// let a new state land with a fudged ordinal and zero matrix
/// coverage). [`Pointer::kind`] is the total-match projection; a
/// new `Pointer` variant refuses to compile there until a mirror
/// variant exists, which grows `ALL` and fails the len-pinned
/// registry tests until the instance lists grow too.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, pleme_allvariants_derive::AllVariants)]
pub(crate) enum PointerKind {
    Up,
    ForwardedPress,
    Selecting,
    HeldUnanchored,
}

impl Pointer {
    /// Total registry size — DERIVED from the mirror enum's
    /// `AllVariants` `ALL`, never a hand const. Test-gated: the
    /// forcing function still binds in CI because `cargo test` /
    /// `clippy --all-targets` compile the test cfg.
    #[cfg(test)]
    pub(crate) const STATE_VARIANTS: usize = PointerKind::ALL.len();

    /// FORCING FUNCTION: total match, no wildcard — a new pointer
    /// state cannot compile until its mirror kind AND every
    /// `on_event` arm are decided.
    #[cfg(test)]
    pub(crate) fn kind(self) -> PointerKind {
        match self {
            Pointer::Up => PointerKind::Up,
            Pointer::ForwardedPress => PointerKind::ForwardedPress,
            Pointer::Selecting { .. } => PointerKind::Selecting,
            Pointer::HeldUnanchored { .. } => PointerKind::HeldUnanchored,
        }
    }

    /// Position of the state's kind in the derive-emitted `ALL`.
    #[cfg(test)]
    pub(crate) fn ordinal(self) -> usize {
        let kind = self.kind();
        PointerKind::ALL
            .iter()
            .position(|k| *k == kind)
            .expect("every PointerKind variant is in the derive-emitted ALL")
    }

    /// Whether the left button is physically held — DERIVED from the
    /// state (pre-FSM this was a sibling bool that could desync from
    /// the drag enum; now the desync is unrepresentable). Drives the
    /// SGR motion code (drag = 32 vs hover = 35) and `ButtonEvent`
    /// (1002)'s motion-only-while-pressed contract — the runtime
    /// consumer is the machine's own Motion arms; the matrix test
    /// asserts the forwarded button bit never diverges from it.
    #[cfg(test)]
    pub(crate) fn left_button_down(self) -> bool {
        !matches!(self, Pointer::Up)
    }

    /// The pure, total transition: `(state, event) -> (state,
    /// effects)`. No I/O, no locks. The outer match carries NO
    /// wildcard arm on the state enum — the forcing function.
    // Length is the design: every (state, event) pair is written out
    // — collapsing arms to shorten the fn would erase the forcing
    // function.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn on_event(self, event: PointerEvent) -> PointerStep {
        match self {
            Pointer::Up => match event {
                PointerEvent::LeftPress(route) => enter_press(route),
                // Stray release with no live press (a release whose
                // press was eaten upstream, or an OS focus-loss edge):
                // run the selection-release ritual so a still-live
                // highlight is committed — never forward to the app
                // (no matching press was forwarded). This is the
                // copy-on-release stray-recovery arm (2026-06-12).
                PointerEvent::LeftRelease => {
                    pointer_step(Pointer::Up, vec![PointerEffect::SelectionRelease])
                }
                PointerEvent::Motion { mode, sgr } => {
                    // Hover: ButtonEvent (1002) forwards only while
                    // pressed (button is up here, so it stays
                    // silent like Off/Normal); AnyEvent (1003)
                    // forwards hover with the no-button code.
                    let effects = match mode {
                        MouseMode::Off | MouseMode::Normal | MouseMode::ButtonEvent => vec![],
                        MouseMode::AnyEvent => {
                            if sgr {
                                vec![PointerEffect::ForwardMotion { button_down: false }]
                            } else {
                                vec![]
                            }
                        }
                    };
                    pointer_step(Pointer::Up, effects)
                }
                PointerEvent::TypedInput | PointerEvent::SelectionDangled => {
                    pointer_step(Pointer::Up, vec![])
                }
            },

            Pointer::ForwardedPress => match event {
                PointerEvent::LeftPress(route) => enter_press(route),
                // The press was forwarded to the app — its release must
                // follow it (the ONLY state whose release forwards).
                // STATE-DERIVED: the routing is the state, not an
                // event-time tracking flag (which could flip mid-gesture
                // and strand the release).
                PointerEvent::LeftRelease => {
                    pointer_step(Pointer::Up, vec![PointerEffect::ForwardRelease])
                }
                PointerEvent::Motion { mode, sgr } => {
                    let effects = match mode {
                        MouseMode::Off | MouseMode::Normal => vec![],
                        MouseMode::ButtonEvent | MouseMode::AnyEvent => {
                            if sgr {
                                vec![PointerEffect::ForwardMotion { button_down: true }]
                            } else {
                                vec![]
                            }
                        }
                    };
                    pointer_step(Pointer::ForwardedPress, effects)
                }
                PointerEvent::TypedInput | PointerEvent::SelectionDangled => {
                    pointer_step(Pointer::ForwardedPress, vec![])
                }
            },

            Pointer::Selecting {
                mode: drag_mode,
                origin,
                bypass,
            } => match event {
                // A press on a live drag commits the orphaned highlight
                // FIRST (copy-on-release recovery, 2026-06-12: a
                // dropped release left this selection live), then
                // re-classifies the pointer for the new press.
                PointerEvent::LeftPress(route) => enter_press_completing_orphan(route),
                // Release ALWAYS commits the selection — STATE-DERIVED,
                // no event-time routing (copy-on-release regression,
                // operator report 2026-06-12). A live drag's release
                // belongs to the selection, period: the prior
                // `release_routing(tracking_on, shift_local)` branch
                // could forward a drag's release to the app when
                // tracking flipped on mid-drag, dropping the copy. The
                // non-copy arm is now unrepresentable — `SelectionRelease`
                // is the only reachable effect.
                PointerEvent::LeftRelease => {
                    pointer_step(Pointer::Up, vec![PointerEffect::SelectionRelease])
                }
                PointerEvent::Motion { mode, sgr } => {
                    // Tracked + un-bypassed motion belongs to the app
                    // (and deliberately does NOT update the drag —
                    // pre-FSM contract); everything else is one drag
                    // tick.
                    let effects = match mode {
                        MouseMode::Off | MouseMode::Normal => vec![PointerEffect::UpdateDrag {
                            mode: drag_mode,
                            origin,
                        }],
                        MouseMode::ButtonEvent | MouseMode::AnyEvent => {
                            if bypass {
                                vec![PointerEffect::UpdateDrag {
                                    mode: drag_mode,
                                    origin,
                                }]
                            } else if sgr {
                                vec![PointerEffect::ForwardMotion { button_down: true }]
                            } else {
                                vec![]
                            }
                        }
                    };
                    pointer_step(
                        Pointer::Selecting {
                            mode: drag_mode,
                            origin,
                            bypass,
                        },
                        effects,
                    )
                }
                // Typing replaces the selection; the drag must not
                // resurrect it on the next motion tick. The bypass
                // survives until release (pre-FSM contract).
                PointerEvent::TypedInput | PointerEvent::SelectionDangled => {
                    pointer_step(Pointer::HeldUnanchored { bypass }, vec![])
                }
            },

            Pointer::HeldUnanchored { bypass } => match event {
                PointerEvent::LeftPress(route) => enter_press(route),
                // The button was held with no live drag (anchor capture
                // failed, or typing killed the drag mid-hold). Its
                // release runs the selection-release ritual — never
                // forwards — so any residual highlight commits.
                // STATE-DERIVED, same copy-on-release law as Selecting.
                PointerEvent::LeftRelease => {
                    pointer_step(Pointer::Up, vec![PointerEffect::SelectionRelease])
                }
                PointerEvent::Motion { mode, sgr } => {
                    let effects = match mode {
                        MouseMode::Off | MouseMode::Normal => vec![],
                        MouseMode::ButtonEvent | MouseMode::AnyEvent => {
                            if !bypass && sgr {
                                vec![PointerEffect::ForwardMotion { button_down: true }]
                            } else {
                                vec![]
                            }
                        }
                    };
                    pointer_step(Pointer::HeldUnanchored { bypass }, effects)
                }
                PointerEvent::TypedInput | PointerEvent::SelectionDangled => {
                    pointer_step(Pointer::HeldUnanchored { bypass }, vec![])
                }
            },
        }
    }
}

/// Payload-free event-class mirror of [`PointerEvent`] — see
/// [`PointerKind`] for the mechanical-registry rationale.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, pleme_allvariants_derive::AllVariants)]
pub(crate) enum PointerEventClass {
    LeftPress,
    LeftRelease,
    Motion,
    TypedInput,
    SelectionDangled,
}

impl PointerEvent {
    /// Event-class registry size — DERIVED from the mirror enum's
    /// `AllVariants` `ALL`, never a hand const.
    #[cfg(test)]
    pub(crate) const CLASS_VARIANTS: usize = PointerEventClass::ALL.len();

    /// FORCING FUNCTION: total match, no wildcard — a new event
    /// class cannot compile until its mirror variant exists, which
    /// grows the registry mechanically.
    #[cfg(test)]
    pub(crate) fn class(&self) -> PointerEventClass {
        match self {
            PointerEvent::LeftPress(_) => PointerEventClass::LeftPress,
            PointerEvent::LeftRelease => PointerEventClass::LeftRelease,
            PointerEvent::Motion { .. } => PointerEventClass::Motion,
            PointerEvent::TypedInput => PointerEventClass::TypedInput,
            PointerEvent::SelectionDangled => PointerEventClass::SelectionDangled,
        }
    }

    /// Position of the event's class in the derive-emitted `ALL`.
    #[cfg(test)]
    pub(crate) fn class_ordinal(&self) -> usize {
        let class = self.class();
        PointerEventClass::ALL
            .iter()
            .position(|c| *c == class)
            .expect("every PointerEventClass variant is in the derive-emitted ALL")
    }
}

/// Payload-free event-class mirror of [`OverlayEvent`] — see
/// [`PointerKind`] for the mechanical-registry rationale.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, pleme_allvariants_derive::AllVariants)]
pub(crate) enum OverlayEventClass {
    OpenSearch,
    OpenDirPicker,
    OpenSessionPicker,
    Key,
}

impl OverlayEvent {
    /// Event-class registry size — DERIVED from the mirror enum's
    /// `AllVariants` `ALL`, never a hand const.
    #[cfg(test)]
    pub(crate) const CLASS_VARIANTS: usize = OverlayEventClass::ALL.len();

    /// FORCING FUNCTION: total match, no wildcard.
    #[cfg(test)]
    pub(crate) fn class(&self) -> OverlayEventClass {
        match self {
            OverlayEvent::OpenSearch => OverlayEventClass::OpenSearch,
            OverlayEvent::OpenDirPicker => OverlayEventClass::OpenDirPicker,
            OverlayEvent::OpenSessionPicker => OverlayEventClass::OpenSessionPicker,
            OverlayEvent::Key(_) => OverlayEventClass::Key,
        }
    }

    /// Position of the event's class in the derive-emitted `ALL`.
    #[cfg(test)]
    pub(crate) fn class_ordinal(&self) -> usize {
        let class = self.class();
        OverlayEventClass::ALL
            .iter()
            .position(|c| *c == class)
            .expect("every OverlayEventClass variant is in the derive-emitted ALL")
    }
}

#[cfg(test)]
#[allow(clippy::too_many_lines)] // matrix tests enumerate every cell on purpose
mod tests {
    //! Mechanical exhaustiveness: ALL states × ALL event classes
    //! from registry arrays whose completeness is asserted against
    //! the ordinal functions (which are themselves total matches —
    //! the compile-time half of the forcing function). Failures
    //! aggregate matrix-style: one run reports every violated
    //! invariant.
    //!
    //! Engine-level companions (the seams the machines feed):
    //! `bare_modifier_press_preserves_selection_so_cmd_c_works`
    //! (bare modifier press clears no selection — clearing lives in
    //! `write_key_input`, which only runs for byte-sending keys) and
    //! `output_while_scrolled_keeps_view_pinned_to_content`
    //! (output never moves the view — untouched by this lift).

    use super::*;
    use crate::terminal::Terminal;

    /// Mint real content anchors — `SelectionAnchor`'s sole producer
    /// is `Terminal::selection_anchor_at` (opaque by design).
    fn anchor_pair() -> (SelectionAnchor, SelectionAnchor) {
        let mut term = Terminal::new(80, 24);
        term.feed(b"hello world");
        let a = term.selection_anchor_at(0, 0).expect("anchor at 0,0");
        let b = term.selection_anchor_at(0, 4).expect("anchor at 0,4");
        (a, b)
    }

    fn all_pointer_states() -> Vec<Pointer> {
        let (a, b) = anchor_pair();
        vec![
            Pointer::Up,
            Pointer::ForwardedPress,
            Pointer::Selecting {
                mode: DragMode::Char,
                origin: (a, b),
                bypass: false,
            },
            Pointer::Selecting {
                mode: DragMode::Word,
                origin: (a, b),
                bypass: true,
            },
            Pointer::Selecting {
                mode: DragMode::Line,
                origin: (a, b),
                bypass: false,
            },
            Pointer::HeldUnanchored { bypass: false },
            Pointer::HeldUnanchored { bypass: true },
        ]
    }

    fn all_pointer_events() -> Vec<PointerEvent> {
        let (a, b) = anchor_pair();
        vec![
            PointerEvent::LeftPress(PressRoute::Forward),
            PointerEvent::LeftPress(PressRoute::Select {
                bypass: false,
                plan: PressPlan::Char { anchor: a },
            }),
            PointerEvent::LeftPress(PressRoute::Select {
                bypass: true,
                plan: PressPlan::Extend { start: a, click: b },
            }),
            PointerEvent::LeftPress(PressRoute::Select {
                bypass: false,
                plan: PressPlan::Word { span: (a, b) },
            }),
            PointerEvent::LeftPress(PressRoute::Select {
                bypass: false,
                plan: PressPlan::Line { span: (a, b) },
            }),
            PointerEvent::LeftPress(PressRoute::Select {
                bypass: true,
                plan: PressPlan::Unanchored,
            }),
            // LeftRelease is payload-free now — the release routing is
            // state-derived (copy-on-release regression, 2026-06-12).
            // One representative is the whole class.
            PointerEvent::LeftRelease,
            PointerEvent::Motion {
                mode: MouseMode::Off,
                sgr: false,
            },
            PointerEvent::Motion {
                mode: MouseMode::Normal,
                sgr: true,
            },
            PointerEvent::Motion {
                mode: MouseMode::ButtonEvent,
                sgr: true,
            },
            PointerEvent::Motion {
                mode: MouseMode::ButtonEvent,
                sgr: false,
            },
            PointerEvent::Motion {
                mode: MouseMode::AnyEvent,
                sgr: true,
            },
            PointerEvent::Motion {
                mode: MouseMode::AnyEvent,
                sgr: false,
            },
            PointerEvent::TypedInput,
            PointerEvent::SelectionDangled,
        ]
    }

    fn all_overlay_states() -> Vec<Overlay> {
        // Overlay is payload-free, so the registry IS the
        // derive-emitted ALL — no hand list to drift.
        Overlay::ALL.to_vec()
    }

    fn key(nav: Option<SearchNav>, class: OverlayKeyClass, text: Option<&str>, plain: bool) -> OverlayKey {
        OverlayKey {
            nav,
            key: class,
            text: text.map(str::to_owned),
            plain,
        }
    }

    fn all_overlay_events() -> Vec<OverlayEvent> {
        vec![
            OverlayEvent::OpenSearch,
            OverlayEvent::OpenDirPicker,
            OverlayEvent::OpenSessionPicker,
            OverlayEvent::Key(key(Some(SearchNav::Close), OverlayKeyClass::Escape, None, true)),
            OverlayEvent::Key(key(Some(SearchNav::Next), OverlayKeyClass::Other, None, false)),
            OverlayEvent::Key(key(Some(SearchNav::Prev), OverlayKeyClass::Other, None, false)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Escape, None, true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Enter, None, true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Up, None, true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Down, None, true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::CtrlN, None, false)),
            OverlayEvent::Key(key(None, OverlayKeyClass::CtrlP, None, false)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Backspace, None, true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Backspace, None, false)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Other, Some("a"), true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Other, Some("A"), false)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Other, None, true)),
            OverlayEvent::Key(key(None, OverlayKeyClass::Other, None, false)),
        ]
    }

    // ── Registry completeness (len-equality against the ordinal
    //    total-matches — the mechanical half of the forcing fn) ────

    #[test]
    fn pointer_registries_cover_every_variant() {
        let mut failures: Vec<String> = Vec::new();
        let state_ords: std::collections::BTreeSet<usize> =
            all_pointer_states().iter().map(|s| s.ordinal()).collect();
        if state_ords != (0..Pointer::STATE_VARIANTS).collect() {
            failures.push(format!(
                "pointer state registry covers ordinals {state_ords:?}, want 0..{}",
                Pointer::STATE_VARIANTS
            ));
        }
        let event_ords: std::collections::BTreeSet<usize> = all_pointer_events()
            .iter()
            .map(PointerEvent::class_ordinal)
            .collect();
        if event_ords != (0..PointerEvent::CLASS_VARIANTS).collect() {
            failures.push(format!(
                "pointer event registry covers class ordinals {event_ords:?}, want 0..{}",
                PointerEvent::CLASS_VARIANTS
            ));
        }
        assert!(
            failures.is_empty(),
            "{} registry gaps:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn overlay_registries_cover_every_variant() {
        let mut failures: Vec<String> = Vec::new();
        let state_ords: std::collections::BTreeSet<usize> =
            all_overlay_states().iter().map(|s| s.ordinal()).collect();
        if state_ords != (0..Overlay::STATE_VARIANTS).collect() {
            failures.push(format!(
                "overlay state registry covers ordinals {state_ords:?}, want 0..{}",
                Overlay::STATE_VARIANTS
            ));
        }
        let event_ords: std::collections::BTreeSet<usize> = all_overlay_events()
            .iter()
            .map(OverlayEvent::class_ordinal)
            .collect();
        if event_ords != (0..OverlayEvent::CLASS_VARIANTS).collect() {
            failures.push(format!(
                "overlay event registry covers class ordinals {event_ords:?}, want 0..{}",
                OverlayEvent::CLASS_VARIANTS
            ));
        }
        assert!(
            failures.is_empty(),
            "{} registry gaps:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    // ── The full matrices ────────────────────────────────────────

    #[test]
    fn pointer_state_event_matrix_holds_invariants() {
        let mut failures: Vec<String> = Vec::new();
        for state in all_pointer_states() {
            for event in all_pointer_events() {
                let step = state.on_event(event);
                let ctx = format!("{state:?} × {event:?}");

                match event {
                    PointerEvent::Motion { mode, sgr } => {
                        if step.state != state {
                            failures.push(format!("{ctx}: motion changed state to {:?}", step.state));
                        }
                        let reportable = matches!(mode, MouseMode::ButtonEvent | MouseMode::AnyEvent);
                        if step
                            .effects
                            .iter()
                            .any(|e| matches!(e, PointerEffect::ForwardMotion { .. }))
                            && !(reportable && sgr)
                        {
                            failures.push(format!(
                                "{ctx}: ForwardMotion emitted outside reportable+SGR"
                            ));
                        }
                        if let Some(PointerEffect::ForwardMotion { button_down }) = step
                            .effects
                            .iter()
                            .find(|e| matches!(e, PointerEffect::ForwardMotion { .. }))
                            && *button_down != state.left_button_down()
                        {
                            failures.push(format!(
                                "{ctx}: forwarded motion button bit diverges from derived button state"
                            ));
                        }
                        let updates = step
                            .effects
                            .iter()
                            .any(|e| matches!(e, PointerEffect::UpdateDrag { .. }));
                        if updates && !matches!(state, Pointer::Selecting { .. }) {
                            failures.push(format!("{ctx}: UpdateDrag emitted outside Selecting"));
                        }
                        if updates
                            && step
                                .effects
                                .iter()
                                .any(|e| matches!(e, PointerEffect::ForwardMotion { .. }))
                        {
                            failures.push(format!(
                                "{ctx}: drag update and motion forward in one tick"
                            ));
                        }
                    }
                    PointerEvent::LeftRelease => {
                        if step.state != Pointer::Up {
                            failures.push(format!(
                                "{ctx}: release did not return the pointer to Up (got {:?})",
                                step.state
                            ));
                        }
                        let forwards = step.effects.contains(&PointerEffect::ForwardRelease);
                        let selects = step.effects.contains(&PointerEffect::SelectionRelease);
                        if forwards == selects {
                            failures.push(format!(
                                "{ctx}: release must route to exactly one of forward/selection"
                            ));
                        }
                        // STATE-DERIVED release routing (copy-on-release
                        // regression, 2026-06-12): a forwarded press
                        // releases to the app; EVERY other state commits
                        // the selection. The non-copy arm is
                        // unrepresentable for any drag-ending or stray
                        // state — there is no event-time fact left that
                        // could route a drag's release to the app.
                        let forwarding_state = matches!(state, Pointer::ForwardedPress);
                        if forwarding_state && !forwards {
                            failures.push(format!(
                                "{ctx}: forwarded press did not forward its release"
                            ));
                        }
                        if !forwarding_state && !selects {
                            failures.push(format!(
                                "{ctx}: non-forwarded state's release did not commit the \
                                 selection — the copy-on-release law leaked"
                            ));
                        }
                    }
                    PointerEvent::TypedInput | PointerEvent::SelectionDangled => {
                        if !step.effects.is_empty() {
                            failures.push(format!("{ctx}: drag-kill events must be effect-free"));
                        }
                        if matches!(step.state, Pointer::Selecting { .. }) {
                            failures.push(format!("{ctx}: drag survived a drag-kill event"));
                        }
                        if step.state.left_button_down() != state.left_button_down() {
                            failures.push(format!(
                                "{ctx}: drag-kill event changed the derived button state"
                            ));
                        }
                    }
                    PointerEvent::LeftPress(route) => {
                        if !step.state.left_button_down() {
                            failures.push(format!("{ctx}: press left the pointer Up"));
                        }
                        if matches!(route, PressRoute::Forward)
                            && !matches!(step.state, Pointer::ForwardedPress)
                        {
                            failures.push(format!("{ctx}: forwarded press not in ForwardedPress"));
                        }
                        // Copy-on-release recovery (2026-06-12): a press
                        // landing on a live Selecting drag commits the
                        // orphaned highlight FIRST (a dropped release
                        // left it live), and ONLY that case does — every
                        // other prior state has no live drag to strand.
                        let completes_orphan = step
                            .effects
                            .contains(&PointerEffect::CompleteOrphanedDrag);
                        let pressing_live_drag = matches!(state, Pointer::Selecting { .. });
                        if completes_orphan != pressing_live_drag {
                            failures.push(format!(
                                "{ctx}: CompleteOrphanedDrag must be emitted iff pressing on a \
                                 live Selecting drag (got {completes_orphan}, want {pressing_live_drag})"
                            ));
                        }
                        // When it IS emitted it must be FIRST — the
                        // orphan copies before the new press's own
                        // selection effects mutate the selection.
                        if completes_orphan
                            && step.effects.first()
                                != Some(&PointerEffect::CompleteOrphanedDrag)
                        {
                            failures.push(format!(
                                "{ctx}: CompleteOrphanedDrag must precede the new press's effects"
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} pointer matrix violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn overlay_state_event_matrix_holds_invariants() {
        let mut failures: Vec<String> = Vec::new();
        for state in all_overlay_states() {
            for event in all_overlay_events() {
                let step = state.on_event(event.clone());
                let ctx = format!("{state:?} × {event:?}");

                // Search query/nav mutations only while Search owns
                // the keyboard; picker mutations only while the
                // picker does (close-half of a switch excepted).
                let search_mutates = step.effects.iter().any(|e| {
                    matches!(
                        e,
                        OverlayEffect::SearchNext
                            | OverlayEffect::SearchPrev
                            | OverlayEffect::SearchAppend(_)
                            | OverlayEffect::SearchBackspace
                    )
                });
                if search_mutates && state != Overlay::Search {
                    failures.push(format!("{ctx}: search mutation outside Overlay::Search"));
                }
                let picker_mutates = step.effects.iter().any(|e| {
                    matches!(
                        e,
                        OverlayEffect::DirPickerMoveUp
                            | OverlayEffect::DirPickerMoveDown
                            | OverlayEffect::DirPickerBackspace
                            | OverlayEffect::DirPickerPush(_)
                            | OverlayEffect::DirPickerAccept
                    )
                });
                if picker_mutates && state != Overlay::DirPicker {
                    failures.push(format!("{ctx}: picker mutation outside Overlay::DirPicker"));
                }
                let session_picker_mutates = step.effects.iter().any(|e| {
                    matches!(
                        e,
                        OverlayEffect::SessionPickerMoveUp
                            | OverlayEffect::SessionPickerMoveDown
                            | OverlayEffect::SessionPickerBackspace
                            | OverlayEffect::SessionPickerPush(_)
                            | OverlayEffect::SessionPickerAccept
                    )
                });
                if session_picker_mutates && state != Overlay::SessionPicker {
                    failures.push(format!(
                        "{ctx}: session-picker mutation outside Overlay::SessionPicker"
                    ));
                }

                match &event {
                    OverlayEvent::OpenSearch => {
                        if step.state != Overlay::Search {
                            failures.push(format!("{ctx}: OpenSearch must land in Search"));
                        }
                    }
                    OverlayEvent::OpenDirPicker => {
                        if step.state != Overlay::DirPicker {
                            failures.push(format!("{ctx}: OpenDirPicker must land in DirPicker"));
                        }
                    }
                    OverlayEvent::OpenSessionPicker => {
                        if step.state != Overlay::SessionPicker {
                            failures.push(format!(
                                "{ctx}: OpenSessionPicker must land in SessionPicker"
                            ));
                        }
                    }
                    OverlayEvent::Key(_) => match state {
                        // THE Esc-eating LAW (2026-06-11): no key is
                        // consumed, no effect runs, with no overlay
                        // open — the consuming arms do not exist.
                        Overlay::None => {
                            if step.routing != OverlayRouting::FallThrough {
                                failures.push(format!("{ctx}: key consumed with no overlay open"));
                            }
                            if !step.effects.is_empty() {
                                failures.push(format!("{ctx}: effects ran with no overlay open"));
                            }
                            if step.state != Overlay::None {
                                failures.push(format!("{ctx}: key opened an overlay by itself"));
                            }
                        }
                        // An open overlay owns the keyboard totally.
                        Overlay::Search | Overlay::DirPicker | Overlay::SessionPicker => {
                            if step.routing != OverlayRouting::Consumed {
                                failures.push(format!("{ctx}: open overlay leaked a key"));
                            }
                        }
                    },
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} overlay matrix violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// THE Esc-eating LAW, named (review finding 2026-06-11): the
    /// arms that consume Search Close/Next/Prev exist ONLY in the
    /// `Overlay::Search` state arm. On `Overlay::None` the nav chords
    /// fall through (bare Esc reaches vim/helix/fzf); on
    /// `Overlay::DirPicker` they are consumed-without-search-effects
    /// (the picker matches raw keys, mirroring the pre-FSM block).
    #[test]
    fn search_nav_arms_exist_only_in_search_state() {
        let mut failures: Vec<String> = Vec::new();
        for nav in [SearchNav::Close, SearchNav::Next, SearchNav::Prev] {
            let ev = OverlayEvent::Key(OverlayKey::nav_only(nav));
            let none_step = Overlay::None.on_event(ev.clone());
            if none_step.routing != OverlayRouting::FallThrough || !none_step.effects.is_empty() {
                failures.push(format!("{nav:?}: consumed/effectful on Overlay::None"));
            }
            let dp_step = Overlay::DirPicker.on_event(ev.clone());
            let dp_search_fx = dp_step.effects.iter().any(|e| {
                matches!(
                    e,
                    OverlayEffect::SearchClose
                        | OverlayEffect::SearchNext
                        | OverlayEffect::SearchPrev
                )
            });
            if dp_search_fx {
                failures.push(format!("{nav:?}: search effect leaked into DirPicker state"));
            }
            let search_step = Overlay::Search.on_event(ev);
            if search_step.routing != OverlayRouting::Consumed || search_step.effects.is_empty() {
                failures.push(format!("{nav:?}: not handled in Overlay::Search"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} nav-arm violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Press routing: the shift bypass arms only when tracking is
    /// actually on (it is an *override* of tracking, not a general
    /// shift mode) — and a forwarded press can never carry it
    /// (structural: `ForwardedPress` has no bypass field).
    #[test]
    fn press_routing_truth_table() {
        let mut failures: Vec<String> = Vec::new();
        for (tracking_on, shift_local, want) in [
            (false, false, PressRouting::Select { bypass: false }),
            (false, true, PressRouting::Select { bypass: false }),
            (true, false, PressRouting::Forward),
            (true, true, PressRouting::Select { bypass: true }),
        ] {
            let got = route_left_press(tracking_on, shift_local);
            if got != want {
                failures.push(format!(
                    "route_left_press({tracking_on}, {shift_local}) = {got:?}, want {want:?}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} routing rows diverged:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}
