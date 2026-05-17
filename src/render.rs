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
use crate::pane::PaneRect;
use crate::search::SearchState;
use crate::selection::Selection;
use crate::terminal::{bold_bright_color, default_ansi_palette, Cell, CellAttrs, Color, Cursor, ImagePlacement, Terminal};
use crate::url::{self, DetectedUrl};
use crate::window::WindowState;

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

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct RectInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
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
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
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
    return out;
}

@fragment
fn fs_main(frag: VertexOutput) -> @location(0) vec4<f32> {
    return frag.color;
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
// Post-processing shader pipeline (custom WGSL + accessibility modes)
// ---------------------------------------------------------------------------

/// Built-in accessibility shader: colorblind simulation.
/// Uses Machado 2009 color vision deficiency simulation matrices.
const COLORBLIND_SHADER: &str = r"
@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var input_samp: sampler;
@group(0) @binding(2) var<uniform> params: PostParams;

struct PostParams {
    resolution: vec2<f32>,
    time: f32,
    mode: u32,  // 0=none, 1=protanopia, 2=deuteranopia, 3=tritanopia
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    let corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0),
    );
    let c = corners[vi];
    var out: VsOut;
    out.position = vec4(c.x * 2.0 - 1.0, 1.0 - c.y * 2.0, 0.0, 1.0);
    out.uv = c;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let color = textureSample(input_tex, input_samp, in.uv);

    if params.mode == 0u { return color; }

    let r = color.r; let g = color.g; let b = color.b;
    var out_r: f32; var out_g: f32; var out_b: f32;

    // Machado et al. 2009 simulation matrices (severity = 1.0)
    if params.mode == 1u {
        // Protanopia (red-blind)
        out_r = 0.152286 * r + 1.052583 * g - 0.204868 * b;
        out_g = 0.114503 * r + 0.786281 * g + 0.099216 * b;
        out_b = -0.003882 * r - 0.048116 * g + 1.051998 * b;
    } else if params.mode == 2u {
        // Deuteranopia (green-blind)
        out_r = 0.367322 * r + 0.860646 * g - 0.227968 * b;
        out_g = 0.280085 * r + 0.672501 * g + 0.047413 * b;
        out_b = -0.011820 * r + 0.042940 * g + 0.968881 * b;
    } else {
        // Tritanopia (blue-blind)
        out_r = 1.255528 * r - 0.076749 * g - 0.178779 * b;
        out_g = -0.078411 * r + 0.930809 * g + 0.147602 * b;
        out_b = 0.004733 * r + 0.691367 * g + 0.303900 * b;
    }

    return vec4(clamp(out_r, 0.0, 1.0), clamp(out_g, 0.0, 1.0), clamp(out_b, 0.0, 1.0), color.a);
}
";

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct PostParams {
    resolution: [f32; 2],
    time: f32,
    mode: u32,
}

struct PostProcessPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    params_buffer: wgpu::Buffer,
    sampler: wgpu::Sampler,
    offscreen_texture: Option<wgpu::Texture>,
    offscreen_view: Option<wgpu::TextureView>,
    bind_group: Option<wgpu::BindGroup>,
    last_width: u32,
    last_height: u32,
}

impl PostProcessPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("postprocess_shader"),
            source: wgpu::ShaderSource::Wgsl(COLORBLIND_SHADER.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("postprocess_bgl"),
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("postprocess_pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("postprocess_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
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
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("postprocess_params"),
            size: std::mem::size_of::<PostParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("postprocess_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            params_buffer,
            sampler,
            offscreen_texture: None,
            offscreen_view: None,
            bind_group: None,
            last_width: 0,
            last_height: 0,
        }
    }

    /// Ensure offscreen texture matches current window size.
    fn ensure_offscreen(&mut self, device: &wgpu::Device, width: u32, height: u32, format: wgpu::TextureFormat) {
        if self.last_width == width && self.last_height == height && self.offscreen_texture.is_some()
        {
            return;
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("postprocess_offscreen"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("postprocess_bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.params_buffer.as_entire_binding(),
                },
            ],
        });

        self.offscreen_texture = Some(texture);
        self.offscreen_view = Some(view);
        self.bind_group = Some(bind_group);
        self.last_width = width;
        self.last_height = height;
    }
}

// ---------------------------------------------------------------------------
// Render snapshot — cloned terminal state for lock-free rendering
// ---------------------------------------------------------------------------

