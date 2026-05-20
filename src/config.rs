use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MadoConfig {
    #[serde(default = "default_font_family")]
    pub font_family: String,
    /// Family used for italic cells. cosmic-text's
    /// `Attrs::style(Style::Italic)` walks the fontdb for an italic
    /// face matching the primary family by default, but explicit
    /// per-app override here lets the operator point italics at a
    /// dedicated calligraphic family (Iosevka Etoile, Maple Mono
    /// Italic, Operator Mono) independent of the regular face.
    /// Sourced from `ishou-tokens::MonoFonts::pleme().italic` when
    /// blackmatter-mado renders the YAML.
    #[serde(default = "default_font_italic")]
    pub font_italic: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub cursor: CursorConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub shaders: ShaderConfig,
    #[serde(default)]
    pub accessibility: AccessibilityConfig,
    #[serde(default)]
    pub shell_integration: ShellIntegrationConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub environment: EnvironmentConfig,
    #[serde(default)]
    pub selection: SelectionConfig,
    #[serde(default)]
    pub search: SearchColorsConfig,
    #[serde(default)]
    pub keybinds: KeybindConfig,
    #[serde(default)]
    pub quick_terminal: QuickTerminalConfig,
    /// Tear-multiplexer integration (theory/MADO-TEAR-M5.md).
    /// Mado discovers + optionally attaches to a tear-daemon at
    /// launch; the mode + socket override + auto-spawn knob all
    /// live here. Default = `TearMode::Auto` — try tear, fall
    /// back to local PTY if not available.
    #[serde(default)]
    pub tear: MadoTearConfig,
    /// Default-on visual effects rendered as overlays after the
    /// text pass. Snow is on by default — set `effects.snow.enabled
    /// = false` to disable.
    #[serde(default)]
    pub effects: MadoEffectsConfig,
}

/// Mado's overlay-effect configuration. Each field is one effect's
/// knobs. Effects are rendered in declaration order after the text
/// pass via `LoadOp::Load` alpha blending.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MadoEffectsConfig {
    #[serde(default)]
    pub snow: MadoSnowConfig,
}

/// Snow overlay knobs. Mirrors `engawa_snow::SnowParams` but only
/// the operator-facing dials; runtime state (time, cursor,
/// typing_pulse, accumulation drift) is mado-managed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MadoSnowConfig {
    /// Master enable. Default `true` — snow is the flagship
    /// effect.
    #[serde(default = "default_snow_enabled")]
    pub enabled: bool,
    /// Master gain, 0..1. Default 0.30.
    #[serde(default = "default_snow_intensity")]
    pub intensity: f32,
    /// Horizontal wind, -1..1. Default 0.0. The current shader
    /// is pure-vertical-gravity, so this knob is currently a
    /// no-op at the shader level (retained for forward-compat).
    #[serde(default)]
    pub wind: f32,
    /// Starting pile level, 0..1. The host integrates real
    /// accumulation over wall time on top of this baseline.
    /// Default 0.0 (empty floor at launch).
    #[serde(default)]
    pub accumulation: f32,
    /// Parallax layer count, 1..3. Default 2.
    #[serde(default = "default_snow_layer_count")]
    pub layer_count: f32,
    /// Temperature 0..1. 0 = freezing — the host increases the
    /// pile from incoming snowfall, no melt. 0.5 = neutral —
    /// pile holds. 1 = warm — pile melts visibly over time. The
    /// host integrates pile drift each frame based on this value.
    /// Default 0.20 (cold — pile grows slowly).
    #[serde(default = "default_snow_temperature")]
    pub temperature: f32,
    /// How fast the pile fills when cold (units per second of
    /// wall time, scaled by `1 - temperature * 2` when below
    /// 0.5). Default 0.04 — fills to max over ~25s at temp=0.
    #[serde(default = "default_snow_pile_rate")]
    pub pile_rate: f32,
    /// How fast the pile melts when warm. Default 0.06.
    #[serde(default = "default_snow_melt_rate")]
    pub melt_rate: f32,
}

// TEMPORARY: defaulted OFF for an A/B launch-perf test. Flip
// back to true once we've isolated whether the perceived "giant
// loading time" is dominated by the snow render pass or by
// mado's GPU + tear cold start. Operators who want snow can set
// `effects.snow.enabled = true` in mado.yaml.
fn default_snow_enabled() -> bool { false }
// Subtle by default — the shader's MAX_ALPHA cap (0.35) keeps
// text readable, but a lower intensity makes the snow feel like
// a gentle backdrop rather than a foreground effect.
fn default_snow_intensity() -> f32 { 0.30 }
fn default_snow_layer_count() -> f32 { 2.0 }
fn default_snow_temperature() -> f32 { 0.20 }
fn default_snow_pile_rate() -> f32 { 0.04 }
fn default_snow_melt_rate() -> f32 { 0.06 }

impl Default for MadoSnowConfig {
    fn default() -> Self {
        Self {
            enabled: default_snow_enabled(),
            intensity: default_snow_intensity(),
            wind: 0.0,
            accumulation: 0.0,
            layer_count: default_snow_layer_count(),
            temperature: default_snow_temperature(),
            pile_rate: default_snow_pile_rate(),
            melt_rate: default_snow_melt_rate(),
        }
    }
}

/// Mado's `[tear]` config section — controls auto-discovery and
/// fallback behaviour around the tear-daemon multiplexer.
///
/// Operating modes (`mode`):
///   * `Auto` — try to connect to the daemon (auto-discovered
///     socket OR explicit `socket` override). If successful, run
///     in tear-attached mode; if not, fall back to local PTY.
///     Optionally auto-spawn the daemon when `auto_spawn = true`.
///   * `Always` — require tear. If no daemon is discoverable AND
///     `auto_spawn = false`, mado refuses to start. If
///     `auto_spawn = true`, mado spawns one and attaches.
///   * `Never` — ignore tear entirely. Always use the local PTY.
///     Useful for headless / scripted invocations where the user
///     genuinely wants single-pane mado with no IPC.
///   * `Attach` — like `Always` but never auto-spawns. Refuses to
///     start if no daemon is reachable. The "I run my tear daemon
///     as a service, mado must talk to that instance" mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MadoTearConfig {
    #[serde(default)]
    pub mode: TearMode,
    /// Where tear's runtime LIVES. `Daemon` (default) talks over
    /// Unix socket to the tear-daemon process; `Embedded` runs
    /// tear's PTY+grid in-process via `tear_core::InProcess`. See
    /// [`TearRuntime`] for the latency/multi-attach tradeoff.
    #[serde(default)]
    pub runtime: TearRuntime,
    /// Explicit UDS path override. `None` (default) → derive from
    /// `$XDG_RUNTIME_DIR/tear.sock` (or `~/.local/share/tear/
    /// tear.sock` fallback) — same default the tear daemon binds
    /// to.
    #[serde(default)]
    pub socket: Option<std::path::PathBuf>,
    /// When `true` and the discovery probe finds no live daemon,
    /// mado spawns one via `Command::new("tear").args(["daemon",
    /// "--socket", <socket>])` and waits up to
    /// `spawn_wait_ms` for it to bind. When `false`, an absent
    /// daemon is left absent (Auto falls back; Always errors out).
    #[serde(default = "default_auto_spawn")]
    pub auto_spawn: bool,
    /// Milliseconds to wait for an auto-spawned daemon to bind
    /// its socket before giving up. Default 2000 — plenty even on
    /// slow CI hardware.
    #[serde(default = "default_spawn_wait_ms")]
    pub spawn_wait_ms: u64,
    /// Session name to attach to / create on first attach. None
    /// = let mado generate a unique name from the current
    /// timestamp.
    #[serde(default)]
    pub session_name: Option<String>,
    /// Pane id to attach to. Mutually exclusive with
    /// `session_name`; if both set, `pane` wins. Use this when
    /// reconnecting to a known long-lived pane.
    #[serde(default)]
    pub pane: Option<String>,
    /// Optional TearConfig overrides mado pushes to the daemon
    /// at attach time (and again on demand during the session).
    /// Implements the "mado authors tear's config" half of the
    /// M5 destination: mado is the front-end + the canonical
    /// author of tear settings when it's the consumer.
    ///
    /// `None` (default) — mado leaves the daemon's config alone.
    /// `Some(overrides)` — mado fetches the daemon's current
    /// TearConfig at attach, merges these overrides in (None
    /// fields leave the daemon value untouched; Some fields
    /// replace it), and pushes the result back via `SetConfig`.
    /// The daemon's on-disk file is NOT touched; a notify-driven
    /// or manual `ReloadConfig` reverts to the file.
    #[serde(default)]
    pub impose: Option<MadoTearImpose>,
}

/// Per-field TearConfig overrides mado optionally pushes to the
/// daemon at attach time. Every field is `Option`-wrapped so the
/// merge is unambiguous: `None` = leave daemon's value alone,
/// `Some(v)` = replace.
///
/// Today's surface is a small useful subset of TearConfig fields
/// — the ones operators most often want mado to author centrally
/// (so a fleet of mado windows agree on prefix / shell / status).
/// New fields land here as new use cases surface; no breaking
/// change to existing operators because every field is optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MadoTearImpose {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub default_shell: Option<String>,
    #[serde(default)]
    pub status_visible: Option<bool>,
    /// Per-pane scrollback policy that mado imposes on the tear
    /// daemon at attach time. Default `None` means "let the
    /// daemon's own tear.yaml settings apply"; set this to
    /// propagate mado's preferred scrollback semantics into every
    /// tear session mado spawns or attaches to.
    ///
    /// Operators almost always want mado's "never lose anything"
    /// scrollback default to apply to tear sessions too. The
    /// pleme.terminal aggregator module sets this by default so
    /// the operator experience is consistent across mado-local
    /// PTYs AND tear-multiplexed sessions.
    #[serde(default)]
    pub scrollback: Option<MadoTearScrollbackImpose>,
}

