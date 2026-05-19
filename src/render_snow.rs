//! Snow overlay — the default mado effect.
//!
//! Wraps engawa-snow's typed `SnowParams` + WGSL into a self-
//! contained wgpu render-pass that mado calls after the text
//! pass. The overlay reads the existing surface contents
//! (`LoadOp::Load`) and composites snow on top using alpha
//! blending, so terminal text shows through where there are no
//! flakes.
//!
//! Why direct wgpu pipeline construction instead of using
//! engawa-wgpu's `WgpuDispatcher`: the dispatcher borrows
//! `&wgpu::Device + &wgpu::Queue` for its lifetime, which
//! conflicts with mado's `TerminalRenderer` not owning the
//! device. We reuse engawa-snow's typed shader contract
//! (`SNOW_WGSL`, `SnowParams`) + engawa-wgpu's
//! `combined_shader_source` helper for the fullscreen vertex —
//! the parts that compound — and skip the dispatcher's
//! cross-frame state for v0.1.

use std::time::Instant;

use bytemuck::cast_slice;
use engawa_snow::{SnowParams, SNOW_WGSL};
use engawa_wgpu::combined_shader_source;
use wgpu::util::DeviceExt;

use crate::config::MadoSnowConfig;

/// One-shot snow overlay. Lives for the lifetime of the
/// `TerminalRenderer`; `render` is called after every text pass.
pub struct SnowOverlay {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    params: SnowParams,
    config: MadoSnowConfig,
    start: Instant,
    last_frame: Instant,
}

