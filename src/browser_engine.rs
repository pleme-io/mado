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
use nami_core::dom::Document;
use nami_core::dom::{Node, NodeData};
use nami_core::engine::{BrowserEngine, ContentRect};
use nami_core::layout::{LayoutEngine, Size};
use nami_core::net::FetchClient;
use nami_core::paint::{self, DisplayList};
use url::Url;

use mado::float::{BackendError, BrowserBackend, LoadState, RenderedFrame};

/// What the live DOM was authored from — the source a resize re-runs the
/// pipeline over.
///
/// Typed rather than the bare `Option<String>` of HTML this used to be, because
/// that shape could only describe the HTML half: a DOM pushed in via
/// [`MadoNamiEngine::set_dom_sexp`] left it `None`, so a later resize had
/// nothing to re-lay out from and silently kept the old-size layout while the
/// panel stretched. With the author's own input kept per variant, EVERY way of
/// authoring a page has a re-layout source, and adding a third author path
/// means adding a variant here rather than discovering another silent hole.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageSource {
    /// Nothing rendered yet, or the last `navigate` failed — nothing to re-lay out.
    Empty,
    /// Authored from HTML (`render_html`, or a `navigate` that fetched a body).
    Html(String),
    /// Authored from a tatara-lisp DOM sexp (`set_dom_sexp`).
    Sexp(String),
}

/// The pure-Rust pixel-painting engine — mado's port of nami-native over
/// nami-core's public pipeline. Caches the display list (rebuilt only on
/// navigate/resize, never per frame).
pub struct MadoNamiEngine {
    content_rect: ContentRect,
    display_list: DisplayList,
    /// The re-layout source — see [`PageSource`].
    source: PageSource,
    /// Whether the last `navigate` fetched + rendered content (vs failed).
    navigate_ok: bool,
    /// The page's DOM as a homoiconic tatara-lisp S-expression (nami-core
    /// `dom_to_sexp`), recomputed on each render — the read side of "The DOM
    /// Way of the Browser": the same tree an operator/vigy transform rewrites.
    last_sexp: String,
}

impl MadoNamiEngine {
    /// Construct with a starting content rect; the display list is empty until
    /// the first `navigate`/`render_html`.
    #[must_use]
    pub fn new(content_rect: ContentRect) -> Self {
        Self {
            content_rect,
            display_list: DisplayList::default(),
            source: PageSource::Empty,
            navigate_ok: false,
            last_sexp: String::new(),
        }
    }

    /// The page DOM as a tatara-lisp S-expression (empty until first render).
    #[must_use]
    pub fn dom_sexp(&self) -> &str {
        &self.last_sexp
    }

    /// Run the parse → cascade → layout → display-list pipeline on a static
    /// HTML string and cache the result. No network — the offline half of
    /// `navigate` (and the path tests + direct-HTML consumers use). Uses the
    /// no-measure `LayoutEngine::compute` (single-line floor), so no
    /// cosmic-text measurer is needed.
    pub fn render_html(&mut self, html: &str) {
        let mut doc = Document::parse(html);
        expand_inline_lisp(&mut doc);
        self.render_document(&doc);
        self.source = PageSource::Html(html.to_owned());
    }

    /// Set the DOM from a tatara-lisp S-expression — the write-side inverse of
    /// [`Self::dom_sexp`], completing the DOM-Way read↔write loop: read the DOM
    /// as a homoiconic sexp, rewrite it, push it back live. Inline `<lisp>` in
    /// the sexp is expanded too (the same runtime fluidity `render_html` gives
    /// HTML). Returns a typed `Err` (never a silent wrong render) when the sexp
    /// doesn't parse.
    pub fn set_dom_sexp(&mut self, sexp: &str) -> Result<(), String> {
        let mut doc = nami_core::lisp::sexp_to_dom(sexp)?;
        expand_inline_lisp(&mut doc);
        self.render_document(&doc);
        // Authored from a sexp, not HTML — record the sexp as the re-layout
        // source so a later resize re-runs the pipeline from it. (This used to
        // clear an HTML-only field and claim the sexp path existed; it did not,
        // and a resize after a set_dom silently kept the old-size layout.)
        self.source = PageSource::Sexp(sexp.to_owned());
        Ok(())
    }

