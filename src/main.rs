//! Mado (窓) — GPU-rendered terminal emulator.
//!
//! Follows Ghostty's philosophy:
//! - Native GPU rendering via wgpu (Metal/Vulkan)
//! - Fast, correct VT100/xterm emulation via vte
//! - WGSL shader plugins for visual effects
//! - Hot-reloadable configuration via shikumi

// Global allocator: mimalloc. Terminal hot paths are allocation-bound
// (per-cell glyphon buffers, per-frame text_areas, per-line snapshots,
// per-row String accumulators) and benefit ~5–15% from a tuned
// small-object allocator. Ghostty / foot / wezterm all do the same.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod action_injection;
mod auto_detect;
mod clipboard_store;
mod config;
mod dir_picker;
mod e2e;
mod engate_consumer;
mod font_size;
mod glyph_class;
mod grid_damage;
mod gui_tear_attach;
mod kanshou_state;
mod keybind;
// L1 integration-test brick (docs/INTEGRATION-TESTING.md §L1): real
// shell + real TerminalSink + probe counters, headless. Unit-test
// module (not tests/) because mado is binary-only — only unit tests
// can reach crate::engate_consumer::TerminalSink.
#[cfg(test)]
mod l1_engate_loop;
mod single_pane;
mod tear_discovery;
mod caps;
mod terminfo;
mod mcp;
mod osc_1337;
mod platform;
mod pointer_shape;
mod prompt_mark;
mod pty;
mod perf;
mod render;
mod render_graph;
mod scenario;
mod search;
mod selection;
mod session;
// mod tab removed at Phase 4 — single-pane mado.
mod term_spec;
mod terminal;
mod theme;
mod url;
mod ux;
mod vigy_host;
// mod window removed at Phase 4 — single-pane mado uses single_pane.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use hasami::{Clipboard, ClipboardProvider};
use madori::event::{AppEvent, KeyEvent, MouseEvent};
use madori::EventResponse;

// SplitDir removed at Phase 4 — single-pane mado.
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::terminal::Color;
use crate::theme::Theme;
// WindowState removed at Phase 4 — single-pane mado uses single_pane::SinglePane.

#[derive(Parser)]
#[command(name = "mado", version, about = "GPU-rendered terminal emulator")]
struct Cli {
    /// Command to execute (default: user's shell)
    #[arg(short, long)]
    command: Option<String>,

    /// Configuration file override
    #[arg(long, env = "MADO_CONFIG")]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    subcmd: Option<SubCmd>,
}

