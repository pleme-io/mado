use crate::config::WindowChromeConfig;

pub const FADE_STEPS: usize = 16;
pub const GLYPH_LOGICAL_PX: f32 = 10.0;
pub const GLYPH_HALF_STROKE_LOGICAL_PX: f32 = 0.5;
pub const HOVER_PILL_LOGICAL_PX: f32 = 24.0;
pub const TITLE_LEFT_MARGIN_LOGICAL_PX: f32 = 16.0;
pub const DOUBLE_CLICK_MS: u128 = 400;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeStyle {
    pub enabled: bool,
    pub height: f32,
    pub edge_width: f32,
    pub button_width: f32,
    pub tint: f32,
    pub title: bool,
}

impl ChromeStyle {
    #[must_use]
    pub fn from_config(config: &WindowChromeConfig) -> Self {
        Self {
            enabled: config.enabled,
            height: config.height.max(0.0),
            edge_width: config.edge_width.max(0.0),
            button_width: config.button_width.max(0.0),
            tint: config.tint.max(0.0).min(1.0),
            title: config.title,
        }
    }

    #[must_use]
    pub fn metrics(&self, band_px: f32, scale: f32, corner_radius: f32) -> madori::ChromeMetrics {
        let edge = self.edge_width * scale;
        madori::ChromeMetrics {
            band_height: band_px,
            edge_width: edge,
            corner_extent: (edge * 2.5).max(corner_radius * scale),
            button_width: self.button_width * scale,
        }
    }
}

impl Default for ChromeStyle {
    fn default() -> Self {
        Self::from_config(&WindowChromeConfig::default())
    }
}

#[must_use]
pub fn fade_steps(band_px: f32, peak: f32) -> Vec<(f32, f32, f32)> {
    if peak <= 0.0 || band_px <= 0.0 {
        return Vec::new();
    }
    let n = FADE_STEPS as f32;
    (0..FADE_STEPS)
        .map(|i| {
            let i = i as f32;
            let top = (i * band_px / n).round();
            let bottom = ((i + 1.0) * band_px / n).round();
            let alpha = peak * (1.0 - (i + 0.5) / n);
            (top, bottom - top, alpha)
        })
        .filter(|(_, height, _)| *height > 0.0)
        .collect()
}

#[must_use]
pub fn clean_title(raw: &str, max_chars: usize) -> Option<String> {
    let title = raw.trim();
    if title.is_empty()
        || title.chars().any(char::is_control)
        || !title.chars().any(char::is_alphanumeric)
        || max_chars < 2
    {
        return None;
    }
    if title.chars().count() <= max_chars {
        return Some(title.to_owned());
    }
    let mut clipped: String = title.chars().take(max_chars - 1).collect();
    clipped.push('…');
    Some(clipped)
}

#[must_use]
pub fn glyphs_are_clean(glyph_ids: impl IntoIterator<Item = u16>) -> bool {
    let mut any = false;
    for id in glyph_ids {
        if id == 0 {
            return false;
        }
        any = true;
    }
    any
}

#[must_use]
pub fn region_pointer_shape(region: madori::WindowRegion) -> crate::pointer_shape::PointerShape {
    use crate::pointer_shape::PointerShape;
    use madori::{ResizeEdge, WindowRegion};
    match region {
        WindowRegion::Interior | WindowRegion::Band | WindowRegion::Button(_) => {
            PointerShape::Default
        }
        WindowRegion::Edge(ResizeEdge::North | ResizeEdge::South) => PointerShape::NsResize,
        WindowRegion::Edge(ResizeEdge::East | ResizeEdge::West) => PointerShape::EwResize,
        WindowRegion::Edge(ResizeEdge::NorthEast | ResizeEdge::SouthWest) => {
            PointerShape::NeswResize
        }
        WindowRegion::Edge(ResizeEdge::NorthWest | ResizeEdge::SouthEast) => {
            PointerShape::NwseResize
        }
    }
}

