//! `Tween` — a scalar moving `from → to` over a duration via a curve.
//!
//! The keyframe arm of the algebra: a value that eases from one number
//! to another across a fixed time span. Bell-flash alpha, a fade-in
//! opacity, a slide offset, a scale — anything that goes `from → to` and
//! *stops* is a `Tween`.
//!
//! The duration is a [`Seconds`] (never negative) and progress is
//! internally a [`Unit`] (always `[0, 1]`), so the curve can only ever
//! be sampled in range. A zero-duration tween is *inert* — instantly
//! complete at `to`, [`is_active`](crate::motion::Advance::is_active)
//! `false` — which is how a resting/absent animation is represented
//! without an `Option`.

use super::{Advance, Curve, Seconds, Unit};

/// A scalar eased `from → to` over `duration`, sampled by `curve`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tween {
    from: f32,
    to: f32,
    duration: Seconds,
    elapsed: Seconds,
    curve: Curve,
}

impl Tween {
    /// A tween from `from` to `to` over `duration`, eased by `curve`,
    /// starting at `elapsed = 0`.
    #[must_use]
    pub fn new(from: f32, to: f32, duration: Seconds, curve: Curve) -> Self {
        Self {
            from,
            to,
            duration,
            elapsed: Seconds::new(0.0),
            curve,
        }
    }

    /// A linear tween `from → to` over `duration`.
    #[must_use]
    pub fn linear(from: f32, to: f32, duration: Seconds) -> Self {
        Self::new(from, to, duration, Curve::Linear)
    }

    /// A resting tween — zero duration, instantly complete, value 0,
    /// inactive. The identity element: a slot that holds "no animation
    /// running" without an `Option`.
    #[must_use]
    pub fn inert() -> Self {
        Self::new(0.0, 0.0, Seconds::new(0.0), Curve::Linear)
    }

    /// Normalized progress `elapsed / duration`, clamped to `[0, 1]`.
    /// A zero-duration tween reports full progress (instantly complete).
    #[must_use]
    pub fn progress(&self) -> Unit {
        let d = self.duration.get();
        if d <= 0.0 {
            Unit::new(1.0)
        } else {
            Unit::new(self.elapsed.get() / d)
        }
    }

    /// The eased value at the current progress.
    fn eval(&self) -> f32 {
        let t = self.curve.ease(self.progress().get());
        self.from + (self.to - self.from) * t
    }

    /// The terminal value this tween eases toward.
    #[must_use]
    pub fn target(&self) -> f32 {
        self.to
    }
}

impl Advance for Tween {
    fn advance(&mut self, dt: f32) -> f32 {
        if dt > 0.0 {
            self.elapsed = self.elapsed.inc_by(dt);
        }
        self.eval()
    }

    fn value(&self) -> f32 {
        self.eval()
    }

    fn is_active(&self) -> bool {
        self.elapsed.get() < self.duration.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::secs;
    use proptest::prelude::*;

    #[test]
    fn endpoints_are_exact() {
        let mut t = Tween::linear(0.10, 0.0, secs(0.2));
        assert_eq!(t.value(), 0.10, "at elapsed=0 the value is `from`");
        // Advance well past the duration.
        t.advance(10.0);
        assert_eq!(t.value(), 0.0, "past the duration the value is `to`");
        assert!(!t.is_active(), "a finished tween is inactive");
    }

    #[test]
    fn dt_zero_is_a_strict_noop() {
        let mut t = Tween::linear(1.0, 2.0, secs(1.0));
        t.advance(0.5);
        let before = t;
        let v = t.advance(0.0);
        assert_eq!(t, before, "dt=0 must not mutate the tween");
        assert_eq!(v, before.value(), "dt=0 returns the current value");
        // Negative dt is also a no-op.
        t.advance(-1.0);
        assert_eq!(t, before, "negative dt must not mutate the tween");
    }

    #[test]
    fn inert_tween_is_the_resting_identity() {
        let t = Tween::inert();
        assert!(!t.is_active(), "inert is inactive");
        assert_eq!(t.value(), 0.0, "inert rests at 0");
    }

    #[test]
    fn linear_midpoint_is_the_average() {
        let mut t = Tween::linear(0.0, 10.0, secs(1.0));
        t.advance(0.5);
        assert!((t.value() - 5.0).abs() < 1e-5, "linear halfway = midpoint");
    }

    proptest! {
        /// dt-invariance: reaching time `T` in one big step or many small
        /// steps lands on the same value (a tween is a pure function of
        /// total elapsed time, not of frame count — the framerate-
        /// independence bell-flash's old frame counter lacked).
        #[test]
        fn splitting_dt_does_not_change_the_value(
            steps in 1usize..40, total in 0.01f32..2.0,
        ) {
            let dur = secs(0.5);
            let mut one = Tween::linear(0.10, 0.0, dur);
            one.advance(total);

            let mut many = Tween::linear(0.10, 0.0, dur);
            let step = total / steps as f32;
            for _ in 0..steps {
                many.advance(step);
            }
            prop_assert!((one.value() - many.value()).abs() < 1e-4,
                "one-step {} vs {}-step {}", one.value(), steps, many.value());
        }

        /// Progress is monotone non-decreasing under advancement.
        #[test]
        fn progress_is_monotone(a in 0.0f32..1.0, b in 0.0f32..1.0) {
            let mut t = Tween::linear(0.0, 1.0, secs(1.0));
            t.advance(a);
            let p0 = t.progress().get();
            t.advance(b);
            let p1 = t.progress().get();
            prop_assert!(p1 + 1e-6 >= p0, "progress went backwards: {p0} -> {p1}");
        }
    }
}