#[derive(clap::Subcommand)]
enum SubCmd {
    /// Run as MCP server (stdio transport) for Claude Code integration.
    Mcp,
    /// Run the L2 end-to-end smoke matrix (docs/INTEGRATION-TESTING.md
    /// §L2): spawn `mado mcp` (this binary) as a stdio child, drive
    /// spawn_term / get_output / send_keys through a typed rmcp
    /// client, and print a JSON row summary
    /// `{shell, rows: [{name, pass, detail}], pass}` to stdout.
    /// Exits nonzero when any row fails. Matrix rows are typed Rust
    /// constants today; a shikumi matrix-YAML surface + the nix
    /// `.#e2e-mado` closure-resolved app are the documented follow-ups.
    E2e {
        /// Shell to spawn inside the smoke session. Defaults to
        /// $MADO_E2E_SHELL, then `frostmourne` resolved on PATH.
        #[arg(long, env = "MADO_E2E_SHELL", default_value = "frostmourne")]
        shell: String,
    },
    /// Detect the runtime posture (displays, refresh rates, GPU,
    /// platform) and print it as JSON. Useful for debugging "what does
    /// mado see on this machine" + fleet-wide auditing via
    /// `tend` / scripted queries.
    PrintPosture,
    /// Show the materialized config at a tier (bare/default/discovered/custom/env).
    ///
    /// Operator surface delegated to `shikumi::cli::ConfigShowCommand` —
    /// the Pillar-12 dual of `TieredConfig`. Replaces mado's prior
    /// hand-rolled `config-show <tier>` so the fleet uses one shape.
    ConfigShow(shikumi::cli::ConfigShowCommand),
    /// Run a `*.scenario.yaml` file in headless mode and exit non-zero
    /// on assertion failure. Used by `tests/scenarios.rs` to dispatch
    /// each scenario as its own process — and by operators to replay
    /// a captured scenario locally without firing up Claude.
    ScenarioRun {
        /// Path to the scenario YAML file.
        path: std::path::PathBuf,
    },
    /// Capture a terminal session as a `*.scenario.yaml` regression
    /// test. Spawns the command non-interactively in a PTY, records
    /// every byte the child writes, and emits a scenario whose single
    /// `send` step replays that byte stream against a `cat` passthrough.
    ///
    /// Typical workflow:
    ///
    /// ```bash
    /// mado record --output tests/scenarios/atuin-bug.scenario.yaml \
    ///     --cols 80 --rows 24 --name atuin-stu-search \
    ///     -- sh -c 'atuin search stu < /dev/null'
    /// ```
    ///
    /// Open the resulting YAML, add `expect:` assertions, drop into
    /// `tests/scenarios/`, and it becomes a permanent regression test.
    /// Any future commit that breaks the recorded behaviour fails CI.
    /// **Phase 3 MVP** — subscribe to a tear-daemon pane's byte
    /// stream and print it to stdout. Proves the cross-app binary
    /// integration end-to-end (mado links tear-client, opens a
    /// fresh UDS, subscribes, receives bytes). A future Phase 3.1
    /// will feed those bytes into mado's own GPU-rendered Terminal
    /// in a window — but the print mode ships first because the
    /// wire is the load-bearing piece.
    ///
    /// Typical usage:
    /// ```bash
    /// tear daemon &                          # one terminal
    /// # ... start a session via a client ...
    /// mado tear-attach <pane-id>             # another terminal
    /// ```
    TearAttach {
        /// 16-char lowercase-hex pane id (as printed by
        /// `tear list` or returned by `tear up`).
        pane: String,
        /// Daemon UDS path. Defaults to
        /// `$XDG_RUNTIME_DIR/tear.sock` (or
        /// `~/.local/share/tear/tear.sock`).
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// **Phase 3.1** — open a real mado GPU window backed by
        /// a single Terminal that subscribes to the pane's PTY
        /// byte stream. Keystrokes forward via `send_keys`;
        /// window resize forwards via `pane_resize_absolute`.
        /// Without this flag, mado runs in stdout-streaming mode
        /// (Phase 3 MVP).
        #[arg(long)]
        gpu: bool,
    },
    Record {
        /// Output scenario YAML path.
        #[arg(long)]
        output: std::path::PathBuf,
        /// Scenario name (defaults to the output filename stem).
        #[arg(long)]
        name: Option<String>,
        /// Grid width in columns (default 80).
        #[arg(long, default_value_t = 80)]
        cols: u16,
        /// Grid height in rows (default 24).
        #[arg(long, default_value_t = 24)]
        rows: u16,
        /// Description text to embed in the scenario.
        #[arg(long, default_value = "Captured via `mado record`.")]
        description: String,
        /// The command + args to run inside the PTY. Use `--` to
        /// separate from mado's own flags.
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
}

/// Detect the runtime posture via a transient winit event loop.
///
/// winit 0.30 exposes monitor queries only on `ActiveEventLoop`, which
/// you only get *inside* a running loop. We therefore run a one-tick
/// loop whose `resumed` handler captures the monitors then exits. This
/// is fine for `--print-posture` (process exits afterward) but not for
/// in-app startup detection — macOS won't allow a second `EventLoop`
/// in the same process. Startup-time detection lands in M1 by plumbing
/// a posture event through madori's own loop.
/// Phase 3 MVP — subscribe to a tear-daemon pane's PTY byte
/// stream and stream it to stdout. Proves that mado links
/// against tear-client end-to-end + the cross-app subscription
/// wire works in real time. Future Phase 3.1 will route the
/// bytes into mado's own GPU Terminal instead of stdout.
fn cmd_tear_attach(
    pane: &str,
    socket: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    // Honour user's mado config but override mode = Never (CLI
    // intent always wins over YAML `tear.mode = "never"` when the
    // user explicitly typed `mado tear-attach`).
    let mado_cfg = crate::config::load(&None).unwrap_or_default();
    let tear_cfg = crate::config::MadoTearConfig {
        socket: socket.or(mado_cfg.tear.socket.clone()),
        mode: match mado_cfg.tear.mode {
            crate::config::TearMode::Never => crate::config::TearMode::Auto,
            other => other,
        },
        ..mado_cfg.tear.clone()
    };
    let (client, socket_path) = match crate::tear_discovery::discover(&tear_cfg) {
        crate::tear_discovery::DiscoveryOutcome::Attached(c, p) => (c, p),
        crate::tear_discovery::DiscoveryOutcome::Required(msg) => {
            return Err(anyhow::anyhow!("tear-daemon required but not reachable: {msg}"));
        }
        crate::tear_discovery::DiscoveryOutcome::Fallback => {
            return Err(anyhow::anyhow!(
                "tear-daemon not reachable. Start one with `tear daemon` \
                 or set `tear.auto_spawn = true` in your mado config."
            ));
        }
    };
    // Impose mado-authored TearConfig overrides if the operator
    // set `tear.impose.*` — same shape as gui_tear_attach but for
    // stdout-streaming mode. Failures here are non-fatal: a
    // session can still proceed against the daemon's original
    // config.
    if let Some(impose) = tear_cfg.impose.as_ref() {
        if impose.has_any_override() {
            if let Ok(mut current) = client.get_config() {
                impose.apply_to(&mut current);
                if let Err(e) = client.set_config(&current) {
                    tracing::warn!(error = %e, "set_config (impose) failed");
                }
            }
        }
    }
    let pane_id: tear_types::PaneId = pane
        .parse()
        .map_err(|e: anyhow::Error| anyhow::anyhow!("invalid pane id `{pane}`: {e}"))?;

    // Signal handler so Ctrl-C cleanly exits the subscribe loop.
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let running = Arc::new(AtomicBool::new(true));
    let r2 = running.clone();
    ctrlc::set_handler(move || r2.store(false, Ordering::SeqCst))
        .map_err(|e| anyhow::anyhow!("signal handler: {e}"))?;

    eprintln!(
        "mado tear-attach: subscribing to pane {pane_id} on {} (Ctrl-C to exit)",
        socket_path.display()
    );
    let stdout = std::io::stdout();
    let handle = client
        .subscribe_pane_bytes(pane_id, move |bytes| {
            // Direct passthrough — the daemon delivers exactly the
            // bytes the PTY produced, including all SGR escapes.
            // Running this in a real terminal renders the colored
            // output correctly via the host terminal's own VT
            // parser.
            let mut out = stdout.lock();
            let _ = out.write_all(bytes);
            let _ = out.flush();
        })
        .map_err(|e| anyhow::anyhow!("subscribe: {e}"))?;
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    handle.stop();
    eprintln!("\nmado tear-attach: disconnected");
    Ok(())
}

fn detect_runtime_posture() -> anyhow::Result<garasu::adaptive::RuntimePosture> {
    use std::sync::{Arc, Mutex};
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::monitor::MonitorHandle;

    struct Capture {
        monitors: Arc<Mutex<Vec<MonitorHandle>>>,
        primary: Arc<Mutex<Option<MonitorHandle>>>,
    }

    impl ApplicationHandler for Capture {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            *self.monitors.lock().unwrap() = event_loop.available_monitors().collect();
            *self.primary.lock().unwrap() = event_loop.primary_monitor();
            event_loop.exit();
        }
        fn window_event(
            &mut self,
            _: &ActiveEventLoop,
            _: winit::window::WindowId,
            _: WindowEvent,
        ) {
        }
    }

