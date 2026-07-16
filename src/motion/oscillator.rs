//! `Oscillator` — a periodic phase driven by elapsed time.
//!
//! The oscillator arm of the algebra: a value that repeats forever — a
//! cursor blink, a text-`BLINK`-attribute toggle, a pulsing highlight.
//! Unlike the finite arms (tween, decay) an oscillator never rests; it is
//! a pure function of elapsed time modulo its period.
//!
//! ## Stateless first (the migration target)
//!
//! mado derives the cursor blink from the *global* render clock, not from
//! an accumulated per-cursor timer: `blink_phase_on(elapsed)` with
//! `period = blink_rate_ms/1000·2`. That exact computation is currently
//! open-coded at three sites in `render.rs`. [`blink_on`] is the single
//! primitive those three sites collapse into — a stateless
//! `(elapsed, period) → bool` so there is one blink law, not three
//! hand-kept copies. [`Oscillator`] wraps it with an accumulated clock
//! for callers that own their own time.

use super::{Advance, Seconds};

/// Whether a blink is in its *on* half at `elapsed_secs`, given the full
/// on-off `period_secs` (on for the first half of each period). A
/// non-positive period is always on (blink disabled). This is the one
/// law the three `render.rs` cursor-blink sites share.
#[must_use]
pub fn blink_on(elapsed_secs: f32, period_secs: f32) -> bool {
    if period_secs <= 0.0 {
        return true;
    }
    elapsed_secs.rem_euclid(period_secs) < period_secs * 0.5
}

/// A periodic oscillator with an accumulated clock. `phase_on` gives the
/// square-wave blink; `wave` gives a smooth `[0, 1]` sine for pulsing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oscillator {
    period: Seconds,
    elapsed: Seconds,
}

impl Oscillator {
    /// An oscillator with the given full on-off period, phase 0.
    #[must_use]
    pub fn new(period: Seconds) -> Self {
        Self { period, elapsed: Seconds::new(0.0) }
    }

    /// The square-wave blink state (on for the first half of each period).
    #[must_use]
    pub fn phase_on(&self) -> bool {
        blink_on(self.elapsed.get(), self.period.get())
    }

    /// A smooth `[0, 1]` sine over the period — for a pulse rather than a
    /// hard blink. `0.5` at phase 0, peaks at a quarter period.
    #[must_use]
    pub fn wave(&self) -> f32 {
        let p = self.period.get();
        if p <= 0.0 {
            return 1.0;
        }
        let theta = std::f32::consts::TAU * self.elapsed.get() / p;
        0.5 * theta.sin() + 0.5
    }
}

impl Advance for Oscillator {
    fn advance(&mut self, dt: f32) -> f32 {
        if dt > 0.0 {
            self.elapsed = self.elapsed.inc_by(dt);
        }
        self.value()
    }

    /// The oscillator's scalar reading is its `[0, 1]` sine wave.
    fn value(&self) -> f32 {
        self.wave()
    }

    /// An oscillator is always active — it never comes to rest (a period
    /// of 0 is the degenerate "always on" that also reports active).
    fn is_active(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::secs;
    use proptest::prelude::*;

    #[test]
    fn blink_on_is_on_for_the_first_half() {
        // period 1.0s: on in [0, 0.5), off in [0.5, 1.0).
        assert!(blink_on(0.0, 1.0));
        assert!(blink_on(0.49, 1.0));
        assert!(!blink_on(0.5, 1.0));
        assert!(!blink_on(0.99, 1.0));
        // Wraps: 1.0 is a fresh period start → on again.
        assert!(blink_on(1.0, 1.0));
        assert!(blink_on(1.25, 1.0));
    }

    #[test]
    fn non_positive_period_is_always_on() {
        assert!(blink_on(3.7, 0.0), "period 0 = blink disabled = always on");
    }

    #[test]
    fn oscillator_dt_zero_is_a_noop() {
        let mut o = Oscillator::new(secs(1.0));
        o.advance(0.3);
        let before = o;
        o.advance(0.0);
        assert_eq!(o, before, "dt=0 must not move the oscillator");
    }

    #[test]
    fn oscillator_tracks_the_stateless_law() {
        // The stateful oscillator's phase must equal the stateless
        // blink_on of the same accumulated elapsed — one law, two surfaces.
        let mut o = Oscillator::new(secs(0.8));
        let mut elapsed = 0.0_f32;
        let dt = 1.0 / 60.0;
        for _ in 0..50 {
            o.advance(dt);
            elapsed += dt;
            assert_eq!(o.phase_on(), blink_on(elapsed, 0.8));
        }
    }

    proptest! {
        /// The stateless blink law is periodic: `blink_on(e)` equals
        /// `blink_on(e + period)` for any elapsed.
        #[test]
        fn blink_is_periodic(e in 0.0f32..10.0, p in 0.05f32..2.0) {
            prop_assert_eq!(blink_on(e, p), blink_on(e + p, p),
                "blink not periodic at e={}, p={}", e, p);
        }

        /// The pulse wave never leaves [0, 1].
        #[test]
        fn wave_stays_in_unit_range(e in 0.0f32..10.0) {
            let mut o = Oscillator::new(secs(0.7));
            o.advance(e);
            let w = o.wave();
            prop_assert!((0.0..=1.0).contains(&w), "wave {w} escaped [0,1]");
        }
    }
}
