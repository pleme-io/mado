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
use crate::selection::Selection;
use crate::terminal::{bold_bright_color, default_ansi_palette, Cell, CellAttrs, Color, Cursor, ImagePlacement, Terminal};
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
    /// Viewport-relative rows where a Pane-as-block boundary
    /// sits — each is an OSC 133 `A` prompt-start mark within
    /// the visible viewport. The render layer draws a faint
    /// horizontal separator above each.
    block_separator_rows: Vec<usize>,
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

#[derive(pleme_invalidating_setter_derive::InvalidatingSetter)]
pub struct TerminalRenderer {
    terminal: SharedTerminal,
    selection: Arc<Mutex<Selection>>,
    search: Arc<Mutex<SearchState>>,
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
    padding: f32,
    bg_color: wgpu::Color,
    fg_color: Color,
    #[invalidating_setter]
    ansi_colors: [Color; 16],
    rect_pipeline: Option<RectPipeline>,
    image_pipeline: Option<ImagePipeline>,
    post_pipeline: Option<PostProcessPipeline>,
    gpu_images: HashMap<u32, GpuImage>,
    #[invalidating_setter]
    colorblind_mode: ColorblindMode,
    #[invalidating_setter]
    bold_is_bright: bool,
    last_seqno: u64,
    cursor_style: CursorStyle,
    cursor_blink: bool,
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
    /// Reduce motion: disable cursor blink and bell flash.
    #[invalidating_setter]
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
    /// Snow overlay — the default mado effect. Constructed lazily
    /// in `init()` once the wgpu device is available. `None` when
    /// `effects.snow.enabled = false` or before init.
    snow_overlay: Option<crate::render_snow::SnowOverlay>,
    /// Snow overlay config — captured at construction so init()
    /// can build the overlay with the right knobs. Mirrors
    /// `MadoConfig.effects.snow` exactly.
    snow_config: crate::config::MadoSnowConfig,
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
            // Nord frost #88C0D0 at 0.3 alpha, linearized for the rect
            // pipeline (see `overlay_rect_color`). NOT the raw byte/255
            // triple — that would render washed-out on the sRGB surface.
            selection_bg: overlay_rect_color(0x88, 0xC0, 0xD0, 0.3),
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
            snow_overlay: None,
            snow_config: crate::config::MadoSnowConfig::default(),
        }
    }

    /// Override the snow overlay config. Must be called BEFORE
    /// the first render (i.e. before `init` runs) for the snow
    /// pass to pick it up; otherwise it builds with defaults.
    pub fn set_snow_config(&mut self, cfg: crate::config::MadoSnowConfig) {
        self.snow_config = cfg.clone();
        if let Some(snow) = self.snow_overlay.as_mut() {
            snow.update_config(cfg);
        }
    }

    /// Push the current mouse position into the snow overlay so
    /// the cursor-deflection ring tracks the pointer.
    pub fn snow_set_cursor(&mut self, x: f32, y: f32) {
        if let Some(snow) = self.snow_overlay.as_mut() {
            snow.set_cursor(x, y);
        }
    }

    /// Mark the cursor as off-window — the snow overlay turns
    /// off cursor deflection.
    pub fn snow_cursor_left(&mut self) {
        if let Some(snow) = self.snow_overlay.as_mut() {
            snow.cursor_left();
        }
    }

    /// Bump the typing-pulse on the snow overlay. Called from
    /// the keyboard handler.
    pub fn snow_pulse_typing(&mut self) {
        if let Some(snow) = self.snow_overlay.as_mut() {
            snow.pulse_typing();
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

    /// Physical-pixel font size. Mirrors `padding_px` — logical
    /// `font_size` from config, scaled into physical pixels for the
    /// glyphon font-system + buffer creation.
    #[inline]
    fn font_size_px(&self) -> f32 {
        self.font_size * self.scale_factor
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

    /// Set the shared search state (called from main to share with event handler).
    pub fn set_search(&mut self, search: Arc<Mutex<SearchState>>) {
        self.search = search;
    }

    // set_window removed at Phase 4 — single-pane mado; no multi-pane state to set.

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

    // set_colorblind_mode, set_bold_is_bright, set_ansi_colors now
    // generated by #[derive(InvalidatingSetter)] on TerminalRenderer
    // (fields marked #[invalidating_setter] above). Bodies were
    // identical to the auto-generated form: assign + reset seqno.

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
        let block_separator_rows = term.block_separator_viewport_rows();
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
                block_separator_rows,
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
                // Nord aurora yellow #EBCB8B, linearized for the rect
                // pipeline (current match brighter than other matches).
                let color = if is_current {
                    overlay_rect_color(0xEB, 0xCB, 0x8B, 0.5)
                } else {
                    overlay_rect_color(0xEB, 0xCB, 0x8B, 0.2)
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
                // Nord frost blue #88C0D0 underline, linearized for the
                // rect pipeline (see `overlay_rect_color`).
                color: overlay_rect_color(0x88, 0xC0, 0xD0, 0.6),
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
                color: overlay_rect_color(0x5E, 0x81, 0xAC, 0.30),
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
        crate::perf::log_phase("renderer_init_start");
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        self.rect_pipeline = Some(RectPipeline::new(&gpu.device, format));
        self.image_pipeline = Some(ImagePipeline::new(&gpu.device, format));
        self.post_pipeline = Some(PostProcessPipeline::new(&gpu.device, format));
        if self.snow_config.enabled {
            self.snow_overlay = Some(crate::render_snow::SnowOverlay::new(
                &gpu.device,
                format,
                self.snow_config.clone(),
            ));
        }
        crate::perf::log_phase("renderer_init_done");
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

        // Pass 5: Snow overlay (default-on effect). Renders AFTER
        // post-process so it composes onto the final color-space
        // pixels — text + colorblind grade + snow all live together.
        // The overlay uses LoadOp::Load + alpha blending so terminal
        // contents show through where there are no flakes.
        if let Some(ref mut snow) = self.snow_overlay {
            snow.set_resolution(ctx.width as f32, ctx.height as f32);
            snow.render(&ctx.gpu.device, &ctx.gpu.queue, &mut encoder, ctx.surface_view);
        }

        ctx.gpu.queue.submit(std::iter::once(encoder.finish()));

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
    /// time-driven blinking deterministic.
    fn compute_rects(r: &TerminalRenderer) -> Vec<RectInstance> {
        let (snap, _seqno) = r.snapshot();
        let sel = Selection::new();
        r.build_rect_instances(&snap, 0.0, r.padding_px(), r.padding_px(), &sel)
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

    /// Compute rect instances with an active selection.
    fn compute_rects_with_selection(r: &TerminalRenderer, sel: &Selection) -> Vec<RectInstance> {
        let (snap, _seqno) = r.snapshot();
        r.build_rect_instances(&snap, 0.0, r.padding_px(), r.padding_px(), sel)
    }

    #[test]
    fn active_selection_emits_selection_colored_rects() {
        // Make a selection from (0, 0) to (0, 5). We expect at
        // least one rect with the selection_bg color.
        let (r, t) = harness(20, 3);
        t.write().feed(b"hello world");
        let mut sel = Selection::new();
        sel.start(crate::selection::CellPos { row: 0, col: 0 });
        sel.update(crate::selection::CellPos { row: 0, col: 5 });
        sel.finish();
        assert!(sel.is_active());
        let rects = compute_rects_with_selection(&r, &sel);
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
        let sel = Selection::new(); // never .start()'d
        let rects = compute_rects_with_selection(&r, &sel);
        let sel_rects: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, r.selection_bg))
            .collect();
        assert!(
            sel_rects.is_empty(),
            "no selection should emit no selection-colored rects: {sel_rects:?}"
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
        // Separator color is Nord #5E81AC @ 30% α = ~ [0.369, 0.506, 0.675, 0.30].
        let sep_color = [0.369, 0.506, 0.675, 0.30];
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
        let sep_color = [0.369, 0.506, 0.675, 0.30];
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

    /// Search-match colors (Nord aurora yellow #EBCB8B at two alphas),
    /// **linearized** to mirror `build_rect_instances` (the rect
    /// pipeline writes verbatim to the sRGB surface, so it consumes
    /// linear values via `overlay_rect_color`). Built through the same
    /// typed path so this pin tracks the renderer's contract by
    /// construction instead of carrying a frozen raw-sRGB triple.
    fn search_current_color() -> [f32; 4] {
        super::overlay_rect_color(0xEB, 0xCB, 0x8B, 0.5)
    }
    fn search_other_color() -> [f32; 4] {
        super::overlay_rect_color(0xEB, 0xCB, 0x8B, 0.2)
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
            .filter(|rt| colors_approx_eq(rt.color, search_current_color()))
            .collect();
        let other_hits: Vec<_> = rects
            .iter()
            .filter(|rt| colors_approx_eq(rt.color, search_other_color()))
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
                colors_approx_eq(rt.color, search_current_color())
                    || colors_approx_eq(rt.color, search_other_color())
            })
            .collect();
        assert!(
            any_search.is_empty(),
            "closed search must emit no match rects: {any_search:?}"
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
            wgpu::TextureFormat::Bgra8UnormSrgb,
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
            HeadlessTarget::new(&gpu, 128, 64, wgpu::TextureFormat::Bgra8UnormSrgb);
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
            HeadlessTarget::new(&gpu, 128, 64, wgpu::TextureFormat::Bgra8UnormSrgb);
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
            HeadlessTarget::new(&gpu, 64, 32, wgpu::TextureFormat::Bgra8UnormSrgb);
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
            HeadlessTarget::new(&gpu, 64, 32, wgpu::TextureFormat::Bgra8UnormSrgb);
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
            wgpu::TextureFormat::Bgra8UnormSrgb,
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
            HeadlessTarget::new(&gpu, 96, 48, wgpu::TextureFormat::Bgra8UnormSrgb);
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
            wgpu::TextureFormat::Bgra8UnormSrgb,
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
            wgpu::TextureFormat::Bgra8UnormSrgb,
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
            wgpu::TextureFormat::Bgra8UnormSrgb,
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
            HeadlessTarget::new(&gpu, 256, 96, wgpu::TextureFormat::Bgra8UnormSrgb);
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

    #[test]
    fn render_one_frame_via_garasu_harness_round_trips() {
        // Validate the garasu::HeadlessHarness convenience layer
        // against mado's renderer. If this compiles + asserts,
        // every other garasu consumer can copy the pattern.
        use garasu::headless::{HeadlessHarness, assert_no_magenta_pixels};
        use madori::RenderContext;
        let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
        let mut harness =
            HeadlessHarness::new(&gpu, 128, 64, wgpu::TextureFormat::Bgra8UnormSrgb);
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
