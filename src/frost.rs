use bytemuck::{Pod, Zeroable};

use crate::config::GlassFrostConfig;

pub const MAX_SEGMENTS: usize = 2048;
const CORNER_FRONDS: usize = 5;
const SIXTY_DEGREES: f32 = std::f32::consts::FRAC_PI_3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrostStyle {
    pub enabled: bool,
    pub intensity: f32,
    pub reach: f32,
    pub color: [f32; 3],
    pub seed: u32,
}

impl FrostStyle {
    #[must_use]
    pub fn from_config(config: &GlassFrostConfig) -> Self {
        let color = ishou_tokens::Srgb::from_hex(&config.color).unwrap_or_else(|| {
            tracing::warn!(color = %config.color, "appearance.glass.frost.color is not a hex colour");
            ishou_tokens::Srgb::new(0xd8, 0xde, 0xe9)
        });
        let linear = color.to_linear();
        Self {
            enabled: config.enabled,
            intensity: config.intensity.max(0.0).min(1.0),
            reach: config.reach.max(0.0),
            color: [linear.r, linear.g, linear.b],
            seed: config.seed,
        }
    }

    #[must_use]
    pub fn active(&self) -> bool {
        self.enabled && self.intensity > 0.0 && self.reach > 0.0
    }
}

impl Default for FrostStyle {
    fn default() -> Self {
        Self::from_config(&GlassFrostConfig::default())
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct FrostSegment {
    pub start: [f32; 2],
    pub end: [f32; 2],
    pub half_width: f32,
    pub alpha_start: f32,
    pub alpha_end: f32,
    _pad: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrostField {
    pub width: f32,
    pub height: f32,
    pub reach: f32,
    pub clear: Option<[f32; 4]>,
    pub scale: f32,
    pub intensity: f32,
    pub seed: u32,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.next()
    }
}

impl FrostField {
    fn edge_distance(&self, x: f32, y: f32) -> f32 {
        x.min(self.width - x).min(y).min(self.height - y)
    }

    fn allows(&self, x: f32, y: f32) -> bool {
        let d = self.edge_distance(x, y);
        if d < 0.0 || d > self.reach {
            return false;
        }
        !self
            .clear
            .is_some_and(|[x0, y0, x1, y1]| x >= x0 && x <= x1 && y >= y0 && y <= y1)
    }

    fn alpha_at(&self, x: f32, y: f32) -> f32 {
        let fade = (1.0 - self.edge_distance(x, y) / self.reach).clamp(0.0, 1.0);
        self.intensity * fade
    }

    fn walk(&self, x: f32, y: f32, angle: f32, length: f32) -> f32 {
        let (dx, dy) = (angle.cos(), angle.sin());
        let steps = length.ceil().max(1.0) as usize;
        let step = length / steps as f32;
        let mut walked = 0.0;
        for i in 1..=steps {
            let t = i as f32 * step;
            if !self.allows(x + dx * t, y + dy * t) {
                break;
            }
            walked = t;
        }
        walked
    }

    #[allow(clippy::too_many_arguments)]
    fn branch(
        &self,
        rng: &mut Rng,
        out: &mut Vec<FrostSegment>,
        x: f32,
        y: f32,
        angle: f32,
        length: f32,
        half_width: f32,
        depth: u32,
    ) {
        if out.len() >= MAX_SEGMENTS {
            return;
        }
        let walked = self.walk(x, y, angle, length);
        if walked < 1.5 * self.scale {
            return;
        }
        let (dx, dy) = (angle.cos(), angle.sin());
        let (end_x, end_y) = (x + dx * walked, y + dy * walked);
        out.push(FrostSegment {
            start: [x, y],
            end: [end_x, end_y],
            half_width,
            alpha_start: self.alpha_at(x, y),
            alpha_end: self.alpha_at(end_x, end_y),
            _pad: 0.0,
        });
        if depth == 0 {
            return;
        }
        let forks = 2 + (rng.next() * 3.0) as usize;
        for fork in 1..=forks {
            let at = walked * (fork as f32 + rng.range(-0.2, 0.2)) / (forks + 1) as f32;
            let (fork_x, fork_y) = (x + dx * at, y + dy * at);
            let remaining = walked - at;
            for side in [-1.0_f32, 1.0] {
                if rng.next() < 0.85 {
                    let child_length = remaining * rng.range(0.45, 0.7);
                    self.branch(
                        rng,
                        out,
                        fork_x,
                        fork_y,
                        angle + side * SIXTY_DEGREES,
                        child_length,
                        half_width * 0.8,
                        depth - 1,
                    );
                }
            }
        }
    }

    fn corner_fronds(&self, rng: &mut Rng, out: &mut Vec<FrostSegment>) {
        let half_width = 0.5 * self.scale;
        for (cx, cy, sx, sy) in [
            (0.0, 0.0, 1.0_f32, 1.0_f32),
            (self.width, 0.0, -1.0, 1.0),
            (0.0, self.height, 1.0, -1.0),
            (self.width, self.height, -1.0, -1.0),
        ] {
            let diagonal = sy.atan2(sx);
            for i in 0..CORNER_FRONDS {
                let fan =
                    (i as f32 / (CORNER_FRONDS - 1) as f32 - 0.5) * 2.0 * 35.0_f32.to_radians()
                        + rng.range(-6.0, 6.0).to_radians();
                let slide = self.reach * rng.range(0.0, 0.7);
                let along_x = fan * sx * sy > 0.0;
                let (x, y) = if along_x {
                    (cx + sx * slide, cy + sy * 0.25)
                } else {
                    (cx + sx * 0.25, cy + sy * slide)
                };
                let length = self.reach * rng.range(0.9, 1.4);
                self.branch(rng, out, x, y, diagonal + fan, length, half_width, 2);
            }
        }
    }

    fn side_fronds(
        &self,
        rng: &mut Rng,
        out: &mut Vec<FrostSegment>,
        span: f32,
        place: impl Fn(f32) -> (f32, f32, f32),
    ) {
        let half_width = 0.42 * self.scale;
        let margin = self.reach * 1.5;
        let mut at = margin + self.reach * rng.range(0.2, 1.6);
        while at < span - margin {
            if rng.next() < 0.75 {
                let (x, y, normal) = place(at);
                let angle = normal + rng.range(-18.0, 18.0).to_radians();
                let length = self.reach * rng.range(0.45, 0.95);
                self.branch(rng, out, x, y, angle, length, half_width, 1);
            }
            at += self.reach * rng.range(1.5, 3.1);
        }
    }

    #[must_use]
    pub fn grow(&self) -> Vec<FrostSegment> {
        let mut out = Vec::new();
        if self.reach <= 0.0 || self.intensity <= 0.0 || self.width <= 0.0 || self.height <= 0.0 {
            return out;
        }
        let mut rng = Rng(u64::from(self.seed).wrapping_mul(0xA24B_AED4_963E_E407));
        self.corner_fronds(&mut rng, &mut out);
        let (w, h) = (self.width, self.height);
        let down = std::f32::consts::FRAC_PI_2;
        let inset = 0.25;
        self.side_fronds(&mut rng, &mut out, w, |at| (at, inset, down));
        self.side_fronds(&mut rng, &mut out, w, |at| (at, h - inset, -down));
        self.side_fronds(&mut rng, &mut out, h, |at| (inset, at, 0.0));
        self.side_fronds(&mut rng, &mut out, h, |at| {
            (w - inset, at, std::f32::consts::PI)
        });
        out
    }
}

#[must_use]
pub fn margin_strips(width: u32, height: u32, band: u32) -> [[u32; 4]; 4] {
    let top = band.min(height.div_ceil(2));
    let bottom_y = height.saturating_sub(band).max(top);
    let left = band.min(width.div_ceil(2));
    let right_x = width.saturating_sub(band).max(left);
    let middle = bottom_y - top;
    [
        [0, 0, width, top],
        [0, bottom_y, width, height - bottom_y],
        [0, top, left, middle],
        [right_x, top, width - right_x, middle],
    ]
}

pub const FROST_MASK_SHADER: &str = r"
struct MaskUniforms {
    resolution: vec2<f32>,
    pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> mask: MaskUniforms;

struct Segment {
    @location(0) start: vec2<f32>,
    @location(1) end: vec2<f32>,
    @location(2) half_width: f32,
    @location(3) alpha_start: f32,
    @location(4) alpha_end: f32,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) start: vec2<f32>,
    @location(1) @interpolate(flat) end: vec2<f32>,
    @location(2) @interpolate(flat) half_width: f32,
    @location(3) @interpolate(flat) alpha_start: f32,
    @location(4) @interpolate(flat) alpha_end: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, segment: Segment) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let corner = corners[vi];
    let axis = normalize(segment.end - segment.start);
    let normal = vec2<f32>(-axis.y, axis.x);
    let extent = segment.half_width + 1.0;
    var along = segment.start - axis * extent;
    if corner.x > 0.0 {
        along = segment.end + axis * extent;
    }
    let pixel = along + normal * corner.y * extent;
    var out: VsOut;
    out.position = vec4<f32>(
        pixel.x / mask.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / mask.resolution.y * 2.0,
        0.0,
        1.0,
    );
    out.start = segment.start;
    out.end = segment.end;
    out.half_width = segment.half_width;
    out.alpha_start = segment.alpha_start;
    out.alpha_end = segment.alpha_end;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let pa = in.position.xy - in.start;
    let ba = in.end - in.start;
    let t = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    let distance = length(pa - ba * t);
    let coverage = clamp(in.half_width + 0.5 - distance, 0.0, 1.0);
    let alpha = mix(in.alpha_start, in.alpha_end, t) * coverage;
    return vec4<f32>(alpha, 0.0, 0.0, 1.0);
}
";

pub const FROST_BLIT_SHADER: &str = r"
struct BlitUniforms {
    color: vec4<f32>,
};

@group(0) @binding(0) var rime: texture_2d<f32>;
@group(0) @binding(1) var<uniform> blit: BlitUniforms;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let alpha = textureLoad(rime, vec2<i32>(position.xy), 0).r;
    return vec4<f32>(blit.color.rgb, alpha);
}
";

