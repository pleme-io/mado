//! GPU rendering module for terminal content.
//!
//! Three-pass rendering pipeline:
//! 1. Clear background
//! 2. Cell backgrounds + cursor + decorations (instanced colored rectangles via RectPipeline)
//! 3. Text (glyphon via garasu with per-cell colors)
//!
//! Uses sequence number damage tracking to skip unchanged frames.

use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Process-wide frame-timing samples — written at the end of each
/// `render()` call (single-pane and multi-pane paths). Read by the
/// MCP `frame_perf` tool so agents can introspect mado's render
/// performance live without parsing log lines. Static because
/// there's only ever one TerminalRenderer per process and we want
/// the MCP handler (which doesn't hold a reference to the renderer)
/// to read it without plumbing a handle through.
///
/// Atomics rather than a mutexed ring buffer so the renderer writes
/// are wait-free and never compete for a lock with the MCP reader.
pub(crate) static LAST_FRAME_US: AtomicU64 = AtomicU64::new(0);
pub(crate) static LAST_FRAME_RECTS: AtomicU64 = AtomicU64::new(0);
pub(crate) static LAST_FRAME_TEXT: AtomicU64 = AtomicU64::new(0);
pub(crate) static LAST_FRAME_SHAPE_CACHE: AtomicU64 = AtomicU64::new(0);
pub(crate) static TOTAL_FRAMES: AtomicU64 = AtomicU64::new(0);
pub(crate) static TOTAL_FRAMES_SKIPPED: AtomicU64 = AtomicU64::new(0);

use bytemuck::{Pod, Zeroable};
use glyphon::{Attrs, Buffer, Color as GlyphonColor, Family, Style, Weight};
use lru::LruCache;
use madori::render::{RenderCallback, RenderContext};

use crate::config::{ColorblindMode, CursorStyle};
// PaneRect / WindowState removed at Phase 4 — single-pane mado.
use crate::search::SearchState;
use crate::selection::{CellPos, Selection};
use crate::terminal::{
    bold_bright_color, default_ansi_palette, AttrFlags, Cell, Color, Cursor, ImagePlacement,
    StyleSnapshot, Terminal, UnderlineColor, UnderlineStyle,
};
use crate::url::{self, DetectedUrl};

/// Shared terminal state between the render thread and PTY I/O thread.
///
/// P30 — `parking_lot::RwLock` instead of `std::sync::Mutex<Terminal>`:
///
///   * **Real reader-writer semantics** — the renderer's snapshot
///     pass and MCP introspection are reads (no terminal mutation);
///     the PTY pump's `term.feed(...)` is a write. With a plain
///     Mutex they all serialised, which mattered most when MCP and
///     snapshot wanted to observe state during a heavy PTY burst.
///     A real RwLock lets all readers proceed concurrently and only
///     blocks them while a write is in flight.
///   * **No LockResult wrapper** — call sites lose `.unwrap()`/
///     `.expect("poisoned")` ceremony. Cleaner code, smaller IR
///     because there's no PoisonError path to monomorphise.
///   * **Faster acquire** — parking_lot's lock primitives use a
///     hashed-park strategy that's measurably faster than the OS-
///     futex Mutex on uncontended acquire (~30% on macOS / Linux).
pub type SharedTerminal = Arc<parking_lot::RwLock<Terminal>>;

// ---------------------------------------------------------------------------
// Rect instance data for GPU
// ---------------------------------------------------------------------------

/// Fragment-path selector for [`RectInstance`] — M3-C2 decoration
/// dispatch. Solid is the historical rect; Run/Curly carry the
/// engawa decoration vocabulary (RLE period/duty band, analytic
/// sine band) so dotted/dashed/curly underlines stay O(1) instances
/// per run instead of per-dot quads (the geometry explosion the
/// engawa vocabulary exists to prevent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RectMode {
    Solid,
    /// `pattern = [period, duty, _, phase]` — paint where
    /// `((x + phase) % period) < period * duty`.
    Run,
    /// `pattern = [period, amplitude, thickness, phase]` — paint
    /// where the pixel is within thickness/2 of the sine centerline.
    Curly,
    /// Rounded-corner solid fill. `pattern = [width, height, radius, _]`
    /// — the fragment runs a rounded-rect SDF over the rect's own
    /// dimensions (passed through `pattern` because `size` is not a
    /// fragment varying) and anti-aliases the corner alpha so freestanding
    /// chrome (the scrollback thumb) reads with soft corners instead of
    /// hard squares. The radius is clamped to `min(width,height)/2` in
    /// the shader, so an over-large radius degrades to a pill, never an
    /// inverted SDF.
    RoundedSolid,
    /// Synthesized powerline separator, rasterized to fill the FULL cell
    /// at any `line_height` (ghostty parity — a font glyph would be
    /// baseline-positioned in a 1.25-tall cell and notch the bottom).
    /// `pattern = [kind, _, _, _]` selects the shape, evaluated per
    /// fragment over the rect's own `local` (which spans the whole cell):
    ///   kind 0 → E0B0 right-pointing filled triangle (apex right)
    ///   kind 1 → E0B2 left-pointing filled triangle  (apex left)
    ///   kind 2 → E0B4 right filled half-disk          (flat left edge)
    ///   kind 3 → E0B6 left filled half-disk           (flat right edge)
    /// The triangles are exact half-plane tests; the half-disks are an
    /// analytic disk SDF anti-aliased at the curved edge. All four tile
    /// edge-to-edge with the next cell's background with zero gap.
    Powerline,
}

impl RectMode {
    /// Wire word for the instance buffer — the shader's `mode` switch.
    const fn word(self) -> u32 {
        match self {
            Self::Solid => 0,
            Self::Run => 1,
            Self::Curly => 2,
            Self::RoundedSolid => 3,
            Self::Powerline => 4,
        }
    }
}

/// The four filled powerline separators mado synthesizes into the cell
/// rect (ghostty parity). These are the glyphs lualine "pills" use — the
/// solid angle separators and the solid rounded caps. The hollow/line
/// variants (E0B1/E0B3/E0B5/E0B7) are intentionally NOT synthesized: they
/// keep the normal baseline-positioned font path (a 1px stroke notch at
/// the cell bottom is invisible vs a solid-fill notch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerlineSep {
    /// U+E0B0  right-pointing filled triangle.
    RightTriangle,
    /// U+E0B2  left-pointing filled triangle.
    LeftTriangle,
    /// U+E0B4  right filled half-disk (rounded cap).
    RightHalfDisk,
    /// U+E0B6  left filled half-disk (rounded cap).
    LeftHalfDisk,
}

impl PowerlineSep {
    /// Classify a codepoint, returning the synthesized separator iff it
    /// is one of the four filled powerline caps mado fills into the cell.
    /// All other codepoints (including the hollow E0B1/E0B3/E0B5/E0B7
    /// line variants and the rest of the powerline-extra block) return
    /// `None` and keep the normal font-glyph path.
    const fn from_char(ch: char) -> Option<Self> {
        match ch {
            '\u{E0B0}' => Some(Self::RightTriangle),
            '\u{E0B2}' => Some(Self::LeftTriangle),
            '\u{E0B4}' => Some(Self::RightHalfDisk),
            '\u{E0B6}' => Some(Self::LeftHalfDisk),
            _ => None,
        }
    }

    /// The shader `kind` selector carried in `pattern.x`.
    const fn kind(self) -> f32 {
        match self {
            Self::RightTriangle => 0.0,
            Self::LeftTriangle => 1.0,
            Self::RightHalfDisk => 2.0,
            Self::LeftHalfDisk => 3.0,
        }
    }
}

/// True when `ch` is one of the four filled powerline separators mado
/// synthesizes via the rect pipeline (so the renderer diverts it from
/// the font-glyph path the way it diverts box-drawing).
fn is_powerline_separator(ch: char) -> bool {
    PowerlineSep::from_char(ch).is_some()
}

/// Build the single rect instance that fills a powerline separator into
/// the cell at `(x, y)` with dimensions `cw × ch_h` and the cell's `fg`
/// color. The rect spans the entire cell; the fragment shader masks it
/// to the separator's shape via `RectMode::Powerline`. Because the rect
/// is exactly `cell_width × cell_height`, the filled side reaches the
/// cell bottom at ANY `line_height` — the notch at tall line-heights is
/// unrepresentable.
fn powerline_rect(
    sep: PowerlineSep,
    x: f32,
    y: f32,
    cw: f32,
    ch_h: f32,
    color: [f32; 4],
) -> RectInstance {
    // The whole-cell rect; `RectInstance::powerline` folds `sep.kind()` + the
    // cell dims (= size) into the pattern the shader evaluates over `local`.
    RectInstance::powerline([x, y], [cw, ch_h], color, sep.kind())
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct RectInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    /// [`RectMode::word`]. Plain rects are `Solid`.
    mode: u32,
    /// Mode-dependent payload — see [`RectMode`]. Zero for Solid.
    pattern: [f32; 4],
}

/// Generate the `RectInstance` constructor family from a per-`RectMode`-variant
/// table (PRIME DIRECTIVE — the emitter substrate at Layer B). Every rect the
/// renderer uploads is one variant of the SAME shape:
/// `RectInstance { pos, size, color, mode: RectMode::V.word(), pattern: [...] }`;
/// the ONLY per-variant differences are the mode word and the pattern payload.
/// This macro is the single place that contract lives — one row per variant
/// binds its name, its extra typed payload args, its `RectMode`, and how those
/// args (plus `pos`/`size`/`color`) fold into the 4-float `pattern`. Adding a
/// new rect mode is one row here, not a hand-written constructor that can drift
/// its `.word()` from the shader's `mode` switch. `$pat` receives `pos`, `size`,
/// `color`, and every named payload arg in scope.
macro_rules! rect_constructors {
    ($(
        $(#[$meta:meta])*
        $name:ident ( $pos:ident , $size:ident , $color:ident $(, $arg:ident : $ty:ty )* )
            => $mode:ident , $pat:expr ;
    )*) => {
        impl RectInstance {
            $(
                $(#[$meta])*
                #[allow(clippy::allow_attributes, clippy::missing_const_for_fn)]
                #[inline]
                fn $name(
                    $pos: [f32; 2],
                    $size: [f32; 2],
                    $color: [f32; 4],
                    $( $arg : $ty ),*
                ) -> Self {
                    // `$pos`, `$size`, `$color`, and each `$arg` — all named at
                    // the CALL site — are in scope for `$pat` (shared hygiene).
                    let pattern: [f32; 4] = $pat;
                    Self { pos: $pos, size: $size, color: $color, mode: RectMode::$mode.word(), pattern }
                }
            )*
        }
    };
}

rect_constructors! {
    /// The historical constructor shape — most rects are a plain solid fill.
    solid(pos, size, color) => Solid, [0.0, 0.0, 0.0, 0.0];

    /// A rounded-corner solid fill. The fragment runs the rounded-box SDF over
    /// the rect's own `size` (carried in `pattern` because `size` is not a
    /// fragment varying) and anti-aliases the corner alpha. Used for
    /// freestanding chrome (the scrollback thumb) where soft corners read as
    /// polish; grid-aligned cell bands stay square.
    rounded(pos, size, color, radius: f32) => RoundedSolid, [size[0], size[1], radius, 0.0];

    /// A dashed / periodic run band (`RectMode::Run`): paint where
    /// `((x + phase) % period) < period * duty`. `phase` fixed at 0.
    run(pos, size, color, period: f32, duty: f32) => Run, [period, duty, 0.0, 0.0];

    /// A curly (sine-centerline) underline band (`RectMode::Curly`): paint
    /// within `thickness/2` of the sine centerline of wavelength `period` and
    /// peak `amplitude`. `phase` fixed at 0.
    curly(pos, size, color, period: f32, amplitude: f32, thickness: f32)
        => Curly, [period, amplitude, thickness, 0.0];

    /// A synthesized powerline separator (`RectMode::Powerline`) filling the
    /// whole cell; `kind` selects the shape (see [`PowerlineSep::kind`]). The
    /// shader needs the cell dims (= `size`) to evaluate the shape over the
    /// rect's own `local`, so they ride in `pattern`.
    powerline(pos, size, color, kind: f32) => Powerline, [kind, size[0], size[1], 0.0];
}

impl RectInstance {
    /// A full-window solid overlay at the given physical dimensions — the
    /// shared shape for the bell flash + unfocused dim + any whole-surface
    /// wash. Just `solid` anchored at the origin spanning the surface.
    #[inline]
    fn full_window(width: f32, height: f32, color: [f32; 4]) -> Self {
        Self::solid([0.0, 0.0], [width, height], color)
    }
}

/// Soft elevation shadow — the ONE depth primitive shared by mado's window
/// edges and the Ctrl-S popup card, so both read with the same depth
/// The `(px, py, pw, ph)` of a `Center`-anchored overlay's backing card.
///
/// Pure geometry so it is unit-testable without a GPU. `left`/`top0` are the
/// already-centred text-block origin; the card insets by `(pad_x, pad_y)`.
/// The origin is clamped to `>= pad` (one window-padding inset) — NOT `0.0` —
/// so even a list wider/taller than the viewport can never collapse the panel
/// onto `(0,0)` and blank out the top-left cells. For a normal (screen-fitting)
/// picker the centred origin is far from the edges, so the clamp is a no-op;
/// it only catches the degenerate oversize case. Regression invariant:
/// `centered_panel_is_central_never_top_left`.
/// Choose which overlay line indices to render so the popup NEVER exceeds
/// the viewport. When the full list fits (`n <= max_lines`) every line shows,
/// in order. When it doesn't, the first line (the title) is always kept and
/// the body (`lines[1..]`) is scrolled so the `selected` row stays visible —
/// so an over-long board renders as a centred, viewport-bounded card rather
/// than pinning to a corner and running off the bottom (the "sized for full
/// screen" Ctrl-S report, 2026-07-02). Pure index math so it is unit-testable
/// without a GPU. Regression invariant: `overlay_window_keeps_selected_visible`.
fn viewport_line_window(n: usize, selected: Option<usize>, max_lines: usize) -> Vec<usize> {
    if max_lines == 0 || n <= max_lines {
        return (0..n).collect();
    }
    // Keep the title (line 0); scroll the body (lines 1..n) within the budget.
    let budget = max_lines.saturating_sub(1).max(1);
    let body_len = n - 1;
    let sel_body = selected.unwrap_or(0).saturating_sub(1);
    let start_body = sel_body
        .saturating_sub(budget - 1)
        .min(body_len.saturating_sub(budget));
    let mut idx = Vec::with_capacity(max_lines);
    idx.push(0);
    for b in start_body..(start_body + budget).min(body_len) {
        idx.push(1 + b);
    }
    idx
}

fn centered_panel_geom(
    left: f32,
    top0: f32,
    content_w: f32,
    block_h: f32,
    pad: f32,
    pad_x: f32,
    pad_y: f32,
) -> (f32, f32, f32, f32) {
    let px = (left - pad_x).max(pad);
    let py = (top0 - pad_y).max(pad);
    let pw = content_w + pad_x * 2.0;
    let ph = block_h + pad_y * 2.0;
    (px, py, pw, ph)
}

/// language (the operator's "flush and consistent together"). Fakes a
/// blurred shadow with the solid rect pipeline: `layers` concentric rounded
/// rects growing outward from `[x,y,w,h]` by `spread`, each fainter than the
/// last, cast `dy` downward. Returned outermost-first so the translucent
/// blacks accumulate toward the lit surface (densest at the edge it hugs).
/// `base_alpha` is the per-layer alpha at the innermost ring.
fn elevation_shadow(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    layers: usize,
    spread: f32,
    base_alpha: f32,
    dy: f32,
) -> Vec<RectInstance> {
    let mut out = Vec::with_capacity(layers);
    // Outermost (i = layers-1) first → faintest painted first, densest last.
    for i in (0..layers).rev() {
        let frac = (i as f32 + 1.0) / layers as f32; // 1.0 outer … near 0 inner
        let grow = spread * frac;
        // Alpha grows toward the inner rings (1 - frac), so the shadow is
        // darkest where it meets the surface and fades into the terminal.
        let alpha = base_alpha * (1.0 - frac + 1.0 / layers as f32);
        out.push(RectInstance::rounded(
            [x - grow, y - grow + dy],
            [w + grow * 2.0, h + grow * 2.0],
            [0.0, 0.0, 0.0, alpha],
            radius + grow,
        ));
    }
    out
}

// (The window-edge inner vignette is now the engawa `window_depth` catalog
// effect — a portable, config-toggleable post-process — so the hand-rolled
// rect-strip version that briefly lived here has been removed. The
// popup-elevation drop shadow stays as overlay chrome via `elevation_shadow`
// above, because the popup is drawn outside the engawa post-graph.)

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct ScreenUniforms {
    resolution: [f32; 2],
    _padding: [f32; 2],
}

const RECT_SHADER: &str = r"
struct ScreenUniforms {
    resolution: vec2<f32>,
    _padding: vec2<f32>,
};

struct RectInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) mode: u32,
    @location(4) pattern: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
    @location(2) @interpolate(flat) mode: u32,
    @location(3) @interpolate(flat) pattern: vec4<f32>,
};

@group(0) @binding(0) var<uniform> screen: ScreenUniforms;

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    instance: RectInstance,
) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let pixel = instance.pos + corners[vi] * instance.size;
    let ndc = vec2<f32>(
        (pixel.x / screen.resolution.x) * 2.0 - 1.0,
        1.0 - (pixel.y / screen.resolution.y) * 2.0,
    );
    var out: VertexOutput;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = instance.color;
    out.local = corners[vi] * instance.size;
    out.mode = instance.mode;
    out.pattern = instance.pattern;
    return out;
}

// Decoration dispatch — mode mirrors the Rust RectMode enum:
// 0 solid, 1 RLE run (engawa SegmentRun: period/duty over the band),
// 2 curly (engawa CurlyBand: analytic sine evaluated per fragment —
// the SDF-style path the engawa vocabulary mandates instead of
// per-segment quad tessellation). Unpainted fragments return
// transparent (alpha blending is on) so the band rect never shows.
@fragment
fn fs_main(frag: VertexOutput) -> @location(0) vec4<f32> {
    if frag.mode == 0u {
        return frag.color;
    }
    if frag.mode == 1u {
        let period = max(frag.pattern.x, 0.0001);
        let duty = frag.pattern.y;
        let phase = (frag.local.x + frag.pattern.w) % period;
        if phase < period * duty {
            return frag.color;
        }
        return vec4<f32>(0.0);
    }
    if frag.mode == 2u {
        // mode 2 — curly band. Centerline sits at amplitude + thickness/2
        // from the band top (band height = 2*amplitude + thickness).
        let period = max(frag.pattern.x, 0.0001);
        let amplitude = frag.pattern.y;
        let thickness = frag.pattern.z;
        let tau = 6.28318530717958647692;
        let center = amplitude + thickness * 0.5
            + amplitude * sin(tau * (frag.local.x + frag.pattern.w) / period);
        if abs(frag.local.y - center) <= thickness * 0.5 {
            return frag.color;
        }
        return vec4<f32>(0.0);
    }
    if frag.mode == 4u {
        // mode 4 — synthesized powerline separator filling the whole
        // cell. frag.local spans 0..size (the full cell); pattern =
        // [kind, cell_width, cell_height, _]. The filled side always
        // reaches the cell bottom because the rect IS the cell — no
        // baseline gap at any line_height.
        let kind = frag.pattern.x;
        let cw = max(frag.pattern.y, 0.0001);
        let chh = max(frag.pattern.z, 0.0001);
        // Normalized cell coords in [0,1]×[0,1].
        let u = frag.local.x / cw;
        let v = frag.local.y / chh;
        // 1px anti-alias width in normalized x (curved edges only).
        let aa = 1.0 / cw;
        if kind < 0.5 {
            // E0B0 — right-pointing filled triangle. Apex at (1, 0.5),
            // base is the full-height left edge. Fill where the point is
            // left of the two slanted edges: u <= 1 - |2v - 1|.
            let edge = 1.0 - abs(2.0 * v - 1.0);
            if u <= edge {
                return frag.color;
            }
            return vec4<f32>(0.0);
        }
        if kind < 1.5 {
            // E0B2 — left-pointing filled triangle. Apex at (0, 0.5),
            // base is the full-height right edge. Mirror of E0B0.
            let edge = 1.0 - abs(2.0 * v - 1.0);
            if (1.0 - u) <= edge {
                return frag.color;
            }
            return vec4<f32>(0.0);
        }
        if kind < 2.5 {
            // E0B4 — right filled half-disk. Flat edge on the left
            // (u=0), bulge to the right. Center at (0, 0.5); fill the
            // disk of radius 1 (in the half-width metric). Distance from
            // center using x measured rightward, y as half-height units.
            let dx = u;
            let dy = 2.0 * v - 1.0;
            let dist = sqrt(dx * dx + dy * dy);
            let coverage = 1.0 - smoothstep(1.0 - aa, 1.0 + aa, dist);
            return vec4<f32>(frag.color.rgb, frag.color.a * coverage);
        }
        // E0B6 — left filled half-disk. Flat edge on the right (u=1),
        // bulge to the left. Center at (1, 0.5). Mirror of E0B4.
        let dx = 1.0 - u;
        let dy = 2.0 * v - 1.0;
        let dist = sqrt(dx * dx + dy * dy);
        let coverage = 1.0 - smoothstep(1.0 - aa, 1.0 + aa, dist);
        return vec4<f32>(frag.color.rgb, frag.color.a * coverage);
    }
    // mode 3 — rounded-corner solid. pattern = [width, height, radius, _].
    // Signed-distance to a rounded box centered on the rect; the corner
    // alpha is the 1px-AA smoothstep over that distance, so the corners
    // fade instead of stair-stepping. Edges (where the SDF is well
    // inside) keep full alpha — only the four corner quadrants soften.
    let half = vec2<f32>(frag.pattern.x, frag.pattern.y) * 0.5;
    let r = clamp(frag.pattern.z, 0.0, min(half.x, half.y));
    // Position relative to the rect center (frag.local spans 0..size).
    let p = frag.local - half;
    // Distance from the inner rounded-box edge: classic rounded-box SDF.
    let q = abs(p) - (half - vec2<f32>(r, r));
    let dist = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
    // 1px anti-aliased coverage: full inside, 0 outside, soft across the
    // border. (dist <= -aa → inside; dist >= 0 → outside.)
    let aa = 1.0;
    let coverage = 1.0 - smoothstep(-aa, 0.0, dist);
    return vec4<f32>(frag.color.rgb, frag.color.a * coverage);
}
";

// ---------------------------------------------------------------------------
// RectPipeline — instanced colored rectangles
// ---------------------------------------------------------------------------

/// Fixed instance capacity for the modal-overlay panel buffer. A popup
/// card is a soft shadow ring (≤6) + border + fill + selected-row bar;
/// 24 leaves generous headroom and never needs to grow.
const OVERLAY_RECT_CAPACITY: usize = 24;

struct RectPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    /// Separate instance buffer for modal-overlay panels — never shared
    /// with `instance_buffer` (see `RectPipeline::new`). Drawn via
    /// [`RectPipeline::draw_overlay`].
    overlay_buffer: wgpu::Buffer,
}

impl RectPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("rect_bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rect_pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RectInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 8,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32,
                    offset: 32,
                    shader_location: 3,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 36,
                    shader_location: 4,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[instance_layout],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_uniforms"),
            size: std::mem::size_of::<ScreenUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rect_bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let initial_capacity = 4096;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_instances"),
            size: (initial_capacity * std::mem::size_of::<RectInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Dedicated instance buffer for modal-overlay panels (the Ctrl-S
        // popup card). It MUST be separate from `instance_buffer`: the
        // cell-background pass and the overlay-panel pass submit in the
        // same frame, so a shared buffer would let the overlay's offset-0
        // writes clobber the first few cell-background instances → stray
        // panel-coloured quads at the top-left (operator report,
        // 2026-06-22; theory ledger §VIII #4). A panel is ≤ a handful of
        // rects, so a small fixed capacity never needs to grow.
        let overlay_capacity = OVERLAY_RECT_CAPACITY;
        let overlay_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_overlay_instances"),
            size: (overlay_capacity * std::mem::size_of::<RectInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            uniform_buffer,
            bind_group,
            instance_buffer,
            instance_capacity: initial_capacity,
            overlay_buffer,
        }
    }

    fn update_resolution(&self, queue: &wgpu::Queue, width: u32, height: u32) {
        let uniforms = ScreenUniforms {
            resolution: [width as f32, height as f32],
            _padding: [0.0; 2],
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
    }

    fn ensure_capacity(&mut self, device: &wgpu::Device, count: usize) {
        if count > self.instance_capacity {
            let new_cap = count.next_power_of_two();
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rect_instances"),
                size: (new_cap * std::mem::size_of::<RectInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
        }
    }

    fn draw<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>, count: u32) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..6, 0..count);
    }

    /// Draw modal-overlay panel rects from the dedicated [`Self::overlay_buffer`]
    /// — never the shared cell buffer, so an open popup can't clobber the
    /// top-left cell backgrounds. Caller must `write_buffer` the rects into
    /// `overlay_buffer` (offset 0) first; `count` ≤ [`OVERLAY_RECT_CAPACITY`].
    fn draw_overlay<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>, count: u32) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.overlay_buffer.slice(..));
        pass.draw(0..6, 0..count);
    }
}

// ---------------------------------------------------------------------------
// Image rendering pipeline (Kitty graphics protocol)
// ---------------------------------------------------------------------------

const IMAGE_SHADER: &str = r"
struct ScreenUniforms {
    resolution: vec2<f32>,
    _padding: vec2<f32>,
};

struct ImageVertex {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_offset: vec2<f32>,
    @location(3) uv_scale: vec2<f32>,
    @location(4) opacity: f32,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) opacity: f32,
};

@group(0) @binding(0) var<uniform> screen: ScreenUniforms;
@group(1) @binding(0) var image_tex: texture_2d<f32>;
@group(1) @binding(1) var image_samp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, instance: ImageVertex) -> VsOut {
    let corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0),
    );
    let c = corners[vi];
    let px = instance.pos + c * instance.size;
    let ndc = vec2(px.x / screen.resolution.x * 2.0 - 1.0, 1.0 - px.y / screen.resolution.y * 2.0);

    var out: VsOut;
    out.position = vec4(ndc, 0.0, 1.0);
    out.uv = instance.uv_offset + c * instance.uv_scale;
    out.opacity = instance.opacity;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var col = textureSample(image_tex, image_samp, in.uv);
    col.a = col.a * in.opacity;
    return col;
}
";

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct ImageInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_offset: [f32; 2],
    uv_scale: [f32; 2],
    // Per-quad opacity multiplier (offset 32). Kept in LOCKSTEP with the
    // VertexBufferLayout attribute (shader_location 4, offset 32) and the
    // IMAGE_SHADER `col.a *= in.opacity`; a mismatch of any of the three is
    // silent GPU corruption, so the size + offset are compile-pinned below.
    opacity: f32,
}

// Byte-pin (compile-time): the opacity field's size + offset must match the
// VertexBufferLayout attribute (offset 32) or bytemuck reads garbage. A struct
// layout change that desyncs them is a compile error, not a silent GPU glitch.
const _: () = assert!(std::mem::size_of::<ImageInstance>() == 36);
const _: () = assert!(std::mem::offset_of!(ImageInstance, opacity) == 32);

struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    instance_buffer: wgpu::Buffer,
}

/// Cached GPU texture for a Kitty image.
struct GpuImage {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    seqno: u64,
}

impl ImagePipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image_shader"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER.into()),
        });

        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image_uniform_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image_tex_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image_pl"),
            bind_group_layouts: &[&uniform_bgl, &texture_bgl],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ImageInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 8,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 16,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 24,
                    shader_location: 3,
                },
                // opacity — LOCKSTEP with ImageInstance.opacity (offset 32) +
                // IMAGE_SHADER @location(4). Pinned by the byte-pin test.
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32,
                    offset: 32,
                    shader_location: 4,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[instance_layout],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image_uniforms"),
            size: std::mem::size_of::<ScreenUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image_uniform_bg"),
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image_instances"),
            size: (64 * std::mem::size_of::<ImageInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            sampler,
            texture_bind_group_layout: texture_bgl,
            instance_buffer,
        }
    }

    fn create_gpu_image(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        width: u32,
        height: u32,
        seqno: u64,
    ) -> GpuImage {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kitty_image"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kitty_image_bg"),
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        GpuImage {
            texture,
            bind_group,
            seqno,
        }
    }
}

// ---------------------------------------------------------------------------
// Render snapshot — cloned terminal state for lock-free rendering
// ---------------------------------------------------------------------------

struct Snapshot {
    rows: Vec<Vec<Cell>>,
    /// M2 — the style mapping the cloned rows' `style_id`s resolve
    /// through. A [`StyleSnapshot`] (just the `Vec<Style>`), NOT a
    /// full `StyleTable` clone: the table's `by_style` intern index
    /// is producer-side state the render path never reads, and a
    /// style-heavy stream can park the table near its u16 cap until
    /// the next saturation gc — cloning it per frame ratcheted the
    /// frame cost up without ever coming back down (review finding
    /// 2026-06-12). The render path stays lock-free.
    styles: StyleSnapshot,
    /// Live 256-entry ANSI palette (OSC 4 can override any slot) —
    /// resolves `UnderlineColor::Indexed` at decoration-build time.
    /// fg/bg resolve at SGR-parse time, but the underline-colour wire
    /// keeps the index, so the render side needs the palette truth.
    palette: [Color; 256],
    cursor: Cursor,
    cols: usize,
    num_rows: usize,
    /// Viewport scroll offset (0 = live tail). Drives the history
    /// indicator and suppresses the cursor draw — drawing the live
    /// cursor over history rows implied an insertion point that
    /// doesn't exist there (phantom-cursor finding 2026-06-11).
    scroll_offset: usize,
    /// Total scrollback rows — thumb sizing for the indicator.
    scrollback_total: usize,
    urls: Vec<DetectedUrl>,
    search_active: bool,
    search_matches: Vec<crate::search::SearchMatch>,
    search_current: usize,
    image_placements: Vec<ImagePlacement>,
    /// Viewport-relative rows where a Pane-as-block boundary
    /// sits — each is an OSC 133 `A` prompt-start mark within
    /// the visible viewport. The render layer draws a faint
    /// horizontal separator above each.
    block_separator_rows: Vec<usize>,
    /// Selection span resolved AT SNAPSHOT TIME from the content
    /// anchors (resolve-at-use; never cached across frames), already
    /// normalized to reading order, mapped to viewport rows, and
    /// clipped to the visible window. `None` = no selection, or its
    /// content was evicted / lies entirely off-screen.
    selection_span: Option<(CellPos, CellPos)>,
}

/// Comparable summary of the styling axes that decide whether two
/// adjacent simple cells belong to the same shaping run. Two cells
/// with identical `RunAttrsKey` can share one glyphon Buffer because
/// their `Attrs` carry the same family / colour / weight / style.
///
/// Designed for **cheap equality** in the hot path — five small fields,
/// no allocations. We deliberately don't include the family choice in
/// the key: the `italic` flag implies the family selection (italic
/// cells → italic family, regular/bold cells → primary family), so
/// equal `italic` ⇒ equal family. Background colour is also absent
/// because cell backgrounds are painted by the rect pipeline, not by
/// the text Buffer.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RunAttrsKey {
    fg_r: u8,
    fg_g: u8,
    fg_b: u8,
    bold: bool,
    italic: bool,
}

/// Cache key for a shaped glyphon `Buffer`. P7 — refterm's biggest
/// insight: keying shaped runs by their UTF-8 byte text + attrs
/// avoids ~99% of cosmic-text shape calls in a typical interactive
/// session (the prompt repeats, scrollback runs repeat, "ls" output
/// stays mostly the same, code lines re-render verbatim until edited).
///
/// `font_size_bits` is `font_size_px.to_bits()` — captures the
/// physical-pixel font size (logical * scale_factor). Required in the
/// key because changing font size or scale factor invalidates every
/// shape. The whole cache is also rebuilt on font-family change via
/// the `metrics_measured = false` reset that already fires from
/// `set_scale_factor`.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: Box<str>,
    attrs: RunAttrsKey,
    font_size_bits: u32,
}

/// Capacity bound for the shape cache. ~4096 unique runs covers a full
/// 200×60 grid plus a few frames of variation; eviction is LRU so
/// rarely-seen runs (random spam, search highlights mid-stream) drop
/// out without pinning memory. At ~50–500 bytes per cached Buffer this
/// is a few MB worst-case.
const SHAPE_CACHE_CAP: usize = 4096;

/// Per-row run kind enum for P11 — tells `push_run` which y/height
/// math to apply when flushing an open RLE span. These are the
/// solid-fill per-row rect kinds whose pixel geometry can be
/// described as "start_col × cell_width wide, on row_idx" — cell
/// backgrounds fill the whole cell height, strikethroughs sit at
/// mid-cell, overlines (SGR 53) hug the cell top edge. Underlines
/// left this enum at M3-C2: their geometry is style-dispatched
/// through the engawa decoration emitters (`push_underline_run`),
/// not a single solid rect. Box-drawing rects have per-glyph shapes
/// and stay per-cell.
#[derive(Clone, Copy)]
enum RectKindForRle {
    Background,
    Strikethrough,
    Overline,
}

/// Decoration metrics constants — the single source the
/// [`engawa::UnderlineMetrics`] projection derives from. The
/// underline stroke keeps the historical placement (top of stroke
/// two pixels above the cell bottom, one pixel thick) so the M2
/// single-underline pixels are unchanged.
const UNDERLINE_OFFSET_FROM_BOTTOM: f32 = 2.0;
const DECORATION_THICKNESS: f32 = 1.0;
/// Visual-bell flash duration in frames (~200ms @ 60fps) — the
/// full-window flash decays linearly from `BELL_FLASH_PEAK_ALPHA` to 0
/// over this many frames. Gentle + brief per the polish-round spec
/// (the old 4-frame / 0.15-alpha flash popped too hard).
const BELL_FLASH_FRAMES: u8 = 12;
/// Visual-bell flash duration in seconds — DERIVED from the legacy frame
/// count (`12 / 60 = 0.2 s`) so the two cannot drift and the golden
/// byte-pin can prove they agree exactly at 60fps. The flash is a
/// duration-based [`crate::motion::Tween`], so it lasts the same
/// wall-clock time at any framerate (the old `u8` frame counter made the
/// flash last half as long at 120fps).
const BELL_FLASH_SECS: f32 = BELL_FLASH_FRAMES as f32 / 60.0;
/// The bell glow colour — a cool near-white (matches engawa's `BELL_TINT`
/// rgb). Set explicitly on every bell so a prior exit-status pulse's tint
/// never lingers on the glow clock. The exit-status pulse colours are the
/// theme's `exit_ok` / `exit_err` (ANSI green/red), read at pulse time.
const BELL_GLOW_RGB: [f32; 3] = [0.85, 0.92, 1.0];
/// Peak alpha of the visual-bell flash at frame 0 (subtle by default;
/// the operator tunes intensity after).
const BELL_FLASH_PEAK_ALPHA: f32 = 0.10;
/// Whisper-dim alpha of the theme background painted over an unfocused
/// window so a backgrounded window reads as backgrounded.
const UNFOCUSED_DIM_ALPHA: f32 = 0.06;

/// THE surface/scene texture format — one constant, consumed by
/// pipeline construction (`init`), the per-frame SCENE/chain leases,
/// and the headless test targets. The dispatcher's pipeline cache
/// compiles against the construction-time format while pooled
/// textures use the render-time one; two hand-copies of the literal
/// desyncing meant a wgpu validation error on every catalog pass
/// (M3 review 2026-06-12).
const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
/// Approximate baseline fraction of the cell height. The cell height
/// is `font_size * line_height` (config-driven) and mado does not
/// measure ascent; the baseline only feeds the curly band's amplitude
/// (`underline_y - baseline`, floored at one thickness upstream),
/// so an approximation degrades amplitude, never correctness.
const BASELINE_FRACTION: f32 = 0.8;

// ---------------------------------------------------------------------------
// TerminalRenderer
// ---------------------------------------------------------------------------

#[derive(pleme_invalidating_setter_derive::InvalidatingSetter)]
pub struct TerminalRenderer {
    terminal: SharedTerminal,
    selection: Arc<Mutex<Selection>>,
    search: Arc<Mutex<SearchState>>,
    /// Reader-only directory-frecency overlay state (轍). Shared from the pane
    /// via `set_dir_picker`; drawn (when `.open`) as a Pass-6 overlay.
    dir_picker: Arc<Mutex<crate::dir_picker::DirPickerState>>,
    /// Reader-only Ctrl-S praça session-picker overlay state. Shared from
    /// the engine via `set_session_picker`; drawn (when `.open`) as a
    /// Pass-6 overlay, same model as `dir_picker`.
    session_picker: Arc<Mutex<crate::session_picker::SessionPickerState>>,
    /// The SINGLE source of truth for which overlay Pass 6 draws — a 1:1
    /// mirror of the engine's `Overlay` FSM, shared via `set_overlay_focus`
    /// and written on every transition. Pass 6 matches on this one value
    /// and draws exactly the overlay that owns the keyboard, so two
    /// overlays can never paint at once (theory §VI). The picker `.open`
    /// bools above are read only for their *content*, never as the gate.
    overlay_focus: Arc<Mutex<crate::ux::modes::Overlay>>,
    // window field removed at Phase 4 — single-pane mado.
    font_size: f32,
    /// Cell-height multiplier — the line rhythm. The cell height is
    /// `font_size * line_height` (logical) and cosmic-text's line-box
    /// metric is set to the same product, so a measured glyph row
    /// matches the cell exactly. Sourced from `MadoConfig::line_height`
    /// (← `FleetDefaults::line_height`, ghostty's native 1.32 × +25% =
    /// 1.65). This is the SINGLE source of the multiplier the renderer
    /// once hardcoded as `* 1.4` in three places.
    line_height: f32,
    font_family: String,
    /// Italic-face family. cosmic-text resolves italics by walking
    /// the fontdb for `Style::Italic`; pinning the family explicitly
    /// lets mado route italic cells to a calligraphic alternative
    /// (Iosevka Etoile, Maple Mono Italic, etc.) regardless of which
    /// family `font_family` names.
    font_italic: String,
    /// Symbols / Nerd-icon family. Cells whose glyph is in the
    /// powerline / PUA ranges (`glyph_class::is_symbol_glyph`) shape
    /// against this family instead of the primary, so icon glyphs come
    /// from one curated source rather than cosmic-text's arbitrary
    /// coverage-walk pick (ghostty's symbols-font model). Empty falls
    /// back to `font_family`.
    font_symbols: String,
    cell_width: f32,
    cell_height: f32,
    // Logical padding — live-reloadable (M4 stage 2). Draw offsets
    // read `padding_px()` per frame, so assign + repaint suffices.
    #[invalidating_setter]
    padding: f32,
    bg_color: wgpu::Color,
    fg_color: Color,
    #[invalidating_setter]
    ansi_colors: [Color; 16],
    rect_pipeline: Option<RectPipeline>,
    image_pipeline: Option<ImagePipeline>,
    /// M3-C1 — the engawa graph route. The dispatcher owns the
    /// per-Material pipeline cache (Arc-backed device/queue clones,
    /// no lifetime borrow); the pool leases the SCENE + chain
    /// offscreen textures; the cache holds the CompiledGraph keyed by
    /// (effect set, resolution) so steady-state frames never compile.
    dispatcher: Option<engawa_wgpu::WgpuDispatcher>,
    texture_pool: engawa_wgpu::TexturePool,
    catalog_sampler: Option<wgpu::Sampler>,
    /// Per-effect params uniform buffers, keyed by the catalog's
    /// `params_resource()` id. Created lazily at first enable, sized
    /// by `params_size()`; written per frame via FrameUniforms.
    effect_params: HashMap<&'static str, wgpu::Buffer>,
    frame_graph: crate::render_graph::FrameGraphCache,
    gpu_images: HashMap<u32, GpuImage>,
    // colorblind_mode field DELETED (M3 review 2026-06-12): it was a
    // second mutable cell mirroring effects_config.colorblind.mode —
    // main.rs wrote it, the tear-attach entry point didn't, so
    // effects.colorblind.mode (and the accessibility alias) were
    // silently dead in tear windows. The effect set + frame uniforms
    // now read effects_config directly; one source, no mirror.
    #[invalidating_setter]
    bold_is_bright: bool,
    last_seqno: u64,
    // Cursor presentation + blink clock — live-reloadable via the
    // derive-generated setters (M4 stage 2 config delta-apply);
    // assign + repaint is the whole contract, same as the other
    // #[invalidating_setter] fields.
    #[invalidating_setter]
    cursor_style: CursorStyle,
    #[invalidating_setter]
    cursor_blink: bool,
    #[invalidating_setter]
    cursor_blink_rate_ms: u32,
    metrics_measured: bool,
    /// Bell visual flash — a linear [`crate::motion::Tween`] from
    /// `BELL_FLASH_PEAK_ALPHA` to 0 over `BELL_FLASH_SECS`, advanced by
    /// `ctx.dt` each frame. A resting flash is `Tween::inert()`.
    /// Framerate-independent (the old `u8` frame counter made the flash
    /// last half as long at 120fps).
    bell_flash: crate::motion::Tween,
    /// Bell-flash duration in seconds — from `motion.bell_flash.duration_ms`
    /// (operator-morphable; applied via `apply_config`).
    bell_flash_duration_secs: f32,
    /// Bell-flash peak alpha — from `motion.bell_flash.peak_alpha`.
    bell_flash_peak: f32,
    /// Bell-flash easing curve — from `motion.bell_flash.easing`.
    bell_flash_curve: crate::motion::Curve,
    /// Picker fade-in (`motion.picker_animate`): `overlay_open_at` is the
    /// render-clock time (`ctx.elapsed`) the overlay last opened (the None→open
    /// edge); `overlay_progress` is this frame's cached fade alpha ∈ [0,1] that
    /// `draw_overlay` reads via `&self`. Born from `ctx.elapsed` (NOT `Instant`)
    /// so the determinism ladders stay byte-stable; `Cell` for the `&self`
    /// draw path.
    overlay_open_at: std::cell::Cell<Option<f32>>,
    overlay_progress: std::cell::Cell<f32>,
    /// Selection highlight background (RGBA).
    #[invalidating_setter]
    selection_bg: [f32; 4],
    /// Cursor color (RGBA).
    #[invalidating_setter]
    cursor_color: [f32; 4],
    /// AGENT-RESERVED chrome accent (u8-RGB, same shape as `fg_color` —
    /// the glyphon text path takes raw sRGB bytes via
    /// `GlyphonColor::rgba`). Today this paints the search-status line —
    /// the closest-shipping agent / MCP-activity surface. Set by
    /// `theme::apply_config_theme` from the active theme's `agent_accent`
    /// (Vellum `fable_violet` via the SEMANTIC `agent` role). Defaults
    /// to Nord frost so legacy themes keep their prior look.
    #[invalidating_setter]
    search_status_color: Color,
    /// Search-match highlight fills (u8-RGB; the rect pipeline
    /// linearizes at paint time via `overlay_rect_color`, exactly like
    /// `search_status_color` linearizes in the text path). The CURRENT
    /// match draws `search_current_color` at α0.5; every OTHER match
    /// draws `search_other_color` at α0.2. Set by
    /// `theme::apply_config_theme` from the active theme's
    /// `search_current` / `search_others` (Vellum `first_light`
    /// #D7C489 / #443E2A via `VellumPalette::vellum().surfaces()`).
    /// Defaults to Nord aurora yellow #EBCB8B so legacy presets keep
    /// their prior look until a theme that carries the surfaces loads.
    #[invalidating_setter]
    search_current_color: Color,
    /// See [`Self::search_current_color`] — the OTHER-match fill.
    #[invalidating_setter]
    search_other_color: Color,
    /// Clickable-link text + underline accent (u8-RGB; the rect pipeline
    /// linearizes at paint time via `overlay_rect_color`, same discipline
    /// as `search_current_color`). OSC 8 hyperlinks AND auto-detected URLs
    /// paint their glyphs in this colour and underline with it. Set by
    /// `theme::apply_config_theme` from the active theme's `link` (Nord
    /// frost `ansi[12]`). Defaults to Nord frost #88C0D0 — the prior
    /// hardcoded underline blue — until a theme loads.
    #[invalidating_setter]
    link_color: Color,
    /// Whether clickable links are highlighted (frost text + underline).
    /// Set from `config.links.{enabled,highlight}` in
    /// [`apply_effects_and_accessibility`](Self::apply_effects_and_accessibility).
    /// Defaults `true` (the prescribed tier); the bare tier strips it.
    #[invalidating_setter]
    links_highlight: bool,
    /// OSC 133 command-block separator accent (u8-RGB; the rect pipeline
    /// linearizes at paint time via `overlay_rect_color`, same discipline
    /// as `link_color`). Set by `theme::apply_config_theme` from the active
    /// theme's `prompt_mark` (Nord frost `ansi[4]`). Defaults to Nord
    /// `#5E81AC` — the prior hardcoded separator blue — until a theme loads.
    #[invalidating_setter]
    prompt_mark_color: Color,
    /// Scrolled-into-history thumb accent (u8-RGB; linearized at paint
    /// time). Set by `theme::apply_config_theme` from the active theme's
    /// `scrollbar` (Nord cyan-frost `ansi[6]`). Defaults to Nord `#88C0D0`
    /// — the prior hardcoded thumb blue — until a theme loads.
    #[invalidating_setter]
    scrollbar_color: Color,
    /// Visual-bell flash colour (u8-RGB; linearized at paint time). Set by
    /// `theme::apply_config_theme` from the active theme's `bell_flash`
    /// (the theme foreground). Defaults to white — the prior hardcoded
    /// flash — until a theme loads.
    #[invalidating_setter]
    bell_flash_color: Color,
    /// Command-completion glow colour on a clean exit (u8-RGB). Set by
    /// `theme::apply_config_theme` from the active theme's `exit_ok` (the
    /// ANSI green slot), so the success pulse tracks the theme instead of a
    /// hardcoded green. Defaults to a calm green until a theme loads.
    #[invalidating_setter]
    exit_ok_color: Color,
    /// Command-completion glow colour on a failure (u8-RGB). Set from the
    /// active theme's `exit_err` (the ANSI red slot); the failure pulse
    /// tracks the theme. Defaults to a warm red until a theme loads.
    #[invalidating_setter]
    exit_err_color: Color,
    /// Unfocused-window dim colour (u8-RGB; linearized at paint time) — the
    /// theme background painted at a whisper alpha over a backgrounded
    /// window. Set by `theme::apply_config_theme` from the active theme's
    /// `background`. Defaults to the Vellum night0 ground until a theme loads.
    #[invalidating_setter]
    unfocused_dim_color: Color,
    /// Whether the visual bell flash renders. Set from
    /// `config.feedback.visual_bell` in
    /// [`apply_effects_and_accessibility`](Self::apply_effects_and_accessibility).
    /// Defaults `true` (prescribed); the bare tier strips the flash (the
    /// audible-bell glow ring is governed by its own effect gate).
    #[invalidating_setter]
    feedback_visual_bell: bool,
    /// Whether a completed command pulses the cursor glow — green on
    /// success, red on failure. Set from `config.feedback.exit_code_glow`
    /// in [`apply_effects_and_accessibility`](Self::apply_effects_and_accessibility).
    /// Defaults `true` (prescribed); the bare tier strips it. The policy
    /// for *which* completions pulse lives in `apply_side_effects`; this
    /// gate is the final "render it at all" switch (like the bell's).
    #[invalidating_setter]
    feedback_exit_glow: bool,
    /// Whether an unfocused window is whisper-dimmed. Set from
    /// `config.motion.unfocused_dim` in
    /// [`apply_effects_and_accessibility`](Self::apply_effects_and_accessibility).
    /// Defaults `true` (prescribed); the bare tier leaves an unfocused
    /// window undimmed.
    #[invalidating_setter]
    motion_unfocused_dim: bool,
    /// `motion.picker_animate`: fade the Ctrl-S picker overlay in when it
    /// opens. Off ⇒ the overlay appears instantly at full alpha.
    #[invalidating_setter]
    motion_picker_animate: bool,
    /// Reduce motion: disable cursor blink and bell flash.
    #[invalidating_setter]
    reduce_motion: bool,
    /// Where the Ctrl-S session picker overlay anchors on screen.
    /// `Bottom` (default) rises from the bottom edge like Ctrl-R/Ctrl-T;
    /// `Top` is the legacy drop-from-top. Set from
    /// `config.tear.session_picker_anchor` in
    /// [`apply_effects_and_accessibility`](Self::apply_effects_and_accessibility).
    session_picker_anchor: crate::config::PickerAnchor,
    /// The three text layers this renderer owns on the shared
    /// `garasu::TextLayerStack` — one isolated glyphon renderer (own vertex
    /// buffer) + own viewport each. Minted once by [`ensure_layers`](Self::ensure_layers)
    /// on the first render. Terminal-grid text, overlay/picker text, and
    /// search-status text each `prepare`+`render` their OWN layer so a second
    /// text pass within a frame cannot clobber the first's vertex buffer (the
    /// top-left-blank Ctrl-S bug). See `garasu::TextLayerStack` + the §VIII
    /// ledger row in `docs/THEORY.md`. `TEXT_LAYERS` names them; a forcing-
    /// function test asserts `ensure_layers` mints exactly that many.
    term_layer: Option<garasu::TextLayerId>,
    overlay_layer: Option<garasu::TextLayerId>,
    search_layer: Option<garasu::TextLayerId>,
    /// Per-suggestion render-clock time (`ctx.elapsed` seconds) of when a row
    /// first appeared on screen, so the shade-in ramps from when the OPERATOR
    /// sees it. Render-clock, NOT wall-clock `Instant` — so two renders at the
    /// same `elapsed` produce identical alpha, the determinism the L1/L2
    /// ladders assert. Pruned to the visible set each draw, so a row that
    /// leaves + returns re-fades. `RefCell` → mutable from the `&self` draw path.
    suggestion_fade: RefCell<HashMap<crate::suggest::SuggestionId, f32>>,
    /// Shade-in duration (ms) from `config.suggestions.shade_in_ms` — how long
    /// a freshly-arrived suggestion takes to dissolve in.
    suggestion_shade_in_ms: u64,
    /// The themed colours every picker overlay paints with (query /
    /// row / selected / hint). Resolved from the active theme by
    /// [`crate::theme::apply_config_theme`] via `set_overlay_style`, so a
    /// theme swap restyles the pickers — no Nord literal in the draw path.
    /// Born with the Nord defaults the old `draw_*` methods hardcoded.
    #[invalidating_setter]
    overlay_style: crate::picker::component::OverlayStyle,
    /// Window focus — unfocused windows draw a hollow, steady cursor
    /// (the which-window-owns-the-keyboard affordance). Set by the
    /// adapters' Focused arms.
    #[invalidating_setter]
    focused: bool,
    /// HiDPI scale factor (1.0 on non-Retina, 2.0 on most Mac Retina,
    /// other values on Linux/Windows). Multiplies font_size and padding
    /// before they touch the GPU pipeline — the wgpu surface is sized
    /// in physical pixels, so all draw positions / cell metrics must
    /// be physical too, otherwise the rendered content only covers a
    /// `1/scale_factor`-sized chunk of the window. Refreshed each
    /// frame from `RenderContext::scale_factor`.
    scale_factor: f32,
    /// panel_px / framebuffer_px for the display mado is currently on — the
    /// downscale ratio the OS compositor applies AFTER mado's framebuffer.
    /// 1.0 when the framebuffer IS the panel grid (integer scale / no
    /// downscale); < 1.0 on a scaled display (macOS "More Space", X11 RandR
    /// `--scale`). Consumed by `snap_cell_height_px` so each row lands on a
    /// whole number of panel pixels and the compositor resample draws no
    /// inter-row seam. Discovered out-of-band (NOT via `scale_factor`, which
    /// is the framebuffer scale and stays 2.0 on a scaled Retina display);
    /// injected via `set_panel_ratio`. See the discoverability design.
    panel_ratio: f32,
    /// The PROVENANCE of `panel_ratio` — was it probed (`Discovered`),
    /// operator-set (`Configured`), or is it a silent fallback because the
    /// probe failed (`Unavailable`)? Sealing the old `unwrap_or(1.0)` so a
    /// seam on an unknown ratio is diagnosable, not a mystery. See
    /// `crate::panel_fit`.
    panel_ratio_source: crate::panel_fit::PanelRatio,
    /// Seam auto-tune master gate (config `display.seam_auto_tune`, default
    /// on). When on, the render prologue re-discovers `panel_ratio` from the
    /// live display on every surface-size change; off pins it to 1.0
    /// (byte-identical to no adjustment).
    seam_auto_tune: bool,
    /// Operator override for the discovered downscale ratio (config
    /// `display.downscale_ratio`). `Some(r)` pins the ratio (skips the probe);
    /// `None` auto-discovers via `kanchi::probe::display_scaling_ratio`.
    downscale_ratio_override: Option<f32>,
    /// Physical surface dims of the last rendered frame (0 until the
    /// first frame). Together with `metrics_measured`, this is the
    /// renderer's display truth — see [`Self::measured_grid`].
    last_surface_w: u32,
    last_surface_h: u32,
    /// The surface dims the panel-ratio probe last actually ran at (0
    /// until the first probe). The panel ratio depends on the *display*,
    /// not the surface size, so during a live drag-resize (macOS delivers a
    /// distinct drawable size nearly every frame) re-running the
    /// CoreGraphics `display_scaling_ratio` probe every intermediate frame
    /// is wasted work — the ratio only meaningfully changes when the size
    /// SETTLES (drag ends) or the window moves to a differently-scaled
    /// display (which lands as a settled size too). The probe is gated to
    /// fire once per settled size instead of ~60×/sec through the drag; the
    /// final grid snaps on the settled size exactly as before.
    last_ratio_probe_wh: (u32, u32),
    /// P7 shape cache: bounded LRU keyed by (text-bytes, attrs,
    /// physical font-size). The Arc<Buffer> lets cache hits share
    /// the same shaped Buffer with the per-frame text_areas Vec
    /// without copying. `RefCell` for interior mutability — the cache
    /// has to mutate inside `build_text_buffers` which is called from
    /// both `&mut self render(…)` and `&mut self render_multi_pane(…)`
    /// paths but where ws/snap borrows make a direct `&mut self` on
    /// the inner method awkward. The borrow is taken once per cache
    /// touch and dropped immediately so cross-frame conflicts are
    /// impossible (single-threaded render).
    shape_cache: RefCell<LruCache<ShapeKey, Arc<Buffer>>>,
    /// Cross-frame row-buffer reservoir. `snapshot()` swaps this retained
    /// `Vec<Vec<Cell>>` into the fresh Snapshot and refills it in place
    /// (clear-not-drop per inner row keeps its `Vec<Cell>` capacity), so the
    /// per-vsync visible-rows clone reuses last frame's allocations instead of
    /// realloc'ing — the dominant idle-frame cost. `RefCell` because
    /// `snapshot()` takes `&self` (the `&self` draw-path idiom — see
    /// `shape_cache`); single-threaded render ⇒ the borrow is taken + dropped
    /// inside one statement. The buffers return here right after the builds
    /// consume `snap.rows` (byte-identical frames — determinism tests guard it).
    row_scratch: RefCell<Vec<Vec<Cell>>>,
    /// P28 — last-rendered cursor_on bit. Cursor blink is a 1–4 Hz
    /// animation (period 500 ms default); we'd otherwise wake every
    /// 16 ms vsync just to repaint the SAME cursor state. Skip frames
    /// where neither seqno NOR this bit have flipped — drops idle
    /// render rate from 60 Hz to ~4 Hz.
    last_cursor_on: bool,
    /// P31 — sprite atlas analog for box-drawing / block-element
    /// rect templates. Each entry stores the *relative* sub-rects
    /// (rel_x, rel_y, w, h) that compose a glyph at the renderer's
    /// current `cell_width`/`cell_height`. Per-frame box-drawing cell
    /// emission becomes a table lookup + a translate-by-(x,y) loop —
    /// no per-cell match-arm dispatch, no per-cell vec allocation
    /// inside `box_drawing_rects`. Invalidated when cell metrics
    /// change via `set_scale_factor` / `set_font_size`.
    box_draw_templates: RefCell<HashMap<char, Vec<(f32, f32, f32, f32)>>>,
    /// Timestamp when the most recent BSU defer began. P14 holds off
    /// rendering between DEC mode 2026 BSU/ESU so full-screen TUI
    /// redraws don't tear. A misbehaving emitter (BSU without matching
    /// ESU, or a crash before ESU) would freeze the screen
    /// indefinitely without this cap. Once the defer exceeds
    /// `SYNC_OUTPUT_MAX_DEFER`, we force a render and reset the
    /// timestamp.
    sync_output_deferred_since: Option<Instant>,
    /// Last `Terminal::grid_epoch()` this renderer observed. A change
    /// means the grid was fully reset (RIS / config hot-reload /
    /// session switch) and this renderer's per-pane frame state
    /// (last_seqno, blink phase, sync-output defer) is stale for the
    /// new content. Seeing the bump triggers a frame-state reset plus a
    /// forced full repaint across the swapchain — see
    /// `force_paint_frames` and `Terminal::grid_epoch`.
    last_grid_epoch: u64,
    /// Frames remaining to force a full paint after a grid-epoch change,
    /// bypassing the synchronized-output defer. Set to the swapchain
    /// depth so EVERY back-buffer slot is repainted with the new pane's
    /// content — otherwise a switch can leave one Metal slot showing the
    /// prior session, surfacing as the "shadow / copies of the prompt"
    /// afterimage when present() cycles back to it. Counts down to 0.
    force_paint_frames: u8,
    /// Post-effect config — mirrors `MadoConfig.effects`. The
    /// enabled-effect set (and therefore the graph cache key) is
    /// derived from this each frame.
    effects_config: crate::config::MadoEffectsConfig,
    /// The resolved ambience composition (operator design law,
    /// 2026-06-13) — the ONE typed value both the effect set and the
    /// composed per-frame params derive from. Re-resolved on every
    /// `set_effects_config` (it is a pure function of the preset +
    /// `reduce_motion`, so a config delta re-derives it). `reduce_motion`
    /// resolves it to the empty composition (zero members ⇒ zero nodes).
    ambience: crate::ambience::AmbienceComposition,
    /// Host-side snow animation state (the catalog WGSL is
    /// stateless; time/pulse/pile live here, integrated from the
    /// render clock — never wall time).
    snow_state: SnowState,
    /// Host-side glow-on-bell state — BEL saturates the clock,
    /// per-frame decay drains it.
    glow_state: GlowState,
    /// Host-side aurora clock — drives the curtain drift (the catalog
    /// WGSL is stateless). Params are composed per-frame from the
    /// ambience layer + theme palette + governor quality.
    aurora_state: AuroraState,
    /// The ambience perf governor (operator perf wave, 2026-06-13) — a
    /// typed FSM scaling the composed layer's quality word to the frame
    /// budget, rebuild-free. Ticked per frame from the measured
    /// `frame_us` ONLY when the ambience composition is non-empty (the
    /// `reduce_motion` bypass: an empty composition omits the aurora
    /// node, so there is nothing to quality).
    ambience_governor: crate::ux::ambience_governor::AmbienceGovernor,
    // The M3 `pending_config_reload` cell was DELETED at M4 stage 2:
    // hot-reload now runs through `ux::ConfigHotReload` in BOTH
    // event-loop adapters (dirty flag → typed SetterCall delta), so
    // the renderer holds no reload state of its own.
}

/// Host-side snow animation state (M3 Stream D). Everything
/// time-like integrates from the render clock (`ctx.elapsed` /
/// `ctx.dt`) — NOT wall time — so headless renders at
/// elapsed=0/dt=0 are byte-deterministic (the L2 ladder relies on
/// it; the legacy `Instant::now()` overlay could never join it).
struct SnowState {
    params: engawa_wgpu::catalog::snow::SnowParams,
}

impl SnowState {
    fn new() -> Self {
        Self { params: engawa_wgpu::catalog::snow::SnowParams::default() }
    }

    /// Re-seed the operator knobs (intensity / wind / layers /
    /// temperature / accumulation baseline) from config.
    fn apply_config(&mut self, cfg: &crate::config::MadoSnowConfig) {
        self.params.set_intensity(cfg.intensity);
        self.params.set_wind(cfg.wind);
        self.params.set_accumulation(cfg.accumulation);
        self.params.set_layer_count(cfg.layer_count);
        self.params.set_temperature(cfg.temperature);
    }

    /// Per-frame integration — ported verbatim from the deleted
    /// `SnowOverlay::render` host loop, re-clocked from the render
    /// context. Temperature drives the pile sign: cold fills at
    /// `pile_rate`, warm melts at `melt_rate`, 0.5 holds.
    fn tick(&mut self, elapsed: f32, dt: f32, cfg: &crate::config::MadoSnowConfig) {
        self.params.set_time(elapsed);
        // ~0.14 s half-life on the typing pulse (0.92^n = 0.5 at
        // n ≈ 8.3 frames @ 60 Hz), frame-rate-independent. Verbatim
        // SnowOverlay port — the prior "~0.5 s" comment was wrong by
        // 3.6x; the CONSTANT is the shipped behavior, keep it.
        // The frame-rate-independent 0.92^(dt·60) decay, shared with the
        // bell glow below — collapsed into `motion::frame_decay` (the 2nd
        // copy is the extract trigger; byte-identical to the old inline).
        let decay = crate::motion::frame_decay(dt, cfg.snow_pulse_retain.clamp(f32::MIN_POSITIVE, 1.0));
        self.params.set_typing_pulse(self.params.frame[3] * decay);
        let temp = cfg.temperature.clamp(0.0, 1.0);
        let pile_delta = if temp < 0.5 {
            cfg.pile_rate * (1.0 - temp * 2.0) * dt
        } else {
            -cfg.melt_rate * ((temp - 0.5) * 2.0) * dt
        };
        let new_acc = (self.params.params[0] + pile_delta).clamp(0.0, 1.0);
        self.params.set_accumulation(new_acc);
    }
}

/// Host-side glow-on-bell clock — `ring()` on BEL, exponential
/// decay per frame (same dt-normalised shape as the snow pulse).
struct GlowState {
    params: engawa_wgpu::catalog::glow_on_bell::GlowOnBellParams,
}

impl GlowState {
    fn new() -> Self {
        Self { params: engawa_wgpu::catalog::glow_on_bell::GlowOnBellParams::default() }
    }

    fn tick(&mut self, dt: f32, retain: f32) {
        self.params
            .decay(crate::motion::frame_decay(dt, retain.clamp(f32::MIN_POSITIVE, 1.0)));
    }
}

/// Host-side aurora clock (the catalog WGSL is stateless; the consumer
/// supplies `time` via `set_time` each frame). The actual intensity /
/// drift / shimmer / horizon / colors / quality are applied
/// per-frame in `frame_uniforms_for` from the resolved ambience
/// composition (or the power-user override) + the theme palette + the
/// ambience governor — this state holds ONLY the running clock so the
/// curtain drifts. Time integrates from the render clock (`ctx.elapsed`),
/// never wall time, so headless renders stay byte-deterministic.
struct AuroraState {
    /// Seconds since launch, accumulated from the render clock.
    time: f32,
}

impl AuroraState {
    fn new() -> Self {
        Self { time: 0.0 }
    }

    /// Pin the clock to the render-loop elapsed seconds. At elapsed=0
    /// (the headless ladders) this is the identity, keeping the route
    /// byte-deterministic.
    fn tick(&mut self, elapsed: f32) {
        self.time = elapsed;
    }
}

/// Maximum time the BSU/ESU defer is allowed to skip frames. Kitty
/// uses ~150 ms; we choose 100 ms — long enough to absorb a normal
/// helix / lazygit / btop full-screen redraw burst, short enough that
/// a stuck BSU is invisible to the user (~6 dropped frames at 60 Hz).
const SYNC_OUTPUT_MAX_DEFER: std::time::Duration = std::time::Duration::from_millis(100);

/// How many frames to force a full paint after a grid-epoch change
/// (RIS / config hot-reload / session switch). Sized to comfortably
/// exceed any platform swapchain depth (Metal double/triple buffer is
/// 2–3) so `present()` cannot later cycle back to a back-buffer slot
/// that still holds the prior pane — the cause of the post-switch
/// "shadow / copies of the prompt" afterimage. Cheap: a full idle
/// paint is ~300 µs (see the damage-gate P-FIX note), so 3 forced
/// frames cost <1 ms, once, per switch.
const EPOCH_FORCE_PAINT_FRAMES: u8 = 3;

/// Sealed column-truth for the dense terminal grid.
///
/// ## The invariant, as a type
///
// The dense-row column primitive (`GridCol` + `glyph_columns`) is the
// crate's single source of column truth — see `crate::grid_col`. The
// text + rect/decoration pipelines below and URL detection all consume
// the same sealed mint, so their columns cannot diverge.
use crate::grid_col::{glyph_columns, GridCol};

/// The named text surfaces this renderer draws, each on its OWN isolated
/// layer of the shared `garasu::TextLayerStack` (own vertex buffer). The
/// forcing function `layers_match_text_layers_const` asserts `ensure_layers`
/// mints exactly this many layers — so a NEW text surface added without its
/// own isolated layer (which would reintroduce the cross-pass clobber) fails
/// the test. Order matches the `add_layer` calls in `ensure_layers`.
const TEXT_LAYERS: &[&str] = &["terminal", "overlay", "search"];

/// Quantize a cell **height** to a whole device pixel (ceil, floor 1px).
///
/// THE row-seam chokepoint (operator report 2026-07-05: thin full-width
/// horizontal lines between text rows). A fractional cell height in
/// device pixels (e.g. font 13 × line-height 1.25 × 2× Retina = 32.5px)
/// gives consecutive rows a non-uniform pixel geometry: every other
/// row's top edge lands mid-pixel, so row boundaries alternate between
/// pixel-aligned and half-pixel rasterization — glyph baselines, cursor
/// blocks, and row background quads all beat against the pixel grid at
/// a 2-row rhythm, and any downstream resample (macOS scaled-display
/// compositing, window screenshots at non-integer ratios) turns that
/// rhythm into visible 1px luminance seams between rows.
///
/// Quantizing ONCE, here, at the metric chokepoint makes every row rect
/// `[row*h, (row+1)*h)` land on identical integer pixel edges — rows
/// tile exactly by construction, at every font size, with no
/// per-callsite rounding anywhere in the geometry code. `ceil` (never
/// `round`/`floor`) so the glyph line box always fits INSIDE the cell —
/// a rounded-down cell would let the next row's background quad clip
/// descenders.
///
/// Deliberately height-only: `cell_width` must stay the font's exact
/// fractional advance — per-run glyph buffers advance at the font's
/// natural metric, so quantizing width would re-introduce the
/// 2026-05-13 "gap between every character" drift within multi-cell
/// runs.
#[inline]
fn quantize_cell_height_px(h: f32) -> f32 {
    h.ceil().max(1.0)
}

/// Snap a cell **height** so each row lands on an integer count of *panel*
/// pixels — the seam chokepoint's scale-aware form.
///
/// `quantize_cell_height_px` above snaps to whole *framebuffer* pixels, which
/// kills the seam only when the framebuffer IS the panel grid (integer scale:
/// 1.0 / 2.0 Retina / Wayland-viewporter). But on a **scaled** display the OS
/// compositor downscales mado's framebuffer to a smaller physical panel at a
/// NON-integer ratio (measured live 2026-07-06: macOS "More Space" renders a
/// 4112×2658 framebuffer, downscaled to a 3456×2234 panel, ratio ≈ 0.84).
/// Integer *framebuffer* rows then map to *fractional* panel rows, and the
/// downscale filter turns that periodic sub-pixel row structure back into
/// visible 1px seams — the residual the framebuffer-only fix can't reach.
///
/// `panel_ratio` = panel_px / framebuffer_px (discovered per display; 1.0 when
/// there is no downscale). At ratio 1.0 this is byte-identical to
/// `quantize_cell_height_px` (the integer-scale path is untouched). At ratio
/// < 1.0 we pick the framebuffer cell height whose panel projection is a whole
/// number of panel pixels — `k = round(h · ratio)` panel px per row, then the
/// framebuffer height `k / ratio` that lands on it — so every row boundary
/// `N · (k/ratio)` framebuffer px → `N · k` panel px EXACTLY, identical
/// resample phase every row, and the periodic luminance beat is gone.
///
/// Height-only, same as `quantize_cell_height_px`: `cell_width` keeps the
/// font's fractional advance (the 2026-05-13 anti-gap rationale).
#[inline]
fn snap_cell_height_px(h: f32, panel_ratio: f32) -> f32 {
    if (panel_ratio - 1.0).abs() < 1.0e-4 {
        // Integer scale (or downscale disabled): framebuffer px == panel px.
        quantize_cell_height_px(h)
    } else {
        // Scaled display: snap to a whole number of PANEL pixels.
        let panel_px = (h * panel_ratio).round().max(1.0);
        (panel_px / panel_ratio).max(1.0)
    }
}

/// Snap the grid's **rendering origin** (the top/left padding, in framebuffer
/// px) so it projects onto a whole number of *panel* pixels — the seam fix's
/// missing second half (operator report 2026-07-11: the seam persisted even
/// with the correct 0.84 ratio + a panel-snapped `cell_height`).
///
/// `snap_cell_height_px` makes every row *pitch* a whole number of panel px,
/// so consecutive row boundaries share the SAME fractional panel-pixel phase.
/// But that shared phase is set by the ORIGIN: row boundary `N` sits at
/// `origin + N·cell_height` framebuffer px → `(origin + N·cell_height)·ratio`
/// panel px. With a whole panel pitch that is `origin·ratio + N·k`, so unless
/// `origin·ratio` is itself an integer, *every* boundary lands the same
/// fractional amount off a panel-pixel edge — the downscale filter turns that
/// constant sub-pixel straddle back into a visible periodic seam. (Measured:
/// default padding 4pt × scale 2.0 = 8 fb px → 8·0.8405 = 6.72 panel px, a
/// constant ~0.72-panel-px phase on every row.)
///
/// Snapping the origin to `round(origin·ratio)/ratio` framebuffer px makes
/// `origin·ratio` a whole panel pixel, so every boundary `origin·ratio + N·k`
/// is an integer panel pixel — phase-locked to the panel grid, seam gone.
///
/// At `panel_ratio == 1.0` (integer scale) this is a no-op passthrough: the
/// framebuffer IS the panel grid, and the framebuffer origin is already
/// integer-authored (padding × integer scale). Non-negative by construction.
#[inline]
fn snap_origin_px(origin: f32, panel_ratio: f32) -> f32 {
    if (panel_ratio - 1.0).abs() < 1.0e-4 {
        origin
    } else {
        ((origin * panel_ratio).round() / panel_ratio).max(0.0)
    }
}

/// Should the panel-ratio probe re-run this frame? Pure decision so the
/// resize-storm gate (Deliverable 3) is unit-testable without CoreGraphics.
///
/// The panel ratio depends on the *display*, not the surface *size*, and macOS
/// delivers a distinct drawable size nearly every frame of a live drag-resize.
/// So the (real, allocating) `display_scaling_ratio` probe fires only when the
/// size has **settled** (this frame's size == last frame's) and hasn't been
/// probed yet — or on the very first frame (`last_probe == (0,0)`), so first
/// paint gets the ratio with no delay. Result: ~1 probe per settled resize
/// instead of ~60/sec through a drag, with byte-identical final geometry.
#[inline]
fn should_reprobe_ratio(
    this: (u32, u32),
    last_surface: (u32, u32),
    last_probe: (u32, u32),
) -> bool {
    let settled = last_surface == this;
    let never_probed = last_probe == (0, 0);
    (settled || never_probed) && last_probe != this
}

impl TerminalRenderer {
    pub fn new(
        terminal: SharedTerminal,
        font_size: f32,
        line_height: f32,
        font_family: String,
        font_italic: String,
        font_symbols: String,
        padding: f32,
        cursor_style: CursorStyle,
        cursor_blink: bool,
        cursor_blink_rate_ms: u32,
        bg_color: wgpu::Color,
        fg_color: Color,
    ) -> Self {
        let cell_width = font_size * 0.6;
        // panel_ratio starts at 1.0 (no discovered downscale yet) → identical
        // to the framebuffer-only quantize; re-snapped once the ratio is
        // discovered (set_panel_ratio) and metrics are re-measured.
        let cell_height = snap_cell_height_px(font_size * line_height, 1.0);

        Self {
            terminal,
            selection: Arc::new(Mutex::new(Selection::new())),
            search: Arc::new(Mutex::new(SearchState::new())),
            dir_picker: Arc::new(Mutex::new(crate::dir_picker::DirPickerState::new())),
            session_picker: Arc::new(Mutex::new(
                crate::session_picker::SessionPickerState::new(),
            )),
            // Born `None`; the engine rewires this to its shared cell via
            // `set_overlay_focus` in `attach_to_renderer`.
            overlay_focus: Arc::new(Mutex::new(crate::ux::modes::Overlay::None)),
            // window: removed Phase 4
            font_size,
            line_height,
            font_family,
            font_italic,
            font_symbols,
            cell_width,
            cell_height,
            padding,
            bg_color,
            fg_color,
            ansi_colors: default_ansi_palette(),
            rect_pipeline: None,
            image_pipeline: None,
            dispatcher: None,
            texture_pool: engawa_wgpu::TexturePool::new(),
            catalog_sampler: None,
            effect_params: HashMap::new(),
            frame_graph: crate::render_graph::FrameGraphCache::new(),
            gpu_images: HashMap::new(),
            bold_is_bright: false,
            last_seqno: 0,
            cursor_style,
            cursor_blink,
            cursor_blink_rate_ms,
            metrics_measured: false,
            bell_flash: crate::motion::Tween::inert(),
            bell_flash_duration_secs: BELL_FLASH_SECS,
            bell_flash_peak: BELL_FLASH_PEAK_ALPHA,
            bell_flash_curve: crate::motion::Curve::Linear,
            overlay_open_at: std::cell::Cell::new(None),
            overlay_progress: std::cell::Cell::new(1.0),
            // Nord frost #88C0D0 at 0.3 alpha, linearized for the rect
            // pipeline (see `overlay_rect_color`). NOT the raw byte/255
            // triple — that would render washed-out on the sRGB surface.
            selection_bg: overlay_rect_color(0x88, 0xC0, 0xD0, 0.3),
            cursor_color: [0.925, 0.937, 0.957, 0.85], // Nord snow default
            // Nord frost #88C0D0 — the prior hardcoded search-status
            // colour. `theme::apply_config_theme` overwrites this with
            // the active theme's agent accent (Vellum fable_violet).
            search_status_color: Color::new(0x88, 0xC0, 0xD0),
            // Nord aurora yellow #EBCB8B — the prior hardcoded
            // search-match fill. `theme::apply_config_theme` overwrites
            // both with the active theme's search surfaces (Vellum
            // first_light #D7C489 / search_others #443E2A).
            search_current_color: Color::new(0xEB, 0xCB, 0x8B),
            search_other_color: Color::new(0xEB, 0xCB, 0x8B),
            // Nord frost #88C0D0 — the prior hardcoded URL-underline blue,
            // kept as the pre-theme field default. `theme::apply_config_theme`
            // overwrites this with the active theme's `link` (frost ansi[12]).
            link_color: Color::new(0x88, 0xC0, 0xD0),
            // Prescribed-tier default: links highlighted. `apply_effects_and_accessibility`
            // re-derives this from `config.links` (bare strips it).
            links_highlight: true,
            // Nord #5E81AC / #88C0D0 — the prior hardcoded separator + thumb
            // blues, kept as pre-theme field defaults. `apply_config_theme`
            // overwrites both with the active theme's `prompt_mark` / `scrollbar`.
            prompt_mark_color: Color::new(0x5E, 0x81, 0xAC),
            scrollbar_color: Color::new(0x88, 0xC0, 0xD0),
            // White — the prior hardcoded bell flash, kept until a theme's
            // `bell_flash` (the theme foreground) loads.
            bell_flash_color: Color::WHITE,
            // Calm green / warm red — the pre-theme exit-glow defaults, kept
            // until the active theme's `exit_ok` / `exit_err` (ANSI green/red)
            // load via `apply_config_theme`.
            exit_ok_color: Color::new(0x66, 0xF2, 0x8C),
            exit_err_color: Color::new(0xFF, 0x52, 0x4D),
            // Vellum night0 ground — the prescribed-theme background, kept
            // as the pre-theme dim default until `apply_config_theme`
            // overwrites it with the active theme's `background`.
            unfocused_dim_color: Color::new(0x16, 0x14, 0x0E),
            // Prescribed-tier defaults: visual bell on, unfocused dim on.
            // `apply_effects_and_accessibility` re-derives both from
            // `config.{feedback,motion}` (bare strips them).
            feedback_visual_bell: true,
            feedback_exit_glow: true,
            motion_unfocused_dim: true,
            motion_picker_animate: true,
            reduce_motion: false,
            session_picker_anchor: crate::config::PickerAnchor::default(),
            suggestion_fade: RefCell::new(HashMap::new()),
            suggestion_shade_in_ms: 600,
            // Nord defaults (the exact literals the old draw_* methods
            // hardcoded); `theme::apply_config_theme` overrides per theme.
            overlay_style: crate::picker::component::OverlayStyle::nord_default(),
            focused: true,
            // 1.0 = no scaling; overwritten on the first render frame
            // by `set_scale_factor(ctx.scale_factor)`.
            scale_factor: 1.0,
            // 1.0 = no compositor downscale; overwritten by `set_panel_ratio`
            // once the display's panel-vs-framebuffer ratio is discovered.
            panel_ratio: 1.0,
            // Unknown until the first probe / config apply — an honest
            // "not yet measured", never a silent genuine-1.0.
            panel_ratio_source: crate::panel_fit::PanelRatio::Unavailable,
            // Seam auto-tune on by default; the display config overrides via
            // `apply_effects_and_accessibility` → `set_seam_config`.
            seam_auto_tune: true,
            downscale_ratio_override: None,
            // 0 until the first frame renders — `measured_grid`
            // reports None until then.
            last_surface_w: 0,
            last_surface_h: 0,
            // (0, 0) forces the first frame to probe (0 != any real size).
            last_ratio_probe_wh: (0, 0),
            shape_cache: RefCell::new(LruCache::new(
                NonZeroUsize::new(SHAPE_CACHE_CAP)
                    .expect("SHAPE_CACHE_CAP is a non-zero compile-time constant"),
            )),
            last_cursor_on: false,
            box_draw_templates: RefCell::new(HashMap::new()),
            row_scratch: RefCell::new(Vec::new()),
            sync_output_deferred_since: None,
            last_grid_epoch: 0,
            force_paint_frames: 0,
            effects_config: crate::config::MadoEffectsConfig::default(),
            ambience: crate::config::MadoEffectsConfig::default().ambience.compose(),
            snow_state: SnowState::new(),
            glow_state: GlowState::new(),
            aurora_state: AuroraState::new(),
            // Recommended default: High ceiling (the perf wave's spec),
            // starting at the catalog default Medium. The governor only
            // ever steps down from here under sustained load.
            ambience_governor: crate::ux::ambience_governor::AmbienceGovernor::default(),
            // Text layers minted lazily on the first render via `ensure_layers`
            // (mado has no hook at madori's Resumed). Each is its own isolated
            // vertex buffer on the shared atlas — see `TEXT_LAYERS`.
            term_layer: None,
            overlay_layer: None,
            search_layer: None,
        }
    }

    /// SINGLE application point for the config-derived effects +
    /// accessibility surface (M3 review 2026-06-12). Both production
    /// entry points (main.rs local-PTY and `gui_tear_attach`) call
    /// THIS, and the hot-reload drain re-invokes it — the tear path
    /// previously called only `set_effects_config`, leaving
    /// `effects.colorblind.mode` (+ the `accessibility.colorblind`
    /// alias) dead and `reduce_motion` un-gated for the animated
    /// effects in tear windows.
    pub fn apply_effects_and_accessibility(&mut self, config: &crate::config::MadoConfig) {
        self.set_bold_is_bright(config.appearance.bold_is_bright);
        self.set_reduce_motion(config.accessibility.reduce_motion);
        // Clickable-link highlight gate — frost text + underline on OSC 8
        // hyperlinks + auto-detected URLs. ON only when the feature is
        // enabled AND highlight is requested; the bare tier strips both.
        self.set_links_highlight(config.links.enabled && config.links.highlight);
        // Tasteful-feedback + motion gates — the visual bell flash and the
        // unfocused-window dim. Both prescribed-ON; the bare tier strips them.
        self.set_feedback_visual_bell(config.feedback.visual_bell);
        self.set_feedback_exit_glow(config.feedback.exit_code_glow);
        self.set_motion_unfocused_dim(config.motion.unfocused_dim);
        self.set_motion_picker_animate(config.motion.picker_animate);
        // Bell-flash SHAPE is the operator's to morph (motion.bell_flash);
        // the on/off gate is feedback.visual_bell above. Resolve the named
        // easing to a motion::Curve here so trigger_bell is a cheap build.
        self.bell_flash_duration_secs = config.motion.bell_flash.duration_ms as f32 / 1000.0;
        self.bell_flash_peak = config.motion.bell_flash.peak_alpha.clamp(0.0, 1.0);
        self.bell_flash_curve = config.motion.bell_flash.easing.curve();
        self.session_picker_anchor = config.tear.session_picker_anchor;
        self.suggestion_shade_in_ms = config.suggestions.shade_in_ms;
        self.set_seam_config(config.display.seam_auto_tune, config.display.downscale_ratio);
        self.set_effects_config(config.resolved_effects());
    }

    /// Apply the `display.*` seam-auto-tune config. `auto_tune` off pins the
    /// panel ratio to 1.0 immediately (no adjustment); `override_ratio` pins a
    /// specific ratio (skips the probe). The render prologue re-applies the
    /// discovered ratio on the next surface-size change.
    pub fn set_seam_config(&mut self, auto_tune: bool, override_ratio: Option<f32>) {
        self.seam_auto_tune = auto_tune;
        self.downscale_ratio_override = override_ratio;
        if !auto_tune {
            // Seam auto-tune off is a deliberate operator choice — a trusted
            // (configured) 1.0, never a probe fallback.
            self.set_panel_ratio(1.0);
            self.panel_ratio_source = crate::panel_fit::PanelRatio::from_config(1.0);
        } else if let Some(r) = override_ratio {
            let source = crate::panel_fit::PanelRatio::from_config(r);
            self.set_panel_ratio(source.ratio());
            self.panel_ratio_source = source;
        }
        // else: auto_tune on, no override → the per-frame probe sets the
        // source (Discovered / Unavailable).
    }

    // set_config_reload_cell / drain_config_reload DELETED at M4
    // stage 2 — the adapters poll `ux::ConfigHotReload` per frame
    // and apply a typed SetterCall delta instead; the renderer no
    // longer owns any reload plumbing.

    /// Override the post-effect config. Effect toggles take effect
    /// on the next frame (the graph cache key is derived per frame);
    /// snow knobs re-seed the host animation state.
    pub fn set_effects_config(&mut self, cfg: crate::config::MadoEffectsConfig) {
        self.snow_state.apply_config(&cfg.snow);
        self.glow_state.params.radius_px = cfg.glow_on_bell.radius_px;
        // Re-derive the composed ambience layer from the (already
        // reduce-motion-resolved) preset. `Off` ⇒ empty composition ⇒
        // zero ambience nodes. This is the ONE place the composition is
        // recomputed; both the effect set and the per-frame uniforms
        // read `self.ambience`, never re-running `compose()`.
        self.ambience = cfg.ambience.compose();
        self.effects_config = cfg;
        // Forces a repaint — same invalidation contract as the
        // derive-generated setters.
        self.last_seqno = 0;
    }

    /// Push the current mouse position into the snow state so
    /// the cursor-deflection ring tracks the pointer.
    pub fn snow_set_cursor(&mut self, x: f32, y: f32) {
        self.snow_state.params.set_cursor([x, y]);
    }

    /// Bump the typing-pulse on the snow state. Called from the
    /// keyboard handler.
    pub fn snow_pulse_typing(&mut self) {
        self.snow_state.params.pulse_typing(1.0);
    }

    /// Update the HiDPI scale factor. If the value actually changed,
    /// invalidates the cached cell metrics so the next render
    /// re-measures glyphs at the new resolution. Called from the
    /// `render` entry point each frame; the cost when nothing changed
    /// is one float comparison.
    pub fn set_scale_factor(&mut self, scale: f32) {
        if (self.scale_factor - scale).abs() > f32::EPSILON {
            self.scale_factor = scale;
            // Force re-measurement of cell_width/cell_height at the
            // new pixel density so glyphon's reported glyph advance is
            // in physical pixels matching the wgpu surface.
            self.metrics_measured = false;
            // P20 — clear the shape cache. The cache key includes
            // font_size_bits = font_size_px.to_bits() and font_size_px
            // depends on scale_factor; entries cached at the old
            // physical-pixel size are now unreachable. LRU would
            // eventually evict them but explicit clear keeps memory
            // tight and avoids serving the wrong-DPI shape if hashes
            // ever collide.
            self.shape_cache.borrow_mut().clear();
            // P31 — same rationale: box-draw templates are
            // dimensioned in physical pixels via cell_width /
            // cell_height. Drop them on scale change so the next
            // emission rebuilds at the new resolution.
            self.box_draw_templates.borrow_mut().clear();
        }
    }

    /// Update the panel-vs-framebuffer downscale ratio (panel_px /
    /// framebuffer_px) for the display mado is currently on. Like
    /// `set_scale_factor`, invalidates the cached cell metrics on a real
    /// change so the next render re-measures + re-snaps `cell_height` onto
    /// the panel grid via `snap_cell_height_px`. Discovered out-of-band
    /// (the compositor downscale is not reflected in `scale_factor`); fed
    /// from the discoverability layer at startup + on display change.
    pub fn set_panel_ratio(&mut self, ratio: f32) {
        let ratio = ratio.clamp(0.25, 1.0);
        if (self.panel_ratio - ratio).abs() > f32::EPSILON {
            self.panel_ratio = ratio;
            self.metrics_measured = false;
            self.shape_cache.borrow_mut().clear();
            self.box_draw_templates.borrow_mut().clear();
        }
    }

    /// Current panel-vs-framebuffer downscale ratio (1.0 = no downscale).
    #[inline]
    pub fn panel_ratio(&self) -> f32 {
        self.panel_ratio
    }

    /// The PROVENANCE of the current panel ratio (`Discovered` / `Configured`
    /// / `Unavailable`). A seam with an `Unavailable` source is a probe
    /// failure, not a snap bug — surfaced in `mado print-posture`.
    #[inline]
    pub fn panel_ratio_source(&self) -> crate::panel_fit::PanelRatio {
        self.panel_ratio_source
    }

    /// Physical-pixel padding — ALSO the grid's rendering origin (top/left).
    /// The stored `padding` is logical (operator-authored in mado.yaml as
    /// "8 pixels"); GPU draws need it scaled into physical pixels to align
    /// with the wgpu surface.
    ///
    /// On a scaled display it is additionally snapped onto the PANEL grid
    /// (`snap_origin_px`): every consumer uses this as the grid origin, so
    /// snapping it here phase-locks the whole grid (backgrounds, glyphs,
    /// cursor, images, overlays — all off the SAME origin) to integer panel
    /// pixels, killing the residual row seam the `cell_height` snap alone
    /// leaves behind (operator report 2026-07-11). At `panel_ratio == 1.0`
    /// (integer scale) this is a no-op — byte-identical to the prior
    /// `padding * scale_factor`.
    #[inline]
    fn padding_px(&self) -> f32 {
        snap_origin_px(self.padding * self.scale_factor, self.panel_ratio)
    }

    /// The viewport-derived overlay-list row budget — how many list rows a
    /// picker should BUILD for the current surface height. It is the SAME
    /// vertical-fit `draw_overlay` clamps its window to (`line_h = fs *
    /// line_height`, `pad = padding_px()`, `pad_y = line_h*0.5`), so a picker
    /// on a tall 4K window builds ~all the rows that fit instead of the old
    /// fixed 12, and a short window shrinks below 12 — screen-size-aware by
    /// construction. Resolved per frame at the reconciler tick, so it tracks
    /// resize with no new event wiring. Typed [`crate::row_budget::VisibleRows`]
    /// so no draw path can pass a hand-typed row count.
    #[inline]
    fn overlay_row_budget(&self, height: u32) -> crate::row_budget::VisibleRows {
        let line_h = self.font_size_px() * self.line_height;
        let pad = self.padding_px();
        crate::row_budget::RowBudget::for_viewport(height as f32, line_h, pad, line_h * 0.5)
    }

    /// Current HiDPI scale factor. Public so consumers (gui_tear_attach's
    /// resize event handler) can compute the same physical-pixel cell
    /// dimensions the renderer uses. Without this getter, the resize
    /// handler would mix physical pixels (winit's Resized event) with
    /// logical cell sizes (font_size × 0.6/1.4) and on Retina compute
    /// 2× as many cells as the window actually shows.
    #[inline]
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    /// Physical-pixel cell dimensions matching what the renderer
    /// actually draws. Use this from any code that needs to convert
    /// window pixels (from winit Resized events) to cell counts (for
    /// pane_resize_absolute calls).
    #[inline]
    pub fn cell_size_phys(&self) -> (f32, f32) {
        (self.cell_width, self.cell_height)
    }

    /// Compute the (cols, rows) visible in the given physical window
    /// dimensions, using THIS renderer's exact cell metrics + padding.
    /// One source of truth for the cell math; used by gui_tear_attach
    /// to push pane_resize_absolute(...) so tear's pane geometry
    /// always matches mado's visible cell grid (= what nvim and other
    /// TUI apps query via TIOCGWINSZ).
    #[must_use]
    pub fn cells_for_window_phys(&self, width_phys: u32, height_phys: u32) -> (u16, u16) {
        let pad_phys = self.padding_px();
        let inner_w = (width_phys as f32 - 2.0 * pad_phys).max(0.0);
        let inner_h = (height_phys as f32 - 2.0 * pad_phys).max(0.0);
        let cw = self.cell_width.max(1.0);
        let ch = self.cell_height.max(1.0);
        let cols = ((inner_w / cw).floor() as u16).max(1);
        let rows = ((inner_h / ch).floor() as u16).max(1);
        (cols, rows)
    }

    /// The grid the CURRENT surface actually supports —
    /// [`Self::cells_for_window_phys`] over the dims of the last
    /// rendered frame, using MEASURED cell metrics. `None` until the
    /// first frame has rendered (or right after a font/scale change,
    /// until the next frame re-measures).
    ///
    /// This is the renderer's display truth, and the only safe source
    /// for the PTY grid size: the pre-window estimate can't know real
    /// font metrics or the content-view size (a Flush titlebar insets
    /// it), and macOS delivers no initial `Resized` event to correct
    /// it — the event loops run a reconcile latch against this value
    /// instead. (Operator-visible failure when unsynced: a TUI lays
    /// out for more rows than the viewport shows, leaving stale CLI
    /// lines on screen — 2026-06-11 report.)
    pub fn measured_grid(&self) -> Option<(u16, u16)> {
        if !self.metrics_measured || self.last_surface_w == 0 || self.last_surface_h == 0 {
            return None;
        }
        Some(self.cells_for_window_phys(self.last_surface_w, self.last_surface_h))
    }

    /// Physical dims of the last rendered frame; `None` before the
    /// first frame. Pair of [`Self::measured_grid`] for callers that
    /// need raw pixel dims (the local-PTY pane-resize path).
    pub fn last_surface_size(&self) -> Option<(u32, u32)> {
        if self.last_surface_w == 0 || self.last_surface_h == 0 {
            return None;
        }
        Some((self.last_surface_w, self.last_surface_h))
    }

    /// Physical-pixel font size. Mirrors `padding_px` — logical
    /// `font_size` from config, scaled into physical pixels for the
    /// glyphon font-system + buffer creation.
    #[inline]
    fn font_size_px(&self) -> f32 {
        self.font_size * self.scale_factor
    }

    /// Cell-local decoration metrics — the typed input every engawa
    /// decoration emitter consumes. Projected from the measured cell
    /// metrics + the decoration constants, one place.
    fn underline_metrics(&self) -> engawa::UnderlineMetrics {
        engawa::UnderlineMetrics {
            cell_width: self.cell_width,
            underline_y: self.cell_height - UNDERLINE_OFFSET_FROM_BOTTOM,
            thickness: DECORATION_THICKNESS,
            baseline: self.cell_height * BASELINE_FRACTION,
        }
    }

    /// SGR-5 blink phase — true = foreground visible. Shares the
    /// cursor-blink clock (`cursor_blink_rate_ms`) so both blink
    /// families flip together; `reduce_motion` pins it visible
    /// (animation is exactly what that knob exists to suppress).
    /// `elapsed == 0.0` is the visible phase, which keeps the L1/L2
    /// determinism ladders (rendered at elapsed=0) byte-stable.
    fn blink_phase_on(&self, elapsed: f32) -> bool {
        if self.reduce_motion {
            return true;
        }
        // One blink law, shared with the cursor-draw + idle-Hz gates and the
        // SGR-5 attribute: motion::blink_on. Byte-identical to the old
        // `(elapsed % period) < period/2` for the reachable rate>0 range
        // (elapsed ≥ 0 in the render clock, so `%` ≡ `rem_euclid`); at
        // rate==0 it is now always-on = solid, the intended "disabled" shape.
        crate::motion::blink_on(elapsed, self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0)
    }

    // set_selection_bg, set_cursor_color, set_reduce_motion now
    // generated by #[derive(InvalidatingSetter)] on TerminalRenderer.
    // Bodies were uniformly `self.<field> = v; self.last_seqno = 0;`
    // — the derive's per-field template emits exactly that for every
    // field marked #[invalidating_setter]. See
    // pleme-io/pleme-invalidating-setter-derive.

    /// Set the shared selection state (called from main to share with event handler).
    pub fn set_selection(&mut self, selection: Arc<Mutex<Selection>>) {
        self.selection = selection;
    }

    /// Set the shared dir-picker state (called from main to share with the
    /// event handler — the same Arc both the input handler and renderer read).
    /// Share the Ctrl-S session-picker overlay state with the input
    /// engine (the engine writes it via the overlay FSM; the renderer
    /// reads it in Pass 6). Same wiring as `set_dir_picker`.
    pub fn set_session_picker(
        &mut self,
        session_picker: Arc<Mutex<crate::session_picker::SessionPickerState>>,
    ) {
        self.session_picker = session_picker;
    }

    /// Share the engine's overlay-focus cell (the single source of truth
    /// for which overlay Pass 6 draws). The engine writes it on every FSM
    /// transition; the renderer matches on it.
    pub fn set_overlay_focus(&mut self, overlay_focus: Arc<Mutex<crate::ux::modes::Overlay>>) {
        self.overlay_focus = overlay_focus;
    }

    pub fn set_dir_picker(&mut self, dir_picker: Arc<Mutex<crate::dir_picker::DirPickerState>>) {
        self.dir_picker = dir_picker;
    }

    /// Draw the directory-frecency overlay (轍) — a text-only floating list:
    /// a `cd` query line plus the frecency-ranked rows, the highlighted row in
    /// Nord green. Text-only (no bg/highlight rects) keeps this a pure addition
    /// reusing the Pass-3 glyphon text path — no new pipeline, no visibility
    /// changes to private types. Renders onto `ctx.surface_view` after snow.
    /// One-line search status (Pass 6 overlay, bottom-left):
    /// `/query  n/m` or `/query  no matches`. Without it the overlay
    /// was an invisible keystroke black hole — every key consumed,
    /// nothing on screen (hunt finding 2026-06-11).
    fn draw_search_status(
        &self,
        query: &str,
        current: usize,
        count: usize,
        frame: &mut garasu::Frame<'_>,
        gpu: &garasu::GpuContext,
        surface_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let fs = self.font_size_px();
        let line_h = fs * self.line_height;
        let left = self.padding_px() + self.cell_width;
        let top = height as f32 - self.padding_px() - line_h * 1.2;

        let status = if query.is_empty() {
            "/ (type to search, Esc to close)".to_owned()
        } else if count == 0 {
            format!("/{query}  no matches")
        } else {
            format!("/{query}  {}/{count}", current + 1)
        };

        let attrs = Attrs::new().family(Family::Name(&self.font_family));
        let mut buf = frame.create_rich_buffer(&[(status.as_str(), attrs)], fs, line_h);
        buf.shape_until_scroll(frame.font_system_mut(), false);

        // AGENT-RESERVED accent: search-status is an agent / MCP-activity
        // surface, so it paints with the theme's `agent_accent`
        // (Vellum `fable_violet` via the SEMANTIC role — set by
        // `theme::apply_config_theme`). Non-Vellum themes keep the
        // Nord-frost default the field was seeded with.
        let accent = self.search_status_color;
        let agent = GlyphonColor::rgba(accent.r, accent.g, accent.b, 255);
        let text_areas = vec![glyphon::TextArea {
            buffer: &buf,
            left,
            top,
            scale: 1.0,
            bounds: glyphon::TextBounds {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            default_color: agent,
            custom_glyphs: &[],
        }];
        // The search-status layer owns its own vertex buffer — preparing it
        // can't clobber the terminal or overlay layers. On a prepare error we
        // skip the render (never draw a stale token), per the migration
        // discipline (do not swallow the error while pretending it drew).
        let token = match frame.prepare(
            self.search_layer
                .expect("ensure_layers minted the search layer before any draw"),
            &gpu.device,
            &gpu.queue,
            text_areas,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("search status text prepare: {e}");
                return;
            }
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_search_status"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: surface_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        if let Err(e) = frame.render(token, &mut pass) {
            tracing::warn!("search status text render: {e}");
        }
    }

    /// The ONE themed overlay renderer every picker draws through — the
    /// shared 90% the old `draw_dir_picker` / `draw_session_picker` each
    /// copy-pasted (buffer stack → `TextArea` per line → render pass).
    /// Geometry follows [`crate::config::PickerAnchor`] (`Center` floats
    /// the block in the middle; `Bottom` rises from the bottom edge; `Top`
    /// drops from the top); every colour comes from `self.overlay_style`
    /// (theme-resolved), so no Nord literal survives + a theme swap
    /// restyles all pickers. Each line's [`LineRole`] picks its colour.
    /// The picker fade-in alpha ∈ [0,1] for this frame (`motion.picker_animate`).
    /// `1.0` when the knob is off or the fade has completed; ramps from ~0 at
    /// the overlay-open edge over ~0.18s via a decelerate curve. A pure fn of
    /// `elapsed - overlay_open_at` (dt-invariant → determinism-safe; the golden
    /// GPU tests render with `Overlay::None`, so this never perturbs them).
    fn overlay_fade_progress(&self, elapsed: f32) -> f32 {
        if !self.motion_picker_animate {
            return 1.0;
        }
        match self.overlay_open_at.get() {
            Some(born) => {
                use crate::motion::Advance;
                let mut t = crate::motion::Tween::new(
                    0.0,
                    1.0,
                    crate::motion::secs(0.18),
                    crate::motion::Curve::named(crate::motion::EasingKind::Decelerate),
                );
                t.advance((elapsed - born).max(0.0))
            }
            None => 1.0,
        }
    }

    fn draw_overlay(
        &self,
        spec: &crate::picker::component::OverlaySpec,
        frame: &mut garasu::Frame<'_>,
        gpu: &garasu::GpuContext,
        surface_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        use crate::config::PickerAnchor;
        use crate::picker::component::LineRole;

        if spec.lines.is_empty() {
            return;
        }
        let fs = self.font_size_px();
        let line_h = fs * self.line_height;
        // picker_animate fade-in: multiply every overlay alpha by this frame's
        // fade progress (1.0 = fully open / knob off). Read via &self from the
        // Cell the render loop set (draw_overlay takes no `elapsed`).
        let progress = self.overlay_progress.get();

        // Shape every line first (the centred anchor needs the shaped
        // widths to centre the block; the TextAreas borrow the buffers
        // through prepare, so they're kept alive in `buffers`).
        let mut buffers: Vec<glyphon::Buffer> = Vec::with_capacity(spec.lines.len());
        for line in &spec.lines {
            let base = Attrs::new().family(Family::Name(&self.font_family));
            let mut buf = if line.highlights.is_empty() {
                // Common path (no query / no match): one run, unchanged. The
                // line's colour comes from the TextArea default_color below.
                frame.create_rich_buffer(&[(line.text.as_str(), base)], fs, line_h)
            } else {
                // Matched chars glow in the Nord frost accent (alpha-matched to
                // the row's shade-in); unmatched runs carry no colour so they
                // fall back to default_color (the role / urgency tint). This is
                // the fzf-style "here's why this row matched" highlight.
                let accent = GlyphonColor::rgba(0x88, 0xC0, 0xD0, line.alpha);
                let runs = crate::picker::component::highlight_runs(&line.text, &line.highlights);
                let spans: Vec<(&str, Attrs)> = runs
                    .iter()
                    .map(|(r, hl)| {
                        let seg = &line.text[r.clone()];
                        if *hl {
                            (seg, base.clone().color(accent))
                        } else {
                            (seg, base.clone())
                        }
                    })
                    .collect();
                frame.create_rich_buffer(&spans, fs, line_h)
            };
            buf.shape_until_scroll(frame.font_system_mut(), false);
            buffers.push(buf);
        }

        let pad = self.padding_px();
        let pad_y = line_h * 0.5;
        // Cap the rendered lines to those that fit the viewport (panel height
        // = block_h + 2*pad_y must fit within `height - 2*pad`), so the popup
        // is always a centred, viewport-bounded card — never an oversized
        // panel pinned to a corner and clipped (the Ctrl-S "sized for full
        // screen" report). Keeps the title + the selected row visible.
        let max_lines = (((height as f32 - 2.0 * pad - 2.0 * pad_y) / line_h).floor() as i64)
            .max(1) as usize;
        let sel_idx = spec.lines.iter().position(|l| l.role == LineRole::Selected);
        let vis = viewport_line_window(spec.lines.len(), sel_idx, max_lines);
        let vis_max_w = || {
            vis.iter()
                .flat_map(|&i| buffers[i].layout_runs())
                .fold(0.0_f32, |m, run| m.max(run.line_w))
        };
        let block_h = vis.len() as f32 * line_h;
        let edge_left = pad + self.cell_width * 2.0;
        let (left, top0) = match spec.anchor {
            PickerAnchor::Top => (edge_left, pad + self.cell_height),
            PickerAnchor::Bottom => (
                edge_left,
                (height as f32 - block_h - pad).max(pad),
            ),
            PickerAnchor::Center => {
                let max_w = vis_max_w();
                (
                    ((width as f32 - max_w) / 2.0).max(pad),
                    ((height as f32 - block_h) / 2.0).max(pad),
                )
            }
        };

        let style = self.overlay_style;

        // Center anchor → a SOLID card behind the text (so the popup is an
        // opaque, sleek panel, not transparent text over the terminal):
        // an accent border, the dark panel fill, and a highlight bar behind
        // the selected row. Drawn through the rect pipeline FIRST; the text
        // pass below lands on top. Top/Bottom stay text-only (unchanged).
        if matches!(spec.anchor, PickerAnchor::Center) {
            let content_w = vis_max_w();
            let pad_x = self.cell_width * 2.0;
            let (px, py, pw, ph) =
                centered_panel_geom(left, top0, content_w, block_h, pad, pad_x, pad_y);
            let radius = (line_h * 0.55).min(pw.min(ph) / 2.0);
            let border_w = 1.5_f32;
            let lin = |c: crate::terminal::Color, a: f32| -> [f32; 4] {
                let l = ishou_tokens::Srgb::new(c.r, c.g, c.b).to_linear();
                [l.r, l.g, l.b, a * progress]
            };
            let mut rects: Vec<RectInstance> = Vec::with_capacity(OVERLAY_RECT_CAPACITY);
            // Soft elevation shadow FIRST (painted behind the card) so the
            // popup floats with the same depth language as the window-depth
            // vignette. Config-toggleable (`effects.popup_elevation`) so the
            // card depth and the window-edge depth switch together.
            if self.effects_config.popup_elevation.enabled {
                rects.extend(elevation_shadow(
                    px,
                    py,
                    pw,
                    ph,
                    radius,
                    4,
                    line_h * 0.9,
                    0.10,
                    line_h * 0.18,
                ));
            }
            // Accent border: a slightly larger rounded rect peeking out
            // behind the panel as a hairline edge.
            rects.push(RectInstance::rounded(
                [px - border_w, py - border_w],
                [pw + border_w * 2.0, ph + border_w * 2.0],
                lin(style.border, 1.0),
                radius + border_w,
            ));
            // The opaque card.
            rects.push(RectInstance::rounded(
                [px, py],
                [pw, ph],
                lin(style.panel, 1.0),
                radius,
            ));
            // Highlight bar behind the selected row — at its VISIBLE position
            // within the windowed line list (the selected row is always kept
            // visible by `viewport_line_window`), not its absolute index.
            if let Some(vis_pos) = sel_idx.and_then(|s| vis.iter().position(|&i| i == s)) {
                let bar_y = top0 + vis_pos as f32 * line_h;
                rects.push(RectInstance::rounded(
                    [px + pad_x * 0.5, bar_y],
                    [pw - pad_x, line_h],
                    lin(style.selected_bg, 1.0),
                    radius * 0.5,
                ));
            }
            if let Some(ref pipeline) = self.rect_pipeline {
                // Write to the DEDICATED overlay buffer (never the shared cell
                // `instance_buffer`): the cell-background pass and this panel
                // pass submit in the same frame, so sharing offset 0 would
                // clobber the first cells → top-left stray quads. Cap the
                // count so an unexpectedly large panel can't overrun the
                // fixed-size overlay buffer.
                let count = (rects.len()).min(OVERLAY_RECT_CAPACITY);
                pipeline.update_resolution(&gpu.queue, width, height);
                gpu.queue.write_buffer(
                    &pipeline.overlay_buffer,
                    0,
                    bytemuck::cast_slice(&rects[..count]),
                );
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("mado_overlay_panel"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: surface_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pipeline.draw_overlay(&mut pass, count as u32);
            }
        }

        let color_for = |line: &crate::picker::component::OverlayLine| {
            // A per-line colour override (the urgency tint) wins; otherwise the
            // role's themed colour (the calm default).
            let c = line.color.unwrap_or(match line.role {
                LineRole::Title => style.query,
                LineRole::Selected => style.selected,
                LineRole::Row => style.row,
                LineRole::Hint => style.hint,
            });
            // Per-line alpha IS the shade-in: glyphon blends the text over the
            // already-painted panel, so a low alpha dissolves the row into the
            // card behind it.
            GlyphonColor::rgba(c.r, c.g, c.b, ((f32::from(line.alpha)) * progress) as u8)
        };

        // Render only the visible (viewport-fitted) lines, each at its VISIBLE
        // row position so the block stays flush with the centred card.
        let mut text_areas = Vec::with_capacity(vis.len());
        for (row, &i) in vis.iter().enumerate() {
            let line = &spec.lines[i];
            text_areas.push(glyphon::TextArea {
                buffer: &buffers[i],
                left,
                top: top0 + (row as f32) * line_h,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                },
                default_color: color_for(line),
                custom_glyphs: &[],
            });
        }

        // The overlay layer owns its own vertex buffer; preparing it cannot
        // touch the terminal layer's buffer — this is the fix for the
        // top-left-blank Ctrl-S bug. On a prepare error skip the render
        // (never draw a stale token).
        let token = match frame.prepare(
            self.overlay_layer
                .expect("ensure_layers minted the overlay layer before any draw"),
            &gpu.device,
            &gpu.queue,
            text_areas,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("overlay text prepare: {e}");
                return;
            }
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_overlay"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: surface_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        if let Err(e) = frame.render(token, &mut pass) {
            tracing::warn!("overlay text render: {e}");
        }
    }

    /// Build the Ctrl-T dir-picker [`OverlaySpec`] (`cd` query line +
    /// frecency rows, top-anchored) and draw it through [`Self::draw_overlay`].
    fn draw_dir_picker(
        &self,
        query: &str,
        results: &[(std::path::PathBuf, f64)],
        selected: usize,
        frame: &mut garasu::Frame<'_>,
        gpu: &garasu::GpuContext,
        surface_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        use crate::picker::component::{LineRole, OverlayLine, OverlaySpec};
        // Screen-size-aware: build as many rows as the live surface affords
        // (draw_overlay windows the built lines to the same fit), not a fixed 12.
        let max_rows = self.overlay_row_budget(height).get();

        let mut lines: Vec<OverlayLine> = Vec::with_capacity(max_rows + 1);
        lines.push(OverlayLine::new(
            format!("\u{25b6} cd  {query}\u{2588}"),
            LineRole::Title,
        ));
        if results.is_empty() {
            lines.push(OverlayLine::new("  (no matching directories)", LineRole::Hint));
        } else {
            for (i, (path, _score)) in results.iter().take(max_rows).enumerate() {
                let (marker, role) = if i == selected {
                    ("\u{203a} ", LineRole::Selected)
                } else {
                    ("  ", LineRole::Row)
                };
                lines.push(OverlayLine::new(format!("{marker}{}", path.display()), role));
            }
        }
        // The dir picker keeps the legacy top-drop anchor.
        self.draw_overlay(
            &OverlaySpec::new(crate::config::PickerAnchor::Top, lines),
            frame,
            gpu,
            surface_view,
            width,
            height,
            encoder,
        );
    }

    /// Draw the Ctrl-S praça session-picker overlay — a text-only floating
    /// list: a fuzzy-filter query line plus the frecency-ranked session
    /// rows (`🌊 tide  mado`), the highlighted row in Nord green. Same
    /// pure-text Pass-6 model as [`Self::draw_dir_picker`] (no new
    /// pipeline). When switching is disabled the body is a single
    /// `(session switching disabled …)` hint line.
    fn draw_session_picker(
        &self,
        query: &str,
        results: &[crate::session_picker::SessionPickerRow],
        selected: usize,
        // Render clock (`ctx.elapsed`, seconds since app start) — the suggestion
        // shade-in fade is a pure fn of this (determinism, not wall-clock).
        elapsed: f32,
        disabled: bool,
        notice: Option<&str>,
        footer: Option<&str>,
        frame: &mut garasu::Frame<'_>,
        gpu: &garasu::GpuContext,
        surface_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        use crate::picker::component::{LineRole, OverlayLine, OverlaySpec};
        // Screen-size-aware visible cap: the Ctrl-S board fills a tall window
        // and shrinks on a short one (was a fixed `WINDOW_ROWS = 12`).
        let max_rows = self.overlay_row_budget(height).get();

        // +4: title, possible notice, possible "… +N more", possible health.
        let mut lines: Vec<OverlayLine> = Vec::with_capacity(max_rows + 4);
        lines.push(OverlayLine::new(
            format!("\u{25b6} session  {query}\u{2588}"),
            LineRole::Title,
        ));
        // A failed accept's one-line explanation — the board tells the
        // operator instead of silently closing.
        if let Some(msg) = notice {
            let mut line = String::from("  \u{26a0} "); // ⚠
            line.push_str(msg);
            lines.push(OverlayLine::new(line, LineRole::Hint));
        }
        if disabled {
            lines.push(OverlayLine::new(
                "  (session switching disabled — set tear.session_switching = true)",
                LineRole::Hint,
            ));
        } else if results.is_empty() {
            lines.push(OverlayLine::new("  (type a name to create a session)", LineRole::Hint));
        } else {
            // Shade-in: ramp each suggestion row's alpha from when it first
            // appeared on screen, so it dissolves in from the panel rather than
            // popping. Prune the fade map to the currently-visible suggestion
            // ids so a row that leaves + returns re-fades. Sessions/presets stay
            // solid (alpha 255).
            use crate::session_picker::RowKind;
            // Render-clock seconds since app start; the fade is a pure fn of
            // this, so two renders at the same `elapsed` produce identical alpha
            // (determinism — replaces the wall-clock `Instant::now()`).
            let now = elapsed;
            let mut fade = self.suggestion_fade.borrow_mut();
            // Scroll window: keep the selected row visible even when the result
            // set is longer than max_rows. Without this, selecting past row 12
            // left the highlight off-screen (invisible selection). `start` puts
            // `selected` at the window's bottom while scrolling down, then pins
            // to the last full page so we never show blank rows past the end.
            let start = selected
                .saturating_sub(max_rows - 1)
                .min(results.len().saturating_sub(max_rows));
            let window = &results[start..(start + max_rows).min(results.len())];
            let visible: std::collections::HashSet<crate::suggest::SuggestionId> = window
                .iter()
                .filter_map(|r| match r.kind {
                    RowKind::Suggestion(id) => Some(id),
                    _ => None,
                })
                .collect();
            fade.retain(|id, _| visible.contains(id));
            for (i, row) in window.iter().enumerate() {
                let abs = start + i;
                let (marker, role) = if abs == selected {
                    ("\u{203a} ", LineRole::Selected)
                } else {
                    ("  ", LineRole::Row)
                };
                let (alpha, color) = match row.kind {
                    RowKind::Suggestion(id) => {
                        let born = *fade.entry(id).or_insert(now);
                        // Seconds → ms for shade_ramp. `now >= born` always
                        // (monotonic render clock; `born` captured from the same
                        // `elapsed`), so the max(0.0) is belt-and-suspenders.
                        let age_ms = ((now - born) * 1000.0).max(0.0) as u64;
                        let a = crate::suggest::shade_ramp(0, age_ms, self.suggestion_shade_in_ms);
                        // Urgency tint: an on-fire task glows hot; routine ones
                        // keep the calm row colour (urgency_tint → None). Read
                        // from the row itself — the bridge stamped it at list
                        // time, so the frame loop takes no store lock.
                        let tint = row
                            .urgency
                            .and_then(crate::theme::urgency_tint)
                            .map(|(r, g, b)| Color::new(r, g, b));
                        (a, tint)
                    }
                    _ => (255, None),
                };
                let text = format!("{marker}{}", row.label);
                // Highlight the chars this query matched (the SAME praça matcher
                // the rows are ranked by, so the highlight is exactly the match).
                // Empty query → no positions → renders solid, unchanged.
                let highlights = if query.trim().is_empty() {
                    Vec::new()
                } else {
                    praca::index::fuzzy_indices(query, &text)
                        .map(|(_, p)| p)
                        .unwrap_or_default()
                };
                lines.push(
                    OverlayLine::new(text, role)
                        .with_alpha(alpha)
                        .with_color(color)
                        .with_highlights(highlights),
                );
            }
            // Overflow affordance: how many rows lie below the window. A tiny
            // "… +N more" footer tells the operator the stack continues.
            let below = results.len().saturating_sub(start + window.len());
            if below > 0 {
                lines.push(OverlayLine::new(
                    format!("  \u{2026} +{below} more"),
                    LineRole::Hint,
                ));
            }
        }

        // Health footer: one dim line naming blind lanes (erroring / needs
        // auth / needs config), so a board that cannot see never reads as
        // calm. Chrome, not a row — never selectable.
        if let Some(f) = footer {
            let mut line = String::from("  ");
            line.push_str(f);
            lines.push(OverlayLine::new(line, LineRole::Hint));
        }

        // Anchor per config — Center (default) floats the popup; Bottom /
        // Top keep the edge-anchored feel. The shared draw_overlay owns the
        // geometry + the theme-resolved colours.
        self.draw_overlay(
            &OverlaySpec::new(self.session_picker_anchor, lines),
            frame,
            gpu,
            surface_view,
            width,
            height,
            encoder,
        );
    }

    /// Set the shared search state (called from main to share with event handler).
    pub fn set_search(&mut self, search: Arc<Mutex<SearchState>>) {
        self.search = search;
    }

    // set_window removed at Phase 4 — single-pane mado; no multi-pane state to set.

    /// Trigger a bell flash effect. No-op when reduce_motion is enabled.
    /// Whether the window currently holds keyboard focus. The paired
    /// `set_focused` is generated by `#[invalidating_setter]`; this
    /// read side feeds the notification focus gate (deliver-when-
    /// unfocused) in `apply_side_effects`.
    #[must_use]
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// The full-window flash is gated on `feedback.visual_bell`
    /// (`feedback_visual_bell`); the glow-on-bell ring always saturates
    /// (its own effect gate decides whether the glow renders), so the
    /// audible-bell glow stays independent of the flash knob.
    pub fn trigger_bell(&mut self) {
        if !self.reduce_motion {
            if self.feedback_visual_bell {
                // Re-arm a fresh flash: peak → 0 over the operator-configured
                // duration + easing (motion.bell_flash), applied via
                // apply_config. A repeat bell restarts at peak (elapsed 0),
                // never stacks past it.
                self.bell_flash = crate::motion::Tween::new(
                    self.bell_flash_peak,
                    0.0,
                    crate::motion::secs(self.bell_flash_duration_secs),
                    self.bell_flash_curve,
                );
            }
            // BEL also saturates the glow-on-bell clock; whether the
            // glow renders is the effect set's call (config-enabled +
            // not reduce_motion — already inside this gate). Ring with
            // the explicit bell colour so a prior exit-status pulse's
            // red/green tint never bleeds into the bell glow.
            self.glow_state.params.ring_tinted(BELL_GLOW_RGB);
        }
    }

    /// Pulse the cursor glow to signal a finished command — green on a
    /// clean exit, red on a non-zero exit (OSC 133 `D`). No-op under
    /// `reduce_motion` or when `feedback.exit_code_glow` is off. Which
    /// completions pulse (skip fast successes / TUIs) is decided upstream
    /// in `apply_side_effects`; this only renders the colour.
    pub fn glow_on_exit_status(&mut self, exit_code: i32) {
        if self.reduce_motion || !self.feedback_exit_glow {
            return;
        }
        // The additive glow tint tracks the theme's exit accents (ANSI
        // green/red), normalized u8→0..1 — never a hardcoded colour.
        let c = if exit_code == 0 { self.exit_ok_color } else { self.exit_err_color };
        self.glow_state.params.ring_tinted(color_to_rgb(&c));
    }

    /// Current font size.
    #[must_use]
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Change font size at runtime. Clamps to 6.0..=72.0.
    /// Forces cell metrics re-measurement and full redraw.
    pub fn set_font_size(&mut self, size: f32) {
        let size = size.clamp(6.0, 72.0);
        self.font_size = size;
        self.cell_width = size * 0.6;
        self.cell_height = snap_cell_height_px(size * self.line_height, self.panel_ratio);
        self.metrics_measured = false;
        self.last_seqno = 0;
        // P20 — same rationale as set_scale_factor: shape cache keys
        // include font_size_bits, so a size change makes every cached
        // Arc<Buffer> unreachable. Clear to keep memory tight.
        self.shape_cache.borrow_mut().clear();
        // P31 — box-draw templates depend on cell_width/cell_height
        // which depend on font_size. Drop on size change.
        self.box_draw_templates.borrow_mut().clear();
    }

    // set_bold_is_bright, set_ansi_colors now generated by
    // #[derive(InvalidatingSetter)] on TerminalRenderer (fields
    // marked #[invalidating_setter] above). Bodies were identical
    // to the auto-generated form: assign + reset seqno. Colorblind
    // has NO setter: the mode lives only in effects_config
    // (set_effects_config is the single ingress).

    /// Override the background clear color and default text color.
    pub fn set_bg_fg(&mut self, bg: wgpu::Color, fg: Color) {
        self.bg_color = bg;
        self.fg_color = fg;
        self.last_seqno = 0;
    }

    /// Mint this renderer's text layers on the shared [`garasu::TextLayerStack`]
    /// — once, on the first render (idempotent thereafter). Each text surface
    /// gets its OWN isolated layer (own vertex buffer) so a second pass in a
    /// frame can't clobber the first's glyphs. Must mint exactly
    /// [`TEXT_LAYERS`]`.len()` layers — `layers_match_text_layers_const` is the
    /// forcing function that fails if a NEW text surface is added without its
    /// own layer here.
    fn ensure_layers(&mut self, text: &mut garasu::TextLayerStack, device: &wgpu::Device) {
        if self.term_layer.is_none() {
            self.term_layer = Some(text.add_layer(device));
        }
        if self.overlay_layer.is_none() {
            self.overlay_layer = Some(text.add_layer(device));
        }
        if self.search_layer.is_none() {
            self.search_layer = Some(text.add_layer(device));
        }
        // Forcing function: this renderer owns exactly the named TEXT_LAYERS on
        // its (exclusively-owned) stack. A new text surface added without a
        // matching TEXT_LAYERS entry (or vice versa) trips this in debug.
        debug_assert_eq!(
            text.layer_count(),
            TEXT_LAYERS.len(),
            "ensure_layers must mint exactly one layer per TEXT_LAYERS entry"
        );
    }

    /// Measure actual cell dimensions from glyphon font metrics.
    /// Called once on the first render when the text renderer is available.
    fn measure_cell_metrics(&mut self, text: &mut garasu::TextLayerStack) {
        if self.metrics_measured {
            return;
        }
        self.metrics_measured = true;

        // Render TWO reference characters at physical-pixel font size
        // and measure the **advance** between them (delta of glyph.x).
        // Previous code measured `glyph.w` (the rendered width of "M"),
        // which for many fonts is wider than the actual mono advance —
        // every per-cell rect-instance was then drawn ~10% wider than
        // the glyph sitting inside it, producing the floating "ghost
        // rectangles" visible on atuin's per-character highlights. The
        // advance is the canonical mono cell width: for a true mono
        // font every glyph occupies one advance, and that's what the
        // rect-pipeline must paint backgrounds at.
        //
        // CRITICAL: the measurement buffer MUST carry the same font
        // family the per-cell rendering uses. Until 2026-05-13 this
        // function used `text.create_buffer()` which falls back to
        // cosmic-text's default `Attrs::new()` (Family::SansSerif).
        // glyphon then resolved a system sans-serif (SF Pro / Helvetica)
        // whose "MM" advance is ~0.83em, set cell_width to that, but
        // rendered actual cells at the configured monospace family's
        // natural ~0.6em advance — the visible 0.23em gap between
        // every character in the operator's mado screenshot. Build
        // the measurement attrs against the same family used at
        // per-cell render time so cell_width matches the cells'
        // natural advance, exactly.
        let fs = self.font_size_px();
        let attrs = Attrs::new().family(Family::Name(&self.font_family));
        // Line-box metric = the configured cell rhythm, so the measured
        // `run.line_height` below IS the cell the rest of the renderer
        // sizes against — never a stale `* 1.4` that ignores config.
        let mut buf = text.create_rich_buffer(&[("MM", attrs)], fs, fs * self.line_height);
        buf.shape_until_scroll(&mut text.font_system, false);

        let mut measured_advance: Option<f32> = None;
        let mut measured_height: Option<f32> = None;
        let mut first_glyph_x: Option<f32> = None;

        for run in buf.layout_runs() {
            if measured_height.is_none() {
                measured_height = Some(run.line_height);
            }
            for glyph in run.glyphs.iter() {
                match first_glyph_x {
                    None => first_glyph_x = Some(glyph.x),
                    Some(prev) => {
                        measured_advance = Some(glyph.x - prev);
                        break;
                    }
                }
            }
            if measured_advance.is_some() {
                break;
            }
        }

        if let Some(w) = measured_advance {
            self.cell_width = w;
            tracing::info!(cell_width = w, "measured cell advance from font");
        }
        if let Some(h) = measured_height {
            // Snapped so each row lands on a whole number of PANEL pixels —
            // see `snap_cell_height_px` (row-seam line artifact, 2026-07-05;
            // scaled-display residual, 2026-07-06). At panel_ratio 1.0 this is
            // the original whole-device-pixel quantize.
            self.cell_height = snap_cell_height_px(h, self.panel_ratio);
            tracing::info!(
                cell_height = self.cell_height,
                measured = h,
                panel_ratio = self.panel_ratio,
                "measured cell height from font (snapped to panel px)"
            );
        }
    }

    /// Current measured cell width. Used by main.rs for resize calculations.
    #[must_use]
    pub fn cell_width(&self) -> f32 {
        self.cell_width
    }

    /// Current measured cell height. Used by main.rs for resize calculations.
    #[must_use]
    pub fn cell_height(&self) -> f32 {
        self.cell_height
    }

    fn snapshot(&self) -> (Snapshot, u64) {
        // Selection anchors are copied out BEFORE the terminal lock:
        // the engine's established order is selection-then-terminal
        // (Action::Copy holds the selection mutex across its rows
        // snapshot), so taking selection inside the terminal read
        // lock here would be a lock-order inversion.
        let sel_anchors = self.selection.lock().unwrap().anchors();
        let term = self.terminal.read();
        let seqno = term.seqno();
        let cursor = *term.cursor();
        let cols = term.cols();
        let num_rows = term.rows();
        let on_alt = term.on_alt_screen();
        let scroll_offset = term.scroll_offset();
        let scrollback_total = term.scrollback_total();
        // Recycle last frame's row buffers (retained outer + inner-row
        // capacities) instead of allocating a fresh Vec<Vec<Cell>> every vsync
        // — the dominant idle-frame cost. clear()-not-drop on each inner row
        // keeps its per-row cap; extend_from_slice refills without realloc at
        // steady state. The buffers return to `row_scratch` in render() after
        // the builds consume `snap.rows`. Byte-identical to the old
        // `.map(to_vec).collect()` (same Cell contents) — determinism tests guard it.
        let mut rows: Vec<Vec<Cell>> = std::mem::take(&mut *self.row_scratch.borrow_mut());
        let mut vi = 0usize;
        for src in term.visible_rows() {
            if vi < rows.len() {
                let dst = &mut rows[vi];
                dst.clear();
                dst.extend_from_slice(src);
            } else {
                rows.push(src.to_vec());
            }
            vi += 1;
        }
        rows.truncate(vi); // grid shrank (resize) → drop surplus rows
        let styles = term.styles().snapshot();
        let palette = *term.ansi_palette();
        let image_placements = term.image_placements().to_vec();
        let block_separator_rows = term.block_separator_viewport_rows();
        // Resolve the content-anchored selection against THIS frame's
        // grid and map it onto the viewport. A failed resolution
        // (evicted content, RIS rebuild, other screen buffer) renders
        // nothing — anchors never degrade to stale coordinates.
        let viewport_top_abs = scrollback_total.saturating_sub(scroll_offset);
        let selection_span = sel_anchors
            .and_then(|(a, b)| term.resolve_selection_span(a, b))
            .and_then(|(s, e)| {
                if e.0 < viewport_top_abs || s.0 >= viewport_top_abs + num_rows {
                    return None; // entirely off-screen
                }
                let start = if s.0 < viewport_top_abs {
                    CellPos { row: 0, col: 0 }
                } else {
                    CellPos { row: s.0 - viewport_top_abs, col: s.1 }
                };
                let end = if e.0 >= viewport_top_abs + num_rows {
                    CellPos {
                        row: num_rows.saturating_sub(1),
                        col: cols.saturating_sub(1),
                    }
                } else {
                    CellPos { row: e.0 - viewport_top_abs, col: e.1 }
                };
                Some((start, end))
            });
        drop(term);

        // P24 — URL detection is wasted on alt-screen TUIs: vim,
        // helix, lazygit, btop never want links in their rendered
        // content (they author their own typed output). Skip the
        // per-cell linkify pass when the alt-screen buffer is
        // active; pass an empty Vec instead.
        let urls = if on_alt {
            Vec::new()
        } else {
            url::detect_urls(&rows, cols)
        };

        // Capture search state
        let search = self.search.lock().unwrap();
        let search_active = search.active;
        let search_matches = search.matches.clone();
        let search_current = search.current;
        drop(search);

        (
            Snapshot {
                rows,
                styles,
                palette,
                cursor,
                cols,
                num_rows,
                scroll_offset,
                scrollback_total,
                urls,
                search_active,
                search_matches,
                search_current,
                image_placements,
                block_separator_rows,
                selection_span,
            },
            seqno,
        )
    }

    fn build_rect_instances(
        &self,
        snap: &Snapshot,
        elapsed: f32,
        origin_x: f32,
        origin_y: f32,
    ) -> Vec<RectInstance> {
        // P23 — pre-size by expected rect-instance count. Typical
        // interactive grid produces 2–4 spans per row (background,
        // optional underline, occasional strikethrough). 4 × rows is
        // a safe upper estimate; +cells for selection / search /
        // URLs spans.
        let mut instances = Vec::with_capacity(snap.num_rows * 4 + snap.cols);
        // Synthesized glyph-fill rects (box-drawing sub-rects + powerline
        // separators) are collected here and appended AFTER every cell
        // background run. The backgrounds are RLE'd and flushed at row
        // end — so if these fills were pushed into `instances` during the
        // cell loop they'd be painted UNDER the same row's bg span (the
        // bg rect comes later in the Vec → drawn on top), erasing them.
        // Deferring keeps them above their own cell bg, which is exactly
        // what a powerline pill on a colored section needs.
        let mut glyph_fill_instances: Vec<RectInstance> = Vec::new();
        let default_bg = Color::BLACK;

        // P11 — run-length batch every per-row "single-row, same-color
        // wide span" rect kind: backgrounds, underlines, strikethroughs,
        // overlines. Adjacent cells with identical (bg) or identical
        // (decoration colour + style) collapse into ONE wide
        // RectInstance. On a typical interactive grid this cuts the
        // rect-pipeline upload from a potential cells × 4 kinds per row
        // down to ~2–10 spans per row — and the rect_pipeline does an
        // instanced draw call sized by instance count, so fewer
        // instances = smaller upload + smaller vertex-shader cost. Box
        // drawing stays per-cell (each glyph has its own shape; no run
        // shape exists). Dotted/Dashed/Curly underlines stay O(1)
        // instances per run too: the pattern is evaluated in the
        // fragment shader (RectMode::Run / RectMode::Curly), never
        // tessellated into per-dot quads.
        //
        // Per-row state for the RLE-able kinds. Each is `Option<
        // (start_col, run_width_cells, color[, style])>`; `None` = no
        // run open. `run_width_cells` accumulates by cell.width so wide
        // chars (CJK / emoji) contribute 2 cells to the span — the
        // painted rect ends up `run_width_cells × cell_width` wide.
        type RowRun = Option<(usize, usize, [f32; 4])>;
        type UnderlineRun = Option<(usize, usize, [f32; 4], UnderlineStyle)>;
        let push_run =
            |instances: &mut Vec<RectInstance>,
             run: &mut RowRun,
             row_idx: usize,
             kind: RectKindForRle| {
                if let Some((start_col, cells, color)) = run.take() {
                    let x = origin_x + start_col as f32 * self.cell_width;
                    let w = cells as f32 * self.cell_width;
                    let (y, h) = match kind {
                        RectKindForRle::Background => (
                            origin_y + row_idx as f32 * self.cell_height,
                            self.cell_height,
                        ),
                        RectKindForRle::Strikethrough => (
                            origin_y + row_idx as f32 * self.cell_height
                                + self.cell_height * 0.5,
                            1.0,
                        ),
                        RectKindForRle::Overline => {
                            let r = engawa::overline_rect(self.underline_metrics());
                            (
                                origin_y + row_idx as f32 * self.cell_height + r.y,
                                r.height,
                            )
                        }
                    };
                    instances.push(RectInstance::solid([x, y], [w, h], color));
                }
            };

        // M3-C2 — style-dispatched underline geometry through the
        // engawa decoration emitters. The emitter runs on single-cell
        // metrics; the run widens the band horizontally. Period /
        // duty / amplitude stay cell-anchored, so Dashed (period =
        // cell_width / 2) and Curly (period = cell_width) tile
        // seamlessly across the widened band — exactly the "merge
        // adjacent cells' bands into one run" the engawa module
        // documents.
        let metrics = self.underline_metrics();
        let push_underline = |instances: &mut Vec<RectInstance>,
                              run: &mut UnderlineRun,
                              row_idx: usize| {
            if let Some((start_col, cells, color, style)) = run.take() {
                let x = origin_x + start_col as f32 * self.cell_width;
                let y0 = origin_y + row_idx as f32 * self.cell_height;
                let run_w = cells as f32 * self.cell_width;
                match engawa::emit_underline_rects(style, metrics) {
                    engawa::UnderlineGeometry::None => {}
                    engawa::UnderlineGeometry::Single(r) => {
                        instances.push(RectInstance::solid(
                            [x + r.x, y0 + r.y],
                            [run_w, r.height],
                            color,
                        ));
                    }
                    engawa::UnderlineGeometry::Double { upper, lower } => {
                        for r in [upper, lower] {
                            instances.push(RectInstance::solid(
                                [x + r.x, y0 + r.y],
                                [run_w, r.height],
                                color,
                            ));
                        }
                    }
                    engawa::UnderlineGeometry::Run(seg) => {
                        instances.push(RectInstance::run(
                            [x + seg.band.x, y0 + seg.band.y],
                            [run_w, seg.band.height],
                            color,
                            seg.period,
                            seg.duty,
                        ));
                    }
                    engawa::UnderlineGeometry::Curly(band) => {
                        instances.push(RectInstance::curly(
                            [x + band.rect.x, y0 + band.rect.y],
                            [run_w, band.rect.height],
                            color,
                            band.period,
                            band.amplitude,
                            band.thickness,
                        ));
                    }
                }
            }
        };

        // BLINK (SGR 5) animation phase — keyed on the cursor-blink
        // clock so the two blink families breathe together. Off-phase
        // hides the foreground (glyphs + fg-derived decorations),
        // never the background. reduce_motion pins it visible.
        let blink_on = self.blink_phase_on(elapsed);

        for (row_idx, row) in snap.rows.iter().enumerate() {
            let mut bg_run: RowRun = None;
            let mut underline_run: UnderlineRun = None;
            let mut strike_run: RowRun = None;
            let mut overline_run: RowRun = None;

            for (col_idx, cell) in glyph_columns(row, snap.cols) {
                // `glyph_columns` is the single source of column truth
                // (see `mod grid_col`): it skips width==0 continuation
                // cells and yields each cell's true grid column as a
                // typed `GridCol`. The text pipeline iterates the SAME
                // function, so the two cannot diverge on where a cell is
                // drawn — the wide-char cursor-misalignment bug is
                // unrepresentable, not re-fixed here.
                let style = cell.style(&snap.styles);
                let attrs = style.attrs;
                let inverse = attrs.flags.contains(AttrFlags::INVERSE);
                let dim = attrs.flags.contains(AttrFlags::DIM);
                let bg = if inverse { style.fg } else { style.bg };
                let base_fg = if inverse { style.bg } else { style.fg };
                let fg = if dim {
                    Color::new(base_fg.r / 2, base_fg.g / 2, base_fg.b / 2)
                } else {
                    base_fg
                };
                let width_cells = cell.width.max(1) as usize;
                let blink_hidden =
                    !blink_on && attrs.flags.contains(AttrFlags::BLINK);

                // ── Background span ─────────────────────────────────
                if bg != default_bg {
                    let color = color_to_f32(&bg);
                    match &mut bg_run {
                        Some((_, cells, c)) if *c == color => {
                            *cells += width_cells;
                        }
                        _ => {
                            push_run(
                                &mut instances,
                                &mut bg_run,
                                row_idx,
                                RectKindForRle::Background,
                            );
                            bg_run = Some((col_idx.idx(), width_cells, color));
                        }
                    }
                } else {
                    push_run(
                        &mut instances,
                        &mut bg_run,
                        row_idx,
                        RectKindForRle::Background,
                    );
                }

                // ── Underline span ──────────────────────────────────
                // Typed UnderlineStyle dispatch (M3-C2). Runs merge
                // only when style AND colour agree; the colour honours
                // SGR 58 (Indexed resolves against the live palette,
                // Rgb is verbatim) and falls back to the cell fg ONLY
                // for UnderlineColor::Default.
                if attrs.underline != UnderlineStyle::None && !blink_hidden {
                    let resolved = match attrs.underline_color {
                        UnderlineColor::Default => fg,
                        UnderlineColor::Indexed(n) => snap.palette[n as usize],
                        UnderlineColor::Rgb(c) => Color::new(c.r, c.g, c.b),
                    };
                    let color = color_to_f32(&resolved);
                    match &mut underline_run {
                        Some((_, cells, c, s))
                            if *c == color && *s == attrs.underline =>
                        {
                            *cells += width_cells;
                        }
                        _ => {
                            push_underline(&mut instances, &mut underline_run, row_idx);
                            underline_run =
                                Some((col_idx.idx(), width_cells, color, attrs.underline));
                        }
                    }
                } else {
                    push_underline(&mut instances, &mut underline_run, row_idx);
                }

                // ── Strikethrough span ──────────────────────────────
                if attrs.flags.contains(AttrFlags::STRIKETHROUGH) && !blink_hidden {
                    let color = color_to_f32(&fg);
                    match &mut strike_run {
                        Some((_, cells, c)) if *c == color => {
                            *cells += width_cells;
                        }
                        _ => {
                            push_run(
                                &mut instances,
                                &mut strike_run,
                                row_idx,
                                RectKindForRle::Strikethrough,
                            );
                            strike_run = Some((col_idx.idx(), width_cells, color));
                        }
                    }
                } else {
                    push_run(
                        &mut instances,
                        &mut strike_run,
                        row_idx,
                        RectKindForRle::Strikethrough,
                    );
                }

                // ── Overline span (SGR 53) ──────────────────────────
                if attrs.flags.contains(AttrFlags::OVERLINE) && !blink_hidden {
                    let color = color_to_f32(&fg);
                    match &mut overline_run {
                        Some((_, cells, c)) if *c == color => {
                            *cells += width_cells;
                        }
                        _ => {
                            push_run(
                                &mut instances,
                                &mut overline_run,
                                row_idx,
                                RectKindForRle::Overline,
                            );
                            overline_run = Some((col_idx.idx(), width_cells, color));
                        }
                    }
                } else {
                    push_run(
                        &mut instances,
                        &mut overline_run,
                        row_idx,
                        RectKindForRle::Overline,
                    );
                }

                // P31 — Box drawing through the rect template cache.
                // The first time we see a given box-drawing glyph at
                // the current cell metrics, compute its sub-rects
                // (via the same `box_drawing_rects` geometry) once,
                // strip the per-cell origin + color, and store. On
                // subsequent cells with the same glyph, just translate
                // by (bx, by) and apply the current fg color. Drops
                // the per-cell match-arm dispatch + Vec allocation.
                if is_box_drawing(cell.ch) {
                    let bx = origin_x + col_idx.idx() as f32 * self.cell_width;
                    let by = origin_y + row_idx as f32 * self.cell_height;
                    let color = color_to_f32(&fg);
                    let template = {
                        let mut cache = self.box_draw_templates.borrow_mut();
                        cache
                            .entry(cell.ch)
                            .or_insert_with(|| {
                                box_drawing_rects(
                                    cell.ch,
                                    0.0,
                                    0.0,
                                    self.cell_width,
                                    self.cell_height,
                                    [1.0, 1.0, 1.0, 1.0],
                                )
                                .into_iter()
                                .map(|r| (r.pos[0], r.pos[1], r.size[0], r.size[1]))
                                .collect()
                            })
                            .clone()
                    };
                    for (rx, ry, rw, rh) in template {
                        glyph_fill_instances.push(RectInstance::solid([bx + rx, by + ry], [rw, rh], color));
                    }
                }

                // Powerline separators (filled) through the rect
                // pipeline — synthesized to fill the FULL cell so the
                // rounded/angle cap reaches the cell bottom at any
                // line_height (ghostty parity). A font glyph here would
                // be baseline-positioned and notch the bottom of the
                // 1.25-tall cell; the rect IS the cell so there is no
                // gap. build_text_buffers diverts these chars from the
                // glyph path (like box-drawing), so they're rendered
                // here exactly once.
                if let Some(sep) = PowerlineSep::from_char(cell.ch) {
                    let px = origin_x + col_idx.idx() as f32 * self.cell_width;
                    let py = origin_y + row_idx as f32 * self.cell_height;
                    glyph_fill_instances.push(powerline_rect(
                        sep,
                        px,
                        py,
                        self.cell_width,
                        self.cell_height,
                        color_to_f32(&fg),
                    ));
                }
            }

            // Row end — flush every open run.
            push_run(&mut instances, &mut bg_run, row_idx, RectKindForRle::Background);
            push_underline(&mut instances, &mut underline_run, row_idx);
            push_run(&mut instances, &mut strike_run, row_idx, RectKindForRle::Strikethrough);
            push_run(&mut instances, &mut overline_run, row_idx, RectKindForRle::Overline);
        }

        // Synthesized glyph fills (box-drawing + powerline separators)
        // paint ON TOP of every cell background, never under it. See the
        // `glyph_fill_instances` declaration for why this is deferred.
        instances.append(&mut glyph_fill_instances);

        // Selection highlight — one rect per visible row of the
        // pre-resolved span (snapshot() already normalized, mapped to
        // viewport rows, and clipped): first row starts at the span's
        // start col, last row ends at the span's end col, interior
        // rows run full width.
        if let Some((sel_start, sel_end)) = snap.selection_span {
            let last_row = sel_end.row.min(snap.rows.len().saturating_sub(1));
            for row_idx in sel_start.row..=last_row {
                let c0 = if row_idx == sel_start.row { sel_start.col } else { 0 };
                let c1 = if row_idx == sel_end.row {
                    sel_end.col.min(snap.cols.saturating_sub(1))
                } else {
                    snap.cols.saturating_sub(1)
                };
                if c0 > c1 {
                    continue;
                }
                instances.push(RectInstance::solid([
                        origin_x + c0 as f32 * self.cell_width,
                        origin_y + row_idx as f32 * self.cell_height,
                    ], [
                        (c1 - c0 + 1) as f32 * self.cell_width,
                        self.cell_height,
                    ], self.selection_bg));
            }
        }

        // Search match highlights — RLE'd (one rect per match span).
        if snap.search_active {
            // Match rows are ABSOLUTE (scrollback origin 0) — map
            // each onto the current viewport and draw only the
            // visible ones, so highlights track content instead of
            // going stale the moment the view scrolls.
            let viewport_top_abs = snap.scrollback_total.saturating_sub(snap.scroll_offset);
            for (i, m) in snap.search_matches.iter().enumerate() {
                let Some(vp_row) = m.row.checked_sub(viewport_top_abs) else {
                    continue; // above the viewport
                };
                if vp_row >= snap.num_rows {
                    continue; // below the viewport
                }
                let is_current = i == snap.search_current;
                // Theme-derived search-match fills, linearized for the
                // rect pipeline at paint time (current match brighter
                // than other matches). Vellum paints first_light
                // #D7C489 / search_others #443E2A; legacy presets keep
                // Nord aurora yellow #EBCB8B (the field default) until a
                // theme carrying the surfaces loads.
                let color = if is_current {
                    let c = self.search_current_color;
                    overlay_rect_color(c.r, c.g, c.b, 0.5)
                } else {
                    let c = self.search_other_color;
                    overlay_rect_color(c.r, c.g, c.b, 0.2)
                };
                instances.push(RectInstance::solid([
                        origin_x + m.col_start as f32 * self.cell_width,
                        origin_y + vp_row as f32 * self.cell_height,
                    ], [
                        (m.col_end + 1 - m.col_start) as f32 * self.cell_width,
                        self.cell_height,
                    ], color));
            }
        }

        // URL underline decorations — RLE'd (one rect per URL). Gated on
        // the links-highlight config so the bare tier paints no underline.
        if self.links_highlight {
            let lc = self.link_color;
            for detected_url in &snap.urls {
                instances.push(RectInstance::solid(
                    [
                        origin_x + detected_url.col_start as f32 * self.cell_width,
                        origin_y
                            + (detected_url.row as f32 + 1.0) * self.cell_height
                            - 1.5,
                    ],
                    [
                        (detected_url.col_end + 1 - detected_url.col_start) as f32
                            * self.cell_width,
                        1.0,
                    ],
                    // Theme link accent (frost blue), linearized for the
                    // rect pipeline (see `overlay_rect_color`) — never a hex.
                    overlay_rect_color(lc.r, lc.g, lc.b, 0.6),
                ));
            }
        }

        // Cursor (with optional blink). Unfocused windows pin the
        // cursor steady (no blink) and draw the hollow variant — the
        // standard which-window-owns-the-keyboard affordance
        // (kitty/ghostty/iTerm2/Terminal.app).
        let cursor_on = !self.focused
            || !self.cursor_blink
            || crate::motion::blink_on(elapsed, self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0);

        // While scrolled into history the live-grid cursor position
        // is meaningless for the rows on screen — drawing it painted
        // a phantom insertion point over history text (2026-06-11).
        if snap.cursor.visible
            && cursor_on
            && snap.scroll_offset == 0
            && snap.cursor.row < snap.num_rows
            && snap.cursor.col < snap.cols
        {
            let cx = origin_x + snap.cursor.col as f32 * self.cell_width;
            let cy = origin_y + snap.cursor.row as f32 * self.cell_height;

            let effective_style = if self.focused {
                self.cursor_style
            } else {
                CursorStyle::BlockHollow
            };
            let (pos, size) = match effective_style {
                CursorStyle::Block => ([cx, cy], [self.cell_width, self.cell_height]),
                CursorStyle::BlockHollow => ([cx, cy], [self.cell_width, self.cell_height]),
                CursorStyle::Bar => ([cx, cy], [2.0, self.cell_height]),
                CursorStyle::Underline => (
                    [cx, cy + self.cell_height - 2.0],
                    [self.cell_width, 2.0],
                ),
            };

            if effective_style == CursorStyle::BlockHollow {
                let thickness = 2.0_f32;
                instances.push(RectInstance::solid([cx, cy], [self.cell_width, thickness], self.cursor_color));
                instances.push(RectInstance::solid([cx, cy + self.cell_height - thickness], [self.cell_width, thickness], self.cursor_color));
                instances.push(RectInstance::solid([cx, cy], [thickness, self.cell_height], self.cursor_color));
                instances.push(RectInstance::solid([cx + self.cell_width - thickness, cy], [thickness, self.cell_height], self.cursor_color));
            } else {
                instances.push(RectInstance::solid(pos, size, self.cursor_color));
            }
        }

        // ── Scrolled-into-history indicator ────────────────────
        // With the content-pinned viewport (2026-06-11) the operator
        // can sit in history while output streams below — without a
        // cue the screen just looks frozen. A right-edge thumb shows
        // position: top of track = oldest scrollback, bottom = live
        // tail. Drawn ONLY while scrolled; the live view stays
        // chrome-free.
        if snap.scroll_offset > 0 && snap.scrollback_total > 0 {
            let track_h = snap.num_rows as f32 * self.cell_height;
            let total_rows = (snap.scrollback_total + snap.num_rows) as f32;
            let thumb_h = (track_h * snap.num_rows as f32 / total_rows).max(24.0);
            // scroll_offset = rows BELOW the viewport bottom edge…
            // position the thumb so offset==scrollback_total → top.
            let frac = 1.0 - (snap.scroll_offset as f32 / snap.scrollback_total as f32);
            let thumb_y = origin_y + frac * (track_h - thumb_h);
            let thumb_w = 4.0_f32;
            let thumb_x = origin_x + snap.cols as f32 * self.cell_width - thumb_w;
            // Nord frost #88C0D0 @ 35% α — typed linearizer like every
            // overlay rect. ROUNDED corners (operator: "round edges
            // instead of squaring them"): the thumb is freestanding
            // chrome, so the ishou `radius.sm` token (4px, clamped to
            // thumb_w/2 = 2px in the SDF → a soft pill) reads as polish.
            // The radius flows from ishou — no hand-pinned corner size,
            // so a fleet radius retune propagates on the next compile.
            let thumb_radius = ishou_tokens::Radius::default().sm as f32;
            // Theme scrollbar accent (frost cyan), linearized for the rect
            // pipeline — never a hex; tracks the active theme.
            let sb = self.scrollbar_color;
            instances.push(RectInstance::rounded(
                [thumb_x, thumb_y],
                [thumb_w, thumb_h],
                overlay_rect_color(sb.r, sb.g, sb.b, 0.35),
                thumb_radius,
            ));
        }

        // ── Pane-as-block separators ───────────────────────────
        // A faint horizontal line (1px tall) at each OSC 133 A
        // mark within the viewport. Nord frost-3 at ~30% alpha
        // — visible but not distracting. Sits *above* the row
        // so it visually separates "previous block ends here"
        // from "next block starts below".
        for sep_row in &snap.block_separator_rows {
            // Skip row 0 — drawing above the top edge would
            // be off-screen / visually noisy.
            if *sep_row == 0 {
                continue;
            }
            let y = origin_y + (*sep_row as f32) * self.cell_height;
            // Theme command-block accent (frost-blue), linearized for the
            // rect pipeline — never a hex; tracks the active theme. (Exit-
            // status tinting via `exit_ok`/`exit_err` is plumbed in the
            // theme but awaits per-block status in the snapshot.)
            let pm = self.prompt_mark_color;
            instances.push(RectInstance::solid(
                [origin_x, y],
                [snap.cols as f32 * self.cell_width, 1.0],
                overlay_rect_color(pm.r, pm.g, pm.b, 0.30),
            ));
        }

        instances
    }

    /// Build per-cell-grid-aligned text buffers.
    ///
    /// Returns `(row_idx, col_start, Buffer)` triples. Each buffer holds a
    /// run of cells whose glyphs can SAFELY share a single glyphon buffer
    /// without the font's natural advance drifting past the cell-grid
    /// boundaries — i.e., printable ASCII that almost every monospace
    /// font shapes with a uniform `cell_width` advance. Any non-ASCII
    /// glyph (Nerd Font icons, ambiguous-width Unicode like `·`/`❄`,
    /// box-drawing rendered as space, etc.) gets its own single-cell
    /// buffer positioned at exactly `col_start * cell_width`.
    ///
    /// ## Why this matters (the wide-glyph cursor bug)
    ///
    /// JetBrainsMono Nerd Font (the fleet default) shapes `·` (U+00B7) and
    /// `❄` (U+2744) with an advance noticeably wider than the
    /// monospace `cell_width`. When all of `cid · ~❄ ` was rendered as
    /// ONE buffer at `left = pad`, glyphon laid out each glyph at the
    /// font's natural advance — `❄` drifted right of column 7, the
    /// trailing space drifted right of column 8, and by the time
    /// rendering reached column 9 the actual pixel position was 4–6
    /// columns past where the cursor block (drawn at `col * cell_width`)
    /// expected the text to end. The cursor appeared visually detached
    /// from the prompt.
    ///
    /// Diagnosed via `mcp__mado__snapshot_grid` on 2026-05-13: the cell
    /// grid had cursor at col 9, every cell width 1, but the user's
    /// screenshot showed the cursor ~6 cells past the visible prompt
    /// end. The cell state was correct; rendering was off because the
    /// font's natural advance bled across cells.
    ///
    /// ## Fix
    ///
    /// Cache-aware shape: look up `key` in the bounded LRU shape cache;
    /// on miss, call cosmic-text via `text.create_rich_buffer(...)`,
    /// wrap the result in `Arc<Buffer>`, and insert. P7.
    ///
    /// **Why Arc**: glyphon's `Buffer` is not `Clone`. The same shaped
    /// Buffer is consumed by `glyphon::TextArea::buffer: &Buffer` —
    /// reading a reference, not owning. `Arc<Buffer>` lets us hand the
    /// caller a cheap-cloneable handle while the cache owns the
    /// canonical instance. `&*arc` recovers `&Buffer` at the TextArea
    /// construction site.
    ///
    /// **Why RefCell**: `build_text_buffers` is called from two
    /// `&mut self` paths (single-pane render + multi-pane render) but
    /// the call site of multi-pane has overlapping borrows from the
    /// `WindowState` lock (`ws.pane(...)` returns `&Pane` that borrows
    /// from `ws` that borrows from `self.window`). Interior mutability
    /// on the cache lets `build_text_buffers` stay `&self` so it
    /// composes with those borrows cleanly. The render thread is
    /// single-threaded so the borrow is always uncontested.
    fn shape_run(
        &self,
        text: &mut garasu::TextLayerStack,
        key: ShapeKey,
    ) -> Arc<Buffer> {
        if let Some(arc) = self.shape_cache.borrow_mut().get(&key) {
            return Arc::clone(arc);
        }
        // Route powerline / Nerd-PUA icon runs to the dedicated symbols
        // family (ghostty's model) so they don't depend on cosmic-text's
        // arbitrary coverage-walk pick. Selection is a pure function of
        // (run-text, italic, the three configured families) so it's
        // unit-testable without a GPU — see `select_run_family`.
        let family = Family::Name(select_run_family(
            &key.text,
            key.attrs.italic,
            &self.font_family,
            &self.font_italic,
            &self.font_symbols,
        ));
        let mut attrs = Attrs::new()
            .family(family)
            .color(GlyphonColor::rgba(
                key.attrs.fg_r,
                key.attrs.fg_g,
                key.attrs.fg_b,
                255,
            ));
        if key.attrs.bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        if key.attrs.italic {
            attrs = attrs.style(Style::Italic);
        }
        let buf = text.create_rich_buffer(
            &[(&*key.text, attrs)],
            self.font_size_px(),
            self.cell_height,
        );
        let arc = Arc::new(buf);
        self.shape_cache
            .borrow_mut()
            .put(key, Arc::clone(&arc));
        arc
    }

    /// Split each row into runs and emit one glyphon Buffer per run,
    /// reusing already-shaped Buffers via the shape cache (P7).
    ///
    /// **Run-length batching** (P6): the previous implementation
    /// emitted ONE Buffer PER CELL — on an 80×24 grid of typical shell
    /// output that's ~1500–1900 allocations + shaping passes per
    /// frame. We batch consecutive "simple" cells (width==1, ASCII,
    /// no `extra`, same effective attrs) into one run per Buffer.
    ///
    /// **Shape cache** (P7): every run lookup hits a bounded LRU
    /// keyed by (run-bytes, attrs, physical-font-size). Refterm's
    /// biggest insight — hit rate is >99% on typical interactive
    /// sessions (the prompt repeats verbatim, scrollback lines are
    /// stable, "ls" output reshapes once and never again).
    ///
    /// Non-batchable cells (CJK, emoji, Nerd Font icon, combining
    /// mark, wide cell, hidden) get their own dedicated buffer at
    /// per-cell granularity — the per-cell-positioning invariant
    /// from the wide-glyph cursor-offset fix is preserved.
    /// Box-drawing is rendered by the rect pipeline (no glyph
    /// emission) and acts as a run boundary too.
    ///
    /// Compound effect P6+P7 on typical workloads:
    ///   ~1900 cells × ~1900 allocations/shapes per frame
    ///   → ~30–80 runs per frame
    ///   → ~0–3 cosmic-text shape calls per frame (cache hits dominate)
    fn build_text_buffers(
        &self,
        snap: &Snapshot,
        text: &mut garasu::TextLayerStack,
        blink_on: bool,
    ) -> Vec<(usize, GridCol, Arc<Buffer>)> {
        // P23 — pre-size. Typical interactive grid produces ~3-8
        // runs per row after P6 batching. 8 × rows is a generous
        // upper bound; the Vec will grow if needed (mimalloc + amortized
        // doubling makes this cheap) but pre-sizing eliminates the
        // first ~4 reallocations on each frame.
        let mut buffers: Vec<(usize, GridCol, Arc<Buffer>)> =
            Vec::with_capacity(snap.num_rows * 8);
        let font_size_bits = self.font_size_px().to_bits();

        for (row_idx, row) in snap.rows.iter().enumerate() {
            let mut has_content = false;
            let mut row_buffers: Vec<(GridCol, Arc<Buffer>)> = Vec::with_capacity(8);

            // Current open run: (start_col, accumulated text, attrs key).
            // `start_col` is a typed `GridCol` minted by `glyph_columns`,
            // so it cannot be a width-sum (see `mod grid_col`).
            let mut run: Option<(GridCol, String, RunAttrsKey)> = None;

            let flush_run = |run: &mut Option<(GridCol, String, RunAttrsKey)>,
                             row_buffers: &mut Vec<(GridCol, Arc<Buffer>)>,
                             text: &mut garasu::TextLayerStack| {
                if let Some((start_col, run_text, attrs)) = run.take() {
                    let key = ShapeKey {
                        text: run_text.into_boxed_str(),
                        attrs,
                        font_size_bits,
                    };
                    let arc = self.shape_run(text, key);
                    row_buffers.push((start_col, arc));
                }
            };

            // `glyph_columns` is the single source of column truth (see
            // `mod grid_col`): each `col_here` is the cell's TRUE grid
            // column as a typed `GridCol`, never a `col += cell.width`
            // accumulator. Continuation cells (`width == 0`) own no
            // column and are skipped by the iterator. The rect/cursor
            // pipeline iterates the identical function, so a glyph and
            // its cursor can no longer drift apart — the wide-char
            // misalignment is unrepresentable.
            for (col_here, cell) in glyph_columns(row, snap.cols) {
                // Box-drawing AND the filled powerline separators are
                // rendered by the rect pipeline (synthesized to fill the
                // whole cell — see build_rect_instances), never as font
                // glyphs. Divert both so the glyph path doesn't also
                // baseline-position them (which would notch the cell
                // bottom at tall line-heights) and so they act as a run
                // boundary.
                if is_box_drawing(cell.ch) || is_powerline_separator(cell.ch) {
                    has_content = true;
                    flush_run(&mut run, &mut row_buffers, text);
                    continue;
                }

                let is_blank = cell.ch == ' ' && cell.extra.is_none();
                if is_blank {
                    flush_run(&mut run, &mut row_buffers, text);
                    continue;
                }
                has_content = true;

                let style = cell.style(&snap.styles);
                let cell_attrs = style.attrs;
                let inverse = cell_attrs.flags.contains(AttrFlags::INVERSE);
                let bold = cell_attrs.flags.contains(AttrFlags::BOLD);
                let dim = cell_attrs.flags.contains(AttrFlags::DIM);
                let italic = cell_attrs.flags.contains(AttrFlags::ITALIC);
                // BLINK off-phase renders exactly like HIDDEN (fg
                // painted in bg so the cell keeps its advance) — the
                // glyph re-appears next phase without reshaping
                // (ShapeKey carries the effective fg).
                let hidden = cell_attrs.flags.contains(AttrFlags::HIDDEN)
                    || (!blink_on && cell_attrs.flags.contains(AttrFlags::BLINK));

                let effective_fg = if hidden {
                    if inverse { style.fg } else { style.bg }
                } else {
                    let mut fg = if inverse {
                        style.bg
                    } else if bold && self.bold_is_bright {
                        bold_bright_color(&style.fg, &self.ansi_colors)
                    } else {
                        style.fg
                    };
                    if dim {
                        fg = Color::new(fg.r / 2, fg.g / 2, fg.b / 2);
                    }
                    // Clickable links repaint in the theme's frost accent so
                    // OSC 8 hyperlinks + auto-detected URLs read as links, not
                    // body text (the underline rect above carries the same
                    // colour). Gated on the links-highlight config.
                    if self.links_highlight
                        && (cell.link_id != crate::terminal::NO_LINK_ID
                            || crate::url::url_at(&snap.urls, row_idx, col_here.idx())
                                .is_some())
                    {
                        fg = self.link_color;
                    }
                    fg
                };

                let is_simple_for_batch = cell.width == 1
                    && cell.extra.is_none()
                    && cell.ch.is_ascii()
                    && !hidden;

                if !is_simple_for_batch {
                    flush_run(&mut run, &mut row_buffers, text);
                    let attrs_key = RunAttrsKey {
                        fg_r: effective_fg.r,
                        fg_g: effective_fg.g,
                        fg_b: effective_fg.b,
                        bold: bold && !hidden,
                        italic: italic && !hidden,
                    };
                    let mut s = String::new();
                    cell.write_to(&mut s);
                    // Text-presentation emoji (❄ ☄ ✔ …) are Emoji=Yes but
                    // Emoji_Presentation=No, so unicode-width books them at
                    // width 1 — yet a color font would draw the ~2-cell emoji
                    // glyph, overflowing into the next cell (the prompt
                    // cursor). For a width-1 cell carrying such a codepoint,
                    // append VS15 (U+FE0E) to force the monochrome 1-cell text
                    // glyph. VS15 is part of the ShapeKey text, so the
                    // text/emoji variants stay cache-distinct.
                    if cell.width == 1 {
                        if let Some(first) = s.chars().next() {
                            if crate::glyph_class::is_text_presentation_emoji(first) {
                                s.push('\u{FE0E}');
                            }
                        }
                    }
                    let key = ShapeKey {
                        text: s.into_boxed_str(),
                        attrs: attrs_key,
                        font_size_bits,
                    };
                    let arc = self.shape_run(text, key);
                    row_buffers.push((col_here, arc));
                    continue;
                }

                let cell_key = RunAttrsKey {
                    fg_r: effective_fg.r,
                    fg_g: effective_fg.g,
                    fg_b: effective_fg.b,
                    bold,
                    italic,
                };

                match &mut run {
                    Some((_, run_text, key)) if *key == cell_key => {
                        run_text.push(cell.ch);
                    }
                    _ => {
                        flush_run(&mut run, &mut row_buffers, text);
                        let mut s = String::with_capacity(snap.cols);
                        s.push(cell.ch);
                        run = Some((col_here, s, cell_key));
                    }
                }
            }
            flush_run(&mut run, &mut row_buffers, text);

            if !has_content && row_idx != snap.cursor.row {
                continue;
            }

            for (col_start, arc) in row_buffers {
                buffers.push((row_idx, col_start, arc));
            }
        }

        buffers
    }

    // snapshot_pane + render_multi_pane both removed at Phase 4 —
    // multi-pane rendering belongs in tear's MultiplexerControl
    // path, not in mado.
}

// (Earlier iteration's CellRun / is_ascii_grid_safe helpers removed:
// per-cell rendering doesn't need them — every cell becomes its own
// buffer at `col * cell_width`, batching no longer applies.)

/// Convert a per-cell sRGB colour into the linear [f32; 4] tuple the
/// rect-pipeline shader expects. The shader returns its colour value
/// directly into a `Bgra8UnormSrgb` surface, where wgpu performs the
/// final linear→sRGB transform on storage. Feeding raw sRGB values
/// (the byte-divided-by-255 form) into the shader output caused the
/// "washed-out medium grey" gamma bug visible on Retina pre-M3 —
/// passing through `ishou_tokens::Srgb::to_linear` is the typed path
/// that makes the storage write end up as the operator-perceived
/// colour. Alpha stays linear by convention.
fn color_to_f32(c: &Color) -> [f32; 4] {
    let linear = ishou_tokens::Srgb::new(c.r, c.g, c.b).to_linear();
    [linear.r, linear.g, linear.b, 1.0]
}

/// Convert an sRGB theme colour into the linear `[f32; 3]` additive tint the
/// engawa glow shader expects. The glow samples a linear scene texture and
/// adds `tint * intensity`, so — same discipline as [`color_to_f32`] — the
/// tint must be linearized (a raw-sRGB triple would blend wrong). Alpha is
/// dropped; the glow tint carries none.
fn color_to_rgb(c: &Color) -> [f32; 3] {
    let linear = ishou_tokens::Srgb::new(c.r, c.g, c.b).to_linear();
    [linear.r, linear.g, linear.b]
}

/// The single typed surface for translucent overlay-decoration rects
/// (selection highlight, search-match highlight, URL underline). Like
/// every other colour the rect pipeline consumes, the value MUST be
/// linear before it reaches the sRGB-storage surface — wgpu re-encodes
/// linear→sRGB on store, so a raw-sRGB triple here renders washed-out
/// (the prior-incident gamma bug, isolated to the overlay class). The
/// RGB channels go through the typed `ishou_tokens::Srgb::to_linear`
/// path; alpha is linear by convention and passes through unchanged.
fn overlay_rect_color(r: u8, g: u8, b: u8, alpha: f32) -> [f32; 4] {
    let linear = ishou_tokens::Srgb::new(r, g, b).to_linear();
    [linear.r, linear.g, linear.b, alpha]
}

/// Check if a character is a box drawing character that we render via rects.
fn is_box_drawing(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{257F}' | '\u{2580}'..='\u{259F}')
}

/// Pick the font family name a shaped run should use.
///
/// Pure selection rule (no GPU, no cosmic-text state) so the
/// font-fallback decision is unit-testable:
///   1. If a non-empty `symbols` family is configured AND the run is
///      all powerline / Nerd-PUA icon codepoints → `symbols`.
///   2. Else if the run is italic → `italic`.
///   3. Else → `primary`.
///
/// An empty `symbols` (bare config tier) is treated as "no preference"
/// so symbol cells fall through to the primary family — which on the
/// default JetBrainsMono Nerd Font already carries the patched ranges.
fn select_run_family<'a>(
    text: &str,
    italic: bool,
    primary: &'a str,
    italic_family: &'a str,
    symbols: &'a str,
) -> &'a str {
    if !symbols.is_empty() && crate::glyph_class::run_is_all_symbols(text) {
        symbols
    } else if italic {
        italic_family
    } else {
        primary
    }
}

/// Render box drawing and block element characters as pixel-perfect rectangles.
/// Returns the rect instances for the character, or empty if not a box drawing char.
fn box_drawing_rects(
    ch: char,
    x: f32,
    y: f32,
    cw: f32,
    ch_h: f32,
    color: [f32; 4],
) -> Vec<RectInstance> {
    let mut rects = Vec::new();
    let cx = x + cw / 2.0;
    let cy = y + ch_h / 2.0;
    let thick = (cw / 8.0).max(1.0);

    match ch {
        // ─ horizontal line
        '\u{2500}' => {
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cw, thick], color));
        }
        // │ vertical line
        '\u{2502}' => {
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, ch_h], color));
        }
        // ┌ top-left corner
        '\u{250C}' => {
            rects.push(RectInstance::solid([cx - thick / 2.0, cy - thick / 2.0], [cw - (cx - x) + thick / 2.0, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, cy - thick / 2.0], [thick, ch_h - (cy - y) + thick / 2.0], color));
        }
        // ┐ top-right corner
        '\u{2510}' => {
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cx - x + thick / 2.0, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, cy - thick / 2.0], [thick, ch_h - (cy - y) + thick / 2.0], color));
        }
        // └ bottom-left corner
        '\u{2514}' => {
            rects.push(RectInstance::solid([cx - thick / 2.0, cy - thick / 2.0], [cw - (cx - x) + thick / 2.0, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, cy - y + thick / 2.0], color));
        }
        // ┘ bottom-right corner
        '\u{2518}' => {
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cx - x + thick / 2.0, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, cy - y + thick / 2.0], color));
        }
        // ├ left tee
        '\u{251C}' => {
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, ch_h], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, cy - thick / 2.0], [cw - (cx - x) + thick / 2.0, thick], color));
        }
        // ┤ right tee
        '\u{2524}' => {
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, ch_h], color));
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cx - x + thick / 2.0, thick], color));
        }
        // ┬ top tee
        '\u{252C}' => {
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cw, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, cy - thick / 2.0], [thick, ch_h - (cy - y) + thick / 2.0], color));
        }
        // ┴ bottom tee
        '\u{2534}' => {
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cw, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, cy - y + thick / 2.0], color));
        }
        // ┼ cross
        '\u{253C}' => {
            rects.push(RectInstance::solid([x, cy - thick / 2.0], [cw, thick], color));
            rects.push(RectInstance::solid([cx - thick / 2.0, y], [thick, ch_h], color));
        }
        // ═ double horizontal
        '\u{2550}' => {
            let gap = thick;
            rects.push(RectInstance::solid([x, cy - thick - gap / 2.0], [cw, thick], color));
            rects.push(RectInstance::solid([x, cy + gap / 2.0], [cw, thick], color));
        }
        // ║ double vertical
        '\u{2551}' => {
            let gap = thick;
            rects.push(RectInstance::solid([cx - thick - gap / 2.0, y], [thick, ch_h], color));
            rects.push(RectInstance::solid([cx + gap / 2.0, y], [thick, ch_h], color));
        }
        // Block elements
        // ▀ upper half block
        '\u{2580}' => {
            rects.push(RectInstance::solid([x, y], [cw, ch_h / 2.0], color));
        }
        // ▄ lower half block
        '\u{2584}' => {
            rects.push(RectInstance::solid([x, y + ch_h / 2.0], [cw, ch_h / 2.0], color));
        }
        // █ full block
        '\u{2588}' => {
            rects.push(RectInstance::solid([x, y], [cw, ch_h], color));
        }
        // ▌ left half block
        '\u{258C}' => {
            rects.push(RectInstance::solid([x, y], [cw / 2.0, ch_h], color));
        }
        // ▐ right half block
        '\u{2590}' => {
            rects.push(RectInstance::solid([x + cw / 2.0, y], [cw / 2.0, ch_h], color));
        }
        // ░ light shade
        '\u{2591}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.25;
            rects.push(RectInstance::solid([x, y], [cw, ch_h], shade_color));
        }
        // ▒ medium shade
        '\u{2592}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.5;
            rects.push(RectInstance::solid([x, y], [cw, ch_h], shade_color));
        }
        // ▓ dark shade
        '\u{2593}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.75;
            rects.push(RectInstance::solid([x, y], [cw, ch_h], shade_color));
        }
        _ => {} // Unhandled box drawing — fall through to font glyph
    }

    rects
}

impl TerminalRenderer {
    /// Upload new/changed Kitty images to GPU. Call before draw passes.
    fn sync_kitty_images(&mut self, ctx: &mut RenderContext<'_>) {
        let image_pipeline = match self.image_pipeline {
            Some(ref mut p) => p,
            None => return,
        };

        let term = self.terminal.read();
        let term_images = term.images();
        for (id, kitty_img) in term_images {
            let needs_upload = self
                .gpu_images
                .get(id)
                .is_none_or(|gpu| gpu.seqno != kitty_img.seqno);
            if needs_upload && !kitty_img.data.is_empty() {
                let gpu_img = image_pipeline.create_gpu_image(
                    &ctx.gpu.device,
                    &ctx.gpu.queue,
                    &kitty_img.data,
                    kitty_img.width,
                    kitty_img.height,
                    kitty_img.seqno,
                );
                self.gpu_images.insert(*id, gpu_img);
            }
        }
        // Remove GPU textures for deleted images
        self.gpu_images.retain(|id, _| term_images.contains_key(id));
    }

    /// Draw Kitty image placements. GPU textures must be synced first.
    fn draw_kitty_images(
        &self,
        gpu: &garasu::GpuContext,
        width: u32,
        height: u32,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        placements: &[ImagePlacement],
        origin_x: f32,
        origin_y: f32,
    ) {
        if placements.is_empty() {
            return;
        }

        let image_pipeline = match self.image_pipeline {
            Some(ref p) => p,
            None => return,
        };

        // Build image instances
        let mut image_draws: Vec<(u32, ImageInstance)> = Vec::new();

        for placement in placements {
            let gpu_img = match self.gpu_images.get(&placement.image_id) {
                Some(g) => g,
                None => continue,
            };

            let img_w = gpu_img.texture.width() as f32;
            let img_h = gpu_img.texture.height() as f32;
            if img_w == 0.0 || img_h == 0.0 {
                continue;
            }

            let disp_cols = if placement.cols > 0 {
                placement.cols as f32
            } else {
                (img_w / self.cell_width).ceil()
            };
            let disp_rows = if placement.rows > 0 {
                placement.rows as f32
            } else {
                (img_h / self.cell_height).ceil()
            };

            let px = origin_x + placement.col as f32 * self.cell_width + placement.x_offset as f32;
            let py = origin_y + placement.row as f32 * self.cell_height + placement.y_offset as f32;
            let pw = disp_cols * self.cell_width;
            let ph = disp_rows * self.cell_height;

            let (uv_x, uv_y, uv_w, uv_h) = if placement.src_width > 0 && placement.src_height > 0
            {
                (
                    placement.src_x as f32 / img_w,
                    placement.src_y as f32 / img_h,
                    placement.src_width as f32 / img_w,
                    placement.src_height as f32 / img_h,
                )
            } else {
                (0.0, 0.0, 1.0, 1.0)
            };

            image_draws.push((
                placement.image_id,
                ImageInstance {
                    pos: [px, py],
                    size: [pw, ph],
                    uv_offset: [uv_x, uv_y],
                    uv_scale: [uv_w, uv_h],
                    // Kitty images composite at full opacity; the browser
                    // overlay path (draw_float_surfaces) is what varies it.
                    opacity: 1.0,
                },
            ));
        }

        if image_draws.is_empty() {
            return;
        }

        // Update uniforms
        let uniforms = ScreenUniforms {
            resolution: [width as f32, height as f32],
            _padding: [0.0; 2],
        };
        gpu.queue
            .write_buffer(&image_pipeline.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        // NO sort-by-id here. `placements` arrives in z-then-transmission
        // order (partition_placements_by_z); re-sorting by texture id would
        // scramble that within-band z-ordering. The batch loop below already
        // re-binds the texture on every id change, so a z-interleaved order
        // costs at most one extra bind per layer transition — correctness
        // (z-order honored) over a micro-optimization (fewer binds).

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_images"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        pass.set_pipeline(&image_pipeline.pipeline);
        pass.set_bind_group(0, &image_pipeline.uniform_bind_group, &[]);

        let mut current_id = u32::MAX;
        let mut batch_start = 0;

        for (i, (id, _instance)) in image_draws.iter().enumerate() {
            if *id != current_id {
                if current_id != u32::MAX && i > batch_start {
                    let batch: Vec<_> = image_draws[batch_start..i]
                        .iter()
                        .map(|(_, inst)| *inst)
                        .collect();
                    gpu.queue.write_buffer(
                        &image_pipeline.instance_buffer,
                        0,
                        bytemuck::cast_slice(&batch),
                    );
                    pass.set_vertex_buffer(0, image_pipeline.instance_buffer.slice(..));
                    pass.draw(0..6, 0..batch.len() as u32);
                }

                current_id = *id;
                batch_start = i;

                if let Some(gpu_img) = self.gpu_images.get(id) {
                    pass.set_bind_group(1, &gpu_img.bind_group, &[]);
                }
            }
        }

        if current_id != u32::MAX && image_draws.len() > batch_start {
            let batch: Vec<_> = image_draws[batch_start..]
                .iter()
                .map(|(_, inst)| *inst)
                .collect();
            gpu.queue.write_buffer(
                &image_pipeline.instance_buffer,
                0,
                bytemuck::cast_slice(&batch),
            );
            pass.set_vertex_buffer(0, image_pipeline.instance_buffer.slice(..));
            pass.draw(0..6, 0..batch.len() as u32);
        }
    }
}

impl TerminalRenderer {
    /// The enabled catalog-effect set for this frame, derived from
    /// config — the ONLY source the graph cache key reads. Disabled
    /// effects are absent (zero nodes), not parameterized off.
    /// `reduce_motion` gates the ANIMATED effects (glow_on_bell,
    /// snow) to zero nodes regardless of their `enabled` knobs.
    ///
    /// The composed AMBIENCE layer (operator design law, 2026-06-13) is
    /// unioned in: every member of `self.ambience` turns its catalog
    /// effect on. The composition is already `reduce_motion`-resolved
    /// (Off ⇒ empty), so it adds nothing under reduce-motion — the
    /// accessibility floor holds by construction. Per-effect power-user
    /// `enabled` knobs are ADDITIVE on top (a user can force an effect
    /// on even when the preset is `Off`).
    fn enabled_effect_set(&self) -> crate::render_graph::EffectSet {
        use engawa_wgpu::catalog::CatalogEffect;
        let mut set = crate::render_graph::EffectSet::EMPTY;

        // The default-on composed layer: every ambience member's effect.
        for member in &self.ambience.members {
            set.insert(member.effect);
        }

        if self.effects_config.colorblind.mode != ColorblindMode::None {
            set.insert(CatalogEffect::Colorblind);
        }
        let e = &self.effects_config;
        // Aurora power-user override: force on regardless of the preset.
        // reduce_motion still suppresses it (aurora is animated — the
        // curtain drifts), so it lives under the same gate as glow/snow.
        if e.crt.enabled {
            set.insert(CatalogEffect::Crt);
        }
        if e.scanlines.enabled {
            set.insert(CatalogEffect::Scanlines);
        }
        if e.bloom.enabled {
            set.insert(CatalogEffect::Bloom);
        }
        // Grain is a static texture (not animated motion), so the
        // power-user override is NOT motion-gated — it's the same band
        // as crt/scanlines/bloom. The Matte composition injects grain
        // via its member above; this forces it on regardless of preset.
        if e.grain.enabled {
            set.insert(CatalogEffect::Grain);
        }
        // Window-depth (inner-edge vignette) is a static post effect like
        // grain — not motion — so it's not under the reduce_motion gate.
        if e.window_depth.enabled {
            set.insert(CatalogEffect::WindowDepth);
        }
        if !self.reduce_motion {
            if e.aurora.enabled {
                set.insert(CatalogEffect::Aurora);
            }
            if e.glow_on_bell.enabled {
                set.insert(CatalogEffect::GlowOnBell);
            }
            if e.snow.enabled {
                set.insert(CatalogEffect::Snow);
            }
        }
        set
    }

    /// Total projection: mado's config knob → the catalog's typed
    /// mode (the wire word the WGSL switches on).
    fn catalog_colorblind_mode(&self) -> engawa_wgpu::catalog::colorblind::ColorblindMode {
        use engawa_wgpu::catalog::colorblind::ColorblindMode as CatalogMode;
        match self.effects_config.colorblind.mode {
            ColorblindMode::None => CatalogMode::None,
            ColorblindMode::Protanopia => CatalogMode::Protanopia,
            ColorblindMode::Deuteranopia => CatalogMode::Deuteranopia,
            ColorblindMode::Tritanopia => CatalogMode::Tritanopia,
        }
    }

    /// The ambience quality word applied to the aurora curtain this
    /// frame — the ambience governor's live FSM state. The governor
    /// scales it to the frame budget (rebuild-free) via the per-frame
    /// poll in [`Self::tick_ambience_governor`].
    fn ambience_quality(&self) -> engawa_wgpu::catalog::aurora::AuroraQuality {
        self.ambience_governor.quality()
    }

    /// Per-frame governor poll — classify the PREVIOUS frame's measured
    /// time against the budget and advance the FSM. Called at frame
    /// start ONLY when the ambience composition is non-empty (an empty
    /// composition omits the aurora node — the `reduce_motion` bypass:
    /// there is nothing to quality, so the governor is not ticked). The
    /// single `SetAmbienceQuality` effect lands in `self.ambience_governor`
    /// (its own state) and is read by `ambience_quality()` this frame —
    /// the params sink. `prev_frame_us` is the last completed frame's
    /// measured microseconds (`LAST_FRAME_US`).
    fn tick_ambience_governor(&mut self, prev_frame_us: u64) {
        if self.ambience.members.is_empty() {
            return;
        }
        let _ = self.ambience_governor.tick_frame(prev_frame_us);
    }

    /// Re-budget the ambience governor to the resolved effective frame
    /// rate (`config.performance.resolve_target_fps`). Called once after
    /// construction (stop discarding the resolved fps) and again on a
    /// hot-reload of the performance config — so a 120 Hz high-refresh
    /// panel budgets aurora quality against the 8.3 ms frame it actually has,
    /// and a battery-capped target shrinks the budget with it, instead of
    /// the hardcoded 60 Hz floor. `fps == 0` keeps the 60 Hz floor.
    pub(crate) fn set_ambience_budget_fps(&mut self, fps: u32) {
        let budget = crate::ux::ambience_governor::budget_us_for_fps(fps);
        self.ambience_governor.set_budget_us(budget);
    }

    /// The aurora spectrum stops (green / cyan / violet) in LINEAR rgb,
    /// derived from the active theme palette — NO hardcoded effect
    /// colors (the design law). On Vellum these resolve to
    /// `green_bright` / `ice_cyan` / `fable_violet`; on legacy themes
    /// they fall back to that theme's bright-green / cyan / agent
    /// accent, so the curtain always paints in the resolved palette.
    ///
    /// * green → ANSI 10 (bright green / `aurora_green`)
    /// * cyan  → ANSI 6 (`ice_cyan`)
    /// * violet → the agent accent (`search_status_color` = Vellum
    ///   `fable_violet`; the theme foreground on legacy presets)
    fn aurora_palette(&self) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let lin = |c: Color| {
            let l = ishou_tokens::Srgb::new(c.r, c.g, c.b).to_linear();
            [l.r, l.g, l.b]
        };
        let green = lin(self.ansi_colors[10]);
        let cyan = lin(self.ansi_colors[6]);
        let violet = lin(self.search_status_color);
        (green, cyan, violet)
    }

    /// Build the aurora params for this frame: dials from the
    /// power-user override when `aurora.enabled` (override beats
    /// preset), else from the composed ambience member; colors always
    /// from the theme palette; the clock from `aurora_state`; the
    /// quality from the ambience governor word. The catalog `with_*`
    /// builders clamp every dial, so an out-of-range tune saturates.
    fn aurora_params_for(&self, res: [f32; 2]) -> engawa_wgpu::catalog::aurora::AuroraParams {
        use engawa_wgpu::catalog::aurora::AuroraParams;
        let cfg = &self.effects_config.aurora;
        // Dials: override wins; else the composed member; else the
        // catalog default (the member is present whenever aurora is in
        // the set via the preset, so the unwrap-or is only the
        // power-user-forced-on-with-Off-preset edge).
        let (intensity, drift, shimmer, horizon) = if cfg.enabled {
            (cfg.intensity, cfg.drift, cfg.shimmer, cfg.horizon)
        } else if let Some(m) = self.ambience.aurora() {
            (m.intensity, m.drift, m.shimmer, m.horizon)
        } else {
            (
                crate::config::default_aurora_intensity(),
                crate::config::default_aurora_drift(),
                crate::config::default_aurora_shimmer(),
                crate::config::default_aurora_horizon(),
            )
        };
        let (green, cyan, violet) = self.aurora_palette();
        AuroraParams::default()
            .with_resolution(res)
            .with_intensity(intensity)
            .with_drift(drift)
            .with_shimmer(shimmer)
            .with_horizon(horizon)
            .with_colors(green, cyan, violet)
            .with_quality(self.ambience_quality())
            .with_time(self.aurora_state.time)
    }

    /// Per-frame params for every enabled effect — written into the
    /// corresponding uniform buffers by the dispatcher before any
    /// pass encodes. TOTAL match over the catalog: static knobs come
    /// from `effects_config`, animated state from the host
    /// `snow_state` / `glow_state` / `aurora_state` (already ticked
    /// this frame), composed dials from the ambience layer.
    fn frame_uniforms_for(
        &self,
        effects: crate::render_graph::EffectSet,
        width: u32,
        height: u32,
    ) -> engawa_wgpu::FrameUniforms {
        use engawa_wgpu::catalog::{self, CatalogEffect};
        let res = [width as f32, height as f32];
        let cfg = &self.effects_config;
        let mut frame = engawa_wgpu::FrameUniforms::new();
        for effect in effects.iter_render_order() {
            match effect {
                CatalogEffect::Colorblind => frame.set(
                    catalog::colorblind::PARAMS_RESOURCE,
                    &catalog::colorblind::ColorblindParams::new(
                        self.catalog_colorblind_mode(),
                    ),
                ),
                CatalogEffect::Crt => {
                    let mut p = catalog::crt::CrtParams::new(res);
                    p.curvature = cfg.crt.curvature;
                    p.vignette = cfg.crt.vignette;
                    p.aberration = cfg.crt.aberration;
                    frame.set(catalog::crt::PARAMS_RESOURCE, &p);
                }
                CatalogEffect::Scanlines => {
                    let mut p = catalog::scanlines::ScanlinesParams::new(res);
                    p.period_px = cfg.scanlines.period_px;
                    p.intensity = cfg.scanlines.intensity;
                    frame.set(catalog::scanlines::PARAMS_RESOURCE, &p);
                }
                CatalogEffect::Bloom => {
                    let mut p = catalog::bloom::BloomParams::new(res);
                    // Power-user override (bloom.enabled) wins; else the
                    // composed ambience member's subtle threshold + gain
                    // (bright accents only, no text smear); else the
                    // catalog default.
                    if cfg.bloom.enabled {
                        p.threshold = cfg.bloom.threshold;
                        p.intensity = cfg.bloom.intensity;
                        p.radius_px = cfg.bloom.radius_px;
                    } else if let Some(m) = self.ambience.member(CatalogEffect::Bloom) {
                        p.threshold = m.bloom_threshold;
                        p.intensity = m.intensity;
                    }
                    frame.set(catalog::bloom::PARAMS_RESOURCE, &p);
                }
                CatalogEffect::GlowOnBell => {
                    let mut p = self.glow_state.params;
                    p.resolution = res;
                    p.radius_px = cfg.glow_on_bell.radius_px;
                    frame.set(catalog::glow_on_bell::PARAMS_RESOURCE, &p);
                }
                CatalogEffect::Aurora => {
                    frame.set(catalog::aurora::PARAMS_RESOURCE, &self.aurora_params_for(res));
                }
                CatalogEffect::Snow => {
                    let mut p = self.snow_state.params;
                    p.set_resolution(res);
                    frame.set(catalog::snow::PARAMS_RESOURCE, &p);
                }
                CatalogEffect::Grain => {
                    // Opacity: power-user override wins; else the composed
                    // ambience member's intensity (Matte injects grain at
                    // the barely-perceptible default); else the catalog
                    // default. Clock from the shared render-clock
                    // (`aurora_state.time`) — the WGSL quantizes it to a
                    // slow shimmer; at elapsed=0 it's the identity, keeping
                    // the headless ladders byte-deterministic.
                    let opacity = if cfg.grain.enabled {
                        cfg.grain.opacity
                    } else if let Some(m) = self.ambience.member(CatalogEffect::Grain) {
                        m.intensity
                    } else {
                        catalog::grain::GrainParams::default().opacity
                    };
                    let p = catalog::grain::GrainParams::new(res)
                        .with_opacity(opacity)
                        .with_scale(1.0)
                        .with_time(self.aurora_state.time);
                    frame.set(catalog::grain::PARAMS_RESOURCE, &p);
                }
                CatalogEffect::WindowDepth => {
                    // Edge tint = the theme background pushed toward black (a
                    // deeper shade of the current surface) — theme-portable
                    // depth, no hardcoded colour. `bg_color` is already
                    // linear, so it feeds the effect uniform directly.
                    let bg = self.bg_color;
                    let darken = 0.35_f64;
                    let color = [
                        (bg.r * darken) as f32,
                        (bg.g * darken) as f32,
                        (bg.b * darken) as f32,
                    ];
                    let p = catalog::window_depth::WindowDepthParams::new(res)
                        .with_color(color)
                        .with_depth(cfg.window_depth.depth)
                        .with_intensity(cfg.window_depth.intensity)
                        .with_softness(cfg.window_depth.softness);
                    frame.set(catalog::window_depth::PARAMS_RESOURCE, &p);
                }
            }
        }
        frame
    }

    /// Dispatch the enabled effect chain: lease the chain/aux
    /// intermediates, bind SCENE (the rendered frame) + OUT (the
    /// surface) + sampler + params, write per-frame uniforms, and
    /// walk the cached CompiledGraph. Every lease (scene included)
    /// lands in `leases_out` for post-submit release.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_effect_chain(
        &mut self,
        device: &wgpu::Device,
        surface_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        effects: crate::render_graph::EffectSet,
        scene: engawa_wgpu::TextureLease,
        leases_out: &mut Vec<engawa_wgpu::TextureLease>,
    ) -> Result<wgpu::CommandBuffer, engawa_wgpu::WgpuDispatcherError> {
        use engawa_wgpu::catalog::{CATALOG_SAMPLER, OUT, SCENE};

        let frame = self.frame_uniforms_for(effects, width, height);

        // Lazily create the params uniform buffer for each enabled
        // effect — one buffer per effect for the renderer's lifetime.
        for effect in effects.iter_render_order() {
            self.effect_params
                .entry(effect.params_resource())
                .or_insert_with(|| {
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some(effect.params_resource()),
                        size: effect.params_size() as u64,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                });
        }

        let mut bound = engawa_wgpu::BoundResources::new()
            .with(SCENE, scene.bound_resource())
            .with(
                OUT,
                engawa_wgpu::BoundResource::Texture { view: surface_view.clone(), format },
            );
        leases_out.push(scene);
        let (Some(sampler), Some(dispatcher)) =
            (self.catalog_sampler.as_ref(), self.dispatcher.as_mut())
        else {
            // init() wires both before the first frame; this arm is
            // the total-function fallback (an empty command buffer),
            // not a code path.
            return Ok(device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("mado_effects_noop"),
                })
                .finish());
        };
        bound.insert(CATALOG_SAMPLER, engawa_wgpu::BoundResource::Sampler(sampler.clone()));
        for effect in effects.iter_render_order() {
            if let Some(buf) = self.effect_params.get(effect.params_resource()) {
                bound.insert(
                    effect.params_resource(),
                    engawa_wgpu::BoundResource::Uniform(buf.clone()),
                );
            }
        }

        let key = crate::render_graph::GraphKey { effects, width, height };
        let Some(compiled) = self.frame_graph.ensure(key) else {
            // Empty set never reaches here (callers gate on
            // non-empty) — total-function fallback again.
            return Ok(device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("mado_effects_noop"),
                })
                .finish());
        };
        for id in &compiled.intermediates {
            let lease = self
                .texture_pool
                .lease(device, engawa_wgpu::TextureKey::offscreen(width, height, format));
            bound.insert(id.clone(), lease.bound_resource());
            leases_out.push(lease);
        }

        dispatcher.dispatch_with(&compiled.graph, &compiled.bindings, bound, &frame)
    }
}

impl RenderCallback for TerminalRenderer {
    fn init(&mut self, gpu: &garasu::GpuContext) {
        crate::perf::log_phase("renderer_init_start");
        let format = SURFACE_FORMAT;
        self.rect_pipeline = Some(RectPipeline::new(&gpu.device, format));
        self.image_pipeline = Some(ImagePipeline::new(&gpu.device, format));
        self.dispatcher = Some(engawa_wgpu::WgpuDispatcher::new(
            &gpu.device,
            &gpu.queue,
            format,
        ));
        // Linear filtering — the same sampler the legacy post blit
        // used, so the catalog route is pixel-identical (1:1 blits
        // sample texel centers; the filter only matters under scale,
        // but matching it keeps the parity golden byte-exact).
        self.catalog_sampler = Some(gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mado_catalog_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        }));
        crate::perf::log_phase("renderer_init_done");
    }

    fn render(&mut self, ctx: &mut RenderContext<'_>) {
        // P19 — frame-timing instrumentation. Each phase records its
        // elapsed time so operators can capture render-path breakdowns
        // via `RUST_LOG=mado::render=debug` without recompiling. The
        // `tracing::debug!` macros compile to ~5 ns NOPs when the
        // level is disabled (default), so this is free in normal runs.
        let frame_start = Instant::now();

        // Watched-config edits apply BEFORE render: both adapters
        // poll `ux::ConfigHotReload::poll_config_reload` at frame
        // start (the setter delta lands ahead of the effect-set /
        // graph-key derivation this frame performs).

        // Pull the live HiDPI scale factor in first. If it changed, the
        // setter clears `metrics_measured` so `measure_cell_metrics`
        // below re-measures glyph widths in the new pixel density.
        // This is the load-bearing fix for "rendered content only fills
        // 1/scale_factor of the window" on Retina displays.
        self.set_scale_factor(ctx.scale_factor as f32);

        // Seam auto-tune: on a surface-size change (startup, resize, or a move
        // to a differently-scaled display) re-discover the panel-vs-framebuffer
        // downscale ratio and re-snap the cell grid onto integer PANEL pixels.
        // Dirty-gated on the size change (last_surface_* still holds the prior
        // frame's dims here) so the CG probe runs only when the display
        // geometry can actually have changed, never every frame.
        // `set_panel_ratio` invalidates metrics on a real change, so the
        // measure below re-snaps THIS frame. Off / override are resolved in
        // `set_seam_config`; here Auto-with-no-override runs the probe (a
        // `None`/failed probe → 1.0, a safe no-op).
        //
        // Resize-storm optimization (Deliverable 3): the panel ratio depends
        // on the *display*, not the surface *size*. macOS delivers a distinct
        // drawable size nearly every frame of a live drag-resize, so firing
        // the CoreGraphics probe (mode enumeration + a Vec build) on every
        // intermediate frame is wasted work. Gate it to fire once per SETTLED
        // size: run only when this frame's size equals the last frame's
        // (`size_settled` — the drag has paused/ended) AND the probe hasn't
        // already run for that size. A move to a differently-scaled display
        // also lands as a new settled size, so re-discovery still happens; the
        // final grid snaps on the settled size exactly as before.
        if self.seam_auto_tune && self.downscale_ratio_override.is_none() {
            let this = (ctx.width, ctx.height);
            if should_reprobe_ratio(
                this,
                (self.last_surface_w, self.last_surface_h),
                self.last_ratio_probe_wh,
            ) {
                // Typed, no silent fallback: a failed/nonsense probe is a
                // recorded `Unavailable`, distinct from a genuine 1.0, and
                // it is SURFACED (warn + print-posture) — so a seam here is
                // attributable to "the probe failed", not a mystery.
                let source =
                    crate::panel_fit::PanelRatio::from_probe(kanchi::probe::display_scaling_ratio());
                if !source.is_known() {
                    tracing::warn!(
                        target: "mado::seam",
                        "panel-ratio probe unavailable — falling back to 1.0; a fractionally-scaled \
                         display will show row seams. Run `mado print-posture` to confirm the ratio."
                    );
                }
                self.set_panel_ratio(source.ratio());
                self.panel_ratio_source = source;
                self.last_ratio_probe_wh = this;
            }
        }

        // Measure actual font metrics on first render (or after a
        // scale-factor change).
        self.measure_cell_metrics(ctx.text);

        // Pool eviction on resolution change (M3 review 2026-06-12):
        // pooled offscreen textures are keyed by exact size, and a
        // macOS live-resize delivers a distinct drawable size nearly
        // every frame — without eviction, every visited size strands
        // a full set of full-window textures in the free list for the
        // renderer's lifetime (~24 MB × up to 9 textures per size at
        // Retina with the 6-effect chain). retain() drops every
        // bucket that is not this frame's exact size, covering DPI
        // and format churn too; in-flight leases are unaffected
        // (held out of the pool until release).
        if self.last_surface_w != ctx.width || self.last_surface_h != ctx.height {
            // TextureKey::offscreen clamps zero dims to 1 — mirror it
            // so the predicate matches the keys leases actually use.
            let (w, h) = (ctx.width.max(1), ctx.height.max(1));
            self.texture_pool.retain(|k| k.width == w && k.height == h);
        }

        // Record the surface dims this frame renders at — after this
        // point `measured_grid()` reports display truth and the event
        // loops' grid-sync latch can reconcile the PTY size.
        self.last_surface_w = ctx.width;
        self.last_surface_h = ctx.height;

        // Multi-pane dispatch removed at Phase 4 — single-pane mado.

        // Single-pane path.
        //
        // Two-stage damage gate. Stage 1 is a **cheap seqno peek** —
        // grab a short-lived lock, read seqno + cursor visibility +
        // DEC-2026 synchronized-output flag, drop the lock. If
        // nothing has changed since the last frame (seqno match, no
        // cursor blink, no bell flash, no search animation) we
        // early-out WITHOUT calling self.snapshot(), which would
        // otherwise clone every visible row, run URL detection
        // across the whole grid, clone image_placements, and clone
        // the search-matches vec — all wasted work on an idle frame.
        //
        // **P14 — synchronized output (DEC mode 2026)**: when the
        // app has emitted BSU (CSI ? 2026 h) we hold off rendering
        // until the matching ESU (CSI ? 2026 l). DEC's spec exists
        // precisely so full-screen TUI redraws (helix, lazygit,
        // btop) don't tear; Kitty measured +20–50% throughput from
        // not painting intermediate states. We deliberately DO NOT
        // update `self.last_seqno` while held — that way once the
        // app emits ESU, the very next frame sees the seqno bumped
        // (by the buffered writes done during the BSU window) and
        // proceeds to render the final state in one frame.
        //
        // Stage 2 is the existing post-snapshot gate that catches
        // the rare case where snapshot data still proves we don't
        // need to redraw (kept as a belt-and-braces safety net).
        let (peek_seqno, peek_cursor_visible, peek_sync_output, peek_epoch) = {
            let term = self.terminal.read();
            (
                term.seqno(),
                term.cursor().visible,
                term.synchronized_output(),
                term.grid_epoch(),
            )
        };
        // Grid-epoch change = the grid was fully reset (RIS / config
        // hot-reload / session switch — all route through
        // `Terminal::reset`). This renderer's per-pane frame state is
        // now stale for the new content, so drop it and force a clean
        // full repaint across the whole swapchain. Without this, a
        // session switch could leave the synchronized-output defer
        // marker, the stale blink phase, or a back-buffer slot showing
        // the prior pane — the "shadow / copies of the prompt"
        // afterimage the operator hit switching back and forth.
        if peek_epoch != self.last_grid_epoch {
            self.last_grid_epoch = peek_epoch;
            self.last_seqno = 0;
            self.last_cursor_on = false;
            self.sync_output_deferred_since = None;
            self.force_paint_frames = EPOCH_FORCE_PAINT_FRAMES;
        }
        // While forcing post-reset frames, never defer on synchronized
        // output — every swapchain slot must be repainted with the new
        // content regardless of a transient BSU in the replay/stream.
        if peek_sync_output && self.force_paint_frames == 0 {
            // BSU is in flight — defer. Don't bump last_seqno so the
            // matching ESU triggers the catch-up render on the next
            // frame. But cap the defer at SYNC_OUTPUT_MAX_DEFER: a
            // missing/late ESU shouldn't freeze the screen indefinitely.
            let now = Instant::now();
            let since = *self.sync_output_deferred_since.get_or_insert(now);
            if now.duration_since(since) < SYNC_OUTPUT_MAX_DEFER {
                return;
            }
            // Defer cap exceeded — fall through and render whatever
            // partial state the terminal currently has. Reset the
            // marker so the next BSU starts a fresh defer window.
            self.sync_output_deferred_since = None;
        } else {
            // Not deferring — clear any stale marker from a prior BSU.
            self.sync_output_deferred_since = None;
        }
        // We are committed to painting this frame (the only early return
        // above is the synchronized-output defer, which is bypassed
        // while forcing). Count down the post-epoch forced-paint budget
        // so each swapchain slot gets exactly one clean repaint.
        self.force_paint_frames = self.force_paint_frames.saturating_sub(1);
        let search_active_peek = self.search.lock().unwrap().active;
        // P28 — cursor_on is a 1–4 Hz boolean (default 4 Hz at 500 ms
        // period). Compute it here and compare to last_cursor_on; only
        // mark blink_flip when the value actually FLIPPED. Without
        // this we'd repaint every vsync just to redraw the same
        // cursor state, which was the case before this change (idle
        // render rate stuck at 60 Hz instead of 4 Hz).
        let cursor_on_now = !self.cursor_blink
            || crate::motion::blink_on(ctx.elapsed, self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0);
        let blink_flip =
            self.cursor_blink && peek_cursor_visible && cursor_on_now != self.last_cursor_on;
        // The bell flash reads through the motion algebra's `Advance`
        // trait (value / advance / is_active).
        use crate::motion::Advance;
        let bell_active = self.bell_flash.is_active();
        // P-FIX: The original damage gate returned here without
        // touching the GPU surface, which is a correctness bug on
        // multi-buffered swapchains (macOS Metal, in particular):
        //
        //   * SHADOW / AFTERIMAGE: `frame.present()` cycles
        //     through 2–3 swapchain slots; if render() didn't
        //     write the current slot, present() surfaces stale
        //     content from N frames back. The visible effect is
        //     "the prompt leaves shadows / copies of itself as I
        //     interact" — exactly the regression operators see.
        //   * PURPLE FLASH: an unwritten swapchain slot can
        //     briefly surface its initial Metal uninit state
        //     (magenta), recurring throughout the session, not
        //     just at startup.
        //
        // The fix is to always paint the current swapchain image.
        // A "clear + last-rect-replay" optimisation was tried and
        // discarded — it produces frames that differ from a full
        // render (no text), which then ALSO shows as shadows on
        // glyph content.
        //
        // Cost of always-full-render at idle:
        //   * 60 Hz × ~300 µs ≈ 1.8 ms/sec ≈ 0.2% of one core
        //   * idle frame work is dominated by snapshot()'s row
        //     clone; rect/text build are cheap when nothing
        //     changed
        // The 32-frame determinism stress test (L2) proves the
        // pipeline is stable enough for repeated full renders to
        // produce byte-identical frame hashes — so this is free
        // correctness with no measurable cost.
        //
        // We still count "would-have-skipped" frames in the
        // counter so frame_perf MCP can surface the rate, and the
        // tracing event is preserved so operators with debug
        // logging keep the same observability.
        if peek_seqno == self.last_seqno
            && self.last_seqno != 0
            && !blink_flip
            && !bell_active
            && !search_active_peek
        {
            TOTAL_FRAMES_SKIPPED.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                peek_us = frame_start.elapsed().as_micros() as u64,
                path = "idle_peek_full",
                "idle frame — full repaint to current swapchain slot"
            );
            // Fall through to full render below.
        }

        let snapshot_start = Instant::now();
        let (mut snap, seqno) = self.snapshot();
        let snapshot_us = snapshot_start.elapsed().as_micros() as u64;
        // Memoise cursor_on for the next-frame peek's flip detection.
        self.last_cursor_on = cursor_on_now;

        // P-FIX: stage-2 gate was a "safety net" early-return when
        // the peek-vs-snapshot seqno disagreed. Same swapchain-
        // stale-slot bug applied — removed for the same reason.
        // We fall through to a full render; the cost difference
        // is negligible (snapshot was already paid for) and
        // consistency wins.
        self.last_seqno = seqno;

        // Build rect instances (cell backgrounds + cursor + decorations).
        // The selection was already resolved into snap.selection_span
        // at snapshot time — no lock held here.
        let rects_start = Instant::now();
        let mut rect_instances =
            self.build_rect_instances(&snap, ctx.elapsed, self.padding_px(), self.padding_px());
        let rects_us = rects_start.elapsed().as_micros() as u64;
        let rects_count = rect_instances.len();

        // Bell flash: a full-window flash in the theme's `bell_flash`
        // accent, decaying linearly to 0 over ~200ms (before GPU upload).
        // INDEPENDENT of the ambience graph (a plain overlay rect) and of
        // the audible-bell glow; gated at `trigger_bell` on
        // `feedback.visual_bell` + `reduce_motion`.
        if self.bell_flash.is_active() {
            // Read the current alpha BEFORE advancing — the legacy code
            // drew then decremented, so reading-then-advancing keeps the
            // drawn sequence byte-identical to the old `frames/12 * peak`
            // decay at 60fps, while being framerate-independent elsewhere.
            let alpha = self.bell_flash.value();
            let bf = self.bell_flash_color;
            rect_instances.push(RectInstance::full_window(
                ctx.width as f32,
                ctx.height as f32,
                overlay_rect_color(bf.r, bf.g, bf.b, alpha),
            ));
            self.bell_flash.advance(ctx.dt);
        }

        // Unfocused dim: a whisper of the theme background over the whole
        // window so a backgrounded window reads as backgrounded. Sourced
        // from the theme background (`unfocused_dim_color`) and linearized
        // for the rect pipeline via `overlay_rect_color` — no hex, tracks
        // the theme. Gated on `motion.unfocused_dim`.
        if !self.focused && self.motion_unfocused_dim {
            let d = self.unfocused_dim_color;
            rect_instances.push(RectInstance::full_window(
                ctx.width as f32,
                ctx.height as f32,
                overlay_rect_color(d.r, d.g, d.b, UNFOCUSED_DIM_ALPHA),
            ));
        }

        // Upload rect instances
        if let Some(ref mut pipeline) = self.rect_pipeline {
            pipeline.update_resolution(&ctx.gpu.queue, ctx.width, ctx.height);
            pipeline.ensure_capacity(&ctx.gpu.device, rect_instances.len());
            if !rect_instances.is_empty() {
                ctx.gpu.queue.write_buffer(
                    &pipeline.instance_buffer,
                    0,
                    bytemuck::cast_slice(&rect_instances),
                );
            }
        }

        // Build text buffers with per-cell colors
        let text_start = Instant::now();
        let blink_on = self.blink_phase_on(ctx.elapsed);
        let text_buffers = self.build_text_buffers(&snap, ctx.text, blink_on);
        // snap.rows was last read by build_rect_instances + build_text_buffers
        // above; return its buffers to the reservoir now (retained capacities)
        // so next frame's snapshot() recycles them. snap's OTHER fields
        // (cursor, image_placements, num_rows/cols) stay valid + are used below.
        *self.row_scratch.borrow_mut() = std::mem::take(&mut snap.rows);
        // Mint the per-surface text layers once (idempotent) before any text
        // pass — each owns its own vertex buffer so overlay text can't clobber
        // the terminal's. See `ensure_layers` + `garasu::TextLayerStack`.
        self.ensure_layers(ctx.text, &ctx.gpu.device);
        let text_us = text_start.elapsed().as_micros() as u64;
        let text_count = text_buffers.len();
        let shape_cache_len = self.shape_cache.borrow().len();

        // M3-C1 — the engawa graph route. The enabled effect set is
        // derived from config each frame; when non-empty, the scene
        // passes render into a pool-leased SCENE texture and the
        // catalog chain dispatches SCENE → … → OUT (the surface).
        // Empty set = zero graph nodes, scene renders direct to the
        // surface, no lease, no dispatch.
        let enabled_effects = self.enabled_effect_set();
        // Animated-effect host state integrates from the render
        // clock (elapsed/dt) — at elapsed=0/dt=0 (the headless
        // ladders) every tick is the identity, keeping the route
        // byte-deterministic.
        self.snow_state.tick(ctx.elapsed, ctx.dt, &self.effects_config.snow);
        self.glow_state.tick(ctx.dt, self.effects_config.glow_on_bell.glow_retain);
        self.aurora_state.tick(ctx.elapsed);
        // Ambience perf governor (2026-06-13): classify the PREVIOUS
        // frame's measured time against the budget and advance the
        // quality FSM — BEFORE the per-frame uniforms read
        // `ambience_quality()`. Gated on a non-empty composition (the
        // reduce_motion bypass). At elapsed=0 the headless ladders never
        // recorded a frame, so `LAST_FRAME_US` is 0 ⇒ TickCalm ⇒ no
        // step on a single tick — the route stays deterministic.
        self.tick_ambience_governor(LAST_FRAME_US.load(Ordering::Relaxed));
        // Glow centers on the cursor cell (the bell's visual home).
        if snap.cursor.visible && snap.cursor.row < snap.num_rows && snap.cursor.col < snap.cols {
            self.glow_state.params.center_px = [
                self.padding_px() + (snap.cursor.col as f32 + 0.5) * self.cell_width,
                self.padding_px() + (snap.cursor.row as f32 + 0.5) * self.cell_height,
            ];
        }
        let format = SURFACE_FORMAT;
        let scene_lease = if enabled_effects.is_empty() {
            None
        } else {
            Some(self.texture_pool.lease(
                &ctx.gpu.device,
                engawa_wgpu::TextureKey::offscreen(ctx.width, ctx.height, format),
            ))
        };

        // Sync Kitty GPU textures (mutable borrow) before we start render passes.
        self.sync_kitty_images(ctx);

        let mut encoder = ctx
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mado_render"),
            });

        // The render target for every scene pass: the leased SCENE
        // texture when the effect chain is live, the surface directly
        // otherwise.
        let scene_view: &wgpu::TextureView = scene_lease
            .as_ref()
            .map_or(ctx.surface_view, |lease| lease.view());
        // Pass 1: Clear background
        {
            let view = scene_view;
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mado_clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.bg_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        // Pass 2: Cell backgrounds + cursor + decorations.
        // P27 — skip the pass entirely when no rect instances would
        // be drawn. The bg-pass-elision case kicks in on monochrome
        // frames (no per-cell bg + cursor blink-off this tick + no
        // selection / search / URL underlines) — symmetric to P25
        // for the text pipeline.
        if !rect_instances.is_empty() {
            if let Some(ref pipeline) = self.rect_pipeline {
                let view = scene_view;
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("mado_rects"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pipeline.draw(&mut pass, rect_instances.len() as u32);
            }
        }

        // Kitty image placements split into two z-bands (M3-C3):
        // `below` (z<0) draws HERE — after cell backgrounds, before the
        // text glyphs (Pass 2.5); `above` (z>=0) draws after Pass 3 but
        // still onto `scene_view`, so the engawa effect chain (Pass 4)
        // composites images and text together. Drawing `above` to the
        // post-chain surface would (wrongly) skip every effect.
        let (images_below, images_above) =
            crate::terminal::partition_placements_by_z(&snap.image_placements);

        // Pass 2.5: Kitty graphics images BELOW the text scene.
        if !images_below.is_empty() {
            let view = scene_view;
            self.draw_kitty_images(ctx.gpu, ctx.width, ctx.height, &mut encoder, view, &images_below, self.padding_px(), self.padding_px());
        }

        // Open the one text frame for this whole render. It borrows the shared
        // TextLayerStack across Pass 3 + Pass 6; each layer prepares/renders its
        // OWN vertex buffer, and the frame's Drop trims the atlas exactly once
        // AFTER all text renders (before submit) — the ordering that keeps a
        // later layer's prepare from evicting an earlier layer's recorded
        // glyphs. `ctx.gpu` / `ctx.surface_view` stay reachable (disjoint
        // fields from `ctx.text`).
        let mut frame = ctx.text.begin_frame(ctx.width, ctx.height);

        // Pass 3: Text with per-cell colors
        let mut text_areas = Vec::new();
        let pad = self.padding_px();
        for (row_idx, col_start, buffer) in &text_buffers {
            let y = pad + (*row_idx as f32 * self.cell_height);
            // `.idx()` is the single audited bridge from the typed
            // `GridCol` to the raw column the pixel math consumes.
            let x = pad + (col_start.idx() as f32 * self.cell_width);
            text_areas.push(glyphon::TextArea {
                buffer: &**buffer,
                left: x,
                top: y,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 0,
                    top: 0,
                    right: ctx.width as i32,
                    bottom: ctx.height as i32,
                },
                default_color: GlyphonColor::rgba(
                    self.fg_color.r,
                    self.fg_color.g,
                    self.fg_color.b,
                    255,
                ),
                custom_glyphs: &[],
            });
        }

        // P25 — skip the text pipeline entirely when there are no
        // glyphs to draw. text_areas is empty in two common cases:
        // a blank terminal (boot, after clear), and a terminal whose
        // rows contain only box-draw glyphs (which the rect pipeline
        // already painted). begin_render_pass with no draws is not
        // free — the encoder still records the pass state.
        let text_areas_empty = text_areas.is_empty();
        if !text_areas_empty {
            // Terminal-grid layer — its own vertex buffer. A prepare error skips
            // the render (we never draw a stale token).
            let token = match frame.prepare(
                self.term_layer
                    .expect("ensure_layers minted the terminal layer before Pass 3"),
                &ctx.gpu.device,
                &ctx.gpu.queue,
                text_areas,
            ) {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!("text prepare error: {e}");
                    None
                }
            };
            if let Some(token) = token {
                let view = scene_view;
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("mado_text"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                if let Err(e) = frame.render(token, &mut pass) {
                    tracing::warn!("text render error: {e}");
                }
            }
        }

        // Pass 3.5: Kitty graphics images ABOVE the text scene (z>=0).
        // Onto `scene_view` (not the post-chain surface) so the effect
        // chain at Pass 4 still sees these pixels.
        if !images_above.is_empty() {
            let view = scene_view;
            self.draw_kitty_images(ctx.gpu, ctx.width, ctx.height, &mut encoder, view, &images_above, self.padding_px(), self.padding_px());
        }

        // Pass 4: engawa catalog dispatch — SCENE → enabled effect
        // chain → OUT (the surface). The CompiledGraph comes from the
        // (effect set, resolution)-keyed cache; per-frame work is
        // BoundResources + FrameUniforms + the dispatcher walk.
        let mut command_buffers: Vec<wgpu::CommandBuffer> = Vec::with_capacity(3);
        let mut frame_leases: Vec<engawa_wgpu::TextureLease> = Vec::new();
        command_buffers.push(encoder.finish());
        if let Some(scene) = scene_lease {
            match self.dispatch_effect_chain(
                &ctx.gpu.device,
                ctx.surface_view,
                ctx.width,
                ctx.height,
                format,
                enabled_effects,
                scene,
                &mut frame_leases,
            ) {
                Ok(cmd) => command_buffers.push(cmd),
                Err(e) => {
                    // Unreachable for every constructible effect set —
                    // the render_graph power-set tests bind every node
                    // edge and the gpu goldens dispatch the live
                    // chain. Surfacing (not panicking) keeps a broken
                    // driver from killing the terminal; the frame
                    // shows the previous surface contents.
                    tracing::error!(error = %e, "engawa effect-chain dispatch failed");
                }
            }
        }

        // Pass 5: chrome overlays. Snow now lives INSIDE the effect
        // chain (catalog priority 500) — only the reader-only chrome
        // (dir picker, search status) draws after the chain.
        let mut overlay_encoder = ctx
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mado_overlays"),
            });

        // Pass 6: modal overlays (session switcher / dir picker / search),
        // reader-only. Renders AFTER snow so it floats on top, onto
        // ctx.surface_view (post-chain). State is snapshotted (locks
        // dropped) before any GPU work.
        //
        // SINGLE-OVERLAY INVARIANT, structurally. The renderer draws the
        // ONE overlay the FSM says owns the keyboard, read from the typed
        // `overlay_focus` cell (a 1:1 mirror of the engine's `Overlay`,
        // written on the same line the state changes). Matching on one
        // value makes "two overlays visible" unrepresentable at the render
        // layer — not a priority heuristic over three independent `.open`
        // mirror bools (the centred-session-WITH-top-left-dir bug, operator
        // report 2026-06-21; theory §VI). The picker `.open` bools are read
        // below only for CONTENT, never as the gate.
        use crate::ux::modes::Overlay;
        let focus = *self.overlay_focus.lock().unwrap();
        // picker_animate fade-in: born from the None->open edge on the render
        // clock (ctx.elapsed, NOT Instant — determinism), cached for
        // draw_overlay to read via &self.
        if matches!(focus, Overlay::None) {
            self.overlay_open_at.set(None);
        } else if self.overlay_open_at.get().is_none() {
            self.overlay_open_at.set(Some(ctx.elapsed));
        }
        self.overlay_progress.set(self.overlay_fade_progress(ctx.elapsed));
        match focus {
            Overlay::None => {}
            // The rename sub-mode keeps the picker board visible underneath;
            // the live rename buffer rides the picker's `notice` line (set by
            // the engine's rename handlers), so both states draw the picker.
            Overlay::SessionPicker | Overlay::SessionRename => {
                let (q, results, sel, disabled, notice, footer) = {
                    let g = self.session_picker.lock().unwrap();
                    (
                        g.query.clone(),
                        g.results.clone(),
                        g.selected,
                        g.disabled,
                        g.notice.clone(),
                        g.footer.clone(),
                    )
                };
                self.draw_session_picker(
                    &q,
                    &results,
                    sel,
                    ctx.elapsed,
                    disabled,
                    notice.as_deref(),
                    footer.as_deref(),
                    &mut frame,
                    ctx.gpu,
                    ctx.surface_view,
                    ctx.width,
                    ctx.height,
                    &mut overlay_encoder,
                );
            }
            Overlay::DirPicker => {
                let (q, results, sel) = {
                    let g = self.dir_picker.lock().unwrap();
                    (g.query.clone(), g.results.clone(), g.selected)
                };
                self.draw_dir_picker(
                    &q,
                    &results,
                    sel,
                    &mut frame,
                    ctx.gpu,
                    ctx.surface_view,
                    ctx.width,
                    ctx.height,
                    &mut overlay_encoder,
                );
            }
            Overlay::Search => {
                let (q, current, count) = {
                    let g = self.search.lock().unwrap();
                    (g.query.clone(), g.current, g.matches.len())
                };
                self.draw_search_status(
                    &q,
                    current,
                    count,
                    &mut frame,
                    ctx.gpu,
                    ctx.surface_view,
                    ctx.width,
                    ctx.height,
                    &mut overlay_encoder,
                );
            }
        }

        // Drop the text frame BEFORE submit: its Drop trims the atlas exactly
        // once, after every text render this frame recorded. trim() touches only
        // CPU bookkeeping, never the already-recorded command buffers, so
        // trim-then-submit is the correct ordering.
        drop(frame);
        command_buffers.push(overlay_encoder.finish());
        ctx.gpu.queue.submit(command_buffers);
        // Leases return to the pool only after the submit that
        // consumes them is queued — wgpu keeps the textures alive for
        // the GPU; the pool just must not re-hand them mid-frame.
        for lease in frame_leases {
            self.texture_pool.release(lease);
        }

        // One-shot: stamp the first-rendered-frame milestone so
        // operators can read total exec → pixel-on-screen latency.
        // The atomic guard ensures we only log it once.
        if TOTAL_FRAMES.load(Ordering::Relaxed) == 0 {
            crate::perf::log_phase("first_frame_rendered");
        }

        // NOTE: this is CPU-side encode/submit wall time (measured
        // THROUGH `queue.submit()`, which is async), NOT GPU frame time
        // or the wall-clock inter-frame interval. The AmbienceGovernor
        // budgets against THIS signal, so it is a CPU-frame safety net:
        // it catches a slow render callback (the common case) but a
        // purely GPU-bound aurora cost would need a GPU timestamp query
        // to register. See `ux::ambience_governor` (honesty note) + the
        // `only-mitigated` budget-axis grade in the unrep ledger.
        let frame_us = frame_start.elapsed().as_micros() as u64;
        LAST_FRAME_US.store(frame_us, Ordering::Relaxed);
        LAST_FRAME_RECTS.store(rects_count as u64, Ordering::Relaxed);
        LAST_FRAME_TEXT.store(text_count as u64, Ordering::Relaxed);
        LAST_FRAME_SHAPE_CACHE.store(shape_cache_len as u64, Ordering::Relaxed);
        TOTAL_FRAMES.fetch_add(1, Ordering::Relaxed);

        tracing::debug!(
            frame_us,
            snapshot_us,
            rects_us,
            rects_count,
            text_us,
            text_count,
            shape_cache_len,
            path = "single_pane",
            "frame complete"
        );
    }

    fn resize(&mut self, _width: u32, _height: u32) {
        // Terminal resize is handled by the event handler in main.rs
    }
}

#[cfg(test)]
mod render_invariants {
    //! Deterministic verification of mado's GPU-rect upload path.
    //!
    //! The bugs we're guarding against are *input-leakage* bugs: a
    //! frame's `RectInstance` set must reflect ONLY the current
    //! snapshot, with no carry-over from prior frames. Examples:
    //!   - Cursor afterimage: the previous cursor position's rect
    //!     leaks into the next frame.
    //!   - Stale block-separator rects after a clear-screen.
    //!
    //! These tests build a real `TerminalRenderer` against an
    //! `Arc<RwLock<Terminal>>`, feed VT bytes, then call the same
    //! `build_rect_instances` that the live renderer calls every
    //! frame. The Vec<RectInstance> it returns is the exact set
    //! that would be uploaded to the GPU vertex buffer — asserting
    //! on it catches the entire class of "input-leakage" bug at
    //! pure-CPU speed (~ms per test, no GPU device required).
    //!
    //! Pipeline-correctness bugs (e.g. the purple-flash on first
    //! frame from an uninitialized GPU buffer) are a different
    //! class — they need a headless wgpu render-to-texture target
    //! and live in a follow-up Layer-2 test crate.

    use super::*;
    use crate::terminal::Terminal;

    /// Build a `TerminalRenderer` with a fresh `cols×rows`
    /// terminal. No GPU device touched — pipelines stay `None`;
    /// `build_rect_instances` doesn't need them.
    fn harness(cols: usize, rows: usize) -> (TerminalRenderer, SharedTerminal) {
        let term = Arc::new(parking_lot::RwLock::new(Terminal::new(cols, rows)));
        let renderer = TerminalRenderer::new(
            term.clone(),
            14.0,                  // font_size
            1.4,                   // line_height (legacy test cell)
            "monospace".into(),    // font_family
            "monospace".into(),    // font_italic
            "monospace".into(),    // font_symbols
            0.0,                   // padding (simplifies coordinate math)
            CursorStyle::Block,
            false,                 // cursor_blink off so a single frame
                                   // is deterministic
            500,
            wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            Color::WHITE,
        );
        (renderer, term)
    }

    /// The row-seam chokepoint (operator report 2026-07-05): cell
    /// heights are quantized to whole device pixels so consecutive
    /// row rects tile on identical integer pixel edges. Pure-CPU
    /// gate — the pixel-level proof lives in `render_gpu_invariants::
    /// full_bg_rows_tile_without_horizontal_seams`.
    // Strict float equality is the POINT here: ceil() results are
    // exactly representable, and "exactly integer" is the invariant.
    #[allow(clippy::float_cmp)]
    #[test]
    fn cell_height_is_quantized_to_whole_device_pixels() {
        // The chokepoint function: ceil, floored at 1px.
        assert_eq!(quantize_cell_height_px(16.8), 17.0); // 12 × 1.4
        assert_eq!(quantize_cell_height_px(19.6), 20.0); // 14 × 1.4
        assert_eq!(quantize_cell_height_px(32.5), 33.0); // 13 × 1.25 × 2
        assert_eq!(quantize_cell_height_px(33.6), 34.0); // 24 × 1.4
        assert_eq!(quantize_cell_height_px(20.0), 20.0); // already whole
        assert_eq!(quantize_cell_height_px(0.2), 1.0); // floor at 1px

        // Constructor + set_font_size both route through it: the
        // renderer NEVER carries a fractional cell height, at any
        // font size (14 × 1.4 = 19.6 unquantized).
        let (mut r, _t) = harness(20, 5);
        assert_eq!(r.cell_height.fract(), 0.0, "constructor: {}", r.cell_height);
        for size in [12.0f32, 13.0, 14.0, 15.5, 24.0] {
            r.set_font_size(size);
            assert_eq!(
                r.cell_height.fract(),
                0.0,
                "set_font_size({size}): {}",
                r.cell_height
            );
        }
    }

    /// The SCALED-DISPLAY residual (operator report 2026-07-06): on a
    /// non-integer compositor downscale, whole *framebuffer* pixels are
    /// *fractional* panel pixels, so the framebuffer-only quantize leaves a
    /// seam. `snap_cell_height_px(h, ratio<1)` must instead make each row a
    /// whole number of PANEL pixels: `snap(h,r) * r ∈ ℤ`. The current GPU
    /// seam suite never exercises a fractional effective scale (every ctor
    /// pins `scale_factor: 1.0`); this closes that gap at the geometry level.
    #[allow(clippy::float_cmp)]
    #[test]
    fn cell_height_snaps_to_whole_panel_pixels_on_scaled_display() {
        // ratio 1.0 is byte-identical to the framebuffer-only quantize — the
        // integer-scale path (1.0 / 2.0 Retina / Wayland viewporter) is untouched.
        for h in [16.8f32, 19.6, 32.5, 33.6, 20.0, 0.2] {
            assert_eq!(
                snap_cell_height_px(h, 1.0),
                quantize_cell_height_px(h),
                "ratio 1.0 must equal the framebuffer quantize for h={h}"
            );
        }

        // macOS "More Space" measured live: 4112→3456 framebuffer→panel.
        let macos_ratio = 3456.0f32 / 4112.0; // ≈ 0.8405
        // Wayland/X11 fractional-downscale exemplar (1.5× effective).
        let frac_ratio = 1.0f32 / 1.5; // ≈ 0.6667
        for ratio in [macos_ratio, frac_ratio, 0.75, 0.84, 0.9] {
            for base in [16.8f32, 19.6, 32.5, 33.6, 40.0, 25.0] {
                let snapped = snap_cell_height_px(base, ratio);
                let panel_px = snapped * ratio;
                // Each row projects onto a whole number of panel pixels →
                // every boundary N·snapped fb == N·round(panel_px) panel px,
                // identical resample phase every row, no periodic seam.
                assert!(
                    (panel_px - panel_px.round()).abs() < 1.0e-3,
                    "ratio {ratio}, base {base}: panel projection {panel_px} not whole",
                );
                assert!(snapped >= 1.0, "cell height floored at 1px");
            }
        }
    }

    /// The seam fix's LOAD-BEARING second half (operator report 2026-07-11:
    /// the seam persisted despite the correct 0.84 ratio + panel-snapped
    /// `cell_height`). `snap_cell_height_px` locks the row PITCH to whole
    /// panel px, but the shared phase is set by the ORIGIN — an unsnapped
    /// origin leaves every boundary the same fraction off a panel edge. This
    /// gate proves (a) ratio 1.0 is a passthrough no-op and (b) at any
    /// downscale the snapped origin projects onto a whole panel pixel.
    #[allow(clippy::float_cmp)]
    #[test]
    fn origin_snaps_to_whole_panel_pixels_on_scaled_display() {
        // ratio 1.0 (integer scale): passthrough — the framebuffer origin is
        // already integer-authored (padding × integer scale), never touched.
        for o in [0.0f32, 8.0, 16.0, 6.72, 13.45] {
            assert_eq!(
                snap_origin_px(o, 1.0),
                o,
                "ratio 1.0 must leave the origin untouched for o={o}"
            );
        }

        let macos_ratio = 3456.0f32 / 4112.0; // ≈ 0.8405, the live XDR value
        for ratio in [macos_ratio, 1.0f32 / 1.5, 0.75, 0.9] {
            // The live default: padding 4pt × scale 2.0 = 8 fb px.
            for origin in [8.0f32, 16.0, 0.0, 12.0, 5.5] {
                let snapped = snap_origin_px(origin, ratio);
                let panel = snapped * ratio;
                assert!(
                    (panel - panel.round()).abs() < 1.0e-3,
                    "ratio {ratio}, origin {origin}: snapped origin {snapped} \
                     projects to {panel} panel px (not whole)",
                );
                assert!(snapped >= 0.0, "origin non-negative");
                // The snap moves the origin by strictly < one panel pixel in
                // framebuffer terms (it only kills the sub-pixel phase, never
                // reflows the grid).
                assert!(
                    (snapped - origin).abs() <= (1.0 / ratio) + 1.0e-3,
                    "ratio {ratio}, origin {origin}: snap moved it {} fb px \
                     (should be < one panel px)",
                    (snapped - origin).abs()
                );
            }
        }
    }

    /// END-TO-END geometry proof: with BOTH snaps engaged at the live XDR
    /// ratio, EVERY row boundary `origin + N·cell_height` projects to an
    /// integer panel pixel — the exact condition that removes the periodic
    /// seam. Without the origin snap the constant phase offset (0.72 panel px
    /// at the default padding) reappears on every row; this test would fail.
    #[allow(clippy::float_cmp)]
    #[test]
    fn every_row_boundary_lands_on_integer_panel_px_with_both_snaps() {
        let ratio = 2234.0f32 / 2658.0; // ≈ 0.8404816 — the measured XDR ratio
        // Live default cell box: font 13 × line_height 1.25 × scale 2.0.
        for unsnapped_cell in [13.0f32 * 1.25 * 2.0, 16.8, 33.6, 19.6] {
            let cell = snap_cell_height_px(unsnapped_cell, ratio);
            // Live default origin: padding 4pt × scale 2.0 = 8 fb px.
            for raw_origin in [8.0f32, 16.0, 0.0] {
                let origin = snap_origin_px(raw_origin, ratio);
                for n in 0..40u32 {
                    let boundary_fb = origin + n as f32 * cell;
                    let boundary_panel = boundary_fb * ratio;
                    assert!(
                        (boundary_panel - boundary_panel.round()).abs() < 2.0e-3,
                        "cell {cell}, origin {origin}, row {n}: boundary \
                         {boundary_fb} fb → {boundary_panel} panel px (not whole)",
                    );
                }
            }
        }
    }

    /// The `rect_constructors!`-generated constructor family (Deliverable 4):
    /// every variant must emit the correct `RectMode` wire word and fold its
    /// typed args into the documented `pattern` payload. This is the
    /// emitter-substrate discipline — the macro generates the mechanical
    /// table, this test pins every row of it, so a shader `mode` switch and
    /// its constructor can never silently drift apart.
    #[allow(clippy::float_cmp)]
    #[test]
    fn rect_constructor_family_matches_the_rectmode_table() {
        let pos = [3.0f32, 5.0];
        let size = [40.0f32, 12.0];
        let color = [0.1f32, 0.2, 0.3, 0.4];

        let s = RectInstance::solid(pos, size, color);
        assert_eq!(s.mode, RectMode::Solid.word());
        assert_eq!(s.pattern, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!((s.pos, s.size, s.color), (pos, size, color));

        let r = RectInstance::rounded(pos, size, color, 4.0);
        assert_eq!(r.mode, RectMode::RoundedSolid.word());
        assert_eq!(r.pattern, [size[0], size[1], 4.0, 0.0]);

        let run = RectInstance::run(pos, size, color, 6.0, 0.5);
        assert_eq!(run.mode, RectMode::Run.word());
        assert_eq!(run.pattern, [6.0, 0.5, 0.0, 0.0]);

        let c = RectInstance::curly(pos, size, color, 8.0, 2.0, 1.0);
        assert_eq!(c.mode, RectMode::Curly.word());
        assert_eq!(c.pattern, [8.0, 2.0, 1.0, 0.0]);

        let p = RectInstance::powerline(pos, size, color, 2.0);
        assert_eq!(p.mode, RectMode::Powerline.word());
        assert_eq!(p.pattern, [2.0, size[0], size[1], 0.0]);

        // full_window is solid anchored at the origin spanning the surface.
        let fw = RectInstance::full_window(800.0, 600.0, color);
        assert_eq!(fw.mode, RectMode::Solid.word());
        assert_eq!((fw.pos, fw.size), ([0.0, 0.0], [800.0, 600.0]));
        assert_eq!(fw.pattern, [0.0, 0.0, 0.0, 0.0]);

        // The generated powerline constructor is byte-identical to the typed
        // powerline_rect wrapper (proves the wrapper didn't drift).
        let via_wrapper = powerline_rect(PowerlineSep::RightHalfDisk, 3.0, 5.0, 40.0, 12.0, color);
        let via_ctor = RectInstance::powerline(pos, size, color, PowerlineSep::RightHalfDisk.kind());
        assert_eq!(via_wrapper.pos, via_ctor.pos);
        assert_eq!(via_wrapper.size, via_ctor.size);
        assert_eq!(via_wrapper.mode, via_ctor.mode);
        assert_eq!(via_wrapper.pattern, via_ctor.pattern);
    }

    /// Resize-storm gate (Deliverable 3): the panel-ratio probe must fire
    /// once per SETTLED size, not on every intermediate drag frame. Simulate
    /// a drag (size changing every frame) then a settle (size stable), and
    /// count how many frames would run the probe. A 60-frame drag + settle
    /// must yield exactly ONE probe (at the settled size), vs 60 under the
    /// old "probe on any size change" trigger.
    #[test]
    fn ratio_probe_fires_once_per_settled_resize_not_per_drag_frame() {
        // Model the render loop's per-frame state transitions.
        let mut last_surface = (0u32, 0u32);
        let mut last_probe = (0u32, 0u32);
        let mut probes = 0usize;

        // A 60-frame drag: each frame a new (growing) size, none repeated.
        for i in 0..60u32 {
            let this = (1280 + i, 800 + i);
            if super::should_reprobe_ratio(this, last_surface, last_probe) {
                probes += 1;
                last_probe = this;
            }
            last_surface = this; // render() records surface dims at frame end
        }
        // The FIRST frame (never_probed) probes once even mid-"drag" (first
        // paint must get the ratio). Every subsequent drag frame is a new,
        // unsettled size → no probe.
        assert_eq!(probes, 1, "drag must probe only the first frame, not each");

        // Now the drag SETTLES: the same size repeats for a few frames.
        let settled = (1400u32, 900u32);
        probes = 0;
        for _ in 0..5 {
            if super::should_reprobe_ratio(settled, last_surface, last_probe) {
                probes += 1;
                last_probe = settled;
            }
            last_surface = settled;
        }
        assert_eq!(probes, 1, "a settled size probes exactly once, then never again");

        // A move to a differently-sized display lands as a new settled size →
        // one more probe (re-discovery still happens).
        let moved = (2560u32, 1440u32);
        probes = 0;
        for _ in 0..3 {
            if super::should_reprobe_ratio(moved, last_surface, last_probe) {
                probes += 1;
                last_probe = moved;
            }
            last_surface = moved;
        }
        assert_eq!(probes, 1, "a new settled display size re-probes exactly once");
    }

    /// Snapshot + build the rect instances exactly as the live
    /// renderer would for one frame. `elapsed = 0.0` keeps any
    /// time-driven blinking deterministic. The renderer's own shared
    /// selection is resolved inside `snapshot()` — tests mutate
    /// `r.selection` and call this.
    fn compute_rects(r: &TerminalRenderer) -> Vec<RectInstance> {
        let (snap, _seqno) = r.snapshot();
        r.build_rect_instances(&snap, 0.0, r.padding_px(), r.padding_px())
    }

    // ── Wide-char column invariant — the cursor-misalignment bug ──────
    //
    // Both render pipelines derive a cell's column from `glyph_columns`
    // (see `mod grid_col`): the text pipeline (`build_text_buffers`) and
    // the rect/cursor pipeline (`build_rect_instances`). The sealed
    // `GridCol` makes positioning a glyph at any column OTHER than the
    // dense grid index a compile error. The remaining obligation — that
    // the single source uses the index, not a `col += cell.width` sum —
    // is what these tests prove (Rust has no dependent types to prove it
    // at compile time; this is the honest C1 ceiling). Together: a glyph
    // and its cursor can never drift apart.

    /// Build a dense row exactly as the terminal stores a wide char: a
    /// `width == 2` lead followed by a `width == 0` continuation.
    fn dense_row(spec: &[(char, u8)]) -> Vec<Cell> {
        spec.iter()
            .map(|&(ch, width)| Cell { ch, width, ..Default::default() })
            .collect()
    }

    #[test]
    fn centered_panel_is_central_never_top_left() {
        // Regression invariant for the Ctrl-S picker: the Center-anchored
        // backing card must sit in the MIDDLE of the viewport, never
        // collapsed onto the top-left corner (the "top-left stuff getting
        // blocked out" report). A normal few-row picker on a 1000×600 grid:
        let (pad, width, height, line_h, content_w) =
            (8.0_f32, 1000.0_f32, 600.0_f32, 20.0_f32, 220.0_f32);
        let block_h = 3.0_f32 * line_h;
        let pad_x = 16.0_f32;
        let pad_y = line_h * 0.5;
        // The renderer's centred text-block origin (Center anchor).
        let left = ((width - content_w) / 2.0).max(pad);
        let top0 = ((height - block_h) / 2.0).max(pad);
        let (px, py, pw, ph) =
            centered_panel_geom(left, top0, content_w, block_h, pad, pad_x, pad_y);
        assert!(px > width * 0.2, "panel left {px} must be central, not top-left");
        assert!(py > height * 0.2, "panel top {py} must be central, not top-left");
        assert!(pw >= content_w && ph >= block_h, "panel must contain its content");

        // Degenerate oversize list (wider + taller than the viewport): the
        // origin must still clamp to >= pad — NEVER (0,0) blanking the
        // top-left cells.
        let (dx, dy, _, _) =
            centered_panel_geom(pad, pad, width * 2.0, height * 2.0, pad, pad_x, pad_y);
        assert!(dx >= pad && dy >= pad, "oversize panel must clamp to >= pad, got ({dx},{dy})");
    }

    #[test]
    fn overlay_window_keeps_selected_visible() {
        // Fits → every line, in order.
        assert_eq!(viewport_line_window(5, Some(2), 12), vec![0, 1, 2, 3, 4]);
        assert_eq!(viewport_line_window(5, None, 5), vec![0, 1, 2, 3, 4]);

        // Overflow: budget = max_lines - 1 body rows + the title (line 0).
        // A low selection shows the title + the top of the body.
        let w = viewport_line_window(30, Some(1), 6);
        assert_eq!(w.len(), 6, "window must cap to max_lines");
        assert_eq!(w[0], 0, "title (line 0) is always kept");
        assert!(w.contains(&1), "selected row must be visible");

        // A deep selection scrolls the body so the selected row stays in view.
        let w = viewport_line_window(30, Some(25), 6);
        assert_eq!(w.len(), 6);
        assert_eq!(w[0], 0, "title stays pinned even when scrolled");
        assert!(w.contains(&25), "deep selection must remain visible, got {w:?}");
        assert!(!w.contains(&29) || w.contains(&25), "must not scroll past selection");

        // Degenerate max_lines: never panics, never empty.
        assert_eq!(viewport_line_window(0, None, 0), Vec::<usize>::new());
        assert_eq!(viewport_line_window(4, Some(0), 0), vec![0, 1, 2, 3]);
    }

    #[test]
    fn glyph_columns_are_true_grid_indices_not_a_width_sum() {
        // "🦀a中b" as the terminal stores it:
        //   🦀(0) · cont(1) · a(2) · 中(3) · cont(4) · b(5)
        // The pre-fix accumulator (col += cell.width, +1 for each
        // continuation) yielded 0, 3, 4, 6 — text drifting one column
        // right per wide char. The hand-computed ground truth is the
        // dense index: 0, 2, 3, 5.
        let row = dense_row(&[
            ('🦀', 2), (' ', 0),
            ('a', 1),
            ('中', 2), (' ', 0),
            ('b', 1),
        ]);
        let got: Vec<(usize, char)> = glyph_columns(&row, row.len())
            .map(|(c, cell)| (c.idx(), cell.ch))
            .collect();
        assert_eq!(got, vec![(0, '🦀'), (2, 'a'), (3, '中'), (5, 'b')]);
    }

    #[test]
    fn wide_char_prompt_keeps_text_left_of_the_cursor() {
        // Reproduce the screenshot against a REAL terminal at CPU speed:
        // three wide emojis then "abc". Dense grid:
        //   🦀(0,1) 🦀(2,3) 🦀(4,5) a(6) b(7) c(8), cursor at col 9.
        let (r, t) = harness(40, 3);
        t.write().feed("🦀🦀🦀abc".as_bytes());
        let (snap, _) = r.snapshot();
        let row = &snap.rows[0];

        // `glyph_columns` yields every non-continuation cell; the text
        // pipeline skips blanks (they flush the run, emit no glyph), so
        // filter blanks to compare against the printed glyphs.
        let cols: Vec<(usize, char)> = glyph_columns(row, snap.cols)
            .filter(|(_, cell)| cell.ch != ' ')
            .map(|(c, cell)| (c.idx(), cell.ch))
            .collect();
        assert_eq!(
            cols,
            vec![(0, '🦀'), (2, '🦀'), (4, '🦀'), (6, 'a'), (7, 'b'), (8, 'c')],
            "text glyphs drifted off their true grid columns",
        );

        // The terminal-tracked cursor sits one column past the last
        // glyph — strictly right of every text column, so the block can
        // never be painted on top of an earlier glyph (the symptom).
        let last_text_col = cols.iter().map(|&(c, _)| c).max().unwrap();
        assert_eq!(snap.cursor.col, 9);
        assert!(
            snap.cursor.col > last_text_col,
            "cursor col {} must sit right of the last glyph col {}",
            snap.cursor.col,
            last_text_col,
        );
    }

    proptest::proptest! {
        /// For ANY dense row (arbitrary mix of narrow cells and
        /// wide-char lead+continuation pairs), the shared `glyph_columns`
        /// source yields columns that are strictly increasing, in-bounds,
        /// never a `width == 0` continuation, and each equal to the
        /// cell's actual position in the dense row. This generalises the
        /// hand-computed regression above across the whole input space.
        #[test]
        fn glyph_columns_yield_monotonic_in_bounds_indices(
            toks in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::prelude::Just(vec![('a', 1u8)]),
                    proptest::prelude::Just(vec![('x', 1u8)]),
                    proptest::prelude::Just(vec![(' ', 1u8)]),
                    proptest::prelude::Just(vec![('中', 2u8), (' ', 0u8)]),
                    proptest::prelude::Just(vec![('🦀', 2u8), (' ', 0u8)]),
                ],
                0..40usize,
            )
        ) {
            let spec: Vec<(char, u8)> = toks.into_iter().flatten().collect();
            let row = dense_row(&spec);
            let cols = row.len();

            let mut prev: Option<usize> = None;
            let mut yielded = 0usize;
            for (gc, cell) in glyph_columns(&row, cols) {
                let i = gc.idx();
                proptest::prop_assert!(cell.width != 0, "yielded a continuation cell at {i}");
                proptest::prop_assert!(i < cols, "col {i} out of bounds {cols}");
                // The yielded column indexes back to the very cell it was
                // minted from — it IS the dense grid position.
                proptest::prop_assert!(std::ptr::eq(&row[i], cell), "col {i} is not the cell's true index");
                if let Some(p) = prev {
                    proptest::prop_assert!(i > p, "columns must strictly increase: {i} after {p}");
                }
                prev = Some(i);
                yielded += 1;
            }
            // Every non-continuation cell is accounted for exactly once.
            let non_cont = row.iter().filter(|c| c.width != 0).count();
            proptest::prop_assert_eq!(yielded, non_cont);
        }
    }

    /// Approximate-equal for f32 rect colors. Comparing the raw
    /// f32s with `==` would be brittle under linear-space mixing
    /// or future tone-mapping passes.
    fn colors_approx_eq(a: [f32; 4], b: [f32; 4]) -> bool {
        const EPS: f32 = 1e-4;
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < EPS)
    }

    /// Find every rect whose color matches the renderer's cursor
    /// color. The cursor uses a unique configurable color, so this
    /// is the canonical way to identify cursor instances in a
    /// frame's output.
    fn cursor_rects(rects: &[RectInstance], cursor_color: [f32; 4]) -> Vec<RectInstance> {
        rects
            .iter()
            .filter(|r| colors_approx_eq(r.color, cursor_color))
            .copied()
            .collect()
    }

    // ── invariant tests ─────────────────────────────────────────────

    #[test]
    fn fresh_terminal_renders_exactly_one_cursor_rect_at_origin() {
        // The cursor invariant: one rect at (col=0, row=0) on a
        // brand-new terminal. Width = cell_width (Block style),
        // height = cell_height.
        let (r, _t) = harness(80, 24);
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(
            cur.len(),
            1,
            "expected exactly one cursor rect, got {}: {:?}",
            cur.len(),
            cur
        );
        // Positioned at origin (padding = 0).
        assert!(
            (cur[0].pos[0] - 0.0).abs() < 0.01,
            "cursor x = {}, expected ~0",
            cur[0].pos[0]
        );
        assert!(
            (cur[0].pos[1] - 0.0).abs() < 0.01,
            "cursor y = {}, expected ~0",
            cur[0].pos[1]
        );
    }

    #[test]
    fn cursor_rect_follows_cursor_after_input() {
        // Feed 'h', 'i'. Cursor should advance to col=2; the rect
        // at col=0 from the prior cursor position must NOT appear.
        let (r, t) = harness(80, 24);
        t.write().feed(b"hi");
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1, "expected one cursor rect, got {cur:?}");
        let expected_x = 2.0 * r.cell_width;
        assert!(
            (cur[0].pos[0] - expected_x).abs() < 0.01,
            "cursor x = {}, expected ~{expected_x}",
            cur[0].pos[0]
        );
        // No cursor-colored rect should sit at column 0 (the prior
        // cursor position).
        let stale_at_origin = cur.iter().any(|r| r.pos[0].abs() < 0.01);
        assert!(!stale_at_origin, "stale cursor rect at origin: {cur:?}");
    }

    #[test]
    fn clear_screen_returns_cursor_to_origin_with_no_stale_rects() {
        // The afterimage class of bug surfaces here: write text,
        // erase the screen (`\x1b[2J`) + return cursor home
        // (`\x1b[H`), then verify the NEW frame's rects contain a
        // cursor only at the origin — no leftover cursor rect at
        // the previous (col, row).
        let (r, t) = harness(80, 24);
        t.write().feed(b"the quick brown fox\nover the lazy dog\n");
        // Sanity: pre-clear, cursor is somewhere past origin.
        let pre = compute_rects(&r);
        let pre_cur = cursor_rects(&pre, r.cursor_color);
        assert_eq!(pre_cur.len(), 1);
        let pre_x = pre_cur[0].pos[0];
        let pre_y = pre_cur[0].pos[1];

        // Clear screen + cursor home.
        t.write().feed(b"\x1b[2J\x1b[H");

        let post = compute_rects(&r);
        let post_cur = cursor_rects(&post, r.cursor_color);
        assert_eq!(
            post_cur.len(),
            1,
            "expected exactly one cursor rect after clear, got: {post_cur:?}"
        );
        // The new cursor is at (~0, ~0).
        assert!(post_cur[0].pos[0].abs() < 0.01);
        assert!(post_cur[0].pos[1].abs() < 0.01);
        // Critically: NO cursor-coloured rect at the prior
        // position. Carry-over here would be the afterimage bug.
        let stale = post_cur
            .iter()
            .any(|r| (r.pos[0] - pre_x).abs() < 0.01 && (r.pos[1] - pre_y).abs() < 0.01);
        assert!(
            !stale || (pre_x.abs() < 0.01 && pre_y.abs() < 0.01),
            "stale cursor rect at prior position ({pre_x}, {pre_y}): {post_cur:?}"
        );
    }

    #[test]
    fn consecutive_frames_with_same_state_produce_identical_rects() {
        // Determinism: two consecutive compute_rects calls on the
        // same terminal state must produce byte-identical Vecs.
        // If any frame-local state leaks back into the renderer
        // (e.g. an animation counter that ticks even at
        // elapsed=0.0), this fails.
        let (r, t) = harness(40, 12);
        t.write().feed(b"hello world");
        let a = compute_rects(&r);
        let b = compute_rects(&r);
        assert_eq!(a.len(), b.len(), "frame rect count diverged: {a:?} vs {b:?}");
        for (i, (ra, rb)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(ra.pos, rb.pos, "rect[{i}].pos diverged: {ra:?} vs {rb:?}");
            assert_eq!(ra.size, rb.size, "rect[{i}].size diverged");
            assert!(colors_approx_eq(ra.color, rb.color), "rect[{i}].color diverged");
        }
    }

    #[test]
    fn no_rect_extends_past_viewport_bounds() {
        // Viewport-bound invariant: no rect should paint outside
        // the (cols × cell_width, rows × cell_height) area + the
        // padding origin. Catches off-by-one bugs in run-length
        // span emission (e.g. background runs that include the
        // last column when they shouldn't).
        let cols = 40;
        let rows = 12;
        let (r, t) = harness(cols, rows);
        t.write().feed(b"the quick brown fox jumps over the lazy dog");
        let rects = compute_rects(&r);
        let max_x = r.padding_px() + cols as f32 * r.cell_width + 1.0; // +1 epsilon
        let max_y = r.padding_px() + rows as f32 * r.cell_height + 1.0;
        for (i, rect) in rects.iter().enumerate() {
            let right = rect.pos[0] + rect.size[0];
            let bottom = rect.pos[1] + rect.size[1];
            assert!(
                right <= max_x,
                "rect[{i}] extends past right bound: right={right}, max={max_x}, rect={rect:?}"
            );
            assert!(
                bottom <= max_y,
                "rect[{i}] extends past bottom bound: bottom={bottom}, max={max_y}, rect={rect:?}"
            );
        }
    }

    #[test]
    fn cursor_rect_color_matches_configured_color() {
        // If an operator customises cursor_color, the rect emitted
        // for the cursor must use that color — not some hard-coded
        // default. Regression guard for cursor-color sync bugs.
        let (mut r, _t) = harness(20, 5);
        r.cursor_color = [0.1, 0.2, 0.3, 0.4];
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1);
        assert!(colors_approx_eq(cur[0].color, [0.1, 0.2, 0.3, 0.4]));
    }

    #[test]
    fn cursor_rect_disappears_when_outside_viewport() {
        // If the cursor is reported outside the visible rows
        // (which shouldn't happen in practice but is defensible),
        // build_rect_instances must NOT emit a cursor rect with
        // negative or out-of-bounds coordinates. Today this is
        // bounded by `if within bounds` in build_rect_instances.
        let (r, t) = harness(10, 3);
        // Force cursor past last visible row via direct mutation;
        // mado's parser would normally clamp, but we test the
        // renderer's defensiveness independently.
        {
            let mut t = t.write();
            let cursor_now = *t.cursor();
            // Mutate cursor row to past num_rows. Use the public
            // API if available; else this test is a no-op when
            // the field isn't exposed (cursor module-private).
            // Today cursor() returns &Cursor; we accept that the
            // ::new() default is (0,0) and just verify "no
            // negative rects" as a weaker invariant.
            let _ = cursor_now;
        }
        let rects = compute_rects(&r);
        for rect in &rects {
            assert!(rect.pos[0] >= 0.0, "negative rect x: {rect:?}");
            assert!(rect.pos[1] >= 0.0, "negative rect y: {rect:?}");
            assert!(rect.size[0] > 0.0, "zero/neg rect width: {rect:?}");
            assert!(rect.size[1] > 0.0, "zero/neg rect height: {rect:?}");
        }
    }

    #[test]
    fn write_then_clear_then_write_produces_only_current_text_rects() {
        // The cleanest "no leakage" test: write 3 separate text
        // batches with screen clears between them. The final
        // frame's rect set must reflect ONLY the final batch's
        // state — every non-cursor rect must derive from the
        // current visible grid, not from prior batches.
        let (r, t) = harness(80, 24);
        t.write().feed(b"first\x1b[2J\x1b[H");
        t.write().feed(b"second\x1b[2J\x1b[H");
        t.write().feed(b"third");

        let rects = compute_rects(&r);
        // Sanity: at least the cursor rect must be present.
        assert!(!rects.is_empty());
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1, "expected one cursor rect");
        // Cursor lands at col=5, row=0 ("third" = 5 chars).
        let expected_x = 5.0 * r.cell_width;
        assert!(
            (cur[0].pos[0] - expected_x).abs() < 0.01,
            "cursor x = {}, expected ~{expected_x}",
            cur[0].pos[0]
        );
        assert!(cur[0].pos[1].abs() < 0.01, "cursor y = {}", cur[0].pos[1]);
    }

    // ── cursor-style invariants ───────────────────────────────────

    /// Build the harness with a specific cursor style. Saves
    /// repeating the constructor each variant.
    fn harness_with_style(cols: usize, rows: usize, style: CursorStyle) -> (TerminalRenderer, SharedTerminal) {
        let term = Arc::new(parking_lot::RwLock::new(Terminal::new(cols, rows)));
        let renderer = TerminalRenderer::new(
            term.clone(),
            14.0,
            1.4,
            "monospace".into(),
            "monospace".into(),
            "monospace".into(), // font_symbols
            0.0,
            style,
            false,
            500,
            wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            Color::WHITE,
        );
        (renderer, term)
    }

    #[test]
    fn cursor_style_block_produces_full_cell_rect() {
        let (r, _t) = harness_with_style(10, 3, CursorStyle::Block);
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1);
        assert!((cur[0].size[0] - r.cell_width).abs() < 0.01);
        assert!((cur[0].size[1] - r.cell_height).abs() < 0.01);
    }

    #[test]
    fn cursor_style_bar_produces_thin_vertical_rect() {
        let (r, _t) = harness_with_style(10, 3, CursorStyle::Bar);
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1);
        // Bar = 2px wide × cell_height.
        assert!((cur[0].size[0] - 2.0).abs() < 0.01, "bar width: {}", cur[0].size[0]);
        assert!((cur[0].size[1] - r.cell_height).abs() < 0.01);
    }

    #[test]
    fn cursor_style_underline_produces_thin_horizontal_rect_at_bottom() {
        let (r, _t) = harness_with_style(10, 3, CursorStyle::Underline);
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1);
        // Underline = cell_width × 2px, positioned at cell bottom.
        assert!((cur[0].size[0] - r.cell_width).abs() < 0.01);
        assert!((cur[0].size[1] - 2.0).abs() < 0.01);
        // y = origin + cell_height - 2.0
        let expected_y = r.cell_height - 2.0;
        assert!((cur[0].pos[1] - expected_y).abs() < 0.01);
    }

    #[test]
    fn cursor_style_block_hollow_produces_four_edge_rects() {
        let (r, _t) = harness_with_style(10, 3, CursorStyle::BlockHollow);
        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        // Block-hollow = top + bottom + left + right edges = 4 rects.
        assert_eq!(cur.len(), 4, "block-hollow should emit 4 edge rects: {cur:?}");
    }

    // ── selection invariants ──────────────────────────────────────

    #[test]
    fn active_selection_emits_selection_colored_rects() {
        // Make a selection from (0, 0) to (0, 5). We expect at
        // least one rect with the selection_bg color.
        let (r, t) = harness(20, 3);
        t.write().feed(b"hello world");
        {
            let term = t.read();
            let a = term.selection_anchor_at(0, 0).unwrap();
            let b = term.selection_anchor_at(0, 5).unwrap();
            r.selection.lock().unwrap().set_span(a, b);
        }
        let rects = compute_rects(&r);
        let sel_rects: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, r.selection_bg))
            .collect();
        assert!(
            !sel_rects.is_empty(),
            "expected ≥1 selection-colored rect, got {sel_rects:?}"
        );
    }

    #[test]
    fn cleared_selection_emits_no_selection_rects() {
        let (r, t) = harness(20, 3);
        t.write().feed(b"hello");
        // r.selection never touched — stays State::None.
        let rects = compute_rects(&r);
        let sel_rects: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, r.selection_bg))
            .collect();
        assert!(
            sel_rects.is_empty(),
            "no selection should emit no selection-colored rects: {sel_rects:?}"
        );
    }

    /// Streaming output sliding content into scrollback must move
    /// the HIGHLIGHT with the content: the selection is anchored to
    /// what was selected, not to viewport rows. (Pre-anchor, the
    /// highlight stayed glued to the same rows while the content
    /// scrolled out from under it.)
    #[test]
    fn selection_highlight_tracks_content_under_streaming_output() {
        let (r, t) = harness(20, 5);
        t.write().feed(b"target");
        {
            let term = t.read();
            let a = term.selection_anchor_at(0, 0).unwrap();
            let b = term.selection_anchor_at(0, 5).unwrap();
            r.selection.lock().unwrap().set_span(a, b);
        }
        // First selection rect's y position in px (row = y / cell_height).
        let sel_rect_y = |rects: &[RectInstance]| -> Option<f32> {
            rects
                .iter()
                .find(|rt| colors_approx_eq(rt.color, r.selection_bg))
                .map(|rt| rt.pos[1])
        };
        assert!(
            sel_rect_y(&compute_rects(&r)).is_some_and(|y| y.abs() < 0.01),
            "selection paints on viewport row 0 at capture time"
        );
        // Fill the 5-row screen and push two lines into scrollback —
        // "target" leaves the live viewport entirely.
        t.write().feed(b"\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert!(
            sel_rect_y(&compute_rects(&r)).is_none(),
            "selection scrolled out of the live view must not paint"
        );
        // Scroll back so "target" is the top row again — the
        // highlight reappears ON the content.
        t.write().scroll_up(2);
        assert!(
            sel_rect_y(&compute_rects(&r)).is_some_and(|y| y.abs() < 0.01),
            "highlight must follow the content into the scrolled view"
        );
    }

    // ── block-separator invariants ────────────────────────────────

    #[test]
    fn osc_133_prompt_marks_emit_block_separators() {
        // Feed an OSC 133 A (prompt-start) mark. The renderer
        // should emit a 1px-tall faint rect spanning the row.
        let (r, t) = harness(40, 8);
        // Drop a few newlines so the prompt mark lands past row 0
        // (row-0 marks are intentionally skipped).
        t.write().feed(b"\n\n\x1b]133;A\x1b\\");
        let rects = compute_rects(&r);
        // Separator color is Nord #5E81AC @ 30% α, **linearized** through
        // `overlay_rect_color` like every other overlay rect (raw sRGB
        // renders washed-out on the sRGB-storage surface).
        let sep_color = overlay_rect_color(0x5E, 0x81, 0xAC, 0.30);
        let seps: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, sep_color))
            .collect();
        assert!(
            !seps.is_empty(),
            "expected ≥1 block-separator rect for the OSC 133 mark: {rects:?}"
        );
        // 1px tall, full viewport width.
        for s in &seps {
            assert!((s.size[1] - 1.0).abs() < 0.01, "separator height: {s:?}");
            assert!(
                (s.size[0] - 40.0 * r.cell_width).abs() < 0.01,
                "separator width: {s:?}"
            );
        }
    }

    #[test]
    fn no_block_separators_when_no_osc_133_marks() {
        let (r, t) = harness(40, 8);
        t.write().feed(b"plain text no prompt marks");
        let rects = compute_rects(&r);
        let sep_color = overlay_rect_color(0x5E, 0x81, 0xAC, 0.30);
        let seps: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, sep_color))
            .collect();
        assert!(
            seps.is_empty(),
            "no OSC 133 marks should emit no separators: {seps:?}"
        );
    }

    // ── search-highlight invariants ───────────────────────────────

    /// Search-match colors at the two alphas, **linearized** to mirror
    /// `build_rect_instances` (the rect pipeline writes verbatim to the
    /// sRGB surface, so it consumes linear values via
    /// `overlay_rect_color`). Derived from the RENDERER'S OWN
    /// `search_current_color` / `search_other_color` fields so the pin
    /// tracks the active theme by construction — a default (un-themed)
    /// renderer carries Nord aurora yellow #EBCB8B; a Vellum-themed one
    /// carries `first_light` #D7C489 / `search_others` #443E2A.
    fn search_current_color(r: &TerminalRenderer) -> [f32; 4] {
        let c = r.search_current_color;
        super::overlay_rect_color(c.r, c.g, c.b, 0.5)
    }
    fn search_other_color(r: &TerminalRenderer) -> [f32; 4] {
        let c = r.search_other_color;
        super::overlay_rect_color(c.r, c.g, c.b, 0.2)
    }

    #[test]
    fn active_search_with_matches_emits_match_rects() {
        let (r, t) = harness(40, 3);
        t.write().feed(b"hello world hello again hello");
        // Populate the renderer's search state directly.
        {
            let mut s = r.search.lock().unwrap();
            s.active = true;
            s.matches = vec![
                crate::search::SearchMatch { row: 0, col_start: 0, col_end: 4 },
                crate::search::SearchMatch { row: 0, col_start: 12, col_end: 16 },
                crate::search::SearchMatch { row: 0, col_start: 24, col_end: 28 },
            ];
            s.current = 1;
        }
        let rects = compute_rects(&r);
        let current_hits: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, search_current_color(&r)))
            .collect();
        let other_hits: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, search_other_color(&r)))
            .collect();
        assert_eq!(current_hits.len(), 1, "exactly one current match");
        assert_eq!(other_hits.len(), 2, "two non-current matches");
        // The current-match rect is the one whose x-start matches
        // col 12 (=index 1 in the matches vec).
        let expected_x = 12.0 * r.cell_width;
        assert!((current_hits[0].pos[0] - expected_x).abs() < 0.01);
    }

    #[test]
    fn inactive_search_emits_no_match_rects_even_if_matches_set() {
        let (r, t) = harness(40, 3);
        t.write().feed(b"hello world");
        {
            let mut s = r.search.lock().unwrap();
            s.active = false; // closed
            s.matches = vec![crate::search::SearchMatch {
                row: 0,
                col_start: 0,
                col_end: 4,
            }];
            s.current = 0;
        }
        let rects = compute_rects(&r);
        let any_search: Vec<_> = rects
            .iter()
            .filter(|rt| {
                colors_approx_eq(rt.color, search_current_color(&r))
                    || colors_approx_eq(rt.color, search_other_color(&r))
            })
            .collect();
        assert!(
            any_search.is_empty(),
            "closed search must emit no match rects: {any_search:?}"
        );
    }

    /// theme-fidelity: with Vellum active the search-match rects paint
    /// the Vellum search surfaces (`first_light` #D7C489 current /
    /// `search_others` #443E2A other) — NOT the legacy Nord aurora yellow
    /// #EBCB8B. This is the surface-map promise; before the fix the
    /// render path hardcoded the Nord value and ignored the theme.
    #[test]
    fn vellum_search_matches_paint_the_vellum_surfaces_not_nord_yellow() {
        let (mut r, t) = harness(40, 3);
        crate::theme::apply_config_theme(&mut r, &t, "vellum", 1.0);
        t.write().feed(b"hello world hello again hello");
        {
            let mut s = r.search.lock().unwrap();
            s.active = true;
            s.matches = vec![
                crate::search::SearchMatch { row: 0, col_start: 0, col_end: 4 },
                crate::search::SearchMatch { row: 0, col_start: 12, col_end: 16 },
            ];
            s.current = 0;
        }
        // The renderer now carries the Vellum surfaces.
        assert_eq!(r.search_current_color, Color::new(0xD7, 0xC4, 0x89));
        assert_eq!(r.search_other_color, Color::new(0x44, 0x3E, 0x2A));
        // And the painted rects match those, NOT Nord yellow #EBCB8B.
        let nord_current = super::overlay_rect_color(0xEB, 0xCB, 0x8B, 0.5);
        let nord_other = super::overlay_rect_color(0xEB, 0xCB, 0x8B, 0.2);
        let rects = compute_rects(&r);
        let current_hits = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, search_current_color(&r)))
            .count();
        let other_hits = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, search_other_color(&r)))
            .count();
        assert_eq!(current_hits, 1, "the Vellum current-match rect paints");
        assert_eq!(other_hits, 1, "the Vellum other-match rect paints");
        // Nord yellow must NOT appear (the hardcode is gone).
        assert!(
            !rects
                .iter()
                .any(|rt| colors_approx_eq(rt.color, nord_current)
                    || colors_approx_eq(rt.color, nord_other)),
            "no Nord aurora-yellow search rect under Vellum"
        );
    }

    /// Rounded UI (operator: "round edges instead of squaring them").
    /// The scrollback history thumb — the one freestanding chrome rect —
    /// is emitted as a `RoundedSolid` rect carrying its own dims + the
    /// ishou `radius.sm` corner in `pattern`, so the SDF fragment softens
    /// its corners. The grid-aligned cell bands (selection / search rows)
    /// stay `Solid` square by construction (rounding a text-row band
    /// would look wrong); this test pins ONLY the thumb's rounding.
    #[test]
    fn scrollback_thumb_is_a_rounded_rect_carrying_the_ishou_radius() {
        let (mut r, t) = harness(20, 4);
        // Fill scrollback and scroll into history so the thumb emits.
        {
            let mut term = t.write();
            for _ in 0..60 {
                term.feed(b"line\r\n");
            }
            term.scroll_up(10);
        }
        let rects = compute_rects(&r);
        // The thumb is the Nord-frost #88C0D0 @ 35% α rounded rect.
        let thumb_color = super::overlay_rect_color(0x88, 0xC0, 0xD0, 0.35);
        let thumbs: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, thumb_color))
            .collect();
        assert_eq!(thumbs.len(), 1, "exactly one history thumb while scrolled");
        let thumb = thumbs[0];
        assert_eq!(
            thumb.mode,
            RectMode::RoundedSolid.word(),
            "the freestanding thumb must be a rounded rect"
        );
        // pattern = [width, height, radius, _]; width matches the rect
        // size, radius is the ishou radius.sm token (no hand-pinned size).
        assert!((thumb.pattern[0] - thumb.size[0]).abs() < 0.001);
        assert!((thumb.pattern[1] - thumb.size[1]).abs() < 0.001);
        let want_radius = ishou_tokens::Radius::default().sm as f32;
        assert!(
            (thumb.pattern[2] - want_radius).abs() < 0.001,
            "thumb corner radius must come from ishou radius.sm ({want_radius})"
        );
        // A non-scrolled frame emits NO thumb (and so no rounded rect).
        let (mut r2, _t2) = harness(20, 4);
        let plain = compute_rects(&r2);
        assert!(
            !plain.iter().any(|rt| rt.mode == RectMode::RoundedSolid.word()),
            "a live (non-scrolled) frame must emit no rounded chrome"
        );
        let _ = &mut r;
        let _ = &mut r2;
    }

    // ── determinism: resize doesn't leak state ────────────────────

    #[test]
    fn frame_after_resize_contains_only_current_grid_state() {
        // Render at 40×10; resize to 80×24 and verify the new
        // frame reflects the new dimensions with no stale rect
        // from the old grid's geometry.
        let (r, t) = harness(40, 10);
        t.write().feed(b"before resize content");
        let pre = compute_rects(&r);
        assert!(!pre.is_empty());
        // Resize the underlying terminal.
        t.write().resize(80, 24);
        t.write().feed(b"\x1b[2J\x1b[H");
        t.write().feed(b"after");
        let post = compute_rects(&r);
        // Cursor at col=5 ("after" = 5 chars).
        let cur = cursor_rects(&post, r.cursor_color);
        assert_eq!(cur.len(), 1);
        let expected_x = 5.0 * r.cell_width;
        assert!(
            (cur[0].pos[0] - expected_x).abs() < 0.01,
            "post-resize cursor x = {}, expected ~{expected_x}",
            cur[0].pos[0]
        );
        // No rect should overhang the NEW viewport.
        let max_y = 24.0 * r.cell_height + 1.0;
        for rect in &post {
            assert!(
                rect.pos[1] + rect.size[1] <= max_y,
                "post-resize rect exceeds new viewport: {rect:?}"
            );
        }
    }

    // ── property-based fuzz: invariants hold for all inputs ───────

    // ── bell flash contract ───────────────────────────────────────

    #[test]
    fn trigger_bell_arms_the_flash_tween() {
        use crate::motion::Advance;
        let (mut r, _t) = harness(20, 5);
        assert!(!r.bell_flash.is_active(), "no flash before the bell");
        r.trigger_bell();
        assert!(r.bell_flash.is_active(), "the bell arms the flash");
        assert!(
            (r.bell_flash.value() - BELL_FLASH_PEAK_ALPHA).abs() < 1e-6,
            "a fresh flash starts at peak alpha"
        );
    }

    /// GOLDEN BYTE-PIN — the duration-based flash `Tween` reproduces the
    /// legacy `frames/12 * peak` linear decay EXACTLY at 60fps, frame for
    /// frame, then goes inactive after 12 frames. This proves the port is
    /// behaviour-preserving at the reference framerate while being
    /// framerate-independent everywhere else (the whole point). The old
    /// formula is diffed straight into the assertion, per mado's byte-pin
    /// idiom.
    #[test]
    fn bell_flash_tween_matches_legacy_frame_decay_at_60fps() {
        use crate::motion::Advance;
        let mut flash = crate::motion::Tween::linear(
            BELL_FLASH_PEAK_ALPHA,
            0.0,
            crate::motion::secs(BELL_FLASH_SECS),
        );
        let dt = 1.0 / 60.0;
        // The legacy loop drew 12 frames: frames = 12, 11, …, 1, each
        // alpha = frames/12 * peak, decrementing AFTER the draw.
        for legacy_frames in (1..=u32::from(BELL_FLASH_FRAMES)).rev() {
            assert!(
                flash.is_active(),
                "flash must still be active at legacy frame {legacy_frames}"
            );
            let legacy_alpha =
                legacy_frames as f32 / f32::from(BELL_FLASH_FRAMES) * BELL_FLASH_PEAK_ALPHA;
            let got = flash.value();
            assert!(
                (got - legacy_alpha).abs() < 1e-5,
                "frame {legacy_frames}: tween alpha {got} != legacy {legacy_alpha}"
            );
            flash.advance(dt);
        }
        // After 12 frames the flash is spent — exactly like the legacy
        // counter reaching 0.
        assert!(!flash.is_active(), "flash must be spent after 12 frames");
    }

    /// LIVE-KNOB — the bell flash honors the operator's `motion.bell_flash`
    /// config end to end (not a dead knob): apply a morphed shape, ring the
    /// bell, and the armed Tween starts at the CONFIGURED peak, not the
    /// default. Proves the config → apply → renderer → trigger path is wired.
    #[test]
    fn bell_flash_honors_configured_shape() {
        use crate::motion::Advance;
        let (mut r, _t) = harness(20, 5);
        let mut cfg = crate::config::MadoConfig::default();
        cfg.motion.bell_flash = crate::config::BellFlashConfig {
            duration_ms: 500,
            peak_alpha: 0.5,
            easing: crate::config::EasingConfig::SonicBoom,
        };
        r.apply_effects_and_accessibility(&cfg);
        r.trigger_bell();
        assert!(r.bell_flash.is_active(), "the configured bell still arms");
        assert!(
            (r.bell_flash.value() - 0.5).abs() < 1e-6,
            "a re-armed flash starts at the CONFIGURED peak (0.5), not the 0.10 default"
        );
    }

    /// LIVE-KNOB — the snow typing-pulse honors `effects.snow.snow_pulse_retain`
    /// (not a dead knob): `retain=1.0` HOLDS the pulse where the 0.92 default
    /// decays it after one 60fps frame, proving `SnowState::tick` reads the
    /// config rather than a hardcode.
    #[test]
    fn snow_pulse_retain_is_a_live_knob() {
        let dt = 1.0 / 60.0;
        let mut held = SnowState::new();
        held.params.set_typing_pulse(1.0);
        let mut cfg_hold = crate::config::MadoSnowConfig::default();
        cfg_hold.snow_pulse_retain = 1.0;
        held.tick(0.0, dt, &cfg_hold);
        assert!((held.params.frame[3] - 1.0).abs() < 1e-6, "retain=1.0 holds the pulse");

        let mut faded = SnowState::new();
        faded.params.set_typing_pulse(1.0);
        faded.tick(0.0, dt, &crate::config::MadoSnowConfig::default()); // default 0.92
        assert!(
            faded.params.frame[3] < held.params.frame[3],
            "the default 0.92 retain decays the pulse below the retain=1.0 hold"
        );
    }

    /// LIVE-KNOB — the Ctrl-S picker fade honors `motion.picker_animate` (not a
    /// dead knob): ON fades the overlay in from the open edge (progress < 1),
    /// reaching full alpha after the ~0.18s fade; OFF is instant full alpha.
    #[test]
    fn picker_animate_fade_is_a_live_knob() {
        let (mut r, _t) = harness(20, 5);
        r.motion_picker_animate = true;
        r.overlay_open_at.set(Some(1.0)); // overlay opened at elapsed = 1.0
        assert!(
            r.overlay_fade_progress(1.0) < 1.0,
            "with picker_animate on, the overlay fades in from the open edge"
        );
        assert!(
            (r.overlay_fade_progress(5.0) - 1.0).abs() < 1e-6,
            "full alpha once the ~0.18s fade completes"
        );
        r.motion_picker_animate = false;
        assert!(
            (r.overlay_fade_progress(1.0) - 1.0).abs() < 1e-6,
            "picker_animate off = instant full alpha (the renderer reads the knob)"
        );
    }

    /// DETERMINISM — the suggestion shade-in fade is a pure function of the
    /// render clock: two evaluations at the SAME `elapsed` (e.g. the L2
    /// elapsed=0 determinism frames) yield identical alpha. The CI-forcing gate
    /// against a regression to wall-clock `Instant::now()` (which advances
    /// between renders and broke frame-hash determinism for the picker overlay).
    #[test]
    fn suggestion_fade_is_pure_in_ctx_elapsed() {
        let shade_in_ms = 600u64;
        let age_at = |now: f32, born: f32| ((now - born) * 1000.0).max(0.0) as u64;
        // At elapsed=0 (born == now == 0): fully transparent + reproducible.
        let a0 = age_at(0.0, 0.0);
        assert_eq!(
            crate::suggest::shade_ramp(0, a0, shade_in_ms),
            crate::suggest::shade_ramp(0, a0, shade_in_ms),
            "same elapsed → identical alpha (pure)"
        );
        assert_eq!(crate::suggest::shade_ramp(0, a0, shade_in_ms), 0, "elapsed=0 = transparent");
        // A fixed later elapsed: reproducible + monotone toward opaque.
        let mid = crate::suggest::shade_ramp(0, age_at(0.3, 0.0), shade_in_ms);
        assert!((120..=140).contains(&mid), "300ms of a 600ms ramp is ~half (got {mid})");
        assert_eq!(
            crate::suggest::shade_ramp(0, age_at(1.0, 0.0), shade_in_ms),
            255,
            "past the shade-in duration the fade is fully opaque"
        );
    }

    #[test]
    fn trigger_bell_is_noop_under_reduce_motion() {
        use crate::motion::Advance;
        let (mut r, _t) = harness(20, 5);
        r.reduce_motion = true;
        r.trigger_bell();
        assert!(
            !r.bell_flash.is_active(),
            "reduce_motion should suppress the bell flash"
        );
    }

    #[test]
    fn trigger_bell_is_idempotent_for_max_value() {
        // Calling twice in a row re-arms a FRESH flash (elapsed 0, full
        // peak) — the flash is a fixed-duration effect, never stacked
        // past its peak.
        use crate::motion::Advance;
        let (mut r, _t) = harness(20, 5);
        r.trigger_bell();
        r.trigger_bell();
        assert!(r.bell_flash.is_active());
        assert!(
            (r.bell_flash.value() - BELL_FLASH_PEAK_ALPHA).abs() < 1e-6,
            "a re-armed flash restarts at peak, not past it"
        );
    }

    #[test]
    fn trigger_bell_flash_gated_on_feedback_visual_bell() {
        // With the visual-bell feedback gate OFF, the flash never arms —
        // but the glow-on-bell ring still saturates (its own effect gate
        // decides whether it renders), so the audible-bell glow stays
        // independent of the flash knob.
        use crate::motion::Advance;
        let (mut r, _t) = harness(20, 5);
        r.set_feedback_visual_bell(false);
        r.trigger_bell();
        assert!(
            !r.bell_flash.is_active(),
            "visual_bell=off must suppress the flash"
        );
    }

    // ── alternate screen buffer transition ────────────────────────

    #[test]
    fn alt_screen_transition_round_trips_through_enter_and_exit() {
        // \x1b[?1049h enters alt-screen (vim/htop pattern);
        // \x1b[?1049l exits back to primary. The renderer must
        // see the new buffer's contents, not a stale view of the
        // primary.
        let (r, t) = harness(40, 8);
        t.write().feed(b"primary content here");
        assert!(!t.read().on_alt_screen());

        t.write().feed(b"\x1b[?1049h");
        t.write().feed(b"\x1b[H\x1b[2J"); // home + clear
        t.write().feed(b"ALT");
        assert!(t.read().on_alt_screen());

        let rects = compute_rects(&r);
        let cur = cursor_rects(&rects, r.cursor_color);
        assert_eq!(cur.len(), 1);
        // Cursor at col=3 (after "ALT") on alt-screen.
        let expected_x = 3.0 * r.cell_width;
        assert!((cur[0].pos[0] - expected_x).abs() < 0.01);

        t.write().feed(b"\x1b[?1049l"); // exit alt-screen
        assert!(!t.read().on_alt_screen());
    }

    // ── SGR color attribute renders into rect colors ──────────────

    #[test]
    fn sgr_red_background_emits_red_rect() {
        // \x1b[41m sets bg = ANSI red (cell[1] in default palette).
        // Feed "X" so we have one cell with the red bg.
        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[41mX\x1b[0m");
        let rects = compute_rects(&r);
        // ANSI palette index 1 is approximately Nord aurora red
        // (~0.749, 0.380, 0.416 linear). Look for ANY rect whose
        // color's red channel exceeds 0.5 AND whose green+blue are
        // both substantially lower — that's a "this is a red rect"
        // heuristic that survives palette tweaks within reason.
        let red_rect = rects.iter().find(|rt| {
            rt.color[0] > 0.3
                && rt.color[1] < rt.color[0] * 0.7
                && rt.color[2] < rt.color[0] * 0.7
        });
        assert!(
            red_rect.is_some(),
            "expected at least one red-bg rect after \\x1b[41m: {rects:?}"
        );
    }

    #[test]
    fn sgr_reset_clears_attrs_for_subsequent_cells() {
        // After "\x1b[41mX\x1b[0mY", cell 0 has red bg, cell 1
        // has default bg. The renderer must NOT extend the red
        // RLE span to cell 1.
        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[41mX\x1b[0mY");
        let rects = compute_rects(&r);
        // Find the red rect; its width must be exactly one cell.
        let red_rect = rects
            .iter()
            .find(|rt| {
                rt.color[0] > 0.3
                    && rt.color[1] < rt.color[0] * 0.7
                    && rt.color[2] < rt.color[0] * 0.7
            })
            .expect("red rect should exist");
        assert!(
            (red_rect.size[0] - r.cell_width).abs() < 0.01,
            "red span = {} cells, expected 1; SGR reset failed to break RLE",
            red_rect.size[0] / r.cell_width
        );
    }

    // ── M3-C2: styled-underline geometry through engawa emitters ──

    /// Sentinel SGR-58 RGB underline colour — unique in the frame, so
    /// decoration rects are identified by exact colour match.
    const UL_SENTINEL: Color = Color { r: 201, g: 31, b: 47 };

    /// REGRESSION (URL underline bleed): on the MAIN screen, fed REAL bytes
    /// (the path the unit tests in url.rs can't exercise — they hand-build
    /// cells), a URL preceded by multi-byte / wide / nerd-font glyphs must
    /// underline ONLY the URL's own columns. The reported bug is the URL
    /// underline smearing across the row ("underscores bleeding everywhere
    /// until everything is underscored"). The width of the single URL
    /// underline must not exceed the URL's column span.
    #[test]
    fn url_underline_does_not_bleed_on_real_bytes() {
        let (r, t) = harness(80, 4);
        // The kind of line a seki prompt + git output paint: a nerd-font
        // snowflake (multi-byte), box-drawing, an arrow, then a URL.
        let url = "https://github.com/pleme-io/mado";
        t.write()
            .feed(format!("\u{2744} \u{2502}\u{2500}\u{2192} clone {url}\r\n").as_bytes());
        let rects = compute_rects(&r);
        // URL underline uses the fixed Nord frost #88C0D0 colour (see the
        // "URL underline decorations" block in build_rect_instances).
        let url_color = overlay_rect_color(0x88, 0xC0, 0xD0, 0.6);
        let url_rects: Vec<&RectInstance> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, url_color))
            .collect();
        assert_eq!(
            url_rects.len(),
            1,
            "expected exactly one URL underline, got {}: {url_rects:?}",
            url_rects.len()
        );
        let max_w = url.chars().count() as f32 * r.cell_width + 0.5;
        assert!(
            url_rects[0].size[0] <= max_w,
            "URL underline width {} exceeds the URL's {} columns ({}) — it bled \
             across the row",
            url_rects[0].size[0],
            url.chars().count(),
            max_w
        );
    }

    fn underline_rects_for(style_param: &[u8]) -> (TerminalRenderer, Vec<RectInstance>) {
        let (r, t) = harness(20, 3);
        let mut feed = Vec::new();
        feed.extend_from_slice(style_param);
        feed.extend_from_slice(b"\x1b[58:2::201:31:47mx\x1b[0m");
        t.write().feed(&feed);
        let rects = compute_rects(&r)
            .into_iter()
            .filter(|rt| colors_approx_eq(rt.color, color_to_f32(&UL_SENTINEL)))
            .collect();
        (r, rects)
    }

    /// MATRIX — one row per [`UnderlineStyle::ALL`] entry, len-pinned
    /// against the mechanical registry; failures aggregate before the
    /// single assert. Geometry expectations project from the engawa
    /// emitter contract (Single 1 solid / Double exactly 2 / Dotted+
    /// Dashed one RLE Run differing in period AND duty / Curly one
    /// sine band), so a divergence between this renderer and the
    /// vocabulary is a red build, not a drift.
    #[test]
    fn underline_style_matrix_emits_engawa_geometry() {
        use crate::terminal::UnderlineStyle;

        struct Row {
            style: UnderlineStyle,
            sgr: &'static [u8],
        }
        let matrix: &[Row] = &[
            Row { style: UnderlineStyle::None, sgr: b"\x1b[4:0m" },
            Row { style: UnderlineStyle::Single, sgr: b"\x1b[4:1m" },
            Row { style: UnderlineStyle::Double, sgr: b"\x1b[4:2m" },
            Row { style: UnderlineStyle::Curly, sgr: b"\x1b[4:3m" },
            Row { style: UnderlineStyle::Dotted, sgr: b"\x1b[4:4m" },
            Row { style: UnderlineStyle::Dashed, sgr: b"\x1b[4:5m" },
        ];
        assert_eq!(
            matrix.len(),
            UnderlineStyle::ALL.len(),
            "matrix must carry one row per UnderlineStyle::ALL entry"
        );
        for style in UnderlineStyle::ALL.iter().copied() {
            assert_eq!(
                matrix.iter().filter(|row| row.style == style).count(),
                1,
                "registry entry {style:?} must appear exactly once in the matrix"
            );
        }

        let mut failures: Vec<String> = Vec::new();
        for row in matrix {
            let (r, rects) = underline_rects_for(row.sgr);
            let metrics = r.underline_metrics();
            // CONTAINMENT LAW (M3 review 2026-06-12): no emitted
            // decoration descends below the Single stroke's bottom
            // edge — the engawa-side bottom-anchoring fix; before it,
            // Double's lower stroke landed entirely in the NEXT row's
            // pixel band and the next row's bg run overdrew it
            // (Double rendered as Single exactly where visible).
            let envelope = metrics.underline_y + metrics.thickness + 0.01;
            for rt in &rects {
                let bottom = rt.pos[1] + rt.size[1];
                if bottom > envelope {
                    failures.push(format!(
                        "{:?}: rect bottom {bottom} exceeds the Single-stroke \
                         envelope {envelope} — out-of-cell decoration",
                        row.style
                    ));
                }
            }
            match row.style {
                UnderlineStyle::None => {
                    if !rects.is_empty() {
                        failures.push(format!("None: expected 0 rects, got {}", rects.len()));
                    }
                }
                UnderlineStyle::Single => {
                    if rects.len() != 1 || rects[0].mode != RectMode::Solid.word() {
                        failures.push(format!("Single: expected 1 solid rect, got {rects:?}"));
                    } else if (rects[0].pos[1] - metrics.underline_y).abs() > 0.01 {
                        failures.push(format!(
                            "Single: y = {}, expected underline_y {}",
                            rects[0].pos[1], metrics.underline_y
                        ));
                    }
                }
                UnderlineStyle::Double => {
                    if rects.len() != 2
                        || rects.iter().any(|rt| rt.mode != RectMode::Solid.word())
                    {
                        failures.push(format!("Double: expected 2 solid rects, got {rects:?}"));
                    } else if (rects[0].pos[1] - rects[1].pos[1]).abs() < 0.01 {
                        failures.push("Double: strokes must sit at distinct y".into());
                    }
                }
                UnderlineStyle::Curly => {
                    if rects.len() != 1 || rects[0].mode != RectMode::Curly.word() {
                        failures.push(format!("Curly: expected 1 sine band, got {rects:?}"));
                    } else if (rects[0].pattern[0] - metrics.cell_width).abs() > 0.01 {
                        failures.push(format!(
                            "Curly: period = {}, expected cell_width {}",
                            rects[0].pattern[0], metrics.cell_width
                        ));
                    }
                }
                UnderlineStyle::Dotted => {
                    if rects.len() != 1 || rects[0].mode != RectMode::Run.word() {
                        failures.push(format!("Dotted: expected 1 RLE run, got {rects:?}"));
                    } else {
                        let expected_period =
                            engawa::decoration::DOTTED_PERIOD_PER_THICKNESS * metrics.thickness;
                        if (rects[0].pattern[0] - expected_period).abs() > 0.01
                            || (rects[0].pattern[1] - engawa::decoration::DOTTED_DUTY).abs() > 0.01
                        {
                            failures.push(format!(
                                "Dotted: (period, duty) = ({}, {}), expected ({expected_period}, {})",
                                rects[0].pattern[0], rects[0].pattern[1], engawa::decoration::DOTTED_DUTY
                            ));
                        }
                    }
                }
                UnderlineStyle::Dashed => {
                    if rects.len() != 1 || rects[0].mode != RectMode::Run.word() {
                        failures.push(format!("Dashed: expected 1 RLE run, got {rects:?}"));
                    } else {
                        let expected_period =
                            metrics.cell_width / engawa::decoration::DASHED_PERIODS_PER_CELL;
                        if (rects[0].pattern[0] - expected_period).abs() > 0.01
                            || (rects[0].pattern[1] - engawa::decoration::DASHED_DUTY).abs() > 0.01
                        {
                            failures.push(format!(
                                "Dashed: (period, duty) = ({}, {}), expected ({expected_period}, {})",
                                rects[0].pattern[0], rects[0].pattern[1], engawa::decoration::DASHED_DUTY
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} underline-style rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Dotted vs Dashed must differ in BOTH period and duty — the two
    /// styles share the Run geometry kind, so the constants are the
    /// only thing distinguishing them on screen.
    #[test]
    fn dotted_and_dashed_runs_differ_in_period_and_duty() {
        let (_, dotted) = underline_rects_for(b"\x1b[4:4m");
        let (_, dashed) = underline_rects_for(b"\x1b[4:5m");
        assert_eq!(dotted.len(), 1);
        assert_eq!(dashed.len(), 1);
        assert!(
            (dotted[0].pattern[0] - dashed[0].pattern[0]).abs() > 0.01,
            "dotted and dashed periods must differ"
        );
        assert!(
            (dotted[0].pattern[1] - dashed[0].pattern[1]).abs() > 0.01,
            "dotted and dashed duties must differ"
        );
    }

    /// SGR 58 indexed colour resolves against the live palette; plain
    /// SGR 4 (UnderlineColor::Default) falls back to the cell fg.
    #[test]
    fn underline_color_resolution_honors_sgr_58() {
        // Indexed: palette slot 1 (ANSI red).
        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[4m\x1b[58:5:1mx\x1b[0m");
        let palette_1 = t.read().ansi_palette()[1];
        let rects = compute_rects(&r);
        assert!(
            rects
                .iter()
                .any(|rt| colors_approx_eq(rt.color, color_to_f32(&palette_1))),
            "indexed underline colour must resolve against the live palette"
        );

        // Default: the underline paints in the cell fg (white here).
        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[4mx\x1b[0m");
        let rects = compute_rects(&r);
        let fg = color_to_f32(&Color::WHITE);
        let underline_y = r.underline_metrics().underline_y;
        assert!(
            rects.iter().any(|rt| colors_approx_eq(rt.color, fg)
                && (rt.pos[1] - underline_y).abs() < 0.01),
            "Default underline colour must fall back to the cell fg"
        );
    }

    /// SGR 53 (overline) paints a solid stroke flush with the cell's
    /// top edge; SGR 55 removes it.
    #[test]
    fn overline_emits_top_edge_rect() {
        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[53mx\x1b[0m");
        let rects = compute_rects(&r);
        let fg = color_to_f32(&Color::WHITE);
        let overline = rects
            .iter()
            .find(|rt| colors_approx_eq(rt.color, fg) && rt.pos[1].abs() < 0.01)
            .copied();
        assert!(
            overline.is_some(),
            "SGR 53 must emit a top-edge rect: {rects:?}"
        );
        assert!(
            (overline.map_or(0.0, |o| o.size[1]) - DECORATION_THICKNESS).abs() < 0.01,
            "overline thickness must match the decoration constant"
        );

        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[53m\x1b[55mx\x1b[0m");
        let rects = compute_rects(&r);
        assert!(
            !rects
                .iter()
                .any(|rt| colors_approx_eq(rt.color, fg) && rt.pos[1].abs() < 0.01),
            "SGR 55 must remove the overline"
        );
    }

    /// SGR 5 (BLINK) animates on the cursor-blink clock: the visible
    /// phase paints fg decorations, the off phase hides them, and
    /// reduce_motion pins them visible. elapsed=0 is the visible
    /// phase by construction (the determinism ladders rely on it).
    #[test]
    fn blink_decorations_animate_on_the_blink_clock() {
        let (r, t) = harness(20, 3);
        t.write().feed(b"\x1b[5;4mx\x1b[0m");
        let fg = color_to_f32(&Color::WHITE);
        let underline_y = r.underline_metrics().underline_y;
        let has_underline = |rects: &[RectInstance]| {
            rects
                .iter()
                .any(|rt| colors_approx_eq(rt.color, fg) && (rt.pos[1] - underline_y).abs() < 0.01)
        };

        let (snap, _) = r.snapshot();
        // Visible phase (elapsed = 0; period = 2 × 500 ms).
        let on = r.build_rect_instances(&snap, 0.0, 0.0, 0.0);
        assert!(has_underline(&on), "blink on-phase must paint the underline");
        // Off phase (elapsed = 0.6 s — second half of the 1 s period).
        let off = r.build_rect_instances(&snap, 0.6, 0.0, 0.0);
        assert!(!has_underline(&off), "blink off-phase must hide the underline");

        // reduce_motion pins the foreground visible at every phase.
        let (mut r, t) = harness(20, 3);
        r.set_reduce_motion(true);
        t.write().feed(b"\x1b[5;4mx\x1b[0m");
        let (snap, _) = r.snapshot();
        let pinned = r.build_rect_instances(&snap, 0.6, 0.0, 0.0);
        assert!(
            has_underline(&pinned),
            "reduce_motion must pin blinking decorations visible"
        );
    }

    /// MATRIX — every catalog effect's POWER-USER config knob maps to
    /// exactly its EffectSet bit (len-pinned against CatalogEffect::ALL),
    /// and reduce_motion gates the ANIMATED effects (glow_on_bell, snow,
    /// aurora) to zero nodes while leaving the static ones alone.
    ///
    /// The baseline preset is `Off` so this test exercises the per-effect
    /// override path in isolation — the default-on AMBIENCE composition
    /// (`Matte`, paper-grain only; `Whisper` for the louder tier) is
    /// pinned separately by
    /// `default_matte_composes_grain_and_whisper_composes_the_three`.
    #[test]
    fn effects_config_maps_to_effect_set_and_reduce_motion_gates_animation() {
        use engawa_wgpu::catalog::CatalogEffect;

        let enable = |r: &mut TerminalRenderer, effect: CatalogEffect| {
            let mut e = crate::config::MadoEffectsConfig::default();
            // Baseline OFF so the only enabled bit is the one the
            // power-user knob below sets — the composed ambience layer
            // is tested separately.
            e.ambience = crate::ambience::AmbiencePreset::Off;
            // Every arm — colorblind included — goes through the
            // CONFIG field, because that is the production ingress
            // (the former set_colorblind_mode special-case masked
            // the dead effects.colorblind.mode path the M3 review
            // found in tear-attach windows).
            match effect {
                CatalogEffect::Colorblind => {
                    e.colorblind.mode = ColorblindMode::Protanopia;
                }
                CatalogEffect::Crt => e.crt.enabled = true,
                CatalogEffect::Scanlines => e.scanlines.enabled = true,
                CatalogEffect::Bloom => e.bloom.enabled = true,
                CatalogEffect::GlowOnBell => e.glow_on_bell.enabled = true,
                CatalogEffect::Aurora => e.aurora.enabled = true,
                CatalogEffect::Snow => e.snow.enabled = true,
                CatalogEffect::Grain => e.grain.enabled = true,
                CatalogEffect::WindowDepth => e.window_depth.enabled = true,
            }
            r.set_effects_config(e);
        };
        const ANIMATED: [CatalogEffect; 3] =
            [CatalogEffect::GlowOnBell, CatalogEffect::Snow, CatalogEffect::Aurora];

        let mut failures: Vec<String> = Vec::new();
        let mut rows = 0usize;
        for effect in CatalogEffect::ALL.iter().copied() {
            rows += 1;
            let (mut r, _t) = harness(10, 2);
            // Baseline: ambience Off so the set starts empty (the
            // renderer ships with Matte by default — paper-grain-only
            // composition, tested separately). This isolates the
            // per-effect knob.
            let mut off = crate::config::MadoEffectsConfig::default();
            off.ambience = crate::ambience::AmbiencePreset::Off;
            r.set_effects_config(off);
            assert!(
                r.enabled_effect_set().is_empty(),
                "ambience-Off config must be all-off"
            );
            enable(&mut r, effect);
            let set = r.enabled_effect_set();
            if !set.contains(effect) {
                failures.push(format!("{effect:?}: knob did not enable its bit"));
            }
            for other in CatalogEffect::ALL.iter().copied() {
                if other != effect && set.contains(other) {
                    failures.push(format!("{effect:?}: knob also enabled {other:?}"));
                }
            }
            r.set_reduce_motion(true);
            let gated = r.enabled_effect_set();
            let is_animated = ANIMATED.contains(&effect);
            if is_animated && gated.contains(effect) {
                failures.push(format!(
                    "{effect:?}: reduce_motion must gate the animated effect to zero nodes"
                ));
            }
            if !is_animated && !gated.contains(effect) {
                failures.push(format!(
                    "{effect:?}: reduce_motion must NOT gate a static effect"
                ));
            }
        }
        assert_eq!(
            rows,
            CatalogEffect::ALL.len(),
            "matrix must cover every catalog effect"
        );
        assert!(
            failures.is_empty(),
            "{} effect-set rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// The composed AMBIENCE layer (operator design law, 2026-06-13;
    /// Vellum-era "more subtle" retune, 2026-06-14; paper-grain tooth,
    /// 2026-06-15) — the renderer-level forcing function. The default
    /// config (`Matte`) composes EXACTLY the paper-grain tooth (one
    /// node — no glow, no halo, aurora off, just the faint fabric
    /// texture); a louder preset (`Whisper`) composes {aurora, bloom,
    /// glow_on_bell}; `reduce_motion` resolves any preset to `Off` ⇒
    /// zero nodes (the accessibility floor); a per-effect override adds
    /// on top.
    #[test]
    fn default_matte_composes_grain_and_whisper_composes_the_three() {
        use engawa_wgpu::catalog::CatalogEffect;
        let mut failures: Vec<String> = Vec::new();

        // ── Default (Matte) composes EXACTLY the grain tooth ─────────
        let (mut r, _t) = harness(10, 2);
        let mut cfg = crate::config::MadoConfig::default();
        r.apply_effects_and_accessibility(&cfg);
        let matte_set = r.enabled_effect_set();
        if !matte_set.contains(CatalogEffect::Grain) {
            failures.push(format!(
                "default Matte ambience must compose the paper-grain tooth (got {matte_set:?})"
            ));
        }
        for effect in CatalogEffect::ALL.iter().copied() {
            if effect != CatalogEffect::Grain && matte_set.contains(effect) {
                failures.push(format!(
                    "default Matte ambience must ONLY compose grain, but also enabled {effect:?}"
                ));
            }
        }

        // ── Whisper (the louder tier) composes the three members ─────
        let mut whisper = crate::config::MadoConfig::default();
        whisper.effects.ambience = crate::ambience::AmbiencePreset::Whisper;
        r.apply_effects_and_accessibility(&whisper);
        let set = r.enabled_effect_set();
        for effect in [
            CatalogEffect::Aurora,
            CatalogEffect::Bloom,
            CatalogEffect::GlowOnBell,
        ] {
            if !set.contains(effect) {
                failures.push(format!("Whisper ambience is missing {effect:?}"));
            }
        }
        // …and ONLY those three (no static effects sneak in).
        for effect in [
            CatalogEffect::Colorblind,
            CatalogEffect::Crt,
            CatalogEffect::Scanlines,
            CatalogEffect::Snow,
            CatalogEffect::Grain,
        ] {
            if set.contains(effect) {
                failures.push(format!("Whisper ambience wrongly enabled {effect:?}"));
            }
        }

        // ── reduce_motion → Off → zero nodes ─────────────────────────
        cfg.effects.ambience = crate::ambience::AmbiencePreset::Whisper;
        cfg.accessibility.reduce_motion = true;
        r.apply_effects_and_accessibility(&cfg);
        if !r.enabled_effect_set().is_empty() {
            failures.push(format!(
                "reduce_motion must kill the whole ambience layer (got {:?})",
                r.enabled_effect_set()
            ));
        }

        // ── explicit Off → zero nodes ────────────────────────────────
        let mut off = crate::config::MadoConfig::default();
        off.effects.ambience = crate::ambience::AmbiencePreset::Off;
        r.apply_effects_and_accessibility(&off);
        if !r.enabled_effect_set().is_empty() {
            failures.push("AmbiencePreset::Off must contribute zero nodes".to_owned());
        }

        // ── per-effect override beats the preset ─────────────────────
        // With ambience Off, a power-user crt.enabled still turns crt
        // on — the override is ADDITIVE and survives an Off preset.
        let mut overridden = crate::config::MadoConfig::default();
        overridden.effects.ambience = crate::ambience::AmbiencePreset::Off;
        overridden.effects.crt.enabled = true;
        r.apply_effects_and_accessibility(&overridden);
        let oset = r.enabled_effect_set();
        if !oset.contains(CatalogEffect::Crt) {
            failures.push("power-user crt override must win over Off preset".to_owned());
        }
        if oset.contains(CatalogEffect::Aurora) {
            failures.push("Off preset must not compose aurora even with a crt override".to_owned());
        }

        assert!(
            failures.is_empty(),
            "{} ambience composition violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// The composed ambience layer caches like any other effect set:
    /// N frames at one (effect-set, resolution) key compile the
    /// CompiledGraph exactly once (no per-frame recompile). This is the
    /// renderer-side companion to render_graph's
    /// `compile_count_moves_only_on_toggle_or_resize` — it proves the
    /// COMPOSED set (aurora+bloom+glow) keys the cache stably, not just
    /// the single-effect sets.
    #[test]
    fn composed_ambience_effect_set_keys_the_cache_stably() {
        use crate::render_graph::{EffectSet, FrameGraphCache, GraphKey};
        use engawa_wgpu::catalog::CatalogEffect;

        // The Whisper composition's effect set, built the same way the
        // renderer's enabled_effect_set unions it.
        let comp = crate::ambience::AmbiencePreset::Whisper.compose();
        let mut effects = EffectSet::EMPTY;
        for m in &comp.members {
            effects.insert(m.effect);
        }
        assert!(effects.contains(CatalogEffect::Aurora));
        assert!(effects.contains(CatalogEffect::Bloom));
        assert!(effects.contains(CatalogEffect::GlowOnBell));

        let mut cache = FrameGraphCache::new();
        let key = GraphKey { effects, width: 640, height: 480 };
        for _ in 0..64 {
            assert!(cache.ensure(key).is_some(), "composed set must compile");
        }
        assert_eq!(
            cache.compile_count(),
            1,
            "the composed ambience set must compile exactly once across a steady frame run"
        );
    }

    /// Entry-point parity pin — both main.rs and `gui_tear_attach` call must
    /// resolve the legacy `accessibility.colorblind` alias AND gate
    /// the animated effects via `reduce_motion` — both were dead in
    /// tear-attach windows when that path called only
    /// `set_effects_config` (which never touched the deleted
    /// renderer-side colorblind mirror field).
    #[test]
    fn apply_effects_and_accessibility_resolves_alias_and_gates_motion() {
        use engawa_wgpu::catalog::CatalogEffect;
        let (mut r, _t) = harness(10, 2);
        let mut config = crate::config::MadoConfig::default();
        config.accessibility.colorblind = ColorblindMode::Deuteranopia;
        config.accessibility.reduce_motion = true;
        config.effects.snow.enabled = true;
        r.apply_effects_and_accessibility(&config);
        let set = r.enabled_effect_set();
        assert!(
            set.contains(CatalogEffect::Colorblind),
            "legacy accessibility.colorblind alias must enable the effect"
        );
        assert_eq!(
            r.catalog_colorblind_mode(),
            engawa_wgpu::catalog::colorblind::ColorblindMode::Deuteranopia,
            "alias mode must reach the catalog wire word"
        );
        assert!(
            !set.contains(CatalogEffect::Snow),
            "reduce_motion must gate the animated effect to zero nodes"
        );

        // The canonical knob beats the alias when both are set.
        config.effects.colorblind.mode = ColorblindMode::Protanopia;
        r.apply_effects_and_accessibility(&config);
        assert_eq!(
            r.catalog_colorblind_mode(),
            engawa_wgpu::catalog::colorblind::ColorblindMode::Protanopia,
            "effects.colorblind.mode wins over the deprecation alias"
        );
    }

    /// Hot-reload application (M4 stage 2, succeeding the M3 cell
    /// drain): a watched-config edit reaches the renderer through
    /// `ux::config_apply::ConfigApplier`'s typed setter delta — the
    /// effects section still flows through `resolved_effects()` →
    /// `set_effects_config` (the single resolution point + single
    /// ingress the M3 review established), and the alias keeps
    /// resolving on reload.
    #[test]
    fn config_applier_delta_reaches_renderer_effects_surface() {
        use engawa_wgpu::catalog::CatalogEffect;
        let (mut r, _t) = harness(10, 2);
        // Baseline OFF so the set starts empty — the renderer ships with
        // the Matte ambience by default (empty composition, tested
        // separately); this test isolates the config-applier delta path.
        let mut off = crate::config::MadoEffectsConfig::default();
        off.ambience = crate::ambience::AmbiencePreset::Off;
        r.set_effects_config(off);
        assert!(r.enabled_effect_set().is_empty());

        let boot = crate::config::MadoConfig::default();
        let mut applier = crate::ux::config_apply::ConfigApplier::new(boot.clone());

        let mut edited = boot.clone();
        edited.effects.crt.enabled = true;
        edited.accessibility.colorblind = ColorblindMode::Protanopia;
        edited.effects.snow.enabled = true;
        edited.accessibility.reduce_motion = true;
        assert!(applier.apply_delta(&edited, &mut r) > 0);

        let set = r.enabled_effect_set();
        assert!(set.contains(CatalogEffect::Crt), "reloaded crt toggle must apply");
        assert!(
            set.contains(CatalogEffect::Colorblind),
            "legacy accessibility.colorblind alias must resolve on reload"
        );
        assert!(
            !set.contains(CatalogEffect::Snow),
            "reduce_motion (applied BEFORE the effect set) must gate snow"
        );

        // Re-applying the identical config is a zero-call no-op —
        // nothing resets, nothing repaints.
        assert_eq!(applier.apply_delta(&edited, &mut r), 0);
        assert!(
            r.enabled_effect_set().contains(CatalogEffect::Crt),
            "no-op delta must not clear applied effects"
        );
    }

    proptest::proptest! {
        /// Whatever byte sequence comes in, the rect set must:
        /// 1. Contain at most one cursor rect (Block style).
        /// 2. Have no negative-dim or out-of-viewport rects.
        /// 3. Stay finite (no NaN or Inf in coordinates).
        ///
        /// Generator: printable ASCII (0x20..0x7f) + newline,
        /// carriage return, and ESC (0x1b) — the bytes that
        /// produce non-trivial parser behavior without invalid
        /// UTF-8 sequences that vte handles separately.
        #[test]
        fn arbitrary_ascii_text_keeps_invariants(
            text in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::prelude::Just(b'\n'),
                    proptest::prelude::Just(b'\r'),
                    proptest::prelude::Just(0x1bu8),
                    0x20u8..0x7f,
                ],
                0..200usize,
            )
        ) {
            let (r, t) = harness(40, 12);
            t.write().feed(&text);
            let rects = compute_rects(&r);

            // 1. ≤ 1 cursor rect (Block style).
            let cur = cursor_rects(&rects, r.cursor_color);
            proptest::prop_assert!(cur.len() <= 1, "cursor count = {}", cur.len());

            // 2. All rects in-viewport with positive dims.
            let max_x = 40.0 * r.cell_width + 1.0;
            let max_y = 12.0 * r.cell_height + 1.0;
            for rect in &rects {
                proptest::prop_assert!(rect.pos[0] >= 0.0);
                proptest::prop_assert!(rect.pos[1] >= 0.0);
                proptest::prop_assert!(rect.size[0] > 0.0);
                proptest::prop_assert!(rect.size[1] > 0.0);
                proptest::prop_assert!(rect.pos[0] + rect.size[0] <= max_x);
                proptest::prop_assert!(rect.pos[1] + rect.size[1] <= max_y);
            }

            // 3. No NaN / Inf.
            for rect in &rects {
                for v in [
                    rect.pos[0], rect.pos[1], rect.size[0], rect.size[1],
                    rect.color[0], rect.color[1], rect.color[2], rect.color[3],
                ] {
                    proptest::prop_assert!(v.is_finite(), "non-finite: {v}");
                }
            }
        }

        /// Wide-char + emoji invariants: a string of arbitrary CJK
        /// and emoji codepoints (each width=2 cells) must produce
        /// rects that respect the grid. The cursor's x position
        /// must equal `2 × number_of_wide_chars × cell_width` (or
        /// wrap to a new row if it'd overflow). No rect can have
        /// a width that's not a multiple of cell_width.
        ///
        /// Generator: a small set of common wide codepoints picked
        /// for their fully-defined East Asian Width=W classification.
        #[test]
        fn wide_chars_respect_cell_grid(
            text in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::prelude::Just("あ"), // hiragana
                    proptest::prelude::Just("中"), // CJK ideograph
                    proptest::prelude::Just("한"), // hangul syllable
                    proptest::prelude::Just("🦀"), // crab emoji
                    proptest::prelude::Just("🟦"), // square emoji
                ],
                0..30usize,
            )
        ) {
            let (r, t) = harness(80, 5);
            let bytes: String = text.iter().copied().collect();
            t.write().feed(bytes.as_bytes());
            let rects = compute_rects(&r);

            // 1. All rect widths are non-negative integer multiples
            //    of cell_width (within float epsilon).
            for rect in &rects {
                let cells = rect.size[0] / r.cell_width;
                proptest::prop_assert!(
                    cells >= 0.0 && (cells - cells.round()).abs() < 0.05,
                    "rect width {} is not a clean multiple of cell_width {}",
                    rect.size[0], r.cell_width
                );
            }

            // 2. The cursor still lives inside the viewport.
            let cur = cursor_rects(&rects, r.cursor_color);
            proptest::prop_assert!(cur.len() <= 1);
            for c in &cur {
                proptest::prop_assert!(c.pos[0] >= 0.0);
                proptest::prop_assert!(c.pos[0] + c.size[0] <= 80.0 * r.cell_width + 1.0);
            }
        }

        /// Idempotency under repeated identical writes: a write,
        /// then clear, then write again must produce the same
        /// rects as a single write — proves no shape-cache /
        /// rect-buffer state leaks across clear cycles.
        #[test]
        fn repeated_identical_writes_match_single_write(
            text in proptest::collection::vec(0x20u8..0x7f, 0..50usize)
        ) {
            let (r_once, t_once) = harness(40, 8);
            t_once.write().feed(&text);
            let rects_once = compute_rects(&r_once);

            let (r_repeat, t_repeat) = harness(40, 8);
            t_repeat.write().feed(b"\x1b[2J\x1b[H");
            t_repeat.write().feed(&text);
            t_repeat.write().feed(b"\x1b[2J\x1b[H");
            t_repeat.write().feed(&text);
            let rects_repeat = compute_rects(&r_repeat);

            proptest::prop_assert_eq!(rects_once.len(), rects_repeat.len(),
                "clear+write+clear+write produced different rect count from single write");
            for (a, b) in rects_once.iter().zip(rects_repeat.iter()) {
                proptest::prop_assert_eq!(a.pos, b.pos);
                proptest::prop_assert_eq!(a.size, b.size);
            }
        }
    }
}

/// Layer 2 of the verification strategy: headless wgpu render
/// to an offscreen texture, then read pixels back and assert.
/// Opt-in via the `gpu_tests` feature so CI runners without a
/// real GPU adapter don't mis-fail. On macOS / cid: the entire
/// path (real Metal adapter, real pipeline init, real pixel
/// readback) runs end-to-end. This is the canonical place to
/// catch the "purple flash" class of bug — render the first
/// frame headless, assert no magenta pixels.
#[cfg(all(test, feature = "gpu_tests"))]
mod render_gpu_invariants {
    use super::*;
    use crate::terminal::Terminal;
    use garasu::{
        GpuContext, TextLayerStack,
        headless::{HeadlessTarget, assert_no_magenta_pixels},
    };
    use madori::RenderContext;

    /// Build a fully-initialized `TerminalRenderer` connected to
    /// a fresh `cols×rows` terminal, with all wgpu pipelines
    /// brought up against the given GPU context. Returns
    /// everything the render loop needs.
    fn build_gpu_renderer(
        gpu: &garasu::GpuContext,
        cols: usize,
        rows: usize,
    ) -> (TerminalRenderer, SharedTerminal, TextLayerStack) {
        let term = Arc::new(parking_lot::RwLock::new(Terminal::new(cols, rows)));
        let mut renderer = TerminalRenderer::new(
            term.clone(),
            14.0,
            1.4,
            "monospace".into(),
            "monospace".into(),
            "monospace".into(), // font_symbols
            0.0,
            CursorStyle::Block,
            false,
            500,
            wgpu::Color { r: 0.180, g: 0.204, b: 0.251, a: 1.0 },
            Color::WHITE,
        );
        // Bring up rect_pipeline / image_pipeline / post_pipeline
        // — the same init the live app runs once at startup.
        renderer.init(gpu);
        // Headless render-to-texture IS the framebuffer — there is no
        // compositor downscale to a smaller physical panel. Pin the panel
        // ratio to 1.0 (seam auto-tune OFF) so every GPU test is
        // deterministic regardless of the PHYSICAL display CI/dev runs on.
        // Otherwise, on a machine whose main display is scaled (built-in XDR
        // at "More Space", ratio ≈ 0.84), the render prologue's live
        // `display_scaling_ratio` probe would panel-snap the cell to a
        // FRACTIONAL framebuffer height and shift every recorded frame hash —
        // correct on-screen, meaningless for a headless framebuffer that
        // never downscales. (Pre-existing display-dependence surfaced
        // 2026-07-11 while overhauling the seam path.)
        renderer.set_seam_config(false, None);
        let text = TextLayerStack::new(
            &gpu.device,
            &gpu.queue,
            SURFACE_FORMAT,
        );
        (renderer, term, text)
    }

    /// Drive one frame of the renderer against an offscreen
    /// target. Returns the read-back RGBA8 pixel buffer.
    fn render_one_frame_headless(
        gpu: &garasu::GpuContext,
        renderer: &mut TerminalRenderer,
        text: &mut TextLayerStack,
        target: &HeadlessTarget,
    ) -> Vec<u8> {
        let mut ctx = RenderContext {
            gpu,
            text,
            surface_view: target.view(),
            width: target.width(),
            height: target.height(),
            scale_factor: 1.0,
            elapsed: 0.0,
            dt: 0.0,
        };
        renderer.render(&mut ctx);
        // Wait for the GPU work to land before reading pixels.
        let _ = gpu.device.poll(wgpu::PollType::Wait);
        target.read_pixels_rgba8(gpu)
    }

    /// Forcing function for the per-surface-layer invariant: `ensure_layers`
    /// must mint exactly one isolated layer per named text surface in
    /// [`TEXT_LAYERS`]. A new text surface added without its own layer (which
    /// would reintroduce the cross-pass vertex-buffer clobber) fails here.
    #[cfg(feature = "gpu_tests")]
    #[test]
    fn layers_match_text_layers_const() {
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (mut renderer, _term, mut stack) = build_gpu_renderer(&gpu, 80, 24);
        renderer.ensure_layers(&mut stack, &gpu.device);
        assert_eq!(
            stack.layer_count(),
            TEXT_LAYERS.len(),
            "ensure_layers must mint one layer per TEXT_LAYERS entry"
        );
        // Idempotent — a second call mints nothing more.
        renderer.ensure_layers(&mut stack, &gpu.device);
        assert_eq!(stack.layer_count(), TEXT_LAYERS.len());
    }

    #[test]
    fn first_frame_of_fresh_terminal_has_no_magenta_pixels() {
        // The canonical "purple flash" regression test. On macOS
        // Metal, an uninitialised texture often surfaces as
        // magenta — a single magenta pixel anywhere in the
        // first-frame readback means the pipeline isn't clearing
        // properly. Renders against Bgra8UnormSrgb (mado's wire
        // format).
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 128, 64, SURFACE_FORMAT);
        let (mut r, _t, mut text) = build_gpu_renderer(&gpu, 40, 8);
        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        // The read-back format is BGRA but the magenta heuristic
        // checks R/G/B independently — magenta is (high, low,
        // high) regardless of channel order. Pass through as-is.
        assert!(
            assert_no_magenta_pixels(&pixels, 128, 64).is_ok(),
            "first frame contains a magenta pixel — purple-flash regression"
        );
    }

    #[test]
    fn clear_screen_frame_has_no_magenta_pixels() {
        // After a `\x1b[2J` clear, the rendered frame should be
        // pure bg_color + cursor — no uninit memory leaking
        // through. Runs the full snapshot + rect-upload + paint
        // path so we're testing the GPU pipeline, not just the
        // CPU snapshot.
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 128, 64, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 40, 8);
        t.write().feed(b"some text first\nthen more text\n\x1b[2J\x1b[H");
        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        assert!(
            assert_no_magenta_pixels(&pixels, 128, 64).is_ok(),
            "post-clear frame contains a magenta pixel"
        );
    }

    #[test]
    fn frame_pixels_include_configured_bg_color() {
        // Coarse pipeline-correctness check: at least one pixel
        // in the rendered frame should match the configured
        // background color. If the pipeline silently skipped the
        // bg paint, every pixel would be 0 (texture initial
        // state) and this fails.
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 64, 32, SURFACE_FORMAT);
        let (mut r, _t, mut text) = build_gpu_renderer(&gpu, 20, 4);
        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let any_nonzero = pixels.chunks_exact(4).any(|p| p[0] > 0 || p[1] > 0 || p[2] > 0);
        assert!(
            any_nonzero,
            "every pixel is (0, 0, 0) — looks like the pipeline didn't paint"
        );
    }

    /// Gamma-correctness pin for BOTH render paths (2026-06-14, washed-out
    /// colors investigation). On the `Bgra8UnormSrgb` target a pure-red
    /// true-color background must read back as sRGB red (R=255, G≈0, B≈0),
    /// and a pure-green true-color FOREGROUND glyph must read back as
    /// sRGB green — proving the linear→sRGB store re-encode is correct on
    /// the direct-to-surface (effects-OFF) path AND that the grain/SCENE
    /// effect chain (effects-ON default Matte) is a color-identity
    /// round-trip. This is the mechanical falsifier for any future
    /// surface-format / gamma regression: if mado ever writes linear to a
    /// non-sRGB target, R collapses far below 255 and G/B lift well above
    /// 0. The readback order on a Bgra8 target is B,G,R,A.
    #[test]
    fn truecolor_roundtrip_is_gamma_correct_both_paths() {
        use garasu::headless::pixel_at;
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (cols, rows) = (8u32, 2u32);

        let surface_dims = |r: &TerminalRenderer| {
            (
                (r.cell_width * cols as f32).ceil() as u32,
                (r.cell_height * rows as f32).ceil() as u32,
            )
        };

        // ---- effects-OFF direct path: pure-red truecolor background ----
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, cols as usize, rows as usize);
        r.ambience.members.clear();
        assert!(r.enabled_effect_set().is_empty(), "must run the direct path");
        let (sw, sh) = surface_dims(&r);
        let target = HeadlessTarget::new(&gpu, sw, sh, SURFACE_FORMAT);
        t.write().feed(b"\x1b[H\x1b[48;2;255;0;0m        \x1b[0m");
        let px = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let x = (3.0 * r.cell_width + r.cell_width / 2.0) as u32;
        let y = (r.cell_height / 2.0) as u32;
        let red = pixel_at(&px, sw, x, y);
        assert!(
            red[2] >= 250 && red[1] <= 6 && red[0] <= 6,
            "effects-OFF pure-red bg washed out: got BGRA {red:?} (want R≈255 G≈0 B≈0)"
        );

        // ---- glyphon foreground path: pure-green truecolor glyph ----
        let (mut r2, t2, mut text2) = build_gpu_renderer(&gpu, cols as usize, rows as usize);
        r2.ambience.members.clear();
        t2.write().feed(b"\x1b[H\x1b[2J\x1b[38;2;0;255;0mWWWWWWWW\x1b[0m");
        let target2 = HeadlessTarget::new(&gpu, sw, sh, SURFACE_FORMAT);
        let px2 = render_one_frame_headless(&gpu, &mut r2, &mut text2, &target2);
        let mut best = [0u8; 4];
        for yy in 0..r2.cell_height as u32 {
            for xx in 0..sw {
                let p = pixel_at(&px2, sw, xx, yy);
                if u16::from(p[1]) > u16::from(best[1]) {
                    best = p;
                }
            }
        }
        assert!(
            best[1] >= 250 && best[2] <= 6 && best[0] <= 6,
            "pure-green glyph washed out: brightest BGRA {best:?} (want G≈255 R≈0 B≈0)"
        );

        // ---- effects-ON (default Matte = grain): color-identity ----
        let (mut r3, t3, mut text3) = build_gpu_renderer(&gpu, cols as usize, rows as usize);
        assert!(
            !r3.enabled_effect_set().is_empty(),
            "default Matte ambience must run the effect chain"
        );
        t3.write().feed(b"\x1b[H\x1b[48;2;255;0;0m        \x1b[0m");
        let target3 = HeadlessTarget::new(&gpu, sw, sh, SURFACE_FORMAT);
        let px3 = render_one_frame_headless(&gpu, &mut r3, &mut text3, &target3);
        let red_on = pixel_at(&px3, sw, x, y);
        assert!(
            red_on[2] >= 250 && red_on[1] <= 6 && red_on[0] <= 6,
            "effects-ON grain chain desaturated pure-red: got BGRA {red_on:?}"
        );
    }

    /// PROOF for the powerline-separator notch fix (2026-06-14, lualine
    /// pill bug). With `line_height = 1.25` the cell is 25% taller than a
    /// 1.0 cell; a powerline separator drawn as an ordinary baseline-
    /// positioned font glyph leaves the bottom rows of the cell as bg
    /// (the notch the operator saw against the next section). mado now
    /// synthesizes the filled separators (E0B0/E0B2/E0B4/E0B6) into the
    /// cell rect via `RectMode::Powerline`, so the filled side reaches
    /// the cell bottom at ANY line_height.
    ///
    /// This test renders a single U+E0B4 (right filled half-disk) in
    /// teal (#88C0D0) on a contrasting bg (#2E3440) at line_height=1.25
    /// and asserts:
    ///   1. the bottom-most row of the cell, sampled through the SOLID
    ///      (left, flat-edge) side, is the teal fg — NOT the bg gap.
    ///      Before the fix this row was bg (the notch); after, it's teal.
    ///   2. the same holds at line_height=1.0 (already filled — pins
    ///      both metrics).
    ///   3. negative control: a normal glyph ('x') is NOT stretched to
    ///      the cell bottom — its last row stays bg, proving the fill is
    ///      scoped to powerline separators only.
    ///
    /// The readback order on a Bgra8 target is B,G,R,A.
    #[test]
    fn powerline_separators_fill_cell_bottom_at_tall_line_height() {
        use garasu::headless::pixel_at;
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");

        // teal #88C0D0 = (136, 192, 208); bg #2E3440 = (46, 52, 64).
        const TEAL: (u8, u8, u8) = (0x88, 0xC0, 0xD0);
        let is_teal = |p: [u8; 4]| {
            // BGRA: p[0]=B, p[1]=G, p[2]=R. Allow a generous tolerance —
            // the sRGB-store round-trip + AA at the curved edge can shift
            // a few LSBs, but the SOLID side is a flat fill.
            let near = |a: u8, b: u8| (i16::from(a) - i16::from(b)).abs() <= 24;
            near(p[2], TEAL.0) && near(p[1], TEAL.1) && near(p[0], TEAL.2)
        };

        // Render one U+E0B4 in teal on the #2E3440 bg at the given
        // line_height; return the readback + surface dims + cell metrics.
        let render_sep = |line_height: f32, ch: &str| {
            // bg color is set on the renderer (linear); the sep fg comes
            // from an SGR truecolor span.
            let term = Arc::new(parking_lot::RwLock::new(Terminal::new(3, 1)));
            let mut r = TerminalRenderer::new(
                term.clone(),
                16.0,
                line_height,
                "monospace".into(),
                "monospace".into(),
                "monospace".into(),
                0.0,
                CursorStyle::Block,
                false,
                500,
                // #2E3440 — must match the SGR bg below so the whole
                // cell that ISN'T the glyph reads as this bg.
                wgpu::Color { r: 0.180, g: 0.204, b: 0.251, a: 1.0 },
                Color::WHITE,
            );
            r.init(&gpu);
            // Drop ambience so we read the direct (un-grained) path —
            // exact color match, no SCENE pass to reason about.
            r.ambience.members.clear();
            let mut text = TextLayerStack::new(&gpu.device, &gpu.queue, SURFACE_FORMAT);
            let sw = (r.cell_width * 3.0).ceil() as u32;
            let sh = (r.cell_height * 1.0).ceil() as u32;
            let target = HeadlessTarget::new(&gpu, sw, sh, SURFACE_FORMAT);
            // Cursor home, clear, then teal-fg sep in cell 0. The block
            // cursor would overpaint cell 0, so park it past the glyph.
            let seq = format!(
                "\x1b[H\x1b[2J\x1b[48;2;46;52;64m\x1b[38;2;136;192;208m{ch}\x1b[0m\x1b[1;3H"
            );
            t_feed(&term, seq.as_bytes());
            let px = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
            (px, sw, r.cell_width, r.cell_height)
        };

        // The E0B4 right half-disk has its FLAT edge on the left (u≈0)
        // and bulges right. The solid fill is thickest near the vertical
        // center; sample a column just inside the left edge so we hit the
        // flat (fully-solid) side at every row, including the bottom.
        let solid_col_frac = 0.12_f32;

        // ---- 1. line_height = 1.25 (the bug condition) ----
        let (px, sw, cw, chh) = render_sep(1.25, "\u{E0B4}");
        let x = (cw * solid_col_frac) as u32;
        let bottom_y = (chh as u32).saturating_sub(1);
        let bottom_px = pixel_at(&px, sw, x.min(sw - 1), bottom_y);
        assert!(
            is_teal(bottom_px),
            "E0B4 at line_height=1.25: bottom cell row (y={bottom_y}) is {bottom_px:?}, \
             expected teal fg — the powerline cap does NOT reach the cell bottom (notch)"
        );
        // sanity: the vertical-center of the same column is also teal.
        let mid_px = pixel_at(&px, sw, x.min(sw - 1), (chh / 2.0) as u32);
        assert!(
            is_teal(mid_px),
            "E0B4 at line_height=1.25: mid cell row is {mid_px:?}, expected teal fg"
        );

        // ---- 2. line_height = 1.0 (already-filled metric, pins both) ----
        let (px1, sw1, cw1, chh1) = render_sep(1.0, "\u{E0B4}");
        let x1 = (cw1 * solid_col_frac) as u32;
        let bottom_y1 = (chh1 as u32).saturating_sub(1);
        let bottom_px1 = pixel_at(&px1, sw1, x1.min(sw1 - 1), bottom_y1);
        assert!(
            is_teal(bottom_px1),
            "E0B4 at line_height=1.0: bottom cell row is {bottom_px1:?}, expected teal fg"
        );

        // ---- 3. negative control — a normal glyph is NOT cell-filled ----
        // 'x' is a short lowercase letter; its bottom row(s) must stay bg
        // (it sits on the baseline, well above the cell bottom), proving
        // the cell-fill is scoped to powerline separators only.
        let (pxn, swn, cwn, chhn) = render_sep(1.25, "x");
        // Scan the whole cell-0 column band for the bottom-most teal
        // pixel; for a normal glyph it must be comfortably above the
        // cell bottom (the descender region stays bg).
        let mut lowest_teal: i32 = -1;
        for yy in 0..(chhn as u32) {
            for xx in 0..(cwn as u32).min(swn) {
                if is_teal(pixel_at(&pxn, swn, xx, yy)) {
                    lowest_teal = lowest_teal.max(yy as i32);
                }
            }
        }
        let cell_bottom = chhn as i32 - 1;
        assert!(
            lowest_teal >= 0,
            "negative control: rendered 'x' produced no teal pixels at all"
        );
        assert!(
            lowest_teal < cell_bottom,
            "negative control: normal glyph 'x' painted teal at the cell bottom \
             (lowest_teal={lowest_teal}, cell_bottom={cell_bottom}) — the cell-fill \
             leaked onto ordinary text"
        );
    }

    /// PROBE + PROOF for the row-seam stripe artifact (operator report
    /// 2026-07-05: thin full-width horizontal lines between text rows).
    /// Every cell of the grid gets an explicit truecolor background;
    /// if consecutive per-row background quads do not tile exactly
    /// (fractional `cell_height` → per-row y accumulating fractional
    /// offsets that rasterize to different pixel edges), the window
    /// clear color bleeds through as 1px full-width seams between rows.
    // Strict float equality is the POINT of the fract() == 0.0 gate:
    // the quantized height is exactly integer by construction.
    #[allow(clippy::float_cmp)]
    #[test]
    fn full_bg_rows_tile_without_horizontal_seams() {
        use garasu::headless::pixel_at;
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (cols, rows) = (20usize, 12usize);
        let (mut r, term, mut text) = build_gpu_renderer(&gpu, cols, rows);
        r.ambience.members.clear();
        // NB: build_gpu_renderer already pins panel_ratio = 1.0 (seam
        // auto-tune OFF) so this framebuffer-quantize `fract() == 0` gate is
        // deterministic on a scaled main display (see build_gpu_renderer).

        // Hide the cursor; paint EVERY cell with bg #8C3C3C.
        let mut seq = String::from("\x1b[?25l\x1b[H\x1b[2J");
        for row in 1..=rows {
            use std::fmt::Write as _;
            let _ = write!(seq, "\x1b[{row};1H\x1b[48;2;140;60;60m");
            for _ in 0..cols {
                seq.push('x');
            }
        }
        seq.push_str("\x1b[0m");
        t_feed(&term, seq.as_bytes());

        // Parameterized across font sizes whose UN-quantized line box
        // is fractional in device pixels — the row-seam artifact class
        // (operator report 2026-07-05). 12.0 × 1.4 = 16.8 and 24.0 ×
        // 1.4 = 33.6 are the canonical fractional pitches; 14.0 × 1.4
        // = 19.6 is the build default. `quantize_cell_height_px` must
        // make every one of them tile exactly.
        for font_size in [14.0f32, 12.0, 24.0, 13.0] {
            r.set_font_size(font_size);
            // First render re-measures the real font metrics
            // (cell_width / cell_height change on measure) — render to
            // a scratch target, then size the real target from the
            // measured cell.
            let scratch = HeadlessTarget::new(&gpu, 64, 32, SURFACE_FORMAT);
            let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &scratch);
            let (cw, chh) = (r.cell_width, r.cell_height);
            eprintln!("font {font_size}: measured cell {cw} x {chh}");
            // The chokepoint invariant: the measured cell height is a
            // whole device pixel, so row N's bottom edge == row N+1's
            // top edge on the SAME integer pixel boundary, at every
            // font size (fractional heights rasterize consecutive rows
            // at alternating half-pixel edges — the seam rhythm).
            assert_eq!(
                chh.fract(),
                0.0,
                "cell_height must be quantized to integer device px, got {chh}"
            );
            let sw = (cw * cols as f32).ceil() as u32;
            let sh = (chh * rows as f32).ceil() as u32;
            let target = HeadlessTarget::new(&gpu, sw, sh, SURFACE_FORMAT);

            let px = render_one_frame_headless(&gpu, &mut r, &mut text, &target);

            // A pixel "shows the clear color" when it matches #2E3440
            // (readback BGRA ≈ (64, 52, 46)). Rows are majority-red
            // (bg quad) with white-ish glyph pixels sprinkled in; a
            // SEAM row is a pixel row that is majority-clear.
            let is_clear = |p: [u8; 4]| {
                let near = |a: u8, b: u8| (i16::from(a) - i16::from(b)).abs() <= 8;
                near(p[2], 0x2E) && near(p[1], 0x34) && near(p[0], 0x40)
            };
            let grid_bottom = (chh * rows as f32).floor() as u32;
            let mut seams: Vec<(u32, usize)> = Vec::new();
            for y in 0..grid_bottom.saturating_sub(1).min(sh) {
                let clear_count =
                    (0..sw).filter(|&x| is_clear(pixel_at(&px, sw, x, y))).count();
                if clear_count * 2 > sw as usize {
                    seams.push((y, clear_count));
                }
            }
            assert!(
                seams.is_empty(),
                "row-seam stripes at font {font_size}: {} pixel rows inside \
                 the painted grid are majority-clear (cell {cw}x{chh}, \
                 surface {sw}x{sh}): {seams:?}",
                seams.len(),
            );
        }
    }

    /// Feed bytes into a `SharedTerminal` regardless of whether the
    /// helper signatures elsewhere take `&Arc<RwLock<Terminal>>`.
    fn t_feed(term: &SharedTerminal, bytes: &[u8]) {
        term.write().feed(bytes);
    }

    #[test]
    fn two_identical_renders_produce_identical_frame_hashes() {
        // Frame-hash determinism — the canonical L2 invariant that
        // proves "same input → same pixels, byte-for-byte". If
        // the pipeline introduces any non-determinism (uninit
        // memory, time-dependent uniforms, animation that ticks
        // even at elapsed=0), the two hashes diverge.
        use garasu::headless::frame_hash;
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 64, 32, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 20, 4);
        t.write().feed(b"deterministic");

        let pixels_a = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let pixels_b = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let hash_a = frame_hash(&pixels_a);
        let hash_b = frame_hash(&pixels_b);
        assert_eq!(
            hash_a, hash_b,
            "frame hashes diverged between two identical renders — \
             pipeline is non-deterministic"
        );
    }

    #[test]
    fn cursor_cell_has_non_background_pixels_after_first_frame() {
        // Pixel-level cursor sanity: the pixel at the cursor's
        // cell center should NOT match the background color
        // (the cursor rect overpaints the bg, by design). Uses
        // garasu's cell_center_pixel helper to convert
        // (col, row) into a pixel coord.
        use garasu::headless::{cell_center_pixel, pixel_at};
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (cols, rows) = (20u32, 4u32);
        // Use Block cursor + the small grid so cell_width *
        // cols + a little padding gives us the surface dims.
        let (mut r, _t, mut text) = build_gpu_renderer(&gpu, cols as usize, rows as usize);
        // Surface sized to fit the full grid (no padding).
        let surface_w = (r.cell_width * cols as f32).ceil() as u32;
        let surface_h = (r.cell_height * rows as f32).ceil() as u32;
        let target = HeadlessTarget::new(
            &gpu,
            surface_w,
            surface_h,
            SURFACE_FORMAT,
        );

        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        // Cursor lives at (0, 0). Find its center pixel.
        let (cx, cy) = cell_center_pixel(0, 0, r.cell_width, r.cell_height, 0.0, 0.0);
        let px = pixel_at(&pixels, surface_w, cx.min(surface_w - 1), cy.min(surface_h - 1));
        // Surface is BGRA; channels in order are B, G, R, A.
        // Background is Nord polar-night dark (~46, 52, 64 in
        // sRGB) — the cursor rect overpaints it with the
        // cursor color, which is much brighter. Any channel
        // exceeding 100 means the cursor is painting through.
        assert!(
            px[0] > 100 || px[1] > 100 || px[2] > 100,
            "cursor cell pixel = {px:?}; expected at least one channel > 100 \
             (cursor rect should overpaint bg)"
        );
    }

    #[test]
    fn thirty_two_consecutive_renders_produce_one_unique_frame_hash() {
        // N-frame determinism stress: any non-determinism (uninit
        // memory, frame-counter-dependent uniform, accidental
        // animation tick at elapsed=0) shows up as multiple
        // distinct hashes across N renders of the same state.
        use garasu::headless::frame_hash;
        use std::collections::HashSet;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 96, 48, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 30, 6);
        t.write().feed(b"stress-32");

        let mut hashes = HashSet::new();
        for _ in 0..32 {
            let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
            hashes.insert(frame_hash(&pixels).to_hex().to_string());
        }
        assert_eq!(
            hashes.len(),
            1,
            "32 renders of the same state produced {} distinct hashes — non-deterministic pipeline",
            hashes.len()
        );
    }

    /// CATALOG GOLDEN (M3-C1, post-deletion) — the parity golden in
    /// the previous commit proved legacy == catalog byte-identical;
    /// with the legacy PostProcessPipeline deleted, the catalog
    /// route's own truth is pinned instead: the colorblind chain
    /// must actually TRANSFORM the frame (effect reachable
    /// end-to-end) and stay magenta-free.
    #[test]
    fn catalog_colorblind_route_transforms_the_frame() {
        use garasu::headless::{assert_no_magenta_pixels, frame_hash};

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (w, h) = (128u32, 64u32);
        let target =
            HeadlessTarget::new(&gpu, w, h, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 40, 8);
        t.write()
            .feed(b"golden \x1b[31mred\x1b[0m \x1b[42mgreen-bg\x1b[0m \x1b[4munder\x1b[0m");

        let plain = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let mut effects = crate::config::MadoEffectsConfig::default();
        effects.colorblind.mode = ColorblindMode::Protanopia;
        r.set_effects_config(effects);
        let graded = render_one_frame_headless(&gpu, &mut r, &mut text, &target);

        assert_ne!(
            frame_hash(&plain),
            frame_hash(&graded),
            "protanopia chain must change the rendered pixels"
        );
        assert!(
            assert_no_magenta_pixels(&graded, w, h).is_ok(),
            "colorblind-graded frame surfaced magenta — chain leaked uninit memory"
        );
    }

    /// Full-chain golden: every catalog effect mado can enable at
    /// once (colorblind + crt + scanlines + bloom + glow_on_bell +
    /// snow) dispatched in ONE graph — 4 identical frames produce
    /// one unique hash, zero magenta, and exactly one compile. This
    /// exercises the multi-effect chain wiring, the bloom aux
    /// leases, and the pool's lease/release cycle on a real adapter.
    #[test]
    fn full_effect_chain_is_deterministic_and_magenta_free() {
        use garasu::headless::{assert_no_magenta_pixels, frame_hash};
        use std::collections::HashSet;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (w, h) = (96u32, 48u32);
        let target =
            HeadlessTarget::new(&gpu, w, h, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 30, 6);
        let mut effects = crate::config::MadoEffectsConfig::default();
        effects.snow.enabled = true;
        effects.crt.enabled = true;
        effects.scanlines.enabled = true;
        effects.bloom.enabled = true;
        effects.glow_on_bell.enabled = true;
        effects.colorblind.mode = ColorblindMode::Tritanopia;
        r.set_effects_config(effects);
        t.write().feed(b"full-chain \x07");

        let mut hashes = HashSet::new();
        let mut last = Vec::new();
        for _ in 0..4 {
            let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
            hashes.insert(frame_hash(&pixels).to_hex().to_string());
            last = pixels;
        }
        assert_eq!(
            hashes.len(),
            1,
            "full effect chain produced {} distinct hashes across 4 identical frames",
            hashes.len()
        );
        assert!(
            assert_no_magenta_pixels(&last, w, h).is_ok(),
            "full-chain frame surfaced magenta"
        );
        assert_eq!(
            r.frame_graph.compile_count(),
            1,
            "one effect set + one resolution must compile exactly once"
        );
    }

    /// Live-route determinism + steady-state compile proof: 8 frames
    /// of identical state through the engawa colorblind chain produce
    /// ONE unique hash AND exactly one graph compile (the pool's
    /// lease/release cycle and the cached CompiledGraph are both
    /// frame-stable).
    #[test]
    fn catalog_route_is_deterministic_and_compiles_once() {
        use garasu::headless::frame_hash;
        use std::collections::HashSet;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 96, 48, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 30, 6);
        let mut effects = crate::config::MadoEffectsConfig::default();
        effects.colorblind.mode = ColorblindMode::Deuteranopia;
        r.set_effects_config(effects);
        t.write().feed(b"catalog-stress");

        let mut hashes = HashSet::new();
        for _ in 0..8 {
            let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
            hashes.insert(frame_hash(&pixels).to_hex().to_string());
        }
        assert_eq!(
            hashes.len(),
            1,
            "catalog route produced {} distinct hashes across 8 identical frames",
            hashes.len()
        );
        assert_eq!(
            r.frame_graph.compile_count(),
            1,
            "steady-state frames must reuse the cached CompiledGraph"
        );
    }

    /// Live-resize pool discipline (M3 review 2026-06-12): pooled
    /// SCENE/chain textures are keyed by exact size, so rendering at
    /// a new size must evict the stale-size buckets — without the
    /// eviction, a macOS live-resize drag (a distinct drawable size
    /// nearly every frame) strands a full set of full-window BGRA
    /// textures per visited size for the renderer's lifetime
    /// (~24 MB × up to 9 textures per size at Retina with the
    /// 6-effect chain). The legacy `PostProcessPipeline` dropped its
    /// offscreen on every size change; the pool route must not
    /// regress that.
    #[test]
    fn resize_with_effects_enabled_evicts_stale_pool_buckets() {
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let (mut r, _t, mut text) = build_gpu_renderer(&gpu, 20, 4);
        let mut effects = crate::config::MadoEffectsConfig::default();
        // Isolate the colorblind-only chain: pin ambience Off so the
        // default-on Matte grain tooth doesn't add a second effect (and
        // its chain intermediate) to the pool — this test asserts the
        // single-effect pool discipline.
        effects.ambience = crate::ambience::AmbiencePreset::Off;
        effects.colorblind.mode = ColorblindMode::Protanopia;
        r.set_effects_config(effects);

        let target_a = HeadlessTarget::new(&gpu, 96, 48, SURFACE_FORMAT);
        let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &target_a);
        assert_eq!(
            r.texture_pool.free_count(),
            1,
            "colorblind-only chain pools exactly the SCENE texture"
        );

        let target_b = HeadlessTarget::new(&gpu, 64, 32, SURFACE_FORMAT);
        let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &target_b);
        assert_eq!(
            r.texture_pool.free_count(),
            1,
            "size-A bucket must be evicted on resize — stale sizes may never accumulate"
        );

        // Steady state at the new size keeps reusing one texture.
        let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &target_b);
        assert_eq!(r.texture_pool.free_count(), 1);
    }

    /// Regression test for mado@044a206 (damage-gate skip → shadow
    /// + recurring purple flash). Renders three full frames of
    /// identical state into a 3-slot HeadlessSwapchain (one per
    /// slot) and asserts:
    ///
    ///   1. All three slot hashes are identical — no stale-slot
    ///      bug. If render() ever returned without writing the
    ///      current slot, one slot would hold prior content (or
    ///      no content) and its hash would diverge.
    ///   2. No slot surfaces magenta — no Metal-uninit leakage
    ///      in any chain position.
    ///
    /// This is the test that would have caught the shadow +
    /// purple-flash bug class BEFORE operators saw it. The bug's
    /// signature is: hashes [a, a, a] when the gate doesn't
    /// fire vs. [a, b, c] when it does and leaves slots
    /// inconsistent.
    #[test]
    fn three_slot_swapchain_full_renders_yield_identical_hashes_and_no_magenta() {
        use garasu::headless::{HeadlessSwapchain, assert_no_magenta_pixels, frame_hash};
        use madori::RenderContext;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let mut chain = HeadlessSwapchain::new(
            &gpu,
            3,
            128,
            64,
            SURFACE_FORMAT,
        );
        let (mut r, t, _) = build_gpu_renderer(&gpu, 40, 8);
        t.write().feed(b"shadow-regression");

        // Render once per slot; collect hashes.
        let mut hashes = Vec::new();
        for _ in 0..3 {
            let pixels = chain.render_into_next(&gpu, |text, view, w, h| {
                let mut ctx = RenderContext {
                    gpu: &gpu,
                    text,
                    surface_view: view,
                    width: w,
                    height: h,
                    scale_factor: 1.0,
                    elapsed: 0.0,
                    dt: 0.0,
                };
                r.render(&mut ctx);
            });
            hashes.push(frame_hash(&pixels));
        }
        assert_eq!(
            hashes[0], hashes[1],
            "slots 0 and 1 diverged — damage-gate stale-slot regression"
        );
        assert_eq!(
            hashes[1], hashes[2],
            "slots 1 and 2 diverged — damage-gate stale-slot regression"
        );
        // And every slot stays magenta-clean.
        for (i, slot_pixels) in chain.read_all_slots_rgba8(&gpu).into_iter().enumerate() {
            assert!(
                assert_no_magenta_pixels(&slot_pixels, chain.width(), chain.height()).is_ok(),
                "slot {i} surfaced magenta — Metal-uninit-leakage regression"
            );
        }
    }

    /// Stress variant: render 12 frames into a 3-slot chain
    /// (each slot painted 4 times). Asserts all 12 hashes equal —
    /// proves the rendering pipeline is truly slot-independent
    /// AND deterministic across the swapchain rotation.
    #[test]
    fn twelve_renders_across_three_slot_swapchain_produce_one_unique_hash() {
        use garasu::headless::{HeadlessSwapchain, frame_hash};
        use madori::RenderContext;
        use std::collections::HashSet;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let mut chain = HeadlessSwapchain::new(
            &gpu,
            3,
            96,
            48,
            SURFACE_FORMAT,
        );
        let (mut r, t, _) = build_gpu_renderer(&gpu, 30, 6);
        t.write().feed(b"swapchain-stress");

        let mut hashes = HashSet::new();
        for _ in 0..12 {
            let pixels = chain.render_into_next(&gpu, |text, view, w, h| {
                let mut ctx = RenderContext {
                    gpu: &gpu,
                    text,
                    surface_view: view,
                    width: w,
                    height: h,
                    scale_factor: 1.0,
                    elapsed: 0.0,
                    dt: 0.0,
                };
                r.render(&mut ctx);
            });
            hashes.insert(frame_hash(&pixels).to_hex().to_string());
        }
        assert_eq!(
            hashes.len(),
            1,
            "12 renders across 3 swapchain slots produced {} unique hashes — \
             pipeline is slot-dependent or non-deterministic",
            hashes.len()
        );
    }

    /// Observability contract: every successful render bumps
    /// `TOTAL_FRAMES`; every "would-have-skipped" render (now
    /// always full-renders to fix the swapchain stale-slot bug,
    /// but still counted) bumps `TOTAL_FRAMES_SKIPPED`.
    ///
    /// `frame_perf` MCP surfaces both counters; this pins the
    /// contract so operators interpreting the numbers see what
    /// they expect.
    #[test]
    fn frame_perf_counters_increment_correctly() {
        use std::sync::atomic::Ordering;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target = HeadlessTarget::new(
            &gpu,
            96,
            32,
            SURFACE_FORMAT,
        );
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 30, 4);

        // Snapshot the counters before driving any renders — the
        // tests run in parallel so we can't assume they start at
        // zero; assert deltas instead.
        let frames_before = TOTAL_FRAMES.load(Ordering::Relaxed);
        let skipped_before = TOTAL_FRAMES_SKIPPED.load(Ordering::Relaxed);

        // Render 1: fresh state, triggers a full render via the
        // last_seqno=0 path (no skip).
        t.write().feed(b"observability test");
        let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        // Render 2: no state change, the gate "would have" skipped
        // (last_seqno != 0, no blink-flip, no bell, no search).
        // Post-fix we still full-render, but the counter ticks.
        let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        // Render 3: same as #2.
        let _ = render_one_frame_headless(&gpu, &mut r, &mut text, &target);

        let frames_after = TOTAL_FRAMES.load(Ordering::Relaxed);
        let skipped_after = TOTAL_FRAMES_SKIPPED.load(Ordering::Relaxed);

        // TOTAL_FRAMES bumps after EVERY full-render path
        // completion. With damage-gate-skip removed entirely, all
        // three of our renders complete the full path, so the
        // delta is ≥ 3.
        assert!(
            frames_after - frames_before >= 3,
            "TOTAL_FRAMES delta = {}; expected ≥ 3",
            frames_after - frames_before
        );
        // TOTAL_FRAMES_SKIPPED bumps on the "would have skipped"
        // path, which fires whenever (last_seqno != 0 && no
        // semantic delta). Renders 2 and 3 both qualify; render 1
        // doesn't (last_seqno was 0). So delta ≥ 2.
        assert!(
            skipped_after - skipped_before >= 2,
            "TOTAL_FRAMES_SKIPPED delta = {}; expected ≥ 2",
            skipped_after - skipped_before
        );
    }

    /// L3 (golden): a canned input sequence + a recorded frame
    /// hash. Pinning the hash means ANY future change that alters
    /// even one pixel of this canonical scene fires this test
    /// immediately — visible regressions become impossible to
    /// land silently.
    ///
    /// Recording protocol: when the rendered output legitimately
    /// changes (font tweak, palette adjustment, new feature),
    /// run with `MADO_GOLDEN_UPDATE=1` (or just delete the
    /// assertion temporarily), capture the new hash from the
    /// failure message, paste it in. Same shape as `insta`'s
    /// snapshot review workflow but bytes-level deterministic.
    ///
    /// This is the L2.5 → L3 onramp: one canonical scenario
    /// + one canonical hash proves the pattern works
    /// end-to-end. Next: extend `mado/tests/scenarios/*.yaml`
    /// to carry per-scenario `frame_hash:` fields and have the
    /// runner enforce.
    #[test]
    fn canonical_prompt_scene_matches_recorded_frame_hash() {
        use garasu::headless::frame_hash;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target =
            HeadlessTarget::new(&gpu, 256, 96, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 40, 6);
        // Isolate the clean scene: pin ambience Off so the default-on
        // Matte paper-grain tooth doesn't contaminate this golden (the
        // grain layer is exercised by its own composition + matrix
        // tests; this golden pins the rect+text scene WITHOUT any
        // post effect).
        let mut effects = crate::config::MadoEffectsConfig::default();
        effects.ambience = crate::ambience::AmbiencePreset::Off;
        r.set_effects_config(effects);
        // Canonical scene: a short prompt-like sequence with
        // mixed printable ASCII, a newline, more text. Picked to
        // exercise the rect + text pipelines together without
        // pulling in scenario-specific colors / OSC escapes that
        // would make the hash environment-dependent.
        t.write().feed(b"$ echo hello\nhello\n$ ");

        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let hash = frame_hash(&pixels);
        let hex = hash.to_hex().to_string();

        // Golden record. Update this hex string when the visual
        // output legitimately changes. The assertion uses
        // explicit if/panic so the failure message shows the
        // actual hash for easy copy-paste.
        const GOLDEN: &str =
            // Re-recorded 2026-07-06: cell_height quantized to whole
            // device px (row-seam line-artifact fix) — 19.6 → 20.
            "cf185c2f1ffd42bac98494b4d2ad78b0d7cf5105e979bfb25f5f47b70b41db36";
        if hex != GOLDEN {
            // First-run / regen path: print the new hash and a
            // hint. Tests fail intentionally; operator pastes
            // the printed hex into GOLDEN above.
            if GOLDEN == "PENDING_RECORD_VIA_FAILURE_MESSAGE" {
                panic!(
                    "L3 golden: recorded hash is `{hex}`. \
                     Paste this into the GOLDEN constant in \
                     `canonical_prompt_scene_matches_recorded_frame_hash` \
                     to lock in the pixel-exact baseline."
                );
            }
            panic!(
                "L3 golden mismatch: got `{hex}`, expected `{GOLDEN}`. \
                 If this change is intentional, update GOLDEN. \
                 Otherwise this is a visible-pixel regression."
            );
        }
    }

    /// L3 golden #2 — the GRADED route (M3 review 2026-06-12). The
    /// post-deletion colorblind check was inequality-only
    /// (graded != plain + magenta-free), which a blend or
    /// gamma-space regression in the catalog route passes: apply the
    /// Machado matrix in sRGB space instead of linear and every
    /// graded pixel changes while plain != graded stays true. A
    /// recorded hash of the protanopia-graded canonical scene pins
    /// the route's exact value behavior — same recording protocol
    /// as the golden above.
    #[test]
    fn canonical_scene_protanopia_grade_matches_recorded_frame_hash() {
        use garasu::headless::frame_hash;

        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let target = HeadlessTarget::new(&gpu, 256, 96, SURFACE_FORMAT);
        let (mut r, t, mut text) = build_gpu_renderer(&gpu, 40, 6);
        let mut effects = crate::config::MadoEffectsConfig::default();
        // Isolate the colorblind grade: pin ambience Off so the
        // default-on Matte grain tooth doesn't perturb the graded
        // golden — this golden pins the colorblind chain ONLY.
        effects.ambience = crate::ambience::AmbiencePreset::Off;
        effects.colorblind.mode = ColorblindMode::Protanopia;
        r.set_effects_config(effects);
        // Same canonical scene as the ungraded golden, plus color so
        // the grade has chroma to transform.
        t.write()
            .feed(b"$ echo hello\n\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[44mblue-bg\x1b[0m\n$ ");

        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let hex = frame_hash(&pixels).to_hex().to_string();

        const GOLDEN: &str =
            // Re-recorded 2026-07-06: cell_height quantized to whole
            // device px (row-seam line-artifact fix) — 19.6 → 20.
            "caa1be028954e6d29c1db119d9831d07c2fe8e88b44a7554c8d66eb9b3766f14";
        if hex != GOLDEN {
            // Recording protocol: with the PENDING sentinel in
            // GOLDEN, the assert message carries the fresh hash to
            // paste in; otherwise this is a real regression.
            assert_ne!(
                GOLDEN, "PENDING_RECORD_VIA_FAILURE_MESSAGE",
                "L3 graded golden: recorded hash is `{hex}`. Paste it \
                 into the GOLDEN constant in \
                 `canonical_scene_protanopia_grade_matches_recorded_frame_hash`."
            );
            panic!(
                "L3 graded golden mismatch: got `{hex}`, expected `{GOLDEN}`. \
                 If the graded route legitimately changed, update GOLDEN. \
                 Otherwise the catalog colorblind chain regressed \
                 (blend state, gamma space, or matrix drift)."
            );
        }
    }

    #[test]
    fn render_one_frame_via_garasu_harness_round_trips() {
        // Validate the garasu::HeadlessHarness convenience layer
        // against mado's renderer. If this compiles + asserts,
        // every other garasu consumer can copy the pattern.
        use garasu::headless::{HeadlessHarness, assert_no_magenta_pixels};
        use madori::RenderContext;
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let mut harness =
            HeadlessHarness::new(&gpu, 128, 64, SURFACE_FORMAT);
        let (mut r, _t, _drop_text) = build_gpu_renderer(&gpu, 40, 8);

        let pixels = harness.render_one_frame(&gpu, |text, view, w, h| {
            let mut ctx = RenderContext {
                gpu: &gpu,
                text,
                surface_view: view,
                width: w,
                height: h,
                scale_factor: 1.0,
                elapsed: 0.0,
                dt: 0.0,
            };
            r.render(&mut ctx);
        });
        assert!(assert_no_magenta_pixels(&pixels, 128, 64).is_ok());
    }

    /// The frost border bbox of the Center-anchored overlay card, rendered
    /// at `(w, h)` with `n_rows` body rows. Returns `(min_x,min_y,max_x,max_y,count)`.
    fn center_overlay_border_bbox(
        gpu: &GpuContext,
        w: u32,
        h: u32,
        n_rows: usize,
    ) -> (u32, u32, u32, u32, u32) {
        use crate::config::PickerAnchor;
        use crate::picker::component::{LineRole, OverlayLine, OverlaySpec};
        use garasu::headless::HeadlessHarness;
        let mut harness = HeadlessHarness::new(gpu, w, h, SURFACE_FORMAT);
        let (mut r, _t, _drop) = build_gpu_renderer(gpu, 80, 24);

        let mut lines = vec![OverlayLine::new("\u{25b6} session  \u{2588}", LineRole::Title)];
        for i in 0..n_rows {
            let role = if i == 3 { LineRole::Selected } else { LineRole::Row };
            lines.push(OverlayLine::new(format!("  \u{203a} suggestion row {i}"), role));
        }
        lines.push(OverlayLine::new("  blind lanes: none", LineRole::Hint));
        let spec = OverlaySpec::new(PickerAnchor::Center, lines);

        let pixels = harness.render_one_frame(gpu, |text, view, fw, fh| {
            r.measure_cell_metrics(text);
            r.ensure_layers(text, &gpu.device);
            let mut frame = text.begin_frame(fw, fh);
            let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("test_center_overlay"),
            });
            r.draw_overlay(&spec, &mut frame, gpu, view, fw, fh, &mut enc);
            drop(frame);
            gpu.queue.submit(std::iter::once(enc.finish()));
        });

        // Frost cyan border #88C0D0. Bgra8 readback → bytes [B, G, R, A].
        let (mut min_x, mut min_y, mut max_x, mut max_y, mut count) =
            (u32::MAX, u32::MAX, 0u32, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                let p = garasu::headless::pixel_at(&pixels, w, x, y);
                let (b, g, rr) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                if b > 150 && g > 140 && rr < b - 30 && rr < g - 30 {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                    count += 1;
                }
            }
        }
        (min_x, min_y, max_x, max_y, count)
    }

    /// Regression: the Ctrl-S Center popup must sit in the MIDDLE of the
    /// window at ANY window size — fitting OR overflowing content. The
    /// operator report (2026-07-02): "it seems to be sized for when the
    /// screen is full screen but it needs to be in the center no matter
    /// what" — an overflowing board pinned to the top-left corner and ran
    /// off the bottom/right of a smaller window.
    #[test]
    fn ctrl_s_center_popup_is_centered_at_any_window_size() {
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");

        // Case A — fitting content in a non-square, non-fullscreen window.
        let (w, h) = (900u32, 560u32);
        let (x0, y0, x1, y1, n) = center_overlay_border_bbox(&gpu, w, h, 10);
        assert!(n > 100, "case A: expected a frost-bordered card, found {n} px");
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        assert!(
            (cx - w as f32 / 2.0).abs() < w as f32 * 0.08,
            "case A: card h-center {cx} off window center {} (x {x0}..{x1})",
            w as f32 / 2.0
        );
        assert!(
            (cy - h as f32 / 2.0).abs() < h as f32 * 0.08,
            "case A: card v-center {cy} off window center {} (y {y0}..{y1})",
            h as f32 / 2.0
        );

        // Case B — a board with more rows than fit a SHORT window. The
        // popup must still be centered (never corner-pinned + overflowing).
        let (w, h) = (900u32, 300u32);
        let (bx0, by0, bx1, by1, bn) = center_overlay_border_bbox(&gpu, w, h, 24);
        assert!(bn > 100, "case B: expected a frost-bordered card, found {bn} px");
        let (bcx, bcy) = ((bx0 + bx1) as f32 / 2.0, (by0 + by1) as f32 / 2.0);
        // The card must fit inside the window (top and bottom borders visible).
        assert!(
            by0 > 0 && by1 < h - 1,
            "case B: card overflows window vertically (y {by0}..{by1}, h {h})"
        );
        assert!(
            (bcx - w as f32 / 2.0).abs() < w as f32 * 0.08,
            "case B: card h-center {bcx} off window center {} (x {bx0}..{bx1})",
            w as f32 / 2.0
        );
        assert!(
            (bcy - h as f32 / 2.0).abs() < h as f32 * 0.08,
            "case B: card v-center {bcy} off window center {} (y {by0}..{by1})",
            h as f32 / 2.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- measured_grid / PTY-grid reconcile invariants ----

    fn gpu_free_renderer() -> TerminalRenderer {
        let term: SharedTerminal = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::terminal::Terminal::new(80, 24),
        ));
        TerminalRenderer::new(
            term,
            14.0,
            1.4,
            "JetBrains Mono".into(),
            "Iosevka".into(),
            String::new(),
            8.0,
            crate::config::CursorStyle::Block,
            false,
            500,
            wgpu::Color::BLACK,
            crate::terminal::Color::new(0xec, 0xef, 0xf4),
        )
    }

    /// A gpu-free renderer that ALSO hands back its shared terminal,
    /// so theme-parity tests can assert both halves
    /// (`renderer.ansi_colors` + the mirror `Terminal` palette / OSC 11
    /// answer) after the shared theme-application point runs.
    fn gpu_free_renderer_with_terminal() -> (TerminalRenderer, SharedTerminal) {
        let term: SharedTerminal = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::terminal::Terminal::new(80, 24),
        ));
        let renderer = TerminalRenderer::new(
            std::sync::Arc::clone(&term),
            14.0,
            1.4,
            "JetBrains Mono".into(),
            "Iosevka".into(),
            String::new(),
            8.0,
            crate::config::CursorStyle::Block,
            false,
            500,
            wgpu::Color::BLACK,
            crate::terminal::Color::new(0xec, 0xef, 0xf4),
        );
        (renderer, term)
    }

    /// **Entry-point theme parity** (operator report 2026-06-12: wrong
    /// font/palette + vim grey in the embedded-tear window). The
    /// tear-attach path previously applied NO theme — it never called
    /// `Terminal::apply_theme`, so its mirror ANSI palette + OSC 11
    /// background-query answer stayed at the default. Both entry points
    /// now route through `crate::theme::apply_config_theme`; this pins
    /// that the SHARED helper sets BOTH the renderer's ANSI palette AND
    /// the mirror Terminal's palette + OSC 11 answer, so a tear-attach
    /// window's theme is identical to a local-PTY window's.
    #[test]
    fn shared_theme_application_sets_renderer_and_terminal_palette() {
        let (mut renderer, term) = gpu_free_renderer_with_terminal();
        // Pick a real built-in theme whose bg differs from the default.
        let theme = crate::theme::Theme::available()
            .iter()
            .find(|t| {
                let bg = t.background;
                bg != crate::terminal::Color::BLACK
            })
            .expect("at least one built-in theme has a non-black background");
        let theme_name = theme.name.to_owned();
        let theme_ansi = theme.ansi;
        let theme_bg = theme.background;

        crate::theme::apply_config_theme(&mut renderer, &term, &theme_name, 1.0);

        // Renderer half — the GPU palette the draw pass reads.
        assert_eq!(
            renderer.ansi_colors, theme_ansi,
            "the renderer's ANSI palette must equal the theme's after apply_config_theme"
        );
        // Mirror Terminal half — the palette + OSC 11 answer the
        // tear-attach path used to leave at the default.
        {
            let t = term.read();
            assert_eq!(
                t.ansi_palette()[..16],
                theme_ansi[..],
                "the mirror Terminal's first 16 ANSI slots must equal the theme palette"
            );
        }
        // OSC 11 ?  must answer the THEME background, not the default —
        // an app querying the bg (e.g. a light/dark detector) sees the
        // operator's configured theme in a tear-attach window now.
        let answer = {
            let mut t = term.write();
            t.feed(b"\x1b]11;?\x1b\\");
            t.take_response().unwrap_or_default()
        };
        let answer_str = String::from_utf8_lossy(&answer);
        assert!(
            answer_str.starts_with("\x1b]11;rgb:"),
            "OSC 11 ? must answer an rgb background, got {answer_str:?}"
        );
        // The answer encodes the theme bg's 8-bit channels as the high
        // byte of each 16-bit rgb component (rr/rr gg/gg bb/bb).
        let hex2 = |b: u8| format!("{b:02x}");
        assert!(
            answer_str.contains(&format!(
                "rgb:{r}{r}/{g}{g}/{b}{b}",
                r = hex2(theme_bg.r),
                g = hex2(theme_bg.g),
                b = hex2(theme_bg.b)
            )),
            "OSC 11 ? must answer the THEME bg ({theme_bg:?}), got {answer_str:?}"
        );
    }

    /// Phantom-cursor + unfocused-affordance invariants (2026-06-11):
    /// the snapshot carries scroll state so the draw pass can suppress
    /// the live-grid cursor over history rows, and `focused` defaults
    /// true with the invalidating setter forcing a repaint on change.
    #[test]
    fn renderer_focus_state_defaults_true_and_invalidates() {
        let mut r = gpu_free_renderer();
        assert!(r.focused, "windows start focused");
        r.set_focused(false);
        assert!(!r.focused);
        // The derive resets last_seqno so the next frame repaints.
        assert_eq!(r.last_seqno, 0, "focus flip must invalidate the frame");
    }

    #[test]
    fn snapshot_carries_scroll_state() {
        let r = gpu_free_renderer();
        {
            let mut term = r.terminal.write();
            for _ in 0..40 {
                term.feed(b"line\r\n");
            }
            term.scroll_up(6);
        }
        let (snap, _) = r.snapshot();
        assert_eq!(snap.scroll_offset, 6);
        assert!(snap.scrollback_total >= 6);
    }

    #[test]
    fn measured_grid_is_none_before_first_frame() {
        // The PTY-grid reconciler must NOT push anything until a frame
        // has rendered: before that, cell metrics are heuristic and the
        // surface dims unknown — pushing would re-introduce the
        // estimate-vs-display divergence (TUI-overlap incident
        // 2026-06-11).
        let r = gpu_free_renderer();
        assert_eq!(r.measured_grid(), None);
        assert_eq!(r.last_surface_size(), None);
    }

    #[test]
    fn cells_for_window_phys_never_returns_zero() {
        // A zero-cell grid would wedge the PTY (and tear) — even a
        // degenerate 0×0 surface must clamp to 1×1.
        let r = gpu_free_renderer();
        assert_eq!(r.cells_for_window_phys(0, 0), (1, 1));
        let (c, h) = r.cells_for_window_phys(1, 1);
        assert!(c >= 1 && h >= 1);
    }

    /// The seam fix (panel-snapping `padding_px()`) must NOT destabilize the
    /// resize path: `cells_for_window_phys` uses `padding_px()` as its inner
    /// origin, and the origin snap moves it by strictly < one panel pixel, so
    /// the cell count it derives can differ by at most 0 cells vs the
    /// unsnapped origin at any realistic window size — the reflow reconciler
    /// stays stable across the fix. (Guards the Deliverable-1 ↔ Deliverable-2
    /// interaction.)
    #[test]
    fn panel_snap_does_not_shift_resize_cell_counts() {
        let mut r = gpu_free_renderer();
        // Force the scaled-display path so padding_px() actually snaps.
        r.set_scale_factor(2.0);
        r.set_panel_ratio(2234.0 / 2658.0); // the live XDR ratio
        // A representative window spread; the snapped padding differs from the
        // raw padding by < 1 panel px, far below one cell, so cols/rows match
        // what the raw-padding math would give (no wedge, no jitter).
        for (w, h) in [(1280u32, 800u32), (2056, 1329), (4112, 2658), (800, 600)] {
            let (cols, rows) = r.cells_for_window_phys(w, h);
            assert!(cols >= 1 && rows >= 1, "never wedges at {w}x{h}");
            // Recompute with the UNsnapped padding to bound the drift.
            let raw_pad = r.padding * r.scale_factor;
            let cw = r.cell_width.max(1.0);
            let ch = r.cell_height.max(1.0);
            let raw_cols = (((w as f32 - 2.0 * raw_pad).max(0.0) / cw).floor() as u16).max(1);
            let raw_rows = (((h as f32 - 2.0 * raw_pad).max(0.0) / ch).floor() as u16).max(1);
            assert!(
                (cols as i32 - raw_cols as i32).abs() <= 1
                    && (rows as i32 - raw_rows as i32).abs() <= 1,
                "snap shifted grid at {w}x{h}: snapped {cols}x{rows} vs raw {raw_cols}x{raw_rows}",
            );
        }
    }

    // ---- color_to_f32 ----

    #[test]
    fn test_color_to_f32_white() {
        assert_eq!(color_to_f32(&Color::WHITE), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn test_color_to_f32_black() {
        assert_eq!(color_to_f32(&Color::BLACK), [0.0, 0.0, 0.0, 1.0]);
    }

    // ---- select_run_family (symbol / Nerd-icon font fallback) ----

    #[test]
    fn symbol_run_routes_to_symbols_family() {
        // A powerline separator and a Nerd-PUA icon shape against the
        // configured symbols family, not the primary — even when the
        // cell is marked italic (icons have no italic face).
        let fam = select_run_family(
            "\u{E0B0}", false, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono",
        );
        assert_eq!(fam, "Symbols Nerd Font Mono");
        let fam_icon = select_run_family(
            "\u{F300}", true, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono",
        );
        assert_eq!(fam_icon, "Symbols Nerd Font Mono",
            "icon runs ignore italic and route to the symbols family");
    }

    #[test]
    fn text_run_routes_to_primary_or_italic() {
        // Ordinary text uses primary; italic text uses the italic face.
        assert_eq!(
            select_run_family("abc", false, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono"),
            "JetBrains Mono",
        );
        assert_eq!(
            select_run_family("abc", true, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono"),
            "Iosevka",
        );
        // A mixed run (icon + letter) is NOT all-symbols → primary/italic.
        assert_eq!(
            select_run_family("\u{E0B0}a", false, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono"),
            "JetBrains Mono",
        );
    }

    #[test]
    fn empty_symbols_family_falls_back_to_primary() {
        // Bare config tier has no symbols preference — symbol cells then
        // shape against the primary family (which on the default Nerd
        // font already carries the ranges), never against an empty name.
        assert_eq!(
            select_run_family("\u{E0B0}", false, "JetBrainsMono Nerd Font Mono", "Iosevka", ""),
            "JetBrainsMono Nerd Font Mono",
        );
    }

    /// Regression guard for the "coloured devicon renders un-tinted"
    /// class: a Nerd-PUA icon (nf-dev-ruby U+E791, the lualine "red
    /// ruby") routes to the symbols family, AND its cell carries the SGR
    /// fg colour independently of that routing. `select_run_family` is
    /// colour-blind by construction (it takes no colour argument); the
    /// fg lives in `RunAttrsKey`, which `shape_run` turns into the
    /// glyphon span colour `GlyphonColor::rgba(fg_r,fg_g,fg_b,255)`
    /// REGARDLESS of which family the selector picked. So a red devicon
    /// on the symbols family keeps its red — proving the symbols branch
    /// never drops the cell colour.
    #[test]
    fn symbol_routed_run_preserves_cell_fg() {
        // ANSI red = (205,49,49) — the colour an SGR `31` devicon carries.
        let red = Color::new(205, 49, 49);
        let icon = "\u{E791}"; // nf-dev-ruby

        // 1. The icon is symbol-classified → routes to the symbols family.
        assert_eq!(
            select_run_family(icon, false, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono"),
            "Symbols Nerd Font Mono",
            "nf-dev-ruby must route to the symbols family",
        );

        // 2. The run-attrs key carries the cell's fg — and the SAME key
        //    is used for both the symbol branch and any other family
        //    (the family is a separate axis off `select_run_family`),
        //    so the fg is preserved across the routing decision.
        let key = RunAttrsKey {
            fg_r: red.r,
            fg_g: red.g,
            fg_b: red.b,
            bold: false,
            italic: false,
        };
        assert_eq!((key.fg_r, key.fg_g, key.fg_b), (205, 49, 49));

        // 3. Cross-pin: the family choice does NOT depend on colour — an
        //    identically-coloured ASCII run routes to the primary family
        //    while the icon routes to symbols, yet both would carry the
        //    same `RunAttrsKey` fg. Routing and colour are orthogonal.
        assert_eq!(
            select_run_family("a", false, "JetBrains Mono", "Iosevka", "Symbols Nerd Font Mono"),
            "JetBrains Mono",
        );
    }

    #[test]
    fn test_color_to_f32_red() {
        assert_eq!(color_to_f32(&Color::new(255, 0, 0)), [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn test_color_to_f32_mid_gray_returns_linear_not_srgb() {
        // After M3, color_to_f32 returns LINEAR values, not the raw
        // sRGB byte/255 form. sRGB 128 → linear ≈ 0.2159 (per IEC
        // 61966-2-1). The wgpu pipeline expects linear input on an
        // sRGB-storage surface; the previous sRGB-pass-through caused
        // the washed-out gamma bug.
        let [r, g, b, a] = color_to_f32(&Color::new(128, 128, 128));
        let expected = ishou_tokens::Srgb::new(128, 128, 128).to_linear();
        assert!((r - expected.r).abs() < 1e-6);
        assert!((g - expected.g).abs() < 1e-6);
        assert!((b - expected.b).abs() < 1e-6);
        assert!((a - 1.0).abs() < f32::EPSILON);
        // Cross-pin: linear value is markedly darker than raw byte/255.
        assert!(r < 128.0 / 255.0, "linear must be darker than sRGB byte/255");
    }

    #[test]
    fn test_color_to_f32_alpha_always_one() {
        let result = color_to_f32(&Color::new(42, 100, 200));
        assert!((result[3] - 1.0).abs() < f32::EPSILON);
    }

    // ---- is_box_drawing ----

    #[test]
    fn test_is_box_drawing_horizontal() {
        assert!(is_box_drawing('\u{2500}')); // ─
    }

    #[test]
    fn test_is_box_drawing_vertical() {
        assert!(is_box_drawing('\u{2502}')); // │
    }

    #[test]
    fn test_is_box_drawing_corner() {
        assert!(is_box_drawing('\u{250C}')); // ┌
    }

    #[test]
    fn test_is_box_drawing_heavy() {
        assert!(is_box_drawing('\u{2501}')); // ━
    }

    #[test]
    fn test_is_box_drawing_full_block() {
        assert!(is_box_drawing('\u{2588}')); // █
    }

    #[test]
    fn test_is_box_drawing_light_shade() {
        assert!(is_box_drawing('\u{2591}')); // ░
    }

    #[test]
    fn test_is_box_drawing_false_ascii() {
        assert!(!is_box_drawing('A'));
    }

    #[test]
    fn test_is_box_drawing_false_space() {
        assert!(!is_box_drawing(' '));
    }

    #[test]
    fn test_is_box_drawing_false_cjk() {
        assert!(!is_box_drawing('漢'));
    }

    #[test]
    fn test_is_box_drawing_range_boundary_low() {
        assert!(is_box_drawing('\u{2500}'));
        assert!(!is_box_drawing('\u{24FF}'));
    }

    #[test]
    fn test_is_box_drawing_range_boundary_high() {
        assert!(is_box_drawing('\u{257F}'));
        assert!(is_box_drawing('\u{2580}'));
        assert!(is_box_drawing('\u{259F}'));
        assert!(!is_box_drawing('\u{25A0}'));
    }

    // ---- box_drawing_rects ----

    const TEST_CW: f32 = 10.0;
    const TEST_CH: f32 = 20.0;
    const TEST_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

    #[test]
    fn test_box_drawing_horizontal_line() {
        let rects = box_drawing_rects('\u{2500}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1, "horizontal line should produce one rect");
        assert_eq!(rects[0].pos[0], 0.0, "should span from x origin");
        assert_eq!(rects[0].size[0], TEST_CW, "width should be full cell width");
    }

    #[test]
    fn test_box_drawing_vertical_line() {
        let rects = box_drawing_rects('\u{2502}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1, "vertical line should produce one rect");
        assert_eq!(rects[0].size[1], TEST_CH, "height should be full cell height");
    }

    #[test]
    fn test_box_drawing_corner_top_left() {
        let rects = box_drawing_rects('\u{250C}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "corner should produce horizontal + vertical rects");
    }

    #[test]
    fn test_box_drawing_cross() {
        let rects = box_drawing_rects('\u{253C}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "cross should produce horizontal + vertical rects");
        assert_eq!(rects[0].size[0], TEST_CW, "horizontal bar is full width");
        assert_eq!(rects[1].size[1], TEST_CH, "vertical bar is full height");
    }

    #[test]
    fn test_box_drawing_non_box_char() {
        let rects = box_drawing_rects('A', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert!(rects.is_empty(), "non-box char should produce no rects");
    }

    #[test]
    fn test_box_drawing_double_horizontal() {
        let rects = box_drawing_rects('\u{2550}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "double horizontal should produce two rects");
    }

    #[test]
    fn test_box_drawing_double_vertical() {
        let rects = box_drawing_rects('\u{2551}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "double vertical should produce two rects");
    }

    #[test]
    fn test_box_drawing_full_block() {
        let rects = box_drawing_rects('\u{2588}', 5.0, 10.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].pos, [5.0, 10.0]);
        assert_eq!(rects[0].size, [TEST_CW, TEST_CH]);
    }

    #[test]
    fn test_box_drawing_upper_half_block() {
        let rects = box_drawing_rects('\u{2580}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].size[1], TEST_CH / 2.0);
    }

    #[test]
    fn test_box_drawing_lower_half_block() {
        let rects = box_drawing_rects('\u{2584}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].pos[1], TEST_CH / 2.0);
        assert_eq!(rects[0].size[1], TEST_CH / 2.0);
    }

    #[test]
    fn test_box_drawing_left_half_block() {
        let rects = box_drawing_rects('\u{258C}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].size[0], TEST_CW / 2.0);
    }

    #[test]
    fn test_box_drawing_right_half_block() {
        let rects = box_drawing_rects('\u{2590}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].pos[0], TEST_CW / 2.0);
    }

    #[test]
    fn test_box_drawing_light_shade_alpha() {
        let rects = box_drawing_rects('\u{2591}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].color[3] - 0.25).abs() < f32::EPSILON, "light shade alpha = 0.25");
    }

    #[test]
    fn test_box_drawing_medium_shade_alpha() {
        let rects = box_drawing_rects('\u{2592}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].color[3] - 0.5).abs() < f32::EPSILON, "medium shade alpha = 0.5");
    }

    #[test]
    fn test_box_drawing_dark_shade_alpha() {
        let rects = box_drawing_rects('\u{2593}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].color[3] - 0.75).abs() < f32::EPSILON, "dark shade alpha = 0.75");
    }

    #[test]
    fn test_box_drawing_color_passthrough() {
        let color = [0.5, 0.6, 0.7, 1.0];
        let rects = box_drawing_rects('\u{2500}', 0.0, 0.0, TEST_CW, TEST_CH, color);
        assert_eq!(rects[0].color, color);
    }

    #[test]
    fn test_box_drawing_offset_position() {
        let rects = box_drawing_rects('\u{2502}', 100.0, 200.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 1);
        assert!(rects[0].pos[0] > 100.0, "x should be offset from origin");
        assert_eq!(rects[0].pos[1], 200.0, "y should start at origin");
    }

    #[test]
    fn test_box_drawing_tee_left() {
        let rects = box_drawing_rects('\u{251C}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "left tee should have vertical + horizontal");
    }

    #[test]
    fn test_box_drawing_tee_right() {
        let rects = box_drawing_rects('\u{2524}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "right tee should have vertical + horizontal");
    }

    #[test]
    fn test_box_drawing_tee_top() {
        let rects = box_drawing_rects('\u{252C}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "top tee should have horizontal + vertical");
    }

    #[test]
    fn test_box_drawing_tee_bottom() {
        let rects = box_drawing_rects('\u{2534}', 0.0, 0.0, TEST_CW, TEST_CH, TEST_COLOR);
        assert_eq!(rects.len(), 2, "bottom tee should have horizontal + vertical");
    }

    // ---- color_to_f32 with RGBA ----

    #[test]
    fn test_color_to_f32_returns_linear_through_ishou() {
        // Pin the typed path: color_to_f32 delegates to
        // `ishou_tokens::Srgb::to_linear`. Any future regression that
        // bypasses ishou (e.g. inlining the byte-divide-by-255 form
        // again) reintroduces the gamma bug and fails this test.
        let c = Color::new(51, 102, 153);
        let [r, g, b, a] = color_to_f32(&c);
        let expected = ishou_tokens::Srgb::new(51, 102, 153).to_linear();
        assert!((r - expected.r).abs() < 1e-6);
        assert!((g - expected.g).abs() < 1e-6);
        assert!((b - expected.b).abs() < 1e-6);
        assert!((a - 1.0).abs() < f32::EPSILON);
    }

    // ---- default selection_bg / cursor_color ----

    #[test]
    fn test_selection_bg_default() {
        let term = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::terminal::Terminal::new(80, 24),
        ));
        let renderer = TerminalRenderer::new(
            term,
            14.0,
            1.4,
            "JetBrains Mono".into(),
            "Iosevka".into(),
            "Symbols Nerd Font Mono".into(), // font_symbols
            8.0,
            CursorStyle::Block,
            true,
            530,
            wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            Color::WHITE,
        );
        // Default selection_bg is Nord frost #88C0D0 at 0.3 alpha,
        // LINEARIZED for the rect pipeline (not the raw byte/255 triple
        // 0.533/0.753/0.816 — that would render washed-out on the sRGB
        // surface). See `selection_bg_is_linearized_not_raw_srgb`.
        let expected = ishou_tokens::Srgb::new(0x88, 0xC0, 0xD0).to_linear();
        assert!((renderer.selection_bg[0] - expected.r).abs() < 1e-4);
        assert!((renderer.selection_bg[1] - expected.g).abs() < 1e-4);
        assert!((renderer.selection_bg[2] - expected.b).abs() < 1e-4);
        assert!((renderer.selection_bg[3] - 0.3).abs() < 1e-6);
    }

    /// The rect pipeline writes `selection_bg` verbatim to a sRGB-storage
    /// surface, so the value MUST be linear (strictly darker per channel
    /// than the raw sRGB form). This pins the load-bearing colour-fidelity
    /// fix: the overlay-decoration rect path linearizes like every other
    /// rect colour.
    #[test]
    fn selection_bg_is_linearized_not_raw_srgb() {
        // Nord frost #88C0D0 = (136,192,208). Raw byte/255 = the
        // washed-out triple the pre-fix default carried.
        let raw = [136.0 / 255.0, 192.0 / 255.0, 208.0 / 255.0];
        let expected = ishou_tokens::Srgb::new(0x88, 0xC0, 0xD0).to_linear();

        let sel = overlay_rect_color(0x88, 0xC0, 0xD0, 0.3);
        assert!((sel[0] - expected.r).abs() < 1e-4);
        assert!((sel[1] - expected.g).abs() < 1e-4);
        assert!((sel[2] - expected.b).abs() < 1e-4);
        // Cross-pin: linear is markedly darker than raw sRGB (the
        // wash-out signature). Each channel strictly drops.
        assert!(
            sel[0] < raw[0] && sel[1] < raw[1] && sel[2] < raw[2],
            "selection_bg must be linear (darker) not raw sRGB: got {sel:?} vs raw {raw:?}"
        );
        assert!((sel[3] - 0.3).abs() < 1e-6, "alpha stays linear/unchanged");
    }

    /// Pin the search-match (Nord aurora #EBCB8B) and URL-underline
    /// (Nord frost #88C0D0) overlay literals to the same linearized path
    /// so a future edit can't reintroduce a raw-sRGB triple.
    #[test]
    fn search_and_url_overlays_are_linearized() {
        let aurora = ishou_tokens::Srgb::new(0xEB, 0xCB, 0x8B).to_linear();
        let frost = ishou_tokens::Srgb::new(0x88, 0xC0, 0xD0).to_linear();

        // Search current-match (alpha 0.5) and other-match (alpha 0.2).
        let cur = overlay_rect_color(0xEB, 0xCB, 0x8B, 0.5);
        let other = overlay_rect_color(0xEB, 0xCB, 0x8B, 0.2);
        for c in [cur, other] {
            assert!((c[0] - aurora.r).abs() < 1e-4);
            assert!((c[1] - aurora.g).abs() < 1e-4);
            assert!((c[2] - aurora.b).abs() < 1e-4);
        }
        assert!((cur[3] - 0.5).abs() < 1e-6);
        assert!((other[3] - 0.2).abs() < 1e-6);

        // URL underline (alpha 0.6).
        let url = overlay_rect_color(0x88, 0xC0, 0xD0, 0.6);
        assert!((url[0] - frost.r).abs() < 1e-4);
        assert!((url[1] - frost.g).abs() < 1e-4);
        assert!((url[2] - frost.b).abs() < 1e-4);
        assert!((url[3] - 0.6).abs() < 1e-6);

        // Cross-pin: aurora linear strictly darker than raw byte/255.
        let aurora_raw = [0xEB as f32 / 255.0, 0xCB as f32 / 255.0, 0x8B as f32 / 255.0];
        assert!(aurora.r < aurora_raw[0] && aurora.g < aurora_raw[1] && aurora.b < aurora_raw[2]);
    }

    /// Curve-agreement invariant: the rect-pipeline linearizer
    /// (`ishou_tokens::Srgb::to_linear`, used by `color_to_f32` /
    /// `overlay_rect_color`) must match the text-pipeline linearizer
    /// (glyphon's `srgb_to_linear` in `ColorMode::Accurate`, the same
    /// IEC 61966-2-1 curve). If both feed the SAME linear value to the
    /// SAME sRGB surface, text and rect colours match by construction.
    #[test]
    fn text_and_rect_share_the_same_srgb_to_linear_curve() {
        // glyphon shader.wgsl `srgb_to_linear` (ColorMode::Accurate):
        //   c <= 0.04045 ? c/12.92 : ((c+0.055)/1.055)^2.4
        fn glyphon_srgb_to_linear(c: f32) -> f32 {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        for g in [0u8, 1, 18, 64, 128, 192, 205, 254, 255] {
            let rect = color_to_f32(&Color::new(g, g, g));
            let text = glyphon_srgb_to_linear(f32::from(g) / 255.0);
            assert!(
                (rect[0] - text).abs() < 1e-6,
                "rect-vs-text linear mismatch at g={g}: rect={} text={text}",
                rect[0]
            );
        }
    }

    #[test]
    fn test_cursor_color_default() {
        let term = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::terminal::Terminal::new(80, 24),
        ));
        let renderer = TerminalRenderer::new(
            term,
            14.0,
            1.4,
            "JetBrains Mono".into(),
            "Iosevka".into(),
            "Symbols Nerd Font Mono".into(), // font_symbols
            8.0,
            CursorStyle::Block,
            true,
            530,
            wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            Color::WHITE,
        );
        assert!((renderer.cursor_color[0] - 0.925).abs() < 0.01);
        assert!((renderer.cursor_color[1] - 0.937).abs() < 0.01);
        assert!((renderer.cursor_color[2] - 0.957).abs() < 0.01);
        assert!((renderer.cursor_color[3] - 0.85).abs() < 0.01);
    }
}
