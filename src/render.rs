//! GPU rendering module for terminal content.
//!
//! Three-pass rendering pipeline:
//! 1. Clear background
//! 2. Cell backgrounds + cursor + decorations (instanced colored rectangles via RectPipeline)
//! 3. Text (glyphon via garasu with per-cell colors)
//!
//! Uses sequence number damage tracking to skip unchanged frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use glyphon::{Attrs, Buffer, Color as GlyphonColor, Family, Style, Weight};
use madori::render::{RenderCallback, RenderContext};

use crate::config::{ColorblindMode, CursorStyle};
use crate::pane::PaneRect;
use crate::search::SearchState;
use crate::selection::Selection;
use crate::terminal::{bold_bright_color, default_ansi_palette, Cell, CellAttrs, Color, Cursor, ImagePlacement, Terminal};
use crate::url::{self, DetectedUrl};
use crate::window::WindowState;

/// Shared terminal state between the render thread and PTY I/O thread.
pub type SharedTerminal = Arc<Mutex<Terminal>>;

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
}

impl TerminalRenderer {
    pub fn new(
        terminal: SharedTerminal,
        font_size: f32,
        font_family: String,
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
        }
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

        // Render a reference character and measure its glyph advance width
        let mut buf = text.create_buffer("M", self.font_size, self.font_size * 1.4);
        buf.shape_until_scroll(&mut text.font_system, false);

        let mut measured_width: Option<f32> = None;
        let mut measured_height: Option<f32> = None;

        for run in buf.layout_runs() {
            if measured_height.is_none() {
                measured_height = Some(run.line_height);
            }
            for glyph in run.glyphs.iter() {
                if measured_width.is_none() {
                    measured_width = Some(glyph.w);
                }
            }
        }

