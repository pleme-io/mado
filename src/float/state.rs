//! Floating-surface state: the surface record, the [`FloatFocus`] z-stack, and
//! the per-surface interaction FSM ([`transition`]).
//!
//! The z-stack is the load-bearing correctness choice. mado's grid overlays
//! obey a *single-overlay law* (one `overlay_focus` value ⇒ exactly one visible
//! overlay) — the invariant that made the 2026-06-21 double-draw bug
//! unrepresentable. A floating browser is **not** a modal picker: N surfaces
//! coexist with the grid and with pickers, so they need a *proper z-order*, not
//! a sibling bool. [`FloatFocus`] is that z-order, with the "at most one surface
//! is keyboard-focused" invariant enforced by construction (every mutator
//! re-establishes it) rather than hoped.
//!
//! The interaction FSM ([`transition`]) is a pure total function
//! `(FloatState, FloatEvent) → FloatStep` with **no wildcard arm** on the state
//! match — a new [`FloatState`] is a compile error until every transition is
//! written (the "make an unhandled state unrepresentable" discipline, mirroring
//! [`crate::ux::modes`]).

use egaku::Rect;
use pleme_allvariants_derive::AllVariants;
use pleme_kindstr_derive::KindStr;

use super::geom::RectProvenance;

/// A floating browser surface's stable identity. Minted per `browser_open`,
/// bound 1:1 to a tear `SessionId` at the L4 control layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BrowserId(pub u32);

impl BrowserId {
    /// The next id in sequence (monotonic minting helper).
    #[must_use]
    pub fn next(self) -> BrowserId {
        BrowserId(self.0.wrapping_add(1))
    }
}

/// The coarse interaction mode of a surface — the sluggable/tabulatable
/// projection of [`FloatState`] (which additionally carries transient drag/snap
/// data). `KindStr` gives the MCP/lisp slug, `AllVariants` the reflection const.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, KindStr, AllVariants)]
pub enum FloatMode {
    #[kind(name = "docked")]
    Docked,
    #[kind(name = "floating")]
    Floating,
    #[kind(name = "dragging")]
    Dragging,
    #[kind(name = "snapping")]
    Snapping,
}

/// One floating browser surface's placement + focus record. `Copy` (pure data).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatingSurface {
    /// Stable identity (bound to a tear session at L4).
    pub id: BrowserId,
    /// Current on-screen rect (panel px).
    pub rect: Rect,
    /// Stacking order — higher is nearer the viewer.
    pub z: u16,
    /// Whether this surface owns the keyboard (at most one in a [`FloatFocus`]).
    pub focused: bool,
    /// How this surface's rect was last set.
    pub provenance: RectProvenance,
    /// Coarse interaction mode.
    pub mode: FloatMode,
}

/// The z-ordered stack of floating surfaces + the single-focus invariant.
///
/// Invariants (re-established by every mutator, checked in tests):
/// - ids are unique,
/// - at most one surface has `focused == true`,
/// - a raised surface has the strictly-greatest `z`.
#[derive(Debug, Clone, Default)]
pub struct FloatFocus {
    surfaces: Vec<FloatingSurface>,
    next_z: u16,
}

impl FloatFocus {
    /// An empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new surface on top, focused. Returns `false` (a no-op) if `id`
    /// already exists — ids are unique by construction.
    pub fn open(&mut self, id: BrowserId, rect: Rect, provenance: RectProvenance) -> bool {
        if self.surfaces.iter().any(|s| s.id == id) {
            return false;
        }
        for s in &mut self.surfaces {
            s.focused = false;
        }
        self.next_z = self.next_z.wrapping_add(1);
        self.surfaces.push(FloatingSurface {
            id,
            rect,
            z: self.next_z,
            focused: true,
            provenance,
            mode: FloatMode::Floating,
        });
        true
    }

