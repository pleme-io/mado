//! Codepoint classification for font-fallback routing.
//!
//! ## Why a typed classifier
//!
//! cosmic-text shapes a run against the configured family and, when a
//! codepoint is missing from that family, walks the loaded fontdb and
//! picks the *first* font whose coverage table claims the codepoint.
//! For powerline separators (U+E0B0…) and the Nerd-Font Private-Use-Area
//! icon blocks that "first hit" is frequently the *wrong* font — a
//! system emoji face, a partial icon font, or a different Nerd variant
//! with different artwork than the one ghostty pins. The symptom the
//! operator sees is "this glyph is right in ghostty but wrong in mado."
//!
//! Ghostty solves this by routing the symbol/PUA ranges to a dedicated
//! *symbols* font explicitly instead of trusting the coverage walk.
//! mado mirrors that: [`is_symbol_glyph`] is the typed predicate the
//! renderer consults to decide whether a cell's glyph should shape
//! against the configured `font_symbols` family (see
//! [`crate::auto_detect::FALLBACK_FONT_SYMBOLS`]) rather than the
//! primary text family.
//!
//! Box-drawing (U+2500–257F) is deliberately NOT classified as a symbol
//! here — mado renders those via the rect pipeline (`is_box_drawing` in
//! `render.rs`), never as glyphs, so routing them to a font would be
//! both wrong and dead.

/// Powerline glyph range, including the powerline-extra separators.
///
/// U+E0A0–E0A2 (branch / line-number / readonly), U+E0B0–E0B3 (the
/// classic angle / round separators), and the extended powerline-extra
/// set up to U+E0D4 (flames, ice, half-circles, trapezoids, …) that
/// modern prompts (starship / seki / oh-my-posh) emit. These live
/// inside the BMP PUA but are special-cased so the predicate reads as
/// intent ("powerline") rather than a bare range check.
const POWERLINE_RANGES: &[(char, char)] = &[
    ('\u{E0A0}', '\u{E0A2}'),
    ('\u{E0B0}', '\u{E0D4}'),
];

/// Nerd-Font icon blocks. The classic patch packs its glyphs across the
/// BMP Private Use Area (U+E000–F8FF: Seti-UI, Devicons, Font Awesome,
/// Weather, Octicons, Material, …). Recent Nerd-Font releases also place
/// icons in the supplementary Private Use Area-A (U+F0000–FFFFD). Both
/// are routed to the symbols family.
///
/// Note U+E0A0–E0D4 (powerline) sits inside the first range; it is also
/// listed in [`POWERLINE_RANGES`] for documentation, but membership in
/// either set yields the same routing decision so the overlap is benign.
const NERD_PUA_RANGES: &[(char, char)] = &[
    ('\u{E000}', '\u{F8FF}'),   // BMP Private Use Area
    ('\u{F0000}', '\u{FFFFD}'), // Supplementary Private Use Area-A
];

/// True when `c` is a powerline separator or a Nerd-Font PUA icon — a
/// codepoint that should shape against the configured symbols family
/// rather than the primary text family.
///
/// ASCII and ordinary text (Latin, CJK, emoji in their own planes)
/// return `false` and keep shaping against the primary / italic family
/// as before, so this only diverts the icon ranges.
#[must_use]
pub fn is_symbol_glyph(c: char) -> bool {
    in_any(c, POWERLINE_RANGES) || in_any(c, NERD_PUA_RANGES)
}

/// True when every `char` in `s` is a symbol glyph (and `s` is
/// non-empty). The renderer routes a run to the symbols family only
/// when the whole run is symbol codepoints — a run is built per-cell
/// for non-ASCII cells, so in practice this is a single icon, but the
/// all-symbol check keeps the routing total/correct if a future batcher
/// ever groups adjacent icons.
#[must_use]
pub fn run_is_all_symbols(s: &str) -> bool {
    let mut any = false;
    for c in s.chars() {
        if !is_symbol_glyph(c) {
            return false;
        }
        any = true;
    }
    any
}

#[inline]
fn in_any(c: char, ranges: &[(char, char)]) -> bool {
    ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powerline_separators_are_symbols() {
        // The canonical right-angle and round separators every prompt
        // theme emits.
        for c in ['\u{E0B0}', '\u{E0B1}', '\u{E0B2}', '\u{E0B3}'] {
            assert!(is_symbol_glyph(c), "{c:?} (U+{:04X}) should be a symbol", c as u32);
        }
        // powerline-extra (flames / ice / half-circles) up to U+E0D4.
        assert!(is_symbol_glyph('\u{E0C0}'));
        assert!(is_symbol_glyph('\u{E0D4}'));
        // The vcs / readonly powerline glyphs.
        assert!(is_symbol_glyph('\u{E0A0}'));
        assert!(is_symbol_glyph('\u{E0A2}'));
    }

    #[test]
    fn nerd_pua_icons_are_symbols() {
        // BMP PUA endpoints + an interior Devicons/FontAwesome point.
        assert!(is_symbol_glyph('\u{E000}'));
        assert!(is_symbol_glyph('\u{F8FF}'));
        assert!(is_symbol_glyph('\u{F300}')); // e.g. nf-linux-* block
        // nf-dev-ruby (U+E791) — the lualine "red ruby" devicon. A PUA
        // devicon, classified as a symbol → routed to the symbols family
        // where it correctly takes the cell's SGR fg (the red span colour).
        assert!(is_symbol_glyph('\u{E791}'));
        // Supplementary PUA-A (modern Nerd releases).
        assert!(is_symbol_glyph('\u{F0001}'));
        assert!(is_symbol_glyph('\u{FFFFD}'));
    }

    #[test]
    fn ordinary_text_is_not_a_symbol() {
        // ASCII, Latin-1, CJK, and an astral emoji must NOT be diverted
        // to the symbols family — they shape against the primary family.
        for c in ['a', 'Z', '0', ' ', 'é', '日', '本', '\u{1F600}'] {
            assert!(!is_symbol_glyph(c), "{c:?} must not be a symbol");
        }
    }

    #[test]
    fn box_drawing_is_not_classified_here() {
        // Box-drawing renders via the rect pipeline, never as a glyph —
        // classifying it as a symbol would route a glyph that's never
        // emitted. Pin that it stays out of the symbol set.
        for c in ['\u{2500}', '\u{2502}', '\u{250C}', '\u{257F}'] {
            assert!(!is_symbol_glyph(c), "box-drawing {c:?} is rect-rendered, not a symbol glyph");
        }
    }

    #[test]
    fn run_all_symbols_requires_every_char_and_nonempty() {
        assert!(run_is_all_symbols("\u{E0B0}"));
        assert!(run_is_all_symbols("\u{E0B0}\u{F300}"));
        assert!(!run_is_all_symbols("")); // empty is not "all symbols"
        assert!(!run_is_all_symbols("\u{E0B0}a")); // mixed → false
        assert!(!run_is_all_symbols("abc"));
    }
}