        if let Some(w) = measured_width {
            self.cell_width = w;
            tracing::info!(cell_width = w, "measured cell width from font");
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
        let term = self.terminal.lock().unwrap();
        let seqno = term.seqno();
        let cursor = *term.cursor();
        let cols = term.cols();
        let num_rows = term.rows();
        let rows: Vec<Vec<Cell>> = term.visible_rows().map(|r| r.to_vec()).collect();
        let image_placements = term.image_placements().to_vec();
        drop(term);

        // Detect URLs in visible rows
        let urls = url::detect_urls(&rows, cols);

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
        let mut instances = Vec::new();
        let default_bg = Color::BLACK;

        for (row_idx, row) in snap.rows.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate().take(snap.cols) {
                // Skip continuation cells (width == 0, part of wide char)
                if cell.width == 0 {
                    continue;
                }

                let inverse = cell.attrs.contains(CellAttrs::INVERSE);
                let dim = cell.attrs.contains(CellAttrs::DIM);
                let bg = if inverse { &cell.fg } else { &cell.bg };
                let base_fg = if inverse { cell.bg } else { cell.fg };
                let fg = if dim {
                    Color::new(base_fg.r / 2, base_fg.g / 2, base_fg.b / 2)
                } else {
                    base_fg
                };

                // Cell background
                if *bg != default_bg {
                    let w = if cell.width == 2 {
                        self.cell_width * 2.0
                    } else {
                        self.cell_width
                    };
                    instances.push(RectInstance {
                        pos: [
                            origin_x + col_idx as f32 * self.cell_width,
                            origin_y + row_idx as f32 * self.cell_height,
                        ],
                        size: [w, self.cell_height],
                        color: color_to_f32(bg),
                    });
                }

                // Underline decoration
                if cell.attrs.contains(CellAttrs::UNDERLINE) {
                    instances.push(RectInstance {
                        pos: [
                            origin_x + col_idx as f32 * self.cell_width,
                            origin_y + (row_idx as f32 + 1.0) * self.cell_height - 2.0,
                        ],
                        size: [self.cell_width, 1.0],
                        color: color_to_f32(&fg),
                    });
                }

                // Strikethrough decoration
                if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
                    instances.push(RectInstance {
                        pos: [
                            origin_x + col_idx as f32 * self.cell_width,
                            origin_y + row_idx as f32 * self.cell_height
                                + self.cell_height * 0.5,
                        ],
                        size: [self.cell_width, 1.0],
                        color: color_to_f32(&fg),
                    });
                }

                // Box drawing / block element characters — render as pixel-perfect rects
                if is_box_drawing(cell.ch) {
                    let bx = origin_x + col_idx as f32 * self.cell_width;
                    let by = origin_y + row_idx as f32 * self.cell_height;
                    instances.extend(box_drawing_rects(
                        cell.ch,
                        bx,
                        by,
                        self.cell_width,
                        self.cell_height,
                        color_to_f32(&fg),
                    ));
                }
            }
        }

        // Selection highlight
        if sel.is_active() {
            for (row_idx, _row) in snap.rows.iter().enumerate() {
                for col_idx in 0..snap.cols {
                    if sel.contains(row_idx, col_idx) {
                        instances.push(RectInstance {
                            pos: [
                                origin_x + col_idx as f32 * self.cell_width,
                                origin_y + row_idx as f32 * self.cell_height,
                            ],
                            size: [self.cell_width, self.cell_height],
                            color: self.selection_bg,
                        });
                    }
                }
            }
        }

        // Search match highlights
        if snap.search_active {
            for (i, m) in snap.search_matches.iter().enumerate() {
                let is_current = i == snap.search_current;
                // Current match: brighter, other matches: dimmer
                let color = if is_current {
                    [0.922, 0.796, 0.545, 0.5] // Nord aurora yellow
                } else {
                    [0.922, 0.796, 0.545, 0.2] // Dimmer yellow
                };
                for col in m.col_start..=m.col_end {
                    instances.push(RectInstance {
                        pos: [
                            origin_x + col as f32 * self.cell_width,
                            origin_y + m.row as f32 * self.cell_height,
                        ],
                        size: [self.cell_width, self.cell_height],
                        color,
                    });
                }
            }
        }

        // URL underline decorations (hyperlinks from OSC 8 or detected URLs)
        for detected_url in &snap.urls {
            for col in detected_url.col_start..=detected_url.col_end {
                instances.push(RectInstance {
                    pos: [
                        origin_x + col as f32 * self.cell_width,
                        origin_y + (detected_url.row as f32 + 1.0) * self.cell_height - 1.5,
                    ],
                    size: [self.cell_width, 1.0],
                    // Nord frost blue underline
                    color: [0.533, 0.753, 0.816, 0.6],
                });
            }
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
                CursorStyle::Bar => ([cx, cy], [2.0, self.cell_height]),
                CursorStyle::Underline => (
                    [cx, cy + self.cell_height - 2.0],
                    [self.cell_width, 2.0],
                ),
            };

            instances.push(RectInstance {
                pos,
                size,
                color: self.cursor_color,
            });
        }

        instances
    }

    /// Build per-row text buffers with per-cell colored spans.
    fn build_text_buffers(
        &self,
        snap: &Snapshot,
        text: &mut garasu::TextRenderer,
    ) -> Vec<(usize, Buffer)> {
        let mut buffers: Vec<(usize, Buffer)> = Vec::new();

        for (row_idx, row) in snap.rows.iter().enumerate() {
            // Build runs of consecutive cells with the same visual attributes
            let mut runs: Vec<SpanRun> = Vec::new();
            let mut has_content = false;

            for cell in row.iter().take(snap.cols) {
                // Skip continuation cells
                if cell.width == 0 {
                    continue;
                }

                // Box drawing chars are rendered via rect pipeline — emit space
                if is_box_drawing(cell.ch) {
                    has_content = true;
                    // Emit a space to maintain column alignment
                    if let Some(last) = runs.last_mut() {
                        last.text.push(' ');
                    } else {
                        runs.push(SpanRun {
                            text: " ".to_string(),
                            fg: cell.fg,
                            bold: false,
                            italic: false,
                        });
                    }
                    continue;
                }

                if cell.ch != ' ' || cell.extra.is_some() {
                    has_content = true;
                }

                let inverse = cell.attrs.contains(CellAttrs::INVERSE);
                let bold = cell.attrs.contains(CellAttrs::BOLD);
                let dim = cell.attrs.contains(CellAttrs::DIM);
                let italic = cell.attrs.contains(CellAttrs::ITALIC);
                let hidden = cell.attrs.contains(CellAttrs::HIDDEN);

                // Hidden text: make invisible by using bg color as fg
                if hidden {
                    let bg = if inverse { &cell.fg } else { &cell.bg };
                    let effective_fg = *bg;
                    if let Some(last) = runs.last_mut() {
                        if last.fg == effective_fg && !last.bold && !last.italic {
                            cell.write_to(&mut last.text);
                            continue;
                        }
                    }
                    let mut s = String::new();
                    cell.write_to(&mut s);
                    runs.push(SpanRun {
                        text: s,
                        fg: effective_fg,
                        bold: false,
                        italic: false,
                    });
                    continue;
                }

                // Bold-as-bright: ANSI colors 0-7 become 8-15 when bold (if enabled)
                let mut effective_fg = if inverse {
                    cell.bg
                } else if bold && self.bold_is_bright {
                    bold_bright_color(&cell.fg, &self.ansi_colors)
                } else {
                    cell.fg
                };

                // Dim: halve the brightness of the foreground color
                if dim {
                    effective_fg = Color::new(
                        effective_fg.r / 2,
                        effective_fg.g / 2,
                        effective_fg.b / 2,
                    );
                }

                // Try to extend the last run if attributes match
                if let Some(last) = runs.last_mut() {
                    if last.fg == effective_fg && last.bold == bold && last.italic == italic {
                        cell.write_to(&mut last.text);
                        continue;
                    }
                }

                let mut s = String::new();
                cell.write_to(&mut s);
                runs.push(SpanRun {
                    text: s,
                    fg: effective_fg,
                    bold,
                    italic,
                });
            }

            // Skip empty rows unless the cursor is here
            if !has_content && row_idx != snap.cursor.row {
                continue;
            }

            // Trim trailing whitespace from the last run
            if let Some(last) = runs.last_mut() {
                let trimmed_len = last.text.trim_end().len();
                if trimmed_len == 0 {
                    runs.pop();
                } else {
                    last.text.truncate(trimmed_len);
                }
            }

            if runs.is_empty() {
                if row_idx == snap.cursor.row {
                    let buf = text.create_buffer(" ", self.font_size, self.cell_height);
                    buffers.push((row_idx, buf));
                }
                continue;
            }

            // Build glyphon spans from runs
            let font_family = Family::Name(&self.font_family);
            let spans: Vec<(&str, Attrs<'_>)> = runs
                .iter()
                .map(|run| {
                    let mut attrs = Attrs::new()
                        .family(font_family)
                        .color(GlyphonColor::rgba(run.fg.r, run.fg.g, run.fg.b, 255));
                    if run.bold {
                        attrs = attrs.weight(Weight::BOLD);
                    }
                    if run.italic {
                        attrs = attrs.style(Style::Italic);
                    }
                    (run.text.as_str(), attrs)
                })
                .collect();

            let buf = text.create_rich_buffer(&spans, self.font_size, self.cell_height);
            buffers.push((row_idx, buf));
        }

        buffers
    }

    /// Snapshot a specific pane's terminal state (for multi-pane rendering).
    fn snapshot_pane(
        &self,
        terminal: &SharedTerminal,
        search: &Arc<Mutex<SearchState>>,
    ) -> (Snapshot, u64) {
        let term = terminal.lock().unwrap();
        let seqno = term.seqno();
        let cursor = *term.cursor();
        let cols = term.cols();
        let num_rows = term.rows();
        let rows: Vec<Vec<Cell>> = term.visible_rows().map(|r| r.to_vec()).collect();
        let image_placements = term.image_placements().to_vec();
        drop(term);

        let urls = url::detect_urls(&rows, cols);

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
    fn render_multi_pane(&mut self, ctx: &mut RenderContext<'_>) {
        let window = self.window.clone().unwrap();
        let ws = window.lock().unwrap();
        let pane_rects = ws.layout(
            self.padding,
            self.padding,
            ctx.width as f32 - 2.0 * self.padding,
            ctx.height as f32 - 2.0 * self.padding,
        );
        let focused_id = ws.focused_pane_id();
        let pane_count = pane_rects.len();

        let mut all_rects = Vec::new();
        let mut text_entries: Vec<(f32, f32, usize, Buffer, PaneRect)> = Vec::new();
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
                for (row_idx, buf) in self.build_text_buffers(&snap, ctx.text) {
                    text_entries.push((rect.x, rect.y, row_idx, buf, *rect));
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
        for (left, top_origin, row_idx, buffer, rect) in &text_entries {
            let y = top_origin + (*row_idx as f32 * self.cell_height);
            text_areas.push(glyphon::TextArea {
                buffer,
                left: *left,
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

        if let Err(e) = ctx.text.prepare(
            &ctx.gpu.device,
            &ctx.gpu.queue,
            ctx.width,
            ctx.height,
            text_areas,
        ) {
            tracing::warn!("text prepare error: {e}");
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

        // Pass 2: Rects
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

        // Pass 2.5: Kitty graphics images (per-pane)
        for (ox, oy, placements) in &all_image_placements {
            let view = scene_view!(self, ctx);
            self.draw_kitty_images(ctx, &mut encoder, view, placements, *ox, *oy);
        }

        // Pass 3: Text
        {
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

/// A run of text with uniform visual attributes.
struct SpanRun {
    text: String,
    fg: Color,
    bold: bool,
    italic: bool,
}

fn color_to_f32(c: &Color) -> [f32; 4] {
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    ]
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

        let term = self.terminal.lock().unwrap();
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
        // Measure actual font metrics on first render
        self.measure_cell_metrics(ctx.text);

        // Multi-pane path: render all panes from WindowState
        if self.window.is_some() {
            self.render_multi_pane(ctx);
            return;
        }

        // Single-pane path
        let (snap, seqno) = self.snapshot();

        // Damage tracking: skip if nothing changed.
        // When cursor blink is on, always redraw to animate.
        // Bell flash and search also force redraw.
        let blink_active = self.cursor_blink && snap.cursor.visible;
        let bell_active = self.bell_flash_frames > 0;
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
        let sel = self.selection.lock().unwrap();
        let mut rect_instances =
            self.build_rect_instances(&snap, ctx.elapsed, self.padding, self.padding, &sel);
        drop(sel);

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
        let text_buffers = self.build_text_buffers(&snap, ctx.text);

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

        // Pass 2: Cell backgrounds + cursor + decorations
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

        // Pass 2.5: Kitty graphics images
        {
            let view = scene_view!(self, ctx);
            self.draw_kitty_images(ctx, &mut encoder, view, &snap.image_placements, self.padding, self.padding);
        }

        // Pass 3: Text with per-cell colors
        let mut text_areas = Vec::new();
        for (row_idx, buffer) in &text_buffers {
            let y = self.padding + (*row_idx as f32 * self.cell_height);
            text_areas.push(glyphon::TextArea {
                buffer,
                left: self.padding,
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

        if let Err(e) = ctx.text.prepare(
            &ctx.gpu.device,
            &ctx.gpu.queue,
            ctx.width,
            ctx.height,
            text_areas,
        ) {
            tracing::warn!("text prepare error: {e}");
        }

        {
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
    fn test_color_to_f32_mid_gray() {
        let [r, g, b, a] = color_to_f32(&Color::new(128, 128, 128));
        assert!((r - 128.0 / 255.0).abs() < 0.001);
        assert!((g - 128.0 / 255.0).abs() < 0.001);
        assert!((b - 128.0 / 255.0).abs() < 0.001);
        assert!((a - 1.0).abs() < f32::EPSILON);
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
    fn test_color_to_f32_rgba_helper() {
        let c = Color::new(51, 102, 153);
        let [r, g, b, a] = color_to_f32(&c);
        assert!((r - 51.0 / 255.0).abs() < 0.001);
        assert!((g - 102.0 / 255.0).abs() < 0.001);
        assert!((b - 153.0 / 255.0).abs() < 0.001);
        assert!((a - 1.0).abs() < f32::EPSILON);
    }

    // ---- default selection_bg / cursor_color ----

    #[test]
    fn test_selection_bg_default() {
        let term = std::sync::Arc::new(std::sync::Mutex::new(
            crate::terminal::Terminal::new(80, 24),
        ));
        let renderer = TerminalRenderer::new(
            term,
            14.0,
            "JetBrains Mono".into(),
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
        let term = std::sync::Arc::new(std::sync::Mutex::new(
            crate::terminal::Terminal::new(80, 24),
        ));
        let renderer = TerminalRenderer::new(
            term,
            14.0,
            "JetBrains Mono".into(),
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