    let event_loop = EventLoop::new()?;
    let monitors_cell = Arc::new(Mutex::new(Vec::new()));
    let primary_cell = Arc::new(Mutex::new(None));
    let mut handler = Capture {
        monitors: Arc::clone(&monitors_cell),
        primary: Arc::clone(&primary_cell),
    };
    event_loop.run_app(&mut handler)?;
    // The handler still owns Arc clones, so `try_unwrap` would fail —
    // drain via `take()` instead. After this the handler holds empty
    // cells but we're about to drop it anyway.
    let monitors = std::mem::take(&mut *monitors_cell.lock().unwrap());
    let primary = primary_cell.lock().unwrap().take();
    drop(handler);

    Ok(garasu::adaptive::detect_all(
        monitors,
        primary,
        // GPU adapter detection requires a wgpu instance/surface —
        // deferred to M1 when we can hand the existing adapter from
        // garasu's GpuContext into recommend() at hot-reload time.
        None,
    ))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Handle subcommands before loading GUI config. MCP must init
    // tracing to stderr only — stdout is the JSON-RPC framing channel
    // and any tracing line on stdout breaks the protocol.
    match cli.subcmd {
        Some(SubCmd::Mcp) => {
            shidou::init_tracing_to_stderr();
            // One-shot config read so the vigy MCP tools (and any
            // future MCP-path knobs) honour `vigy.enabled`. The MCP
            // path doesn't use the watched ConfigStore — vigy + tools
            // bind at boot and never reload.
            let mcp_config = config::load(&cli.config).unwrap_or_default();
            let rt = shidou::create_runtime()?;
            rt.block_on(async {
                if mcp_config.vigy.enabled {
                    if let Err(e) = vigy_host::init().await {
                        tracing::warn!(err = %e, "embedded vigy runtime failed to start; continuing without it");
                    }
                } else {
                    tracing::debug!("vigy disabled in config (vigy.enabled = false)");
                }
                mcp::run(mcp_config).await
            })
            .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
            return Ok(());
        }
        Some(SubCmd::E2e { ref shell }) => {
            // Tracing to stderr — stdout carries exactly one JSON
            // summary so CI / the nix `.#e2e-mado` app can parse it
            // directly.
            shidou::init_tracing_to_stderr();
            let rt = shidou::create_runtime()?;
            let summary = rt
                .block_on(e2e::run(shell))
                .map_err(|e| anyhow::anyhow!("e2e harness error: {e:#}"))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            if !summary.pass {
                // Row details are already on stdout; the exit code is
                // the machine-readable verdict (matrix discipline:
                // report every row, then fail once).
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(SubCmd::PrintPosture) => {
            let posture = detect_runtime_posture()?;
            println!("{}", serde_json::to_string_pretty(&posture)?);
            return Ok(());
        }
        Some(SubCmd::ConfigShow(cmd)) => {
            // No tracing setup — pure stdout YAML so the output
            // pipes cleanly into `diff` / `yq` / etc.
            cmd.run::<crate::config::MadoConfig>("MADO_TIER")
                .map_err(|e| anyhow::anyhow!("config-show: {e}"))?;
            return Ok(());
        }
        Some(SubCmd::ScenarioRun { ref path }) => {
            // Stderr-only tracing so the scenario harness can route
            // failure context to the test runner without interleaving
            // with the test runner's own JSON output.
            shidou::init_tracing_to_stderr();
            scenario::run_sync(path)
                .map_err(|e| anyhow::anyhow!("scenario {path:?} failed:\n{e:#}"))?;
            return Ok(());
        }
        Some(SubCmd::TearAttach { ref pane, ref socket, gpu }) => {
            shidou::init_tracing_to_stderr();
            if gpu {
                let pane_id: tear_types::PaneId = pane.parse().map_err(
                    |e: anyhow::Error| anyhow::anyhow!("invalid pane id `{pane}`: {e}"),
                )?;
                let socket_path = socket
                    .clone()
                    .unwrap_or_else(tear_types::wire::default_socket_path);
                gui_tear_attach::run(pane_id, socket_path)?;
            } else {
                cmd_tear_attach(pane, socket.clone())?;
            }
            return Ok(());
        }
        Some(SubCmd::Record {
            ref output,
            ref name,
            cols,
            rows,
            ref description,
            ref cmd,
        }) => {
            shidou::init_tracing_to_stderr();
            let rt = shidou::create_runtime()?;
            let scenario_name = name
                .clone()
                .or_else(|| {
                    output
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.trim_end_matches(".scenario").to_string())
                })
                .unwrap_or_else(|| "captured-session".to_string());
            rt.block_on(scenario::record_session(scenario::RecordOpts {
                output: output.clone(),
                name: scenario_name,
                description: description.clone(),
                cols,
                rows,
                cmd: cmd.clone(),
            }))
            .map_err(|e| anyhow::anyhow!("mado record failed:\n{e:#}"))?;
            return Ok(());
        }
        None => {}
    }

    // Windowed mado uses the default stdout-fmt subscriber.
    //
    // Default filter silences cosmic-text's per-font-load WARN
    // for legacy bitmap-only system fonts (GB18030Bitmap and
    // friends ship on macOS but cosmic-text's TrueType parser
    // can't load them — the warning is informational, not a
    // failure). `RUST_LOG` always wins if the operator sets it.
    shidou::init_tracing_with_level("info,cosmic_text::font::system=error");

    // Kick off the system-font scan on a background thread RIGHT
    // NOW so the ~150-250 ms cosmic-text scan overlaps with
    // tear discovery + window creation + wgpu init. By the time
    // madori's TextRenderer::new wants the FontSystem, the
    // preload thread has already finished and the get is a
    // thread-join that returns immediately.
    garasu::preload_fonts();

    // Launch-perf timeline. Every "phase reached" log stamps
    // milliseconds since process exec so the operator can read
    // the cold-start breakdown right out of `mado` stderr:
    //   `info mado: phase=tracing_init ms=12`
    //   `info mado: phase=config_loaded ms=48`
    //   `info mado: phase=tear_attached ms=210`
    //   `info mado: phase=first_frame_rendered ms=1180`
    // Disable by setting RUST_LOG=warn or filtering mado=warn.
    let launch_start = std::time::Instant::now();
    crate::perf::set_launch_start(launch_start);
    crate::perf::log_phase("tracing_init");

    // Hot-reload hand-off (M4 stage 2): the watch callback is a
    // dirty flag, nothing more — the shikumi store stays alive as
    // the single config source. Each render loop polls the flag per
    // frame through ux::ConfigHotReload, which diffs old→new into a
    // typed SetterCall list and runs ONLY the changed renderer
    // setters (theme / font / cursor / padding / effects). The M3
    // park-a-config cell this replaces re-applied effects only.
    let config_dirty = Arc::new(AtomicBool::new(false));
    let watcher_dirty = Arc::clone(&config_dirty);
    let (config, config_store) = config::load_and_watch(&cli.config, move |_new_config| {
        tracing::info!("config reloaded — typed setter delta applies next frame");
        watcher_dirty.store(true, Ordering::Release);
    })?;
    let config_reload_source =
        crate::ux::ConfigReloadSource::new(Arc::new(config_store), config_dirty);
    crate::perf::log_phase("config_loaded");

    // Embedded vigy reconciler runtime — gated by `vigy.enabled`
    // (defaults OFF). When disabled, no SQLite DB open, no tick
    // loops, no second tokio runtime; the vigy MCP tools return a
    // typed "disabled" error if invoked. Operators flip
    // `vigy.enabled = true` in mado.yaml to spawn the runtime +
    // register the default heartbeat.
    if config.vigy.enabled {
        std::thread::Builder::new()
            .name("vigy-runtime".into())
            .spawn(|| {
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("vigy-tokio")
                    .build()
                {
                    Ok(rt) => rt.block_on(async {
                        if let Err(e) = vigy_host::init().await {
                            tracing::warn!(err = %e, "embedded vigy runtime failed to start");
                            return;
                        }
                        tracing::info!("embedded vigy runtime live; default heartbeat ticking");
                        std::future::pending::<()>().await;
                    }),
                    Err(e) => {
                        tracing::warn!(err = %e, "could not create vigy tokio runtime; reconcilers disabled");
                    }
                }
            })
            .expect("spawn vigy-runtime thread");
    } else {
        tracing::debug!("vigy disabled in config (vigy.enabled = false)");
    }

    // ── Kanshou introspection server ─────────────────────────────────
    // Expose the GUI's live AppState (frame_perf atomics, session
    // registry, loaded config, process metadata) over a Unix socket
    // so operator tools, MCP servers, and sibling processes query
    // the actual state instead of process-local zeros. See
    // pleme-io/kanshou. Best-effort: bind failure is non-fatal —
    // mado runs without the socket and the operator sees the
    // warn-level log explaining why introspection is unavailable.
    let kanshou_state = std::sync::Arc::new(kanshou_state::MadoAppState::new(
        std::sync::Arc::new(config.clone()),
        std::sync::Arc::new(crate::session::SessionRegistry::default()),
    ));
    let kanshou_state_for_server = std::sync::Arc::clone(&kanshou_state);
    std::thread::Builder::new()
        .name("kanshou".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name("kanshou-tokio")
                .build()
            {
                Ok(rt) => rt.block_on(async {
                    match kanshou_state::spawn_server("mado", kanshou_state_for_server) {
                        Ok(path) => {
                            tracing::info!(
                                socket = %path.display(),
                                "kanshou introspection live"
                            );
                            std::future::pending::<()>().await;
                        }
                        Err(e) => {
                            tracing::warn!(err = %e, "kanshou bind failed; introspection disabled");
                        }
                    }
                }),
                Err(e) => {
                    tracing::warn!(err = %e, "could not create kanshou tokio runtime");
                }
            }
        })
        .expect("spawn kanshou thread");
    crate::perf::log_phase("kanshou_started");