    /// Re-run the whole pipeline at a new content rect, from whatever the live
    /// DOM was authored from. Returns `true` when a re-layout actually happened
    /// — i.e. there WAS a source — which is the caller's staleness signal for
    /// its cached display list / rasterized texture.
    ///
    /// Inherent (and separately named) rather than the `BrowserEngine::resize`
    /// trait method, because the trait's `-> ()` cannot carry that signal;
    /// `resize` delegates here and drops it.
    pub fn relayout_at(&mut self, rect: ContentRect) -> bool {
        self.content_rect = rect;
        // Moved out so the re-render can borrow `self` mutably; each arm either
        // re-authors the source or puts it back.
        match std::mem::replace(&mut self.source, PageSource::Empty) {
            PageSource::Empty => false,
            PageSource::Html(html) => {
                self.render_html(&html);
                true
            }
            PageSource::Sexp(sexp) => match self.set_dom_sexp(&sexp) {
                Ok(()) => true,
                Err(e) => {
                    // It parsed once already, so this is not expected. Keep the
                    // prior page + the source (a later resize retries) rather
                    // than blanking the panel.
                    tracing::warn!(error = %e, "mado-nami: resize re-layout from the DOM sexp failed");
                    self.source = PageSource::Sexp(sexp);
                    false
                }
            },
        }
    }

    /// The shared cascade → layout → paint → sexp tail: turn a (post-expansion)
    /// `Document` into the cached display list + homoiconic sexp read. The one
    /// render path both `render_html` and `set_dom_sexp` funnel through — the
    /// pipeline lives once.
    fn render_document(&mut self, doc: &Document) {
        let css = collect_style_text(&doc.root);
        let mut resolver = StyleResolver::new();
        if !css.trim().is_empty() {
            match StyleSheet::parse(&css) {
                Ok(sheet) => resolver.add_sheet(sheet),
                Err(e) => tracing::warn!(error = %e, "mado-nami: <style> parse failed"),
            }
        }
        let styled = resolver.resolve(doc);
        let viewport = Size::new(self.content_rect.width, self.content_rect.height);
        let mut engine = LayoutEngine::new();
        let layout = engine.compute(&styled, viewport);
        self.display_list = paint::build_display_list(&layout, &styled, doc);
        // The homoiconic read: the post-expansion DOM as a tatara-lisp sexp.
        self.last_sexp = nami_core::lisp::dom_to_sexp(doc);
    }
}