/// Subset of tear-config's `ScrollbackConfig` that mado can
/// override via the impose mechanism. Mirrors the upstream
/// schema one-to-one but every field is optional so partial
/// overrides land cleanly (operator imposes one knob, daemon
/// keeps the rest of its own settings).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MadoTearScrollbackImpose {
    #[serde(default)]
    pub rows: Option<usize>,
    #[serde(default)]
    pub max_bytes: Option<Option<usize>>,
    #[serde(default)]
    pub keep_on_clear: Option<bool>,
    #[serde(default)]
    pub on_alt_screen: Option<bool>,
    #[serde(default)]
    pub skip_blank_rows: Option<bool>,
    #[serde(default)]
    pub reflow_on_resize: Option<bool>,
}

impl MadoTearScrollbackImpose {
    pub fn has_any_override(&self) -> bool {
        self.rows.is_some()
            || self.max_bytes.is_some()
            || self.keep_on_clear.is_some()
            || self.on_alt_screen.is_some()
            || self.skip_blank_rows.is_some()
            || self.reflow_on_resize.is_some()
    }

    pub fn apply_to(&self, cfg: &mut tear_config::ScrollbackConfig) {
        if let Some(v) = self.rows { cfg.rows = v; }
        if let Some(v) = self.max_bytes { cfg.max_bytes = v; }
        if let Some(v) = self.keep_on_clear { cfg.keep_on_clear = v; }
        if let Some(v) = self.on_alt_screen { cfg.on_alt_screen = v; }
        if let Some(v) = self.skip_blank_rows { cfg.skip_blank_rows = v; }
        if let Some(v) = self.reflow_on_resize { cfg.reflow_on_resize = v; }
    }
}

impl MadoTearImpose {
    /// True iff at least one override is set. Lets the caller
    /// skip the get_config → mutate → set_config round-trip
    /// when there's nothing to impose.
    pub fn has_any_override(&self) -> bool {
        self.prefix.is_some()
            || self.default_shell.is_some()
            || self.status_visible.is_some()
            || self.scrollback.as_ref().is_some_and(|s| s.has_any_override())
    }

    /// Apply overrides in-place onto a TearConfig snapshot. Each
    /// `Some(v)` overwrites the corresponding field; `None`
    /// fields leave the snapshot untouched.
    pub fn apply_to(&self, cfg: &mut tear_config::TearConfig) {
        if let Some(p) = &self.prefix {
            cfg.prefix = p.clone();
        }
        if let Some(s) = &self.default_shell {
            cfg.default_shell = s.clone();
        }
        if let Some(v) = self.status_visible {
            cfg.status.visible = v;
        }
        if let Some(sb) = &self.scrollback {
            sb.apply_to(&mut cfg.scrollback);
        }
    }
}

impl Default for MadoTearConfig {
    fn default() -> Self {
        Self {
            mode: TearMode::default(),
            runtime: TearRuntime::default(),
            socket: None,
            auto_spawn: default_auto_spawn(),
            spawn_wait_ms: default_spawn_wait_ms(),
            session_name: None,
            pane: None,
            impose: None,
        }
    }
}

/// How mado interacts with the tear-daemon multiplexer. See
/// [`MadoTearConfig`] for the per-mode contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TearMode {
    /// Try tear; fall back to local PTY on failure. Default.
    #[default]
    Auto,
    /// Require tear; auto-spawn if missing-AND `auto_spawn=true`,
    /// else error.
    Always,
    /// Ignore tear entirely; always local PTY.
    Never,
    /// Like Always but never spawns — must find an existing daemon.
    Attach,
}

/// Where the tear runtime LIVES. Orthogonal to [`TearMode`] (which
/// is about discovery / fallback semantics): `TearRuntime` picks
/// IPC topology.
///
/// * `Daemon` (default for safety / backwards-compat) — talk to the
///   tear-daemon over a Unix socket. ~5-10ms IPC hop per render
///   frame; required for multi-attach scenarios where ≥2 consumers
///   (ayatsuri overlay, namimado debug inspector, remote ssh)
///   share the same session.
///
/// * `Embedded` — run tear's PTY+grid in-process inside mado via
///   `tear_core::InProcess`. Zero IPC, ~16ms ghostty-class
///   latency. The right choice for the default single-window case
///   (operator opens mado, types, closes — no one else needs the
///   session). See `pleme-io/maestro/stacks/mado-default.yaml`
///   for the maestro declaration of this mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TearRuntime {
    /// In-process via `tear_core::InProcess` (no IPC, no daemon
    /// spawn, ghostty-class latency).
    Embedded,
    /// Over Unix socket via `tear_client::Client` (multi-attach
    /// safe). Default for backwards compat.
    #[default]
    Daemon,
}

fn default_auto_spawn() -> bool {
    true
}

fn default_spawn_wait_ms() -> u64 {
    2000
}

/// Font family and rendering configuration (mirrors Ghostty's font-* options).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontConfig {
    #[serde(default)]
    pub family_bold: Option<String>,
    #[serde(default)]
    pub family_italic: Option<String>,
    #[serde(default)]
    pub family_bold_italic: Option<String>,
    #[serde(default)]
    pub thicken: bool,
    #[serde(default)]
    pub synthetic_style: bool,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub codepoint_map: HashMap<String, String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family_bold: None,
            family_italic: None,
            family_bold_italic: None,
            thicken: false,
            synthetic_style: true,
            features: Vec::new(),
            codepoint_map: HashMap::new(),
        }
    }
}

/// Selection colors and behavior (mirrors Ghostty's selection-* options).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionConfig {
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default = "default_selection_word_chars")]
    pub word_chars: String,
    #[serde(default = "default_true")]
    pub clear_on_typing: bool,
    #[serde(default)]
    pub clear_on_copy: bool,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            foreground: None,
            background: None,
            word_chars: default_selection_word_chars(),
            clear_on_typing: true,
            clear_on_copy: false,
        }
    }
}

/// Search highlight colors (mirrors Ghostty's search-* options).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchColorsConfig {
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub selected_foreground: Option<String>,
    #[serde(default)]
    pub selected_background: Option<String>,
}

/// Custom keybind entries loaded from config (mirrors Ghostty's keybind option).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeybindConfig {
    #[serde(default)]
    pub custom: Vec<KeybindEntry>,
}

/// A single keybind mapping from config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindEntry {
    pub trigger: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_padding")]
    pub padding: u32,
    /// Show server / window-manager decorations (titlebar, border,
    /// close/minimize buttons). Default is **platform-aware**:
    ///
    /// * macOS: `true` — preserves the traffic-light buttons.
    ///   Combined with `platform::apply_native_styling()`'s
    ///   `FullSizeContentView` + transparent titlebar, the chrome
    ///   integrates into the content area for a "minimal but
    ///   functional" look. Operators who want a truly chromeless
    ///   look (no traffic lights) override to `false`.
    /// * Linux / Windows: `false` — server/wm decorations are
    ///   removed entirely for a pure borderless window. Most
    ///   tiling-WM operators on Linux already disable
    ///   decorations via their WM; this aligns mado's default.
    ///
    /// The operator-facing contract: "as little chrome as
    /// possible by default, per platform." Override in
    /// `~/.config/mado/mado.yaml` if you want a specific
    /// behavior.
    #[serde(default = "default_decorations")]
    pub decorations: bool,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_unfocused_split_opacity")]
    pub unfocused_split_opacity: f32,
    #[serde(default)]
    pub split_divider_color: Option<String>,
    #[serde(default)]
    pub background_image: Option<PathBuf>,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub maximize: bool,
    #[serde(default = "default_true")]
    pub inherit_working_directory: bool,
    #[serde(default = "default_true")]
    pub inherit_font_size: bool,
    #[serde(default = "default_true")]
    pub padding_balance: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearanceConfig {
    #[serde(default = "default_bg")]
    pub background: String,
    #[serde(default = "default_fg")]
    pub foreground: String,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default)]
    pub bold_is_bright: bool,
    #[serde(default = "default_minimum_contrast")]
    pub minimum_contrast: f32,
    #[serde(default)]
    pub background_blur: bool,
    #[serde(default)]
    pub unfocused_split_fill: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    Block,
    BlockHollow,
    Bar,
    Underline,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self::Block
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorConfig {
    #[serde(default)]
    pub style: CursorStyle,
    #[serde(default = "default_cursor_blink")]
    pub blink: bool,
    #[serde(default = "default_cursor_blink_rate")]
    pub blink_rate_ms: u32,
    #[serde(default = "default_cursor_color")]
    pub color: String,
    #[serde(default = "default_cursor_opacity")]
    pub opacity: f32,
    #[serde(default)]
    pub text_color: Option<String>,
    #[serde(default)]
    pub click_to_move: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorConfig {
    #[serde(default = "default_scrollback")]
    pub scrollback_lines: usize,
    #[serde(default = "default_copy_on_select")]
    pub copy_on_select: bool,
    #[serde(default)]
    pub confirm_close: bool,
    #[serde(default = "default_mouse_hide")]
    pub mouse_hide_while_typing: bool,
    #[serde(default = "default_mouse_scroll_mult")]
    pub mouse_scroll_multiplier: u32,
    #[serde(default)]
    pub wait_after_command: bool,
    #[serde(default = "default_true")]
    pub link_url: bool,
    #[serde(default = "default_true")]
    pub mouse_reporting: bool,
    #[serde(default)]
    pub mouse_shift_capture: MouseShiftCapture,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MouseShiftCapture {
    #[default]
    False,
    True,
    Never,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellIntegrationConfig {
    #[serde(default = "default_shell_integration_enabled")]
    pub enabled: bool,
    #[serde(default = "default_shell_integration_features")]
    pub features: Vec<String>,
}

impl Default for ShellIntegrationConfig {
    fn default() -> Self {
        Self {
            enabled: default_shell_integration_enabled(),
            features: default_shell_integration_features(),
        }
    }
}

/// Performance / pacing knobs. Fields use `Option<u32>` for the
/// hardcoded ← detected ← user precedence chain: `None` means "let
/// `garasu::adaptive` recommend", `Some(v)` means "operator said so,
/// no detection override."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    #[serde(default = "default_vsync")]
    pub vsync: bool,
    /// Explicit frame-rate target. `None` (the default) defers to
    /// `garasu::adaptive::recommend` — typically the primary display's
    /// refresh rate. Set explicitly to override detection.
    #[serde(default)]
    pub target_fps: Option<u32>,
    /// Upper bound on the adaptive recommendation. `None` = no ceiling.
    /// Use e.g. `240` to prevent a 360Hz panel from pushing too high.
    #[serde(default)]
    pub fps_cap: Option<u32>,
    /// Upper bound applied when on battery power. `None` = same as
    /// `fps_cap`. Battery detection lands in a follow-up — this slot
    /// is wired now so config schemas don't churn later.
    #[serde(default)]
    pub battery_fps_cap: Option<u32>,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            vsync: default_vsync(),
            target_fps: None,
            fps_cap: None,
            battery_fps_cap: None,
        }
    }
}

