//! The `mado banner` wordmark — composed from katsuji's typed pieces.
//!
//! # Why the composition lives HERE and not in katsuji
//!
//! katsuji is a portable emission library: it owns *how a styled line becomes
//! bytes*, and explicitly not layout ("multi-line arrangement stays with the
//! consumer"). The banner is mado's content, so mado composes it. Putting a
//! `mado_banner()` in katsuji would make a generic library know about one of
//! its consumers — the coupling the library's own docs rule out.
//!
//! # Why every glyph here is a [`Crisp`]
//!
//! `render.rs::box_drawing_rects` synthesises exactly 21 box/block chars as GPU
//! rects — pixel-perfect, zero font dependency. The other 139 in the same
//! Unicode block have **no geometry** and fall through to whatever the font
//! coverage walk picks. The pretty ones a human reaches for are precisely the
//! broken ones: `╭╮╰╯` rounded corners, `━` heavy, `╌` dashed. `Crisp` has no
//! variant for any of them, so this banner cannot name a glyph mado cannot
//! draw. That is the whole reason it is typed rather than a string literal.
//!
//! Note `╔╗╚╝` are NOT sprites even though `═` and `║` are — a double-line box
//! would have crisp edges and font-dependent corners. This uses the single
//! line set, which is crisp throughout.
//!
//! # Version
//!
//! Taken from `CARGO_PKG_VERSION` at the call site, never written here. A
//! literal would be a second place to update, and that drift is not
//! hypothetical: the installed `Mado.app` reported `0.1.0` against a `0.1.98`
//! binary because its Info.plist carried a hardcoded default.

use katsuji::{Attr, Crisp, Ink, Line, Piece};

/// The wordmark: a window (窓) in the crisp set, with the running version.
///
/// Restraint is deliberate — one accent slot, `Dim` for de-emphasis, and gaps
/// doing the spacing. The dither glyphs `░▒▓` are 25/50/75% checkerboards that
/// read as fuzz at cell size, which is the opposite of crisp, so nothing here
/// uses them.
#[must_use]
pub fn wordmark(version: &str) -> Vec<Line> {
    let accent = Ink::Cyan;
    let edge = |g: Crisp, n: usize| Piece::glyphs(g, n).ink(accent);

    vec![
        Line::new()
            .piece(edge(Crisp::CornerTopLeft, 1))
            .piece(edge(Crisp::Horizontal, 3))
            .piece(edge(Crisp::TeeDown, 1))
            .piece(edge(Crisp::Horizontal, 3))
            .piece(edge(Crisp::CornerTopRight, 1)),
        Line::new()
            .piece(edge(Crisp::Vertical, 1))
            .gap(3)
            .piece(edge(Crisp::Vertical, 1))
            .gap(3)
            .piece(edge(Crisp::Vertical, 1))
            .gap(4)
            .piece(Piece::text("mado").attr(Attr::Bold))
            .gap(2)
            .piece(Piece::text(version).attr(Attr::Dim)),
        Line::new()
            .piece(edge(Crisp::TeeLeft, 1))
            .piece(edge(Crisp::Horizontal, 3))
            .piece(edge(Crisp::Cross, 1))
            .piece(edge(Crisp::Horizontal, 3))
            .piece(edge(Crisp::TeeRight, 1)),
        Line::new()
            .piece(edge(Crisp::Vertical, 1))
            .gap(3)
            .piece(edge(Crisp::Vertical, 1))
            .gap(3)
            .piece(edge(Crisp::Vertical, 1))
            .gap(4)
            .piece(Piece::text("gpu terminal").attr(Attr::Dim)),
        Line::new()
            .piece(edge(Crisp::CornerBottomLeft, 1))
            .piece(edge(Crisp::Horizontal, 3))
            .piece(edge(Crisp::TeeUp, 1))
            .piece(edge(Crisp::Horizontal, 3))
            .piece(edge(Crisp::CornerBottomRight, 1)),
    ]
}

/// The wordmark as terminal-ready bytes, styled.
#[must_use]
pub fn render(version: &str) -> String {
    let mut out = String::new();
    for line in wordmark(version) {
        out.push_str(&line.render());
        out.push('\n');
    }
    out
}

/// The wordmark with every escape omitted — what a pipe or a log receives.
#[must_use]
pub fn plain(version: &str) -> String {
    let mut out = String::new();
    for line in wordmark(version) {
        out.push_str(&line.plain());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seal that matters: every decorative glyph is one mado can actually
    /// draw. A future edit reaching for `╭` to "round the corners" fails here
    /// rather than shipping tofu to somebody else's terminal.
    #[test]
    fn every_decorative_glyph_is_in_the_crisp_set() {
        for line in wordmark("0.1.101") {
            for ch in line.plain().chars() {
                if !ch.is_ascii() {
                    assert!(
                        Crisp::ALL.iter().any(|g| g.ch() == ch),
                        "{ch:?} has no GPU geometry — it would render as tofu"
                    );
                }
            }
        }
    }

    #[test]
    fn the_frame_is_square() {
        let widths: Vec<usize> = wordmark("0.1.101")
            .iter()
            .map(|l| l.plain().chars().take(9).count())
            .collect();
        assert!(widths.iter().all(|&w| w == 9), "frame rows disagree: {widths:?}");
    }

    /// No styled piece may leave its attribute open — a bleed would follow the
    /// banner into the operator's prompt.
    #[test]
    fn no_line_leaves_a_style_open() {
        for line in wordmark("0.1.101") {
            let s = line.render();
            if s.contains('\u{1b}') {
                assert!(s.ends_with("\u{1b}[0m"), "style left open: {s:?}");
            }
        }
    }

    #[test]
    fn the_version_is_the_one_it_was_given_not_a_literal() {
        let s = plain("9.9.9");
        assert!(s.contains("9.9.9"), "version not rendered");
        assert!(!s.contains("0.1.0"), "a version was invented");
    }

    #[test]
    fn plain_is_pipe_safe_and_render_is_styled() {
        assert!(!plain("1.2.3").contains('\u{1b}'), "plain leaked an escape");
        assert!(render("1.2.3").contains('\u{1b}'), "render lost its styling");
        assert!(plain("1.2.3").contains("mado"));
    }
}
