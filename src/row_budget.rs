//! `VisibleRows` — the typed, viewport-derived picker/overlay row budget.
//!
//! Screen-size-awareness sealed as a **type**: how many list rows a picker
//! shows can ONLY be produced from live viewport dimensions via
//! [`RowBudget::for_viewport`], so "a picker sized without the current screen"
//! is unrepresentable in a draw path. This replaces the former fixed
//! `session_picker::WINDOW_ROWS = 12` — the constant that made the Ctrl-S
//! board and the Ctrl-T dir picker show at most 12 rows on a 4K window that
//! could fit ~40 (screen-size-*unaware* by construction).
//!
//! The core mado-GUI invariant (per `docs/MACRO-VOCABULARY.md` and the org
//! responsive-by-default direction): every overlay list is sized against the
//! live surface and reflows as it resizes — the budget is resolved at the same
//! per-frame reconciler tick that already reconciles the terminal grid, so it
//! tracks resize with zero new event wiring.
//!
//! Built on the fleet `ishou_tokens::Refined<T, B: Bounds<T>>` primitive — the
//! same typed-bound shape as [`crate::font_size::BoundedFontSize`]; clamping /
//! saturation semantics come from ishou-tokens.

use ishou_tokens::{Bounds, Refined};

/// Floor — a picker always shows at least one row (the selected one), even on
/// a degenerately short viewport.
pub const ROWS_MIN: usize = 1;

/// Ceiling — no overlay list needs more than this many rows on screen at once;
/// also the value a huge/degenerate viewport saturates to.
pub const ROWS_MAX: usize = 200;

/// The historical fixed window height, kept as the [`Bounds::default`] and the
/// screen-less fallback (e.g. the session-picker reserved-band anchor, a
/// union-ordering policy that is not a viewport-fit concern).
pub const ROWS_DEFAULT: usize = 12;

/// Zero-sized marker for the picker row-budget bounds. The `Bounds<usize>`
/// impl makes the constants above visible to `Refined<usize, Self>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowBudgetBounds;

impl Bounds<usize> for RowBudgetBounds {
    fn min() -> usize {
        ROWS_MIN
    }
    fn max() -> usize {
        ROWS_MAX
    }
    fn default() -> usize {
        ROWS_DEFAULT
    }
}

/// A visible-row count proven (by type) to satisfy `ROWS_MIN <= n <= ROWS_MAX`.
/// The only in-draw-path constructor is [`RowBudget::for_viewport`].
pub type VisibleRows = Refined<usize, RowBudgetBounds>;

/// The viewport → row-budget resolver.
pub struct RowBudget;

impl RowBudget {
    /// The ONLY way to derive a [`VisibleRows`] from a live viewport — the
    /// SAME vertical-fit formula `render.rs::draw_overlay` clamps its own list
    /// to (`floor((height − 2·pad − 2·pad_y) / line_h)`), so a picker builds
    /// exactly the rows a tall screen affords and no more than a short one
    /// fits. Clamped by the type at both ends.
    ///
    /// There is deliberately no `VisibleRows::from(constant)` reachable from a
    /// draw path: screen-size-awareness is unrepresentable-to-omit.
    #[must_use]
    pub fn for_viewport(height_px: f32, line_h: f32, pad: f32, pad_y: f32) -> VisibleRows {
        let usable = (height_px - 2.0 * pad - 2.0 * pad_y).max(0.0);
        let rows = if line_h > 0.0 {
            // floor, ≥1 — matches draw_overlay's `max_lines`.
            (usable / line_h).floor().max(1.0) as usize
        } else {
            ROWS_MIN
        };
        VisibleRows::new(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_constants() {
        assert_eq!(<RowBudgetBounds as Bounds<usize>>::min(), 1);
        assert_eq!(<RowBudgetBounds as Bounds<usize>>::max(), 200);
        assert_eq!(<RowBudgetBounds as Bounds<usize>>::default(), 12);
    }

    #[test]
    fn budget_grows_with_height() {
        // line_h=20, pad=8, pad_y=10 → usable = h - 16 - 20 = h - 36.
        let short = RowBudget::for_viewport(120.0, 20.0, 8.0, 10.0).get(); // (84/20)=4
        let tall = RowBudget::for_viewport(2160.0, 20.0, 8.0, 10.0).get(); // (2124/20)=106
        assert_eq!(short, 4);
        assert_eq!(tall, 106);
        assert!(tall > short, "a taller viewport must afford more rows");
    }

    #[test]
    fn budget_is_refined_clamped_at_the_floor() {
        // A viewport with no usable height still yields the typed MIN (1),
        // never 0 — a picker always shows the selected row.
        assert_eq!(
            RowBudget::for_viewport(0.0, 20.0, 8.0, 10.0).get(),
            ROWS_MIN
        );
        assert_eq!(
            RowBudget::for_viewport(120.0, 0.0, 8.0, 10.0).get(),
            ROWS_MIN
        );
    }

    #[test]
    fn budget_saturates_at_the_ceiling() {
        // A pathologically tall viewport clamps to the typed MAX, not usize::MAX.
        assert_eq!(
            RowBudget::for_viewport(1_000_000.0, 1.0, 0.0, 0.0).get(),
            ROWS_MAX
        );
    }

    #[test]
    fn matches_draw_overlay_max_lines_formula() {
        // Byte-for-byte the render.rs:2274 `max_lines` computation for a
        // representative overlay metric set, so the picker's build-cap and the
        // renderer's window-cap can never diverge.
        let (height, line_h, pad) = (800.0_f32, 24.0_f32, 12.0_f32);
        let pad_y = line_h * 0.5;
        let expected =
            (((height - 2.0 * pad - 2.0 * pad_y) / line_h).floor() as i64).max(1) as usize;
        assert_eq!(
            RowBudget::for_viewport(height, line_h, pad, pad_y).get(),
            expected
        );
    }
}