    // Apply active profile if set — same resolution the hot-reload
    // source performs on every reload (config.rs::with_active_profile).
    let config = config.with_active_profile();

    tracing::debug!("mado starting with config: {:?}", config);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        theme = %config.theme,
        font = %config.font_family,
        size = config.font_size,
        "mado starting"
    );

    if crate::platform::is_dark_mode() {
        tracing::debug!("system dark mode detected");
    }

    // Adaptive posture detection at startup is deferred to M1 because
    // winit 0.30 requires a running event loop to enumerate monitors,
    // and macOS forbids a second EventLoop after the first. For now we
    // resolve target_fps against `None` (falls through to user explicit
    // or hardcoded FALLBACK_FPS); fleet operators can still inspect
    // the detected posture via `mado print-posture`. M1 plumbs a
    // typed posture event through madori so the live event loop owns
    // detection and feeds it back to the renderer.
    let runtime_posture: Option<garasu::adaptive::RuntimePosture> = None;
    let _effective_fps = config.performance.resolve_target_fps(runtime_posture.as_ref());
    tracing::debug!(target_fps = _effective_fps, "resolved target fps");

    let shell = cli
        .command
        .or(config.shell.command.clone())
        .unwrap_or_else(default_shell);

    // ── Default-on tear attachment ───────────────────────────────
    // Discover (or auto-spawn) the tear-daemon and run mado as a
    // tear-attached window with a freshly-created session. Falls
    // through to the local-PTY path below when:
    //   * tear.mode = "never"
    //   * the daemon is unreachable AND spawn failed
    // Hard errors (e.g. tear.mode = "always" + daemon dead +
    // spawn failed) bubble out via the `Error` arm.
    crate::perf::log_phase("pre_tear_attach");
    match gui_tear_attach::try_run_default(
        config.clone(),
        shell.clone(),
        std::sync::Arc::clone(&kanshou_state),
        Some(config_reload_source.clone()),
    ) {
        gui_tear_attach::TearDefaultOutcome::Ran => return Ok(()),
        gui_tear_attach::TearDefaultOutcome::Error(e) => return Err(e),
        gui_tear_attach::TearDefaultOutcome::Unavailable => {
            tracing::debug!("tear unavailable — falling through to local-PTY mode");
        }
    }

    let extra_env = config.environment.vars.clone();
    // window.inherit_working_directory resolution (M4 stage 2):
    // explicit environment.working_directory wins; knob on → None
    // (the PTY child inherits mado's process cwd = launch-shell
    // dir); knob off → $HOME. See MadoConfig::boot_spawn_cwd.
    let working_directory = config.boot_spawn_cwd();
    let initial_command = config.environment.initial_command.clone();

    let effective_font_size = config.font_size * config.accessibility.font_scale;
    let padding = config.window.padding as f32;
    let cell_w = effective_font_size * 0.6;
    let cell_h = effective_font_size * 1.4;

    let cols = ((config.window.width as f32 - 2.0 * padding) / cell_w) as usize;
    let rows = ((config.window.height as f32 - 2.0 * padding) / cell_h) as usize;
    let cols = cols.max(10);
    let rows = rows.max(3);

    let scrollback = config.behavior.scrollback_lines;

    // Phase-4 — single pane only. Multiplexing belongs in tear
    // (theory/MADO-TEAR-M5.md). One Terminal + one PTY +
    // single SinglePane wrapper, all in single_pane.rs.
    let pane = Arc::new(single_pane::spawn(
        shell,
        cols,
        rows,
        scrollback,
        config.behavior.reflow_on_resize,
        None,
        extra_env,
        working_directory,
        initial_command,
    ));
    let initial_terminal: SharedTerminal = Arc::clone(&pane.terminal);

    // ─── Theme architecture M3 (HiDPI gamma fix by construction) ──────
    //
    // sRGB → Linear → wgpu::Color via ishou_tokens. The previous
    // `parse_hex_color(...)` returning raw sRGB floats and being
    // assigned straight to `wgpu::Color { r, g, b, a }` on a
    // `Bgra8UnormSrgb` surface caused wgpu to gamma-correct the values
    // a second time — the washed-out medium-gray instead of dark Nord
    // visible in screenshots prior to M3. The typed path makes this
    // class of bug uncompilable: there is no `From<Srgb> for
    // wgpu_types::Color` in ishou-tokens — every wgpu::Color must come
    // from a LinearRgba. Architecture:
    // pleme-io/theory/THEME-ARCHITECTURE.md
    let bg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.background)
        .unwrap_or(ishou_tokens::Srgb::new(0x2e, 0x34, 0x40));
    let bg_color: wgpu::Color = bg_srgb
        .to_linear()
        .with_alpha(config.appearance.opacity)
        .into();
    let fg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.foreground)
        .unwrap_or(ishou_tokens::Srgb::new(0xec, 0xef, 0xf4));

    let cursor_blink = config.cursor.blink && !config.accessibility.reduce_motion;
    let mut renderer = TerminalRenderer::new(
        initial_terminal,
        effective_font_size,
        config.font_family.clone(),
        config.font_italic.clone(),
        config.font_symbols.clone(),
        padding,
        config.cursor.style,
        cursor_blink,
        config.cursor.blink_rate_ms,
        bg_color,
        Color::new(fg_srgb.r, fg_srgb.g, fg_srgb.b),
    );

    // Effects + accessibility — colorblind (incl. the legacy
    // accessibility.colorblind alias via MadoConfig::resolved_effects),
    // reduce_motion gating, bold_is_bright, and the snow/crt/
    // scanlines/bloom/glow knobs — through the ONE application point
    // shared with gui_tear_attach, so the two entry points cannot
    // diverge (M3 review 2026-06-12). Hot-reload re-application
    // happens via the per-frame ConfigHotReload poll below.
    renderer.apply_effects_and_accessibility(&config);

    // Selection/search/dir-picker renderer hooks are wired by
    // InputEngine::attach_to_renderer below — the engine is the only
    // path that can hold those Arcs, so the renderer cannot end up
    // with half the wiring (the historical embedded silent-highlight
    // bug class).

    // Log available themes on startup (debug)
    let theme_names: Vec<&str> = Theme::available().iter().map(|t| t.name).collect();
    tracing::debug!(themes = ?theme_names, "available themes");

    // Apply theme colors to terminal and renderer
    if let Some(theme) = Theme::by_name(&config.theme) {
        tracing::debug!(theme = theme.name, "applying terminal color theme");
        renderer.set_ansi_colors(theme.ansi);
        renderer.set_selection_bg(theme.selection_bg);
        renderer.set_cursor_color(color_to_f32_rgba(&theme.cursor, 0.85));
        // Theme bg also flows through the typed Srgb → Linear path so
        // theme-swap doesn't reintroduce the gamma confusion the
        // architecture eliminated above.
        let theme_bg: wgpu::Color = ishou_tokens::Srgb::new(
            theme.background.r,
            theme.background.g,
            theme.background.b,
        )
        .to_linear()
        .with_alpha(config.appearance.opacity)
        .into();
        renderer.set_bg_fg(theme_bg, theme.foreground);
        pane.terminal
            .write()
            .apply_theme(theme.foreground, theme.background, theme.ansi);
    }

    let pane_for_events = Arc::clone(&pane);
    let clipboard: Arc<dyn ClipboardProvider> = Arc::new(
        Clipboard::new().expect("failed to initialize clipboard"),
    );
    // Curated default baseline + operator `keybinds.custom` overrides —
    // the same assembly the kanshou `simulate_chord` resolver uses
    // (keybind::manager_from_config), so chord→Action resolution can't
    // drift between surfaces or render modes. (Pre-M1 this path loaded
    // ONLY the custom binds — doc/code drift the M1 convergence fixed.)
    let keybinds = crate::keybind::manager_from_config(&config);
    tracing::debug!(
        bindings = keybinds.bindings().len(),
        "keybindings loaded"
    );
    let behavior = crate::ux::UxBehavior::from(&config);
    let confirm_close = behavior.confirm_close;
    let pending_close = Arc::new(AtomicBool::new(false));
    // Boot-time notification dispatcher (M4 drain consumer): macOS
    // osascript backend on Darwin, tsuuchi LogBackend elsewhere.
    let notifier = crate::platform::notification_dispatcher();
    let default_font_size = effective_font_size;
    // macOS window-chrome styling latch — owns the style extracted from
    // the shikumi config plus its applied state, so the `'static`
    // event-loop closure carries one small value. Ticked on every
    // redraw until a window actually exists. Every axis is operator-
    // configurable: `window.macos.*` + `appearance.background`.
    let mut native_styling = crate::platform::NativeStylingLatch::from_config(&config);
    // Watched-config delta driver (M4 stage 2) — seeded with the
    // boot config so the first reload diffs against what this
    // renderer was actually built from. Polled once per frame in
    // the RedrawRequested arm.
    let mut hot_reload =
        crate::ux::ConfigHotReload::new(config_reload_source, config.clone());

    // ── M1 unified input/UX engine ───────────────────────────────
    // Every UX capability (selection, copy/paste, search + dir-picker
    // overlays, mouse forwarding, kitty CSI-u, focus, IME, font zoom,
    // the PTY-grid⇄display reconciler) lives in ux::InputEngine; this
    // event loop is a thin adapter (tests/ux_unification.rs pins that
    // structurally). The local-PTY divergences are injected here:
    // PTY writes → input_tx, grid pushes → resize_tx (PTY winsize),
    // DECCKM → mirror-Terminal read.
    let pty_sink: Box<dyn crate::ux::PtySink> = {
        let pane_for_pty = Arc::clone(&pane);
        Box::new(move |bytes: &[u8]| pane_for_pty.send_input(bytes.to_vec()))
    };
    let resize_sink: Box<dyn crate::ux::ResizeSink> = {
        let pane_for_resize = Arc::clone(&pane);
        Box::new(move |cols: u16, rows: u16| pane_for_resize.resize(cols, rows))
    };
    let cursor_keys_mode: Box<dyn Fn() -> bool + Send + Sync> = {
        let term = Arc::clone(&pane.terminal);
        Box::new(move || term.read().cursor_keys_mode())
    };
    let mut engine = crate::ux::InputEngine::attach_to_renderer(
        &mut renderer,
        crate::ux::InputEngineParams {
            terminal: Arc::clone(&pane.terminal),
            pty: pty_sink,
            resize: resize_sink,
            shared: crate::ux::SharedUxState {
                selection: Arc::clone(&pane.selection),
                search: Arc::clone(&pane.search),
                dir_picker: Arc::clone(&pane.dir_picker),
            },
            clipboard: Arc::clone(&clipboard),
            keybinds,
            behavior,
            cursor_keys_mode,
            default_font_size,
            padding,
        },
    );

    let app_config = madori::AppConfig {
        title: "mado".into(),
        width: config.window.width,
        height: config.window.height,
        resizable: true,
        vsync: config.performance.vsync,
        transparent: false,
        decorations: config.window.decorations,
    };
    madori::App::builder(renderer)
        .config(app_config)
        .on_event(move |event, renderer| -> EventResponse {
            // Check if PTY has exited — request window close
            if pane_for_events.any_exited() {
                return exit_response(confirm_close, &pending_close);
            }

            // M1 adapter: each arm translates AppEvent fields into one
            // InputEngine call and maps the typed EventOutcome back to
            // madori's EventResponse. Loop-specific concerns that stay
            // here: child-exit close (above), NativeStylingLatch ticks,
            // snow pulses, and the per-frame M4 side-effect drain
            // (one drain_side_effects call routed through the shared
            // ux::apply_side_effects consumer).
            match event {
                AppEvent::Key(key_event @ KeyEvent { pressed: true, .. }) => {
                    // Snow overlay: every keystroke pulses the
                    // typing-shimmer; pulse decays per frame at ~0.5s
                    // half-life so rapid typing builds up brightness.
                    renderer.snow_pulse_typing();
                    engine.on_key(key_event, renderer).into()
                }
                // IME commit — forward composed text to PTY
                AppEvent::Ime(madori::ImeEvent::Commit(text)) => {
                    engine.on_ime_commit(text).into()
                }
                // Mouse button events — selection or forward to PTY
                AppEvent::Mouse(MouseEvent::Button {
                    button,
                    pressed,
                    x,
                    y,
                    modifiers,
                }) => engine
                    .on_mouse_button(*button, *pressed, *x, *y, *modifiers, renderer)
                    .into(),
                // Mouse move — update selection drag or forward to PTY
                AppEvent::Mouse(MouseEvent::Moved { x, y }) => {
                    // Snow overlay: track cursor for the deflection ring
                    // on the near-layer flakes.
                    renderer.snow_set_cursor(*x as f32, *y as f32);
                    engine.on_mouse_moved(*x, *y, renderer).into()
                }
                AppEvent::Mouse(MouseEvent::Scroll { dy, .. }) => {
                    engine.on_mouse_scroll(*dy, renderer).into()
                }
                // Focus events → engine emits ESC[I/ESC[O when focus
                // reporting (mode 1004) is enabled.
                AppEvent::Focused(focused) => {
                    renderer.set_focused(*focused);
                    engine.on_focus(*focused).into()
                }
                AppEvent::CloseRequested => exit_response(confirm_close, &pending_close),
                AppEvent::Resized { width, height } => {
                    engine.on_resize(*width, *height, renderer).into()
                }
                // Drain terminal side effects once per frame (M4)
                AppEvent::RedrawRequested => {
                    // Chrome styling retries inside the latch until a
                    // window exists — the first redraws can tick before
                    // AppKit registers the window.
                    native_styling.tick();
                    // Watched-config edits: poll the dirty flag and
                    // apply the typed setter delta (M4 stage 2) —
                    // BEFORE this frame's render reads the config-
                    // derived state.
                    hot_reload.poll_config_reload(renderer);
                    // PTY-grid ⇄ display reconciler — engine-owned
                    // latch over the rendered-surface signature (same
                    // contract as the tear path).
                    engine.on_redraw_tick(renderer);
                    // ONE typed drain + ONE shared consumer — the M4
                    // seam. Title change-edges come back typed; the
                    // adapter owns only the EventResponse translation.
                    let ws = &*pane_for_events;
                    if let Some(pane) = ws.focused_pane() {
                        let effects = pane.terminal.write().drain_side_effects();
                        if let Some(title) = crate::ux::apply_side_effects(
                            effects,
                            renderer,
                            &*clipboard,
                            &notifier,
                        ) {
                            return EventResponse {
                                set_title: Some(title),
                                ..Default::default()
                            };
                        }
                    }
                    EventResponse::ignored()
                }
                _ => EventResponse::ignored(),
            }
        })
        .run()
        .map_err(|e| anyhow::anyhow!("madori error: {e}"))?;

    Ok(())
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}

