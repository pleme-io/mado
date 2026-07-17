//! The REAL browser engine behind [`mado::float::BrowserBackend`] — a pure-Rust
//! HTML→pixels pipeline over `nami-core`.
//!
//! namimado's `NamiNativeEngine` is bin-only (unreachable), so mado re-authors
//! the ~40-line engine here over nami-core's PUBLIC pipeline (the same one
//! namimado drives):
//!
//! ```text
//! navigate(url)                          render_html(html)  ← offline, no net
//!   → nami-core net fetch (todoku)   ┐
//!                                    ├─→ Document::parse (html5ever)
//!                                    │   → <style> text + StyleSheet::parse
//!                                    │   → StyleResolver::resolve → StyledTree
//!                                    │   → LayoutEngine::compute (no-measure,
//!                                    │     single-line floor — NO cosmic-text)
//!                                    │   → paint::build_display_list → DisplayList
//! ```
//!
//! Fidelity is tier-honest reader-view: simple/static HTML renders faithfully
//! (proven by the ported offline test), modern JS/SPA pages do NOT (no JS, no
//! inline-flow wrapping under the no-measure path, block-only backgrounds).
//! The engine is swappable at mado's real seam ([`mado::float::BrowserBackend`]);
//! a servo/CDP backend later is a new `BrowserBackend` impl, not a fork.
//!
//! This lives in the BIN (not the lib `float` module) so the pure `float`
//! substrate keeps zero engine dependency — the trait is in the lib, the heavy
//! concrete impl is here.

use nami_core::css::{StyleResolver, StyleSheet};
use nami_core::dom::{Node, NodeData};
use nami_core::dom::Document;
use nami_core::engine::{BrowserEngine, ContentRect};
use nami_core::layout::{LayoutEngine, Size};
use nami_core::net::FetchClient;
use nami_core::paint::{self, DisplayList};
use url::Url;

use mado::float::{BackendError, BrowserBackend, LoadState, RenderedFrame};

/// The pure-Rust pixel-painting engine — mado's port of nami-native over
/// nami-core's public pipeline. Caches the display list (rebuilt only on
/// navigate/resize, never per frame).
pub struct MadoNamiEngine {
    content_rect: ContentRect,
    display_list: DisplayList,
    last_html: Option<String>,
    /// Whether the last `navigate` fetched + rendered content (vs failed).
    navigate_ok: bool,
}

impl MadoNamiEngine {
    /// Construct with a starting content rect; the display list is empty until
    /// the first `navigate`/`render_html`.
    #[must_use]
    pub fn new(content_rect: ContentRect) -> Self {
        Self {
            content_rect,
            display_list: DisplayList::default(),
            last_html: None,
            navigate_ok: false,
        }
    }

    /// Run the parse → cascade → layout → display-list pipeline on a static
    /// HTML string and cache the result. No network — the offline half of
    /// `navigate` (and the path tests + direct-HTML consumers use). Uses the
    /// no-measure `LayoutEngine::compute` (single-line floor), so no
    /// cosmic-text measurer is needed.
    pub fn render_html(&mut self, html: &str) {
        let doc = Document::parse(html);
        let css = collect_style_text(&doc.root);
        let mut resolver = StyleResolver::new();
        if !css.trim().is_empty() {
            match StyleSheet::parse(&css) {
                Ok(sheet) => resolver.add_sheet(sheet),
                Err(e) => tracing::warn!(error = %e, "mado-nami: <style> parse failed"),
            }
        }
        let styled = resolver.resolve(&doc);
        let viewport = Size::new(self.content_rect.width, self.content_rect.height);
        let mut engine = LayoutEngine::new();
        let layout = engine.compute(&styled, viewport);
        self.display_list = paint::build_display_list(&layout, &styled, &doc);
        self.last_html = Some(html.to_owned());
    }

    /// Command count in the cached display list (loaded-vs-empty probe).
    #[must_use]
    pub fn content_len(&self) -> usize {
        self.display_list.cmds.len()
    }
}

impl BrowserEngine for MadoNamiEngine {
    fn name(&self) -> &'static str {
        "mado-nami"
    }

    fn navigate(&mut self, url: &Url) {
        let client = FetchClient::new();
        match client.fetch(url) {
            Ok(resp) => match resp.text() {
                Some(html) => {
                    let html = html.to_owned();
                    self.render_html(&html);
                    self.navigate_ok = true;
                }
                None => {
                    tracing::warn!(url = %url, "mado-nami: response had no text body");
                    self.display_list = DisplayList::default();
                    self.navigate_ok = false;
                }
            },
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "mado-nami: fetch failed");
                self.display_list = DisplayList::default();
                self.navigate_ok = false;
            }
        }
    }

    fn resize(&mut self, rect: ContentRect) {
        self.content_rect = rect;
        if let Some(html) = self.last_html.take() {
            self.render_html(&html);
        }
    }

    fn renders_pixels(&self) -> bool {
        true
    }

    fn take_display_list(&mut self) -> DisplayList {
        std::mem::take(&mut self.display_list)
    }
}

/// Concatenate every `<style>` element's text into one CSS source (ported from
/// nami-native — walks the DOM depth-first).
fn collect_style_text(root: &Node) -> String {
    let mut out = String::new();
    collect_style_into(root, &mut out);
    out
}