impl PerformanceConfig {
    /// Hardcoded fallback frame rate used when neither the user config
    /// nor the adaptive recommender supplies a value. The safe floor —
    /// every panel since 2003 supports 60Hz.
    pub const FALLBACK_FPS: u32 = 60;

    /// Resolve the effective fps target by walking the precedence
    /// chain: user `target_fps` → adaptive recommendation → hardcoded
    /// [`FALLBACK_FPS`].
    #[must_use]
    pub fn resolve_target_fps(&self, posture: Option<&garasu::adaptive::RuntimePosture>) -> u32 {
        if let Some(fps) = self.target_fps {
            return fps;
        }
        if let Some(p) = posture {
            let profile = garasu::adaptive::RecommendationProfile {
                fps_cap: self.fps_cap,
                battery_fps_cap: self.battery_fps_cap,
                force_battery_mode: false,
            };
            if let Some(fps) = garasu::adaptive::recommend(p, &profile).fps_target {
                return fps;
            }
        }
        Self::FALLBACK_FPS
    }
}

/// Environment configuration for PTY spawning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    /// Extra environment variables to set for spawned processes.
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Initial working directory for spawned processes.
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    /// Command to run for the first terminal only (overrides shell).
    #[serde(default)]
    pub initial_command: Option<String>,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            vars: HashMap::new(),
            working_directory: None,
            initial_command: None,
        }
    }
}

/// Named profile — overrides any top-level config field when activated.
/// Example in mado.yaml:
/// ```yaml
/// profiles:
///   light:
///     theme: "solarized_light"
///     appearance:
///       background: "#fdf6e3"
///       foreground: "#657b83"
///   coding:
///     font_size: 16
///     font_family: "Fira Code"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub font: Option<FontConfig>,
    pub theme: Option<String>,
    pub appearance: Option<AppearanceConfig>,
    pub cursor: Option<CursorConfig>,
    pub shell: Option<ShellConfig>,
    pub behavior: Option<BehaviorConfig>,
    pub performance: Option<PerformanceConfig>,
    pub environment: Option<EnvironmentConfig>,
    pub selection: Option<SelectionConfig>,
    pub window: Option<WindowConfig>,
}

/// Custom WGSL shader post-processing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShaderConfig {
    /// Enable custom shader post-processing.
    #[serde(default)]
    pub enabled: bool,
    /// Paths to WGSL shader files (applied in order).
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

impl Default for ShaderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            files: Vec::new(),
        }
    }
}

/// Accessibility features configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessibilityConfig {
    /// Colorblind simulation mode.
    #[serde(default)]
    pub colorblind: ColorblindMode,
    /// Minimum contrast ratio (0.0 = disabled, 4.5 = WCAG AA, 7.0 = WCAG AAA).
    #[serde(default)]
    pub min_contrast: f32,
    /// Font scale multiplier (1.0 = normal, 2.0 = double size).
    #[serde(default = "default_font_scale")]
    pub font_scale: f32,
    /// Reduce motion (disable cursor blink and animations).
    #[serde(default)]
    pub reduce_motion: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorblindMode {
    #[default]
    None,
    /// Protanopia (red-blind).
    Protanopia,
    /// Deuteranopia (green-blind).
    Deuteranopia,
    /// Tritanopia (blue-blind).
    Tritanopia,
}

impl Default for AccessibilityConfig {
    fn default() -> Self {
        Self {
            colorblind: ColorblindMode::None,
            min_contrast: 0.0,
            font_scale: default_font_scale(),
            reduce_motion: false,
        }
    }
}

fn default_font_scale() -> f32 {
    1.0
}

// ── Quick Terminal ──────────────────────────────────────────────────────────
//
// ghostty's distinguishing UX feature: a terminal window that stays
// hidden under a global hotkey and slides in from a screen edge when
// the user presses it (similar to Tilda, Guake, iTerm2's hotkey
// window, macOS "Visor"). mado absorbs the typed surface here; the
// runtime wire-up (global-hotkey listener + slide animation) arrives
// in a subsequent tick.

/// Which screen edge a Quick Terminal slides in from.
///
/// `Center` is a floating panel variant — width × height are both
/// `size_fraction * screen_dim`, positioned centered. The other
/// variants pin to one edge with the perpendicular axis filling
/// the screen.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuickTerminalEdge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

/// Typed Quick Terminal config — declarative equivalent of ghostty's
/// `quick-terminal-*` keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickTerminalConfig {
    /// When false, the Quick Terminal machinery is dormant — no
    /// global hotkey registration, no hidden window. Default: false
    /// (opt-in).
    #[serde(default)]
    pub enabled: bool,
    /// Which edge of the focused screen the Quick Terminal slides in
    /// from. Default: `top`.
    #[serde(default)]
    pub edge: QuickTerminalEdge,
    /// Fraction of the screen's long-axis size (relative to `edge`)
    /// the Quick Terminal occupies. For `top` / `bottom`, this is
    /// the height fraction; for `left` / `right`, the width; for
    /// `center`, both dimensions. Clamped to `[0.1, 1.0]` at
    /// resolution time. Default: 0.4 (40%).
    #[serde(default = "default_quick_terminal_size_fraction")]
    pub size_fraction: f32,
    /// Slide / fade animation duration in milliseconds. Zero disables
    /// animation and snaps to the final position. Default: 150ms.
    #[serde(default = "default_quick_terminal_animation_ms")]
    pub animation_ms: u64,
    /// Hide automatically when the window loses focus. Matches
    /// ghostty's `quick-terminal-autohide` default: true.
    #[serde(default = "default_true")]
    pub autohide_on_blur: bool,
    /// Global hotkey that toggles visibility — parsed by `awase`.
    /// Empty string = no automatic toggle (the Quick Terminal can
    /// still be driven via MCP). Default: empty.
    #[serde(default)]
    pub hotkey: String,
}

impl Default for QuickTerminalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            edge: QuickTerminalEdge::Top,
            size_fraction: default_quick_terminal_size_fraction(),
            animation_ms: default_quick_terminal_animation_ms(),
            autohide_on_blur: true,
            hotkey: String::new(),
        }
    }
}

impl QuickTerminalConfig {
    /// True when the config is both `enabled` and has a non-empty
    /// `hotkey` — the minimum for the global-hotkey registration to
    /// make sense. An enabled config without a hotkey is valid (MCP-
    /// driven) but `is_active_hotkey()` is false.
    #[must_use]
    #[allow(dead_code)] // Typed surface for the pending runtime wire-up.
    pub fn is_active_hotkey(&self) -> bool {
        self.enabled && !self.hotkey.is_empty()
    }

    /// Compute the Quick Terminal window size for the given screen
    /// pixel dimensions. Returns `(width, height)` after clamping
    /// `size_fraction` into `[0.1, 1.0]`.
    ///
    /// The math mirrors ghostty's resolution: edge-pinned variants
    /// fill the perpendicular axis; `Center` fractionates both axes.
    #[must_use]
    #[allow(dead_code)] // Typed surface for the pending runtime wire-up.
    pub fn resolve_size_pixels(&self, (screen_w, screen_h): (u32, u32)) -> (u32, u32) {
        let fraction = self.size_fraction.clamp(0.1, 1.0);
        // Minimum-1 pixel so downstream winit calls can't panic on 0.
        let axis = |n: u32, f: f32| ((n as f32 * f).round() as u32).max(1);
        match self.edge {
            QuickTerminalEdge::Top | QuickTerminalEdge::Bottom => {
                (screen_w, axis(screen_h, fraction))
            }
            QuickTerminalEdge::Left | QuickTerminalEdge::Right => {
                (axis(screen_w, fraction), screen_h)
            }
            QuickTerminalEdge::Center => {
                (axis(screen_w, fraction), axis(screen_h, fraction))
            }
        }
    }

    /// Window-origin (top-left) in screen pixels for the computed
    /// size. Paired with [`Self::resolve_size_pixels`] gives the
    /// full placement tuple. `Center` returns the origin that
    /// centers a `size_fraction × size_fraction` rectangle.
    #[must_use]
    #[allow(dead_code)] // Typed surface for the pending runtime wire-up.
    pub fn resolve_origin_pixels(&self, screen: (u32, u32)) -> (u32, u32) {
        let (w, h) = self.resolve_size_pixels(screen);
        let (sw, sh) = screen;
        match self.edge {
            QuickTerminalEdge::Top | QuickTerminalEdge::Left => (0, 0),
            QuickTerminalEdge::Bottom => (0, sh.saturating_sub(h)),
            QuickTerminalEdge::Right => (sw.saturating_sub(w), 0),
            QuickTerminalEdge::Center => {
                ((sw.saturating_sub(w)) / 2, (sh.saturating_sub(h)) / 2)
            }
        }
    }
}

fn default_quick_terminal_size_fraction() -> f32 {
    0.4
}

