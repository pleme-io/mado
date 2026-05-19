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

mod clipboard_store;
mod config;
mod gui_tear_attach;
mod keybind;
mod single_pane;
mod tear_discovery;
mod mcp;
mod osc_1337;
mod scripting;
mod platform;
mod pointer_shape;
mod prompt_mark;
mod pty;
mod perf;
mod render;
mod render_snow;
mod scenario;
mod search;
mod selection;
mod session;
// mod tab removed at Phase 4 — single-pane mado.
mod term_spec;
mod terminal;
mod theme;
mod url;
// mod window removed at Phase 4 — single-pane mado uses single_pane.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::Parser;
use hasami::{Clipboard, ClipboardProvider};
use madori::event::{AppEvent, KeyEvent, MouseEvent};
use madori::EventResponse;

use crate::keybind::{Action, KeybindManager};
// SplitDir removed at Phase 4 — single-pane mado.
use crate::render::{SharedTerminal, TerminalRenderer};
use crate::scripting::{ScriptEvent, ScriptManager};
use crate::selection::CellPos;
use crate::terminal::{Color, MouseMode};
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
    /// Detect the runtime posture (displays, refresh rates, GPU,
    /// platform) and print it as JSON. Useful for debugging "what does
    /// mado see on this machine" + fleet-wide auditing via
    /// `tend` / scripted queries.
    PrintPosture,
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
            let rt = shidou::create_runtime()?;
            rt.block_on(mcp::run())
                .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
            return Ok(());
        }
        Some(SubCmd::PrintPosture) => {
            let posture = detect_runtime_posture()?;
            println!("{}", serde_json::to_string_pretty(&posture)?);
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

    let (config, _config_store) = config::load_and_watch(&cli.config, |new_config| {
        tracing::debug!("config reloaded: {:?}", new_config);
    })?;
    crate::perf::log_phase("config_loaded");

    // Apply active profile if set
    let config = match &config.active_profile {
        Some(name) => config.with_profile(name),
        None => config,
    };

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
    match gui_tear_attach::try_run_default(config.clone(), shell.clone()) {
        gui_tear_attach::TearDefaultOutcome::Ran => return Ok(()),
        gui_tear_attach::TearDefaultOutcome::Error(e) => return Err(e),
        gui_tear_attach::TearDefaultOutcome::Unavailable => {
            tracing::debug!("tear unavailable — falling through to local-PTY mode");
        }
    }

    let extra_env = config.environment.vars.clone();
    let working_directory = config.environment.working_directory.clone();
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
        padding,
        config.cursor.style,
        cursor_blink,
        config.cursor.blink_rate_ms,
        bg_color,
        Color::new(fg_srgb.r, fg_srgb.g, fg_srgb.b),
    );

    // Apply accessibility and appearance settings from config
    renderer.set_colorblind_mode(config.accessibility.colorblind);
    renderer.set_bold_is_bright(config.appearance.bold_is_bright);
    renderer.set_reduce_motion(config.accessibility.reduce_motion);
    // Snow overlay — the default mado effect. Pre-init so the
    // overlay constructor in `init()` picks up the operator's
    // knobs (intensity, wind, accumulation, layer_count, enabled).
    renderer.set_snow_config(config.effects.snow.clone());

    // Set selection/search on renderer (Phase 4 — single pane).
    renderer.set_selection(Arc::clone(&pane.selection));
    renderer.set_search(Arc::clone(&pane.search));

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

    let scripts = ScriptManager::new();
    scripts.fire_event(ScriptEvent::OnStart);

    let pane_for_events = Arc::clone(&pane);
    let clipboard = Arc::new(Mutex::new(
        Clipboard::new().expect("failed to initialize clipboard"),
    ));
    let mut keybinds = KeybindManager::new();
    for entry in &config.keybinds.custom {
        if let Some(action) = crate::keybind::parse_action(&entry.action) {
            match keybinds.bind_str(&entry.trigger, action) {
                Ok(()) => tracing::debug!(trigger = %entry.trigger, action = %entry.action, "custom keybind loaded"),
                Err(e) => tracing::warn!(trigger = %entry.trigger, error = %e, "failed to parse custom keybind trigger"),
            }
        } else {
            tracing::warn!(action = %entry.action, "unknown keybind action");
        }
    }
    tracing::debug!(
        bindings = keybinds.bindings().len(),
        "keybindings loaded"
    );
    let confirm_close = config.behavior.confirm_close;
    let copy_on_select = config.behavior.copy_on_select;
    let mouse_scroll_multiplier = config.behavior.mouse_scroll_multiplier;
    let mouse_hide_while_typing = config.behavior.mouse_hide_while_typing;
    let pending_close = Arc::new(AtomicBool::new(false));
    let mouse_visible = Arc::new(AtomicBool::new(true));
    // Track last known title to detect changes
    let mut last_title: Option<String> = None;
    // Track window dimensions for split resize
    let mut last_width = config.window.width;
    let mut last_height = config.window.height;
    // Double/triple click tracking
    let default_font_size = effective_font_size;
    let mut last_click_time = Instant::now();
    let mut click_count: u8 = 0;
    let mut last_click_pos = CellPos { row: 0, col: 0 };
    let mut native_styling_applied = false;

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
            {
                let ws = &*pane_for_events;
                if ws.any_exited() {
                    return exit_response(confirm_close, &pending_close);
                }
            }

            match event {
                AppEvent::Key(KeyEvent {
                    pressed: true,
                    text,
                    key,
                    modifiers,
                    ..
                }) => {
                    // Snow overlay: every keystroke pulses the
                    // typing-shimmer; pulse decays per frame at ~0.5s
                    // half-life so rapid typing builds up brightness.
                    renderer.snow_pulse_typing();
                    let hide_cursor =
                        mouse_hide_while_typing && mouse_visible.swap(false, Ordering::SeqCst);
                    // Map winit modifiers to awase modifiers
                    let mut awase_mods = awase::Modifiers::NONE;
                    if modifiers.ctrl {
                        awase_mods = awase_mods | awase::Modifiers::CTRL;
                    }
                    if modifiers.alt {
                        awase_mods = awase_mods | awase::Modifiers::ALT;
                    }
                    if modifiers.shift {
                        awase_mods = awase_mods | awase::Modifiers::SHIFT;
                    }
                    if modifiers.meta {
                        awase_mods = awase_mods | awase::Modifiers::CMD;
                    }

                    // Keep track of raw modifiers for non-keybind usage
                    let mods = modifiers;

                    // Determine awase key from the event
                    let awase_key = winit_to_awase_key(key, text);

                    // Check for keybind action first
                    let action = awase_key
                        .map(|k| awase::Hotkey::new(awase_mods, k))
                        .and_then(|hk| keybinds.lookup(&hk));

                    // Handle search mode input
                    {
                        let ws = &*pane_for_events;
                        if let Some(pane) = ws.focused_pane() {
                            let search_active = pane.search.lock().unwrap().active;
                            if search_active {
                                match action {
                                    Some(Action::SearchClose) => {
                                        pane.search.lock().unwrap().close();
                                        return with_cursor_visibility(
                                            EventResponse::consumed(),
                                            hide_cursor.then_some(false),
                                        );
                                    }
                                    Some(Action::SearchNext) => {
                                        pane.search.lock().unwrap().next();
                                        return with_cursor_visibility(
                                            EventResponse::consumed(),
                                            hide_cursor.then_some(false),
                                        );
                                    }
                                    Some(Action::SearchPrev) => {
                                        pane.search.lock().unwrap().prev();
                                        return with_cursor_visibility(
                                            EventResponse::consumed(),
                                            hide_cursor.then_some(false),
                                        );
                                    }
                                    _ => {}
                                }
                                // In search mode, typing updates the query (no modifiers)
                                if !mods.ctrl && !mods.meta && !mods.alt {
                                    if let Some(text) = text {
                                        if !text.is_empty() {
                                            let mut s = pane.search.lock().unwrap();
                                            let mut query = s.query.clone();
                                            query.push_str(text);
                                            let term = pane.terminal.read();
                                            let rows: Vec<_> =
                                                term.visible_rows().map(|r| r.to_vec()).collect();
                                            let cols = term.cols();
                                            drop(term);
                                            s.set_query(&query, &rows, cols);
                                            return with_cursor_visibility(
                                                EventResponse::consumed(),
                                                hide_cursor.then_some(false),
                                            );
                                        }
                                    }
                                    // Handle backspace in search
                                    if matches!(key, madori::event::KeyCode::Backspace) {
                                        let mut s = pane.search.lock().unwrap();
                                        let mut query = s.query.clone();
                                        query.pop();
                                        let term = pane.terminal.read();
                                        let rows: Vec<_> =
                                            term.visible_rows().map(|r| r.to_vec()).collect();
                                        let cols = term.cols();
                                        drop(term);
                                        s.set_query(&query, &rows, cols);
                                        return with_cursor_visibility(
                                            EventResponse::consumed(),
                                            hide_cursor.then_some(false),
                                        );
                                    }
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                        }
                    }

                    // Dispatch keybind action
                    if let Some(action) = action {
                        match action {
                            Action::Copy => {
                                let selected_text = {
                                    let ws = &*pane_for_events;
                                    ws.focused_pane().and_then(|pane| {
                                        let sel = pane.selection.lock().unwrap();
                                        let term = pane.terminal.read();
                                        let rows: Vec<_> =
                                            term.visible_rows().map(|r| r.to_vec()).collect();
                                        let cols = term.cols();
                                        drop(term);
                                        sel.extract_text(&rows, cols)
                                    })
                                };
                                if let Some(text) = selected_text {
                                    if let Ok(cb) = clipboard.lock() {
                                        let _ = cb.copy_text(&text);
                                    }
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::Paste => {
                                let pasted_text =
                                    clipboard.lock().ok().and_then(|cb| cb.paste_text().ok());
                                if let Some(pasted) = pasted_text {
                                    if !pasted.is_empty() {
                                        let ws = &*pane_for_events;
                                        if let Some(pane) = ws.focused_pane() {
                                            let term = pane.terminal.read();
                                            let bracketed = term.bracketed_paste();
                                            drop(term);
                                            if bracketed {
                                                let _ =
                                                    pane.input_tx.send(b"\x1b[200~".to_vec());
                                            }
                                            let _ = pane.input_tx.send(pasted.into_bytes());
                                            if bracketed {
                                                let _ =
                                                    pane.input_tx.send(b"\x1b[201~".to_vec());
                                            }
                                        }
                                    }
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::SearchOpen => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    pane.search.lock().unwrap().open();
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::SearchClose | Action::SearchNext | Action::SearchPrev => {
                                // Already handled above when search is active
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::ScrollPageUp => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    let mut term = pane.terminal.write();
                                    let page = term.rows();
                                    term.scroll_up(page);
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::ScrollPageDown => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    let mut term = pane.terminal.write();
                                    let page = term.rows();
                                    term.scroll_down(page);
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            // Action::SplitHorizontal / SplitVertical /
                            // ClosePane / FocusNext / FocusPrev removed
                            // at Phase 4 — tear owns multiplexing.
                            Action::FontIncrease => {
                                let new_size = renderer.font_size() + 1.0;
                                renderer.set_font_size(new_size);
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let ws = &*pane_for_events;
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::FontDecrease => {
                                let new_size = renderer.font_size() - 1.0;
                                renderer.set_font_size(new_size);
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let ws = &*pane_for_events;
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::FontReset => {
                                renderer.set_font_size(default_font_size);
                                let cw = renderer.cell_width();
                                let ch = renderer.cell_height();
                                let ws = &*pane_for_events;
                                ws.resize_panes(
                                    last_width as f32,
                                    last_height as f32,
                                    padding,
                                    cw,
                                    ch,
                                );
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::ScrollToTop => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    pane.terminal.write().scroll_to_top();
                                }
                                return EventResponse::consumed();
                            }
                            Action::ScrollToBottom => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    pane.terminal.write().scroll_to_bottom();
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::ResetTerminal => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    pane.terminal.write().reset();
                                }
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                            // Action::NewTab / CloseTab / NextTab /
                            // PrevTab removed at Phase 4 — tear's job.
                            Action::ToggleFullscreen => {
                                return with_cursor_visibility(
                                    EventResponse {
                                        consumed: true,
                                        toggle_fullscreen: true,
                                        ..Default::default()
                                    },
                                    hide_cursor.then_some(false),
                                );
                            }
                            Action::ScrollUp | Action::ScrollDown => {
                                // These are handled by scroll wheel events
                            }
                            Action::PasteFromSelection => {
                                let pasted = clipboard.lock().ok().and_then(|cb| cb.paste_text().ok());
                                if let Some(text) = pasted {
                                    let ws = &*pane_for_events;
                                    ws.send_input(text.into_bytes());
                                }
                            }
                            Action::JumpToPrompt | Action::JumpToPromptPrev => {
                                // Scroll to the previous OSC 133 A mark (ghostty's
                                // canonical Cmd-Up binding).
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    let mut term = pane.terminal.write();
                                    if let Some(target) = term.scroll_offset_to_prev_prompt() {
                                        let delta = target.saturating_sub(term.scroll_offset());
                                        if delta > 0 {
                                            term.scroll_up(delta);
                                        } else {
                                            let back = term.scroll_offset().saturating_sub(target);
                                            term.scroll_down(back);
                                        }
                                        tracing::trace!(target, "jumped to prev prompt");
                                    } else {
                                        tracing::debug!("no prompt marks above viewport");
                                    }
                                }
                            }
                            Action::JumpToPromptNext => {
                                // Scroll to the next OSC 133 A mark forward from
                                // the current view top (ghostty's Cmd-Down).
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    let mut term = pane.terminal.write();
                                    if let Some(target) = term.scroll_offset_to_next_prompt() {
                                        let cur = term.scroll_offset();
                                        if target < cur {
                                            term.scroll_down(cur - target);
                                        } else if target > cur {
                                            term.scroll_up(target - cur);
                                        }
                                        tracing::trace!(target, "jumped to next prompt");
                                    } else {
                                        tracing::debug!("no prompt marks below viewport");
                                    }
                                }
                            }
                            Action::ClearScreen => {
                                let ws = &*pane_for_events;
                                ws.send_input(b"\x0c".to_vec()); // Ctrl+L
                            }
                            Action::SelectAll => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    let term = pane.terminal.read();
                                    let cols = term.cols();
                                    let num_rows = term.rows();
                                    drop(term);
                                    let mut sel = pane.selection.lock().unwrap();
                                    sel.start(CellPos { row: 0, col: 0 });
                                    sel.update(CellPos { row: num_rows.saturating_sub(1), col: cols.saturating_sub(1) });
                                    sel.finish();
                                }
                            }
                            Action::CopyUrlToClipboard => {
                                let ws = &*pane_for_events;
                                if let Some(pane) = ws.focused_pane() {
                                    let term = pane.terminal.read();
                                    let cols = term.cols();
                                    let cursor_row = term.cursor().row;
                                    let row_cells: Vec<_> = (0..cols).map(|c| term.cell(cursor_row, c).clone()).collect();
                                    drop(term);
                                    let urls = crate::url::detect_urls_in_row(&row_cells, cols, cursor_row);
                                    if let Some(url) = urls.first() {
                                        if let Ok(cb) = clipboard.lock() {
                                            let _ = cb.copy_text(&url.url);
                                        }
                                    }
                                }
                            }
                            Action::ToggleMouseReporting => {
                                tracing::info!("mouse reporting toggled");
                            }
                        }
                    }

                    // Clear selection on any key press
                    {
                        let ws = &*pane_for_events;
                        if let Some(pane) = ws.focused_pane() {
                            pane.selection.lock().unwrap().clear();
                        }
                    }

                    // Kitty keyboard protocol: if active, encode keys using the protocol
                    {
                        let ws = &*pane_for_events;
                        if let Some(pane) = ws.focused_pane() {
                            let kitty_flags =
                                pane.terminal.read().kitty_keyboard_flags();
                            if kitty_flags > 0 {
                                if let Some(encoded) =
                                    kitty_encode_key(key, text, modifiers, kitty_flags)
                                {
                                    let _ = pane.input_tx.send(encoded);
                                    return with_cursor_visibility(
                                        EventResponse::consumed(),
                                        hide_cursor.then_some(false),
                                    );
                                }
                            }
                        }
                    }

                    // Handle text input
                    if let Some(text) = text {
                        if !text.is_empty() {
                            // Ctrl+letter → control byte (0x01..0x1A)
                            if modifiers.ctrl && text.len() == 1 {
                                let ch = text.chars().next().unwrap();
                                if ch.is_ascii_alphabetic() {
                                    let ctrl_byte =
                                        (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                                    let ws = &*pane_for_events;
                                    ws.send_input(vec![ctrl_byte]);
                                    return with_cursor_visibility(
                                        EventResponse::consumed(),
                                        hide_cursor.then_some(false),
                                    );
                                }
                            }

                            // Alt+key → ESC prefix + character
                            if modifiers.alt {
                                let mut bytes = vec![0x1b];
                                bytes.extend_from_slice(text.as_bytes());
                                let ws = &*pane_for_events;
                                ws.send_input(bytes);
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }

                            let ws = &*pane_for_events;
                            ws.send_input(text.as_bytes().to_vec());
                            return with_cursor_visibility(
                                EventResponse::consumed(),
                                hide_cursor.then_some(false),
                            );
                        }
                    }

                    // Handle keys without text
                    let ws = &*pane_for_events;
                    let app_mode = ws
                        .focused_pane()
                        .map(|p| p.terminal.read().cursor_keys_mode())
                        .unwrap_or(false);

                    // Ctrl+letter for Char keys without text
                    if modifiers.ctrl {
                        if let madori::event::KeyCode::Char(ch) = key {
                            if ch.is_ascii_alphabetic() {
                                let ctrl_byte = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                                ws.send_input(vec![ctrl_byte]);
                                return with_cursor_visibility(
                                    EventResponse::consumed(),
                                    hide_cursor.then_some(false),
                                );
                            }
                        }
                    }

                    // Alt+char for Char keys without text
                    if modifiers.alt {
                        if let madori::event::KeyCode::Char(ch) = key {
                            let mut bytes = vec![0x1b];
                            let mut buf = [0u8; 4];
                            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            ws.send_input(bytes);
                            return with_cursor_visibility(
                                EventResponse::consumed(),
                                hide_cursor.then_some(false),
                            );
                        }
                    }

                    let bytes: Option<Vec<u8>> = match key {
                        madori::event::KeyCode::Enter => Some(b"\r".to_vec()),
                        madori::event::KeyCode::Backspace => Some(b"\x7f".to_vec()),
                        madori::event::KeyCode::Tab => Some(b"\t".to_vec()),
                        madori::event::KeyCode::Escape => Some(b"\x1b".to_vec()),
                        madori::event::KeyCode::Space => Some(b" ".to_vec()),
                        // Cursor keys: application mode (ESC O x) vs normal (ESC [ x)
                        madori::event::KeyCode::Up => Some(cursor_key(b'A', app_mode)),
                        madori::event::KeyCode::Down => Some(cursor_key(b'B', app_mode)),
                        madori::event::KeyCode::Right => Some(cursor_key(b'C', app_mode)),
                        madori::event::KeyCode::Left => Some(cursor_key(b'D', app_mode)),
                        madori::event::KeyCode::Home => Some(cursor_key(b'H', app_mode)),
                        madori::event::KeyCode::End => Some(cursor_key(b'F', app_mode)),
                        madori::event::KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
                        madori::event::KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
                        madori::event::KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
                        // F1-F12
                        madori::event::KeyCode::F(n) => Some(f_key_escape(*n)),
                        _ => None,
                    };
                    if let Some(bytes) = bytes {
                        ws.send_input(bytes);
                        return with_cursor_visibility(
                            EventResponse::consumed(),
                            hide_cursor.then_some(false),
                        );
                    }
                    with_cursor_visibility(
                        EventResponse::ignored(),
                        hide_cursor.then_some(false),
                    )
                }
                // IME commit — forward composed text to PTY
                AppEvent::Ime(madori::ImeEvent::Commit(text)) => {
                    if !text.is_empty() {
                        let ws = &*pane_for_events;
                        ws.send_input(text.as_bytes().to_vec());
                    }
                    EventResponse::consumed()
                }
                // Mouse button events — selection or forward to PTY
                AppEvent::Mouse(MouseEvent::Button {
                    button,
                    pressed,
                    x,
                    y,
                    modifiers,
                }) => {
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let col = ((*x as f32 - padding) / cw).max(0.0) as usize;
                    let row = ((*y as f32 - padding) / ch).max(0.0) as usize;

                    let ws = &*pane_for_events;
                    let Some(pane) = ws.focused_pane() else {
                        return EventResponse::consumed();
                    };

                    let term = pane.terminal.read();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();
                    let term_cols = term.cols();
                    let term_rows = term.rows();
                    drop(term);

                    let col = col.min(term_cols.saturating_sub(1));
                    let row = row.min(term_rows.saturating_sub(1));

                    // Forward mouse events to PTY if mouse tracking is active
                    if mouse_mode != MouseMode::Off
                        && *button == madori::event::MouseButton::Left
                    {
                        let cx = (col + 1).min(223) as u8;
                        let cy = (row + 1).min(223) as u8;
                        if sgr {
                            let action = if *pressed { 'M' } else { 'm' };
                            let seq = format!("\x1b[<0;{};{}{action}", col + 1, row + 1);
                            let _ = pane.input_tx.send(seq.into_bytes());
                        } else if *pressed {
                            let _ =
                                pane.input_tx.send(vec![0x1b, b'[', b'M', 32, cx + 32, cy + 32]);
                        } else {
                            let _ =
                                pane.input_tx.send(vec![0x1b, b'[', b'M', 35, cx + 32, cy + 32]);
                        }
                        return EventResponse::consumed();
                    }

                    // Text selection via left mouse button
                    if *button == madori::event::MouseButton::Left {
                        if *pressed {
                            let now = Instant::now();
                            let same_pos = last_click_pos.row == row && last_click_pos.col == col;
                            let quick = now.duration_since(last_click_time).as_millis() < 400;

                            if same_pos && quick {
                                click_count = (click_count + 1).min(3);
                            } else {
                                click_count = 1;
                            }
                            last_click_time = now;
                            last_click_pos = CellPos { row, col };

                            let mut sel = pane.selection.lock().unwrap();
                            match click_count {
                                2 => {
                                    // Double-click: select word
                                    let term = pane.terminal.read();
                                    let rows: Vec<_> =
                                        term.visible_rows().map(|r| r.to_vec()).collect();
                                    let cols_count = term.cols();
                                    drop(term);
                                    sel.select_word(CellPos { row, col }, &rows, cols_count);
                                }
                                3 => {
                                    // Triple-click: select entire line
                                    let term = pane.terminal.read();
                                    let cols_count = term.cols();
                                    drop(term);
                                    sel.select_line(row, cols_count);
                                }
                                _ => {
                                    sel.start(CellPos { row, col });
                                }
                            }
                        } else {
                            let mut sel = pane.selection.lock().unwrap();
                            if click_count == 1 {
                                sel.finish();
                                if copy_on_select {
                                    let term = pane.terminal.read();
                                    let rows: Vec<_> =
                                        term.visible_rows().map(|r| r.to_vec()).collect();
                                    let cols = term.cols();
                                    drop(term);
                                    if let Some(text) = sel.extract_text(&rows, cols) {
                                        if let Ok(cb) = clipboard.lock() {
                                            let _ = cb.copy_text(&text);
                                        }
                                    }
                                }
                                // Cmd+click (macOS) / Ctrl+click (Linux) to open URLs
                                if modifiers.meta || modifiers.ctrl {
                                    drop(sel);
                                    let term = pane.terminal.read();
                                    let row_cells: Vec<Vec<crate::terminal::Cell>> =
                                        term.visible_rows().map(|r| r.to_vec()).collect();
                                    let cols = term.cols();
                                    drop(term);
                                    let detected =
                                        crate::url::detect_urls(&row_cells, cols);
                                    if let Some(url) =
                                        crate::url::url_at(&detected, row, col)
                                    {
                                        // Phase-4 note: `ws` is a `&SinglePane` now
                                        // (not a MutexGuard). `drop(&ws)` was a no-op
                                        // and clippy flagged it; the old code dropped
                                        // a real WindowState lock here.
                                        if let Err(e) = open::that(&url.url) {
                                            tracing::warn!(
                                                error = %e,
                                                url = %url.url,
                                                "failed to open URL"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    EventResponse::consumed()
                }
                // Mouse move — update selection drag or forward to PTY
                AppEvent::Mouse(MouseEvent::Moved { x, y }) => {
                    let was_hidden = !mouse_visible.swap(true, Ordering::SeqCst);
                    let show_cursor = mouse_hide_while_typing && was_hidden;
                    // Snow overlay: track cursor for the deflection ring
                    // on the near-layer flakes.
                    renderer.snow_set_cursor(*x as f32, *y as f32);
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let col = ((*x as f32 - padding) / cw).max(0.0) as usize;
                    let row = ((*y as f32 - padding) / ch).max(0.0) as usize;

                    let ws = &*pane_for_events;
                    let Some(pane) = ws.focused_pane() else {
                        return EventResponse::consumed();
                    };

                    let term = pane.terminal.read();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();
                    let term_cols = term.cols();
                    let term_rows = term.rows();
                    drop(term);

                    let col = col.min(term_cols.saturating_sub(1));
                    let row = row.min(term_rows.saturating_sub(1));

                    // Forward mouse motion to PTY if button-event or any-event mode
                    if matches!(mouse_mode, MouseMode::ButtonEvent | MouseMode::AnyEvent) {
                        if sgr {
                            let seq = format!("\x1b[<32;{};{}M", col + 1, row + 1);
                            let _ = pane.input_tx.send(seq.into_bytes());
                        }
                        return if show_cursor {
                            EventResponse {
                                consumed: true,
                                set_cursor_visible: Some(true),
                                ..Default::default()
                            }
                        } else {
                            EventResponse::consumed()
                        };
                    }

                    // Update text selection if dragging
                    let mut sel = pane.selection.lock().unwrap();
                    if sel.is_active() {
                        sel.update(CellPos { row, col });
                    }
                    if show_cursor {
                        EventResponse {
                            consumed: true,
                            set_cursor_visible: Some(true),
                            ..Default::default()
                        }
                    } else {
                        EventResponse::consumed()
                    }
                }
                AppEvent::Mouse(MouseEvent::Scroll { dy, .. }) => {
                    let ws = &*pane_for_events;
                    let Some(pane) = ws.focused_pane() else {
                        return EventResponse::consumed();
                    };

                    let mut term = pane.terminal.write();
                    let mouse_mode = term.mouse_mode();
                    let sgr = term.sgr_mouse();

                    // If mouse tracking is active, forward scroll as button events
                    if mouse_mode != MouseMode::Off {
                        drop(term);
                        let button = if *dy > 0.0 { 64 } else { 65 };
                        if sgr {
                            let seq = format!("\x1b[<{button};1;1M");
                            let _ = pane.input_tx.send(seq.into_bytes());
                        } else {
                            let _ =
                                pane.input_tx.send(vec![0x1b, b'[', b'M', button + 32, 33, 33]);
                        }
                        return EventResponse::consumed();
                    }

                    let lines = (*dy as isize).unsigned_abs().max(1)
                        * (mouse_scroll_multiplier as usize).max(1);
                    if *dy > 0.0 {
                        term.scroll_up(lines);
                    } else {
                        term.scroll_down(lines);
                    }
                    EventResponse::consumed()
                }
                // Focus events → send to PTY if focus reporting is enabled
                AppEvent::Focused(focused) => {
                    let ws = &*pane_for_events;
                    if let Some(pane) = ws.focused_pane() {
                        let term = pane.terminal.read();
                        let reporting = term.focus_reporting();
                        drop(term);
                        if reporting {
                            if *focused {
                                let _ = pane.input_tx.send(b"\x1b[I".to_vec());
                            } else {
                                let _ = pane.input_tx.send(b"\x1b[O".to_vec());
                            }
                        }
                    }
                    EventResponse::consumed()
                }
                AppEvent::CloseRequested => exit_response(confirm_close, &pending_close),
                AppEvent::Resized { width, height } => {
                    last_width = *width;
                    last_height = *height;
                    let cw = renderer.cell_width();
                    let ch = renderer.cell_height();
                    let ws = &*pane_for_events;
                    ws.resize_panes(*width as f32, *height as f32, padding, cw, ch);
                    EventResponse::consumed()
                }
                // Check for title/bell/clipboard changes on every redraw
                AppEvent::RedrawRequested => {
                    if !native_styling_applied {
                        native_styling_applied = true;
                        crate::platform::apply_native_styling();
                    }
                    let ws = &*pane_for_events;
                    if let Some(pane) = ws.focused_pane() {
                        let mut term = pane.terminal.write();
                        let current_title = term.title().map(String::from);
                        let bell = term.take_bell();
                        let osc52_clip = term.take_clipboard();
                        drop(term);
                        // Phase-4 note: `ws` is `&SinglePane`; `drop(&ws)`
                        // is a no-op clippy flagged. `term` was a real
                        // RwLockWriteGuard — dropping it above is real.

                        // OSC 52 clipboard sync — copy terminal clipboard to system
                        if let Some(clip_text) = osc52_clip {
                            if let Ok(cb) = clipboard.lock() {
                                let _ = cb.copy_text(&clip_text);
                            }
                        }

                        // Bell flash
                        if bell {
                            renderer.trigger_bell();
                        }

                        if current_title != last_title {
                            last_title = current_title.clone();
                            if let Some(title) = current_title {
                                return EventResponse {
                                    set_title: Some(title),
                                    ..Default::default()
                                };
                            }
                        }
                    }
                    EventResponse::ignored()
                }
                _ => EventResponse::ignored(),
            }
        })
        .run()
        .map_err(|e| anyhow::anyhow!("madori error: {e}"))?;

    scripts.fire_event(ScriptEvent::OnQuit);

    Ok(())
}

/// Encode a key event for the Kitty keyboard protocol.
/// Returns the escape sequence bytes if the protocol is active, None otherwise.
fn kitty_encode_key(
    key: &madori::event::KeyCode,
    text: &Option<String>,
    modifiers: &madori::event::Modifiers,
    flags: u32,
) -> Option<Vec<u8>> {
    if flags == 0 {
        return None;
    }

    // Build modifier value: 1 + bitmask
    let mut mod_bits: u32 = 0;
    if modifiers.shift {
        mod_bits |= 1;
    }
    if modifiers.alt {
        mod_bits |= 2;
    }
    if modifiers.ctrl {
        mod_bits |= 4;
    }
    if modifiers.meta {
        mod_bits |= 8; // super
    }
    let mod_val = 1 + mod_bits;

    // Map key to unicode codepoint or special key number
    match key {
        madori::event::KeyCode::Enter => Some(kitty_key_seq(13, mod_val, 'u')),
        madori::event::KeyCode::Tab => Some(kitty_key_seq(9, mod_val, 'u')),
        madori::event::KeyCode::Backspace => Some(kitty_key_seq(127, mod_val, 'u')),
        madori::event::KeyCode::Escape => Some(kitty_key_seq(27, mod_val, 'u')),
        madori::event::KeyCode::Space => Some(kitty_key_seq(b' ' as u32, mod_val, 'u')),
        madori::event::KeyCode::Delete => Some(kitty_tilde_seq(3, mod_val)),
        madori::event::KeyCode::Up => Some(kitty_key_seq(1, mod_val, 'A')),
        madori::event::KeyCode::Down => Some(kitty_key_seq(1, mod_val, 'B')),
        madori::event::KeyCode::Right => Some(kitty_key_seq(1, mod_val, 'C')),
        madori::event::KeyCode::Left => Some(kitty_key_seq(1, mod_val, 'D')),
        madori::event::KeyCode::Home => Some(kitty_key_seq(1, mod_val, 'H')),
        madori::event::KeyCode::End => Some(kitty_key_seq(1, mod_val, 'F')),
        madori::event::KeyCode::PageUp => Some(kitty_tilde_seq(5, mod_val)),
        madori::event::KeyCode::PageDown => Some(kitty_tilde_seq(6, mod_val)),
        madori::event::KeyCode::F(n) => {
            let (num, suffix) = match n {
                1 => (1, 'P'),
                2 => (1, 'Q'),
                3 => (1, 'R'),
                4 => (1, 'S'),
                5 => (15, '~'),
                6 => (17, '~'),
                7 => (18, '~'),
                8 => (19, '~'),
                9 => (20, '~'),
                10 => (21, '~'),
                11 => (23, '~'),
                12 => (24, '~'),
                _ => return None,
            };
            if suffix == '~' {
                Some(kitty_tilde_seq(num, mod_val))
            } else {
                Some(kitty_key_seq(num, mod_val, suffix))
            }
        }
        madori::event::KeyCode::Char(ch) => {
            // Level 1 (disambiguate): only encode with modifiers beyond shift
            if mod_bits <= 1 {
                // No modifiers or just shift — send as normal UTF-8
                return None;
            }
            let cp = *ch as u32;
            Some(kitty_key_seq(cp, mod_val, 'u'))
        }
        _ => {
            // Try text content
            if let Some(t) = text {
                if let Some(ch) = t.chars().next() {
                    if t.len() == ch.len_utf8() && mod_bits > 1 {
                        return Some(kitty_key_seq(ch as u32, mod_val, 'u'));
                    }
                }
            }
            None
        }
    }
}

/// Build CSI number ; modifiers X sequence for Kitty protocol.
fn kitty_key_seq(number: u32, modifiers: u32, suffix: char) -> Vec<u8> {
    if modifiers <= 1 && suffix == 'u' {
        // No modifiers — CSI number u
        format!("\x1b[{number}{suffix}").into_bytes()
    } else if suffix == 'u' {
        format!("\x1b[{number};{modifiers}{suffix}").into_bytes()
    } else if modifiers <= 1 {
        // Arrow/Home/End without modifiers — standard CSI X
        format!("\x1b[{suffix}").into_bytes()
    } else {
        format!("\x1b[{number};{modifiers}{suffix}").into_bytes()
    }
}

/// Build CSI number ; modifiers ~ sequence for Kitty protocol (tilde keys).
fn kitty_tilde_seq(number: u32, modifiers: u32) -> Vec<u8> {
    if modifiers <= 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{modifiers}~").into_bytes()
    }
}

/// Map madori key event to awase Key.
fn winit_to_awase_key(key: &madori::event::KeyCode, text: &Option<String>) -> Option<awase::Key> {
    match key {
        madori::event::KeyCode::Enter => Some(awase::Key::Return),
        madori::event::KeyCode::Escape => Some(awase::Key::Escape),
        madori::event::KeyCode::Tab => Some(awase::Key::Tab),
        madori::event::KeyCode::Backspace => Some(awase::Key::Backspace),
        madori::event::KeyCode::Delete => Some(awase::Key::Delete),
        madori::event::KeyCode::Home => Some(awase::Key::Home),
        madori::event::KeyCode::End => Some(awase::Key::End),
        madori::event::KeyCode::PageUp => Some(awase::Key::PageUp),
        madori::event::KeyCode::PageDown => Some(awase::Key::PageDown),
        madori::event::KeyCode::Up => Some(awase::Key::Up),
        madori::event::KeyCode::Down => Some(awase::Key::Down),
        madori::event::KeyCode::Left => Some(awase::Key::Left),
        madori::event::KeyCode::Right => Some(awase::Key::Right),
        madori::event::KeyCode::F(n) => match n {
            1 => Some(awase::Key::F1),
            2 => Some(awase::Key::F2),
            3 => Some(awase::Key::F3),
            4 => Some(awase::Key::F4),
            5 => Some(awase::Key::F5),
            6 => Some(awase::Key::F6),
            7 => Some(awase::Key::F7),
            8 => Some(awase::Key::F8),
            9 => Some(awase::Key::F9),
            10 => Some(awase::Key::F10),
            11 => Some(awase::Key::F11),
            12 => Some(awase::Key::F12),
            _ => None,
        },
        madori::event::KeyCode::Char(ch) => char_to_awase_key(*ch),
        madori::event::KeyCode::Space => Some(awase::Key::Space),
        _ => {
            // Try to extract from text
            if let Some(t) = text {
                if let Some(ch) = t.chars().next() {
                    if t.len() == ch.len_utf8() {
                        return char_to_awase_key(ch);
                    }
                }
            }
            None
        }
    }
}

/// Map a character to an awase key.
fn char_to_awase_key(ch: char) -> Option<awase::Key> {
    match ch.to_ascii_lowercase() {
        'a' => Some(awase::Key::A),
        'b' => Some(awase::Key::B),
        'c' => Some(awase::Key::C),
        'd' => Some(awase::Key::D),
        'e' => Some(awase::Key::E),
        'f' => Some(awase::Key::F),
        'g' => Some(awase::Key::G),
        'h' => Some(awase::Key::H),
        'i' => Some(awase::Key::I),
        'j' => Some(awase::Key::J),
        'k' => Some(awase::Key::K),
        'l' => Some(awase::Key::L),
        'm' => Some(awase::Key::M),
        'n' => Some(awase::Key::N),
        'o' => Some(awase::Key::O),
        'p' => Some(awase::Key::P),
        'q' => Some(awase::Key::Q),
        'r' => Some(awase::Key::R),
        's' => Some(awase::Key::S),
        't' => Some(awase::Key::T),
        'u' => Some(awase::Key::U),
        'v' => Some(awase::Key::V),
        'w' => Some(awase::Key::W),
        'x' => Some(awase::Key::X),
        'y' => Some(awase::Key::Y),
        'z' => Some(awase::Key::Z),
        '0' => Some(awase::Key::Num0),
        '1' => Some(awase::Key::Num1),
        '2' => Some(awase::Key::Num2),
        '3' => Some(awase::Key::Num3),
        '4' => Some(awase::Key::Num4),
        '5' => Some(awase::Key::Num5),
        '6' => Some(awase::Key::Num6),
        '7' => Some(awase::Key::Num7),
        '8' => Some(awase::Key::Num8),
        '9' => Some(awase::Key::Num9),
        ' ' => Some(awase::Key::Space),
        '/' => Some(awase::Key::Slash),
        '+' | '=' => Some(awase::Key::Equal),
        '-' => Some(awase::Key::Minus),
        ',' => Some(awase::Key::Comma),
        '.' => Some(awase::Key::Period),
        _ => None,
    }
}

/// Generate cursor key escape sequence based on mode.
fn cursor_key(ch: u8, application_mode: bool) -> Vec<u8> {
    if application_mode {
        vec![0x1b, b'O', ch]
    } else {
        vec![0x1b, b'[', ch]
    }
}

/// Generate F-key escape sequences.
fn f_key_escape(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => vec![],
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}

/// Add cursor visibility to response when mouse_hide_while_typing changed it.
fn with_cursor_visibility(resp: EventResponse, visible: Option<bool>) -> EventResponse {
    if let Some(v) = visible {
        EventResponse {
            set_cursor_visible: Some(v),
            ..resp
        }
    } else {
        resp
    }
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

    // ---- cursor_key ----

    #[test]
    fn test_cursor_key_up_normal() {
        assert_eq!(cursor_key(b'A', false), b"\x1b[A");
    }

    #[test]
    fn test_cursor_key_up_app_mode() {
        assert_eq!(cursor_key(b'A', true), b"\x1bOA");
    }

    #[test]
    fn test_cursor_key_down_normal() {
        assert_eq!(cursor_key(b'B', false), b"\x1b[B");
    }

    #[test]
    fn test_cursor_key_left_normal() {
        assert_eq!(cursor_key(b'D', false), b"\x1b[D");
    }

    #[test]
    fn test_cursor_key_right_normal() {
        assert_eq!(cursor_key(b'C', false), b"\x1b[C");
    }

    #[test]
    fn test_cursor_key_home_app_mode() {
        assert_eq!(cursor_key(b'H', true), b"\x1bOH");
    }

    // ---- f_key_escape ----

    #[test]
    fn test_f_key_f1() {
        assert_eq!(f_key_escape(1), b"\x1bOP");
    }

    #[test]
    fn test_f_key_f2() {
        assert_eq!(f_key_escape(2), b"\x1bOQ");
    }

    #[test]
    fn test_f_key_f3() {
        assert_eq!(f_key_escape(3), b"\x1bOR");
    }

    #[test]
    fn test_f_key_f4() {
        assert_eq!(f_key_escape(4), b"\x1bOS");
    }

    #[test]
    fn test_f_key_f5() {
        assert_eq!(f_key_escape(5), b"\x1b[15~");
    }

    #[test]
    fn test_f_key_f12() {
        assert_eq!(f_key_escape(12), b"\x1b[24~");
    }

    #[test]
    fn test_f_key_out_of_range() {
        assert!(f_key_escape(13).is_empty());
    }

    #[test]
    fn test_f_key_zero() {
        assert!(f_key_escape(0).is_empty());
    }

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

    #[test]
    fn test_with_cursor_visibility_true() {
        let resp = with_cursor_visibility(EventResponse::consumed(), Some(true));
        assert_eq!(resp.set_cursor_visible, Some(true));
        assert!(resp.consumed);
    }

    #[test]
    fn test_with_cursor_visibility_false() {
        let resp = with_cursor_visibility(EventResponse::consumed(), Some(false));
        assert_eq!(resp.set_cursor_visible, Some(false));
    }

    #[test]
    fn test_with_cursor_visibility_none() {
        let resp = with_cursor_visibility(EventResponse::consumed(), None);
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

    // ---- kitty_key_seq ----

    #[test]
    fn test_kitty_key_seq_no_modifiers_u() {
        let seq = kitty_key_seq(97, 1, 'u');
        assert_eq!(seq, b"\x1b[97u");
    }

    #[test]
    fn test_kitty_key_seq_with_modifiers_u() {
        let seq = kitty_key_seq(97, 5, 'u');
        assert_eq!(seq, b"\x1b[97;5u");
    }

    #[test]
    fn test_kitty_key_seq_arrow_no_modifiers() {
        let seq = kitty_key_seq(1, 1, 'A');
        assert_eq!(seq, b"\x1b[A");
    }

    #[test]
    fn test_kitty_key_seq_arrow_with_modifiers() {
        let seq = kitty_key_seq(1, 5, 'A');
        assert_eq!(seq, b"\x1b[1;5A");
    }

    // ---- kitty_tilde_seq ----

    #[test]
    fn test_kitty_tilde_seq_no_modifiers() {
        let seq = kitty_tilde_seq(3, 1);
        assert_eq!(seq, b"\x1b[3~");
    }

    #[test]
    fn test_kitty_tilde_seq_with_modifiers() {
        let seq = kitty_tilde_seq(3, 5);
        assert_eq!(seq, b"\x1b[3;5~");
    }

    // ---- kitty_encode_key ----

    #[test]
    fn test_kitty_encode_no_flags_returns_none() {
        let mods = madori::event::Modifiers::default();
        assert!(kitty_encode_key(&madori::event::KeyCode::Char('a'), &None, &mods, 0).is_none());
    }

    #[test]
    fn test_kitty_encode_char_with_ctrl() {
        let mods = madori::event::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let result = kitty_encode_key(&madori::event::KeyCode::Char('a'), &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[97;5u");
    }

    #[test]
    fn test_kitty_encode_enter() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Enter, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[13u");
    }

    #[test]
    fn test_kitty_encode_char_no_modifiers_returns_none() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Char('x'), &None, &mods, 1);
        assert!(result.is_none(), "plain char with no modifiers falls through to normal input");
    }

    #[test]
    fn test_kitty_encode_f1() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::F(1), &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[P");
    }

    #[test]
    fn test_kitty_encode_arrow_up_with_shift() {
        let mods = madori::event::Modifiers {
            shift: true,
            ..Default::default()
        };
        let result = kitty_encode_key(&madori::event::KeyCode::Up, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[1;2A");
    }

    #[test]
    fn test_kitty_encode_tab() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Tab, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[9u");
    }

    #[test]
    fn test_kitty_encode_backspace() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Backspace, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[127u");
    }

    #[test]
    fn test_kitty_encode_escape() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Escape, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[27u");
    }

    #[test]
    fn test_kitty_encode_arrow_down() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Down, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[B");
    }

    #[test]
    fn test_kitty_encode_arrow_left() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Left, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[D");
    }

    #[test]
    fn test_kitty_encode_arrow_right() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Right, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[C");
    }

    #[test]
    fn test_kitty_encode_home() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Home, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[H");
    }

    #[test]
    fn test_kitty_encode_end() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::End, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[F");
    }

    #[test]
    fn test_kitty_encode_delete() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::Delete, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[3~");
    }

    #[test]
    fn test_kitty_encode_page_up() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::PageUp, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[5~");
    }

    #[test]
    fn test_kitty_encode_page_down() {
        let mods = madori::event::Modifiers::default();
        let result = kitty_encode_key(&madori::event::KeyCode::PageDown, &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[6~");
    }

    #[test]
    fn test_kitty_encode_char_with_alt() {
        let mods = madori::event::Modifiers {
            alt: true,
            ..Default::default()
        };
        let result = kitty_encode_key(&madori::event::KeyCode::Char('a'), &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[97;3u");
    }

    #[test]
    fn test_kitty_encode_char_ctrl_shift() {
        let mods = madori::event::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let result = kitty_encode_key(&madori::event::KeyCode::Char('a'), &None, &mods, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"\x1b[97;6u");
    }

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
