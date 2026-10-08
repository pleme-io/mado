use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{PacingMode, PerformanceConfig, TearRuntime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PacingRow {
    pub pacing: &'static str,
    pub parks: bool,
    pub hides: bool,
    pub cells: &'static [&'static str],
    pub control: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeRow {
    pub runtime: &'static str,
    pub case: &'static str,
    pub window_cells: &'static [&'static str],
}

pub const DEMAND_CELLS: &[&str] = &["C10/idle-ticks", "C10/window-wakeups", "C3/present"];
pub const CAPPED_CELLS: &[&str] = &["C10/idle-ticks", "C10/window-wakeups"];

#[must_use]
pub const fn pacing_row(pacing: madori::FramePacing) -> PacingRow {
    match pacing {
        madori::FramePacing::Reactive(_) => PacingRow {
            pacing: "reactive",
            parks: true,
            hides: true,
            cells: DEMAND_CELLS,
            control: None,
        },
        madori::FramePacing::Capped(_) => PacingRow {
            pacing: "capped",
            parks: false,
            hides: false,
            cells: CAPPED_CELLS,
            control: Some("pacing-capped"),
        },
        madori::FramePacing::Continuous => PacingRow {
            pacing: "continuous",
            parks: false,
            hides: false,
            cells: &[],
            control: None,
        },
    }
}

#[must_use]
pub const fn mode_name(mode: PacingMode) -> &'static str {
    match mode {
        PacingMode::Demand => "demand",
        PacingMode::Capped => "capped",
        PacingMode::Continuous => "continuous",
    }
}

#[must_use]
pub const fn runtime_row(runtime: TearRuntime) -> RuntimeRow {
    match runtime {
        TearRuntime::Embedded => RuntimeRow {
            runtime: "embedded",
            case: "C1",
            window_cells: &[],
        },
        TearRuntime::Daemon => RuntimeRow {
            runtime: "daemon",
            case: "C2",
            window_cells: &[],
        },
        TearRuntime::Resident => RuntimeRow {
            runtime: "resident",
            case: "C3",
            window_cells: &[
                "C3/rpcs",
                "C3/present",
                "C10/idle-ticks",
                "C10/window-wakeups",
            ],
        },
    }
}

#[must_use]
pub fn rows() -> serde_json::Value {
    let pacings: Vec<serde_json::Value> = PacingMode::ALL
        .iter()
        .map(|mode| {
            let config = PerformanceConfig {
                pacing: *mode,
                ..PerformanceConfig::default()
            };
            serde_json::json!({
                "mode": mode_name(*mode),
                "row": pacing_row(config.frame_pacing()),
            })
        })
        .collect();
    let runtimes: Vec<RuntimeRow> = TearRuntime::ALL.iter().map(|r| runtime_row(*r)).collect();
    serde_json::json!({
        "pacings": pacings,
        "runtimes": runtimes,
        "present_scenes": PresentScene::ALL.iter().map(|s| s.name()).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, pleme_allvariants_derive::AllVariants)]
pub enum PresentScene {
    FullRebuild,
    OneRow,
    Repeat,
}

impl PresentScene {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FullRebuild => "full-rebuild",
            Self::OneRow => "one-row",
            Self::Repeat => "repeat",
        }
    }

    fn feed(self, frame: usize, rows: usize, cols: usize) -> Vec<u8> {
        match self {
            Self::FullRebuild => {
                let mut out = b"\x1b[H".to_vec();
                for row in 0..rows {
                    out.extend(row_bytes(row, frame, cols));
                    if row + 1 < rows {
                        out.extend_from_slice(b"\r\n");
                    }
                }
                out
            }
            Self::OneRow => {
                let row = frame % rows;
                let mut out = format!("\x1b[{};1H", row + 1).into_bytes();
                out.extend(row_bytes(row, frame, cols));
                out
            }
            Self::Repeat => Vec::new(),
        }
    }
}