struct Snapshot {
    rows: Vec<Vec<Cell>>,
    cursor: Cursor,
    cols: usize,
    num_rows: usize,
    urls: Vec<DetectedUrl>,
    search_active: bool,
    search_matches: Vec<crate::search::SearchMatch>,
    search_current: usize,
    image_placements: Vec<ImagePlacement>,
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
/// math to apply when flushing an open RLE span. The three kinds are
/// the only per-row rect kinds whose pixel geometry can be described
/// as "start_col × cell_width wide, on row_idx" — cell backgrounds
/// fill the whole cell height, underlines sit two pixels above the
/// bottom edge, strikethroughs sit at mid-cell. Box-drawing rects
/// have per-glyph shapes and stay per-cell.
#[derive(Clone, Copy)]
enum RectKindForRle {
    Background,
    Underline,
    Strikethrough,
}

// ---------------------------------------------------------------------------
// TerminalRenderer
// ---------------------------------------------------------------------------

pub struct TerminalRenderer {
    terminal: SharedTerminal,
    selection: Arc<Mutex<Selection>>,
    search: Arc<Mutex<SearchState>>,
    /// Multi-pane window state. When set, overrides single-terminal rendering.
    window: Option<Arc<Mutex<WindowState>>>,
    font_size: f32,
    font_family: String,
    /// Italic-face family. cosmic-text resolves italics by walking
    /// the fontdb for `Style::Italic`; pinning the family explicitly
    /// lets mado route italic cells to a calligraphic alternative
    /// (Iosevka Etoile, Maple Mono Italic, etc.) regardless of which
    /// family `font_family` names.
    font_italic: String,
    cell_width: f32,
    cell_height: f32,
    padding: f32,
    bg_color: wgpu::Color,
    fg_color: Color,
    ansi_colors: [Color; 16],
    rect_pipeline: Option<RectPipeline>,
    image_pipeline: Option<ImagePipeline>,
    post_pipeline: Option<PostProcessPipeline>,
    gpu_images: HashMap<u32, GpuImage>,
    colorblind_mode: ColorblindMode,
    bold_is_bright: bool,
    last_seqno: u64,
    cursor_style: CursorStyle,
    cursor_blink: bool,
    cursor_blink_rate_ms: u32,
    metrics_measured: bool,
    /// Bell visual flash — remaining frames to show.
    bell_flash_frames: u8,
    /// Selection highlight background (RGBA).
    selection_bg: [f32; 4],
    /// Cursor color (RGBA).
    cursor_color: [f32; 4],
    /// Reduce motion: disable cursor blink and bell flash.
    reduce_motion: bool,
    /// HiDPI scale factor (1.0 on non-Retina, 2.0 on most Mac Retina,
    /// other values on Linux/Windows). Multiplies font_size and padding
    /// before they touch the GPU pipeline — the wgpu surface is sized
    /// in physical pixels, so all draw positions / cell metrics must
    /// be physical too, otherwise the rendered content only covers a
    /// `1/scale_factor`-sized chunk of the window. Refreshed each
    /// frame from `RenderContext::scale_factor`.
    scale_factor: f32,
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
            window: None,
            font_size,
            font_family,
            font_italic,
            cell_width,
            cell_height,
            padding,
            bg_color,
            fg_color,
            ansi_colors: default_ansi_palette(),
            rect_pipeline: None,
            image_pipeline: None,
            post_pipeline: None,
            gpu_images: HashMap::new(),
            colorblind_mode: ColorblindMode::None,
            bold_is_bright: false,
            last_seqno: 0,
            cursor_style,
            cursor_blink,
            cursor_blink_rate_ms,
            metrics_measured: false,
            bell_flash_frames: 0,
            selection_bg: [0.533, 0.753, 0.816, 0.3], // Nord frost default
            cursor_color: [0.925, 0.937, 0.957, 0.85], // Nord snow default
            reduce_motion: false,
            // 1.0 = no scaling; overwritten on the first render frame
            // by `set_scale_factor(ctx.scale_factor)`.
            scale_factor: 1.0,
            shape_cache: RefCell::new(LruCache::new(
                NonZeroUsize::new(SHAPE_CACHE_CAP)
                    .expect("SHAPE_CACHE_CAP is a non-zero compile-time constant"),
            )),
            last_cursor_on: false,
            box_draw_templates: RefCell::new(HashMap::new()),
            sync_output_deferred_since: None,
        }
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

    /// Physical-pixel font size. Mirrors `padding_px` — logical
    /// `font_size` from config, scaled into physical pixels for the
    /// glyphon font-system + buffer creation.
    #[inline]
    fn font_size_px(&self) -> f32 {
        self.font_size * self.scale_factor
    }

    /// Set selection highlight background (RGBA).
    pub fn set_selection_bg(&mut self, bg: [f32; 4]) {
        self.selection_bg = bg;
        self.last_seqno = 0;
    }

    /// Set cursor color (RGBA).
    pub fn set_cursor_color(&mut self, color: [f32; 4]) {
        self.cursor_color = color;
        self.last_seqno = 0;
    }

    /// Set reduce motion mode (disables cursor blink and bell flash).
    pub fn set_reduce_motion(&mut self, enabled: bool) {
        self.reduce_motion = enabled;
        self.last_seqno = 0;
    }

    /// Set the shared selection state (called from main to share with event handler).
    pub fn set_selection(&mut self, selection: Arc<Mutex<Selection>>) {
        self.selection = selection;
    }

    /// Set the shared search state (called from main to share with event handler).
    pub fn set_search(&mut self, search: Arc<Mutex<SearchState>>) {
        self.search = search;
    }

    /// Set the window state for multi-pane rendering.
    pub fn set_window(&mut self, window: Arc<Mutex<WindowState>>) {
        self.window = Some(window);
    }