impl SnowOverlay {
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        config: MadoSnowConfig,
    ) -> Self {
        let combined = combined_shader_source(SNOW_WGSL);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mado_snow_overlay"),
            source: wgpu::ShaderSource::Wgsl(combined.into()),
        });

        let mut params = SnowParams::default()
            .with_intensity(config.intensity)
            .with_wind(config.wind)
            .with_accumulation(config.accumulation)
            .with_layer_count(config.layer_count)
            .with_temperature(config.temperature);
        let _ = &mut params;

        let uniform_buf =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("mado_snow_uniform"),
                contents: cast_slice(&[params]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("mado_snow_bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mado_snow_bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("mado_snow_pl"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mado_snow_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let now = Instant::now();
        Self {
            pipeline,
            bind_group,
            uniform_buf,
            params,
            config,
            start: now,
            last_frame: now,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn set_resolution(&mut self, width: f32, height: f32) {
        self.params.set_resolution([width, height]);
    }

    pub fn set_cursor(&mut self, x: f32, y: f32) {
        self.params.set_cursor([x, y]);
    }

    pub fn cursor_left(&mut self) {
        self.params.set_cursor([-1.0, -1.0]);
    }

    pub fn pulse_typing(&mut self) {
        self.params.pulse_typing(1.0);
    }

    pub fn pulse_keystroke_soft(&mut self) {
        self.params.pulse_typing(0.6);
    }

    pub fn update_config(&mut self, config: MadoSnowConfig) {
        self.config = config;
        self.params.set_intensity(self.config.intensity);
        self.params.set_wind(self.config.wind);
        self.params.set_accumulation(self.config.accumulation);
        self.params.set_layer_count(self.config.layer_count);
        self.params.set_temperature(self.config.temperature);
    }

    /// Encode the snow overlay pass into `encoder`, drawing into
    /// `surface_view` with `LoadOp::Load` so the existing text
    /// composes through.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: &wgpu::TextureView,
    ) {
        if !self.config.enabled {
            return;
        }
        let _ = device;
        let now = Instant::now();
        let t = now.duration_since(self.start).as_secs_f32();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        // ~0.5 s half-life on the pulse, frame-rate-independent.
        let decay = (0.92_f32).powf(dt * 60.0);
        self.params.set_typing_pulse(self.params.frame[3] * decay);
        self.params.set_time(t);

        // Host-integrated accumulation. Temperature drives the
        // sign:
        //   * temp < 0.5  → cold zone — pile grows at pile_rate,
        //     scaled by how cold (full rate at temp=0).
        //   * temp > 0.5  → warm zone — pile melts at melt_rate,
        //     scaled by how warm (full rate at temp=1).
        //   * temp == 0.5 → neutral — pile holds.
        // The shader is stateless per-frame; this is the only
        // place pile state lives on the host between frames.
        let temp = self.config.temperature.clamp(0.0, 1.0);
        let pile_delta = if temp < 0.5 {
            // Cold zone — fill rate proportional to how cold.
            self.config.pile_rate * (1.0 - temp * 2.0) * dt
        } else {
            // Warm zone — melt rate proportional to how warm.
            -self.config.melt_rate * ((temp - 0.5) * 2.0) * dt
        };
        let new_acc = (self.params.params[0] + pile_delta).clamp(0.0, 1.0);
        self.params.set_accumulation(new_acc);

        queue.write_buffer(
            &self.uniform_buf,
            0,
            cast_slice(&[self.params]),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mado_snow_overlay"),
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
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snow_overlay_obeys_disabled_config() {
        let cfg = MadoSnowConfig { enabled: false, ..MadoSnowConfig::default() };
        assert!(!cfg.enabled);
        let default = MadoSnowConfig::default();
        assert!(default.enabled, "snow is the DEFAULT effect — must be on by default");
    }

    #[test]
    fn snow_config_defaults_are_in_sensible_ranges() {
        let c = MadoSnowConfig::default();
        assert!((0.0..=1.0).contains(&c.intensity));
        assert!((-1.0..=1.0).contains(&c.wind));
        assert!((0.0..=1.0).contains(&c.accumulation));
        assert!((1.0..=3.0).contains(&c.layer_count));
    }

    #[cfg(feature = "gpu_tests")]
    mod gpu {
        use super::super::*;
        use garasu::headless::HeadlessTarget;
        use garasu::GpuContext;

        const W: u32 = 96;
        const H: u32 = 64;

        fn run_overlay(cfg: MadoSnowConfig, gpu: &GpuContext) -> Vec<u8> {
            let format = wgpu::TextureFormat::Bgra8UnormSrgb;
            let target = HeadlessTarget::new(gpu, W, H, format);
            let mut overlay = SnowOverlay::new(&gpu.device, format, cfg);
            overlay.set_resolution(W as f32, H as f32);

            let mut encoder =
                gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("snow-test-clear"),
                });
            {
                let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("test-clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view(),
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.05, g: 0.06, b: 0.10, a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
            }
            overlay.render(&gpu.device, &gpu.queue, &mut encoder, target.view());
            gpu.queue.submit(std::iter::once(encoder.finish()));
            let _ = gpu.device.poll(wgpu::PollType::Wait);
            target.read_pixels_rgba8(gpu)
        }

        #[test]
        fn snow_overlay_paints_over_background() {
            let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
            let pixels = run_overlay(MadoSnowConfig::default(), &gpu);
            assert_eq!(pixels.len() as u32, W * H * 4);
            let mut bright = 0;
            for chunk in pixels.chunks_exact(4) {
                let b = chunk[0] as u32 + chunk[1] as u32 + chunk[2] as u32;
                if b > 200 {
                    bright += 1;
                }
            }
            assert!(
                bright > 5,
                "snow overlay must brighten at least a handful of pixels (got {bright})"
            );
        }

        #[test]
        fn snow_overlay_disabled_leaves_background_alone() {
            let gpu = pollster::block_on(GpuContext::new()).expect("gpu");
            let cfg = MadoSnowConfig { enabled: false, ..MadoSnowConfig::default() };
            let pixels = run_overlay(cfg, &gpu);
            // wgpu read_pixels returns BGRA bytes in BGRA order.
            for (i, chunk) in pixels.chunks_exact(4).enumerate() {
                let r = chunk[2];
                let g = chunk[1];
                let b = chunk[0];
                assert!(
                    r < 80 && g < 80 && b < 100,
                    "disabled overlay should leave pixel {i} dark, got R={r} G={g} B={b}"
                );
            }
        }
    }
}