/// Build EventResponse for exit request, applying confirm_close logic when enabled.
fn exit_response(
    confirm_close: bool,
    pending_close: &AtomicBool,
) -> EventResponse {
    if !confirm_close {
        return EventResponse {
            consumed: true,
            exit: true,
            ..Default::default()
        };
    }
    if pending_close.swap(false, Ordering::SeqCst) {
        return EventResponse {
            consumed: true,
            exit: true,
            ..Default::default()
        };
    }
    pending_close.store(true, Ordering::SeqCst);
    tracing::info!("Press close again to exit");
    EventResponse {
        consumed: true,
        set_title: Some("mado — press close again to exit".into()),
        ..Default::default()
    }
}

/// Convert a terminal `Color` (sRGB-byte) + alpha into the linear
/// `[f32; 4]` shape the rect-pipeline shader writes into a
/// `Bgra8UnormSrgb` surface. Goes through ishou-tokens' typed
/// `Srgb::to_linear` — see the gamma docstring on
/// `render::color_to_f32` (it's the same correctness argument; this
/// variant just carries an explicit alpha channel for selection /
/// cursor / search-highlight overlays).
fn color_to_f32_rgba(c: &Color, alpha: f32) -> [f32; 4] {
    let linear = ishou_tokens::Srgb::new(c.r, c.g, c.b).to_linear();
    [linear.r, linear.g, linear.b, alpha]
}

