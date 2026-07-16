//! `Curve` — a timing curve mado can evaluate.
//!
//! ishou-tokens ships the *vocabulary* of curves ([`ishou_tokens::motion::Cubic`],
//! [`ishou_tokens::motion::Easings`]) as CSS cubic-bézier tuples, but no
//! way to *evaluate* one. This module supplies that evaluator, so mado
//! reuses the fleet's curve values and adds only the missing sampler.
//!
//! ## The algorithm — the canonical `UnitBezier` solve
//!
//! A CSS cubic-bézier `Cubic(x1, y1, x2, y2)` is the Bézier through
//! `P0 = (0, 0)`, `P1 = (x1, y1)`, `P2 = (x2, y2)`, `P3 = (1, 1)`,
//! parameterized by `s ∈ [0, 1]`. Easing needs `y` as a function of the
//! *time* `x` — but `x(s)` is itself a cubic, so we must invert it. We
//! use the exact method every browser engine uses (WebKit's
//! `UnitBezier::solve`): **Newton–Raphson from `s = x`, with a
//! bisection fallback** when the derivative is too small to converge.
//! This is the best-fit canonical algorithm for the job — deterministic,
//! allocation-free, and correct to `1e-6`.
//!
//! `Curve::Linear` short-circuits to the exact identity (`t → t`) — the
//! degenerate bézier `(0,0,1,1)` evaluates to it, but naming it lets the
//! common case skip the solve entirely.

use ishou_tokens::motion::{Cubic, Easings, Motion};

/// Newton–Raphson convergence target (WebKit uses `1e-6`).
const EPSILON: f32 = 1e-6;
/// Newton iterations before falling back to bisection.
const NEWTON_ITERS: u32 = 8;
/// Bisection iterations (the guaranteed-convergence fallback).
const BISECTION_ITERS: u32 = 24;

/// A timing curve. `Linear` is the exact identity; `Bezier` wraps an
/// ishou cubic-bézier token evaluated with the `UnitBezier` solve.
#[derive(Debug, Clone, Copy)]
pub enum Curve {
    /// The identity `t → t`.
    Linear,
    /// A CSS cubic-bézier easing token from [`ishou_tokens::motion`].
    Bezier(Cubic),
}

// `ishou_tokens::motion::Cubic` is `Copy` but not `PartialEq`, so we
// hand-impl equality by its four control coordinates — letting `Curve`
// (and thus `Tween`) be compared in tests.
impl PartialEq for Curve {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Curve::Linear, Curve::Linear) => true,
            (Curve::Bezier(a), Curve::Bezier(b)) => {
                (a.0, a.1, a.2, a.3) == (b.0, b.1, b.2, b.3)
            }
            _ => false,
        }
    }
}

/// Selector over ishou's named easing curves — mado names the curve, the
/// *values* stay owned by `ishou_tokens::motion::Easings` (solve-once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EasingKind {
    /// Material "standard" — accelerate then decelerate.
    Standard,
    /// Decelerate — fast start, gentle stop (entering the screen).
    Decelerate,
    /// Accelerate — gentle start, fast exit (leaving the screen).
    Accelerate,
    /// "Sonic boom" — quick attack, long settle (matches the shader).
    SonicBoom,
    /// "Saber swoop" — curved in/out, steady middle (prompt-saber shader).
    Saber,
}

impl Curve {
    /// Build a curve from ishou's named easing set — the curve *values*
    /// come from `Easings`, mado only picks which one. Pass
    /// `&Easings::default()` for the fleet defaults.
    #[must_use]
    pub fn from_easing(easings: &Easings, kind: EasingKind) -> Self {
        let cubic = match kind {
            EasingKind::Standard => easings.standard,
            EasingKind::Decelerate => easings.decelerate,
            EasingKind::Accelerate => easings.accelerate,
            EasingKind::SonicBoom => easings.sonic_boom,
            EasingKind::Saber => easings.saber,
        };
        Curve::Bezier(cubic)
    }

    /// Build a fleet-default named curve. The default easing set is
    /// `ishou_tokens::motion::Motion::default().easing` (ishou hangs the
    /// defaults off `Motion`, not off `Easings` directly).
    #[must_use]
    pub fn named(kind: EasingKind) -> Self {
        Self::from_easing(&Motion::default().easing, kind)
    }

    /// Evaluate the curve at normalized time `t ∈ [0, 1]`, returning the
    /// eased value (also in `[0, 1]` for every well-formed easing token).
    /// `t` is clamped to `[0, 1]` first, so the endpoints are exact:
    /// `ease(0) == 0`, `ease(1) == 1`.
    #[must_use]
    pub fn ease(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Curve::Linear => t,
            Curve::Bezier(c) => UnitBezier::new(c).solve(t),
        }
    }
}

/// Precomputed polynomial coefficients for a `UnitBezier`, so the solve
/// evaluates `x(s)`, `y(s)`, `x'(s)` as plain Horner folds.
struct UnitBezier {
    ax: f32,
    bx: f32,
    cx: f32,
    ay: f32,
    by: f32,
    cy: f32,
}