    /// Raise `id` to the top of the stack and make it the *only* focused
    /// surface. Returns `false` if `id` is absent.
    pub fn raise(&mut self, id: BrowserId) -> bool {
        if !self.surfaces.iter().any(|s| s.id == id) {
            return false;
        }
        self.next_z = self.next_z.wrapping_add(1);
        let top = self.next_z;
        for s in &mut self.surfaces {
            if s.id == id {
                s.z = top;
                s.focused = true;
            } else {
                s.focused = false;
            }
        }
        true
    }

    /// Close `id`. If it was focused, focus falls to the new top surface (if
    /// any). Returns `false` if `id` was absent.
    pub fn close(&mut self, id: BrowserId) -> bool {
        let before = self.surfaces.len();
        self.surfaces.retain(|s| s.id != id);
        if self.surfaces.len() == before {
            return false;
        }
        if self.focused().is_none() {
            if let Some(top_id) = self.top().map(|s| s.id) {
                for s in &mut self.surfaces {
                    s.focused = s.id == top_id;
                }
            }
        }
        true
    }

    /// Move a surface's rect + record its new provenance (drag / snap commit).
    pub fn set_rect(&mut self, id: BrowserId, rect: Rect, provenance: RectProvenance) -> bool {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.id == id) {
            s.rect = rect;
            s.provenance = provenance;
            true
        } else {
            false
        }
    }

    /// Set a surface's coarse interaction mode.
    pub fn set_mode(&mut self, id: BrowserId, mode: FloatMode) -> bool {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.id == id) {
            s.mode = mode;
            true
        } else {
            false
        }
    }

    /// The focused surface's id, if any.
    #[must_use]
    pub fn focused(&self) -> Option<BrowserId> {
        self.surfaces.iter().find(|s| s.focused).map(|s| s.id)
    }

    /// Borrow a surface by id.
    #[must_use]
    pub fn get(&self, id: BrowserId) -> Option<&FloatingSurface> {
        self.surfaces.iter().find(|s| s.id == id)
    }

    /// The top (greatest-z) surface.
    #[must_use]
    pub fn top(&self) -> Option<&FloatingSurface> {
        self.surfaces.iter().max_by_key(|s| s.z)
    }

    /// How many surfaces are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    /// Whether the stack is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// Surfaces in draw order — bottom (lowest z) to top (highest z).
    #[must_use]
    pub fn draw_order(&self) -> Vec<&FloatingSurface> {
        let mut v: Vec<&FloatingSurface> = self.surfaces.iter().collect();
        v.sort_by_key(|s| s.z);
        v
    }

    /// The topmost surface whose rect contains `(x, y)` — hit-testing for a
    /// pointer press, respecting z-order.
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<BrowserId> {
        self.surfaces
            .iter()
            .filter(|s| s.rect.contains(x, y))
            .max_by_key(|s| s.z)
            .map(|s| s.id)
    }
}

/// The per-surface interaction state. Carries transient drag/snap data; the
/// coarse projection is [`FloatMode`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FloatState {
    /// Docked into the grid layout (not floating).
    Docked,
    /// Floating free, idle.
    Floating,
    /// Being dragged; `grab_offset` is (cursor − rect.origin) at grab time.
    Dragging { grab_offset: (f32, f32) },
    /// Mid-drag over a snap zone; `target` is the previewed snap rect,
    /// `grab_offset` retained so leaving the zone resumes a free drag.
    Snapping {
        target: Rect,
        grab_offset: (f32, f32),
    },
}

/// An event fed to the interaction FSM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FloatEvent {
    /// Undock → floating.
    Float,
    /// Dock back into the grid.
    Dock,
    /// Begin a title-bar drag; `offset` is (cursor − rect.origin).
    GrabTitle { offset: (f32, f32) },
    /// Pointer moved to `cursor` during a drag.
    Move { cursor: (f32, f32) },
    /// The cursor entered a snap zone previewing `target`.
    SnapPreview { target: Rect },
    /// Pointer released.
    Release,
    /// Abort the current interaction.
    Cancel,
}