fn default_quick_terminal_animation_ms() -> u64 {
    150
}

impl MadoConfig {
    /// Apply a named profile's overrides to this config.
    /// Returns a new config with the profile's values merged in.
    #[must_use]
    pub fn with_profile(&self, profile_name: &str) -> Self {
        let Some(profile) = self.profiles.get(profile_name) else {
            tracing::warn!(profile_name, "profile not found");
            return self.clone();
        };

        let mut config = self.clone();
        if let Some(ref family) = profile.font_family {
            config.font_family = family.clone();
        }
        if let Some(size) = profile.font_size {
            config.font_size = size;
        }
        if let Some(ref theme) = profile.theme {
            config.theme = theme.clone();
        }
        if let Some(ref appearance) = profile.appearance {
            config.appearance = appearance.clone();
        }
        if let Some(ref cursor) = profile.cursor {
            config.cursor = cursor.clone();
        }
        if let Some(ref shell) = profile.shell {
            config.shell = shell.clone();
        }
        if let Some(ref behavior) = profile.behavior {
            config.behavior = behavior.clone();
        }
        if let Some(ref performance) = profile.performance {
            config.performance = performance.clone();
        }
        if let Some(ref environment) = profile.environment {
            config.environment = environment.clone();
        }
        if let Some(ref font) = profile.font {
            config.font = font.clone();
        }
        if let Some(ref selection) = profile.selection {
            config.selection = selection.clone();
        }
        if let Some(ref window) = profile.window {
            config.window = window.clone();
        }
        config
    }
}

// Defaults

impl Default for MadoConfig {
    fn default() -> Self {
        Self {
            font_family: default_font_family(),
            font_italic: default_font_italic(),
            font_size: default_font_size(),
            font: FontConfig::default(),
            window: WindowConfig::default(),
            shell: ShellConfig::default(),
            appearance: AppearanceConfig::default(),
            cursor: CursorConfig::default(),
            behavior: BehaviorConfig::default(),
            theme: default_theme(),
            profiles: HashMap::new(),
            active_profile: None,
            shaders: ShaderConfig::default(),
            accessibility: AccessibilityConfig::default(),
            shell_integration: ShellIntegrationConfig::default(),
            performance: PerformanceConfig::default(),
            environment: EnvironmentConfig::default(),
            selection: SelectionConfig::default(),
            search: SearchColorsConfig::default(),
            keybinds: KeybindConfig::default(),
            quick_terminal: QuickTerminalConfig::default(),
            tear: MadoTearConfig::default(),
            effects: MadoEffectsConfig::default(),
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            padding: default_padding(),
            decorations: default_decorations(),
            title: None,
            unfocused_split_opacity: default_unfocused_split_opacity(),
            split_divider_color: None,
            background_image: None,
            fullscreen: false,
            maximize: false,
            inherit_working_directory: true,
            inherit_font_size: true,
            padding_balance: true,
        }
    }
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: vec![],
        }
    }
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            background: default_bg(),
            foreground: default_fg(),
            opacity: default_opacity(),
            bold_is_bright: false,
            minimum_contrast: default_minimum_contrast(),
            background_blur: false,
            unfocused_split_fill: None,
        }
    }
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            style: CursorStyle::default(),
            blink: default_cursor_blink(),
            blink_rate_ms: default_cursor_blink_rate(),
            color: default_cursor_color(),
            opacity: default_cursor_opacity(),
            text_color: None,
            click_to_move: false,
        }
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: default_scrollback(),
            copy_on_select: default_copy_on_select(),
            confirm_close: false,
            mouse_hide_while_typing: default_mouse_hide(),
            mouse_scroll_multiplier: default_mouse_scroll_mult(),
            wait_after_command: false,
            link_url: true,
            mouse_reporting: true,
            mouse_shift_capture: MouseShiftCapture::default(),
        }
    }
}

fn default_font_family() -> String {
    // "JetBrainsMono Nerd Font Mono" — the canonical pleme-io fleet
    // terminal font per `ishou-tokens::MonoFonts::pleme()`. The
    // **Mono**-suffixed Nerd Fonts family forces every glyph (icons
    // included) into a single-cell advance equal to plain
    // `JetBrains Mono`. The non-`Mono` variant widens ASCII advance
    // to ~0.83em to leave room for double-cell icons — correct for
    // editor/web monospace, but disastrous for terminal layout
    // where cell_width is measured from "MM" advance and ASCII
    // glyphs then render with a visible gap between every character
    // (the 2026-05-13 wide-gap rendering bug).
    //
    // This default is what ships if mado is invoked WITHOUT shikumi
    // config. When blackmatter-mado's HM module is enabled, it
    // writes the same name into ~/.config/mado/mado.yaml sourced
    // from `ishou::fleet-fonts` AND installs
    // `pkgs.nerd-fonts.jetbrains-mono` via home.packages — that
    // single package ships both `JetBrainsMono Nerd Font` (variable)
    // and `JetBrainsMono Nerd Font Mono` (strict-monospace) faces,
    // so the install half is identical.
    "JetBrainsMono Nerd Font Mono".into()
}

fn default_font_italic() -> String {
    // Calligraphic italic per `ishou-tokens::MonoFonts::pleme()`.
    // cosmic-text's `Attrs::style(Style::Italic)` walks the fontdb
    // for an italic face; pinning the family here lets mado render
    // italic cells in Iosevka (the fleet's calligraphic italic)
    // independent of which family the primary regular face uses.
    // blackmatter-mado's HM module installs `pkgs.iosevka` so the
    // resolution succeeds at runtime.
    "Iosevka".into()
}
fn default_font_size() -> f32 {
    14.0
}
fn default_width() -> u32 {
    1200
}
fn default_height() -> u32 {
    800
}
fn default_padding() -> u32 {
    // Operator-facing default: zero internal padding so the
    // first cell sits flush against the window edge. Pre-2026-05
    // default was 8 px to mimic VTE's slight inset; flipped to
    // 0 as part of the "minimal borders + edges everywhere"
    // operator UX. Operators who want padding back set
    // `window.padding: 8` (or whatever value they prefer) in
    // `~/.config/mado/mado.yaml`.
    0
}

fn default_decorations() -> bool {
    // Platform-aware: true on macOS so traffic-light buttons
    // stay (and platform::apply_native_styling can integrate
    // chrome via FullSizeContentView + transparent titlebar);
    // false on Linux/Windows for pure borderless. See
    // WindowConfig::decorations doc for the operator contract.
    cfg!(target_os = "macos")
}
fn default_bg() -> String {
    "#2e3440".into()
}
fn default_fg() -> String {
    "#eceff4".into()
}
fn default_opacity() -> f32 {
    1.0
}
fn default_cursor_blink() -> bool {
    true
}
fn default_cursor_blink_rate() -> u32 {
    530
}
fn default_cursor_color() -> String {
    "#eceff4".into()
}
fn default_scrollback() -> usize {
    // Operator-facing contract: "never lose anything." Host RAM
    // is the only ceiling; VecDeque grows on demand so memory
    // tracks actual scrollback usage, not the cap. Matches
    // tear-config's ScrollbackConfig::default — operators
    // get effectively-unlimited scrollback in mado AND in any
    // tear sessions mado attaches to.
    usize::MAX
}
fn default_copy_on_select() -> bool {
    false
}
fn default_mouse_hide() -> bool {
    true
}
fn default_mouse_scroll_mult() -> u32 {
    2
}
fn default_shell_integration_enabled() -> bool {
    true
}
fn default_shell_integration_features() -> Vec<String> {
    vec!["cursor".into(), "sudo".into(), "title".into()]
}
fn default_vsync() -> bool {
    true
}
// `default_target_fps` removed — the field is now `Option<u32>` with the
// `None` default deferring to `garasu::adaptive`. The hardcoded floor
// (60) lives on `PerformanceConfig::FALLBACK_FPS` so the precedence
// chain has one canonical name for it.
fn default_theme() -> String {
    "nord".into()
}
fn default_true() -> bool {
    true
}
fn default_cursor_opacity() -> f32 {
    1.0
}
fn default_unfocused_split_opacity() -> f32 {
    0.85
}
fn default_minimum_contrast() -> f32 {
    1.0
}
fn default_selection_word_chars() -> String {
    "\t'\"│`|:;,()[]{}<>$".into()
}

/// Load configuration using shikumi discovery chain.
#[allow(dead_code)]
pub fn load(override_path: &Option<PathBuf>) -> anyhow::Result<MadoConfig> {
    let path = match override_path {
        Some(p) => p.clone(),
        None => match shikumi::ConfigDiscovery::new("mado")
            .env_override("MADO_CONFIG")
            .discover()
        {
            Ok(p) => p,
            Err(_) => {
                tracing::info!("no config file found, using defaults");
                return Ok(MadoConfig::default());
            }
        },
    };

    let store = shikumi::ConfigStore::<MadoConfig>::load(&path, "MADO_")?;
    Ok(MadoConfig::clone(&store.get()))
}

/// Load configuration with hot-reload watching.
/// Returns the initial config and a store that automatically reloads on file change.
/// The `on_reload` callback is invoked when the config file changes.
pub fn load_and_watch<F>(
    override_path: &Option<PathBuf>,
    on_reload: F,
) -> anyhow::Result<(MadoConfig, shikumi::ConfigStore<MadoConfig>)>
where
    F: Fn(&MadoConfig) + Send + Sync + 'static,
{
    let path = match override_path {
        Some(p) => p.clone(),
        None => match shikumi::ConfigDiscovery::new("mado")
            .env_override("MADO_CONFIG")
            .discover()
        {
            Ok(p) => p,
            Err(_) => {
                tracing::info!("no config file found, using defaults (no hot-reload)");
                let config = MadoConfig::default();
                // Create a temp file for the store so we have something to watch
                let fallback = std::env::temp_dir().join("mado-default.yaml");
                let store = shikumi::ConfigStore::<MadoConfig>::load(&fallback, "MADO_")?;
                return Ok((config, store));
            }
        },
    };

    let store = shikumi::ConfigStore::<MadoConfig>::load_and_watch(&path, "MADO_", on_reload)?;
    let config = MadoConfig::clone(&store.get());
    Ok((config, store))
}