    /// Trigger a bell flash effect. No-op when reduce_motion is enabled.
    pub fn trigger_bell(&mut self) {
        if !self.reduce_motion {
            self.bell_flash_frames = 4;
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

    /// Set the colorblind simulation mode for accessibility.
    pub fn set_colorblind_mode(&mut self, mode: ColorblindMode) {
        self.colorblind_mode = mode;
        self.last_seqno = 0; // force redraw
    }

    /// Set whether bold text uses bright colors (ANSI 0-7 → 8-15).
    pub fn set_bold_is_bright(&mut self, enabled: bool) {
        self.bold_is_bright = enabled;
        self.last_seqno = 0; // force redraw
    }

    /// Set the 16-color ANSI palette used for bold-as-bright resolution.
    pub fn set_ansi_colors(&mut self, colors: [Color; 16]) {
        self.ansi_colors = colors;
        self.last_seqno = 0; // force redraw
    }

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
        let term = self.terminal.read();
        let seqno = term.seqno();
        let cursor = *term.cursor();
        let cols = term.cols();
        let num_rows = term.rows();
        let on_alt = term.on_alt_screen();
        let rows: Vec<Vec<Cell>> = term.visible_rows().map(|r| r.to_vec()).collect();
        let image_placements = term.image_placements().to_vec();
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
                cursor,
                cols,
                num_rows,
                urls,
                search_active,
                search_matches,
                search_current,
                image_placements,
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
        sel: &Selection,
    ) -> Vec<RectInstance> {
        // P23 — pre-size by expected rect-instance count. Typical
        // interactive grid produces 2–4 spans per row (background,
        // optional underline, occasional strikethrough). 4 × rows is
        // a safe upper estimate; +cells for selection / search /
        // URLs spans.
        let mut instances = Vec::with_capacity(snap.num_rows * 4 + snap.cols);
        let default_bg = Color::BLACK;

        // P11 — run-length batch every per-row "single-row, same-color
        // wide span" rect kind: backgrounds, underlines, strikethroughs.
        // Adjacent cells with identical (bg) or identical (underline +
        // fg) collapse into ONE wide RectInstance. On a typical
        // interactive grid this cuts the rect-pipeline upload from a
        // potential cells × 4 kinds per row down to ~2–10 spans per row
        // — and the rect_pipeline does an instanced draw call sized by
        // instance count, so fewer instances = smaller upload + smaller
        // vertex-shader cost. Box drawing stays per-cell (each glyph
        // has its own shape; no run shape exists).
        //
        // Per-row state for the three RLE-able kinds. Each is `Option<
        // (start_col, run_width_cells, color)>`; `None` = no run open.
        // `run_width_cells` accumulates by cell.width so wide chars
        // (CJK / emoji) contribute 2 cells to the span — the painted
        // rect ends up `run_width_cells × cell_width` wide.
        type RowRun = Option<(usize, usize, [f32; 4])>;
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
                        RectKindForRle::Underline => (
                            origin_y + (row_idx as f32 + 1.0) * self.cell_height - 2.0,
                            1.0,
                        ),
                        RectKindForRle::Strikethrough => (
                            origin_y + row_idx as f32 * self.cell_height
                                + self.cell_height * 0.5,
                            1.0,
                        ),
                    };
                    instances.push(RectInstance {
                        pos: [x, y],
                        size: [w, h],
                        color,
                    });
                }
            };

        for (row_idx, row) in snap.rows.iter().enumerate() {
            let mut bg_run: RowRun = None;
            let mut underline_run: RowRun = None;
            let mut strike_run: RowRun = None;

            for (col_idx, cell) in row.iter().enumerate().take(snap.cols) {
                // Continuation cells: don't break or extend the run on
                // their own — the wide-glyph cell already booked 2 cells
                // worth of width when it joined. Skip without flushing.
                if cell.width == 0 {
                    continue;
                }

                let inverse = cell.attrs.contains(CellAttrs::INVERSE);
                let dim = cell.attrs.contains(CellAttrs::DIM);
                let bg = if inverse { cell.fg } else { cell.bg };
                let base_fg = if inverse { cell.bg } else { cell.fg };
                let fg = if dim {
                    Color::new(base_fg.r / 2, base_fg.g / 2, base_fg.b / 2)
                } else {
                    base_fg
                };
                let width_cells = cell.width.max(1) as usize;

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
                if cell.attrs.contains(CellAttrs::UNDERLINE) {
                    let color = color_to_f32(&fg);
                    match &mut underline_run {
                        Some((_, cells, c)) if *c == color => {
                            *cells += width_cells;
                        }
                        _ => {
                            push_run(
                                &mut instances,
                                &mut underline_run,
                                row_idx,
                                RectKindForRle::Underline,
                            );
                            underline_run = Some((col_idx, width_cells, color));
                        }
                    }
                } else {
                    push_run(
                        &mut instances,
                        &mut underline_run,
                        row_idx,
                        RectKindForRle::Underline,
                    );
                }

                // ── Strikethrough span ──────────────────────────────
                if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
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
                            color,
                        });
                    }
                }
            }

            // Row end — flush every open run.
            push_run(&mut instances, &mut bg_run, row_idx, RectKindForRle::Background);
            push_run(&mut instances, &mut underline_run, row_idx, RectKindForRle::Underline);
            push_run(&mut instances, &mut strike_run, row_idx, RectKindForRle::Strikethrough);
        }

        // Selection highlight — RLE'd. Selection spans are almost
        // always contiguous within a row (a triple-click line, a
        // drag selection from col A to col B); per-cell rects were
        // pure waste.
        if sel.is_active() {
            for row_idx in 0..snap.rows.len() {
                let mut run_start: Option<usize> = None;
                for col_idx in 0..snap.cols {
                    if sel.contains(row_idx, col_idx) {
                        if run_start.is_none() {
                            run_start = Some(col_idx);
                        }
                    } else if let Some(start) = run_start.take() {
                        instances.push(RectInstance {
                            pos: [
                                origin_x + start as f32 * self.cell_width,
                                origin_y + row_idx as f32 * self.cell_height,
                            ],
                            size: [
                                (col_idx - start) as f32 * self.cell_width,
                                self.cell_height,
                            ],
                            color: self.selection_bg,
                        });
                    }
                }
                if let Some(start) = run_start {
                    instances.push(RectInstance {
                        pos: [
                            origin_x + start as f32 * self.cell_width,
                            origin_y + row_idx as f32 * self.cell_height,
                        ],
                        size: [
                            (snap.cols - start) as f32 * self.cell_width,
                            self.cell_height,
                        ],
                        color: self.selection_bg,
                    });
                }
            }
        }

        // Search match highlights — RLE'd (one rect per match span).
        if snap.search_active {
            for (i, m) in snap.search_matches.iter().enumerate() {
                let is_current = i == snap.search_current;
                let color = if is_current {
                    [0.922, 0.796, 0.545, 0.5] // Nord aurora yellow
                } else {
                    [0.922, 0.796, 0.545, 0.2] // Dimmer yellow
                };
                instances.push(RectInstance {
                    pos: [
                        origin_x + m.col_start as f32 * self.cell_width,
                        origin_y + m.row as f32 * self.cell_height,
                    ],
                    size: [
                        (m.col_end + 1 - m.col_start) as f32 * self.cell_width,
                        self.cell_height,
                    ],
                    color,
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
                // Nord frost blue underline
                color: [0.533, 0.753, 0.816, 0.6],
            });
        }

        // Cursor (with optional blink)
        let cursor_on = !self.cursor_blink || {
            let period = self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0;
            (elapsed % period) < period / 2.0
        };

        if snap.cursor.visible
            && cursor_on
            && snap.cursor.row < snap.num_rows
            && snap.cursor.col < snap.cols
        {
            let cx = origin_x + snap.cursor.col as f32 * self.cell_width;
            let cy = origin_y + snap.cursor.row as f32 * self.cell_height;

            let (pos, size) = match self.cursor_style {
                CursorStyle::Block => ([cx, cy], [self.cell_width, self.cell_height]),
                CursorStyle::BlockHollow => ([cx, cy], [self.cell_width, self.cell_height]),
                CursorStyle::Bar => ([cx, cy], [2.0, self.cell_height]),
                CursorStyle::Underline => (
                    [cx, cy + self.cell_height - 2.0],
                    [self.cell_width, 2.0],
                ),
            };

            if self.cursor_style == CursorStyle::BlockHollow {
                let thickness = 2.0_f32;
                instances.push(RectInstance { pos: [cx, cy], size: [self.cell_width, thickness], color: self.cursor_color });
                instances.push(RectInstance { pos: [cx, cy + self.cell_height - thickness], size: [self.cell_width, thickness], color: self.cursor_color });
                instances.push(RectInstance { pos: [cx, cy], size: [thickness, self.cell_height], color: self.cursor_color });
                instances.push(RectInstance { pos: [cx + self.cell_width - thickness, cy], size: [thickness, self.cell_height], color: self.cursor_color });
            } else {
                instances.push(RectInstance {
                    pos,
                    size,
                    color: self.cursor_color,
                });
            }
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
        let family = if key.attrs.italic {
            Family::Name(&self.font_italic)
        } else {
            Family::Name(&self.font_family)
        };
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

                let inverse = cell.attrs.contains(CellAttrs::INVERSE);
                let bold = cell.attrs.contains(CellAttrs::BOLD);
                let dim = cell.attrs.contains(CellAttrs::DIM);
                let italic = cell.attrs.contains(CellAttrs::ITALIC);
                let hidden = cell.attrs.contains(CellAttrs::HIDDEN);

                let effective_fg = if hidden {
                    if inverse { cell.fg } else { cell.bg }
                } else {
                    let mut fg = if inverse {
                        cell.bg
                    } else if bold && self.bold_is_bright {
                        bold_bright_color(&cell.fg, &self.ansi_colors)
                    } else {
                        cell.fg
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

    /// Snapshot a specific pane's terminal state (for multi-pane rendering).
    fn snapshot_pane(
        &self,
        terminal: &SharedTerminal,
        search: &Arc<Mutex<SearchState>>,
    ) -> (Snapshot, u64) {
        let term = terminal.read();
        let seqno = term.seqno();
        let cursor = *term.cursor();
        let cols = term.cols();
        let num_rows = term.rows();
        let on_alt = term.on_alt_screen();
        let rows: Vec<Vec<Cell>> = term.visible_rows().map(|r| r.to_vec()).collect();
        let image_placements = term.image_placements().to_vec();
        drop(term);

        // P24 — alt-screen panes also skip URL detection.
        let urls = if on_alt {
            Vec::new()
        } else {
            url::detect_urls(&rows, cols)
        };

        let search = search.lock().unwrap();
        let search_active = search.active;
        let search_matches = search.matches.clone();
        let search_current = search.current;
        drop(search);

        (
            Snapshot {
                rows,
                cursor,
                cols,
                num_rows,
                urls,
                search_active,
                search_matches,
                search_current,
                image_placements,
            },
            seqno,
        )
    }

    /// Multi-pane render path — renders all panes from WindowState.
    ///
    /// P17 — analog of P2's peek path for multi-pane. Before doing
    /// the per-pane snapshot (which clones every visible row and
    /// runs URL detection per pane), XOR all panes' seqnos and any
    /// cursor-visible bit into a single u64 fingerprint. If the
    /// fingerprint matches the last frame AND nothing is animating,
    /// early-return.
    ///
    /// XOR is associative + commutative so the order doesn't matter,
    /// and collision probability for "real change in any pane" is
    /// vanishingly small at u64 granularity. The pane lock is held
    /// only for the seqno + cursor.visible read per pane — micro-
    /// seconds total even on 8-pane layouts.
    fn render_multi_pane(&mut self, ctx: &mut RenderContext<'_>) {
        let window = self.window.clone().unwrap();
        let ws = window.lock().unwrap();
        let pad = self.padding_px();
        let pane_rects = ws.layout(
            pad,
            pad,
            ctx.width as f32 - 2.0 * pad,
            ctx.height as f32 - 2.0 * pad,
        );
        let focused_id = ws.focused_pane_id();
        let pane_count = pane_rects.len();

        // Stage-1 peek: fingerprint = XOR of all panes' seqnos with
        // each pane's cursor-visible bit folded in. Identical
        // fingerprint + no animations + no search/bell → skip frame.
        let mut fingerprint: u64 = 0;
        let mut any_cursor_visible = false;
        for rect in &pane_rects {
            if let Some(pane) = ws.pane(&rect.id) {
                let term = pane.terminal.read();
                let seqno = term.seqno();
                let cur = *term.cursor();
                drop(term);
                fingerprint ^= seqno;
                if cur.visible {
                    fingerprint ^= 1u64.rotate_left((rect.id.0 % 64) as u32);
                    any_cursor_visible = true;
                }
            }
        }
        // P28 — same cursor-on flip detection as the single-pane
        // path. Only force a render if blink actually changes state
        // this frame; the idle steady state runs at the blink toggle
        // rate (~4 Hz), not the vsync rate (60 Hz).
        let cursor_on_now = !self.cursor_blink || {
            let period = self.cursor_blink_rate_ms as f32 / 1000.0 * 2.0;
            (ctx.elapsed % period) < period / 2.0
        };
        let blink_flip =
            self.cursor_blink && any_cursor_visible && cursor_on_now != self.last_cursor_on;
        let bell_active = self.bell_flash_frames > 0;
        if fingerprint == self.last_seqno
            && self.last_seqno != 0
            && !blink_flip
            && !bell_active
        {
            TOTAL_FRAMES_SKIPPED.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                fingerprint,
                path = "multi_pane_skip",
                "frame skipped"
            );
            return;
        }
        self.last_seqno = fingerprint;
        self.last_cursor_on = cursor_on_now;

        let mut all_rects = Vec::new();
        let mut text_entries: Vec<(f32, f32, usize, usize, Arc<Buffer>, PaneRect)> =
            Vec::with_capacity(pane_count * 24 * 8);
        let mut all_image_placements: Vec<(f32, f32, Vec<ImagePlacement>)> = Vec::new();

        for rect in &pane_rects {
            if let Some(pane) = ws.pane(&rect.id) {
                let (snap, _) = self.snapshot_pane(&pane.terminal, &pane.search);
                let sel = pane.selection.lock().unwrap();
                all_rects.extend(self.build_rect_instances(
                    &snap,
                    ctx.elapsed,
                    rect.x,
                    rect.y,
                    &sel,
                ));
                drop(sel);
                for (row_idx, col_start, buf) in self.build_text_buffers(&snap, ctx.text) {
                    text_entries.push((rect.x, rect.y, row_idx, col_start, buf, *rect));
                }
                if !snap.image_placements.is_empty() {
                    all_image_placements.push((rect.x, rect.y, snap.image_placements));
                }
            }
        }

        // Pane borders (only when >1 pane)
        if pane_count > 1 {
            for rect in &pane_rects {
                let color = if rect.id == focused_id {
                    [0.533, 0.753, 0.816, 0.6] // Nord frost
                } else {
                    [0.369, 0.396, 0.435, 0.4] // Nord dim
                };
                all_rects.push(RectInstance {
                    pos: [rect.x + rect.width, rect.y],
                    size: [1.0, rect.height],
                    color,
                });
                all_rects.push(RectInstance {
                    pos: [rect.x, rect.y + rect.height],
                    size: [rect.width + 1.0, 1.0],
                    color,
                });
            }
        }

        drop(ws);

        // Bell flash
        if self.bell_flash_frames > 0 {
            let alpha = self.bell_flash_frames as f32 / 4.0 * 0.15;
            all_rects.push(RectInstance {
                pos: [0.0, 0.0],
                size: [ctx.width as f32, ctx.height as f32],
                color: [1.0, 1.0, 1.0, alpha],
            });
            self.bell_flash_frames -= 1;
        }

        // Upload rect instances
        if let Some(ref mut pipeline) = self.rect_pipeline {
            pipeline.update_resolution(&ctx.gpu.queue, ctx.width, ctx.height);
            pipeline.ensure_capacity(&ctx.gpu.device, all_rects.len());
            if !all_rects.is_empty() {
                ctx.gpu.queue.write_buffer(
                    &pipeline.instance_buffer,
                    0,
                    bytemuck::cast_slice(&all_rects),
                );
            }
        }

        // Build text areas
        let mut text_areas = Vec::new();
        for (left, top_origin, row_idx, col_start, buffer, rect) in &text_entries {
            let y = top_origin + (*row_idx as f32 * self.cell_height);
            let x = *left + (*col_start as f32 * self.cell_width);
            text_areas.push(glyphon::TextArea {
                buffer: &**buffer,
                left: x,
                top: y,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: rect.x as i32,
                    top: rect.y as i32,
                    right: (rect.x + rect.width) as i32,
                    bottom: (rect.y + rect.height) as i32,
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

        // P25 — same skip-if-empty optimisation as the single-pane
        // path. Save the empty flag to gate the later text render
        // pass too.
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
        }

        // Determine post-processing mode
        let colorblind_mode = match self.colorblind_mode {
            ColorblindMode::None => 0u32,
            ColorblindMode::Protanopia => 1,
            ColorblindMode::Deuteranopia => 2,
            ColorblindMode::Tritanopia => 3,
        };
        let use_postprocess = colorblind_mode > 0;

        if use_postprocess {
            if let Some(ref mut post) = self.post_pipeline {
                let format = wgpu::TextureFormat::Bgra8UnormSrgb;
                post.ensure_offscreen(&ctx.gpu.device, ctx.width, ctx.height, format);
            }
        }

        // Sync Kitty GPU textures before render passes
        self.sync_kitty_images(ctx);

        let mut encoder = ctx
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mado_render"),
            });

        macro_rules! scene_view {
            ($self:expr, $ctx:expr) => {
                if use_postprocess {
                    $self
                        .post_pipeline
                        .as_ref()
                        .and_then(|p| p.offscreen_view.as_ref())
                        .unwrap_or($ctx.surface_view)
                } else {
                    $ctx.surface_view
                }
            };
        }

        // Pass 1: Clear background
        {
            let view = scene_view!(self, ctx);
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

        // Pass 2: Rects — skip when no rect instances queued (P27).
        if !all_rects.is_empty() {
            if let Some(ref pipeline) = self.rect_pipeline {
                let view = scene_view!(self, ctx);
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
                pipeline.draw(&mut pass, all_rects.len() as u32);
            }
        }

        // Pass 2.5: Kitty graphics images (per-pane)
        for (ox, oy, placements) in &all_image_placements {
            let view = scene_view!(self, ctx);
            self.draw_kitty_images(ctx, &mut encoder, view, placements, *ox, *oy);
        }

        // Pass 3: Text — skip when there are no glyphs (text_areas
        // empty, e.g. a blank screen or box-draw-only frame).
        if !text_areas_empty {
            let view = scene_view!(self, ctx);
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

        // Pass 4: Post-processing blit (offscreen → surface through shader)
        if use_postprocess {
            if let Some(ref post) = self.post_pipeline {
                let params = PostParams {
                    resolution: [ctx.width as f32, ctx.height as f32],
                    time: ctx.elapsed,
                    mode: colorblind_mode,
                };
                ctx.gpu
                    .queue
                    .write_buffer(&post.params_buffer, 0, bytemuck::bytes_of(&params));

                if let Some(ref bind_group) = post.bind_group {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("mado_postprocess"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: ctx.surface_view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    pass.set_pipeline(&post.pipeline);
                    pass.set_bind_group(0, bind_group, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
        }

        ctx.gpu.queue.submit(std::iter::once(encoder.finish()));
    }
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

/// Check if a character is a box drawing character that we render via rects.
fn is_box_drawing(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{257F}' | '\u{2580}'..='\u{259F}')
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
                color,
            });
        }
        // │ vertical line
        '\u{2502}' => {
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color,
            });
        }
        // ┌ top-left corner
        '\u{250C}' => {
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [cw - (cx - x) + thick / 2.0, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [thick, ch_h - (cy - y) + thick / 2.0],
                color,
            });
        }
        // ┐ top-right corner
        '\u{2510}' => {
            rects.push(RectInstance {
                pos: [x, cy - thick / 2.0],
                size: [cx - x + thick / 2.0, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [thick, ch_h - (cy - y) + thick / 2.0],
                color,
            });
        }
        // └ bottom-left corner
        '\u{2514}' => {
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [cw - (cx - x) + thick / 2.0, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, cy - y + thick / 2.0],
                color,
            });
        }
        // ┘ bottom-right corner
        '\u{2518}' => {
            rects.push(RectInstance {
                pos: [x, cy - thick / 2.0],
                size: [cx - x + thick / 2.0, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, cy - y + thick / 2.0],
                color,
            });
        }
        // ├ left tee
        '\u{251C}' => {
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [cw - (cx - x) + thick / 2.0, thick],
                color,
            });
        }
        // ┤ right tee
        '\u{2524}' => {
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color,
            });
            rects.push(RectInstance {
                pos: [x, cy - thick / 2.0],
                size: [cx - x + thick / 2.0, thick],
                color,
            });
        }
        // ┬ top tee
        '\u{252C}' => {
            rects.push(RectInstance {
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, cy - thick / 2.0],
                size: [thick, ch_h - (cy - y) + thick / 2.0],
                color,
            });
        }
        // ┴ bottom tee
        '\u{2534}' => {
            rects.push(RectInstance {
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, cy - y + thick / 2.0],
                color,
            });
        }
        // ┼ cross
        '\u{253C}' => {
            rects.push(RectInstance {
                pos: [x, cy - thick / 2.0],
                size: [cw, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [cx - thick / 2.0, y],
                size: [thick, ch_h],
                color,
            });
        }
        // ═ double horizontal
        '\u{2550}' => {
            let gap = thick;
            rects.push(RectInstance {
                pos: [x, cy - thick - gap / 2.0],
                size: [cw, thick],
                color,
            });
            rects.push(RectInstance {
                pos: [x, cy + gap / 2.0],
                size: [cw, thick],
                color,
            });
        }
        // ║ double vertical
        '\u{2551}' => {
            let gap = thick;
            rects.push(RectInstance {
                pos: [cx - thick - gap / 2.0, y],
                size: [thick, ch_h],
                color,
            });
            rects.push(RectInstance {
                pos: [cx + gap / 2.0, y],
                size: [thick, ch_h],
                color,
            });
        }
        // Block elements
        // ▀ upper half block
        '\u{2580}' => {
            rects.push(RectInstance {
                pos: [x, y],
                size: [cw, ch_h / 2.0],
                color,
            });
        }
        // ▄ lower half block
        '\u{2584}' => {
            rects.push(RectInstance {
                pos: [x, y + ch_h / 2.0],
                size: [cw, ch_h / 2.0],
                color,
            });
        }
        // █ full block
        '\u{2588}' => {
            rects.push(RectInstance {
                pos: [x, y],
                size: [cw, ch_h],
                color,
            });
        }
        // ▌ left half block
        '\u{258C}' => {
            rects.push(RectInstance {
                pos: [x, y],
                size: [cw / 2.0, ch_h],
                color,
            });
        }
        // ▐ right half block
        '\u{2590}' => {
            rects.push(RectInstance {
                pos: [x + cw / 2.0, y],
                size: [cw / 2.0, ch_h],
                color,
            });
        }
        // ░ light shade
        '\u{2591}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.25;
            rects.push(RectInstance {
                pos: [x, y],
                size: [cw, ch_h],
                color: shade_color,
            });
        }
        // ▒ medium shade
        '\u{2592}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.5;
            rects.push(RectInstance {
                pos: [x, y],
                size: [cw, ch_h],
                color: shade_color,
            });
        }
        // ▓ dark shade
        '\u{2593}' => {
            let mut shade_color = color;
            shade_color[3] *= 0.75;
            rects.push(RectInstance {
                pos: [x, y],
                size: [cw, ch_h],
                color: shade_color,
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

        image_draws.sort_by_key(|(id, _)| *id);

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

impl RenderCallback for TerminalRenderer {
    fn init(&mut self, gpu: &garasu::GpuContext) {
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        self.rect_pipeline = Some(RectPipeline::new(&gpu.device, format));
        self.image_pipeline = Some(ImagePipeline::new(&gpu.device, format));
        self.post_pipeline = Some(PostProcessPipeline::new(&gpu.device, format));
    }

    fn render(&mut self, ctx: &mut RenderContext<'_>) {
        // P19 — frame-timing instrumentation. Each phase records its
        // elapsed time so operators can capture render-path breakdowns
        // via `RUST_LOG=mado::render=debug` without recompiling. The
        // `tracing::debug!` macros compile to ~5 ns NOPs when the
        // level is disabled (default), so this is free in normal runs.
        let frame_start = Instant::now();

        // Pull the live HiDPI scale factor in first. If it changed, the
        // setter clears `metrics_measured` so `measure_cell_metrics`
        // below re-measures glyph widths in the new pixel density.
        // This is the load-bearing fix for "rendered content only fills
        // 1/scale_factor of the window" on Retina displays.
        self.set_scale_factor(ctx.scale_factor as f32);

        // Measure actual font metrics on first render (or after a
        // scale-factor change).
        self.measure_cell_metrics(ctx.text);

        // Multi-pane path: render all panes from WindowState
        if self.window.is_some() {
            self.render_multi_pane(ctx);
            let frame_us = frame_start.elapsed().as_micros() as u64;
            LAST_FRAME_US.store(frame_us, Ordering::Relaxed);
            TOTAL_FRAMES.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(frame_us, path = "multi_pane", "frame complete");
            return;
        }

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
        if peek_seqno == self.last_seqno
            && self.last_seqno != 0
            && !blink_flip
            && !bell_active
            && !search_active_peek
        {
            TOTAL_FRAMES_SKIPPED.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                peek_us = frame_start.elapsed().as_micros() as u64,
                path = "idle_peek_skip",
                "frame skipped"
            );
            return;
        }

        let snapshot_start = Instant::now();
        let (snap, seqno) = self.snapshot();
        let snapshot_us = snapshot_start.elapsed().as_micros() as u64;
        // Memoise cursor_on for the next-frame peek's flip detection.
        self.last_cursor_on = cursor_on_now;

        // Stage-2 gate (rare: peek seqno can shift between the peek
        // and the snapshot; keep the safety net so we never paint a
        // stale frame).
        let blink_active = self.cursor_blink && snap.cursor.visible;
        if seqno == self.last_seqno
            && self.last_seqno != 0
            && !blink_active
            && !bell_active
            && !snap.search_active
        {
            return;
        }
        self.last_seqno = seqno;

        // Build rect instances (cell backgrounds + cursor + decorations)
        let rects_start = Instant::now();
        let sel = self.selection.lock().unwrap();
        let mut rect_instances =
            self.build_rect_instances(&snap, ctx.elapsed, self.padding_px(), self.padding_px(), &sel);
        drop(sel);
        let rects_us = rects_start.elapsed().as_micros() as u64;
        let rects_count = rect_instances.len();

        // Bell flash: add full-screen semi-transparent overlay (before GPU upload)
        if self.bell_flash_frames > 0 {
            let alpha = self.bell_flash_frames as f32 / 4.0 * 0.15;
            rect_instances.push(RectInstance {
                pos: [0.0, 0.0],
                size: [ctx.width as f32, ctx.height as f32],
                color: [1.0, 1.0, 1.0, alpha],
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
        let text_buffers = self.build_text_buffers(&snap, ctx.text);
        let text_us = text_start.elapsed().as_micros() as u64;
        let text_count = text_buffers.len();
        let shape_cache_len = self.shape_cache.borrow().len();

        // Determine post-processing mode
        let colorblind_mode = match self.colorblind_mode {
            ColorblindMode::None => 0u32,
            ColorblindMode::Protanopia => 1,
            ColorblindMode::Deuteranopia => 2,
            ColorblindMode::Tritanopia => 3,
        };
        let use_postprocess = colorblind_mode > 0;

        // When post-processing is active, render scene to offscreen texture,
        // then blit to surface through the shader. Otherwise render to surface directly.
        if use_postprocess {
            if let Some(ref mut post) = self.post_pipeline {
                let format = wgpu::TextureFormat::Bgra8UnormSrgb;
                post.ensure_offscreen(&ctx.gpu.device, ctx.width, ctx.height, format);
            }
        }

        // Sync Kitty GPU textures (mutable borrow) before we start render passes.
        self.sync_kitty_images(ctx);

        let mut encoder = ctx
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mado_render"),
            });

        // Helper macro to resolve the render target for each pass.
        // When post-processing is active, all scene passes target the offscreen texture.
        macro_rules! scene_view {
            ($self:expr, $ctx:expr) => {
                if use_postprocess {
                    $self
                        .post_pipeline
                        .as_ref()
                        .and_then(|p| p.offscreen_view.as_ref())
                        .unwrap_or($ctx.surface_view)
                } else {
                    $ctx.surface_view
                }
            };
        }

        // Pass 1: Clear background
        {
            let view = scene_view!(self, ctx);
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
                let view = scene_view!(self, ctx);
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

        // Pass 2.5: Kitty graphics images
        if !snap.image_placements.is_empty() {
            let view = scene_view!(self, ctx);
            self.draw_kitty_images(ctx, &mut encoder, view, &snap.image_placements, self.padding_px(), self.padding_px());
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

            let view = scene_view!(self, ctx);
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

        // Pass 4: Post-processing blit (offscreen → surface through shader)
        if use_postprocess {
            if let Some(ref post) = self.post_pipeline {
                let params = PostParams {
                    resolution: [ctx.width as f32, ctx.height as f32],
                    time: ctx.elapsed,
                    mode: colorblind_mode,
                };
                ctx.gpu
                    .queue
                    .write_buffer(&post.params_buffer, 0, bytemuck::bytes_of(&params));

                if let Some(ref bind_group) = post.bind_group {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("mado_postprocess"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: ctx.surface_view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    pass.set_pipeline(&post.pipeline);
                    pass.set_bind_group(0, bind_group, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
        }

        ctx.gpu.queue.submit(std::iter::once(encoder.finish()));

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
mod tests {
    use super::*;

    // ---- color_to_f32 ----

    #[test]
    fn test_color_to_f32_white() {
        assert_eq!(color_to_f32(&Color::WHITE), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn test_color_to_f32_black() {
        assert_eq!(color_to_f32(&Color::BLACK), [0.0, 0.0, 0.0, 1.0]);
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
            8.0,
            CursorStyle::Block,
            true,
            530,
            wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            Color::WHITE,
        );
        assert!((renderer.selection_bg[0] - 0.533).abs() < 0.01);
        assert!((renderer.selection_bg[1] - 0.753).abs() < 0.01);
        assert!((renderer.selection_bg[2] - 0.816).abs() < 0.01);
        assert!((renderer.selection_bg[3] - 0.3).abs() < 0.01);
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