fn collect_style_into(node: &Node, out: &mut String) {
    if let NodeData::Element(el) = &node.data {
        if el.tag == "style" {
            out.push_str(&node.text_content());
            out.push('\n');
            return;
        }
    }
    for child in &node.children {
        collect_style_into(child, out);
    }
}

/// The concrete [`mado::float::BrowserBackend`] over [`MadoNamiEngine`]. The
/// live GPU compositor (Ledge E) takes the [`DisplayList`] from
/// [`Self::display_list`] and paints its `DrawCmd`s onto mado's RectPipeline +
/// glyphon.
pub struct RealBrowserBackend {
    engine: MadoNamiEngine,
    load: LoadState,
    clock: f64,
    width: u32,
    height: u32,
}

impl RealBrowserBackend {
    /// A nami-native backend for a `width`×`height` content area.
    #[must_use]
    pub fn nami_native(width: u32, height: u32) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let rect = ContentRect::new(0.0, 0.0, width as f32, height as f32);
        Self {
            engine: MadoNamiEngine::new(rect),
            load: LoadState::Idle,
            clock: 0.0,
            width,
            height,
        }
    }

    /// Render HTML offline (no network) + mark loaded. For feeding HTML
    /// directly + tests.
    pub fn render_html(&mut self, html: &str) {
        self.engine.render_html(html);
        self.load = LoadState::Loaded;
    }

    /// Take the current display list (the GPU layer consumes this). Clears it,
    /// so a stale list is never re-painted.
    pub fn display_list(&mut self) -> DisplayList {
        self.engine.take_display_list()
    }

    /// The content-area size (CSS px).
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl BrowserBackend for RealBrowserBackend {
    fn navigate(&mut self, url: &Url) -> Result<(), BackendError> {
        self.load = LoadState::Loading;
        self.engine.navigate(url);
        if self.engine.navigate_ok {
            self.load = LoadState::Loaded;
            Ok(())
        } else {
            self.load = LoadState::Failed;
            Err(BackendError::Load(url.to_string()))
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        #[allow(clippy::cast_precision_loss)]
        self.engine
            .resize(ContentRect::new(0.0, 0.0, width as f32, height as f32));
    }

    fn load_state(&self) -> LoadState {
        self.load
    }

    fn eval(&mut self, _script: &str) -> Result<String, BackendError> {
        // nami-native has no JS host → a visible Unsupported, never a silent Ok.
        Err(BackendError::Unsupported("javascript eval"))
    }

    fn screenshot(&mut self) -> Result<RenderedFrame, BackendError> {
        // The RGBA readback path is the L2 GPU wiring (garasu HeadlessTarget) —
        // deferred here → a typed NoSurface, never a lie.
        Err(BackendError::NoSurface)
    }

    fn now(&self) -> f64 {
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nami_core::paint::DrawCmd;

    fn backend() -> RealBrowserBackend {
        RealBrowserBackend::nami_native(800, 600)
    }

    #[test]
    fn render_html_builds_a_nonempty_display_list_offline() {
        // The real pipeline on inline HTML — no network. A sized div gives a
        // background Rect; a p with text gives a Text command.
        let mut b = backend();
        b.render_html(
            "<style>div{background-color:#3050ff;width:200px;height:100px} \
             p{color:#ffffff;height:30px}</style><div><p>Hello</p></div>",
        );
        assert_eq!(b.load_state(), LoadState::Loaded);
        let dl = b.display_list();
        assert!(!dl.is_empty(), "expected a non-empty display list: {dl:?}");
        assert!(
            dl.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Rect { color, .. } if color.b > 0.9)),
            "expected a blue div background Rect: {:?}",
            dl.cmds
        );
        assert!(
            dl.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Text { text, .. } if text == "Hello")),
            "expected a 'Hello' Text command: {:?}",
            dl.cmds
        );
    }

    #[test]
    fn display_list_clears_after_take() {
        let mut b = backend();
        b.render_html("<style>div{background-color:#112233;width:50px;height:50px}</style><div></div>");
        assert!(!b.display_list().is_empty());
        // Second take is empty — the cached list was consumed.
        assert!(b.display_list().is_empty());
    }

    #[test]
    fn eval_is_unsupported_never_silent() {
        let mut b = backend();
        assert_eq!(b.eval("1+1"), Err(BackendError::Unsupported("javascript eval")));
    }

    #[test]
    fn screenshot_is_no_surface_until_gpu_wiring_lands() {
        let mut b = backend();
        assert_eq!(b.screenshot(), Err(BackendError::NoSurface));
    }

    #[test]
    fn the_engine_paints_pixels_and_names_itself() {
        let e = MadoNamiEngine::new(ContentRect::new(0.0, 0.0, 800.0, 600.0));
        assert!(e.renders_pixels());
        assert_eq!(e.name(), "mado-nami");
    }

    #[test]
    fn resize_relays_out_from_cached_html() {
        let mut b = backend();
        b.render_html("<style>div{background-color:#00ff00;width:100px;height:40px}</style><div></div>");
        let _ = b.display_list(); // consume
        // A resize re-lays out from the cached HTML (no re-render_html call).
        b.resize(400, 300);
        assert!(!b.display_list().is_empty(), "resize should re-layout the cached page");
        assert_eq!(b.size(), (400, 300));
    }
}
