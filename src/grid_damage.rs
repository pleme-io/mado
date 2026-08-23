//! M2 stage 2 — typed damage-tracking primitives for the M7
//! threading decouple.
//!
//! Contract: `docs/GRID-THREADING-CONTRACT.md`. These types are
//! SHAPED now — co-designed with the `Line`/`LogicalLineId` grid in
//! the single M2 grid touch — and CONSUMED at M7 (bounded parse
//! mailbox + coalesce-redraw). They are deliberately not wired into
//! the render path yet: the render-thread decouple fights madori
//! `RenderCallback` ownership and is explicitly deferred behind the
//! named M7 milestone (REMEDIATION-PLAN.md §M7), so wiring damage
//! into today's whole-frame renderer would be a second grid-adjacent
//! touch with no consumer.
//!
//! The one rule the M7 implementer inherits: damage is tracked
//! per-PHYSICAL-row (viewport coordinates), `union`ed across parser
//! batches, and drained exactly once per frame by the renderer via
//! [`DirtyRegion::spans`].

#![allow(dead_code)] // Consumed at M7 — see module docs.
//!
//! ★ **UPDATE 2026-08-21 — "whether to draw" is now solved WITHOUT this, and
//! that changes what M7 is for.**
//!
//! `RenderCallback::needs_frame` (madori 0.1.14) answers *is a frame needed at
//! all*, before the swapchain acquire, and mado implements it. That was the
//! expensive half: the idle repaint cost **50.7% of a core on a static
//! screen** and is now gone. So M7 is no longer "make idle cheap" — it is the
//! remaining, narrower question: **when a frame IS needed, say WHAT changed.**
//!
//! ★ CORRECTED, SAME DAY. I first wrote that per-row spans here would cut the
//! compositor's per-keystroke copy. **They will not, and the reason is worth
//! recording so nobody re-plans around it.** mado never calls
//! `wl_surface.damage_buffer` — presentation goes through wgpu, and
//! `SurfaceTexture::present()` takes no damage argument at all (wgpu exposes
//! no `VK_KHR_incremental_present` equivalent). So the whole surface is
//! reported dirty on every present no matter what this module computes, and
//! omoya copies ~7.9 MB per keystroke either way.
//!
//! Measured on plo: a pointer-caused compositor frame costs **250 us**, a
//! commit-caused one **4194 us** — 17x, and past the 2.78 ms vblank at
//! 360 Hz. That gap is the compositor's import, and it is not mado's to close
//! from here.
//!
//! What M7 *does* buy is mado's OWN render cost: `last_frame_us` is ~440 us
//! today and most of it is `snapshot()`'s full grid clone. Real, worth having,
//! and roughly a tenth of the number people would expect from the paragraph
//! above.
//!
//! The compositor-side fix is **direct scanout** (`pending-omoya-planes`):
//! hand a fullscreen client's buffer straight to a DRM plane and copy nothing.
//! That is omoya's work, not this module's.
//!
//! Note the division of labour, because conflating the two is exactly the
//! mistake that produced the idle repaint: **`needs_frame` decides IF, this
//! module describes WHAT.** They are not alternatives and neither subsumes the
//! other.

use std::ops::Range;

/// Per-row dirty bitset over a fixed viewport row count. One bit per
/// visible row; 64 rows per word. All operations are O(rows/64)
/// except [`Self::spans`], which is O(rows) in the dirty-row count.
///
/// Out-of-range rows are ignored on mark (the parser may race a
/// shrink by one batch under the M7 mailbox — a stale row index must
/// not panic the parse thread; the next full-damage resize redraw
/// covers it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyRegion {
    /// Bit `r % 64` of `bits[r / 64]` = row `r` is dirty.
    bits: Vec<u64>,
    /// Number of addressable rows (viewport height).
    rows: usize,
}

impl DirtyRegion {
    /// All-clean region addressing `rows` viewport rows.
    #[must_use]
    pub fn new(rows: usize) -> Self {
        Self {
            bits: vec![0; rows.div_ceil(64)],
            rows,
        }
    }

    /// Number of addressable rows.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Mark one row dirty. Out-of-range is a no-op (see type docs).
    pub fn mark(&mut self, row: usize) {
        if row < self.rows {
            self.bits[row / 64] |= 1 << (row % 64);
        }
    }

    /// Mark every row in `range` dirty (end-exclusive, clamped).
    pub fn mark_range(&mut self, range: Range<usize>) {
        let end = range.end.min(self.rows);
        for row in range.start..end {
            self.bits[row / 64] |= 1 << (row % 64);
        }
    }

    /// Mark every addressable row dirty — the resize / full-redraw
    /// escape hatch.
    pub fn mark_all(&mut self) {
        self.mark_range(0..self.rows);
    }

    /// Clear one row. Out-of-range is a no-op.
    pub fn clear_row(&mut self, row: usize) {
        if row < self.rows {
            self.bits[row / 64] &= !(1 << (row % 64));
        }
    }

    /// Clear every row — the renderer calls this after draining a
    /// frame's damage.
    pub fn clear(&mut self) {
        self.bits.fill(0);
    }

    /// Is `row` dirty? Out-of-range rows are clean by definition.
    #[must_use]
    pub fn is_dirty(&self, row: usize) -> bool {
        row < self.rows && (self.bits[row / 64] >> (row % 64)) & 1 == 1
    }

    /// Any dirty row at all?
    #[must_use]
    pub fn any(&self) -> bool {
        self.bits.iter().any(|w| *w != 0)
    }