#[cfg(test)]
mod tear_tests {
    use super::*;

    #[test]
    fn default_tear_mode_is_auto() {
        let cfg = MadoTearConfig::default();
        assert_eq!(cfg.mode, TearMode::Auto);
        assert!(cfg.auto_spawn, "auto_spawn defaults to true");
        assert_eq!(cfg.spawn_wait_ms, 2000);
        assert!(cfg.socket.is_none());
        assert!(cfg.impose.is_none());
    }

    #[test]
    fn mado_config_default_includes_tear_section() {
        let cfg = MadoConfig::default();
        assert_eq!(cfg.tear.mode, TearMode::Auto);
    }

    #[test]
    fn tear_mode_serde_uses_snake_case() {
        let yaml_auto = serde_yaml_ng::to_string(&TearMode::Auto).unwrap();
        assert!(yaml_auto.contains("auto"));
        let yaml_attach = serde_yaml_ng::to_string(&TearMode::Attach).unwrap();
        assert!(yaml_attach.contains("attach"));
        let parsed: TearMode = serde_yaml_ng::from_str("never").unwrap();
        assert_eq!(parsed, TearMode::Never);
    }

    #[test]
    fn tear_section_parses_from_yaml() {
        let yaml = r#"
mode: always
socket: /tmp/managed-tear.sock
auto_spawn: false
spawn_wait_ms: 5000
session_name: mado-main
impose:
  prefix: C-z
  default_shell: /bin/zsh
  status_visible: false
"#;
        let cfg: MadoTearConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.mode, TearMode::Always);
        assert_eq!(
            cfg.socket.as_deref().map(|p| p.to_string_lossy().to_string()),
            Some("/tmp/managed-tear.sock".to_string())
        );
        assert!(!cfg.auto_spawn);
        assert_eq!(cfg.spawn_wait_ms, 5000);
        assert_eq!(cfg.session_name.as_deref(), Some("mado-main"));
        let imp = cfg.impose.as_ref().unwrap();
        assert_eq!(imp.prefix.as_deref(), Some("C-z"));
        assert_eq!(imp.default_shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(imp.status_visible, Some(false));
    }

