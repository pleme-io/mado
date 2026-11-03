use bytemuck::{Pod, Zeroable};
use wgpu::CompositeAlphaMode;

use crate::config::WindowConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassEncoding {
    ElectricalPremultiplied,
    Straight,
}

impl GlassEncoding {
    #[must_use]
    pub fn for_surface(alpha: madori::SurfaceAlpha) -> Option<Self> {
        alpha
            .translucent_mode()
            .map(|mode| Self::for_mode(mode, cfg!(target_os = "macos")))
    }

    #[must_use]
    fn for_mode(mode: CompositeAlphaMode, core_animation: bool) -> Self {
        match mode {
            CompositeAlphaMode::PostMultiplied if !core_animation => Self::Straight,
            _ => Self::ElectricalPremultiplied,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassShape {
    pub corner_radius: f32,
    pub rim_width: f32,
    pub rim_color: [f32; 4],
}

impl Default for GlassShape {
    fn default() -> Self {
        Self {
            corner_radius: 0.0,
            rim_width: 0.0,
            rim_color: [0.0; 4],
        }
    }
}

impl GlassShape {
    #[must_use]
    pub fn from_window(window: &WindowConfig) -> Self {
        let rim = ishou_tokens::Srgb::from_hex(&window.rim.color).unwrap_or_else(|| {
            tracing::warn!(color = %window.rim.color, "window.rim.color is not a hex colour");
            ishou_tokens::Srgb::new(0xec, 0xef, 0xf4)
        });
        let linear = rim.to_linear();
        Self {
            corner_radius: window.corner_radius.max(0.0),
            rim_width: window.rim.width.max(0.0),
            rim_color: [
                linear.r,
                linear.g,
                linear.b,
                window.rim.opacity.max(0.0).min(1.0),
            ],
        }
    }

    #[must_use]
    pub fn flat(self) -> Self {
        Self {
            corner_radius: 0.0,
            rim_width: 0.0,
            ..self
        }
    }
}

#[must_use]
pub fn premultiply(color: wgpu::Color) -> wgpu::Color {
    let a = color.a.max(0.0).min(1.0);
    wgpu::Color {
        r: color.r * a,
        g: color.g * a,
        b: color.b * a,
        a,
    }
}

#[must_use]
pub fn srgb_encode(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[must_use]
pub fn srgb_decode(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[must_use]
pub fn rounded_box_distance(p: [f32; 2], half: [f32; 2], radius: f32) -> f32 {
    let qx = p[0].abs() - half[0] + radius;
    let qy = p[1].abs() - half[1] + radius;
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    outside + qx.max(qy).min(0.0) - radius
}

#[must_use]
pub fn shape_coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

#[must_use]
pub fn rim_coverage(distance: f32, width: f32) -> f32 {
    if width <= 0.0 {
        0.0
    } else {
        (distance + width + 0.5).clamp(0.0, 1.0)
    }
}

#[must_use]
pub fn resolve_pixel(
    scene: [f32; 4],
    coverage: f32,
    rim_cover: f32,
    rim_color: [f32; 4],
    encoding: GlassEncoding,
) -> [f32; 4] {
    let rim_a = rim_color[3] * rim_cover;
    let mix = |rim: f32, under: f32| rim * rim_a + under * (1.0 - rim_a);
    let rgb = [
        mix(rim_color[0], scene[0]),
        mix(rim_color[1], scene[1]),
        mix(rim_color[2], scene[2]),
    ];
    let composed = rim_a + scene[3] * (1.0 - rim_a);
    let a = composed.max(rgb[0]).max(rgb[1]).max(rgb[2]);
    let out_a = a * coverage;
    let channel = |c: f32| {
        let straight = if a > 0.0 { c / a } else { 0.0 };
        match encoding {
            GlassEncoding::ElectricalPremultiplied => srgb_decode(srgb_encode(straight) * out_a),
            GlassEncoding::Straight => straight,
        }
    };
    [channel(rgb[0]), channel(rgb[1]), channel(rgb[2]), out_a]
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct GlassUniforms {
    size: [f32; 2],
    radius: f32,
    rim_width: f32,
    rim_color: [f32; 4],
    premultiply: f32,
    _pad: [f32; 3],
}

impl GlassUniforms {
    #[must_use]
    pub fn new(
        shape: GlassShape,
        encoding: GlassEncoding,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Self {
        let size = [width as f32, height as f32];
        Self {
            size,
            radius: (shape.corner_radius * scale).min(size[0].min(size[1]) * 0.5),
            rim_width: shape.rim_width * scale,
            rim_color: shape.rim_color,
            premultiply: match encoding {
                GlassEncoding::ElectricalPremultiplied => 1.0,
                GlassEncoding::Straight => 0.0,
            },
            _pad: [0.0; 3],
        }
    }

    #[must_use]
    pub fn expected_pixel(&self, scene: [f32; 4], x: u32, y: u32) -> [f32; 4] {
        let half = [self.size[0] * 0.5, self.size[1] * 0.5];
        let p = [x as f32 + 0.5 - half[0], y as f32 + 0.5 - half[1]];
        let d = rounded_box_distance(p, half, self.radius);
        let encoding = if self.premultiply > 0.5 {
            GlassEncoding::ElectricalPremultiplied
        } else {
            GlassEncoding::Straight
        };
        resolve_pixel(
            scene,
            shape_coverage(d),
            rim_coverage(d, self.rim_width),
            self.rim_color,
            encoding,
        )
    }
}

pub const GLASS_SHADER: &str = r"
struct GlassUniforms {
    size: vec2<f32>,
    radius: f32,
    rim_width: f32,
    rim_color: vec4<f32>,
    premultiply: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
};

@group(0) @binding(0) var scene: texture_2d<f32>;
@group(0) @binding(1) var<uniform> glass: GlassUniforms;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn encode(c: f32) -> f32 {
    if c <= 0.0031308 {
        return c * 12.92;
    }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

fn decode(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn channel(c: f32, a: f32, out_a: f32) -> f32 {
    var straight = 0.0;
    if a > 0.0 {
        straight = c / a;
    }
    if glass.premultiply > 0.5 {
        return decode(encode(straight) * out_a);
    }
    return straight;
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let s = textureLoad(scene, vec2<i32>(position.xy), 0);
    let half = glass.size * 0.5;
    let p = position.xy - half;
    let q = abs(p) - half + vec2<f32>(glass.radius);
    let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - glass.radius;
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    var rim_cover = 0.0;
    if glass.rim_width > 0.0 {
        rim_cover = clamp(d + glass.rim_width + 0.5, 0.0, 1.0);
    }
    let rim_a = glass.rim_color.a * rim_cover;
    let rgb = glass.rim_color.rgb * rim_a + s.rgb * (1.0 - rim_a);
    let composed = rim_a + s.a * (1.0 - rim_a);
    let a = max(composed, max(rgb.r, max(rgb.g, rgb.b)));
    let out_a = a * coverage;
    return vec4<f32>(
        channel(rgb.r, a, out_a),
        channel(rgb.g, a, out_a),
        channel(rgb.b, a, out_a),
        out_a,
    );
}
";

pub struct GlassPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
}

impl GlassPipeline {
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glass_shader"),
            source: wgpu::ShaderSource::Wgsl(GLASS_SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glass_bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
            label: Some("glass_pl"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glass_pipeline"),
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
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glass_uniforms"),
            size: std::mem::size_of::<GlassUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            uniforms,
        }
    }

    pub fn resolve(
        &self,
        gpu: &garasu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        scene: &wgpu::TextureView,
        surface: &wgpu::TextureView,
        uniforms: &GlassUniforms,
    ) {
        gpu.queue
            .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(uniforms));
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glass_bg"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(scene),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.uniforms.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_glass_resolve"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: surface,
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
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_RIM: [f32; 4] = [0.0; 4];

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn premultiply_scales_colour_and_keeps_alpha_with_a_control_at_one() {
        let c = wgpu::Color {
            r: 0.4,
            g: 0.2,
            b: 0.8,
            a: 1.0,
        };
        assert_eq!(premultiply(c), c, "control: an opaque colour is unchanged");
        let half = premultiply(wgpu::Color { a: 0.5, ..c });
        assert_eq!(
            (half.r, half.g, half.b, half.a),
            (0.2, 0.1, 0.4, 0.5),
            "linear premultiply"
        );
        assert_eq!(premultiply(wgpu::Color { a: 7.0, ..c }), c);
        let none = premultiply(wgpu::Color { a: -1.0, ..c });
        assert_eq!((none.r, none.a), (0.0, 0.0));
        assert_eq!(premultiply(wgpu::Color { a: f64::NAN, ..c }).a, 0.0);
    }

    #[test]
    fn encode_and_decode_are_inverses_at_the_knee_and_across_the_range() {
        for i in 0..=1000 {
            let x = i as f32 / 1000.0;
            assert!(close(srgb_decode(srgb_encode(x)), x), "{x}");
        }
    }

    #[test]
    fn a_translucent_pixel_is_premultiplied_in_the_electrical_space_the_compositor_blends_in() {
        let c = 0.2_f32;
        let a = 0.5_f32;
        let out = resolve_pixel(
            [c * a, c * a, c * a, a],
            1.0,
            0.0,
            NO_RIM,
            GlassEncoding::ElectricalPremultiplied,
        );
        let stored = srgb_encode(out[0]);
        assert!(close(stored, srgb_encode(c) * a), "stored {stored}");
        assert!(close(out[3], a));
        let naive_optical = srgb_encode(c * a);
        assert!(
            (stored - naive_optical).abs() > 0.05,
            "control: an sRGB surface fed the linear premultiply stores {naive_optical}, \
             which is the too-bright pane this exists to avoid"
        );
    }

    #[test]
    fn an_opaque_pixel_passes_through_so_text_keeps_full_strength() {
        for c in [0.0, 0.03, 0.2, 0.75, 1.0_f32] {
            let out = resolve_pixel(
                [c, c, c, 1.0],
                1.0,
                0.0,
                NO_RIM,
                GlassEncoding::ElectricalPremultiplied,
            );
            assert!(close(out[0], c), "{c} became {}", out[0]);
            assert!(close(out[3], 1.0));
        }
    }

    #[test]
    fn a_glyph_edge_never_stores_more_light_than_its_alpha() {
        for pane in [0.0, 0.12, 0.5_f32] {
            for cov in [0.05, 0.25, 0.5, 0.9_f32] {
                let bg = 0.03 * pane;
                let rgb = 1.0 * cov + bg * (1.0 - cov);
                let a = cov + pane * (1.0 - cov);
                let out = resolve_pixel(
                    [rgb, rgb, rgb, a],
                    1.0,
                    0.0,
                    NO_RIM,
                    GlassEncoding::ElectricalPremultiplied,
                );
                assert!(
                    srgb_encode(out[0]) <= out[3] + 1e-4,
                    "pane {pane} coverage {cov}: {} over alpha {}",
                    srgb_encode(out[0]),
                    out[3]
                );
            }
        }
    }

    #[test]
    fn light_past_the_pane_raises_alpha_rather_than_store_an_invalid_pixel() {
        let out = resolve_pixel(
            [0.5, 0.2, 0.1, 0.12],
            1.0,
            0.0,
            NO_RIM,
            GlassEncoding::ElectricalPremultiplied,
        );
        assert!(close(out[3], 0.5), "alpha rises to the brightest channel");
        assert!(srgb_encode(out[0]) <= out[3] + 1e-4);
        let clear = resolve_pixel(
            [0.0; 4],
            1.0,
            0.0,
            NO_RIM,
            GlassEncoding::ElectricalPremultiplied,
        );
        assert_eq!(clear, [0.0; 4], "a clear pixel stays clear");
    }

    #[test]
    fn straight_encoding_emits_the_colour_and_leaves_scaling_to_the_compositor() {
        let out = resolve_pixel(
            [0.1, 0.1, 0.1, 0.5],
            1.0,
            0.0,
            NO_RIM,
            GlassEncoding::Straight,
        );
        assert!(close(out[0], 0.2));
        assert!(close(out[3], 0.5));
    }

    #[test]
    fn the_shape_is_full_on_straight_edges_and_clear_past_the_corner() {
        let half = [100.0, 50.0];
        for r in [0.0, 12.0] {
            let edge = rounded_box_distance([0.0, 49.5], half, r);
            assert_eq!(shape_coverage(edge), 1.0, "radius {r} top edge");
            let side = rounded_box_distance([99.5, 0.0], half, r);
            assert_eq!(shape_coverage(side), 1.0, "radius {r} side edge");
        }
        let corner = [99.5, 49.5];
        assert_eq!(
            shape_coverage(rounded_box_distance(corner, half, 12.0)),
            0.0,
            "rounded corner is clear"
        );
        assert_eq!(
            shape_coverage(rounded_box_distance(corner, half, 0.0)),
            1.0,
            "control: a square corner stays covered"
        );
    }

    #[test]
    fn the_rim_is_a_hairline_inside_the_edge_and_zero_width_draws_nothing() {
        assert_eq!(rim_coverage(-0.5, 1.0), 1.0, "outermost pixel row");
        assert_eq!(rim_coverage(-1.5, 1.0), 0.0, "next pixel row is clean");
        assert_eq!(rim_coverage(-0.5, 0.0), 0.0);
        let out = resolve_pixel(
            [0.0, 0.0, 0.0, 0.2],
            1.0,
            rim_coverage(-0.5, 1.0),
            [1.0, 1.0, 1.0, 1.0],
            GlassEncoding::ElectricalPremultiplied,
        );
        assert!(close(out[0], 1.0));
        assert!(close(out[3], 1.0));
    }

    #[test]
    fn the_uniforms_scale_by_the_display_and_clamp_the_radius() {
        let shape = GlassShape {
            corner_radius: 10.0,
            rim_width: 1.0,
            rim_color: [1.0; 4],
        };
        let u = GlassUniforms::new(shape, GlassEncoding::Straight, 200, 100, 2.0);
        assert_eq!((u.radius, u.rim_width, u.premultiply), (20.0, 2.0, 0.0));
        let big = GlassShape {
            corner_radius: 1000.0,
            ..shape
        };
        let u = GlassUniforms::new(big, GlassEncoding::ElectricalPremultiplied, 200, 100, 1.0);
        assert_eq!((u.radius, u.premultiply), (50.0, 1.0));
    }

    #[test]
    fn every_surface_mode_has_an_encoding_and_metal_postmultiplied_is_core_animation() {
        use CompositeAlphaMode::{Inherit, PostMultiplied, PreMultiplied};
        assert_eq!(
            GlassEncoding::for_mode(PreMultiplied, false),
            GlassEncoding::ElectricalPremultiplied
        );
        assert_eq!(
            GlassEncoding::for_mode(Inherit, false),
            GlassEncoding::ElectricalPremultiplied
        );
        assert_eq!(
            GlassEncoding::for_mode(PostMultiplied, false),
            GlassEncoding::Straight
        );
        assert_eq!(
            GlassEncoding::for_mode(PostMultiplied, true),
            GlassEncoding::ElectricalPremultiplied
        );
        let opaque = madori::SurfaceAlpha {
            mode: CompositeAlphaMode::Opaque,
            transparent: true,
        };
        assert_eq!(GlassEncoding::for_surface(opaque), None);
        let unasked = madori::SurfaceAlpha {
            mode: PreMultiplied,
            transparent: false,
        };
        assert_eq!(GlassEncoding::for_surface(unasked), None);
    }

    #[test]
    fn a_bad_shape_config_cannot_reach_the_gpu() {
        let mut window = WindowConfig::default();
        window.corner_radius = f32::NAN;
        window.rim.width = -3.0;
        window.rim.opacity = f32::NAN;
        window.rim.color = "not a colour".into();
        let shape = GlassShape::from_window(&window);
        assert_eq!(shape.corner_radius, 0.0);
        assert_eq!(shape.rim_width, 0.0);
        assert_eq!(shape.rim_color[3], 0.0);
        assert!(shape.rim_color[0] > 0.0, "falls back to a real colour");
    }

    #[test]
    fn the_shader_parses_and_validates_without_a_gpu() {
        let module = naga::front::wgsl::parse_str(GLASS_SHADER).expect("GLASS_SHADER parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("GLASS_SHADER validates");
    }
}
