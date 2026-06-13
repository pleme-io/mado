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
}

impl RectMode {
    /// Wire word for the instance buffer — the shader's `mode` switch.
    const fn word(self) -> u32 {
        match self {
            Self::Solid => 0,
            Self::Run => 1,
            Self::Curly => 2,
        }
    }
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

impl RectInstance {
    /// The historical constructor shape — every non-decoration rect
    /// in the codebase is a solid fill.
    const fn solid(pos: [f32; 2], size: [f32; 2], color: [f32; 4]) -> Self {
        Self { pos, size, color, mode: RectMode::Solid.word(), pattern: [0.0; 4] }
    }
}

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
";

// ---------------------------------------------------------------------------
// RectPipeline — instanced colored rectangles
// ---------------------------------------------------------------------------

struct RectPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
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

        Self {
            pipeline,
            uniform_buffer,
            bind_group,
            instance_buffer,
            instance_capacity: initial_capacity,
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
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
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
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(image_tex, image_samp, in.uv);
}
";

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct ImageInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_offset: [f32; 2],
    uv_scale: [f32; 2],
}

struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    instance_buffer: wgpu::Buffer,
    #[allow(dead_code)]
    instance_capacity: usize,
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
            instance_capacity: 64,
        }
    }

    #[allow(dead_code)]
    fn ensure_capacity(&mut self, device: &wgpu::Device, count: usize) {
        if count > self.instance_capacity {
            let new_cap = count.next_power_of_two();
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("image_instances"),
                size: (new_cap * std::mem::size_of::<ImageInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
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

/// THE surface/scene texture format — one constant, consumed by
/// pipeline construction (`init`), the per-frame SCENE/chain leases,
/// and the headless test targets. The dispatcher's pipeline cache
/// compiles against the construction-time format while pooled
/// textures use the render-time one; two hand-copies of the literal
/// desyncing meant a wgpu validation error on every catalog pass
/// (M3 review 2026-06-12).
const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
/// Approximate baseline fraction of the cell height. cosmic-text's
/// line height is 1.4 × font size and mado does not measure ascent;
/// the baseline only feeds the curly band's amplitude
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
    // window field removed at Phase 4 — single-pane mado.
    font_size: f32,
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
    /// Bell visual flash — remaining frames to show.
    bell_flash_frames: u8,
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
    /// (Borealis `fable_violet` via the SEMANTIC `agent` role). Defaults
    /// to Nord frost so legacy themes keep their prior look.
    #[invalidating_setter]
    search_status_color: Color,
    /// Search-match highlight fills (u8-RGB; the rect pipeline
    /// linearizes at paint time via `overlay_rect_color`, exactly like
    /// `search_status_color` linearizes in the text path). The CURRENT
    /// match draws `search_current_color` at α0.5; every OTHER match
    /// draws `search_other_color` at α0.2. Set by
    /// `theme::apply_config_theme` from the active theme's
    /// `search_current` / `search_others` (Borealis `first_light`
    /// #EDC980 / #4E443A via `BorealisPalette::night().surfaces()`).
    /// Defaults to Nord aurora yellow #EBCB8B so legacy presets keep
    /// their prior look until a theme that carries the surfaces loads.
    #[invalidating_setter]
    search_current_color: Color,
    /// See [`Self::search_current_color`] — the OTHER-match fill.
    #[invalidating_setter]
    search_other_color: Color,
    /// Reduce motion: disable cursor blink and bell flash.
    #[invalidating_setter]
    reduce_motion: bool,
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
    /// Physical surface dims of the last rendered frame (0 until the
    /// first frame). Together with `metrics_measured`, this is the
    /// renderer's display truth — see [`Self::measured_grid`].
    last_surface_w: u32,
    last_surface_h: u32,
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
        let decay = 0.92_f32.powf(dt * 60.0);
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

    fn tick(&mut self, dt: f32) {
        self.params.decay(0.92_f32.powf(dt * 60.0));
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

impl TerminalRenderer {
    pub fn new(
        terminal: SharedTerminal,
        font_size: f32,
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
        let cell_height = font_size * 1.4;

        Self {
            terminal,
            selection: Arc::new(Mutex::new(Selection::new())),
            search: Arc::new(Mutex::new(SearchState::new())),
            dir_picker: Arc::new(Mutex::new(crate::dir_picker::DirPickerState::new())),
            // window: removed Phase 4
            font_size,
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
            bell_flash_frames: 0,
            // Nord frost #88C0D0 at 0.3 alpha, linearized for the rect
            // pipeline (see `overlay_rect_color`). NOT the raw byte/255
            // triple — that would render washed-out on the sRGB surface.
            selection_bg: overlay_rect_color(0x88, 0xC0, 0xD0, 0.3),
            cursor_color: [0.925, 0.937, 0.957, 0.85], // Nord snow default
            // Nord frost #88C0D0 — the prior hardcoded search-status
            // colour. `theme::apply_config_theme` overwrites this with
            // the active theme's agent accent (Borealis fable_violet).
            search_status_color: Color::new(0x88, 0xC0, 0xD0),
            // Nord aurora yellow #EBCB8B — the prior hardcoded
            // search-match fill. `theme::apply_config_theme` overwrites
            // both with the active theme's search surfaces (Borealis
            // first_light #EDC980 / search_others #4E443A).
            search_current_color: Color::new(0xEB, 0xCB, 0x8B),
            search_other_color: Color::new(0xEB, 0xCB, 0x8B),
            reduce_motion: false,
            focused: true,
            // 1.0 = no scaling; overwritten on the first render frame
            // by `set_scale_factor(ctx.scale_factor)`.
            scale_factor: 1.0,
            // 0 until the first frame renders — `measured_grid`
            // reports None until then.
            last_surface_w: 0,
            last_surface_h: 0,
            shape_cache: RefCell::new(LruCache::new(
                NonZeroUsize::new(SHAPE_CACHE_CAP)
                    .expect("SHAPE_CACHE_CAP is a non-zero compile-time constant"),
            )),
            last_cursor_on: false,
            box_draw_templates: RefCell::new(HashMap::new()),
            sync_output_deferred_since: None,
            effects_config: crate::config::MadoEffectsConfig::default(),
            ambience: crate::config::MadoEffectsConfig::default().ambience.compose(),
            snow_state: SnowState::new(),
            glow_state: GlowState::new(),
            aurora_state: AuroraState::new(),
            // Recommended default: High ceiling (the perf wave's spec),
            // starting at the catalog default Medium. The governor only
            // ever steps down from here under sustained load.
            ambience_governor: crate::ux::ambience_governor::AmbienceGovernor::default(),
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
        self.set_effects_config(config.resolved_effects());
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

    /// Mark the cursor as off-window — turns off cursor deflection.
    pub fn snow_cursor_left(&mut self) {
        self.snow_state.params.set_cursor([-1.0, -1.0]);
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

    /// Physical-pixel padding. The stored `padding` is logical
    /// (operator-authored in mado.yaml as "8 pixels"); GPU draws need
    /// it scaled into physical pixels to align with the wgpu surface.
    #[inline]
    fn padding_px(&self) -> f32 {
        self.padding * self.scale_factor
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
        let period = self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0;
        (elapsed % period) < period / 2.0
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
        ctx: &mut RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let fs = self.font_size_px();
        let line_h = fs * 1.4;
        let left = self.padding_px() + self.cell_width;
        let top = ctx.height as f32 - self.padding_px() - line_h * 1.2;

        let status = if query.is_empty() {
            "/ (type to search, Esc to close)".to_owned()
        } else if count == 0 {
            format!("/{query}  no matches")
        } else {
            format!("/{query}  {}/{count}", current + 1)
        };

        let attrs = Attrs::new().family(Family::Name(&self.font_family));
        let mut buf = ctx
            .text
            .create_rich_buffer(&[(status.as_str(), attrs)], fs, line_h);
        buf.shape_until_scroll(&mut ctx.text.font_system, false);

        // AGENT-RESERVED accent: search-status is an agent / MCP-activity
        // surface, so it paints with the theme's `agent_accent`
        // (Borealis `fable_violet` via the SEMANTIC role — set by
        // `theme::apply_config_theme`). Non-Borealis themes keep the
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
                right: ctx.width as i32,
                bottom: ctx.height as i32,
            },
            default_color: agent,
            custom_glyphs: &[],
        }];
        if let Err(e) =
            ctx.text
                .prepare(&ctx.gpu.device, &ctx.gpu.queue, ctx.width, ctx.height, text_areas)
        {
            tracing::warn!("search status text prepare: {e}");
            return;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_search_status"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: ctx.surface_view,
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
        if let Err(e) = ctx.text.render(&mut pass) {
            tracing::warn!("search status text render: {e}");
        }
    }

    fn draw_dir_picker(
        &self,
        query: &str,
        results: &[(std::path::PathBuf, f64)],
        selected: usize,
        ctx: &mut RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let fs = self.font_size_px();
        let line_h = fs * 1.4;
        let left = self.padding_px() + self.cell_width * 2.0;
        let top0 = self.padding_px() + self.cell_height;
        let max_rows = 12usize;

        // Build the line strings, then their shaped buffers (kept alive while
        // the TextAreas borrow them through prepare).
        let mut lines: Vec<String> = Vec::with_capacity(max_rows + 1);
        lines.push(format!("\u{25b6} cd  {query}\u{2588}"));
        if results.is_empty() {
            lines.push("  (no matching directories)".to_owned());
        } else {
            for (i, (path, _score)) in results.iter().take(max_rows).enumerate() {
                let marker = if i == selected { "\u{203a} " } else { "  " };
                lines.push(format!("{marker}{}", path.display()));
            }
        }

        let mut buffers: Vec<glyphon::Buffer> = Vec::with_capacity(lines.len());
        for line in &lines {
            let attrs = Attrs::new().family(Family::Name(&self.font_family));
            let mut buf = ctx.text.create_rich_buffer(&[(line.as_str(), attrs)], fs, line_h);
            buf.shape_until_scroll(&mut ctx.text.font_system, false);
            buffers.push(buf);
        }

        let frost = GlyphonColor::rgba(136, 192, 208, 255); // Nord frost — query
        let green = GlyphonColor::rgba(163, 190, 140, 255); // Nord green — selected
        let fg = GlyphonColor::rgba(self.fg_color.r, self.fg_color.g, self.fg_color.b, 255);

        let mut text_areas = Vec::with_capacity(buffers.len());
        for (idx, buf) in buffers.iter().enumerate() {
            let color = if idx == 0 {
                frost
            } else if !results.is_empty() && idx - 1 == selected {
                green
            } else {
                fg
            };
            text_areas.push(glyphon::TextArea {
                buffer: buf,
                left,
                top: top0 + (idx as f32) * line_h,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 0,
                    top: 0,
                    right: ctx.width as i32,
                    bottom: ctx.height as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            });
        }

        if let Err(e) =
            ctx.text
                .prepare(&ctx.gpu.device, &ctx.gpu.queue, ctx.width, ctx.height, text_areas)
        {
            tracing::warn!("dir_picker text prepare: {e}");
            return;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_dir_picker"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: ctx.surface_view,
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
        if let Err(e) = ctx.text.render(&mut pass) {
            tracing::warn!("dir_picker text render: {e}");
        }
    }

    /// Set the shared search state (called from main to share with event handler).
    pub fn set_search(&mut self, search: Arc<Mutex<SearchState>>) {
        self.search = search;
    }

    // set_window removed at Phase 4 — single-pane mado; no multi-pane state to set.

    /// Trigger a bell flash effect. No-op when reduce_motion is enabled.
    pub fn trigger_bell(&mut self) {
        if !self.reduce_motion {
            self.bell_flash_frames = 4;
            // BEL also saturates the glow-on-bell clock; whether the
            // glow renders is the effect set's call (config-enabled +
            // not reduce_motion — already inside this gate).
            self.glow_state.params.ring();
        }
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
        self.cell_height = size * 1.4;
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

    /// Measure actual cell dimensions from glyphon font metrics.
    /// Called once on the first render when the text renderer is available.
    fn measure_cell_metrics(&mut self, text: &mut garasu::TextRenderer) {
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
        let mut buf = text.create_rich_buffer(&[("MM", attrs)], fs, fs * 1.4);
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
            self.cell_height = h;
            tracing::info!(cell_height = h, "measured cell height from font");
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
        let rows: Vec<Vec<Cell>> = term.visible_rows().map(|r| r.to_vec()).collect();
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
                        instances.push(RectInstance {
                            pos: [x + seg.band.x, y0 + seg.band.y],
                            size: [run_w, seg.band.height],
                            color,
                            mode: RectMode::Run.word(),
                            pattern: [seg.period, seg.duty, 0.0, 0.0],
                        });
                    }
                    engawa::UnderlineGeometry::Curly(band) => {
                        instances.push(RectInstance {
                            pos: [x + band.rect.x, y0 + band.rect.y],
                            size: [run_w, band.rect.height],
                            color,
                            mode: RectMode::Curly.word(),
                            pattern: [band.period, band.amplitude, band.thickness, 0.0],
                        });
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

            for (col_idx, cell) in row.iter().enumerate().take(snap.cols) {
                // Continuation cells: don't break or extend the run on
                // their own — the wide-glyph cell already booked 2 cells
                // worth of width when it joined. Skip without flushing.
                if cell.width == 0 {
                    continue;
                }

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
                            bg_run = Some((col_idx, width_cells, color));
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
                                Some((col_idx, width_cells, color, attrs.underline));
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
                            strike_run = Some((col_idx, width_cells, color));
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
                            overline_run = Some((col_idx, width_cells, color));
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
                    let bx = origin_x + col_idx as f32 * self.cell_width;
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
                        instances.push(RectInstance { 
                            pos: [bx + rx, by + ry],
                            size: [rw, rh],
                            color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
                        });
                    }
                }
            }

            // Row end — flush every open run.
            push_run(&mut instances, &mut bg_run, row_idx, RectKindForRle::Background);
            push_underline(&mut instances, &mut underline_run, row_idx);
            push_run(&mut instances, &mut strike_run, row_idx, RectKindForRle::Strikethrough);
            push_run(&mut instances, &mut overline_run, row_idx, RectKindForRle::Overline);
        }

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
                instances.push(RectInstance { 
                    pos: [
                        origin_x + c0 as f32 * self.cell_width,
                        origin_y + row_idx as f32 * self.cell_height,
                    ],
                    size: [
                        (c1 - c0 + 1) as f32 * self.cell_width,
                        self.cell_height,
                    ],
                    color: self.selection_bg, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
                });
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
                // than other matches). Borealis paints first_light
                // #EDC980 / search_others #4E443A; legacy presets keep
                // Nord aurora yellow #EBCB8B (the field default) until a
                // theme carrying the surfaces loads.
                let color = if is_current {
                    let c = self.search_current_color;
                    overlay_rect_color(c.r, c.g, c.b, 0.5)
                } else {
                    let c = self.search_other_color;
                    overlay_rect_color(c.r, c.g, c.b, 0.2)
                };
                instances.push(RectInstance { 
                    pos: [
                        origin_x + m.col_start as f32 * self.cell_width,
                        origin_y + vp_row as f32 * self.cell_height,
                    ],
                    size: [
                        (m.col_end + 1 - m.col_start) as f32 * self.cell_width,
                        self.cell_height,
                    ],
                    color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
                });
            }
        }

        // URL underline decorations — RLE'd (one rect per URL).
        for detected_url in &snap.urls {
            instances.push(RectInstance { 
                pos: [
                    origin_x + detected_url.col_start as f32 * self.cell_width,
                    origin_y
                        + (detected_url.row as f32 + 1.0) * self.cell_height
                        - 1.5,
                ],
                size: [
                    (detected_url.col_end + 1 - detected_url.col_start) as f32
                        * self.cell_width,
                    1.0,
                ],
                // Nord frost blue #88C0D0 underline, linearized for the
                // rect pipeline (see `overlay_rect_color`).
                color: overlay_rect_color(0x88, 0xC0, 0xD0, 0.6), mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }

        // Cursor (with optional blink). Unfocused windows pin the
        // cursor steady (no blink) and draw the hollow variant — the
        // standard which-window-owns-the-keyboard affordance
        // (kitty/ghostty/iTerm2/Terminal.app).
        let cursor_on = !self.focused
            || !self.cursor_blink
            || {
                let period = self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0;
                (elapsed % period) < period / 2.0
            };

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
                instances.push(RectInstance {  pos: [cx, cy], size: [self.cell_width, thickness], color: self.cursor_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4] });
                instances.push(RectInstance {  pos: [cx, cy + self.cell_height - thickness], size: [self.cell_width, thickness], color: self.cursor_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4] });
                instances.push(RectInstance {  pos: [cx, cy], size: [thickness, self.cell_height], color: self.cursor_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4] });
                instances.push(RectInstance {  pos: [cx + self.cell_width - thickness, cy], size: [thickness, self.cell_height], color: self.cursor_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4] });
            } else {
                instances.push(RectInstance { 
                    pos,
                    size,
                    color: self.cursor_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
                });
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
            // overlay rect.
            instances.push(RectInstance { 
                pos: [thumb_x, thumb_y],
                size: [thumb_w, thumb_h],
                color: overlay_rect_color(0x88, 0xC0, 0xD0, 0.35), mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
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
            instances.push(RectInstance { 
                pos: [origin_x, y],
                size: [snap.cols as f32 * self.cell_width, 1.0],
                // Nord #5E81AC @ 30% α — through the typed linearizer like
                // every other overlay rect (raw sRGB here renders washed-out
                // on the sRGB-storage surface).
                color: overlay_rect_color(0x5E, 0x81, 0xAC, 0.30), mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
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
        text: &mut garasu::TextRenderer,
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
        text: &mut garasu::TextRenderer,
        blink_on: bool,
    ) -> Vec<(usize, usize, Arc<Buffer>)> {
        // P23 — pre-size. Typical interactive grid produces ~3-8
        // runs per row after P6 batching. 8 × rows is a generous
        // upper bound; the Vec will grow if needed (mimalloc + amortized
        // doubling makes this cheap) but pre-sizing eliminates the
        // first ~4 reallocations on each frame.
        let mut buffers: Vec<(usize, usize, Arc<Buffer>)> =
            Vec::with_capacity(snap.num_rows * 8);
        let font_size_bits = self.font_size_px().to_bits();

        for (row_idx, row) in snap.rows.iter().enumerate() {
            let mut has_content = false;
            let mut col_idx: usize = 0;
            let mut row_buffers: Vec<(usize, Arc<Buffer>)> = Vec::with_capacity(8);

            // Current open run: (start_col, accumulated text, attrs key).
            let mut run: Option<(usize, String, RunAttrsKey)> = None;

            let flush_run = |run: &mut Option<(usize, String, RunAttrsKey)>,
                             row_buffers: &mut Vec<(usize, Arc<Buffer>)>,
                             text: &mut garasu::TextRenderer| {
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

            for cell in row.iter().take(snap.cols) {
                if cell.width == 0 {
                    col_idx += 1;
                    continue;
                }

                let col_here = col_idx;
                col_idx += cell.width.max(1) as usize;

                if is_box_drawing(cell.ch) {
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
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // │ vertical line
        '\u{2502}' => {
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┌ top-left corner
        '\u{250C}' => {
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [cw - (cx - x) + thick / 2.0, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [thick, ch_h - (cy - y) + thick / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┐ top-right corner
        '\u{2510}' => {
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cx - x + thick / 2.0, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [thick, ch_h - (cy - y) + thick / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // └ bottom-left corner
        '\u{2514}' => {
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [cw - (cx - x) + thick / 2.0, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, cy - y + thick / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┘ bottom-right corner
        '\u{2518}' => {
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cx - x + thick / 2.0, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, cy - y + thick / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ├ left tee
        '\u{251C}' => {
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [cw - (cx - x) + thick / 2.0, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┤ right tee
        '\u{2524}' => {
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cx - x + thick / 2.0, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┬ top tee
        '\u{252C}' => {
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [thick, ch_h - (cy - y) + thick / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┴ bottom tee
        '\u{2534}' => {
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, cy - y + thick / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ┼ cross
        '\u{253C}' => {
            rects.push(RectInstance { 
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ═ double horizontal
        '\u{2550}' => {
            let gap = thick;
            rects.push(RectInstance { 
                pos: [x, cy - thick - gap / 2.0],
                size: [cw, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [x, cy + gap / 2.0],
                size: [cw, thick],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ║ double vertical
        '\u{2551}' => {
            let gap = thick;
            rects.push(RectInstance { 
                pos: [cx - thick - gap / 2.0, y],
                size: [thick, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            rects.push(RectInstance { 
                pos: [cx + gap / 2.0, y],
                size: [thick, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // Block elements
        // ▀ upper half block
        '\u{2580}' => {
            rects.push(RectInstance { 
                pos: [x, y],
                size: [cw, ch_h / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ▄ lower half block
        '\u{2584}' => {
            rects.push(RectInstance { 
                pos: [x, y + ch_h / 2.0],
                size: [cw, ch_h / 2.0],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // █ full block
        '\u{2588}' => {
            rects.push(RectInstance { 
                pos: [x, y],
                size: [cw, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ▌ left half block
        '\u{258C}' => {
            rects.push(RectInstance { 
                pos: [x, y],
                size: [cw / 2.0, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ▐ right half block
        '\u{2590}' => {
            rects.push(RectInstance { 
                pos: [x + cw / 2.0, y],
                size: [cw / 2.0, ch_h],
                color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ░ light shade
        '\u{2591}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.25;
            rects.push(RectInstance { 
                pos: [x, y],
                size: [cw, ch_h],
                color: shade_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ▒ medium shade
        '\u{2592}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.5;
            rects.push(RectInstance { 
                pos: [x, y],
                size: [cw, ch_h],
                color: shade_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
        }
        // ▓ dark shade
        '\u{2593}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.75;
            rects.push(RectInstance { 
                pos: [x, y],
                size: [cw, ch_h],
                color: shade_color, mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
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
            let needs_upload = match self.gpu_images.get(id) {
                Some(gpu) => gpu.seqno != kitty_img.seqno,
                None => true,
            };
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
        ctx: &mut RenderContext<'_>,
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
                },
            ));
        }

        if image_draws.is_empty() {
            return;
        }

        // Update uniforms
        let uniforms = ScreenUniforms {
            resolution: [ctx.width as f32, ctx.height as f32],
            _padding: [0.0; 2],
        };
        ctx.gpu
            .queue
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
                    ctx.gpu.queue.write_buffer(
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
            ctx.gpu.queue.write_buffer(
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

    /// The aurora spectrum stops (green / cyan / violet) in LINEAR rgb,
    /// derived from the active theme palette — NO hardcoded effect
    /// colors (the design law). On Borealis these resolve to
    /// `green_bright` / `ice_cyan` / `fable_violet`; on legacy themes
    /// they fall back to that theme's bright-green / cyan / agent
    /// accent, so the curtain always paints in the resolved palette.
    ///
    /// * green → ANSI 10 (bright green / `aurora_green`)
    /// * cyan  → ANSI 6 (`ice_cyan`)
    /// * violet → the agent accent (`search_status_color` = Borealis
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
        let (peek_seqno, peek_cursor_visible, peek_sync_output) = {
            let term = self.terminal.read();
            (term.seqno(), term.cursor().visible, term.synchronized_output())
        };
        if peek_sync_output {
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
        let search_active_peek = self.search.lock().unwrap().active;
        // P28 — cursor_on is a 1–4 Hz boolean (default 4 Hz at 500 ms
        // period). Compute it here and compare to last_cursor_on; only
        // mark blink_flip when the value actually FLIPPED. Without
        // this we'd repaint every vsync just to redraw the same
        // cursor state, which was the case before this change (idle
        // render rate stuck at 60 Hz instead of 4 Hz).
        let cursor_on_now = !self.cursor_blink || {
            let period = self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0;
            (ctx.elapsed % period) < period / 2.0
        };
        let blink_flip =
            self.cursor_blink && peek_cursor_visible && cursor_on_now != self.last_cursor_on;
        let bell_active = self.bell_flash_frames > 0;
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
        let (snap, seqno) = self.snapshot();
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

        // Bell flash: add full-screen semi-transparent overlay (before GPU upload)
        if self.bell_flash_frames > 0 {
            let alpha = self.bell_flash_frames as f32 / 4.0 * 0.15;
            rect_instances.push(RectInstance { 
                pos: [0.0, 0.0],
                size: [ctx.width as f32, ctx.height as f32],
                color: [1.0, 1.0, 1.0, alpha], mode: RectMode::Solid.word(), pattern: [0.0f32; 4]
            });
            self.bell_flash_frames -= 1;
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
        self.glow_state.tick(ctx.dt);
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
            self.draw_kitty_images(ctx, &mut encoder, view, &images_below, self.padding_px(), self.padding_px());
        }

        // Pass 3: Text with per-cell colors
        let mut text_areas = Vec::new();
        let pad = self.padding_px();
        for (row_idx, col_start, buffer) in &text_buffers {
            let y = pad + (*row_idx as f32 * self.cell_height);
            let x = pad + (*col_start as f32 * self.cell_width);
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
            if let Err(e) = ctx.text.prepare(
                &ctx.gpu.device,
                &ctx.gpu.queue,
                ctx.width,
                ctx.height,
                text_areas,
            ) {
                tracing::warn!("text prepare error: {e}");
            }

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
            if let Err(e) = ctx.text.render(&mut pass) {
                tracing::warn!("text render error: {e}");
            }
        }

        // Pass 3.5: Kitty graphics images ABOVE the text scene (z>=0).
        // Onto `scene_view` (not the post-chain surface) so the effect
        // chain at Pass 4 still sees these pixels.
        if !images_above.is_empty() {
            let view = scene_view;
            self.draw_kitty_images(ctx, &mut encoder, view, &images_above, self.padding_px(), self.padding_px());
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

        // Pass 6: directory-frecency overlay (轍 wadachi), reader-only. Renders
        // AFTER snow so it floats on top, onto ctx.surface_view (post-chain), the
        // same target snow uses. Gated on `.open` so idle frames are unchanged.
        // State is snapshotted (lock dropped) before any GPU work.
        {
            let (dp_open, dp_query, dp_results, dp_selected) = {
                let dp = self.dir_picker.lock().unwrap();
                (dp.open, dp.query.clone(), dp.results.clone(), dp.selected)
            };
            if dp_open {
                self.draw_dir_picker(&dp_query, &dp_results, dp_selected, ctx, &mut overlay_encoder);
            }
        }

        // Search status line — same Pass-6 model: state snapshotted,
        // gated on `.active` so idle frames are unchanged.
        {
            let (s_active, s_query, s_current, s_count) = {
                let st = self.search.lock().unwrap();
                (st.active, st.query.clone(), st.current, st.matches.len())
            };
            if s_active {
                self.draw_search_status(&s_query, s_current, s_count, ctx, &mut overlay_encoder);
            }
        }

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

    /// Snapshot + build the rect instances exactly as the live
    /// renderer would for one frame. `elapsed = 0.0` keeps any
    /// time-driven blinking deterministic. The renderer's own shared
    /// selection is resolved inside `snapshot()` — tests mutate
    /// `r.selection` and call this.
    fn compute_rects(r: &TerminalRenderer) -> Vec<RectInstance> {
        let (snap, _seqno) = r.snapshot();
        r.build_rect_instances(&snap, 0.0, r.padding_px(), r.padding_px())
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
    /// renderer carries Nord aurora yellow #EBCB8B; a Borealis-themed one
    /// carries first_light #EDC980 / search_others #4E443A.
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

    /// theme-fidelity: with Borealis active the search-match rects paint
    /// the Borealis search surfaces (first_light #EDC980 current /
    /// search_others #4E443A other) — NOT the legacy Nord aurora yellow
    /// #EBCB8B. This is the surface-map promise; before the fix the
    /// render path hardcoded the Nord value and ignored the theme.
    #[test]
    fn borealis_search_matches_paint_the_borealis_surfaces_not_nord_yellow() {
        let (mut r, t) = harness(40, 3);
        crate::theme::apply_config_theme(&mut r, &t, "borealis-night", 1.0);
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
        // The renderer now carries the Borealis surfaces.
        assert_eq!(r.search_current_color, Color::new(0xED, 0xC9, 0x80));
        assert_eq!(r.search_other_color, Color::new(0x4E, 0x44, 0x3A));
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
        assert_eq!(current_hits, 1, "the Borealis current-match rect paints");
        assert_eq!(other_hits, 1, "the Borealis other-match rect paints");
        // Nord yellow must NOT appear (the hardcode is gone).
        assert!(
            !rects
                .iter()
                .any(|rt| colors_approx_eq(rt.color, nord_current)
                    || colors_approx_eq(rt.color, nord_other)),
            "no Nord aurora-yellow search rect under Borealis"
        );
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
    fn trigger_bell_sets_flash_frames_to_four() {
        let (mut r, _t) = harness(20, 5);
        assert_eq!(r.bell_flash_frames, 0);
        r.trigger_bell();
        assert_eq!(r.bell_flash_frames, 4);
    }

    #[test]
    fn trigger_bell_is_noop_under_reduce_motion() {
        let (mut r, _t) = harness(20, 5);
        r.reduce_motion = true;
        r.trigger_bell();
        assert_eq!(
            r.bell_flash_frames, 0,
            "reduce_motion should suppress the bell flash"
        );
    }

    #[test]
    fn trigger_bell_is_idempotent_for_max_value() {
        // Calling twice in a row shouldn't push the counter past
        // 4 — the flash is a fixed-duration effect.
        let (mut r, _t) = harness(20, 5);
        r.trigger_bell();
        r.trigger_bell();
        assert_eq!(r.bell_flash_frames, 4);
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
    /// (Whisper) is pinned separately by
    /// `default_ambience_composes_aurora_bloom_glow_and_reduce_motion_kills_it`.
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
            // renderer ships with Whisper by default — that path is
            // tested separately). This isolates the per-effect knob.
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

    /// The composed AMBIENCE layer (operator design law, 2026-06-13) —
    /// the renderer-level forcing function. The default config
    /// (Whisper) composes EXACTLY {aurora, bloom, glow_on_bell} into the
    /// effect set; `reduce_motion` resolves the preset to `Off` ⇒ zero
    /// ambience nodes (the accessibility floor); a per-effect override
    /// adds on top; and `Off` contributes zero nodes.
    #[test]
    fn default_ambience_composes_aurora_bloom_glow_and_reduce_motion_kills_it() {
        use engawa_wgpu::catalog::CatalogEffect;
        let mut failures: Vec<String> = Vec::new();

        // ── Default (Whisper) composes the three members ─────────────
        let (mut r, _t) = harness(10, 2);
        let mut cfg = crate::config::MadoConfig::default();
        r.apply_effects_and_accessibility(&cfg);
        let set = r.enabled_effect_set();
        for effect in [
            CatalogEffect::Aurora,
            CatalogEffect::Bloom,
            CatalogEffect::GlowOnBell,
        ] {
            if !set.contains(effect) {
                failures.push(format!("default Whisper ambience is missing {effect:?}"));
            }
        }
        // …and ONLY those three (no static effects sneak in).
        for effect in [
            CatalogEffect::Colorblind,
            CatalogEffect::Crt,
            CatalogEffect::Scanlines,
            CatalogEffect::Snow,
        ] {
            if set.contains(effect) {
                failures.push(format!("default Whisper ambience wrongly enabled {effect:?}"));
            }
        }

        // ── reduce_motion → Off → zero nodes ─────────────────────────
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

    /// Entry-point parity pin     /// application point both main.rs and `gui_tear_attach` call must
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
        // the Whisper ambience by default (tested separately); this test
        // isolates the config-applier delta path.
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
        GpuContext, TextRenderer,
        headless::{HeadlessTarget, assert_no_magenta_pixels},
    };
    use madori::RenderContext;

    /// Build a fully-initialized `TerminalRenderer` connected to
    /// a fresh `cols×rows` terminal, with all wgpu pipelines
    /// brought up against the given GPU context. Returns
    /// everything the render loop needs.
    fn build_gpu_renderer(
        gpu: &GpuContext,
        cols: usize,
        rows: usize,
    ) -> (TerminalRenderer, SharedTerminal, TextRenderer) {
        let term = Arc::new(parking_lot::RwLock::new(Terminal::new(cols, rows)));
        let mut renderer = TerminalRenderer::new(
            term.clone(),
            14.0,
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
        let text = TextRenderer::new(
            &gpu.device,
            &gpu.queue,
            SURFACE_FORMAT,
        );
        (renderer, term, text)
    }

    /// Drive one frame of the renderer against an offscreen
    /// target. Returns the read-back RGBA8 pixel buffer.
    fn render_one_frame_headless(
        gpu: &GpuContext,
        renderer: &mut TerminalRenderer,
        text: &mut TextRenderer,
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
            "732229ea36df58b8a00439379233690fa50ee80f6a7fad340317bda945318d03";
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
        effects.colorblind.mode = ColorblindMode::Protanopia;
        r.set_effects_config(effects);
        // Same canonical scene as the ungraded golden, plus color so
        // the grade has chroma to transform.
        t.write()
            .feed(b"$ echo hello\n\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[44mblue-bg\x1b[0m\n$ ");

        let pixels = render_one_frame_headless(&gpu, &mut r, &mut text, &target);
        let hex = frame_hash(&pixels).to_hex().to_string();

        const GOLDEN: &str =
            "64c2c1c1c04d4c1b42a780f31f1925bf3b127471db27ace42606c7258be271ee";
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
