//! `BoundedFontSize` — typed font size for mado.
//!
//! Thin newtype over `ishou_tokens::Refined<f32, FontSizeBounds>`
//! (the fleet-wide typed-bound primitive). The bound + step + step
//! mutations live here; the clamping/saturation/serde semantics
//! come from ishou-tokens.
//!
//! # Why ishou-tokens
//!
//! The "value with statically-known bounds" pattern is shared across
//! every GUI app in the fleet (mado, namimado, escriba, hibiki, …).
//! Pillar 12: extract to ishou-tokens, every consumer pulls in one
//! line. See `ishou_tokens::refined` for the full design.
//!
//! # Historical
//!
//! Before ishou@fb774b6 this file owned a hand-rolled f32 newtype
//! (~270 LOC + 14 tests) — extracted from the 2026-05-21 runaway-
//! font incident. Collapsed when `Refined<T, B>` shipped fleet-wide.

use ishou_tokens::{Bounds, Refined};

/// Smallest font size mado will render at. Unreadable below this
/// on the densest displays mado targets.
pub const FONT_MIN: f32 = 6.0;

/// Largest font size mado will render at. Above this, cell metrics
/// blow past reasonable budgets + most operators' window dimensions.
pub const FONT_MAX: f32 = 64.0;

/// Default per-keystroke step for `inc_step` / `dec_step`.
pub const FONT_STEP: f32 = 1.0;

/// Default starting font size — matches `FleetDefaults::prescribed().font_size`.
pub const FONT_DEFAULT: f32 = 14.0;

/// Zero-sized marker for mado's font-size bounds. The `Bounds<f32>`
/// impl makes the constants above visible to `Refined<f32, Self>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontSizeBounds;

impl Bounds<f32> for FontSizeBounds {
    fn min() -> f32 { FONT_MIN }
    fn max() -> f32 { FONT_MAX }
    fn default() -> f32 { FONT_DEFAULT }
}

/// A font size proven (by type) to satisfy `FONT_MIN <= v <= FONT_MAX`.
/// Constructor + every mutation clamps. `Copy + Clone + PartialEq +
/// Debug + Display` all derive through the underlying `Refined`.
pub type BoundedFontSize = Refined<f32, FontSizeBounds>;

// ── Step-mutations (mado-specific layer on top of generic Refined) ──
//
// Refined<T, B> handles `inc_by` / `dec_by` with arbitrary deltas;
// the mado dispatch layer wants "one step" — these helpers fold the
// FONT_STEP constant in. Trait-extension style: extra methods on
// the typealias.

/// Step helpers for `BoundedFontSize` — one-keystroke font scaling.
pub trait FontSizeSteps {
    /// Add one `FONT_STEP`. Saturates at `FONT_MAX`.
    fn inc_step(self) -> Self;
    /// Subtract one `FONT_STEP`. Saturates at `FONT_MIN`.
    fn dec_step(self) -> Self;
}

impl FontSizeSteps for BoundedFontSize {
    fn inc_step(self) -> Self {
        self.inc_by(FONT_STEP)
    }
    fn dec_step(self) -> Self {
        self.dec_by(FONT_STEP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exhaustive Refined<T, B> contract (clamping, saturation,
    // serde round-trip, Copy/Clone/Debug/Display, cross-app type
    // sharing) is pinned by ishou_tokens::refined::tests. These
    // tests only cover the mado-specific delta: the FontSizeBounds
    // values + the FontSizeSteps trait.

    #[test]
    fn bounds_constants_match_fleet() {
        // FONT_MAX must stay at 64.0 — every renderer cell-metric
        // budget assumes this. Lowering breaks layout; raising
        // means widening cell_height limits.
        assert_eq!(FONT_MIN, 6.0);
        assert_eq!(FONT_MAX, 64.0);
        assert_eq!(FONT_DEFAULT, 14.0);
        assert_eq!(FONT_STEP, 1.0);
    }

    #[test]
    fn bounds_trait_impl_returns_constants() {
        assert_eq!(<FontSizeBounds as Bounds<f32>>::min(), FONT_MIN);
        assert_eq!(<FontSizeBounds as Bounds<f32>>::max(), FONT_MAX);
        assert_eq!(<FontSizeBounds as Bounds<f32>>::default(), FONT_DEFAULT);
    }

    #[test]
    fn default_matches_fleet_font_default() {
        // The default font size mado picks (BoundedFontSize::default())
        // MUST match ishou_tokens::FleetDefaults::prescribed().font_size.
        // Both are 14.0 today; if either drifts, this test fires.
        let bfs = BoundedFontSize::default();
        let fd = ishou_tokens::FleetDefaults::prescribed();
        assert!(
            (bfs.get() - fd.font_size).abs() < 0.001,
            "BoundedFontSize::default() ({}) drifted from FleetDefaults::prescribed().font_size ({})",
            bfs.get(), fd.font_size
        );
    }

    #[test]
    fn inc_step_saturates_at_max() {
        // The 2026-05-21 runaway-font scenario: 1000 increments
        // cannot exceed FONT_MAX.
        let mut s = BoundedFontSize::new(14.0);
        for _ in 0..1000 {
            s = s.inc_step();
        }
        assert_eq!(s.get(), FONT_MAX);
        assert!(s.at_max());
    }

    #[test]
    fn dec_step_saturates_at_min() {
        let mut s = BoundedFontSize::new(14.0);
        for _ in 0..1000 {
            s = s.dec_step();
        }
        assert_eq!(s.get(), FONT_MIN);
        assert!(s.at_min());
    }

    #[test]
    fn inc_dec_step_are_inverses_in_safe_range() {
        let s = BoundedFontSize::new(14.0);
        assert_eq!(s.inc_step().inc_step().dec_step().dec_step().get(), 14.0);
    }
}
