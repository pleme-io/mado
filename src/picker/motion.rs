use std::cell::{Cell, RefCell};

use crate::motion::{Curve, EasingKind, Glide, secs};
use crate::picker::component::OverlaySpec;

const BAR_SECS: f32 = 0.11;
const CARD_SECS: f32 = 0.14;
const CLOSE_SECS: f32 = 0.12;

#[derive(Debug)]
pub struct OverlayMotion {
    clock: Cell<f32>,
    enabled: Cell<bool>,
    settled: Cell<bool>,
    bar: Cell<Glide>,
    card_w: Cell<Glide>,
    card_h: Cell<Glide>,
    was_open: Cell<bool>,
    closed_at: Cell<Option<f32>>,
    ghost: RefCell<Option<OverlaySpec>>,
}

impl Default for OverlayMotion {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayMotion {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clock: Cell::new(0.0),
            enabled: Cell::new(true),
            settled: Cell::new(false),
            bar: Cell::new(glide(BAR_SECS)),
            card_w: Cell::new(glide(CARD_SECS)),
            card_h: Cell::new(glide(CARD_SECS)),
            was_open: Cell::new(false),
            closed_at: Cell::new(None),
            ghost: RefCell::new(None),
        }
    }

    pub fn begin_frame(&self, now: f32, open: bool, enabled: bool) {
        self.clock.set(now);
        self.enabled.set(enabled);
        if open {
            self.closed_at.set(None);
            *self.ghost.borrow_mut() = None;
        } else {
            self.settled.set(false);
            if self.was_open.get() && enabled && self.ghost.borrow().is_some() {
                self.closed_at.set(Some(now));
            }
        }
        self.was_open.set(open);
    }

    pub fn remember(&self, spec: &OverlaySpec) {
        if self.closed_at.get().is_none() {
            *self.ghost.borrow_mut() = Some(spec.clone());
        }
    }

    #[must_use]
    pub fn ghost(&self) -> Option<(OverlaySpec, f32)> {
        let closed = self.closed_at.get()?;
        let now = self.clock.get();
        let t = (now - closed) / CLOSE_SECS;
        if t >= 1.0 {
            self.closed_at.set(None);
            *self.ghost.borrow_mut() = None;
            return None;
        }
        let eased = Curve::named(EasingKind::Accelerate).ease(t.clamp(0.0, 1.0));
        self.ghost.borrow().clone().map(|spec| (spec, 1.0 - eased))
    }

    #[must_use]
    pub fn closing(&self) -> bool {
        self.closed_at.get().is_some()
    }

    pub fn end_frame(&self) {
        self.settled.set(true);
    }

    #[must_use]
    pub fn bar_row(&self, target: f32) -> f32 {
        self.follow(&self.bar, target)
    }

    #[must_use]
    pub fn card(&self, w: f32, h: f32) -> (f32, f32) {
        (self.follow(&self.card_w, w), self.follow(&self.card_h, h))
    }

    #[must_use]
    pub fn in_flight(&self, now: f32) -> bool {
        [&self.bar, &self.card_w, &self.card_h]
            .iter()
            .any(|g| g.get().in_flight(now))
    }

    fn follow(&self, cell: &Cell<Glide>, target: f32) -> f32 {
        let now = self.clock.get();
        let mut g = cell.get();
        if self.settled.get() && self.enabled.get() {
            g.retarget(target, now);
        } else {
            g.snap(target);
        }
        cell.set(g);
        g.sample(now)
    }
}