fn row_bytes(row: usize, frame: usize, cols: usize) -> Vec<u8> {
    let mut out = format!("\x1b[3{}m", (row + frame) % 8).into_bytes();
    let line = format!("{row:03} {frame:06} the quick brown fox jumps over the lazy dog ");
    let mut text: String = line.chars().cycle().take(cols).collect();
    text.truncate(cols);
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[0m");
    out
}

#[derive(Debug, Clone, Copy)]
pub struct PresentOptions {
    pub frames: usize,
    pub warmup: usize,
    pub cols: usize,
    pub rows: usize,
    pub width: u32,
    pub height: u32,
}

fn quantile(sorted: &[Duration], percent: usize) -> Duration {
    sorted
        .get(sorted.len().saturating_sub(1) * percent / 100)
        .copied()
        .unwrap_or_default()
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn renderer_for(terminal: &crate::render::SharedTerminal) -> crate::render::TerminalRenderer {
    crate::render::TerminalRenderer::new(
        Arc::clone(terminal),
        14.0,
        1.4,
        "monospace".into(),
        "monospace".into(),
        "monospace".into(),
        0.0,
        crate::config::CursorStyle::Block,
        false,
        500,
        wgpu::Color {
            r: 0.180,
            g: 0.204,
            b: 0.251,
            a: 1.0,
        },
        crate::terminal::Color::WHITE,
    )
}

fn run_scene(gpu: &garasu::GpuContext, scene: PresentScene, opts: PresentOptions) {
    use madori::{RenderCallback, RenderContext};
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let target = garasu::headless::HeadlessTarget::new(gpu, opts.width, opts.height, format);
    let terminal = Arc::new(parking_lot::RwLock::new(
        crate::terminal::Terminal::with_scrollback(opts.cols, opts.rows, 1_000),
    ));
    terminal
        .write()
        .feed(&PresentScene::FullRebuild.feed(0, opts.rows, opts.cols));
    let mut renderer = renderer_for(&terminal);
    renderer.init(gpu);
    let mut text = garasu::TextLayerStack::new(&gpu.device, &gpu.queue, format);
    let start = Instant::now();
    let mut paint = Vec::with_capacity(opts.frames);
    let mut wait = Vec::with_capacity(opts.frames);
    for frame in 0..opts.warmup + opts.frames {
        terminal
            .write()
            .feed(&scene.feed(frame + 1, opts.rows, opts.cols));
        let mut ctx = RenderContext {
            gpu,
            text: &mut text,
            surface_view: target.view(),
            width: target.width(),
            height: target.height(),
            scale_factor: 1.0,
            elapsed: start.elapsed().as_secs_f32(),
            dt: 1.0 / 120.0,
        };
        let t0 = Instant::now();
        renderer.render(&mut ctx);
        let painted = t0.elapsed();
        let t1 = Instant::now();
        let _ = gpu.device.poll(wgpu::PollType::Wait);
        let waited = t1.elapsed();
        if let Some(i) = frame.checked_sub(opts.warmup) {
            println!(
                "{}\t{i}\t{}\t{}",
                scene.name(),
                painted.as_nanos(),
                waited.as_nanos()
            );
            paint.push(painted);
            wait.push(waited);
        }
    }
    paint.sort_unstable();
    wait.sort_unstable();
    println!(
        "# {} paint_us p50={:.1} p90={:.1} gpu_wait_us p50={:.1} p90={:.1}",
        scene.name(),
        micros(quantile(&paint, 50)),
        micros(quantile(&paint, 90)),
        micros(quantile(&wait, 50)),
        micros(quantile(&wait, 90)),
    );
}

pub fn present(opts: PresentOptions) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .context("bench present: runtime")?;
    let gpu = match runtime.block_on(garasu::GpuContext::new()) {
        Ok(gpu) => gpu,
        Err(e) => {
            println!("# blind: no GPU adapter: {e}");
            return Ok(());
        }
    };
    let info = gpu.adapter.get_info();
    println!(
        "# adapter={} backend={:?} cols={} rows={} width={} height={} frames={} warmup={}",
        info.name,
        info.backend,
        opts.cols,
        opts.rows,
        opts.width,
        opts.height,
        opts.frames,
        opts.warmup
    );
    println!("scene\tframe\tpaint_ns\tgpu_wait_ns");
    for scene in PresentScene::ALL.iter().copied() {
        run_scene(&gpu, scene, opts);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pacing_mode_reaches_a_pacing_row_and_only_demand_parks() {
        for mode in PacingMode::ALL {
            let config = PerformanceConfig {
                pacing: *mode,
                ..PerformanceConfig::default()
            };
            let row = pacing_row(config.frame_pacing());
            assert_eq!(
                row.parks,
                matches!(mode, PacingMode::Demand),
                "{mode:?}: only demand pacing parks"
            );
            assert_eq!(row.parks, row.hides, "{mode:?}");
        }
        assert!(!PacingMode::ALL.is_empty());
    }

    #[test]
    fn the_capped_control_reddens_the_cells_demand_is_graded_on() {
        let capped = pacing_row(madori::FramePacing::from_target_fps(60));
        assert_eq!(capped.control, Some("pacing-capped"));
        for cell in capped.cells {
            assert!(DEMAND_CELLS.contains(cell), "{cell} is a demand cell");
        }
    }

    #[test]
    fn every_runtime_has_a_row_and_the_resident_window_carries_the_window_cells() {
        assert_eq!(TearRuntime::ALL.len(), 3);
        for runtime in TearRuntime::ALL {
            let row = runtime_row(*runtime);
            assert!(row.case.starts_with('C'), "{runtime:?}");
        }
        let resident = runtime_row(TearRuntime::Resident);
        for cell in DEMAND_CELLS {
            assert!(resident.window_cells.contains(cell));
        }
    }

    #[test]
    fn the_rows_document_names_every_variant() {
        let v = rows();
        assert_eq!(
            v["pacings"].as_array().unwrap().len(),
            PacingMode::ALL.len()
        );
        assert_eq!(
            v["runtimes"].as_array().unwrap().len(),
            TearRuntime::ALL.len()
        );
        assert_eq!(
            v["present_scenes"].as_array().unwrap().len(),
            PresentScene::ALL.len()
        );
    }

    #[test]
    fn a_one_row_scene_rewrites_one_row_and_a_full_rebuild_every_row() {
        let one = PresentScene::OneRow.feed(5, 48, 163);
        let full = PresentScene::FullRebuild.feed(5, 48, 163);
        assert!(one.starts_with(b"\x1b[6;1H"));
        assert_eq!(full.windows(2).filter(|w| w == b"\r\n").count(), 47);
        assert!(PresentScene::Repeat.feed(5, 48, 163).is_empty());
    }
}
