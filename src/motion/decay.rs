//! `Decay` — a scalar falling exponentially toward zero.
//!
//! The decay arm of the algebra: a value that fades on its own, with no
//! target and no fixed duration — a glow after a bell, the snow-pulse
//! intensity, a heat that cools. It is the same exponential law
//! [`crate::ux::scroll_kinetics::ScrollKinetics`] bleeds its velocity off
//! with (`v *= e^(-friction·dt)`), factored out as its own primitive.
//!
//! ## One law, three tunings
//!
//! Decay is `value *= e^(-λ·dt)` for a per-second rate `λ`. Two ergonomic
//! constructors reach the same law from the two ways the codebase already
//! thinks about it:
//!
//! * [`from_retain_per_60fps`](Decay::from_retain_per_60fps) — mado's
//!   snow and glow both fade by `0.92^(dt·60)` per frame; that is exactly
//!   `e^(-λ·dt)` with `λ = -60·ln(0.92)`. This constructor takes the
//!   `0.92` retain factor directly, collapsing the *duplicated* decay
//!   formula those two effects hand-roll into one primitive.
//! * [`with_half_life`](Decay::with_half_life) — the physical reading:
//!   the value halves every `h` seconds (`λ = ln2 / h`).
//!
//! Being framerate-independent (a function of `dt`, not frame count) is
//! the property; expressing it once is the win.

use super::Advance;

/// The per-frame decay MULTIPLIER for a per-60fps retention factor at
/// timestep `dt` — `retain^(dt·60)`, the frame-rate-independent
/// `0.92^(dt·60)` shape mado's snow typing-pulse and the bell glow both
/// hand-roll (and which engawa-wgpu's `glow_on_bell.rs` documents as the
/// shared decay). This one-line free fn is the collapse of that
/// twice-verbatim expression (Prime Directive: the 2nd copy is the
/// extract trigger).
///
/// Use this to multiply an *existing in-place accumulator* (a particle
/// sim's own state term); use the [`Decay`] struct when the decaying
/// value is standalone. It deliberately does NOT swallow the multi-term
/// stateful particle sims — it collapses only the shared scalar factor.
///
/// > Tier-honest: this is a **2-use** consolidation, not a 3-use proof of
/// > the broader motion algebra. It earns its keep as a dedup; it does
/// > not on its own ripen the tween/curve family (that stays deferred).
#[must_use]
pub fn frame_decay(dt: f32, retain_per_60fps: f32) -> f32 {
    retain_per_60fps.powf(dt * 60.0)
}

/// Below this magnitude the value snaps to exactly 0 — a clean rest, no
/// infinite sub-perceptual tail. Matches `ScrollKinetics`' stop-epsilon
/// discipline (there in velocity units; here in the value's own units).
const STOP_EPSILON: f32 = 1e-4;

/// A scalar decaying exponentially toward zero at a per-second rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decay {
    value: f32,
    /// Per-second decay constant `λ` (`value *= e^(-λ·dt)`). `>= 0`.
    rate: f32,
}

impl Decay {
    /// A decay starting at `value` with an explicit per-second rate `λ`.
    #[must_use]
    pub fn new(value: f32, rate: f32) -> Self {
        Self {
            value,
            rate: rate.max(0.0),
        }
    }

    /// A decay whose per-60fps-frame retention is `retain` — i.e. each
    /// `1/60 s` the value is multiplied by `retain`. This is mado's snow
    /// / glow `0.92^(dt·60)` shape: pass `0.92`. `retain` is clamped to
    /// `(0, 1]`; `1.0` never decays, values `<= 0` snap straight to rest.
    #[must_use]
    pub fn from_retain_per_60fps(value: f32, retain: f32) -> Self {
        let retain = retain.clamp(f32::MIN_POSITIVE, 1.0);
        // 0.92^(dt·60) = e^(dt·60·ln 0.92) = e^(-λ·dt), λ = -60·ln(retain).
        let rate = -60.0 * retain.ln();
        Self {
            value,
            rate: rate.max(0.0),
        }
    }

    /// A decay that halves every `half_life_secs` seconds
    /// (`λ = ln2 / h`). A non-positive half-life means "instantly gone".
    #[must_use]
    pub fn with_half_life(value: f32, half_life_secs: f32) -> Self {
        if half_life_secs <= 0.0 {
            return Self {
                value: 0.0,
                rate: f32::INFINITY,
            };
        }
        Self {
            value,
            rate: std::f32::consts::LN_2 / half_life_secs,
        }
    }

