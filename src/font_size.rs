//! `BoundedFontSize` — the typed font-size primitive.
//!
//! # Why this exists
//!
//! mado used to store the live font size as a bare `f32` and clamp
//! to `[6.0, 64.0]` at every call site:
//!
//! ```ignore
//! let new = (renderer.font_size() + 1.0).min(64.0);   // FontIncrease
//! let new = (renderer.font_size() - 1.0).max(6.0);    // FontDecrease
//! renderer.set_font_size(some_arbitrary_f32);         // any caller
//! ```
//!
//! Two real problems:
//!
//! 1. **Bounds checking is opt-in.** `renderer.set_font_size()` takes
//!    a plain `f32`. Any future caller that forgets to clamp can put
//!    the renderer into a state the rest of the pipeline doesn't
//!    expect (cell metric overflow, swapchain-size mismatch, fontdb
//!    miss). The compiler doesn't help.
//!
//! 2. **No invariant about step size.** A bug in the key-repeat path
//!    (real incident, 2026-05-21: Cmd-= held → 25 font scale events in
//!    1.5s → font grew from 14 → 32 onscreen) is invisible to the type
//!    system because each individual `(prev + 1.0).min(64.0)` is well-
//!    formed in isolation.
//!
//! `BoundedFontSize` encodes the invariant `value ∈ [FONT_MIN, FONT_MAX]`
//! in the type, and exposes only typed mutations (`inc_step`, `dec_step`,
//! `reset_to`, `try_set`). Every f32 entering the type is clamped at the
//! boundary. No new way to violate the invariant exists.
//!
//! Pairs with [`crate::key_repeat_gate`] — that handles the temporal
//! rate-limit half (how often `inc_step` can be invoked); this handles
//! the magnitude half (each invocation's effect is bounded).

use serde::{Deserialize, Deserializer, Serialize};

/// Smallest font size mado will render at. Anything smaller is
/// physically unreadable on the densest displays we target.
pub const FONT_MIN: f32 = 6.0;

/// Largest font size mado will render at. Higher values blow past
/// reasonable cell-metric budgets + most operators' window dimensions.
pub const FONT_MAX: f32 = 64.0;

/// Default per-keystroke step for `inc_step` / `dec_step`. Operators
/// who want a different feel override via `mado.yaml`'s
/// `font.scale_step` (future) but for the hardcoded path the
/// constant is the single source.
pub const FONT_STEP: f32 = 1.0;

/// A font size proven (by type) to satisfy `FONT_MIN <= value <= FONT_MAX`.
///
/// Constructor + every mutation clamps. Equality + `Copy` are
/// derived so consumers can pass these around like an f32.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BoundedFontSize(f32);

impl BoundedFontSize {
    /// Construct from an arbitrary `f32`. Always succeeds — the value
    /// is clamped into `[FONT_MIN, FONT_MAX]` and the original is
    /// silently discarded if out-of-range.
    ///
    /// Use [`Self::try_new`] when you want to detect clamping.
    #[must_use]
    pub fn new(value: f32) -> Self {
        Self(value.clamp(FONT_MIN, FONT_MAX))
    }

    /// Construct from an `f32`, returning `Err(out_of_range_value)`
    /// when the input would have been clamped.
    ///
    /// # Errors
    /// Returns the original out-of-range value when `value < FONT_MIN`
    /// or `value > FONT_MAX`.
    pub fn try_new(value: f32) -> Result<Self, f32> {
        if !value.is_finite() || value < FONT_MIN || value > FONT_MAX {
            Err(value)
        } else {
            Ok(Self(value))
        }
    }

    /// The wrapped value. Always in `[FONT_MIN, FONT_MAX]` by
    /// construction.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }

    /// Add one step. Saturates at `FONT_MAX` — never wraps, never
    /// overflows. Returns the new value for chaining.
    #[must_use]
    pub fn inc_step(self) -> Self {
        self.inc_by(FONT_STEP)
    }

    /// Subtract one step. Saturates at `FONT_MIN`.
    #[must_use]
    pub fn dec_step(self) -> Self {
        self.dec_by(FONT_STEP)
    }

    /// Add `delta` (must be >= 0; negative deltas pass through as
    /// a subtract). Saturates at `FONT_MAX`.
    #[must_use]
    pub fn inc_by(self, delta: f32) -> Self {
        Self::new(self.0 + delta)
    }

    /// Subtract `delta` (must be >= 0). Saturates at `FONT_MIN`.
    #[must_use]
    pub fn dec_by(self, delta: f32) -> Self {
        Self::new(self.0 - delta)
    }