#[must_use]
pub fn button_op(button: madori::ChromeButton) -> madori::WindowOp {
    match button {
        madori::ChromeButton::Minimize => madori::WindowOp::Minimize,
        madori::ChromeButton::Maximize => madori::WindowOp::ToggleMaximize,
        madori::ChromeButton::Close => madori::WindowOp::Close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_fades_to_nothing_so_it_has_no_seam_against_the_terminal() {
        let steps = fade_steps(60.0, 0.2);
        assert_eq!(steps.len(), FADE_STEPS);
        let last = steps.last().unwrap();
        assert!(
            last.2 * 255.0 < 2.0,
            "the bottom row is at most a rounding error: {}",
            last.2
        );
        for pair in steps.windows(2) {
            assert!(pair[0].2 > pair[1].2, "alpha falls towards the terminal");
            assert_eq!(pair[0].0 + pair[0].1, pair[1].0, "rows touch with no gap");
        }
        assert_eq!(steps[0].0, 0.0);
        assert_eq!(last.0 + last.1, 60.0, "the fade ends on the band's edge");
    }

    #[test]
    fn no_tint_or_no_band_draws_no_fade() {
        assert!(fade_steps(60.0, 0.0).is_empty());
        assert!(fade_steps(0.0, 0.2).is_empty());
    }

    #[test]
    fn a_short_band_loses_empty_steps_and_stays_contiguous() {
        let steps = fade_steps(4.0, 0.2);
        assert!(!steps.is_empty());
        assert_eq!(steps.iter().map(|s| s.1).sum::<f32>(), 4.0);
    }

    #[test]
    fn a_title_with_nothing_to_read_is_not_a_title() {
        for bad in ["❄", "  ", "", "✦ ✦", "bad\u{7}title", "tab\tinside"] {
            assert_eq!(clean_title(bad, 40), None, "{bad:?}");
        }
    }

    #[test]
    fn a_real_title_passes_and_a_long_one_is_clipped() {
        assert_eq!(
            clean_title(" ~/code/mado ", 40).as_deref(),
            Some("~/code/mado")
        );
        assert_eq!(clean_title("日本語", 40).as_deref(), Some("日本語"));
        assert_eq!(clean_title("abcdefghij", 5).as_deref(), Some("abcd…"));
        assert_eq!(clean_title("abc", 1), None);
    }

    #[test]
    fn a_missing_glyph_hides_the_title() {
        assert!(glyphs_are_clean([12, 40, 7]));
        assert!(!glyphs_are_clean([12, 0, 7]), "glyph 0 is the font's tofu");
        assert!(!glyphs_are_clean([]), "nothing shaped is nothing to show");
    }

    #[test]
    fn every_region_has_a_pointer_that_matches_madoris_icon() {
        use madori::{ChromeButton, ResizeEdge, WindowRegion};
        let mut regions = vec![
            WindowRegion::Interior,
            WindowRegion::Band,
            WindowRegion::Button(ChromeButton::Close),
        ];
        regions.extend(ResizeEdge::ALL.map(WindowRegion::Edge));
        for region in regions {
            assert_eq!(
                region_pointer_shape(region).icon(),
                region.pointer_icon(),
                "{region:?}"
            );
        }
    }

    #[test]
    fn every_button_has_its_window_operation() {
        use madori::{ChromeButton, WindowOp};
        assert_eq!(button_op(ChromeButton::Close), WindowOp::Close);
        assert_eq!(button_op(ChromeButton::Minimize), WindowOp::Minimize);
        assert_eq!(button_op(ChromeButton::Maximize), WindowOp::ToggleMaximize);
    }

    #[test]
    fn a_bad_style_config_cannot_reach_the_layout() {
        let config = WindowChromeConfig {
            height: f32::NAN,
            edge_width: -2.0,
            button_width: f32::NAN,
            tint: 5.0,
            ..WindowChromeConfig::default()
        };
        let style = ChromeStyle::from_config(&config);
        assert_eq!(
            (
                style.height,
                style.edge_width,
                style.button_width,
                style.tint
            ),
            (0.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn the_corner_grab_follows_the_scale_and_a_large_corner_radius() {
        let style = ChromeStyle::from_config(&WindowChromeConfig::default());
        let m = style.metrics(60.0, 2.0, 0.0);
        assert_eq!(m.band_height, 60.0);
        assert_eq!(m.edge_width, style.edge_width * 2.0);
        assert_eq!(m.corner_extent, m.edge_width * 2.5);
        let rounded = style.metrics(60.0, 2.0, 30.0);
        assert_eq!(rounded.corner_extent, 60.0);
    }
}