/// Runtime fluidity: macroexpand inline tatara-lisp in the page tree
/// (conditionals / loops / interpolation) BEFORE cascade + layout — DOM
/// manipulation AS lisp. A no-op for a tree with no inline lisp, so ordinary
/// HTML/sexp is unaffected; the homoiconic manipulation surface for pages that
/// carry it ("The DOM Way of the Browser").
fn expand_inline_lisp(doc: &mut Document) {
    let evaluator = nami_core::eval::NamiEvaluator::new();
    let _report = nami_core::inline_lisp::expand(doc, &evaluator);
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
        // The trait returns (); the re-laid-out signal is on `relayout_at`,
        // which the backend calls directly.
        let _ = self.relayout_at(rect);
    }

    fn renders_pixels(&self) -> bool {
        true
    }

    fn display_list(&self) -> &DisplayList {
        &self.display_list
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
/// The non-blank card rendered while an off-thread fetch is in flight.
const BROWSER_LOADING_HTML: &str = "<style>div{background-color:#2e3440;width:420px;height:80px} p{color:#88c0d0;height:24px}</style><div><p>loading…</p></div>";
/// The typed error card (the actual error goes to the log + `load_state`, never
/// into built HTML — TYPED EMISSION).
const BROWSER_ERROR_HTML: &str = "<style>div{background-color:#3b1f24;width:520px;height:100px} p{color:#bf616a;height:24px}</style><div><p>page load failed</p></div>";

/// One live floating browser surface's non-geometry state (the geometry, focus,
/// z-order + drag FSM all live in `float::FloatFocus`, keyed by the same
/// `BrowserId`). Promoted from a bare tuple so each debt paydown APPENDS a field
/// rather than re-churning a tuple across every call site (the collision-safe
/// shape).
pub struct BrowserSurface {
    /// The live engine.
    pub backend: RealBrowserBackend,
    /// The current URL.
    pub url: String,
    /// The fetch epoch — bumped on each navigate; the stale-guard that drops a
    /// slow out-of-order fetch superseded by a newer one.
    pub fetch_epoch: u64,
}

pub struct RealBrowserBackend {
    engine: MadoNamiEngine,
    load: LoadState,
    clock: f64,
    width: u32,
    height: u32,
    /// The last built display list, cached (shared) so the GPU layer can
    /// re-rasterize it on demand without a re-parse.
    last_list: std::sync::Arc<DisplayList>,
    /// Bumped on every content change (render_html / successful navigate) — the
    /// renderer re-renders the panel texture only when this moves.
    content_seqno: u64,
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
            last_list: std::sync::Arc::new(DisplayList::default()),
            content_seqno: 0,
        }
    }

    /// Render HTML offline (no network), cache the display list, bump the
    /// content seqno, + mark loaded. For feeding HTML directly + tests.
    pub fn render_html(&mut self, html: &str) {
        self.engine.render_html(html);
        self.last_list = std::sync::Arc::new(self.engine.display_list().clone());
        self.content_seqno = self.content_seqno.wrapping_add(1);
        self.load = LoadState::Loaded;
    }

    /// Set the DOM from a tatara-lisp S-expression (the write-side of the
    /// DOM-Way loop), cache the new list, bump the seqno, mark loaded. Returns a
    /// typed `Err` when the sexp doesn't parse — the surface keeps its prior
    /// page, never a silent blank.
    pub fn set_dom_sexp(&mut self, sexp: &str) -> Result<(), String> {
        self.engine.set_dom_sexp(sexp)?;
        self.last_list = std::sync::Arc::new(self.engine.display_list().clone());
        self.content_seqno = self.content_seqno.wrapping_add(1);
        self.load = LoadState::Loaded;
        Ok(())
    }

    /// Show a non-blank "loading" card + mark `Loading` — rendered while an
    /// off-thread fetch is in flight (so the panel is never blank).
    pub fn set_loading(&mut self) {
        self.render_html(BROWSER_LOADING_HTML);
        self.load = LoadState::Loading;
    }

    /// Render a typed error card + mark `Failed`. TYPED EMISSION: the message
    /// is NOT built into HTML (`format!()` of markup is banned) — it surfaces
    /// via `load_state()` + a `tracing` log; the card itself is a const.
    pub fn render_error(&mut self, msg: &str) {
        tracing::warn!(error = %msg, "browser: page load failed");
        self.render_html(BROWSER_ERROR_HTML);
        self.load = LoadState::Failed;
    }

    /// The current cached display list (shared) — the GPU layer rasterizes this
    /// into the panel texture. Not consumed; stable until the next content change.
    #[must_use]
    pub fn current_display_list(&self) -> std::sync::Arc<DisplayList> {
        std::sync::Arc::clone(&self.last_list)
    }

    /// The content-change counter — the renderer re-rasterizes only when it moves.
    #[must_use]
    pub fn content_seqno(&self) -> u64 {
        self.content_seqno
    }

    /// The page DOM as a homoiconic tatara-lisp S-expression (`dom_to_sexp`) —
    /// the read side of the DOM-manipulation surface (queried via
    /// `(mado-browser-dom-sexp id)` + `browser_dom_sexp`).
    #[must_use]
    pub fn dom_sexp(&self) -> String {
        self.engine.dom_sexp().to_owned()
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
            self.last_list = std::sync::Arc::new(self.engine.display_list().clone());
            self.content_seqno = self.content_seqno.wrapping_add(1);
            self.load = LoadState::Loaded;
            Ok(())
        } else {
            self.load = LoadState::Failed;
            Err(BackendError::Load(url.to_string()))
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if (width, height) == (self.width, self.height) {
            // A no-op resize (e.g. snapping to the zone already occupied) must
            // not move the seqno — that would re-rasterize an identical page.
            return;
        }
        self.width = width;
        self.height = height;
        #[allow(clippy::cast_precision_loss)]
        let rect = ContentRect::new(0.0, 0.0, width as f32, height as f32);
        // A re-layout invalidates BOTH cached artifacts: the display list the
        // GPU layer rasterizes, and the seqno it keys that raster on. Refreshing
        // neither is why a resize used to stretch the old-size bitmap — the
        // engine re-laid out and nothing downstream was ever told.
        if self.engine.relayout_at(rect) {
            self.last_list = std::sync::Arc::new(self.engine.display_list().clone());
            self.content_seqno = self.content_seqno.wrapping_add(1);
        }
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

/// The bare offscreen `DisplayList` → RGBA rasterizer. Now the ONE shared
/// `nami_core::gpu` translator (Op#1 solve-once — mado + namimado both consume
/// it, `pending-shared-content-translator` closed). Re-exported so the render-
/// side call sites keep resolving `crate::browser_engine::render_display_list_to_rgba`.
pub use nami_core::gpu::render_display_list_to_rgba;

#[cfg(test)]
mod tests {
    use super::*;
    use nami_core::paint::DrawCmd;

    fn backend() -> RealBrowserBackend {
        RealBrowserBackend::nami_native(800, 600)
    }

    /// A page whose only box is an auto-width block, so its painted width is
    /// the content width (minus the default body margin) — i.e. the geometry
    /// MOVES when the surface is re-laid out at a new size, and does not when
    /// the old-size list is merely re-read.
    const AUTO_WIDTH_PAGE: &str =
        "<style>div{background-color:#00ff00;height:40px}</style><div></div>";

    /// The width of the first background `Rect` — the observable "this was laid
    /// out at that viewport width" signal used by the resize tests.
    fn first_rect_width(dl: &DisplayList) -> f32 {
        dl.cmds
            .iter()
            .find_map(|c| match c {
                DrawCmd::Rect { width, .. } => Some(*width),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a background Rect: {:?}", dl.cmds))
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
        let dl = b.current_display_list();
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
    fn content_seqno_bumps_on_each_render() {
        let mut b = backend();
        let s0 = b.content_seqno();
        b.render_html(
            "<style>div{background-color:#112233;width:50px;height:50px}</style><div></div>",
        );
        let s1 = b.content_seqno();
        assert_ne!(s0, s1, "seqno must bump on render");
        // The cached list is stable (not consumed) across reads.
        assert!(!b.current_display_list().is_empty());
        assert!(!b.current_display_list().is_empty());
        assert_eq!(
            b.content_seqno(),
            s1,
            "reading the list must not bump the seqno"
        );
    }

    #[test]
    fn eval_is_unsupported_never_silent() {
        let mut b = backend();
        assert_eq!(
            b.eval("1+1"),
            Err(BackendError::Unsupported("javascript eval"))
        );
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
    fn resize_relays_out_from_cached_html_and_bumps_the_seqno() {
        let mut b = backend();
        b.render_html(AUTO_WIDTH_PAGE);
        let seq0 = b.content_seqno();
        let w0 = first_rect_width(&b.current_display_list());

        // A resize re-lays out from the cached HTML (no re-render_html call).
        b.resize(400, 300);

        // The staleness signal the renderer keys its texture cache on MUST move,
        // or the panel keeps stretching the old-size bitmap forever.
        assert_ne!(
            b.content_seqno(),
            seq0,
            "a resize must bump the content seqno — render.rs re-rasterizes only when it moves"
        );
        // …and the cached list must be the NEW-size layout, not the old one:
        // the auto-width block narrows by exactly the viewport delta.
        let w1 = first_rect_width(&b.current_display_list());
        assert!(
            w1 < w0,
            "the block must narrow with the viewport: {w0} -> {w1}"
        );
        assert!(
            (w0 - w1 - 400.0).abs() < 1.0,
            "the re-layout must track the 800->400 viewport delta: {w0} -> {w1}"
        );
        assert_eq!(b.size(), (400, 300));
    }

    #[test]
    fn resize_after_set_dom_sexp_still_relays_out() {
        // The sexp-authored DOM is a first-class page: a resize must re-run the
        // pipeline from the sexp exactly as it does from HTML. (It could not
        // before — the only re-layout source was an HTML field that set_dom_sexp
        // cleared, so this resize was a silent no-op.)
        let mut b = backend();
        b.render_html(AUTO_WIDTH_PAGE);
        let sexp = b.dom_sexp();
        b.set_dom_sexp(&sexp)
            .expect("the read-back DOM sexp must set cleanly");

        let seq0 = b.content_seqno();
        let w0 = first_rect_width(&b.current_display_list());

        b.resize(400, 300);

        assert_ne!(
            b.content_seqno(),
            seq0,
            "a resize after set_dom_sexp must bump the content seqno too"
        );
        let w1 = first_rect_width(&b.current_display_list());
        assert!(
            (w0 - w1 - 400.0).abs() < 1.0,
            "the sexp page must re-lay out at the new width: {w0} -> {w1}"
        );
    }

    #[test]
    fn a_no_op_resize_leaves_the_seqno_alone() {
        // Re-rasterizing an identical page is pure waste, so the signal moves on
        // a real size change only.
        let mut b = backend();
        b.render_html(AUTO_WIDTH_PAGE);
        let seq0 = b.content_seqno();
        b.resize(800, 600);
        assert_eq!(b.content_seqno(), seq0, "same size = no content change");
    }

    #[test]
    fn resizing_a_blank_surface_leaves_the_seqno_alone() {
        // Nothing was ever authored, so there is nothing to re-lay out and no
        // stale raster to invalidate.
        let mut b = backend();
        let seq0 = b.content_seqno();
        b.resize(400, 300);
        assert_eq!(b.content_seqno(), seq0, "no page = nothing to invalidate");
        assert_eq!(b.size(), (400, 300));
    }

    #[test]
    fn set_dom_sexp_round_trips_from_the_dom_read() {
        // The DOM-Way read↔write loop: render → read the DOM as a tatara-lisp
        // sexp → push it back via set_dom_sexp → still a live, non-empty page.
        let mut b = backend();
        b.render_html(
            "<style>div{background-color:#00ff00;width:120px;height:40px}</style><div></div>",
        );
        let sexp = b.dom_sexp().to_owned();
        assert!(
            sexp.starts_with("(document"),
            "dom_sexp is a (document …) form: {sexp}"
        );
        b.set_dom_sexp(&sexp)
            .expect("the read-back DOM sexp must set cleanly");
        assert!(
            !b.current_display_list().is_empty(),
            "set_dom re-renders a non-empty display list"
        );
    }

    #[test]
    fn set_dom_sexp_rejects_a_malformed_sexp_without_blanking() {
        let mut b = backend();
        b.render_html("<div></div>");
        let before_len = b.current_display_list().cmds.len();
        assert!(
            b.set_dom_sexp("not an s-expression").is_err(),
            "a malformed sexp must be a typed Err"
        );
        // The prior page is untouched — a bad sexp never silently blanks it.
        assert_eq!(b.current_display_list().cmds.len(), before_len);
    }

    // GPU golden — needs an adapter. Gated on `gpu_tests` (cid + dev
    // workstations have one; CI runners typically don't) so plain `cargo test`
    // never needs a GPU. The empirical proof that real page pixels reach the
    // panel texture — the layer below render_html_builds_a_nonempty_display_list.
    #[cfg(feature = "gpu_tests")]
    mod gpu {
        use super::*;

        #[test]
        fn page_render_paints_the_blue_div_and_is_not_a_uniform_fill() {
            let gpu = pollster::block_on(garasu::GpuContext::new()).expect("GPU adapter");
            let mut b = backend();
            b.render_html(
                "<style>div{background-color:#3050ff;width:200px;height:100px} \
                 p{color:#ffffff;height:30px}</style><div><p>Hello</p></div>",
            );
            let dl = b.current_display_list();
            let bg = [0.03_f32, 0.04, 0.06, 1.0];
            let fg = [0.85_f32, 0.87, 0.91, 1.0];
            let rgba = render_display_list_to_rgba(&gpu, &dl, 400, 300, bg, fg);
            assert_eq!(rgba.len(), 400 * 300 * 4, "tightly-packed RGBA8");
            // A blue div pixel exists — b channel high, r/g low (≈#3050ff).
            let has_blue = rgba
                .chunks_exact(4)
                .any(|px| px[2] > 180 && px[0] < 120 && px[1] < 120);
            assert!(
                has_blue,
                "expected a blue div pixel — real page pixels reached the texture"
            );
            // The page is NOT a uniform fill (something painted over the clear).
            let first = &rgba[0..4];
            let uniform = rgba.chunks_exact(4).all(|px| px == first);
            assert!(!uniform, "the rendered page must not be a uniform fill");
        }
    }
}