// `parse_hex_color` and `parse_hex_rgb` removed in theme architecture
// M3 — every consumer now flows through `ishou_tokens::Srgb::from_hex`
// + `to_linear`. Keeping local hex parsers would re-introduce the
// untyped path that produced the gamma bug; their absence is part of
// the type-level guarantee.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    // ---- hex parsing (now delegated to ishou_tokens::Srgb) ----
    //
    // The hex-parse + gamma path moved into ishou-tokens at M3. These
    // tests pin the typed call site shape mado uses — equivalent
    // coverage to the deleted `parse_hex_color` / `parse_hex_rgb`
    // tests, but with the gamma-correct path the architecture now
    // mandates.

    #[test]
    fn ishou_srgb_from_hex_black() {
        let s = ishou_tokens::Srgb::from_hex("#000000").unwrap();
        assert_eq!(s, ishou_tokens::Srgb::new(0, 0, 0));
        let l = s.to_linear();
        assert!((l.r - 0.0).abs() < 1e-6);
    }

    #[test]
    fn ishou_srgb_from_hex_white() {
        let s = ishou_tokens::Srgb::from_hex("#ffffff").unwrap();
        assert_eq!(s, ishou_tokens::Srgb::new(255, 255, 255));
        let l = s.to_linear();
        assert!((l.r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ishou_srgb_from_hex_nord_polar_night_0() {
        // The exact colour the M3 gamma fix was about. After M3,
        // `#2e3440` reaches a wgpu surface through
        // `Srgb → Linear → LinearRgba → wgpu::Color`, producing the
        // operator-perceived dark Nord — not the washed-out medium
        // gray the direct-sRGB-write path produced before.
        let s = ishou_tokens::Srgb::from_hex("#2e3440").unwrap();
        let l = s.to_linear();
        // Same canonical values pinned in ishou-tokens' tests.
        assert!((l.r - 0.027_320_892).abs() < 1e-5);
        assert!((l.g - 0.034_339_808).abs() < 1e-5);
    }

    #[test]
    fn ishou_srgb_from_hex_no_hash_prefix() {
        // mirrors the legacy parse_hex_color "no hash" test.
        let a = ishou_tokens::Srgb::from_hex("ffffff");
        let b = ishou_tokens::Srgb::from_hex("#ffffff");
        assert_eq!(a, b);
    }

    #[test]
    fn ishou_srgb_from_hex_empty_returns_none() {
        assert!(ishou_tokens::Srgb::from_hex("").is_none());
    }

    #[test]
    fn ishou_srgb_from_hex_short_returns_none() {
        // The legacy `parse_hex_color` silently returned the default
        // on short input; the typed `Srgb::from_hex` returns None so
        // the call site explicitly chooses a fallback. This is the
        // architectural win: malformed input cannot silently pretend
        // to be a colour.
        assert!(ishou_tokens::Srgb::from_hex("#abc").is_none());
    }

    #[test]
    fn ishou_srgb_from_hex_invalid_returns_none() {
        assert!(ishou_tokens::Srgb::from_hex("#zzzzzz").is_none());
    }

    // cursor_key + f_key_escape moved into `keybind::cursor_key_bytes`
    // and `keybind::f_key_escape` as part of the input-encoding
    // consolidation. Coverage lives next to the helpers — see the
    // `cursor_keys_normal_mode_emit_csi`, `cursor_keys_application_
    // mode_emit_ss3`, and `f_keys_map_to_xterm_sequences` tests in
    // `keybind.rs`. The `embedded_tear_flow_ctrl_r_reaches_pty` test
    // there is the dedicated regression guard for the 2026-05-26
    // Ctrl-R bug.

    // ---- default_shell ----

    #[test]
    fn test_default_shell_is_nonempty() {
        let shell = default_shell();
        assert!(!shell.is_empty());
    }

    // ---- exit_response ----

    #[test]
    fn test_exit_response_no_confirm() {
        let pending = AtomicBool::new(false);
        let resp = exit_response(false, &pending);
        assert!(resp.consumed);
        assert!(resp.exit);
    }

    #[test]
    fn test_exit_response_confirm_first_press() {
        let pending = AtomicBool::new(false);
        let resp = exit_response(true, &pending);
        assert!(resp.consumed);
        assert!(!resp.exit, "first close with confirm should NOT exit");
        assert!(pending.load(Ordering::SeqCst), "pending should be set");
    }

    #[test]
    fn test_exit_response_confirm_second_press() {
        let pending = AtomicBool::new(true);
        let resp = exit_response(true, &pending);
        assert!(resp.consumed);
        assert!(resp.exit, "second close with confirm SHOULD exit");
    }

    // ---- with_cursor_visibility ----
    //
    // The helper moved to `ux::EventOutcome::with_cursor_visibility`
    // at M1; same asserted behavior, new seam — including the map
    // onto madori's EventResponse the adapter performs.

    #[test]
    fn test_with_cursor_visibility_true() {
        let resp: EventResponse = crate::ux::EventOutcome::consumed()
            .with_cursor_visibility(Some(true))
            .into();
        assert_eq!(resp.set_cursor_visible, Some(true));
        assert!(resp.consumed);
    }

    #[test]
    fn test_with_cursor_visibility_false() {
        let resp: EventResponse = crate::ux::EventOutcome::consumed()
            .with_cursor_visibility(Some(false))
            .into();
        assert_eq!(resp.set_cursor_visible, Some(false));
    }

    #[test]
    fn test_with_cursor_visibility_none() {
        let resp: EventResponse = crate::ux::EventOutcome::consumed()
            .with_cursor_visibility(None)
            .into();
        assert!(resp.set_cursor_visible.is_none());
    }

    // ---- color_to_f32_rgba ----

    #[test]
    fn test_color_to_f32_rgba_white() {
        let c = Color::WHITE;
        let result = color_to_f32_rgba(&c, 1.0);
        assert_eq!(result, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn test_color_to_f32_rgba_black_with_alpha() {
        let c = Color::BLACK;
        let result = color_to_f32_rgba(&c, 0.5);
        assert_eq!(result, [0.0, 0.0, 0.0, 0.5]);
    }

    // The kitty_encode_key / kitty_key_seq / kitty_tilde_seq tests
    // moved to `keybind::tests` with the functions themselves when
    // the encoder was promoted to the shared module (so the
    // embedded-tear path can consume it too).

    #[test]
    fn test_default_shell_contains_path() {
        let shell = default_shell();
        assert!(shell.contains('/') || shell.contains("sh"));
    }

    #[test]
    fn test_exit_response_without_confirm() {
        let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resp = exit_response(false, &pending);
        assert!(resp.exit);
    }

    // The hex-parse smoke tests above (test_parse_hex_color_red etc.)
    // moved to `ishou_srgb_from_hex_*` callers earlier in this module
    // after the M3 migration replaced both `parse_hex_color` and
    // `parse_hex_rgb` with `ishou_tokens::Srgb::from_hex`.

    #[test]
    fn test_color_to_rgba_white_full() {
        let c = crate::terminal::Color::WHITE;
        let rgba = color_to_f32_rgba(&c, 1.0);
        assert!((rgba[0] - 1.0).abs() < 0.01);
        assert!((rgba[1] - 1.0).abs() < 0.01);
        assert!((rgba[2] - 1.0).abs() < 0.01);
        assert!((rgba[3] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_color_to_rgba_black_half() {
        let c = crate::terminal::Color::BLACK;
        let rgba = color_to_f32_rgba(&c, 0.5);
        assert!(rgba[0].abs() < 0.01);
        assert!(rgba[1].abs() < 0.01);
        assert!(rgba[2].abs() < 0.01);
        assert!((rgba[3] - 0.5).abs() < 0.01);
    }
}
