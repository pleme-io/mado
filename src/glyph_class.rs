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
//! Box-drawing (U+2500–259F) is deliberately NOT classified as a symbol
//! here — mado renders those via the rect pipeline ([`is_box_drawing`]),
//! never as glyphs, so routing them to a font would be both wrong and
//! dead. [`is_box_drawing`] lives in this module (not `render.rs`, its
//! only consumer for a long time) so `terminal.rs`'s reflow logic can
//! also depend on it without the model layer reaching into the
//! renderer — one classification, two consumers, no layering
//! violation.

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

/// BMP symbol blocks that hold the `Emoji=Yes` + `Emoji_Presentation=No`
/// codepoints — characters that are emoji-capable but default to a
/// monochrome *text* glyph (❄ U+2744, ☄ U+2604, ✳ U+2733, ✔ U+2714, …).
/// They live in Miscellaneous Technical (U+2300–23FF), Miscellaneous
/// Symbols (U+2600–26FF) and Dingbats (U+2700–27BF); the single covering
/// range U+2300–U+27FF is the idiom this module already uses for
/// powerline/PUA.
///
/// The grid (unicode-width) assigns these `width == 1`, so the cell only
/// reserves one column. The renderer must therefore draw the 1-cell text
/// glyph, not the ~2-cell color/emoji glyph a color font would otherwise
/// pick — see [`is_text_presentation_emoji`].
const TEXT_PRESENTATION_EMOJI_RANGES: &[(char, char)] = &[
    ('\u{2300}', '\u{27FF}'), // Misc Technical · Misc Symbols · Dingbats
];

/// True when `c` is an emoji-capable symbol that defaults to *text*
/// presentation (`Emoji=Yes`, `Emoji_Presentation=No`).
///
/// The renderer consults this for `width == 1` cells and appends VS15
/// (U+FE0E) to the shaping run so cosmic-text selects the monochrome,
/// single-cell glyph instead of the wide color variant that would
/// otherwise overflow into the next cell (the prompt cursor). The
/// covering range is mildly over-inclusive (it also spans a few non-emoji
/// symbols), but that is **benign**: VS15 only changes glyph selection for
/// codepoints that actually have *both* a text and an emoji presentation;
/// on a single-presentation symbol the font ignores the selector and
/// renders its sole glyph unchanged. Astral emoji (U+1F300+) are
/// deliberately excluded — they are `Emoji_Presentation=Yes`, `width == 2`,
/// and must keep their color glyph.
#[must_use]
pub fn is_text_presentation_emoji(c: char) -> bool {
    in_any(c, TEXT_PRESENTATION_EMOJI_RANGES)
}

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

/// Box-drawing / block-element range mado renders as GPU rects
/// (`render.rs::box_drawing_rects`) rather than shaped glyphs, so a
/// character in this range never goes through the font system at all.
///
/// Also consulted by `terminal.rs`'s reflow (`rewrap_to_cols`): a
/// contiguous run of these is a rigid pre-computed layout (a table
/// border, a box outline) the writer sized for ONE specific column
/// count, never prose a reflow can meaningfully re-wrap — splitting one
/// mid-run relocates every character after the split relative to
/// whatever alignment the writer intended, which reads as corruption
/// even though no cell's content actually changed.
#[must_use]
pub fn is_box_drawing(c: char) -> bool {
    in_any(c, BOX_DRAWING_RANGES)
}

/// U+2500–257F (box-drawing proper: lines, corners, tees, crosses) plus
/// U+2580–259F (block elements: half/quadrant/shade blocks) — the same
/// two ranges `render.rs` has always drawn as rects.
const BOX_DRAWING_RANGES: &[(char, char)] = &[('\u{2500}', '\u{257F}'), ('\u{2580}', '\u{259F}')];

#[inline]
fn in_any(c: char, ranges: &[(char, char)]) -> bool {
    ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_presentation_emoji_are_classified() {
        // The operator-reported snowflake plus its siblings from the
        // Misc-Symbols / Dingbats blocks — all Emoji=Yes, EP=No.
        assert!(is_text_presentation_emoji('\u{2744}')); // ❄ snowflake
        assert!(is_text_presentation_emoji('\u{2604}')); // ☄ comet
        assert!(is_text_presentation_emoji('\u{2733}')); // ✳ eight-spoked asterisk
        assert!(is_text_presentation_emoji('\u{2714}')); // ✔ heavy check mark
    }

    #[test]
    fn ordinary_text_is_not_text_presentation_emoji() {
        // ASCII / Latin-1 / CJK / astral-emoji must NOT get VS15 forced.
        // Astral 🍎 is Emoji_Presentation=Yes (width 2) and stays color.
        for c in ['a', 'Z', '0', ' ', 'é', '日', '本', '\u{1F34E}'] {
            assert!(
                !is_text_presentation_emoji(c),
                "{c:?} must not be classified as text-presentation emoji"
            );
        }
    }

    #[test]
    fn box_drawing_covers_lines_corners_and_blocks() {
        assert!(is_box_drawing('\u{2500}')); // ─ horizontal
        assert!(is_box_drawing('\u{2502}')); // │ vertical
        assert!(is_box_drawing('\u{250C}')); // ┌ corner
        assert!(is_box_drawing('\u{2501}')); // ━ heavy horizontal
        assert!(is_box_drawing('\u{2588}')); // █ full block
        assert!(is_box_drawing('\u{2591}')); // ░ light shade
    }

    #[test]
    fn box_drawing_excludes_ordinary_text() {
        for c in ['A', ' ', '漢'] {
            assert!(!is_box_drawing(c), "{c:?} must not be box-drawing");
        }
    }

    #[test]
    fn box_drawing_range_boundaries() {
        assert!(is_box_drawing('\u{2500}'));
        assert!(!is_box_drawing('\u{24FF}'));
        assert!(is_box_drawing('\u{257F}'));
        assert!(is_box_drawing('\u{2580}'));
        assert!(is_box_drawing('\u{259F}'));
        assert!(!is_box_drawing('\u{25A0}'));
    }

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
