//! Typed, themed inputs for the ONE overlay renderer
//! (`crate::render::TerminalRenderer::draw_overlay`) every picker draws
//! through.
//!
//! Pre-base, three near-identical `draw_*` methods each built a glyphon
//! buffer stack, a `TextArea` per line, and a render pass — and each
//! hardcoded the same two Nord literals (`rgba(136,192,208)` query,
//! `rgba(163,190,140)` selected). This module is the typed surface that
//! collapses them: a picker builds an [`OverlaySpec`] (anchor + typed
//! [`OverlayLine`]s), the renderer draws it once, and every colour
//! resolves from the active theme via [`OverlayStyle`] — no Nord literal
//! survives, so a theme swap restyles every overlay.
//!
//! The colour values live here as `terminal::Color` (u8-RGB, the
//! renderer's native shape); the theme→style resolution lives in
//! `crate::theme` where the `Theme` + ANSI table are. The geometry
//! (Top / Bottom / Center anchoring, the centred panel) lives in the
//! renderer where the cell metrics are.

use crate::config::PickerAnchor;
use crate::terminal::Color;

/// The resolved colours one overlay paints with. Resolved from the active
/// theme (see `crate::theme::OverlayStyle::from_theme`) so the picker
/// chrome tracks the operator's theme instead of a baked-in palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayStyle {
    /// The query/prompt line (`▶ session  foo█`). Theme accent.
    pub query: Color,
    /// A non-selected result row. Theme foreground.
    pub row: Color,
    /// The highlighted result row's TEXT. Theme success/selected accent.
    pub selected: Color,
    /// An empty-state / disabled hint line. Dimmed foreground.
    pub hint: Color,
    /// The solid popup-panel fill behind a `Center`-anchored picker — the
    /// opaque card so the popup isn't transparent text floating over the
    /// terminal. A dark, theme-cohesive surface (Nord polar night by
    /// default). Drawn at full opacity (linearized at paint time).
    pub panel: Color,
    /// The panel's accent border (a hairline edge around the card).
    pub border: Color,
    /// The highlight bar behind the selected row (the moving cursor of the
    /// list — what makes juggling sessions feel sleek).
    pub selected_bg: Color,
}

impl OverlayStyle {
    /// The pre-theme Nord defaults — the literals the old `draw_*` methods
    /// used for text, plus the popup-card surfaces. The fallback the
    /// renderer is born with; `theme::apply_config_theme` overrides per
    /// theme.
    #[must_use]
    pub const fn nord_default() -> Self {
        Self {
            query: Color::new(0x88, 0xC0, 0xD0),       // Nord frost
            row: Color::new(0xD8, 0xDE, 0xE9),         // Nord snow
            selected: Color::new(0xEC, 0xEF, 0xF4),    // Nord snow-bright
            hint: Color::new(0x4C, 0x56, 0x6A),        // Nord polar dim
            panel: Color::new(0x2E, 0x34, 0x40),       // Nord polar night (card)
            border: Color::new(0x88, 0xC0, 0xD0),      // Nord frost (edge)
            selected_bg: Color::new(0x43, 0x4C, 0x5E), // Nord polar 2 (highlight bar)
        }
    }
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self::nord_default()
    }
}

/// The semantic role of one overlay line — maps to a colour in
/// [`OverlayStyle`] at draw time, so the call site never names a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRole {
    /// The query/prompt line.
    Title,
    /// The highlighted result row.
    Selected,
    /// A non-selected result row.
    Row,
    /// An empty-state / disabled hint.
    Hint,
}

/// One line of an overlay: its already-composed display text + the role
/// that picks its colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLine {
    pub text: String,
    pub role: LineRole,
    /// Text opacity 0..=255 (255 = solid). The suggestion-stream shade-in
    /// ramps this per frame so a freshly-arrived row dissolves in from the
    /// panel behind it rather than popping. Solid for every other line.
    pub alpha: u8,
    /// Optional foreground override. `None` = the [`LineRole`]'s themed colour
    /// (the calm default); `Some` = a deliberate tint (e.g. an urgent task
    /// suggestion glowing hot). Kept sparing so the picker stays a calm home.
    pub color: Option<Color>,
    /// CHAR positions in [`Self::text`] that matched the picker query — the
    /// renderer tints exactly these (the fzf-style "here's why this row
    /// matched" highlight). Empty = no highlight (the default / empty-query
    /// view renders byte-identically to before).
    pub highlights: Vec<usize>,
}

impl OverlayLine {
    #[must_use]
    pub fn new(text: impl Into<String>, role: LineRole) -> Self {
        Self {
            text: text.into(),
            role,
            alpha: 255,
            color: None,
            highlights: Vec::new(),
        }
    }

    /// Set the line's opacity (the shade-in ramp). Chainable.
    #[must_use]
    pub fn with_alpha(mut self, alpha: u8) -> Self {
        self.alpha = alpha;
        self
    }