    /// Reset to a fresh starting value, keeping the rate (e.g. a bell
    /// re-rings the glow to full).
    pub fn reset_to(&mut self, value: f32) {
        self.value = value;
    }
}

impl Advance for Decay {
    fn advance(&mut self, dt: f32) -> f32 {
        if dt > 0.0 {
            self.value *= (-self.rate * dt).exp();
            if self.value.abs() < STOP_EPSILON {
                self.value = 0.0;
            }
        }
        self.value
    }

    fn value(&self) -> f32 {
        self.value
    }

    fn is_active(&self) -> bool {
        self.value != 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn retain_per_60fps_matches_the_hand_rolled_formula() {
        // The exact shape snow/glow hand-roll: value *= 0.92 each 1/60 s.
        // Our Decay must reproduce it frame-for-frame at 60fps.
        let mut d = Decay::from_retain_per_60fps(1.0, 0.92);
        let mut hand = 1.0_f32;
        let dt = 1.0 / 60.0;
        for _ in 0..30 {
            d.advance(dt);
            hand *= 0.92;
            assert!(
                (d.value() - hand).abs() < 1e-4,
                "decay {} drifted from hand-rolled 0.92^n {hand}",
                d.value()
            );
        }
    }

    #[test]
    fn frame_decay_matches_the_verbatim_inline_shape() {
        // Byte-pin: `frame_decay` is EXACTLY the `0.92^(dt·60)` the snow
        // + glow sites hand-rolled. At dt = 1/60 it is the retain factor;
        // in general it equals the inline expression it replaced; at
        // dt = 0 the multiplier is 1.0 (no decay).
        let dt = 1.0 / 60.0;
        assert!(
            (frame_decay(dt, 0.92) - 0.92).abs() < 1e-6,
            "one 60fps frame = retain"
        );
        for &(dt, r) in &[(1.0 / 60.0, 0.92_f32), (1.0 / 120.0, 0.85), (0.05, 0.5)] {
            assert!(
                (frame_decay(dt, r) - r.powf(dt * 60.0)).abs() < 1e-6,
                "frame_decay must equal the inline r^(dt·60)"
            );
        }
        assert_eq!(frame_decay(0.0, 0.92), 1.0, "dt=0 must not decay");
    }

    #[test]
    fn dt_zero_is_a_strict_noop() {
        let mut d = Decay::from_retain_per_60fps(1.0, 0.92);
        let before = d;
        let v = d.advance(0.0);
        assert_eq!(d, before, "dt=0 must not decay");
        assert_eq!(v, before.value());
    }

    #[test]
    fn half_life_halves_on_schedule() {
        let mut d = Decay::with_half_life(1.0, 0.25);
        d.advance(0.25);
        assert!(
            (d.value() - 0.5).abs() < 1e-3,
            "one half-life halves the value"
        );
    }

    #[test]
    fn decays_to_a_clean_rest() {
        let mut d = Decay::from_retain_per_60fps(1.0, 0.5);
        for _ in 0..1000 {
            d.advance(1.0 / 60.0);
        }
        assert_eq!(d.value(), 0.0, "must reach exactly 0, no infinite tail");
        assert!(!d.is_active());
    }

    proptest! {
        /// dt-invariance under the semigroup law: e^(-λa)·e^(-λb) =
        /// e^(-λ(a+b)). Advancing by a then b lands where advancing by
        /// a+b lands (away from the epsilon floor).
        #[test]
        fn decay_is_dt_invariant(a in 0.001f32..0.2, b in 0.001f32..0.2) {
            let mut split = Decay::new(1.0, 3.0);
            split.advance(a);
            split.advance(b);

            let mut once = Decay::new(1.0, 3.0);
            once.advance(a + b);

            prop_assert!((split.value() - once.value()).abs() < 1e-4,
                "split {} vs once {}", split.value(), once.value());
        }

        /// Monotone non-increasing in magnitude — a decay never grows.
        #[test]
        fn decay_never_grows(start in 0.01f32..1.0, dt in 0.001f32..0.1) {
            let mut d = Decay::new(start, 2.0);
            let before = d.value();
            let after = d.advance(dt);
            prop_assert!(after <= before + 1e-6, "decay grew: {before} -> {after}");
        }
    }
}