impl UnitBezier {
    fn new(Cubic(x1, y1, x2, y2): Cubic) -> Self {
        // Polynomial form of a bézier with P0=(0,0), P3=(1,1):
        //   c = 3·P1 ; b = 3·(P2−P1) − c ; a = 1 − c − b
        let cx = 3.0 * x1;
        let bx = 3.0 * (x2 - x1) - cx;
        let ax = 1.0 - cx - bx;
        let cy = 3.0 * y1;
        let by = 3.0 * (y2 - y1) - cy;
        let ay = 1.0 - cy - by;
        Self { ax, bx, cx, ay, by, cy }
    }

    fn sample_x(&self, s: f32) -> f32 {
        ((self.ax * s + self.bx) * s + self.cx) * s
    }

    fn sample_y(&self, s: f32) -> f32 {
        ((self.ay * s + self.by) * s + self.cy) * s
    }

    fn sample_dx(&self, s: f32) -> f32 {
        (3.0 * self.ax * s + 2.0 * self.bx) * s + self.cx
    }

    /// Invert `x(s) = x` for `s`, then return `y(s)`.
    fn solve(&self, x: f32) -> f32 {
        self.sample_y(self.solve_x(x))
    }

    /// Solve `x(s) = x` for `s ∈ [0, 1]` — Newton–Raphson from `s = x`,
    /// bisection fallback. Mirrors WebKit's `UnitBezier::solveCurveX`.
    fn solve_x(&self, x: f32) -> f32 {
        // Fast path: Newton–Raphson seeded at s = x (a good guess for
        // near-linear curves).
        let mut s = x;
        for _ in 0..NEWTON_ITERS {
            let err = self.sample_x(s) - x;
            if err.abs() < EPSILON {
                return s;
            }
            let d = self.sample_dx(s);
            if d.abs() < EPSILON {
                break; // derivative too flat — hand off to bisection
            }
            s -= err / d;
        }

        // Bisection fallback — guaranteed to converge on the monotone
        // x(s) of any valid CSS easing (control-point x's in [0, 1]).
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        let mut s = x.clamp(lo, hi);
        for _ in 0..BISECTION_ITERS {
            let xs = self.sample_x(s);
            if (xs - x).abs() < EPSILON {
                return s;
            }
            if xs > x {
                hi = s;
            } else {
                lo = s;
            }
            s = (lo + hi) * 0.5;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn all_named() -> [EasingKind; 5] {
        [
            EasingKind::Standard,
            EasingKind::Decelerate,
            EasingKind::Accelerate,
            EasingKind::SonicBoom,
            EasingKind::Saber,
        ]
    }

    #[test]
    fn linear_is_the_exact_identity() {
        for t in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            assert_eq!(Curve::Linear.ease(t), t, "linear must be exact at {t}");
        }
    }

    #[test]
    fn endpoints_are_exact_for_every_named_curve() {
        for kind in all_named() {
            let c = Curve::named(kind);
            assert!(c.ease(0.0).abs() < 1e-4, "{kind:?}: ease(0) must be 0");
            assert!((c.ease(1.0) - 1.0).abs() < 1e-4, "{kind:?}: ease(1) must be 1");
        }
    }

    #[test]
    fn ease_clamps_out_of_range_time() {
        // Times outside [0,1] saturate to the endpoints, never NaN/blow up.
        let c = Curve::named(EasingKind::Standard);
        assert_eq!(c.ease(-2.0), c.ease(0.0));
        assert_eq!(c.ease(5.0), c.ease(1.0));
    }

    #[test]
    fn named_curves_read_their_values_from_ishou() {
        // Reuse proof: the SonicBoom curve mado evaluates is exactly the
        // one ishou declares (no duplicated tuple in mado).
        let ishou = Motion::default().easing;
        if let Curve::Bezier(c) = Curve::named(EasingKind::SonicBoom) {
            assert_eq!((c.0, c.1, c.2, c.3), (
                ishou.sonic_boom.0, ishou.sonic_boom.1, ishou.sonic_boom.2, ishou.sonic_boom.3,
            ));
        } else {
            panic!("named() must produce a Bezier");
        }
    }

    proptest! {
        /// Every named easing stays within [0,1] across the whole domain
        /// (its y control points are in [0,1], so the curve can't overshoot).
        #[test]
        fn named_curves_stay_in_unit_range(t in 0.0f32..=1.0) {
            for kind in all_named() {
                let y = Curve::named(kind).ease(t);
                prop_assert!((-1e-3..=1.0 + 1e-3).contains(&y),
                    "{kind:?}.ease({t}) = {y} escaped [0,1]");
            }
        }

        /// The Newton/bisection solve actually inverts x: for the standard
        /// curve, feeding a time back through the solve reconstructs x(s)
        /// to tolerance (proves the inversion converges, not just returns).
        #[test]
        fn standard_solve_inverts_x(t in 0.0f32..=1.0) {
            let ub = UnitBezier::new(Motion::default().easing.standard);
            let s = ub.solve_x(t);
            prop_assert!((ub.sample_x(s) - t).abs() < 1e-3,
                "solve_x({t}) = {s}, but x({s}) = {} != {t}", ub.sample_x(s));
        }
    }
}