    /// Override the line's foreground colour (the urgency tint). `None` keeps
    /// the role colour. Chainable.
    #[must_use]
    pub fn with_color(mut self, color: Option<Color>) -> Self {
        self.color = color;
        self
    }

    /// Set the matched-char positions to highlight (the query-match tint).
    /// Chainable.
    #[must_use]
    pub fn with_highlights(mut self, highlights: Vec<usize>) -> Self {
        self.highlights = highlights;
        self
    }
}

/// Split `text` into consecutive byte-range runs flagged matched / unmatched,
/// from a set of matched CHAR positions. The renderer turns each run into a
/// glyphon span (matched runs get the accent colour). Pure — the unit the
/// highlight render is tested through.
#[must_use]
pub fn highlight_runs(text: &str, positions: &[usize]) -> Vec<(std::ops::Range<usize>, bool)> {
    if positions.is_empty() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![(0..text.len(), false)]
        };
    }
    let set: std::collections::HashSet<usize> = positions.iter().copied().collect();
    let mut runs: Vec<(std::ops::Range<usize>, bool)> = Vec::new();
    let mut run_start = 0usize;
    let mut run_hl: Option<bool> = None;
    for (ci, (byte, _ch)) in text.char_indices().enumerate() {
        let hl = set.contains(&ci);
        match run_hl {
            None => {
                run_hl = Some(hl);
                run_start = byte;
            }
            Some(prev) if prev != hl => {
                runs.push((run_start..byte, prev));
                run_hl = Some(hl);
                run_start = byte;
            }
            _ => {}
        }
    }
    if let Some(prev) = run_hl {
        runs.push((run_start..text.len(), prev));
    }
    runs
}

/// A complete overlay to draw: where it anchors + its typed lines. The
/// renderer owns the geometry (line height, centring, the panel) and the
/// [`OverlayStyle`]; this is purely *what* to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySpec {
    /// Where the block sits: [`PickerAnchor::Center`] floats it in the
    /// middle (with a backing panel), `Bottom` rises from the bottom edge
    /// (Ctrl-R/Ctrl-T feel), `Top` drops from the top.
    pub anchor: PickerAnchor,
    /// The lines, top to bottom: a title line then the rows (or a hint).
    pub lines: Vec<OverlayLine>,
}

impl OverlaySpec {
    #[must_use]
    pub fn new(anchor: PickerAnchor, lines: Vec<OverlayLine>) -> Self {
        Self { anchor, lines }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_runs_splits_matched_and_unmatched() {
        // "abcd" with chars 1,2 matched → [a](no) [bc](yes) [d](no).
        let runs = highlight_runs("abcd", &[1, 2]);
        assert_eq!(runs, vec![(0..1, false), (1..3, true), (3..4, false)]);
        // No positions → one unmatched run over the whole string.
        assert_eq!(highlight_runs("abc", &[]), vec![(0..3, false)]);
        // Empty text → no runs.
        assert_eq!(highlight_runs("", &[5]), Vec::new());
        // A leading match → first run is matched.
        assert_eq!(
            highlight_runs("xy", &[0]),
            vec![(0..1, true), (1..2, false)]
        );
        // Concatenating the run slices reconstructs the text exactly.
        let text = "pr#7 mado";
        let reconstructed: String = highlight_runs(text, &[0, 1, 5, 6, 7, 8])
            .iter()
            .map(|(r, _)| &text[r.clone()])
            .collect();
        assert_eq!(reconstructed, text);
    }

    #[test]
    fn highlight_runs_is_multibyte_safe() {
        // 🌊 is 4 bytes; matching the ASCII tail must use correct byte offsets.
        let text = "🌊ab";
        let runs = highlight_runs(text, &[1]); // char 1 = 'a'
        // Each run slice must be a valid &str (no panic) and rejoin to text.
        let joined: String = runs.iter().map(|(r, _)| &text[r.clone()]).collect();
        assert_eq!(joined, text);
        assert!(runs.iter().any(|(r, hl)| *hl && &text[r.clone()] == "a"));
    }

    #[test]
    fn overlay_line_is_solid_uncoloured_by_default_and_ramps() {
        let plain = OverlayLine::new("row", LineRole::Row);
        assert_eq!(plain.alpha, 255, "lines are solid unless shaded");
        assert_eq!(plain.color, None, "lines use the role colour unless tinted");
        let faint = OverlayLine::new("row", LineRole::Row).with_alpha(64);
        assert_eq!(faint.alpha, 64);
        let hot =
            OverlayLine::new("row", LineRole::Row).with_color(Some(Color::new(0xBF, 0x61, 0x6A)));
        assert_eq!(hot.color, Some(Color::new(0xBF, 0x61, 0x6A)));
        // builders leave text + role untouched (only opacity / colour).
        assert_eq!(faint.text, plain.text);
        assert_eq!(hot.role, plain.role);
    }
}