const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

fn render_pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    layout: &wgpu::BindGroupLayout,
    buffers: &[wgpu::VertexBufferLayout<'_>],
    target: wgpu::ColorTargetState,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers,
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
            targets: &[Some(target)],
        }),
        multiview: None,
        cache: None,
    })
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

struct GrownMask {
    field: FrostField,
    bind_group: wgpu::BindGroup,
}

pub struct FrostPipeline {
    mask_pipeline: wgpu::RenderPipeline,
    mask_bind_group: wgpu::BindGroup,
    mask_uniforms: wgpu::Buffer,
    instances: wgpu::Buffer,
    blit_pipeline: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    blit_uniforms: wgpu::Buffer,
    grown: Option<GrownMask>,
}

impl FrostPipeline {
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let mask_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frost_mask_bgl"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX)],
        });
        let strongest_wins = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Max,
        };
        let mask_pipeline = render_pipeline(
            device,
            "frost_mask",
            FROST_MASK_SHADER,
            &mask_layout,
            &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<FrostSegment>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![
                    0 => Float32x2,
                    1 => Float32x2,
                    2 => Float32,
                    3 => Float32,
                    4 => Float32,
                ],
            }],
            wgpu::ColorTargetState {
                format: MASK_FORMAT,
                blend: Some(wgpu::BlendState {
                    color: strongest_wins,
                    alpha: strongest_wins,
                }),
                write_mask: wgpu::ColorWrites::RED,
            },
        );
        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frost_blit_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                uniform_entry(1, wgpu::ShaderStages::FRAGMENT),
            ],
        });
        let blit_pipeline = render_pipeline(
            device,
            "frost_blit",
            FROST_BLIT_SHADER,
            &blit_layout,
            &[],
            wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            },
        );
        let mask_uniforms = uniform_buffer(device, "frost_mask_uniforms", 16);
        let blit_uniforms = uniform_buffer(device, "frost_blit_uniforms", 16);
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frost_segments"),
            size: (MAX_SEGMENTS * std::mem::size_of::<FrostSegment>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mask_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frost_mask_bg"),
            layout: &mask_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: mask_uniforms.as_entire_binding(),
            }],
        });
        Self {
            mask_pipeline,
            mask_bind_group,
            mask_uniforms,
            instances,
            blit_pipeline,
            blit_layout,
            blit_uniforms,
            grown: None,
        }
    }

    fn regrow(
        &mut self,
        gpu: &garasu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        field: FrostField,
    ) {
        let segments = field.grow();
        gpu.queue
            .write_buffer(&self.instances, 0, bytemuck::cast_slice(&segments));
        gpu.queue.write_buffer(
            &self.mask_uniforms,
            0,
            bytemuck::cast_slice(&[field.width, field.height, 0.0, 0.0]),
        );
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frost_mask"),
            size: wgpu::Extent3d {
                width: field.width as u32,
                height: field.height as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: MASK_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mado_frost_mask"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.mask_pipeline);
            pass.set_bind_group(0, &self.mask_bind_group, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            pass.draw(0..6, 0..segments.len() as u32);
        }
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frost_blit_bg"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.blit_uniforms.as_entire_binding(),
                },
            ],
        });
        self.grown = Some(GrownMask { field, bind_group });
    }

    pub fn draw(
        &mut self,
        gpu: &garasu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        field: FrostField,
        color: [f32; 3],
    ) {
        if self.grown.as_ref().is_none_or(|g| g.field != field) {
            self.regrow(gpu, encoder, field);
        }
        gpu.queue.write_buffer(
            &self.blit_uniforms,
            0,
            bytemuck::cast_slice(&[color[0], color[1], color[2], 1.0]),
        );
        let Some(grown) = self.grown.as_ref() else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_frost"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
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
        pass.set_pipeline(&self.blit_pipeline);
        pass.set_bind_group(0, &grown.bind_group, &[]);
        let band = (field.reach + 2.0 * field.scale).ceil() as u32;
        for [x, y, w, h] in margin_strips(field.width as u32, field.height as u32, band) {
            if w > 0 && h > 0 {
                pass.set_scissor_rect(x, y, w, h);
                pass.draw(0..3, 0..1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field() -> FrostField {
        FrostField {
            width: 1600.0,
            height: 1000.0,
            reach: 56.0,
            clear: None,
            scale: 2.0,
            intensity: 0.16,
            seed: 1,
        }
    }

    fn walk_points(s: &FrostSegment) -> impl Iterator<Item = (f32, f32)> + '_ {
        let (dx, dy) = (s.end[0] - s.start[0], s.end[1] - s.start[1]);
        let n = dx.hypot(dy).ceil().max(1.0) as usize;
        (0..=n).map(move |i| {
            let t = i as f32 / n as f32;
            (s.start[0] + dx * t, s.start[1] + dy * t)
        })
    }

    #[test]
    fn growth_is_deterministic_and_the_seed_changes_it() {
        let f = field();
        assert_eq!(f.grow(), f.grow(), "the same seed grows the same rime");
        let other = FrostField { seed: 2, ..f };
        assert_ne!(f.grow(), other.grow(), "another seed grows another rime");
    }

    #[test]
    fn there_is_rime_and_every_line_stays_within_reach_of_an_edge() {
        let f = field();
        let segments = f.grow();
        assert!(segments.len() > 100, "{} segments", segments.len());
        let tolerance = 1.0;
        for s in &segments {
            for (x, y) in walk_points(s) {
                assert!(
                    x >= -tolerance
                        && x <= f.width + tolerance
                        && y >= -tolerance
                        && y <= f.height + tolerance,
                    "outside the window: {x},{y}"
                );
                assert!(
                    f.edge_distance(x, y) <= f.reach + tolerance,
                    "too far in: {x},{y}"
                );
            }
        }
    }

    #[test]
    fn a_larger_reach_reaches_further_and_a_zero_reach_grows_nothing() {
        let deepest = |f: &FrostField| {
            f.grow()
                .iter()
                .flat_map(|s| [s.start, s.end])
                .map(|[x, y]| f.edge_distance(x, y))
                .fold(0.0_f32, f32::max)
        };
        let near = FrostField {
            reach: 20.0,
            ..field()
        };
        let far = FrostField {
            reach: 80.0,
            ..field()
        };
        assert!(deepest(&far) > deepest(&near) + 20.0);
        assert!(
            FrostField {
                reach: 0.0,
                ..field()
            }
            .grow()
            .is_empty()
        );
    }

    #[test]
    fn no_line_enters_the_terminal_area() {
        let clear = [28.0, 88.0, 1572.0, 972.0];
        let f = FrostField {
            clear: Some(clear),
            ..field()
        };
        let segments = f.grow();
        assert!(segments.len() > 20, "{} segments", segments.len());
        for s in &segments {
            for (x, y) in walk_points(s) {
                let inside = x > clear[0] + 1.0
                    && x < clear[2] - 1.0
                    && y > clear[1] + 1.0
                    && y < clear[3] - 1.0;
                assert!(!inside, "a hairline enters the terminal area at {x},{y}");
            }
        }
    }

    #[test]
    fn intensity_caps_the_alpha_and_the_tips_fade_as_they_grow_in() {
        let f = field();
        let segments = f.grow();
        let peak = segments
            .iter()
            .map(|s| s.alpha_start.max(s.alpha_end))
            .fold(0.0_f32, f32::max);
        assert!(peak <= f.intensity + 1e-5, "peak {peak}");
        assert!(
            peak > f.intensity * 0.8,
            "the roots reach the intensity: {peak}"
        );
        let louder = FrostField {
            intensity: 0.32,
            ..f
        };
        let ratio = louder.grow()[0].alpha_start / segments[0].alpha_start;
        assert!(
            (ratio - 2.0).abs() < 1e-3,
            "alpha follows intensity: {ratio}"
        );
        assert!(
            FrostField {
                intensity: 0.0,
                ..f
            }
            .grow()
            .is_empty()
        );
        for s in &segments {
            let start = f.edge_distance(s.start[0], s.start[1]);
            let end = f.edge_distance(s.end[0], s.end[1]);
            if end > start + 1.0 {
                assert!(s.alpha_end < s.alpha_start, "a line growing inward fades");
            }
        }
    }

    #[test]
    fn the_corners_carry_more_rime_than_the_edges() {
        let f = field();
        let segments = f.grow();
        let corner_zone = f.reach * 2.0;
        let in_corner = |[x, y]: [f32; 2]| {
            (x < corner_zone || x > f.width - corner_zone)
                && (y < corner_zone || y > f.height - corner_zone)
        };
        let corners = segments.iter().filter(|s| in_corner(s.start)).count() as f32;
        let edges = segments.iter().filter(|s| !in_corner(s.start)).count() as f32;
        let corner_area = 4.0 * corner_zone * corner_zone;
        let perimeter = 2.0 * (f.width + f.height) - 8.0 * corner_zone;
        let edge_area = perimeter * f.reach;
        assert!(
            corners / corner_area > 2.0 * edges / edge_area,
            "corners {corners} over {corner_area}, edges {edges} over {edge_area}"
        );
    }

    #[test]
    fn branches_fork_on_the_hexagonal_axes_of_their_parent() {
        let f = field();
        let segments = f.grow();
        let angle = |s: &FrostSegment| (s.end[1] - s.start[1]).atan2(s.end[0] - s.start[0]);
        let (mut hexagonal, mut other) = (0, 0);
        for child in &segments {
            for parent in &segments {
                if child == parent {
                    continue;
                }
                let (px, py) = (
                    parent.end[0] - parent.start[0],
                    parent.end[1] - parent.start[1],
                );
                let (cx, cy) = (
                    child.start[0] - parent.start[0],
                    child.start[1] - parent.start[1],
                );
                let cross = px * cy - py * cx;
                let along = (px * cx + py * cy) / (px * px + py * py);
                if cross.abs() / px.hypot(py) < 0.01 && along > 0.01 && along < 0.99 {
                    let mut turn = (angle(child) - angle(parent)).abs();
                    while turn > std::f32::consts::PI {
                        turn = (turn - 2.0 * std::f32::consts::PI).abs();
                    }
                    if (turn - SIXTY_DEGREES).abs() < 1e-2 {
                        hexagonal += 1;
                    } else {
                        other += 1;
                    }
                }
            }
        }
        assert!(hexagonal > 50, "{hexagonal} forks");
        assert!(
            other * 20 < hexagonal,
            "{other} lines only happen to touch a neighbour, against {hexagonal} forks"
        );
    }

    #[test]
    fn the_segments_always_fit_their_buffer() {
        for (width, height, reach, scale) in [
            (800.0, 600.0, 28.0, 1.0),
            (4112.0, 2658.0, 56.0, 2.0),
            (7680.0, 4320.0, 12.0, 1.0),
            (7680.0, 4320.0, 400.0, 2.0),
        ] {
            let f = FrostField {
                width,
                height,
                reach,
                scale,
                ..field()
            };
            assert!(
                f.grow().len() <= MAX_SEGMENTS,
                "{width}x{height} reach {reach}"
            );
        }
    }

    #[test]
    fn every_key_reaches_the_style_and_off_is_inactive() {
        let config = GlassFrostConfig {
            enabled: true,
            intensity: 0.3,
            reach: 40.0,
            color: "#FFFFFF".to_owned(),
            seed: 9,
        };
        let style = FrostStyle::from_config(&config);
        assert!(style.active());
        assert_eq!(style.intensity, 0.3);
        assert_eq!(style.reach, 40.0);
        assert_eq!(style.color, [1.0, 1.0, 1.0]);
        assert_eq!(style.seed, 9);

        assert!(!FrostStyle::default().active(), "the default is off");
        for off in [
            GlassFrostConfig {
                enabled: false,
                ..config.clone()
            },
            GlassFrostConfig {
                intensity: 0.0,
                ..config.clone()
            },
            GlassFrostConfig {
                reach: 0.0,
                ..config.clone()
            },
        ] {
            assert!(!FrostStyle::from_config(&off).active(), "{off:?}");
        }
    }

    #[test]
    fn a_bad_frost_config_cannot_reach_the_gpu() {
        let style = FrostStyle::from_config(&GlassFrostConfig {
            enabled: true,
            intensity: f32::NAN,
            reach: -4.0,
            color: "pale".to_owned(),
            seed: 0,
        });
        assert_eq!(style.intensity, 0.0);
        assert_eq!(style.reach, 0.0);
        assert!(style.color[0] > 0.0, "falls back to a real colour");
        assert!(!style.active());
        let loud = FrostStyle::from_config(&GlassFrostConfig {
            intensity: 40.0,
            ..GlassFrostConfig::default()
        });
        assert_eq!(loud.intensity, 1.0);
    }

    #[test]
    fn the_margin_strips_cover_each_edge_pixel_once_and_nothing_in_the_middle() {
        for (w, h, band) in [
            (40u32, 30u32, 6u32),
            (41, 31, 6),
            (10, 8, 20),
            (9, 7, 4),
            (1, 1, 3),
        ] {
            let strips = margin_strips(w, h, band);
            for y in 0..h {
                for x in 0..w {
                    let hits = strips
                        .iter()
                        .filter(|[sx, sy, sw, sh]| {
                            x >= *sx && x < sx + sw && y >= *sy && y < sy + sh
                        })
                        .count();
                    let near = x < band
                        || y < band
                        || x >= w.saturating_sub(band)
                        || y >= h.saturating_sub(band);
                    assert_eq!(hits, usize::from(near), "{w}x{h} band {band} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn the_shaders_parse_and_validate_without_a_gpu() {
        for source in [FROST_MASK_SHADER, FROST_BLIT_SHADER] {
            let module = naga::front::wgsl::parse_str(source).expect("a frost shader parses");
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .expect("a frost shader validates");
        }
    }
}