    #[test]
    fn empty_tear_section_uses_all_defaults() {
        let yaml = "{}";
        let cfg: MadoTearConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.mode, TearMode::Auto);
        assert!(cfg.auto_spawn);
    }

    #[test]
    fn impose_has_any_override_is_true_only_when_set() {
        let empty = MadoTearImpose::default();
        assert!(!empty.has_any_override());

        let with_prefix = MadoTearImpose {
            prefix: Some("C-x".into()),
            ..Default::default()
        };
        assert!(with_prefix.has_any_override());

        let with_shell = MadoTearImpose {
            default_shell: Some("/bin/fish".into()),
            ..Default::default()
        };
        assert!(with_shell.has_any_override());

        let with_status = MadoTearImpose {
            status_visible: Some(false),
            ..Default::default()
        };
        assert!(with_status.has_any_override());
    }

    /// Property: applying impose-A twice is identical to applying
    /// impose-A once (idempotent under repetition). Catches future
    /// regressions where a "merge" semantics accidentally
    /// accumulates state across applies.
    #[test]
    fn impose_apply_to_is_idempotent_under_double_apply() {
        use proptest::prelude::*;
        proptest!(|(
            prefix in proptest::option::of("[A-Za-z0-9:_-]{0,20}"),
            shell in proptest::option::of("[/A-Za-z0-9_-]{1,40}"),
            status in proptest::option::of(any::<bool>())
        )| {
            let imp = MadoTearImpose {
                prefix,
                default_shell: shell,
                status_visible: status,
                scrollback: None,
            };
            let mut once = tear_config::TearConfig::default();
            imp.apply_to(&mut once);
            let mut twice = tear_config::TearConfig::default();
            imp.apply_to(&mut twice);
            imp.apply_to(&mut twice);
            prop_assert_eq!(once.prefix, twice.prefix);
            prop_assert_eq!(once.default_shell, twice.default_shell);
            prop_assert_eq!(once.status.visible, twice.status.visible);
        });
    }

    /// Property: a field whose impose value is None is preserved
    /// bit-for-bit from the input config. Catches regressions where
    /// `apply_to` accidentally overwrites with a default when the
    /// override is absent.
    #[test]
    fn impose_apply_to_leaves_none_fields_untouched() {
        use proptest::prelude::*;
        proptest!(|(
            // Random non-default starting state.
            seed_prefix in "[A-Za-z0-9:_-]{1,12}",
            seed_shell in "[/A-Za-z0-9_-]{2,30}",
            seed_status in any::<bool>(),
            // Random impose overlay (every field independently optional).
            imp_prefix in proptest::option::of("[A-Za-z0-9:_-]{1,12}"),
            imp_shell in proptest::option::of("[/A-Za-z0-9_-]{2,30}"),
            imp_status in proptest::option::of(any::<bool>())
        )| {
            let mut cfg = tear_config::TearConfig {
                prefix: seed_prefix.clone(),
                default_shell: seed_shell.clone(),
                ..tear_config::TearConfig::default()
            };
            cfg.status.visible = seed_status;
            let imp = MadoTearImpose {
                prefix: imp_prefix.clone(),
                default_shell: imp_shell.clone(),
                status_visible: imp_status,
                scrollback: None,
            };
            imp.apply_to(&mut cfg);
            match imp_prefix {
                Some(p) => prop_assert_eq!(cfg.prefix, p),
                None    => prop_assert_eq!(cfg.prefix, seed_prefix),
            }
            match imp_shell {
                Some(s) => prop_assert_eq!(cfg.default_shell, s),
                None    => prop_assert_eq!(cfg.default_shell, seed_shell),
            }
            match imp_status {
                Some(b) => prop_assert_eq!(cfg.status.visible, b),
                None    => prop_assert_eq!(cfg.status.visible, seed_status),
            }
        });
    }

    #[test]
    fn impose_apply_to_only_changes_some_fields() {
        let mut cfg = tear_config::TearConfig::default();
        let original_prefix = cfg.prefix.clone();
        let original_shell = cfg.default_shell.clone();
        let original_status_visible = cfg.status.visible;

        // Override only prefix.
        let imp = MadoTearImpose {
            prefix: Some("C-Space".into()),
            ..Default::default()
        };
        imp.apply_to(&mut cfg);
        assert_eq!(cfg.prefix, "C-Space");
        assert_eq!(cfg.default_shell, original_shell);
        assert_eq!(cfg.status.visible, original_status_visible);

        // Override only shell.
        let mut cfg2 = tear_config::TearConfig::default();
        let imp2 = MadoTearImpose {
            default_shell: Some("/bin/fish".into()),
            ..Default::default()
        };
        imp2.apply_to(&mut cfg2);
        assert_eq!(cfg2.default_shell, "/bin/fish");
        assert_eq!(cfg2.prefix, original_prefix);

        // Override all three.
        let mut cfg3 = tear_config::TearConfig::default();
        let imp3 = MadoTearImpose {
            prefix: Some("C-z".into()),
            default_shell: Some("/usr/bin/dash".into()),
            status_visible: Some(false),
            scrollback: None,
        };
        imp3.apply_to(&mut cfg3);
        assert_eq!(cfg3.prefix, "C-z");
        assert_eq!(cfg3.default_shell, "/usr/bin/dash");
        assert!(!cfg3.status.visible);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values() {
        let config = MadoConfig::default();
        assert_eq!(config.font_family, "JetBrainsMono Nerd Font Mono");
        assert_eq!(config.font_italic, "Iosevka");
        assert_eq!(config.font_size, 14.0);
        assert_eq!(config.theme, "nord");
        assert!(config.active_profile.is_none());
        assert_eq!(config.window.width, 1200);
        assert_eq!(config.window.height, 800);
        // Operator-facing default flipped to 0 (minimal edges).
        assert_eq!(config.window.padding, 0);
        // Platform-aware decorations default: true on macOS so
        // traffic-light buttons stay (and apply_native_styling
        // integrates the chrome); false on Linux/Windows for
        // pure borderless. See WindowConfig::decorations doc.
        assert_eq!(config.window.decorations, cfg!(target_os = "macos"));
        assert!(config.window.title.is_none());
        assert!((config.window.unfocused_split_opacity - 0.85).abs() < 0.001);
        assert!(config.window.split_divider_color.is_none());
        assert!(config.window.background_image.is_none());
        assert!(!config.window.fullscreen);
        assert!(!config.window.maximize);
        assert!(config.window.inherit_working_directory);
        assert!(config.window.inherit_font_size);
        assert!(config.window.padding_balance);
        assert_eq!(config.appearance.background, "#2e3440");
        assert_eq!(config.appearance.foreground, "#eceff4");
        assert_eq!(config.appearance.opacity, 1.0);
        assert!(!config.appearance.bold_is_bright);
        assert!((config.appearance.minimum_contrast - 1.0).abs() < 0.001);
        assert!(!config.appearance.background_blur);
        assert!(config.appearance.unfocused_split_fill.is_none());
        assert_eq!(config.cursor.style, CursorStyle::Block);
        assert!(config.cursor.blink);
        assert_eq!(config.cursor.blink_rate_ms, 530);
        assert_eq!(config.cursor.color, "#eceff4");
        assert!((config.cursor.opacity - 1.0).abs() < 0.001);
        assert!(config.cursor.text_color.is_none());
        assert!(!config.cursor.click_to_move);
        // Operator-facing default: "never lose anything"; host
        // RAM is the only ceiling. VecDeque grows on demand.
        assert_eq!(config.behavior.scrollback_lines, usize::MAX);
        assert!(!config.behavior.copy_on_select);
        assert!(!config.behavior.confirm_close);
        assert!(config.behavior.mouse_hide_while_typing);
        assert_eq!(config.behavior.mouse_scroll_multiplier, 2);
        assert!(!config.behavior.wait_after_command);
        assert!(config.behavior.link_url);
        assert!(config.behavior.mouse_reporting);
        assert_eq!(config.behavior.mouse_shift_capture, MouseShiftCapture::False);
        assert!(config.shell_integration.enabled);
        assert_eq!(config.shell_integration.features, ["cursor", "sudo", "title"]);
        assert!(config.performance.vsync);
        assert_eq!(config.performance.target_fps, None);
        assert_eq!(config.performance.fps_cap, None);
        assert_eq!(config.performance.battery_fps_cap, None);
        assert!(!config.shaders.enabled);
        assert!(config.shaders.files.is_empty());
        assert_eq!(config.accessibility.colorblind, ColorblindMode::None);
        assert_eq!(config.accessibility.min_contrast, 0.0);
        assert_eq!(config.accessibility.font_scale, 1.0);
        assert!(!config.accessibility.reduce_motion);
        assert!(config.environment.vars.is_empty());
        assert!(config.environment.working_directory.is_none());
        assert!(config.environment.initial_command.is_none());
        // Selection config
        assert!(config.selection.foreground.is_none());
        assert!(config.selection.background.is_none());
        assert!(config.selection.clear_on_typing);
        assert!(!config.selection.clear_on_copy);
        assert!(!config.selection.word_chars.is_empty());
        // Search colors config
        assert!(config.search.foreground.is_none());
        assert!(config.search.background.is_none());
        assert!(config.search.selected_foreground.is_none());
        assert!(config.search.selected_background.is_none());
        // Font config
        assert!(config.font.family_bold.is_none());
        assert!(config.font.family_italic.is_none());
        assert!(config.font.family_bold_italic.is_none());
        assert!(!config.font.thicken);
        assert!(config.font.synthetic_style);
        assert!(config.font.features.is_empty());
        assert!(config.font.codepoint_map.is_empty());
        // Keybind config
        assert!(config.keybinds.custom.is_empty());
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = MadoConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: MadoConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.font_family, restored.font_family);
        assert_eq!(config.font_size, restored.font_size);
        assert_eq!(config.theme, restored.theme);
        assert_eq!(config.window.width, restored.window.width);
        assert_eq!(config.cursor.style, restored.cursor.style);
    }

    #[test]
    fn test_config_yaml_deserialization() {
        let yaml = r#"
font_family: "Fira Code"
font_size: 16
theme: "dracula"
active_profile: "light"
window:
  width: 1600
  height: 900
"#;
        let config: MadoConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.font_family, "Fira Code");
        assert_eq!(config.font_size, 16.0);
        assert_eq!(config.theme, "dracula");
        assert_eq!(config.active_profile.as_deref(), Some("light"));
        assert_eq!(config.window.width, 1600);
        assert_eq!(config.window.height, 900);
    }

    #[test]
    fn test_with_profile_applies_overrides() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "coding".to_string(),
            ProfileConfig {
                font_family: Some("Fira Code".into()),
                font_size: Some(16.0),
                theme: Some("dracula".into()),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig {
            profiles,
            ..MadoConfig::default()
        };
        let applied = config.with_profile("coding");
        assert_eq!(applied.font_family, "Fira Code");
        assert_eq!(applied.font_size, 16.0);
        assert_eq!(applied.theme, "dracula");
    }

    #[test]
    fn test_with_profile_nonexistent_returns_clone() {
        let config = MadoConfig::default();
        let applied = config.with_profile("nonexistent");
        assert_eq!(applied.font_family, config.font_family);
        assert_eq!(applied.font_size, config.font_size);
        assert_eq!(applied.theme, config.theme);
    }

    #[test]
    fn test_cursor_style_variants() {
        for style in [CursorStyle::Block, CursorStyle::BlockHollow, CursorStyle::Bar, CursorStyle::Underline] {
            let json = serde_json::to_string(&style).unwrap();
            let restored: CursorStyle = serde_json::from_str(&json).unwrap();
            assert_eq!(style, restored);
        }
    }

    #[test]
    fn test_colorblind_mode_variants() {
        for mode in [
            ColorblindMode::None,
            ColorblindMode::Protanopia,
            ColorblindMode::Deuteranopia,
            ColorblindMode::Tritanopia,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let restored: ColorblindMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, restored);
        }
    }

    #[test]
    fn test_window_config_defaults() {
        let w = WindowConfig::default();
        assert_eq!(w.width, 1200);
        assert_eq!(w.height, 800);
        assert_eq!(w.padding, 0);
    }

    #[test]
    fn test_shell_config_defaults() {
        let s = ShellConfig::default();
        assert!(s.command.is_none());
        assert!(s.args.is_empty());
    }

    #[test]
    fn test_appearance_config_defaults() {
        let a = AppearanceConfig::default();
        assert_eq!(a.background, "#2e3440");
        assert_eq!(a.foreground, "#eceff4");
        assert_eq!(a.opacity, 1.0);
        assert!(!a.bold_is_bright);
    }

    #[test]
    fn test_cursor_config_defaults() {
        let c = CursorConfig::default();
        assert_eq!(c.style, CursorStyle::Block);
        assert!(c.blink);
        assert_eq!(c.blink_rate_ms, 530);
        assert_eq!(c.color, "#eceff4");
    }

    #[test]
    fn test_behavior_config_defaults() {
        let b = BehaviorConfig::default();
        assert_eq!(b.scrollback_lines, usize::MAX);
        assert!(!b.copy_on_select);
        assert!(!b.confirm_close);
        assert!(b.mouse_hide_while_typing);
        assert_eq!(b.mouse_scroll_multiplier, 2);
    }

    #[test]
    fn test_shader_config_defaults() {
        let s = ShaderConfig::default();
        assert!(!s.enabled);
        assert!(s.files.is_empty());
    }

    #[test]
    fn test_accessibility_config_defaults() {
        let a = AccessibilityConfig::default();
        assert_eq!(a.colorblind, ColorblindMode::None);
        assert_eq!(a.min_contrast, 0.0);
        assert_eq!(a.font_scale, 1.0);
        assert!(!a.reduce_motion);
    }

    #[test]
    fn test_profile_config_default_all_none() {
        let p = ProfileConfig::default();
        assert!(p.font_family.is_none());
        assert!(p.font_size.is_none());
        assert!(p.font.is_none());
        assert!(p.theme.is_none());
        assert!(p.appearance.is_none());
        assert!(p.cursor.is_none());
        assert!(p.shell.is_none());
        assert!(p.behavior.is_none());
        assert!(p.performance.is_none());
        assert!(p.environment.is_none());
        assert!(p.selection.is_none());
        assert!(p.window.is_none());
    }

    #[test]
    fn test_config_with_profile_font_override() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "large".to_string(),
            ProfileConfig {
                font_family: Some("Monaco".into()),
                font_size: Some(18.0),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig {
            profiles,
            ..MadoConfig::default()
        };
        let applied = config.with_profile("large");
        assert_eq!(applied.font_family, "Monaco");
        assert_eq!(applied.font_size, 18.0);
        assert_eq!(applied.theme, "nord");
    }

    #[test]
    fn test_config_with_profile_theme_override() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "light".to_string(),
            ProfileConfig {
                theme: Some("solarized-light".into()),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig {
            profiles,
            ..MadoConfig::default()
        };
        let applied = config.with_profile("light");
        assert_eq!(applied.theme, "solarized-light");
    }

    #[test]
    fn test_shell_integration_config_defaults() {
        let si = ShellIntegrationConfig::default();
        assert!(si.enabled);
        assert_eq!(si.features, ["cursor", "sudo", "title"]);
    }

    #[test]
    fn test_performance_config_defaults() {
        let p = PerformanceConfig::default();
        assert!(p.vsync);
        assert_eq!(p.target_fps, None);
        assert_eq!(p.fps_cap, None);
        assert_eq!(p.battery_fps_cap, None);
    }

    #[test]
    fn resolve_target_fps_user_wins_over_detection() {
        let p = PerformanceConfig {
            vsync: true,
            target_fps: Some(144),
            fps_cap: Some(60),
            battery_fps_cap: None,
        };
        // Posture present, but user-set value preempts.
        let posture = make_posture_with_refresh(Some(120));
        assert_eq!(p.resolve_target_fps(Some(&posture)), 144);
    }

    #[test]
    fn resolve_target_fps_detection_when_no_user_value() {
        let p = PerformanceConfig {
            vsync: true,
            target_fps: None,
            fps_cap: None,
            battery_fps_cap: None,
        };
        let posture = make_posture_with_refresh(Some(120));
        assert_eq!(p.resolve_target_fps(Some(&posture)), 120);
    }

    #[test]
    fn resolve_target_fps_detection_respects_fps_cap() {
        let p = PerformanceConfig {
            vsync: true,
            target_fps: None,
            fps_cap: Some(90),
            battery_fps_cap: None,
        };
        let posture = make_posture_with_refresh(Some(240));
        assert_eq!(p.resolve_target_fps(Some(&posture)), 90);
    }

    #[test]
    fn resolve_target_fps_falls_back_to_60_when_no_posture() {
        let p = PerformanceConfig::default();
        assert_eq!(p.resolve_target_fps(None), PerformanceConfig::FALLBACK_FPS);
        assert_eq!(p.resolve_target_fps(None), 60);
    }

    #[test]
    fn resolve_target_fps_falls_back_to_60_when_posture_has_no_refresh() {
        let p = PerformanceConfig::default();
        let posture = make_posture_with_refresh(None);
        assert_eq!(p.resolve_target_fps(Some(&posture)), 60);
    }

    fn make_posture_with_refresh(hz: Option<u32>) -> garasu::adaptive::RuntimePosture {
        garasu::adaptive::RuntimePosture {
            displays: vec![garasu::adaptive::Display {
                name: Some("test".into()),
                size: (1920, 1080),
                scale_factor: 2.0,
                refresh_hz: hz,
                primary: true,
            }],
            gpu: None,
            platform: garasu::adaptive::detect_platform(),
            high_refresh: hz.is_some_and(|v| v > 60),
        }
    }

    #[test]
    fn performance_yaml_missing_fields_default_to_none() {
        // The bare minimum YAML — only vsync set — leaves all
        // adaptive-eligible fields at None so detection can fill them.
        let yaml = "vsync: true\n";
        let p: PerformanceConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(p.vsync);
        assert_eq!(p.target_fps, None);
        assert_eq!(p.fps_cap, None);
        assert_eq!(p.battery_fps_cap, None);
    }

    #[test]
    fn performance_yaml_explicit_target_fps_deserializes_as_some() {
        // Operator explicitly pinning target_fps: 144 → Some(144).
        let yaml = "vsync: true\ntarget_fps: 144\n";
        let p: PerformanceConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(p.target_fps, Some(144));
        assert_eq!(p.fps_cap, None);
    }

    #[test]
    fn performance_yaml_all_caps_present() {
        let yaml = concat!(
            "vsync: false\n",
            "target_fps: 60\n",
            "fps_cap: 120\n",
            "battery_fps_cap: 30\n",
        );
        let p: PerformanceConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!p.vsync);
        assert_eq!(p.target_fps, Some(60));
        assert_eq!(p.fps_cap, Some(120));
        assert_eq!(p.battery_fps_cap, Some(30));
    }

    #[test]
    fn performance_yaml_explicit_null_target_fps_round_trips() {
        // Some YAML authors write `target_fps: null` to signal "no
        // override, let adaptive decide." Must deserialize as None.
        let yaml = "vsync: true\ntarget_fps: null\n";
        let p: PerformanceConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(p.target_fps, None);
    }

    #[test]
    fn performance_serde_full_round_trip() {
        let p = PerformanceConfig {
            vsync: true,
            target_fps: Some(120),
            fps_cap: Some(240),
            battery_fps_cap: Some(60),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: PerformanceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.vsync, p.vsync);
        assert_eq!(back.target_fps, p.target_fps);
        assert_eq!(back.fps_cap, p.fps_cap);
        assert_eq!(back.battery_fps_cap, p.battery_fps_cap);
    }

    #[test]
    fn performance_fallback_fps_is_60() {
        // Pin the canonical hardcoded floor as a named constant. Future
        // changes to this number break this test on purpose — it's a
        // promise to operators that without detection AND without
        // config, mado lands on the universally-supported 60Hz baseline.
        assert_eq!(PerformanceConfig::FALLBACK_FPS, 60);
    }

    #[test]
    fn resolve_target_fps_battery_cap_only_kicks_in_when_forced() {
        // The battery_fps_cap field is plumbed but force_battery_mode
        // is always false in M0. Setting battery_fps_cap alone has no
        // effect on the resolved fps until force_battery_mode flips.
        let p = PerformanceConfig {
            vsync: true,
            target_fps: None,
            fps_cap: Some(120),
            battery_fps_cap: Some(30),
        };
        let posture = make_posture_with_refresh(Some(144));
        // Without force_battery_mode, fps_cap (120) clamps, not battery_fps_cap.
        assert_eq!(p.resolve_target_fps(Some(&posture)), 120);
    }

    #[test]
    fn resolve_target_fps_explicit_zero_is_honoured() {
        // Edge case: operator pins target_fps: 0 (uncapped). User
        // explicit always wins, even if it's a degenerate value. The
        // renderer is responsible for sanity-checking; resolve_*
        // doesn't second-guess.
        let p = PerformanceConfig {
            vsync: true,
            target_fps: Some(0),
            fps_cap: None,
            battery_fps_cap: None,
        };
        let posture = make_posture_with_refresh(Some(120));
        assert_eq!(p.resolve_target_fps(Some(&posture)), 0);
    }

    #[test]
    fn resolve_target_fps_with_120hz_promotion() {
        // Real-world case: 14" MBP with ProMotion (120Hz variable).
        // User left target_fps unset; we want the renderer to know.
        let p = PerformanceConfig::default();
        let posture = make_posture_with_refresh(Some(120));
        assert_eq!(p.resolve_target_fps(Some(&posture)), 120);
    }

    #[test]
    fn resolve_target_fps_with_60hz_external_display() {
        // Real-world case: external 4K monitor at 60Hz on the same MBP.
        // Detected refresh becomes the new default — operator never
        // needed to touch config.
        let p = PerformanceConfig::default();
        let posture = make_posture_with_refresh(Some(60));
        assert_eq!(p.resolve_target_fps(Some(&posture)), 60);
    }

    #[test]
    fn test_config_with_active_profile() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "coding".to_string(),
            ProfileConfig {
                font_family: Some("Fira Code".into()),
                font_size: Some(16.0),
                theme: Some("dracula".into()),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig {
            active_profile: Some("coding".into()),
            profiles: profiles.clone(),
            ..MadoConfig::default()
        };
        let applied = config.with_profile("coding");
        assert_eq!(applied.font_family, "Fira Code");
        assert_eq!(applied.font_size, 16.0);
        assert_eq!(applied.theme, "dracula");
    }

    #[test]
    fn test_config_with_profile_performance_override() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "gaming".to_string(),
            ProfileConfig {
                performance: Some(PerformanceConfig {
                    vsync: false,
                    target_fps: Some(240),
                    fps_cap: None,
                    battery_fps_cap: None,
                }),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig {
            profiles,
            ..MadoConfig::default()
        };
        let applied = config.with_profile("gaming");
        assert!(!applied.performance.vsync);
        assert_eq!(applied.performance.target_fps, Some(240));
    }

    #[test]
    fn test_behavior_config_new_fields() {
        let b = BehaviorConfig::default();
        assert_eq!(b.confirm_close, false);
        assert_eq!(b.mouse_hide_while_typing, true);
        assert_eq!(b.mouse_scroll_multiplier, 2);
    }

    #[test]
    fn test_active_profile_none_by_default() {
        let config = MadoConfig::default();
        assert!(config.active_profile.is_none());
    }

    #[test]
    fn test_config_yaml_with_active_profile() {
        let yaml = r#"
active_profile: "dark"
"#;
        let config: MadoConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.active_profile.as_deref(), Some("dark"));
    }

    #[test]
    fn test_font_config_defaults() {
        let f = FontConfig::default();
        assert!(f.family_bold.is_none());
        assert!(f.family_italic.is_none());
        assert!(f.family_bold_italic.is_none());
        assert!(!f.thicken);
        assert!(f.synthetic_style);
        assert!(f.features.is_empty());
        assert!(f.codepoint_map.is_empty());
    }

    #[test]
    fn test_font_config_yaml() {
        let yaml = concat!(
            "family_bold: Fira Code Bold\n",
            "family_italic: Fira Code Italic\n",
            "thicken: true\n",
            "synthetic_style: false\n",
            "features:\n",
            "  - '-calt'\n",
            "  - '-liga'\n",
        );
        let f: FontConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(f.family_bold.as_deref(), Some("Fira Code Bold"));
        assert_eq!(f.family_italic.as_deref(), Some("Fira Code Italic"));
        assert!(f.thicken);
        assert!(!f.synthetic_style);
        assert_eq!(f.features, vec!["-calt", "-liga"]);
    }

    #[test]
    fn test_selection_config_defaults() {
        let s = SelectionConfig::default();
        assert!(s.foreground.is_none());
        assert!(s.background.is_none());
        assert!(s.clear_on_typing);
        assert!(!s.clear_on_copy);
        assert!(s.word_chars.contains('\t'));
        assert!(s.word_chars.contains('|'));
    }

    #[test]
    fn test_selection_config_yaml() {
        let yaml = "foreground: '#ffffff'\nbackground: '#005577'\nclear_on_typing: false\nclear_on_copy: true\n";
        let s: SelectionConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(s.foreground.as_deref(), Some("#ffffff"));
        assert_eq!(s.background.as_deref(), Some("#005577"));
        assert!(!s.clear_on_typing);
        assert!(s.clear_on_copy);
    }

    #[test]
    fn test_search_colors_config_defaults() {
        let s = SearchColorsConfig::default();
        assert!(s.foreground.is_none());
        assert!(s.background.is_none());
        assert!(s.selected_foreground.is_none());
        assert!(s.selected_background.is_none());
    }

    #[test]
    fn test_search_colors_config_yaml() {
        let yaml = "foreground: '#000000'\nbackground: '#ffcc00'\nselected_foreground: '#000000'\nselected_background: '#ff9900'\n";
        let s: SearchColorsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(s.foreground.as_deref(), Some("#000000"));
        assert_eq!(s.background.as_deref(), Some("#ffcc00"));
        assert_eq!(s.selected_foreground.as_deref(), Some("#000000"));
        assert_eq!(s.selected_background.as_deref(), Some("#ff9900"));
    }

    #[test]
    fn test_keybind_config_yaml() {
        let yaml = concat!(
            "custom:\n",
            "  - trigger: cmd+k\n",
            "    action: clear_screen\n",
            "  - trigger: ctrl+shift+c\n",
            "    action: copy\n",
        );
        let k: KeybindConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(k.custom.len(), 2);
        assert_eq!(k.custom[0].trigger, "cmd+k");
        assert_eq!(k.custom[0].action, "clear_screen");
        assert_eq!(k.custom[1].trigger, "ctrl+shift+c");
        assert_eq!(k.custom[1].action, "copy");
    }

    #[test]
    fn test_cursor_style_block_hollow() {
        let style = CursorStyle::BlockHollow;
        let json = serde_json::to_string(&style).unwrap();
        assert_eq!(json, "\"block_hollow\"");
        let restored: CursorStyle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, CursorStyle::BlockHollow);
    }

    #[test]
    fn test_cursor_config_new_fields() {
        let c = CursorConfig::default();
        assert!((c.opacity - 1.0).abs() < 0.001);
        assert!(c.text_color.is_none());
        assert!(!c.click_to_move);
    }

    #[test]
    fn test_window_config_new_fields() {
        let w = WindowConfig::default();
        // Platform-aware: macOS keeps decorations (traffic
        // lights stay usable); Linux/Windows borderless.
        assert_eq!(w.decorations, cfg!(target_os = "macos"));
        assert!(w.title.is_none());
        assert!((w.unfocused_split_opacity - 0.85).abs() < 0.001);
        assert!(!w.fullscreen);
        assert!(!w.maximize);
        assert!(w.inherit_working_directory);
        assert!(w.inherit_font_size);
        assert!(w.padding_balance);
    }

    #[test]
    fn test_behavior_config_ghostty_fields() {
        let b = BehaviorConfig::default();
        assert!(!b.wait_after_command);
        assert!(b.link_url);
        assert!(b.mouse_reporting);
        assert_eq!(b.mouse_shift_capture, MouseShiftCapture::False);
    }

    #[test]
    fn test_mouse_shift_capture_variants() {
        for variant in [
            MouseShiftCapture::False,
            MouseShiftCapture::True,
            MouseShiftCapture::Never,
            MouseShiftCapture::Always,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let restored: MouseShiftCapture = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, restored);
        }
    }

    #[test]
    fn test_appearance_config_new_fields() {
        let a = AppearanceConfig::default();
        assert!((a.minimum_contrast - 1.0).abs() < 0.001);
        assert!(!a.background_blur);
        assert!(a.unfocused_split_fill.is_none());
    }

    #[test]
    fn test_environment_config_defaults() {
        let e = EnvironmentConfig::default();
        assert!(e.vars.is_empty());
        assert!(e.working_directory.is_none());
        assert!(e.initial_command.is_none());
    }

    #[test]
    fn test_environment_config_yaml() {
        let yaml = concat!(
            "vars:\n",
            "  EDITOR: nvim\n",
            "  MY_VAR: hello\n",
            "working_directory: /tmp/test\n",
            "initial_command: nvim\n",
        );
        let e: EnvironmentConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(e.vars.get("EDITOR").unwrap(), "nvim");
        assert_eq!(e.vars.get("MY_VAR").unwrap(), "hello");
        assert_eq!(e.working_directory.as_ref().unwrap().to_str().unwrap(), "/tmp/test");
        assert_eq!(e.initial_command.as_deref(), Some("nvim"));
    }

    #[test]
    fn test_full_config_yaml_roundtrip() {
        let yaml = concat!(
            "font_family: Hack\n",
            "font_size: 13.5\n",
            "theme: dracula\n",
            "font:\n",
            "  family_bold: Hack Bold\n",
            "  thicken: true\n",
            "  features:\n",
            "    - '-liga'\n",
            "window:\n",
            "  width: 1920\n",
            "  height: 1080\n",
            "  decorations: false\n",
            "  fullscreen: true\n",
            "  maximize: true\n",
            "selection:\n",
            "  foreground: '#ff0000'\n",
            "  clear_on_typing: false\n",
            "cursor:\n",
            "  style: bar\n",
            "  opacity: 0.8\n",
            "  text_color: '#000000'\n",
            "behavior:\n",
            "  wait_after_command: true\n",
            "  link_url: false\n",
            "  mouse_reporting: false\n",
        );
        let config: MadoConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.font_family, "Hack");
        assert_eq!(config.font_size, 13.5);
        assert_eq!(config.theme, "dracula");
        assert_eq!(config.font.family_bold.as_deref(), Some("Hack Bold"));
        assert!(config.font.thicken);
        assert_eq!(config.font.features, vec!["-liga"]);
        assert_eq!(config.window.width, 1920);
        assert_eq!(config.window.height, 1080);
        assert!(!config.window.decorations);
        assert!(config.window.fullscreen);
        assert!(config.window.maximize);
        assert_eq!(config.selection.foreground.as_deref(), Some("#ff0000"));
        assert!(!config.selection.clear_on_typing);
        assert_eq!(config.cursor.style, CursorStyle::Bar);
        assert!((config.cursor.opacity - 0.8).abs() < 0.001);
        assert_eq!(config.cursor.text_color.as_deref(), Some("#000000"));
        assert!(config.behavior.wait_after_command);
        assert!(!config.behavior.link_url);
        assert!(!config.behavior.mouse_reporting);
    }

    #[test]
    fn test_with_profile_selection_override() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "highlight".to_string(),
            ProfileConfig {
                selection: Some(SelectionConfig {
                    foreground: Some("#ffffff".into()),
                    background: Some("#ff0000".into()),
                    ..SelectionConfig::default()
                }),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig { profiles, ..MadoConfig::default() };
        let applied = config.with_profile("highlight");
        assert_eq!(applied.selection.foreground.as_deref(), Some("#ffffff"));
        assert_eq!(applied.selection.background.as_deref(), Some("#ff0000"));
        assert!(applied.selection.clear_on_typing);
    }

    #[test]
    fn test_with_profile_window_override() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "fullscreen".to_string(),
            ProfileConfig {
                window: Some(WindowConfig {
                    fullscreen: true,
                    maximize: true,
                    ..WindowConfig::default()
                }),
                ..ProfileConfig::default()
            },
        );
        let config = MadoConfig { profiles, ..MadoConfig::default() };
        let applied = config.with_profile("fullscreen");
        assert!(applied.window.fullscreen);
        assert!(applied.window.maximize);
    }

    // ── Quick Terminal ──────────────────────────────────────────────────────

    #[test]
    fn quick_terminal_defaults_are_opt_in() {
        let qt = QuickTerminalConfig::default();
        assert!(!qt.enabled);
        assert_eq!(qt.edge, QuickTerminalEdge::Top);
        assert!((qt.size_fraction - 0.4).abs() < 1e-6);
        assert_eq!(qt.animation_ms, 150);
        assert!(qt.autohide_on_blur);
        assert!(qt.hotkey.is_empty());
        assert!(!qt.is_active_hotkey());
    }

    #[test]
    fn quick_terminal_is_active_hotkey_requires_both_fields() {
        let qt = QuickTerminalConfig {
            enabled: true,
            hotkey: String::new(),
            ..Default::default()
        };
        assert!(!qt.is_active_hotkey(), "enabled w/o hotkey is MCP-only");

        let qt = QuickTerminalConfig {
            enabled: false,
            hotkey: "cmd+`".into(),
            ..Default::default()
        };
        assert!(!qt.is_active_hotkey(), "hotkey w/o enabled stays dormant");

        let qt = QuickTerminalConfig {
            enabled: true,
            hotkey: "cmd+`".into(),
            ..Default::default()
        };
        assert!(qt.is_active_hotkey());
    }

    #[test]
    fn quick_terminal_resolves_size_for_each_edge() {
        let screen = (1600u32, 1000u32);

        // Top / Bottom: full width, fractional height.
        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Top,
            size_fraction: 0.5,
            ..Default::default()
        };
        assert_eq!(qt.resolve_size_pixels(screen), (1600, 500));
        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Bottom,
            size_fraction: 0.3,
            ..Default::default()
        };
        assert_eq!(qt.resolve_size_pixels(screen), (1600, 300));

        // Left / Right: fractional width, full height.
        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Left,
            size_fraction: 0.25,
            ..Default::default()
        };
        assert_eq!(qt.resolve_size_pixels(screen), (400, 1000));

        // Center: fractional in both axes.
        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Center,
            size_fraction: 0.5,
            ..Default::default()
        };
        assert_eq!(qt.resolve_size_pixels(screen), (800, 500));
    }

    #[test]
    fn quick_terminal_clamps_size_fraction() {
        let screen = (1000u32, 800u32);

        // Below floor (0.1).
        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Top,
            size_fraction: -1.0,
            ..Default::default()
        };
        assert_eq!(qt.resolve_size_pixels(screen), (1000, 80));

        // Above ceiling (1.0).
        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Top,
            size_fraction: 5.0,
            ..Default::default()
        };
        assert_eq!(qt.resolve_size_pixels(screen), (1000, 800));
    }

    #[test]
    fn quick_terminal_origin_pins_to_edge() {
        let screen = (1600u32, 1000u32);

        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Top,
            size_fraction: 0.5,
            ..Default::default()
        };
        assert_eq!(qt.resolve_origin_pixels(screen), (0, 0));

        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Bottom,
            size_fraction: 0.3,
            ..Default::default()
        };
        // Bottom edge: origin.y = screen.h - window.h.
        assert_eq!(qt.resolve_origin_pixels(screen), (0, 700));

        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Right,
            size_fraction: 0.25,
            ..Default::default()
        };
        // Right edge: origin.x = screen.w - window.w.
        assert_eq!(qt.resolve_origin_pixels(screen), (1200, 0));

        let qt = QuickTerminalConfig {
            edge: QuickTerminalEdge::Center,
            size_fraction: 0.5,
            ..Default::default()
        };
        // Center: origin = (screen - window) / 2.
        assert_eq!(qt.resolve_origin_pixels(screen), (400, 250));
    }

    #[test]
    fn quick_terminal_deserializes_from_snake_case_edge() {
        // Edge enum uses serde rename_all = snake_case, so YAML
        // authors write `edge: bottom` / `edge: center`.
        let yaml = r#"
            enabled: true
            edge: bottom
            size_fraction: 0.35
            hotkey: "cmd+`"
            "#;
        let qt: QuickTerminalConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(qt.enabled);
        assert_eq!(qt.edge, QuickTerminalEdge::Bottom);
        assert!((qt.size_fraction - 0.35).abs() < 1e-6);
        assert_eq!(qt.hotkey, "cmd+`");
        // Non-specified fields fall back to defaults.
        assert_eq!(qt.animation_ms, 150);
        assert!(qt.autohide_on_blur);
    }
}