    /// Count of dirty rows.
    #[must_use]
    pub fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Coalesce damage from another region into this one (bitwise
    /// OR) — how per-parse-batch damage accumulates into the frame's
    /// damage under the M7 mailbox. Row counts must match; a
    /// mismatched union marks everything (the safe overapproximation
    /// during a resize race).
    pub fn union(&mut self, other: &Self) {
        if self.rows == other.rows {
            for (w, o) in self.bits.iter_mut().zip(&other.bits) {
                *w |= o;
            }
        } else {
            self.mark_all();
        }
    }

    /// Maximal contiguous dirty row spans, in ascending order —
    /// the renderer's per-region redraw unit. Coalescing adjacent
    /// dirty rows into one span is what turns an N-row flood into
    /// one draw call.
    #[must_use]
    pub fn spans(&self) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut start: Option<usize> = None;
        for row in 0..self.rows {
            match (self.is_dirty(row), start) {
                (true, None) => start = Some(row),
                (false, Some(s)) => {
                    out.push(s..row);
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            out.push(s..self.rows);
        }
        out
    }

    /// Re-shape to a new viewport height. Damage does not survive a
    /// resize — the whole new viewport is dirty by definition.
    pub fn resize(&mut self, rows: usize) {
        *self = Self::new(rows);
        self.mark_all();
    }
}

/// One frame's worth of grid damage — what the M7 parse thread hands
/// the render thread per redraw request. Typed sketch per
/// `docs/GRID-THREADING-CONTRACT.md`; the `Scrolled` arm exists so a
/// full-screen scroll (the dominant streaming case) can be expressed
/// as blit-up + one dirty bottom row instead of N dirty rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridDamage {
    /// Everything changed (resize, palette swap, alt-screen flip).
    Full,
    /// Exactly these viewport rows changed.
    Rows(DirtyRegion),
    /// The viewport scrolled `lines` rows upward within `region`
    /// (top..end, end-exclusive) and `dirty` rows changed on top of
    /// the blit.
    Scrolled {
        region: Range<usize>,
        lines: usize,
        dirty: DirtyRegion,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_region_is_clean() {
        let d = DirtyRegion::new(100);
        assert!(!d.any());
        assert_eq!(d.count(), 0);
        assert!(d.spans().is_empty());
        for row in 0..100 {
            assert!(!d.is_dirty(row));
        }
    }

    #[test]
    fn mark_and_clear_single_rows_across_word_boundaries() {
        // 130 rows = 3 words; exercise bits 0, 63, 64, 127, 129 —
        // both edges of every u64 word.
        let mut d = DirtyRegion::new(130);
        for row in [0, 63, 64, 127, 129] {
            d.mark(row);
            assert!(d.is_dirty(row), "row {row} should be dirty");
        }
        assert_eq!(d.count(), 5);

        d.clear_row(64);
        assert!(!d.is_dirty(64));
        assert!(d.is_dirty(63), "clearing 64 must not touch 63");
        assert!(d.is_dirty(127));
        assert_eq!(d.count(), 4);

        d.clear();
        assert!(!d.any());
    }

    #[test]
    fn out_of_range_mark_is_ignored_not_panicking() {
        let mut d = DirtyRegion::new(24);
        d.mark(24); // one past the end
        d.mark(usize::MAX);
        d.clear_row(500);
        assert!(!d.any());
        assert!(!d.is_dirty(9999));
    }

    #[test]
    fn mark_range_clamps_and_mark_all_covers() {
        let mut d = DirtyRegion::new(70);
        d.mark_range(60..200); // end clamps to 70
        assert_eq!(d.count(), 10);
        assert!(d.is_dirty(60));
        assert!(d.is_dirty(69));
        assert!(!d.is_dirty(59));

        d.mark_all();
        assert_eq!(d.count(), 70);
    }

    #[test]
    fn spans_coalesce_adjacent_dirty_rows() {
        let mut d = DirtyRegion::new(24);
        // Two runs + one singleton: [2..5), [7..8), [20..24).
        d.mark_range(2..5);
        d.mark(7);
        d.mark_range(20..24);
        assert_eq!(d.spans(), vec![2..5, 7..8, 20..24]);

        // Filling the gap merges the first two spans.
        d.mark_range(5..7);
        assert_eq!(d.spans(), vec![2..8, 20..24]);
    }

    #[test]
    fn union_accumulates_batch_damage() {
        let mut frame = DirtyRegion::new(24);
        let mut batch_a = DirtyRegion::new(24);
        let mut batch_b = DirtyRegion::new(24);
        batch_a.mark_range(0..3);
        batch_b.mark_range(2..6);
        frame.union(&batch_a);
        frame.union(&batch_b);
        assert_eq!(frame.spans(), vec![0..6]);
        assert_eq!(frame.count(), 6);
    }

    #[test]
    fn union_with_mismatched_rows_overapproximates_to_full() {
        let mut frame = DirtyRegion::new(24);
        let other = DirtyRegion::new(30); // resize raced the batch
        frame.union(&other);
        assert_eq!(frame.count(), 24, "safe overapproximation = all dirty");
    }

    #[test]
    fn resize_marks_whole_new_viewport() {
        let mut d = DirtyRegion::new(24);
        d.mark(3);
        d.resize(40);
        assert_eq!(d.rows(), 40);
        assert_eq!(d.count(), 40);
    }
}
