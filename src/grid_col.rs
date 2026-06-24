//! `grid_col` — THE single source of column truth for a dense terminal
//! row, shared by every pipeline that must agree on "which grid column
//! is this cell at": the renderer's text + rect/decoration passes, the
//! cursor block, and URL/decoration detection.
//!
//! A terminal row is *dense*: `Grid` guarantees exactly one [`Cell`] per
//! column, so a cell's column IS its index in the row. A wide char
//! (CJK / emoji, `width == 2`) occupies a lead cell plus a `width == 0`
//! **continuation** cell that the lead's glyph spans; the continuation
//! owns no column of its own.
//!
//! Two historical bugs lived in this exact gap. (1) The text pipeline
//! derived its column by *summing* `cell.width` (`col += cell.width`)
//! while ALSO visiting the `width == 0` continuation cell — double
//! counting it (+1 per wide char), so text drifted one column right of
//! the cursor block per wide char (`m▮in 📦 ❄` instead of `…❄ ▮`).
//! (2) The URL underline aliased linkify's *byte* offsets as columns, so
//! a link preceded by any multi-byte glyph painted shifted-right of the
//! URL ("everything is underlined"). Both are the same class: a column
//! invented from something other than the dense index.
//!
//! [`GridCol`]'s field is private to this module and the *only* mint is
//! [`glyph_columns`], which uses the dense index. No code elsewhere can
//! fabricate a `GridCol` from a width sum or a byte offset — a width-sum
//! (or byte-offset) `usize` is not a `GridCol`, and the compiler refuses
//! to position a glyph or decoration at a hand-computed column. Column
//! drift between pipelines is therefore *unrepresentable*, not merely
//! tested: a `GridCol` value IS a proof that the column came from the
//! dense index (Curry–Howard). Every pipeline consumes [`glyph_columns`],
//! so the single source of column truth is shared by construction.
//!
//! Tier (per theory/UNREPRESENTABILITY.md): *truly-unrepresentable* for
//! "a glyph/decoration positioned at a non-index column" (sealed
//! constructor). The residual obligations are *test-proven*, not
//! compile-proven (C1 — Rust has no dependent types): that
//! [`glyph_columns`]' body itself uses the index (locked by
//! `glyph_columns_*` proptests) and that the terminal-owned `cursor.col`
//! lands on a column [`glyph_columns`] would yield (locked by
//! `cursor_column_is_a_glyph_column`).

use crate::terminal::Cell;

/// A column into a dense terminal row, guaranteed to be a cell's true
/// grid column (its index), never a width-sum or a byte offset.
/// Construct only via [`glyph_columns`]. See the module docs for the
/// full invariant.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub(crate) struct GridCol(usize);

impl GridCol {
    /// The underlying grid column. The sole bridge from the typed column
    /// to the raw `usize` the `col * cell_width` pixel math needs — kept
    /// to one method so every position site is auditable.
    #[inline]
    #[must_use]
    pub(crate) fn idx(self) -> usize {
        self.0
    }
}

/// THE single source of column truth: yield `(GridCol, &Cell)` for every
/// glyph-owning cell of a dense `row`, where the column is the
/// enumeration index — the cell's true grid column — by construction.
/// Wide-char continuation cells (`width == 0`, spanned by the preceding
/// lead glyph) own no column and are skipped. Every pipeline (text,
/// rect/decoration, URL detection) iterates this, so their columns
/// cannot diverge.
pub(crate) fn glyph_columns<'a>(
    row: &'a [Cell],
    cols: usize,
) -> impl Iterator<Item = (GridCol, &'a Cell)> + 'a {
    row.iter()
        .enumerate()
        .take(cols)
        .filter(|(_, cell)| cell.width != 0)
        .map(|(i, cell)| (GridCol(i), cell))
}