/// A side effect the FSM asks the host to perform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FloatEffect {
    /// Move the surface origin to (x, y).
    MoveTo(f32, f32),
    /// Commit the surface to this snapped rect.
    SnapTo(Rect),
    /// A drag started (host may show a drag cursor).
    BeginDrag,
    /// A drag/interaction ended.
    EndDrag,
    /// The surface needs a repaint (e.g. a snap-preview outline changed).
    Repaint,
}

/// The result of one FSM step: the next state + the effects to apply.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatStep {
    /// The state after the event.
    pub state: FloatState,
    /// Effects the host applies, in order.
    pub effects: Vec<FloatEffect>,
}

impl FloatStep {
    fn to(state: FloatState, effects: &[FloatEffect]) -> Self {
        Self {
            state,
            effects: effects.to_vec(),
        }
    }

    fn stay(state: FloatState) -> Self {
        Self {
            state,
            effects: Vec::new(),
        }
    }
}

/// The pure, total interaction transition. **No wildcard arm on the state
/// match** — adding a [`FloatState`] variant is a compile error until its
/// transitions are written. Events irrelevant in a given state are explicit
/// no-ops (so a new [`FloatEvent`] is *also* a compile error via the inner
/// exhaustive match).
#[must_use]
pub fn transition(state: FloatState, ev: FloatEvent) -> FloatStep {
    use FloatEffect as Fx;
    use FloatEvent as E;
    use FloatState as S;
    match state {
        S::Docked => match ev {
            E::Float => FloatStep::to(S::Floating, &[Fx::Repaint]),
            E::Dock
            | E::GrabTitle { .. }
            | E::Move { .. }
            | E::SnapPreview { .. }
            | E::Release
            | E::Cancel => FloatStep::stay(state),
        },
        S::Floating => match ev {
            E::Dock => FloatStep::to(S::Docked, &[Fx::Repaint]),
            E::GrabTitle { offset } => FloatStep::to(
                S::Dragging {
                    grab_offset: offset,
                },
                &[Fx::BeginDrag],
            ),
            E::Float | E::Move { .. } | E::SnapPreview { .. } | E::Release | E::Cancel => {
                FloatStep::stay(state)
            }
        },
        S::Dragging { grab_offset } => match ev {
            E::Move { cursor } => FloatStep::to(
                S::Dragging { grab_offset },
                &[Fx::MoveTo(
                    cursor.0 - grab_offset.0,
                    cursor.1 - grab_offset.1,
                )],
            ),
            E::SnapPreview { target } => FloatStep::to(
                S::Snapping {
                    target,
                    grab_offset,
                },
                &[Fx::Repaint],
            ),
            E::Release | E::Cancel => FloatStep::to(S::Floating, &[Fx::EndDrag]),
            E::Float | E::Dock | E::GrabTitle { .. } => FloatStep::stay(state),
        },
        S::Snapping {
            target,
            grab_offset,
        } => match ev {
            E::SnapPreview { target: t } => FloatStep::to(
                S::Snapping {
                    target: t,
                    grab_offset,
                },
                &[Fx::Repaint],
            ),
            // Leaving the zone (a bare move) resumes a free drag.
            E::Move { cursor } => FloatStep::to(
                S::Dragging { grab_offset },
                &[Fx::MoveTo(
                    cursor.0 - grab_offset.0,
                    cursor.1 - grab_offset.1,
                )],
            ),
            E::Release => FloatStep::to(S::Floating, &[Fx::SnapTo(target)]),
            E::Cancel => FloatStep::to(S::Floating, &[Fx::EndDrag]),
            E::Float | E::Dock | E::GrabTitle { .. } => FloatStep::stay(state),
        },
    }
}

/// A tiny stateful wrapper over [`transition`] for host call sites.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatWindowFsm {
    state: FloatState,
}

impl Default for FloatWindowFsm {
    fn default() -> Self {
        Self {
            state: FloatState::Docked,
        }
    }
}

