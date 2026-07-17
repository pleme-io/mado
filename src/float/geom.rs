//! Floating-surface geometry primitives — the tier-honest rect vocabulary the
//! snap ([`super::snap`]) and window-state ([`super::state`]) cores share.
//!
//! We build on [`egaku::Rect`] (the fleet widget-geometry type, already a mado
//! dependency) rather than mint a second `Rect` — "no second geometry type" is
//! the drift-prevention discipline (Op#1: reuse the near-miss primitive). The
//! helpers a floating/snapping window needs that `egaku::Rect` does not yet
//! carry — viewport fractions, quadrants, integer-pixel snapping, in-bounds
//! clamping — live here as the [`RectExt`] extension trait, so they compose
//! without forking `egaku`. (When the floating-window *chrome widget* lands, it
//! belongs in `egaku`/`ishou` per QUADRO T1 — these pure helpers are the
//! legitimate mado-local first-consumer, like `motion`/`scroll` were.)
//!
//! Every rect a surface holds also records HOW it got its geometry
//! ([`RectProvenance`]) — the same tier-honest provenance discipline as
//! [`crate::panel_fit::PanelRatio`]: a snapped rect, a freely-dragged rect, and
//! a config-declared rect are distinguishable, so "why is this window here?" is
//! never a guess.

use egaku::Rect;
use pleme_allvariants_derive::AllVariants;
use pleme_kindstr_derive::KindStr;

/// A screen edge a half / edge-fraction snap zone hugs.
///
/// The `#[kind(name = …)]` table is the one authored slug registry; `KindStr`
/// derives the [`Self::as_str`]/[`Self::from_str_kind`] round-trip (consumed by
/// the MCP `browser_snap` arg + the `(mado-browser-snap … :left)` lisp keyword),
/// `AllVariants` the always-complete [`Self::ALL`] reflection const.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, KindStr, AllVariants)]
pub enum Edge {
    #[kind(name = "left")]
    Left,
    #[kind(name = "right")]
    Right,
    #[kind(name = "top")]
    Top,
    #[kind(name = "bottom")]
    Bottom,
}

/// A viewport corner a quadrant snap zone occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, KindStr, AllVariants)]
pub enum Corner {
    #[kind(name = "top-left")]
    TopLeft,
    #[kind(name = "top-right")]
    TopRight,
    #[kind(name = "bottom-left")]
    BottomLeft,
    #[kind(name = "bottom-right")]
    BottomRight,
}

/// How a [`super::state::FloatingSurface`]'s rect got its geometry — the
/// tier-honest provenance discipline mirrored from [`crate::panel_fit::PanelRatio`].
///
/// A rect is never just coordinates: it remembers whether a human dragged it
/// there, a snap zone placed it, or a `(deffloatingbrowser …)` declaration
/// configured it — so a later drag can be distinguished from a snap re-flow and
/// the "restore" of a maximized window knows what to restore to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, KindStr, AllVariants)]
pub enum RectProvenance {
    #[kind(name = "free-dragged")]
    FreeDragged,
    #[kind(name = "snapped")]
    Snapped,
    #[kind(name = "configured")]
    Configured,
}

/// Floating-window geometry helpers layered onto [`egaku::Rect`].
///
/// Every method is a pure total function of `f32` geometry — no I/O, no clock,
/// no GPU. Fractions are clamped to `0.0..=1.0`; the integer-pixel rounding is
/// the discipline that keeps a snapped surface off the fractional-scale seam
/// that bit the scaled-display probe (see [`crate::panel_fit`]).
pub trait RectExt: Sized + Copy {
    /// The right edge (`x + width`).
    fn right(self) -> f32;
    /// The bottom edge (`y + height`).
    fn bottom(self) -> f32;
    /// `width * height`.
    fn area(self) -> f32;
    /// The left slice covering fraction `f` of the width (full height).
    fn left_fraction(self, f: f32) -> Self;
    /// The right slice covering fraction `f` of the width (full height).
    fn right_fraction(self, f: f32) -> Self;
    /// The top slice covering fraction `f` of the height (full width).
    fn top_fraction(self, f: f32) -> Self;
    /// The bottom slice covering fraction `f` of the height (full width).
    fn bottom_fraction(self, f: f32) -> Self;
    /// The `edge`-hugging slice covering fraction `f` of that edge's dimension.
    fn edge_fraction(self, edge: Edge, f: f32) -> Self;
    /// The half-width, half-height sub-rect anchored at `corner`.
    fn quadrant(self, corner: Corner) -> Self;
    /// Round `x`/`y`/`width`/`height` to whole pixels (the snap discipline).
    fn round_to_int_px(self) -> Self;
    /// Shrink + reposition so the rect lies fully inside `bounds` (size first).
    fn clamp_within(self, bounds: Self) -> Self;
}

impl RectExt for Rect {
    #[inline]
    fn right(self) -> f32 {
        self.x + self.width
    }

    #[inline]
    fn bottom(self) -> f32 {
        self.y + self.height
    }

    #[inline]
    fn area(self) -> f32 {
        self.width * self.height
    }

    fn left_fraction(self, f: f32) -> Self {
        let f = f.clamp(0.0, 1.0);
        Rect::new(self.x, self.y, self.width * f, self.height)
    }

    fn right_fraction(self, f: f32) -> Self {
        let f = f.clamp(0.0, 1.0);
        let w = self.width * f;
        Rect::new(self.right() - w, self.y, w, self.height)
    }

