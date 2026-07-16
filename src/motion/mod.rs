//! `motion` — mado's data-first animation algebra.
//!
//! ## The vision
//!
//! Every animated scalar in mado — a bell-flash alpha, a cursor-blink
//! phase, a decaying glow, a scroll glide — is **one pure function of
//! `(typed declaration, elapsed time)`**, never an imperative per-frame
//! `-= 1`. An animation is *data* (a `Tween`, a `Decay`, an
//! `Oscillator`, a `ScrollKinetics`); a frame is the *fold*
//! [`Advance::advance`] applies to it. Nothing about how a value moves
//! is scattered across the render loop; it lives in the value's type.
//!
//! This is the same shape [`crate::ux::scroll_kinetics::ScrollKinetics`]
//! already proved for scrolling — a typed sub-state advanced by a pure
//! `tick(dt)` — generalized to every animated surface. `ScrollKinetics`
//! is the **integrator** arm of this algebra; this module adds the
//! **tween**, **decay**, and **oscillator** arms and the one contract
//! they all share.
//!
//! ## Reuse, not reinvention
//!
//! The *curve vocabulary* — the cubic-bézier easing tokens and the
//! named duration scales — is the fleet's, not mado's: it lives in
//! [`ishou_tokens::motion`] (`Cubic`, `Easings`, `Durations`,
//! shader-informed so UI motion feels continuous with the GPU effects).
//! mado consumes those tokens and supplies the piece ishou lacks — the
//! **evaluator**: [`curve::Curve::ease`] samples a bézier, and the arms
//! fold that sample over time.
//!
//! > **Destination (the solve-once move):** lift this evaluator up into
//! > `ishou_tokens::motion` so egaku / quadro / tela / garasu share one
//! > motion evaluator, exactly as `Refined<T, B>` was lifted out of
//! > mado's `font_size.rs`. mado is the first consumer and the proving
//! > ground; extraction lands at the 3rd consumer (Pillar 12). Until
//! > then this module is honestly mado-local.
//!
//! ## The determinism contract (load-bearing)
//!
//! Every [`Advance::advance`] MUST be a strict no-op when `dt <= 0.0`:
//! it returns the current value and mutates nothing. The render loop's
//! L1/L2 determinism ladders render headless at `elapsed = 0` / `dt = 0`
//! and assert byte-identical frame hashes; an arm that moved at `dt == 0`
//! would break that byte-stability. This is the same guard
//! `ScrollKinetics::tick` opens with, made a contract of the whole
//! algebra.
//!
//! ## Bounds — tier-honest: only-mitigated, NOT unrepresentable
//!
//! A duration is a [`Seconds`] and normalized progress / intensity a
//! [`Unit`], both `ishou_tokens::Refined`. Being precise (never round
//! up): this is a **runtime clamp**, not a compile-time refusal.
//! `Seconds::new(-5.0)` is a legal call that succeeds by clamping to 0,
//! and `Refined::default()` can bypass the bound. Per
//! `theory/UNREPRESENTABILITY.md` §III.3, f32-backed `Refined` is graded
//! **only-mitigated** — const generics preclude compile-time f32 bounds
//! today. So the honest claim is "an out-of-range value *saturates* at
//! the boundary and never flows into a curve as, say, `1.3`," NOT
//! "illegal motion is unrepresentable."
//!
//! The truly-unrep / parse-rejected upgrade (a galho-style
//! `validate`-trait `Refined<Duration>` routing deserialize through
//! `try_new`, rejecting rather than clamping) is available only for the
//! const-evaluable `std::time::Duration` case; it is named here as the
//! destination for the `Duration` bound, not claimed as shipped.

// Not every arm has a production consumer yet — bell-flash (Tween) is
// the first (see `crate::render`); the snow/glow (Decay) and cursor-
// blink (Oscillator) migrations are the named fast-follows. The arms
// are fully exercised by their `#[cfg(test)]` suites today; these allows
// keep the binary build clean (mado is a bin crate, so unconsumed pub
// re-exports read as unused) until those consumers land.
#![allow(dead_code, unused_imports)]

use ishou_tokens::{Bounds, Refined};

pub mod curve;
pub mod decay;
pub mod oscillator;
pub mod tween;

pub use curve::{Curve, EasingKind};
pub use decay::{frame_decay, Decay};
pub use oscillator::{blink_on, Oscillator};
pub use tween::Tween;

/// The one contract every CPU motion arm satisfies: a pure step of an
/// animated scalar by `dt` seconds.
///
/// The three methods form a tiny algebra: [`value`](Advance::value)
/// reads the current scalar without moving, [`advance`](Advance::advance)
/// steps and returns the new scalar, and
/// [`is_active`](Advance::is_active) reports whether motion is still
/// pending so the caller can skip finished animations entirely.
///
/// **Determinism:** `advance(dt)` is a strict no-op for `dt <= 0.0`.
pub trait Advance {
    /// Step forward by `dt` seconds and return the new scalar.
    /// A no-op (returns [`value`](Advance::value), mutates nothing)
    /// when `dt <= 0.0`.
    fn advance(&mut self, dt: f32) -> f32;

    /// The current scalar, without stepping.
    fn value(&self) -> f32;

    /// Whether the animation still has motion pending. A finished
    /// animation rests at its terminal value and reports `false`, so
    /// the render loop can drop it from the active set.
    fn is_active(&self) -> bool;
}

// ── Typed bounds — illegal motion is unrepresentable ────────────────

/// Bounds marker: a non-negative duration in seconds. `min = 0` makes a
/// negative duration unconstructible (`Refined::new` clamps it to 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonNegSecs;

impl Bounds<f32> for NonNegSecs {
    fn min() -> f32 {
        0.0
    }
    fn max() -> f32 {
        f32::MAX
    }
    fn default() -> f32 {
        0.0
    }
}

/// A duration in seconds, clamped `>= 0` at construction (only-mitigated
/// — the clamp is a runtime saturation, not a compile-time refusal; see
/// the module "Bounds" note).
pub type Seconds = Refined<f32, NonNegSecs>;

/// Bounds marker: the closed unit interval `[0, 1]`. Progress, alpha,
/// and normalized intensity all live here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitBounds;

impl Bounds<f32> for UnitBounds {
    fn min() -> f32 {
        0.0
    }
    fn max() -> f32 {
        1.0
    }
    fn default() -> f32 {
        0.0
    }
}

/// A scalar clamped to lie in `[0, 1]` — normalized progress or
/// intensity. An out-of-range value saturates at the boundary (a runtime
/// clamp, only-mitigated), so it can never flow into a curve as, say,
/// `1.3`.
pub type Unit = Refined<f32, UnitBounds>;

/// Convenience: build a [`Seconds`] from an `f32` (clamps `< 0` to 0).
#[must_use]
pub fn secs(v: f32) -> Seconds {
    Seconds::new(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_clamps_negative_to_zero() {
        // Only-mitigated, not unrepresentable: the call succeeds by
        // clamping, it does not refuse.
        assert_eq!(secs(-5.0).get(), 0.0, "a negative duration clamps to zero");
        assert_eq!(secs(0.25).get(), 0.25);
    }

    #[test]
    fn unit_saturates_outside_the_interval() {
        assert_eq!(Unit::new(1.3).get(), 1.0);
        assert_eq!(Unit::new(-0.2).get(), 0.0);
        assert_eq!(Unit::new(0.5).get(), 0.5);
    }
}