fn glide(seconds: f32) -> Glide {
    Glide::new(0.0, secs(seconds), Curve::named(EasingKind::Decelerate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(m: &OverlayMotion, now: f32, target: f32) -> f32 {
        m.begin_frame(now, true, true);
        let v = m.bar_row(target);
        m.end_frame();
        v
    }

    #[test]
    fn the_first_frame_after_opening_places_the_bar_without_travel() {
        let m = OverlayMotion::new();
        assert_eq!(frame(&m, 10.0, 4.0), 4.0);
        assert!(!m.in_flight(10.0));
    }

    #[test]
    fn moving_the_selection_glides_the_bar_and_lands_on_the_row() {
        let m = OverlayMotion::new();
        frame(&m, 1.0, 0.0);
        let first = frame(&m, 1.0, 1.0);
        assert_eq!(first, 0.0, "the bar starts where it was");
        let mid = frame(&m, 1.05, 1.0);
        assert!(mid > 0.0 && mid < 1.0, "{mid}");
        assert!(m.in_flight(1.05));
        assert_eq!(frame(&m, 1.2, 1.0), 1.0);
        assert!(!m.in_flight(1.2));
    }

    #[test]
    fn closing_and_reopening_never_animates_from_the_last_session() {
        let m = OverlayMotion::new();
        frame(&m, 1.0, 7.0);
        m.begin_frame(2.0, false, true);
        assert_eq!(frame(&m, 3.0, 0.0), 0.0);
    }

    #[test]
    fn the_card_resizes_smoothly_when_the_result_set_changes() {
        let m = OverlayMotion::new();
        m.begin_frame(1.0, true, true);
        assert_eq!(m.card(400.0, 300.0), (400.0, 300.0), "opens at full size");
        m.end_frame();
        m.begin_frame(1.0, true, true);
        let _ = m.card(250.0, 100.0);
        m.end_frame();
        m.begin_frame(1.07, true, true);
        let (w, h) = m.card(250.0, 100.0);
        m.end_frame();
        assert!(w < 400.0 && w > 250.0, "{w}");
        assert!(h < 300.0 && h > 100.0, "{h}");
        assert!(m.in_flight(1.07));
        m.begin_frame(1.3, true, true);
        assert_eq!(m.card(250.0, 100.0), (250.0, 100.0));
        m.end_frame();
        assert!(!m.in_flight(1.3));
    }

    fn spec() -> OverlaySpec {
        use crate::picker::component::{LineRole, OverlayLine};
        OverlaySpec::new(
            crate::config::PickerAnchor::Center,
            vec![OverlayLine::new("▶ session", LineRole::Title)],
        )
    }

    #[test]
    fn closing_leaves_a_ghost_that_fades_out_and_then_clears() {
        let m = OverlayMotion::new();
        m.begin_frame(1.0, true, true);
        m.remember(&spec());
        m.end_frame();
        m.begin_frame(2.0, false, true);
        assert!(m.closing());
        let (_, a0) = m.ghost().expect("a ghost right after close");
        assert!((a0 - 1.0).abs() < 1e-6);
        m.begin_frame(2.06, false, true);
        let (_, mid) = m.ghost().expect("still fading");
        assert!(mid > 0.0 && mid < 1.0, "{mid}");
        m.begin_frame(2.2, false, true);
        assert!(m.ghost().is_none());
        assert!(!m.closing(), "the clearing frame ends the close");
    }

    #[test]
    fn reopening_mid_close_cancels_the_ghost_and_reduced_motion_never_leaves_one() {
        let m = OverlayMotion::new();
        m.begin_frame(1.0, true, true);
        m.remember(&spec());
        m.begin_frame(2.0, false, true);
        m.begin_frame(2.05, true, true);
        assert!(!m.closing());
        let r = OverlayMotion::new();
        r.begin_frame(1.0, true, false);
        r.remember(&spec());
        r.begin_frame(2.0, false, false);
        assert!(!r.closing());
        assert!(r.ghost().is_none());
    }

    #[test]
    fn an_overlay_that_never_drew_a_spec_leaves_no_stale_ghost() {
        let m = OverlayMotion::new();
        m.begin_frame(1.0, true, true);
        m.remember(&spec());
        m.begin_frame(2.0, true, true);
        m.begin_frame(3.0, false, true);
        assert!(
            !m.closing(),
            "a search bar closing must not replay an old picker"
        );
        assert!(m.ghost().is_none());
    }

    #[test]
    fn reduced_motion_snaps_every_move() {
        let m = OverlayMotion::new();
        frame(&m, 1.0, 0.0);
        m.begin_frame(1.0, true, false);
        assert_eq!(m.bar_row(5.0), 5.0);
        m.end_frame();
        assert!(!m.in_flight(1.0));
    }
}