    fn top_fraction(self, f: f32) -> Self {
        let f = f.clamp(0.0, 1.0);
        Rect::new(self.x, self.y, self.width, self.height * f)
    }

    fn bottom_fraction(self, f: f32) -> Self {
        let f = f.clamp(0.0, 1.0);
        let h = self.height * f;
        Rect::new(self.x, self.bottom() - h, self.width, h)
    }

    fn edge_fraction(self, edge: Edge, f: f32) -> Self {
        match edge {
            Edge::Left => self.left_fraction(f),
            Edge::Right => self.right_fraction(f),
            Edge::Top => self.top_fraction(f),
            Edge::Bottom => self.bottom_fraction(f),
        }
    }

    fn quadrant(self, corner: Corner) -> Self {
        let hw = self.width * 0.5;
        let hh = self.height * 0.5;
        match corner {
            Corner::TopLeft => Rect::new(self.x, self.y, hw, hh),
            Corner::TopRight => Rect::new(self.x + hw, self.y, hw, hh),
            Corner::BottomLeft => Rect::new(self.x, self.y + hh, hw, hh),
            Corner::BottomRight => Rect::new(self.x + hw, self.y + hh, hw, hh),
        }
    }

    fn round_to_int_px(self) -> Self {
        Rect::new(
            self.x.round(),
            self.y.round(),
            self.width.round(),
            self.height.round(),
        )
    }

    fn clamp_within(self, bounds: Self) -> Self {
        // Size can never exceed the bounds.
        let width = self.width.min(bounds.width).max(0.0);
        let height = self.height.min(bounds.height).max(0.0);
        // Position keeps the (possibly shrunk) rect fully inside bounds.
        let max_x = bounds.right() - width;
        let max_y = bounds.bottom() - height;
        let x = self.x.clamp(bounds.x, max_x.max(bounds.x));
        let y = self.y.clamp(bounds.y, max_y.max(bounds.y));
        Rect::new(x, y, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn vp() -> Rect {
        Rect::new(0.0, 0.0, 1000.0, 800.0)
    }

    #[test]
    fn edge_and_corner_slugs_round_trip() {
        for e in Edge::ALL {
            assert_eq!(Edge::from_str_kind(e.as_str()), Some(*e));
        }
        for c in Corner::ALL {
            assert_eq!(Corner::from_str_kind(c.as_str()), Some(*c));
        }
        for p in RectProvenance::ALL {
            assert_eq!(RectProvenance::from_str_kind(p.as_str()), Some(*p));
        }
        // The slug table is the MCP/lisp contract — pin the exact spellings.
        assert_eq!(Corner::TopLeft.as_str(), "top-left");
        assert_eq!(Edge::Bottom.as_str(), "bottom");
        assert_eq!(RectProvenance::FreeDragged.as_str(), "free-dragged");
    }

    #[test]
    fn no_slug_collisions() {
        let mut slugs: Vec<&str> = Edge::ALL
            .iter()
            .map(|e| e.as_str())
            .chain(Corner::ALL.iter().map(|c| c.as_str()))
            .collect();
        let n = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), n, "slug collision across Edge+Corner");
    }

    #[test]
    fn left_and_right_halves_partition_the_viewport() {
        let v = vp();
        let l = v.left_fraction(0.5);
        let r = v.right_fraction(0.5);
        assert_eq!(l, Rect::new(0.0, 0.0, 500.0, 800.0));
        assert_eq!(r, Rect::new(500.0, 0.0, 500.0, 800.0));
        assert!((l.area() + r.area() - v.area()).abs() < 1.0);
    }

    #[test]
    fn quadrants_tile_the_viewport() {
        let v = vp();
        let total: f32 = Corner::ALL.iter().map(|c| v.quadrant(*c).area()).sum();
        assert!((total - v.area()).abs() < 1.0, "4 quadrants must tile the vp");
        assert_eq!(v.quadrant(Corner::TopRight), Rect::new(500.0, 0.0, 500.0, 400.0));
        assert_eq!(
            v.quadrant(Corner::BottomRight),
            Rect::new(500.0, 400.0, 500.0, 400.0),
        );
    }

    proptest! {
        #[test]
        fn round_to_int_px_is_idempotent(
            x in -5000.0f32..5000.0, y in -5000.0f32..5000.0,
            w in 0.0f32..5000.0, h in 0.0f32..5000.0,
        ) {
            let r = Rect::new(x, y, w, h).round_to_int_px();
            prop_assert_eq!(r, r.round_to_int_px());
        }

        #[test]
        fn clamp_within_is_always_inside_bounds(
            x in -2000.0f32..3000.0, y in -2000.0f32..3000.0,
            w in 0.0f32..3000.0, h in 0.0f32..3000.0,
        ) {
            let bounds = Rect::new(0.0, 0.0, 1000.0, 800.0);
            let c = Rect::new(x, y, w, h).clamp_within(bounds);
            prop_assert!(c.x >= bounds.x - 0.01);
            prop_assert!(c.y >= bounds.y - 0.01);
            prop_assert!(c.right() <= bounds.right() + 0.01);
            prop_assert!(c.bottom() <= bounds.bottom() + 0.01);
        }

        #[test]
        fn fractions_are_clamped_and_stay_within(
            f in -1.0f32..2.0,
        ) {
            let v = vp();
            for e in Edge::ALL {
                let s = v.edge_fraction(*e, f);
                prop_assert!(s.width <= v.width + 0.01 && s.width >= -0.01);
                prop_assert!(s.height <= v.height + 0.01 && s.height >= -0.01);
            }
        }
    }
}
