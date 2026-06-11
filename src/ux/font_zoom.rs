//! [`FontZoomTarget`] — the renderer surface the unified engine
//! needs: font-size get/set (zoom) plus the measured cell metrics
//! (pixel→cell mouse math).
//!
//! `TerminalRenderer` is the production impl; tests can drive the
//! engine against a headless renderer (no GPU device — same harness
//! shape as `render::tests::gpu_free_renderer`). Every mutation goes
//! through `BoundedFontSize` in the engine, so both render modes are
//! bounded by construction (the 2026-05-21 runaway-font class cannot
//! return in either mode).

/// What the engine needs from the renderer. Kept narrow on purpose:
/// the engine must not grow ad-hoc renderer reach-ins — if a handler
/// needs more renderer surface, it gets a named trait method here.
pub trait FontZoomTarget {
    /// Current logical font size.
    fn font_size(&self) -> f32;
    /// Set the logical font size (engine passes only
    /// `BoundedFontSize`-clamped values).
    fn set_font_size(&mut self, size: f32);
    /// Physical-pixel cell width (measured when a frame has rendered,
    /// heuristic before).
    fn cell_width(&self) -> f32;
    /// Physical-pixel cell height.
    fn cell_height(&self) -> f32;
    /// HiDPI scale factor — logical padding × this = physical pad.
    fn scale_factor(&self) -> f32;
}

impl FontZoomTarget for crate::render::TerminalRenderer {
    fn font_size(&self) -> f32 {
        crate::render::TerminalRenderer::font_size(self)
    }
    fn set_font_size(&mut self, size: f32) {
        crate::render::TerminalRenderer::set_font_size(self, size);
    }
    fn cell_width(&self) -> f32 {
        crate::render::TerminalRenderer::cell_width(self)
    }
    fn cell_height(&self) -> f32 {
        crate::render::TerminalRenderer::cell_height(self)
    }
    fn scale_factor(&self) -> f32 {
        crate::render::TerminalRenderer::scale_factor(self)
    }
}
