//! GPU rendering module for terminal content.
//!
//! Three-pass rendering pipeline:
//! 1. Clear background
//! 2. Cell backgrounds + cursor + decorations (instanced colored rectangles via RectPipeline)
//! 3. Text (glyphon via garasu with per-cell colors)
//!
//! Uses sequence number damage tracking to skip unchanged frames.

use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use glyphon::{Attrs, Buffer, Color as GlyphonColor, Style, Weight};
use madori::render::{RenderCallback, RenderContext};

use crate::config::CursorStyle;
use crate::selection::Selection;
use crate::terminal::{Cell, CellAttrs, Color, Cursor, Terminal};

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
// Render snapshot — cloned terminal state for lock-free rendering
// ---------------------------------------------------------------------------

struct Snapshot {
    rows: Vec<Vec<Cell>>,
    cursor: Cursor,
    cols: usize,
    num_rows: usize,
}

// ---------------------------------------------------------------------------
// TerminalRenderer
// ---------------------------------------------------------------------------

pub struct TerminalRenderer {
    terminal: SharedTerminal,
    selection: Arc<Mutex<Selection>>,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    padding: f32,
    bg_color: wgpu::Color,
    rect_pipeline: Option<RectPipeline>,
    last_seqno: u64,
    cursor_style: CursorStyle,
    cursor_blink: bool,
    cursor_blink_rate_ms: u32,
    metrics_measured: bool,
}

impl TerminalRenderer {
    pub fn new(
        terminal: SharedTerminal,
        font_size: f32,
        padding: f32,
        cursor_style: CursorStyle,
        cursor_blink: bool,
        cursor_blink_rate_ms: u32,
    ) -> Self {
        let cell_width = font_size * 0.6;
        let cell_height = font_size * 1.4;

        Self {
            terminal,
            selection: Arc::new(Mutex::new(Selection::new())),
            font_size,
            cell_width,
            cell_height,
            padding,
            bg_color: wgpu::Color {
                r: 0.180,
                g: 0.204,
                b: 0.251,
                a: 1.0,
            },
            rect_pipeline: None,
            last_seqno: 0,
            cursor_style,
            cursor_blink,
            cursor_blink_rate_ms,
            metrics_measured: false,
        }
    }

    /// Set the shared selection state (called from main to share with event handler).
    pub fn set_selection(&mut self, selection: Arc<Mutex<Selection>>) {
        self.selection = selection;
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
        (
            Snapshot {
                rows,
                cursor,
                cols,
                num_rows,
            },
            seqno,
        )
    }

    fn build_rect_instances(&self, snap: &Snapshot, elapsed: f32) -> Vec<RectInstance> {
        let mut instances = Vec::new();
        let default_bg = Color::BLACK;
        let sel = self.selection.lock().unwrap();

        for (row_idx, row) in snap.rows.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate().take(snap.cols) {
                // Skip continuation cells (width == 0, part of wide char)
                if cell.width == 0 {
                    continue;
                }

                let inverse = cell.attrs.contains(CellAttrs::INVERSE);
                let bg = if inverse { &cell.fg } else { &cell.bg };
                let fg = if inverse { &cell.bg } else { &cell.fg };

                // Cell background
                if *bg != default_bg {
                    let w = if cell.width == 2 {
                        self.cell_width * 2.0
                    } else {
                        self.cell_width
                    };
                    instances.push(RectInstance {
                        pos: [
                            self.padding + col_idx as f32 * self.cell_width,
                            self.padding + row_idx as f32 * self.cell_height,
                        ],
                        size: [w, self.cell_height],
                        color: color_to_f32(bg),
                    });
                }

                // Underline decoration
                if cell.attrs.contains(CellAttrs::UNDERLINE) {
                    instances.push(RectInstance {
                        pos: [
                            self.padding + col_idx as f32 * self.cell_width,
                            self.padding + (row_idx as f32 + 1.0) * self.cell_height - 2.0,
                        ],
                        size: [self.cell_width, 1.0],
                        color: color_to_f32(fg),
                    });
                }

                // Strikethrough decoration
                if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
                    instances.push(RectInstance {
                        pos: [
                            self.padding + col_idx as f32 * self.cell_width,
                            self.padding + row_idx as f32 * self.cell_height
                                + self.cell_height * 0.5,
                        ],
                        size: [self.cell_width, 1.0],
                        color: color_to_f32(fg),
                    });
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
                                self.padding + col_idx as f32 * self.cell_width,
                                self.padding + row_idx as f32 * self.cell_height,
                            ],
                            size: [self.cell_width, self.cell_height],
                            // Semi-transparent highlight (Nord frost)
                            color: [0.533, 0.753, 0.816, 0.3],
                        });
                    }
                }
            }
        }
        drop(sel);

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
            let cx = self.padding + snap.cursor.col as f32 * self.cell_width;
            let cy = self.padding + snap.cursor.row as f32 * self.cell_height;

            let (pos, size) = match self.cursor_style {
                CursorStyle::Block => ([cx, cy], [self.cell_width, self.cell_height]),
                CursorStyle::Bar => ([cx, cy], [2.0, self.cell_height]),
                CursorStyle::Underline => (
                    [cx, cy + self.cell_height - 2.0],
                    [self.cell_width, 2.0],
                ),
            };

            // Nord snow with slight transparency
            instances.push(RectInstance {
                pos,
                size,
                color: [0.925, 0.937, 0.957, 0.85],
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

                if cell.ch != ' ' || cell.extra.is_some() {
                    has_content = true;
                }

                let inverse = cell.attrs.contains(CellAttrs::INVERSE);
                let effective_fg = if inverse { cell.bg } else { cell.fg };
                let bold = cell.attrs.contains(CellAttrs::BOLD);
                let italic = cell.attrs.contains(CellAttrs::ITALIC);

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
            let spans: Vec<(&str, Attrs<'_>)> = runs
                .iter()
                .map(|run| {
                    let mut attrs =
                        Attrs::new().color(GlyphonColor::rgba(run.fg.r, run.fg.g, run.fg.b, 255));
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

impl RenderCallback for TerminalRenderer {
    fn init(&mut self, gpu: &garasu::GpuContext) {
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        self.rect_pipeline = Some(RectPipeline::new(&gpu.device, format));
    }

    fn render(&mut self, ctx: &mut RenderContext<'_>) {
        // Measure actual font metrics on first render
        self.measure_cell_metrics(ctx.text);

        // Snapshot terminal state under short lock
        let (snap, seqno) = self.snapshot();

        // Damage tracking: skip if nothing changed.
        // When cursor blink is on, always redraw to animate.
        let blink_active = self.cursor_blink && snap.cursor.visible;
        if seqno == self.last_seqno && self.last_seqno != 0 && !blink_active {
            return;
        }
        self.last_seqno = seqno;

        // Build rect instances (cell backgrounds + cursor + decorations)
        let rect_instances = self.build_rect_instances(&snap, ctx.elapsed);

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

        let mut encoder = ctx
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mado_render"),
            });

        // Pass 1: Clear background
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mado_clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: ctx.surface_view,
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
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mado_rects"),
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
            pipeline.draw(&mut pass, rect_instances.len() as u32);
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
                // Fallback color for any characters without explicit color
                default_color: GlyphonColor::rgba(236, 239, 244, 255),
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
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mado_text"),
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
                tracing::warn!("text render error: {e}");
            }
        }

        ctx.gpu.queue.submit(std::iter::once(encoder.finish()));
    }

    fn resize(&mut self, _width: u32, _height: u32) {
        // Terminal resize is handled by the event handler in main.rs
    }
}