impl FloatWindowFsm {
    /// Start floating (skip the docked state).
    #[must_use]
    pub fn floating() -> Self {
        Self {
            state: FloatState::Floating,
        }
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> FloatState {
        self.state
    }

    /// Feed an event; advance the state; return the effects to apply.
    pub fn on_event(&mut self, ev: FloatEvent) -> Vec<FloatEffect> {
        let step = transition(self.state, ev);
        self.state = step.state;
        step.effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn float_mode_slugs_round_trip_and_are_complete() {
        assert_eq!(FloatMode::ALL.len(), 4);
        for m in FloatMode::ALL {
            assert_eq!(FloatMode::from_str_kind(m.as_str()), Some(*m));
        }
    }

    #[test]
    fn open_focuses_the_new_surface_and_defocuses_others() {
        let mut f = FloatFocus::new();
        assert!(f.open(
            BrowserId(1),
            r(0.0, 0.0, 100.0, 100.0),
            RectProvenance::Configured
        ));
        assert_eq!(f.focused(), Some(BrowserId(1)));
        assert!(f.open(
            BrowserId(2),
            r(10.0, 10.0, 100.0, 100.0),
            RectProvenance::FreeDragged
        ));
        assert_eq!(f.focused(), Some(BrowserId(2)));
        assert_eq!(f.len(), 2);
        // duplicate id is a no-op
        assert!(!f.open(
            BrowserId(2),
            r(0.0, 0.0, 1.0, 1.0),
            RectProvenance::Configured
        ));
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn raise_makes_the_target_top_and_sole_focus() {
        let mut f = FloatFocus::new();
        f.open(
            BrowserId(1),
            r(0.0, 0.0, 10.0, 10.0),
            RectProvenance::Configured,
        );
        f.open(
            BrowserId(2),
            r(0.0, 0.0, 10.0, 10.0),
            RectProvenance::Configured,
        );
        assert_eq!(f.focused(), Some(BrowserId(2)));
        assert!(f.raise(BrowserId(1)));
        assert_eq!(f.focused(), Some(BrowserId(1)));
        assert_eq!(f.top().map(|s| s.id), Some(BrowserId(1)));
        assert!(!f.raise(BrowserId(99)));
    }

    #[test]
    fn close_reassigns_focus_to_the_new_top() {
        let mut f = FloatFocus::new();
        f.open(
            BrowserId(1),
            r(0.0, 0.0, 10.0, 10.0),
            RectProvenance::Configured,
        );
        f.open(
            BrowserId(2),
            r(0.0, 0.0, 10.0, 10.0),
            RectProvenance::Configured,
        );
        assert!(f.close(BrowserId(2)));
        assert_eq!(f.focused(), Some(BrowserId(1)));
        assert!(f.close(BrowserId(1)));
        assert!(f.is_empty());
        assert_eq!(f.focused(), None);
        assert!(!f.close(BrowserId(1)));
    }

    #[test]
    fn hit_test_respects_z_order() {
        let mut f = FloatFocus::new();
        f.open(
            BrowserId(1),
            r(0.0, 0.0, 100.0, 100.0),
            RectProvenance::Configured,
        );
        f.open(
            BrowserId(2),
            r(50.0, 50.0, 100.0, 100.0),
            RectProvenance::Configured,
        );
        // (60,60) is under both; the top (id 2, opened last) wins.
        assert_eq!(f.hit_test(60.0, 60.0), Some(BrowserId(2)));
        // (10,10) only under id 1.
        assert_eq!(f.hit_test(10.0, 10.0), Some(BrowserId(1)));
        // nowhere.
        assert_eq!(f.hit_test(500.0, 500.0), None);
    }

    #[test]
    fn drag_then_snap_commit_emits_the_snap_rect() {
        let mut fsm = FloatWindowFsm::floating();
        // grab
        let fx = fsm.on_event(FloatEvent::GrabTitle { offset: (5.0, 3.0) });
        assert_eq!(fx, vec![FloatEffect::BeginDrag]);
        // move
        let fx = fsm.on_event(FloatEvent::Move {
            cursor: (105.0, 203.0),
        });
        assert_eq!(fx, vec![FloatEffect::MoveTo(100.0, 200.0)]);
        // enter a snap zone
        let target = r(0.0, 0.0, 500.0, 800.0);
        let fx = fsm.on_event(FloatEvent::SnapPreview { target });
        assert_eq!(fx, vec![FloatEffect::Repaint]);
        assert!(matches!(fsm.state(), FloatState::Snapping { .. }));
        // release → commit the snap
        let fx = fsm.on_event(FloatEvent::Release);
        assert_eq!(fx, vec![FloatEffect::SnapTo(target)]);
        assert_eq!(fsm.state(), FloatState::Floating);
    }

    #[test]
    fn leaving_the_zone_resumes_a_free_drag() {
        let mut fsm = FloatWindowFsm::floating();
        fsm.on_event(FloatEvent::GrabTitle { offset: (0.0, 0.0) });
        fsm.on_event(FloatEvent::SnapPreview {
            target: r(0.0, 0.0, 500.0, 800.0),
        });
        assert!(matches!(fsm.state(), FloatState::Snapping { .. }));
        let fx = fsm.on_event(FloatEvent::Move {
            cursor: (300.0, 300.0),
        });
        assert_eq!(fx, vec![FloatEffect::MoveTo(300.0, 300.0)]);
        assert!(matches!(fsm.state(), FloatState::Dragging { .. }));
    }

    // A tiny hand-rolled generator so the property test needs no data derive.
    fn ops(seed: u64) -> Vec<u8> {
        let mut v = Vec::new();
        let mut x = seed;
        for _ in 0..24 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            v.push((x >> 33) as u8 % 5);
        }
        v
    }

    proptest! {
        #[test]
        fn focus_stack_invariants_hold_after_any_op_sequence(seed in any::<u64>()) {
            let mut f = FloatFocus::new();
            let mut next = BrowserId(1);
            for op in ops(seed) {
                match op {
                    0 => { if f.open(next, r(0.0, 0.0, 10.0, 10.0), RectProvenance::Configured) { next = next.next(); } }
                    1 => { let _ = f.raise(BrowserId(1)); }
                    2 => { let _ = f.raise(BrowserId(2)); }
                    3 => { let _ = f.close(BrowserId(1)); }
                    _ => { let _ = f.close(BrowserId(2)); }
                }
                // Invariant 1: at most one focused.
                let focused = f.draw_order().iter().filter(|s| s.focused).count();
                prop_assert!(focused <= 1, "more than one focused surface");
                // Invariant 2: ids unique.
                let mut ids: Vec<_> = f.draw_order().iter().map(|s| s.id).collect();
                let n = ids.len();
                ids.sort_unstable();
                ids.dedup();
                prop_assert_eq!(ids.len(), n, "duplicate surface id");
                // Invariant 3: a non-empty stack always has a focused surface.
                prop_assert_eq!(f.is_empty(), f.focused().is_none());
            }
        }

        #[test]
        fn transition_is_total_and_never_panics(
            si in 0u8..4, ei in 0u8..7, ax in -50.0f32..50.0, ay in -50.0f32..50.0,
        ) {
            let state = match si {
                0 => FloatState::Docked,
                1 => FloatState::Floating,
                2 => FloatState::Dragging { grab_offset: (ax, ay) },
                _ => FloatState::Snapping { target: r(0.0, 0.0, 100.0, 100.0), grab_offset: (ax, ay) },
            };
            let ev = match ei {
                0 => FloatEvent::Float,
                1 => FloatEvent::Dock,
                2 => FloatEvent::GrabTitle { offset: (ax, ay) },
                3 => FloatEvent::Move { cursor: (ax, ay) },
                4 => FloatEvent::SnapPreview { target: r(0.0, 0.0, 10.0, 10.0) },
                5 => FloatEvent::Release,
                _ => FloatEvent::Cancel,
            };
            // Must not panic and must yield a well-formed step.
            let step = transition(state, ev);
            prop_assert!(step.effects.len() <= 2);
        }
    }
}