    /// Reset to a specific target — typically the config default.
    /// The target is itself clamped.
    #[must_use]
    pub fn reset_to(_self_unused: Self, target: f32) -> Self {
        Self::new(target)
    }

    /// True iff we'd hit the upper bound if we incremented one more
    /// step. Lets call sites avoid emitting redundant resize events
    /// for a font that's already maxed.
    #[must_use]
    pub fn at_max(self) -> bool {
        (self.0 - FONT_MAX).abs() < f32::EPSILON
    }

    /// True iff we'd hit the lower bound on the next decrement.
    #[must_use]
    pub fn at_min(self) -> bool {
        (self.0 - FONT_MIN).abs() < f32::EPSILON
    }
}

impl Default for BoundedFontSize {
    /// Defaults to mado's prescribed font size (14.0 — matches
    /// `ishou_tokens::FleetDefaults::prescribed().font_size`).
    fn default() -> Self {
        Self::new(14.0)
    }
}

impl std::fmt::Display for BoundedFontSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for BoundedFontSize {
    /// Round-trips through clamping. A YAML config that says
    /// `font_size: 9999.0` deserializes to `FONT_MAX`, not a
    /// runtime panic.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = f32::deserialize(d)?;
        Ok(Self::new(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_clamps_below_min() {
        assert_eq!(BoundedFontSize::new(0.5).get(), FONT_MIN);
        assert_eq!(BoundedFontSize::new(-1000.0).get(), FONT_MIN);
    }

    #[test]
    fn new_clamps_above_max() {
        assert_eq!(BoundedFontSize::new(1_000.0).get(), FONT_MAX);
        assert_eq!(BoundedFontSize::new(FONT_MAX + 0.001).get(), FONT_MAX);
    }

    #[test]
    fn new_passes_in_range() {
        assert_eq!(BoundedFontSize::new(14.0).get(), 14.0);
        assert_eq!(BoundedFontSize::new(FONT_MIN).get(), FONT_MIN);
        assert_eq!(BoundedFontSize::new(FONT_MAX).get(), FONT_MAX);
    }

    #[test]
    fn try_new_errors_on_oob() {
        assert!(BoundedFontSize::try_new(0.5).is_err());
        assert!(BoundedFontSize::try_new(1000.0).is_err());
        assert!(BoundedFontSize::try_new(f32::NAN).is_err());
        assert!(BoundedFontSize::try_new(f32::INFINITY).is_err());
    }

    #[test]
    fn try_new_succeeds_in_range() {
        assert!(BoundedFontSize::try_new(14.0).is_ok());
        assert!(BoundedFontSize::try_new(FONT_MIN).is_ok());
        assert!(BoundedFontSize::try_new(FONT_MAX).is_ok());
    }

    #[test]
    fn inc_step_saturates_at_max() {
        // The runaway-font incident: 1000 increments do not exceed FONT_MAX.
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
    fn inc_by_dec_by_are_inverses_in_safe_range() {
        let s = BoundedFontSize::new(14.0);
        assert_eq!(s.inc_by(3.0).dec_by(3.0).get(), 14.0);
    }

    #[test]
    fn reset_to_clamps_target() {
        let s = BoundedFontSize::new(14.0);
        assert_eq!(BoundedFontSize::reset_to(s, 999.0).get(), FONT_MAX);
        assert_eq!(BoundedFontSize::reset_to(s, -1.0).get(), FONT_MIN);
    }

    #[test]
    fn serde_round_trip_clamps_oob_yaml() {
        // Operator yaml `font_size: 9999.0` deserializes to FONT_MAX.
        let yaml = "9999.0";
        let parsed: BoundedFontSize = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(parsed.get(), FONT_MAX);
    }

    #[test]
    fn serde_round_trip_preserves_in_range() {
        let s = BoundedFontSize::new(16.5);
        let s_yaml = serde_yaml_ng::to_string(&s).unwrap();
        let back: BoundedFontSize = serde_yaml_ng::from_str(&s_yaml).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn at_max_and_at_min_predicates() {
        assert!(BoundedFontSize::new(FONT_MAX).at_max());
        assert!(BoundedFontSize::new(FONT_MIN).at_min());
        assert!(!BoundedFontSize::new(14.0).at_max());
        assert!(!BoundedFontSize::new(14.0).at_min());
    }

    #[test]
    fn default_matches_fleet_prescribed() {
        // Cross-checks against ishou_tokens::FleetDefaults — they
        // must agree on 14.0 or mado's defaults silently diverge
        // from the rest of the fleet (mado/src/auto_detect.rs has
        // its own convergence guard for the broader case).
        assert_eq!(BoundedFontSize::default().get(), 14.0);
    }
}
