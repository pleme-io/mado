use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, pleme_fleet_themed_derive::FleetThemed)]
// ── ★★ EMITTER SUBSTRATE: the FleetThemedConfig impl is DERIVED ──
// The flagship hand-written `impl FleetThemedConfig` (the fleet-audit
// reference) is now `#[derive(FleetThemed)]` + per-field `#[fleet(…)]`
// attributes. The flat `FleetDefaults → field` assignments are
// mechanized; the genuinely-unique tail (theme-surface mapping, per-OS
// decoration split, cursor name→enum map, the scrollback override) lives
// in the named `*_from_fleet` escape-hatch fns + the `finalize` fn — see
// below the struct. `base = "mado_fleet_base"` supplies the per-section
// `*Config::default()` values for every untouched field via `..base()`.
#[fleet(base = "mado_fleet_base", finalize = mado_fleet_scrollback_floor)]
pub struct MadoConfig {
    #[serde(default = "default_font_family")]
    #[fleet]
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
    #[fleet]
    pub font_italic: String,
    /// Family used for powerline separators (U+E0B0…) and Nerd-Font
    /// Private-Use-Area icon codepoints (see
    /// [`crate::glyph_class::is_symbol_glyph`]). Routing the symbol
    /// ranges to a dedicated family — ghostty's "Symbols Nerd Font"
    /// model — keeps icon glyphs shaping from ONE curated source
    /// instead of whatever installed font cosmic-text's coverage walk
    /// picks first (the "wrong glyph in mado, right in ghostty" class).
    /// Empty = no preference; symbol cells then shape against
    /// `font_family` (which, on the default JetBrainsMono Nerd Font,
    /// already carries the patched ranges). Sourced from
    /// `ishou-tokens::MonoFonts` when blackmatter-mado renders the YAML.
    #[serde(default = "default_font_symbols")]
    pub font_symbols: String,
    #[serde(default = "default_font_size")]
    #[fleet(font_size, copy)]
    pub font_size: f32,
    /// Cell-height multiplier — the line rhythm. The rendered cell is
    /// `font_size * line_height` (logical px); cosmic-text's line-box
    /// metric is set to the same product so measured glyph rows match
    /// the cell. Sourced from `FleetDefaults::line_height` (ghostty's
    /// native 1.32 × its +25% cell = 1.65) via `from_fleet`. Replaces
    /// the renderer's old hardcoded `* 1.4`, which ignored config and
    /// produced a cramped rhythm vs ghostty's airier cell.
    #[serde(default = "default_line_height")]
    #[fleet(line_height, copy)]
    pub line_height: f32,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    #[fleet(with = mado_window_from_fleet)]
    pub window: WindowConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    #[fleet(with = mado_appearance_from_fleet)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    #[fleet(with = mado_cursor_from_fleet)]
    pub cursor: CursorConfig,
    #[serde(default)]
    #[fleet(with = mado_behavior_from_fleet)]
    pub behavior: BehaviorConfig,
    #[serde(default = "default_theme")]
    #[fleet(with = mado_theme_name_from_fleet)]
    pub theme: String,
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub shaders: ShaderConfig,
    #[serde(default)]
    #[fleet(with = mado_accessibility_from_fleet)]
    pub accessibility: AccessibilityConfig,
    #[serde(default)]
    pub shell_integration: ShellIntegrationConfig,
    #[serde(default)]
    #[fleet(with = mado_performance_from_fleet)]
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
    /// Opt-in visual effects rendered as overlays after the text
    /// pass. **Snow defaults OFF** as of the May 2026 prescribed
    /// default (matches the clean blackmatter + stylix + nord-dark
    /// fleet look). Set `effects.snow.enabled = true` in
    /// `~/.config/mado/mado.yaml` to enable.
    #[serde(default)]
    pub effects: MadoEffectsConfig,
    /// Embedded vigy reconciler runtime. **Defaults OFF.** Operator
    /// sets `vigy.enabled = true` to spawn the in-process
    /// tatara-lisp reconciler runtime + open the vigy MCP tool
    /// surface. When disabled, the GUI thread + MCP-path init are
    /// skipped and the vigy MCP tools (vigy_register / vigy_list
    /// / vigy_inspect / vigy_tick / vigy_delete) return a typed
    /// `{ok: false, error: "vigy disabled in mado config"}` so
    /// callers see a clean reason rather than a panic or hang.
    #[serde(default)]
    pub vigy: MadoVigyConfig,
    /// The continuously-refreshing task-suggestion stream the Ctrl-S picker
    /// shades in (see [`SuggestionsConfig`] + `crate::suggest`). Prescribed
    /// default ON with a gentle cadence; the bare tier strips it off.
    #[serde(default)]
    pub suggestions: SuggestionsConfig,
    /// The safra observability-curation plane (see `crate::safra` +
    /// `docs/SAFRA.md`): tracked environments × data-kinds → curated signals
    /// projected onto the Ctrl-S board. **Defaults OFF** — a private config
    /// layer (blackmatter) supplies the environments, SecretRefs, and tuning.
    #[serde(default)]
    pub safra: crate::safra::SafraConfig,
    /// The reactive session-janitor plane (see [`JanitorsConfig`] +
    /// `crate::janitors` + `docs/JANITORS.md`). Prescribed default ON,
    /// **shadow-first**; the bare tier strips it off.
    #[serde(default)]
    pub janitors: JanitorsConfig,
    /// Clickable, highlighted links (see [`MadoLinksConfig`]). Prescribed
    /// default ON (highlight + hover cursor + click-to-open); the bare
    /// tier strips every affordance.
    #[serde(default)]
    pub links: MadoLinksConfig,
    /// Tasteful-feedback flourishes (see [`FeedbackConfig`]). Prescribed
    /// default ON (visual bell + copy flash + exit-code coloring); the
    /// bare tier strips them.
    #[serde(default)]
    pub feedback: FeedbackConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    /// Desktop-notification system (see [`NotificationsConfig`] +
    /// `docs/NOTIFICATIONS.md`). Prescribed default ON: native
    /// UNUserNotificationCenter backend (no Script-Editor popup),
    /// focus-aware, command-completion notify, OSC 9/777/99. The bare
    /// tier disables the whole subsystem.
    #[serde(default)]
    pub notifications: NotificationsConfig,
    /// Motion-easing knobs (see [`MotionConfig`]). Prescribed default ON
    /// (blink ease + picker animate + scroll lerp + unfocused dim); the
    /// bare tier makes every transition instant.
    #[serde(default)]
    pub motion: MotionConfig,
}

/// Mado's embedded-vigy gate. Defaults the runtime OFF — operators
/// who want the in-process reconciler set `vigy.enabled = true`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MadoVigyConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Mado's post-effect configuration — one section per engawa
/// catalog effect (M3 Stream D). The render side derives its
/// enabled-effect set from THIS struct each frame; a disabled
/// effect contributes zero graph nodes. Effects compose in catalog
/// priority order (bloom → glow → snow → scanlines → crt →
/// colorblind), not declaration order.
///
/// `accessibility.reduce_motion` gates the animated effects
/// (`glow_on_bell`, `snow`) to zero nodes regardless of their
/// `enabled` knobs.
///
/// `PartialEq` (here + every per-effect struct below) is
/// load-bearing for hot-reload: `ux::config_apply::diff` compares
/// the resolved effects section value-wise so an unchanged section
/// emits zero `set_effects_config` calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MadoEffectsConfig {
    /// THE default-on composed layer (operator design law, 2026-06-13):
    /// ONE barely-perceptible ambience that combines the catalog
    /// effects at threshold intensities sharing one clock + the
    /// Vellum palette. `Matte` by default (effects recede to almost
    /// nothing — no glow, no halo, aurora off); `Whisper`/`Present` for
    /// the louder tiers; `Off` for the clean look. `reduce_motion`
    /// forces `Off`.
    ///
    /// The per-effect knobs below REMAIN for power users — an explicit
    /// `effects.aurora.enabled` / `effects.bloom.enabled` / … turns
    /// that effect on regardless of the preset, and explicit per-effect
    /// params override the composed defaults. `resolved_effects` folds
    /// the composition into the per-effect surface, override-aware.
    #[serde(default)]
    pub ambience: crate::ambience::AmbiencePreset,
    #[serde(default)]
    pub aurora: MadoAuroraConfig,
    #[serde(default)]
    pub snow: MadoSnowConfig,
    #[serde(default)]
    pub colorblind: MadoColorblindConfig,
    #[serde(default)]
    pub crt: MadoCrtConfig,
    #[serde(default)]
    pub scanlines: MadoScanlinesConfig,
    #[serde(default)]
    pub bloom: MadoBloomConfig,
    #[serde(default)]
    pub glow_on_bell: MadoGlowOnBellConfig,
    #[serde(default)]
    pub grain: MadoGrainConfig,
    /// Window-depth — the inner-edge vignette engawa catalog effect
    /// (a recessed "depth around the sides and edges" frame).
    #[serde(default)]
    pub window_depth: MadoWindowDepthConfig,
    /// Popup-elevation — the soft drop-shadow behind the centred Ctrl-S
    /// card. Overlay chrome (drawn outside the engawa post-graph via the
    /// rect pipeline), config-toggleable like the catalog effects so the
    /// window-depth + popup depth read as one consistent, switchable look.
    #[serde(default)]
    pub popup_elevation: MadoPopupElevationConfig,
}

/// Aurora (Vellum signature curtain) power-user override. The
/// ambience preset composes aurora at its threshold intensities; a
/// power user who sets `enabled = true` forces aurora on independent of
/// the preset, and the explicit dials below override the composed
/// values. Colors always flow from the resolved theme palette — no
/// hardcoded effect colors (the design law).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoAuroraConfig {
    /// Force aurora on regardless of the ambience preset. Default
    /// `false` — the preset is the default-on path.
    #[serde(default)]
    pub enabled: bool,
    /// Master opacity 0..1. Default mirrors the catalog's "sky
    /// dressing" gain (the scene reads through).
    #[serde(default = "default_aurora_intensity")]
    pub intensity: f32,
    /// Drift-speed multiplier over the slow base rate, 0..4.
    #[serde(default = "default_aurora_drift")]
    pub drift: f32,
    /// Shimmer amount 0..1.
    #[serde(default = "default_aurora_shimmer")]
    pub shimmer: f32,
    /// Horizon line (screen-space y, 0=top 1=bottom); the curtain is
    /// zero below it.
    #[serde(default = "default_aurora_horizon")]
    pub horizon: f32,
}

pub(crate) fn default_aurora_intensity() -> f32 { 0.35 }
pub(crate) fn default_aurora_drift() -> f32 { 1.0 }
pub(crate) fn default_aurora_shimmer() -> f32 { 0.5 }
pub(crate) fn default_aurora_horizon() -> f32 { 0.62 }

impl Default for MadoAuroraConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: default_aurora_intensity(),
            drift: default_aurora_drift(),
            shimmer: default_aurora_shimmer(),
            horizon: default_aurora_horizon(),
        }
    }
}

/// Colorblind-simulation effect knobs. `mode != None` IS the enable
/// (no separate boolean to drift out of sync). The legacy
/// `accessibility.colorblind` knob keeps working as a deprecation
/// alias: when this mode is `None`, the accessibility value wins —
/// resolved in [`MadoConfig::resolved_effects`], the single point
/// every renderer ingress (both entry points + hot-reload) flows
/// through.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MadoColorblindConfig {
    #[serde(default)]
    pub mode: ColorblindMode,
}

/// CRT-look effect knobs — defaults mirror the engawa catalog's
/// `CrtParams::default()` (the tuned reference values).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoCrtConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Barrel distortion strength (0 = flat; 0.05..0.15 typical).
    #[serde(default = "default_crt_curvature")]
    pub curvature: f32,
    /// Edge-darkening strength 0..=1.
    #[serde(default = "default_crt_vignette")]
    pub vignette: f32,
    /// Chromatic-aberration shift in pixels at the screen edge.
    #[serde(default = "default_crt_aberration")]
    pub aberration: f32,
}

fn default_crt_curvature() -> f32 { 0.08 }
fn default_crt_vignette() -> f32 { 0.25 }
fn default_crt_aberration() -> f32 { 0.6 }

impl Default for MadoCrtConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            curvature: default_crt_curvature(),
            vignette: default_crt_vignette(),
            aberration: default_crt_aberration(),
        }
    }
}

/// Scanlines effect knobs — defaults mirror the catalog's
/// `ScanlinesParams::default()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoScanlinesConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Scanline period in physical pixels (shader floor: 1.0).
    #[serde(default = "default_scanlines_period_px")]
    pub period_px: f32,
    /// Darkening strength 0..=1 (0 = exact pass-through).
    #[serde(default = "default_scanlines_intensity")]
    pub intensity: f32,
}

fn default_scanlines_period_px() -> f32 { 3.0 }
fn default_scanlines_intensity() -> f32 { 0.25 }

impl Default for MadoScanlinesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            period_px: default_scanlines_period_px(),
            intensity: default_scanlines_intensity(),
        }
    }
}

/// Bloom effect knobs — defaults mirror the catalog's
/// `BloomParams::default()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoBloomConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Luminance cutoff 0..=1 — below goes black.
    #[serde(default = "default_bloom_threshold")]
    pub threshold: f32,
    /// Additive gain of the blurred bright buffer.
    #[serde(default = "default_bloom_intensity")]
    pub intensity: f32,
    /// Blur tap spread in physical pixels.
    #[serde(default = "default_bloom_radius_px")]
    pub radius_px: f32,
}

fn default_bloom_threshold() -> f32 { 0.75 }
fn default_bloom_intensity() -> f32 { 0.6 }
fn default_bloom_radius_px() -> f32 { 2.5 }

impl Default for MadoBloomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_bloom_threshold(),
            intensity: default_bloom_intensity(),
            radius_px: default_bloom_radius_px(),
        }
    }
}

/// Glow-on-bell effect knobs — the BEL-driven cursor glow.
/// Gated to zero nodes by `accessibility.reduce_motion`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoGlowOnBellConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Gaussian sigma in physical pixels.
    #[serde(default = "default_glow_radius_px")]
    pub radius_px: f32,
}

fn default_glow_radius_px() -> f32 { 240.0 }

impl Default for MadoGlowOnBellConfig {
    fn default() -> Self {
        Self { enabled: false, radius_px: default_glow_radius_px() }
    }
}

/// Paper-grain ("tooth") knobs — the luma-only film grain that gives
/// the Vellum matte its faint fabric texture. The Matte ambience preset
/// composes grain at the barely-perceptible default opacity; a power
/// user who sets `enabled = true` forces it on regardless of the preset
/// and overrides the composed opacity. Defaults mirror the catalog's
/// `GrainParams::default()` (opacity 1.5 %).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoGrainConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Luma-jitter amplitude 0..=1 (0 = exact pass-through). Default
    /// 1.5 % — barely perceptible.
    #[serde(default = "default_grain_opacity")]
    pub opacity: f32,
}

fn default_grain_opacity() -> f32 { 0.015 }

impl Default for MadoGrainConfig {
    fn default() -> Self {
        Self { enabled: false, opacity: default_grain_opacity() }
    }
}

/// Window-depth knobs — the inner-edge vignette engawa catalog effect
/// that gives the whole surface a recessed "depth around the sides and
/// edges" frame. The edge tint is fed from the resolved theme (a deeper
/// shade of the background), so it tracks the theme; these dials shape
/// the geometry. Defaults mirror the catalog `WindowDepthParams`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoWindowDepthConfig {
    /// Force the vignette on. Default `false` — opt-in depth.
    #[serde(default)]
    pub enabled: bool,
    /// Reach inward as a fraction of the shorter dimension (0.08 = 8 %).
    #[serde(default = "default_window_depth_depth")]
    pub depth: f32,
    /// Max edge darkening 0..=1 (0 = exact pass-through). Default 22 %.
    #[serde(default = "default_window_depth_intensity")]
    pub intensity: f32,
    /// Falloff exponent; higher hugs the edge tighter. Default 1.6.
    #[serde(default = "default_window_depth_softness")]
    pub softness: f32,
}

fn default_window_depth_depth() -> f32 { 0.08 }
fn default_window_depth_intensity() -> f32 { 0.22 }
fn default_window_depth_softness() -> f32 { 1.6 }

impl Default for MadoWindowDepthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            depth: default_window_depth_depth(),
            intensity: default_window_depth_intensity(),
            softness: default_window_depth_softness(),
        }
    }
}

/// Popup-elevation knobs — the soft drop-shadow behind the centred
/// Ctrl-S session-switcher card. Unlike [`MadoWindowDepthConfig`] this is
/// overlay CHROME (the popup is drawn outside the engawa post-graph via
/// the rect pipeline), so it lives here as a toggle rather than a catalog
/// effect, but shares the same depth language so the window edges and the
/// card read as one consistent look. Defaults ON — the card floats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MadoPopupElevationConfig {
    /// Cast the soft shadow behind the centred popup card. Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for MadoPopupElevationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Snow overlay knobs. Mirrors the engawa catalog `SnowParams` but only
/// the operator-facing dials; runtime state (time, cursor,
/// typing_pulse, accumulation drift) is mado-managed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

// PRESCRIBED DEFAULT: snow OFF. The launch-perf A/B test (May
// 2026) confirmed the snow render pass adds measurable cold-start
// + per-frame work; the canonical pleme-io default is the clean
// no-effects look (matches blackmatter + stylix + nord-dark
// aesthetic of escriba / tear / frost / frostmourne). Operators
// who want snow opt in via `effects.snow.enabled = true` in
// mado.yaml — every snow param remains tuned + ready for them.
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
    /// Runtime single-pane re-attach. **Defaults ON.** When `true`
    /// (the default) mado polls a switch channel and can re-attach the
    /// displayed pane to a DIFFERENT live in-process tear session at
    /// runtime — same window, same renderer, fresh terminal — without
    /// tabs or splits. This is what makes the Ctrl-S session switcher
    /// actually switch; the `switch_session` MCP tool (forwarded via
    /// kanshou) posts switch requests. When `false`, mado binds its one
    /// GUI pane to ONE tear pane for the window's lifetime — the
    /// byte-identical legacy one-shot path — and the tool is a typed
    /// no-op (`switching-disabled`).
    ///
    /// Embedded runtime only for now: the switch targets a pane in
    /// the GUI's own `tear_core::InProcess`. Daemon-mode switching is
    /// a later phase (persistence across restarts). A config that omits
    /// this key (e.g. a partial mado.yaml) gets `true` via
    /// [`default_true`], so the switcher works out of the box fleet-wide.
    #[serde(default = "default_true")]
    pub session_switching: bool,
    /// Auto-attach-on-cd — the headline praça automation. When the
    /// *displayed* session's shell `cd`s into a DIFFERENT project, mado
    /// auto-switches its pane to that project's session (spawning +
    /// naming + binding it if none exists). **Defaults `Off`** — a `cd`
    /// never moves the pane until the operator opts in.
    ///
    /// Auto-attach drives the runtime switch channel, so any active
    /// mode (`AutoSwitch` / `Suggest`) additionally REQUIRES
    /// `session_switching = true`. When `auto_attach != Off` but
    /// `session_switching == false`, mado logs a one-time warning and
    /// behaves as `Off`. See [`AutoAttachMode`].
    #[serde(default)]
    pub auto_attach: AutoAttachMode,
    /// Where the Ctrl-S session picker overlay is anchored on screen.
    /// **Defaults [`PickerAnchor::Center`]** — a centred popup (the
    /// fzf/Telescope feel). `Bottom` rises from the bottom edge (Ctrl-R /
    /// Ctrl-T feel); `Top` drops from the top.
    #[serde(default)]
    pub session_picker_anchor: PickerAnchor,
    /// Whether the Ctrl-S picker surfaces LATENT presets (saved/authored
    /// `(defsession)` definitions with no live instance) as ○ Instantiate
    /// rows, interleaved with the live sessions. **Prescribed default
    /// `true`** — but the catalog is empty until a preset is saved, so the
    /// picker is unchanged until then. The bare tier sets `false`: the
    /// picker is strictly live-sessions-only (the stripped legacy flow),
    /// even if presets exist. A partial yaml gets `true` via [`default_true`].
    #[serde(default = "default_true")]
    pub session_picker_surface_presets: bool,
    /// How the picker badges live vs latent rows. **Prescribed default
    /// [`BadgeMode::Auto`]** — badges appear ONLY when the list actually
    /// mixes live + latent (so an all-live picker, the common case, stays
    /// byte-identical). `Off` never badges (stripped); `Always` badges
    /// every row (● live / ○ latent). The bare tier sets `Off`.
    #[serde(default)]
    pub session_picker_badges: BadgeMode,
}

/// How the Ctrl-S union picker badges live vs latent rows — a tiered knob
/// so the badge surface scales from stripped (`Off`) to always-on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BadgeMode {
    /// Never badge — rows show their bare name (the stripped look).
    Off,
    /// Badge ● live / ○ latent ONLY when the rendered list mixes both, so
    /// an all-live picker (the common case) is byte-identical to legacy.
    /// The prescribed default — minimal impact, maximal clarity when mixed.
    #[default]
    Auto,
    /// Always badge every row (● live / ○ latent), even an all-live list.
    Always,
}

/// The continuously-refreshing task-suggestion stream the Ctrl-S picker shades
/// in beneath the live + preset rows (see `crate::suggest`). Tiered: bare =
/// fully OFF (stripped — picker shows only sessions/presets); prescribed = ON
/// with a gentle cadence + every implemented source at its default. Per-source
/// overrides live in `sources` (keyed by kebab `SourceKind` slug).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuggestionsConfig {
    /// Master switch. `false` (bare) = no engine, no watchers, no rows.
    pub enabled: bool,
    /// Whether a source with no explicit `sources` override runs by default.
    pub default_enabled: bool,
    /// Max suggestion rows shown in the picker.
    pub max_visible: usize,
    /// Cap how many rows a single source may contribute to the visible band, so
    /// one noisy source (20 CrashLoop pods) can't drown your PRs/tickets. The
    /// band stays diverse. 0 = no cap.
    pub per_source_cap: usize,
    /// Fade-in duration (ms) for a newly-arrived suggestion row (the slow
    /// shade-in). 0 = appear instantly.
    pub shade_in_ms: u64,
    /// Global TTL FLOOR (seconds): a suggestion is dropped at least this long
    /// after it was last seen. NOT the whole story — each source's items live
    /// for `max(3× its poll interval, this floor)`, so a slow (hourly) source
    /// never flickers under a fast global TTL. `0` does **not** mean "never": it
    /// removes the floor, leaving each source's `3× poll interval` fallback in
    /// force — items still age out, just per-source.
    pub ttl_secs: u64,
    /// Lazily persist the cache to disk (atomic temp→rename) so a restart
    /// re-surfaces the last-known tasks instantly while the watchers re-poll.
    /// `~/.local/share/mado/suggestions.json` (override `MADO_SUGGEST_DB`).
    pub persist: bool,
    /// Coalesce disk writes: persist at most once per this many seconds, so the
    /// 27 parallel watchers can't thrash the disk. 0 = persist on every change.
    pub persist_debounce_secs: u64,
    /// Hard cap on total cached suggestions (memory insurance): if exceeded, the
    /// lowest-ranked / stalest are evicted. The store is already structurally
    /// bounded for well-behaved sources; this guards a source that stops polling
    /// with `ttl_secs = 0` or a mis-set `max_items`. 0 = unbounded.
    pub max_entries: usize,
    /// Per-source overrides, MERGED over the prescribed arm-list by kind slug
    /// (see [`SuggestionsConfig::effective_sources`]): an entry overrides that
    /// one kind (params, cadence, enabled) and never disarms the others. Kinds
    /// in neither list follow `default_enabled`.
    pub sources: Vec<SuggestionSourceConfig>,
    /// Escape hatch: `true` makes `sources` REPLACE the prescribed arm-list
    /// entirely instead of merging over it — an explicit allow-list for an
    /// operator who wants exactly-these-sources and nothing else.
    pub sources_replace: bool,
    /// Band rows guaranteed INSIDE the picker's render window on the empty
    /// query: with many live sessions the band is inserted above the fold
    /// instead of appended below it (displaced session rows stay scrollable).
    /// 0 = plain append (the band can fall below the fold).
    pub reserved_rows: usize,
    /// Ambient signal while the board is closed: a NEW Critical suggestion
    /// bounces the dock / flashes the taskbar once per issue (the platform
    /// attention request), so an incident reaches the operator without the
    /// picker being open. Warm-restart rows never alert.
    pub attention_on_critical: bool,
}

impl Default for SuggestionsConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl SuggestionsConfig {
    /// Bare tier — the whole stream off (stripped).
    #[must_use]
    pub fn bare() -> Self {
        Self {
            enabled: false,
            default_enabled: false,
            max_visible: 0,
            per_source_cap: 0,
            shade_in_ms: 0,
            ttl_secs: 0,
            persist: false,
            persist_debounce_secs: 0,
            max_entries: 0,
            sources: Vec::new(),
            sources_replace: false,
            reserved_rows: 0,
            attention_on_critical: false,
        }
    }

    /// Prescribed tier — the stream is ON with the operator's **full workflow
    /// surface armed**: every source except the three steady-cost external
    /// pollers (`AwsHealth` / `DatadogMonitors` / `CloudflareDeployments`,
    /// which stay opt-in to avoid recurring API spend). Every source degrades
    /// gracefully — a missing credential/param/cluster yields no rows and a
    /// typed `Unavailable` health state, never an error — so the band fills
    /// from whatever is actually reachable. `default_enabled = false` remains
    /// the gate for kinds NOT in this list: a future source never silently
    /// starts making network calls just by existing.
    ///
    /// A yaml `sources` list is a per-kind OVERRIDE merged over this arm-list
    /// (see [`SuggestionsConfig::effective_sources`]) — supplying params for
    /// one source never disarms the rest.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            enabled: true,
            // OFF by default — see the doc above. Only the explicitly-listed
            // sources below run.
            default_enabled: false,
            max_visible: 6,
            per_source_cap: 3,
            shade_in_ms: 600,
            ttl_secs: 900,
            persist: true,
            persist_debounce_secs: 5,
            max_entries: 200,
            sources: vec![
                // Every source degrades gracefully — a missing dep/cred/cluster
                // yields an empty Vec, never an error or a blocking call (see
                // suggest/source.rs contract). So the prescribed default arms the
                // operator's full workflow surface: the band fills from whatever
                // is actually reachable, live-streamed into the Ctrl-S board. The
                // three external-cost pollers (AwsHealth / DatadogMonitors /
                // CloudflareDeployments) stay opt-in to avoid steady API spend.
                //
                // Zero-network local tier — works for ANY download, no creds.
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::RecentDirs),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::ProjectMarks),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GitBranchPr),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::TendRepos),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::CargoWarnings),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::TodoBacklog),
                // GitHub (operator token).
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GithubReviewRequested),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GithubAssignedIssues),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GithubActionsFailing),
                // Jira + Confluence.
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::JiraSprint),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::JiraAssigned),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::ConfluenceMentions),
                // Fleet cluster + ops surface.
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::FluxFailing),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::K8sUnhealthy),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::BreatheConflict),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::EngenhoNodes),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GrafanaAlerts),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GrafanaIncidents),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GrafanaOncall),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::OpsgenieAlerts),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::KurageAgents),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::SecretAge),
                // Personal cadence.
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GoogleTasks),
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::GoogleCalendar),
                // Curated observability (armed; typed "needs config" until the
                // safra: section declares cells).
                SuggestionSourceConfig::enable(crate::suggest::SourceKind::Safra),
            ],
            sources_replace: false,
            reserved_rows: 3,
            attention_on_critical: true,
        }
    }

    /// The EFFECTIVE per-source override list the engine runs from: the
    /// prescribed arm-list with the operator's `sources` entries merged over it
    /// by kind slug (an operator entry wins wholesale for its kind; unknown
    /// slugs ride along and are ignored downstream). This is the load-bearing
    /// fix for the "a params-only yaml override disarmed 22 sources" failure:
    /// serde replaces a yaml `Vec` outright, so the merge has to happen here,
    /// after deserialize. `sources_replace = true` restores replace semantics
    /// as an explicit allow-list.
    #[must_use]
    pub fn effective_sources(&self) -> Vec<SuggestionSourceConfig> {
        if self.sources_replace {
            return self.sources.clone();
        }
        let mut merged = Self::prescribed().sources;
        for over in &self.sources {
            match merged.iter_mut().find(|m| m.kind == over.kind) {
                Some(slot) => *slot = over.clone(),
                None => merged.push(over.clone()),
            }
        }
        merged
    }
}

/// The reactive session-janitor plane (see `crate::janitors` +
/// `docs/JANITORS.md`): typed invariant-holders riding the suggest engine
/// thread's maintenance tick, publishing findings on the `crate::fibers`
/// bus and (optionally) onto the Ctrl-S board. Tiered: bare = fully OFF;
/// prescribed = ON **shadow-first** (findings surface, remediation held —
/// the operator flips `authority: effect` per janitor or globally).
///
/// Hot-reload: this section rides the same `EngineCommand::Swap` path as
/// `suggestions`/`safra` — a config edit rebuilds the janitor runner live,
/// no restart needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JanitorsConfig {
    /// Master switch. `false` (bare) = no janitors, no findings, no rows.
    pub enabled: bool,
    /// Global remediation authority — the shadow-first default every
    /// janitor inherits unless it carries its own override.
    pub authority: crate::janitors::Authority,
    /// Project findings onto the Ctrl-S board as agent-lane rows (the
    /// `suggest_inject` path; stable keys ⇒ one living row per finding).
    pub board_rows: bool,
    /// The ghost-session sweeper (embedded tear registry).
    pub ghost_session: GhostSessionJanitorConfig,
    /// The suggestion-source health watcher (izumi store).
    pub suggest_health: SuggestHealthJanitorConfig,
}

impl Default for JanitorsConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl JanitorsConfig {
    /// Bare tier — the whole plane off (stripped).
    #[must_use]
    pub fn bare() -> Self {
        Self {
            enabled: false,
            authority: crate::janitors::Authority::Shadow,
            board_rows: false,
            ghost_session: GhostSessionJanitorConfig {
                enabled: false,
                interval_secs: 0,
                grace_secs: 0,
                authority: None,
            },
            suggest_health: SuggestHealthJanitorConfig {
                enabled: false,
                interval_secs: 0,
                min_consecutive_polls: 0,
                authority: None,
            },
        }
    }

    /// Prescribed tier — both janitors armed, **shadow-first**: every
    /// finding publishes + surfaces, no remediation runs until the
    /// operator explicitly flips an `authority` knob to `effect`.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            enabled: true,
            authority: crate::janitors::Authority::Shadow,
            board_rows: true,
            ghost_session: GhostSessionJanitorConfig {
                enabled: true,
                // Sweep once a minute; tolerate 3 minutes of ghost-hood
                // (an agent may legitimately leave an exited session
                // briefly before re-attaching or closing it).
                interval_secs: 60,
                grace_secs: 180,
                authority: None,
            },
            suggest_health: SuggestHealthJanitorConfig {
                enabled: true,
                // Health moves at poll cadence (30s–1h per source);
                // checking every 2 minutes is plenty.
                interval_secs: 120,
                min_consecutive_polls: 3,
                authority: None,
            },
        }
    }
}

/// Per-janitor knobs for the ghost-session sweeper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GhostSessionJanitorConfig {
    /// Run this janitor at all.
    pub enabled: bool,
    /// Observation cadence (seconds, floored at 1 by the runner).
    pub interval_secs: u64,
    /// How long a session must HOLD the ghost predicate (agent-owned +
    /// fully exited + zero subscribers) before it is reported/closed.
    pub grace_secs: u64,
    /// Per-janitor authority override; `None` inherits the global.
    pub authority: Option<crate::janitors::Authority>,
}

impl Default for GhostSessionJanitorConfig {
    fn default() -> Self {
        JanitorsConfig::prescribed().ghost_session
    }
}

/// Per-janitor knobs for the suggestion-source health watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuggestHealthJanitorConfig {
    /// Run this janitor at all.
    pub enabled: bool,
    /// Observation cadence (seconds, floored at 1 by the runner).
    pub interval_secs: u64,
    /// Consecutive COMPLETED bad polls (`Error`/`AuthMissing`) before a
    /// source is reported (floored at 1 by the janitor).
    pub min_consecutive_polls: u32,
    /// Per-janitor authority override; `None` inherits the global.
    pub authority: Option<crate::janitors::Authority>,
}

impl Default for SuggestHealthJanitorConfig {
    fn default() -> Self {
        JanitorsConfig::prescribed().suggest_health
    }
}

/// Clickable, highlighted links. Both OSC 8 hyperlinks (cells carrying a
/// `link_id`) and auto-detected bare URLs are highlighted in the theme's
/// frost accent + underlined, show a pointer/hand cursor on hover, and
/// open on a plain (no-drag) click — a URL through the OS opener, a
/// `file://path:line` through the operator's `$VISUAL`/`$EDITOR`. Tiered:
/// bare = fully OFF; prescribed = every knob ON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MadoLinksConfig {
    /// Master switch. `false` (bare) strips every link affordance.
    pub enabled: bool,
    /// Highlight links: frost-blue text + the underline decoration.
    pub highlight: bool,
    /// A plain (no-drag) left click on a link opens it.
    pub open_on_click: bool,
    /// Hovering a link shows a pointer/hand cursor.
    pub pointer_cursor: bool,
}

impl Default for MadoLinksConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl MadoLinksConfig {
    /// Bare tier — every link affordance off.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            enabled: false,
            highlight: false,
            open_on_click: false,
            pointer_cursor: false,
        }
    }

    /// Prescribed tier — highlight + hover cursor + click-to-open all on.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            enabled: true,
            highlight: true,
            open_on_click: true,
            pointer_cursor: true,
        }
    }
}

/// Tasteful-feedback knobs — small visual acknowledgements of
/// otherwise-invisible actions. `bare()` = every flourish off;
/// `prescribed()` = every flourish on (the fleet default).
///
/// `copy_flash` + `exit_code_coloring` are forward gates: the typed
/// surface lands now (so operators can opt out from day one), the
/// render wiring follows once the copy-path signal + per-block exit
/// status reach the renderer.
/// `display.*` — how mado adapts its cell grid to the physical display.
///
/// On a macOS "scaled" ("More Space") mode (and X11 RandR `--scale`) the OS
/// compositor DOWNSCALES mado's framebuffer to a smaller physical panel at a
/// non-integer ratio, which smears text-row boundaries into thin horizontal
/// seams — an artifact mado can't fix in its own (pixel-perfect) framebuffer.
/// Seam auto-tune discovers that panel-vs-framebuffer ratio and snaps the cell
/// height so every row lands on a whole number of PANEL pixels, so the
/// downscale has no periodic sub-pixel row structure to amplify.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    /// Discover the display downscale ratio + snap the cell grid onto integer
    /// panel pixels (kills the scaled-display row seam). Default on; off pins
    /// the ratio to 1.0 (byte-identical to no adjustment).
    pub seam_auto_tune: bool,
    /// Pin the panel-vs-framebuffer downscale ratio instead of auto-detecting
    /// (e.g. `0.8405`). `None` = auto-discover. Useful on a platform where the
    /// probe can't read the geometry, or to force a specific value.
    pub downscale_ratio: Option<f32>,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            seam_auto_tune: true,
            downscale_ratio: None,
        }
    }
}

impl DisplayConfig {
    /// The bare tier — everything-off contract: seam auto-tune disabled.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            seam_auto_tune: false,
            downscale_ratio: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedbackConfig {
    /// A short selection-overlay flash when `copy_on_select` copies.
    pub copy_flash: bool,
    /// The full-window visual bell flash on BEL (decays ~200ms).
    pub visual_bell: bool,
    /// Tint OSC 133 command-block separators by exit status
    /// (green on 0 / red on non-zero).
    pub exit_code_coloring: bool,
    /// A brief green (exit 0) / red (non-zero) cursor glow when a command
    /// completes — peripheral success/fail feedback (OSC 133 `D`).
    pub exit_code_glow: bool,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl FeedbackConfig {
    /// Bare tier — every feedback flourish off.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            copy_flash: false,
            visual_bell: false,
            exit_code_coloring: false,
            exit_code_glow: false,
        }
    }

    /// Prescribed tier — every feedback flourish on.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            copy_flash: true,
            visual_bell: true,
            exit_code_coloring: true,
            exit_code_glow: true,
        }
    }
}

/// Which desktop-notification backend mado uses. See
/// [`crate::platform::notification_dispatcher`] + `docs/NOTIFICATIONS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyBackend {
    /// Native `UNUserNotificationCenter` when bundled (`Mado.app`), else a
    /// silent log (dock attention still fires). **No Script-Editor
    /// popup.** The default.
    #[default]
    Auto,
    /// Force the native backend; falls back to log when unbundled.
    Native,
    /// The legacy `osascript` path — attributed to *Script Editor* and
    /// tripping the automation popup. Opt-in only (the one way to get a
    /// banner from an unbundled CLI mado).
    Osascript,
    /// Never raise an OS banner — log only (dock attention still fires).
    Log,
}

/// Focus policy: when a notification is actually delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyWhen {
    /// Deliver regardless of focus.
    Always,
    /// Deliver only when mado is **not** the focused window — the
    /// standard terminal UX. The default.
    #[default]
    Unfocused,
}

/// Which sound the audible bell plays. `Beep` is the classic system
/// alert (`NSBeep`); the rest are named macOS system sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BellSound {
    /// The classic system alert beep (`NSBeep`). The default.
    #[default]
    Beep,
    /// Named macOS system sounds.
    Basso,
    /// A short high ping.
    Ping,
    /// A soft pop.
    Pop,
    /// A glassy chime.
    Glass,
    /// A submarine sonar ping.
    Submarine,
    /// A light tink.
    Tink,
    /// A funk tone.
    Funk,
    /// A hero fanfare.
    Hero,
    /// The classic "Sosumi".
    Sosumi,
}

impl BellSound {
    /// The `NSSound` name, or `None` for the plain system beep.
    #[must_use]
    pub fn sound_name(self) -> Option<&'static str> {
        match self {
            BellSound::Beep => None,
            BellSound::Basso => Some("Basso"),
            BellSound::Ping => Some("Ping"),
            BellSound::Pop => Some("Pop"),
            BellSound::Glass => Some("Glass"),
            BellSound::Submarine => Some("Submarine"),
            BellSound::Tink => Some("Tink"),
            BellSound::Funk => Some("Funk"),
            BellSound::Hero => Some("Hero"),
            BellSound::Sosumi => Some("Sosumi"),
        }
    }
}

/// BEL behaviour: the *audible* bell (native system sound) + the optional
/// desktop-notification on BEL. (The *visual* bell flash lives in
/// [`FeedbackConfig::visual_bell`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BellNotifyConfig {
    /// Play the native audible bell on BEL. Off by default — bells are
    /// frequent, so audio is opt-in (the visual flash always fires).
    pub audible: bool,
    /// Which sound the audible bell plays.
    pub sound: BellSound,
    /// Raise a desktop notification on BEL. Off by default.
    pub notify: bool,
    /// Urgency for bell notifications, if enabled.
    pub urgency: NotifyUrgency,
}

impl Default for BellNotifyConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl BellNotifyConfig {
    /// Bare tier — no bell audio, no bell notifications.
    #[must_use]
    pub fn bare() -> Self {
        Self { audible: false, sound: BellSound::Beep, notify: false, urgency: NotifyUrgency::Normal }
    }

    /// Prescribed tier — bell audio + notifications off (the visual bell
    /// suffices; audio/banner are opt-in because bells are frequent).
    #[must_use]
    pub fn prescribed() -> Self {
        Self { audible: false, sound: BellSound::Beep, notify: false, urgency: NotifyUrgency::Normal }
    }
}

/// Urgency level, mirrored from [`tsuuchi::Urgency`] for the config
/// surface (config must not depend on the exact tsuuchi type layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyUrgency {
    /// Informational (macOS passive interruption level).
    Low,
    /// Standard (macOS active). The default.
    #[default]
    Normal,
    /// Time-sensitive (pierces Focus).
    Critical,
}

impl From<NotifyUrgency> for tsuuchi::Urgency {
    fn from(u: NotifyUrgency) -> Self {
        match u {
            NotifyUrgency::Low => tsuuchi::Urgency::Low,
            NotifyUrgency::Normal => tsuuchi::Urgency::Normal,
            NotifyUrgency::Critical => tsuuchi::Urgency::Critical,
        }
    }
}

/// Long-command-completion notification policy — the "✓ `cargo build`
/// finished in 2m 14s" banner when a slow command completes while mado is
/// unfocused. Keyed off OSC 133 shell-integration prompt marks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandCompletionConfig {
    /// Master switch for command-completion notifications.
    pub enabled: bool,
    /// Only notify for commands that ran at least this long (ms).
    pub min_duration_ms: u64,
    /// Notify when the command exited 0.
    pub notify_on_success: bool,
    /// Notify when the command exited non-zero.
    pub notify_on_failure: bool,
    /// Only notify when mado is not the focused window.
    pub only_when_unfocused: bool,
    /// Command basenames that never notify (interactive/full-screen tools
    /// whose completion is not interesting). Matched against argv[0]'s
    /// basename.
    pub deny: Vec<String>,
    /// Skip the notification when the command entered the alternate screen
    /// (a TUI: vim/less/lazygit/btop) — you just quit an editor, not
    /// finished a batch job. The robust, text-free filter.
    pub respect_alt_screen: bool,
}

impl Default for CommandCompletionConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl CommandCompletionConfig {
    /// Bare tier — no command-completion notifications.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            enabled: false,
            min_duration_ms: 10_000,
            notify_on_success: true,
            notify_on_failure: true,
            only_when_unfocused: true,
            deny: Self::default_deny(),
            respect_alt_screen: true,
        }
    }

    /// Prescribed tier — notify on slow (≥10s) command completion while
    /// unfocused, success or failure, excluding interactive tools.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            enabled: true,
            min_duration_ms: 10_000,
            notify_on_success: true,
            notify_on_failure: true,
            only_when_unfocused: true,
            deny: Self::default_deny(),
            respect_alt_screen: true,
        }
    }

    /// Interactive / full-screen tools whose "completion" is just the
    /// operator quitting — never interesting as a banner.
    fn default_deny() -> Vec<String> {
        [
            "vim", "nvim", "vi", "emacs", "nano", "less", "more", "man", "top", "htop", "btop",
            "watch", "ssh", "tmux", "screen", "fzf", "tig", "lazygit", "bat", "vise", "mado",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    /// Whether a completed command should raise a notification, given the
    /// completion signal + current focus. The pure, testable decision
    /// core: enabled ∧ not-a-TUI ∧ slow-enough ∧ outcome-wanted ∧ away.
    #[must_use]
    pub fn should_notify(&self, c: &crate::ux::CommandCompletion, focused: bool) -> bool {
        self.enabled
            && !(self.respect_alt_screen && c.used_alt_screen)
            && c.duration_ms >= self.min_duration_ms
            && (if c.succeeded() { self.notify_on_success } else { self.notify_on_failure })
            && !(self.only_when_unfocused && focused)
    }
}

/// Toggles for the terminal desktop-notification escape protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OscNotifyConfig {
    /// OSC 9 (iTerm2 simple notification).
    pub osc9: bool,
    /// OSC 777 (urxvt/foot `notify`).
    pub osc777: bool,
    /// OSC 99 (kitty rich desktop-notification protocol).
    pub osc99: bool,
}

impl Default for OscNotifyConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl OscNotifyConfig {
    /// Bare tier — all OSC notification protocols off.
    #[must_use]
    pub fn bare() -> Self {
        Self { osc9: false, osc777: false, osc99: false }
    }

    /// Prescribed tier — all OSC notification protocols on.
    #[must_use]
    pub fn prescribed() -> Self {
        Self { osc9: true, osc777: true, osc99: true }
    }
}

/// The desktop-notification system config. Governs the backend, focus
/// policy, coalescing/rate-limiting, history, and every notification
/// source. See `docs/NOTIFICATIONS.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Master switch. When `false`, no desktop notification is ever
    /// raised (the visual/audible bell + dock attention still work).
    pub enabled: bool,
    /// Which backend delivers banners.
    pub backend: NotifyBackend,
    /// Global focus policy (a source may tighten it, never loosen it).
    pub when: NotifyWhen,
    /// Max notifications delivered per rolling minute (0 = unlimited).
    /// A storm beyond this is dropped (and traced), never queued forever.
    pub rate_limit_per_min: u32,
    /// Collapse a repeat of the same (title, group) within this window
    /// into the earlier one (0 = no coalescing).
    pub coalesce_window_ms: u64,
    /// How many delivered notifications to retain in history (for the MCP
    /// `notifications_list` surface).
    pub history_capacity: usize,
    /// BEL → notification policy.
    pub bell: BellNotifyConfig,
    /// Long-command-completion notification policy.
    pub command_completion: CommandCompletionConfig,
    /// Terminal notification-escape protocol toggles.
    pub osc: OscNotifyConfig,
    /// Badge the dock icon with the count of notifications delivered
    /// while mado was unfocused; cleared when the window regains focus.
    pub badge_unread: bool,
    /// Show OSC 9;4 (ConEmu) command progress in the dock badge (e.g.
    /// `45%`), taking precedence over the unread count while active.
    pub progress_dock: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl NotificationsConfig {
    /// Bare tier — the whole subsystem off.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            enabled: false,
            backend: NotifyBackend::Auto,
            when: NotifyWhen::Unfocused,
            rate_limit_per_min: 30,
            coalesce_window_ms: 800,
            history_capacity: 50,
            bell: BellNotifyConfig::bare(),
            command_completion: CommandCompletionConfig::bare(),
            osc: OscNotifyConfig::bare(),
            badge_unread: false,
            progress_dock: false,
        }
    }

    /// Prescribed tier — native backend (no popup), focus-aware,
    /// command-completion + all OSC protocols on.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            enabled: true,
            backend: NotifyBackend::Auto,
            when: NotifyWhen::Unfocused,
            rate_limit_per_min: 30,
            coalesce_window_ms: 800,
            history_capacity: 50,
            bell: BellNotifyConfig::prescribed(),
            command_completion: CommandCompletionConfig::prescribed(),
            osc: OscNotifyConfig::prescribed(),
            badge_unread: true,
            progress_dock: true,
        }
    }
}

/// Motion-easing knobs — animations ease instead of popping.
/// `bare()` = every easing off (instant/hard); `prescribed()` = every
/// easing on (the fleet default).
///
/// `blink_ease` + `picker_animate` + `scroll_lerp` are forward gates:
/// the typed surface lands now; their render wiring follows. Only
/// `unfocused_dim` is wired in this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MotionConfig {
    /// Ease the cursor blink alpha (smoothstep edges) instead of a hard
    /// on/off pop.
    pub blink_ease: bool,
    /// Fade + scale the Ctrl-S picker overlay in when it opens.
    pub picker_animate: bool,
    /// Lerp the rendered scroll offset toward its target each frame.
    pub scroll_lerp: bool,
    /// Whisper-dim an unfocused window so a backgrounded window reads as
    /// backgrounded.
    pub unfocused_dim: bool,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self::prescribed()
    }
}

impl MotionConfig {
    /// Bare tier — every easing off (instant, hard transitions).
    #[must_use]
    pub fn bare() -> Self {
        Self {
            blink_ease: false,
            picker_animate: false,
            scroll_lerp: false,
            unfocused_dim: false,
        }
    }

    /// Prescribed tier — every easing on.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            blink_ease: true,
            picker_animate: true,
            scroll_lerp: true,
            unfocused_dim: true,
        }
    }
}

/// Per-source override in [`SuggestionsConfig::sources`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionSourceConfig {
    /// Source kind kebab slug (e.g. `git-branch-pr`). Unknown slugs ignored.
    pub kind: String,
    /// Run this source. Defaults `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the poll cadence (seconds).
    #[serde(default)]
    pub interval_secs: Option<u64>,
    /// Override the per-poll item cap.
    #[serde(default)]
    pub max_items: Option<usize>,
    /// Free per-source params (token env override, JQL, grafana folder, …).
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
}

impl SuggestionSourceConfig {
    /// An override that simply enables a source at its default cadence/params —
    /// the typed way to opt a `SourceKind` into the stream (slug stays in sync
    /// with the enum via [`crate::suggest::SourceKind::slug`]).
    #[must_use]
    pub fn enable(kind: crate::suggest::SourceKind) -> Self {
        Self {
            kind: kind.slug().to_string(),
            enabled: true,
            interval_secs: None,
            max_items: None,
            params: std::collections::BTreeMap::new(),
        }
    }
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
            session_switching: true,
            auto_attach: AutoAttachMode::default(),
            session_picker_anchor: PickerAnchor::default(),
            // Prescribed: surface presets (empty until saved) + auto-badge
            // (only when the list mixes live + latent → minimal impact).
            session_picker_surface_presets: true,
            session_picker_badges: BadgeMode::Auto,
        }
    }
}

/// Where a floating picker overlay (Ctrl-S session picker) is anchored
/// on screen. The operator's stated preference (2026-06-21): the session
/// switcher floats in the **center** as a popup — so
/// [`Center`](PickerAnchor::Center) is the default; `Bottom` (Ctrl-R /
/// Ctrl-T feel) and `Top` remain available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PickerAnchor {
    /// Float the picker in the CENTER of the window as a popup, with a
    /// backing panel — the fzf/Telescope feel. **Default** for the Ctrl-S
    /// session switcher (operator request 2026-06-21: "the entire session
    /// switcher in the center of the screen as a popup").
    #[default]
    Center,
    /// Pin the picker to the bottom of the window — it grows upward from
    /// near the bottom edge. Matches the Ctrl-R/Ctrl-T feel.
    Bottom,
    /// Pin the picker to the top of the window — the legacy drop-from-top
    /// behavior, kept for operators who prefer it.
    Top,
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
    /// spawn, ghostty-class latency). **The default** — every fresh
    /// `mado` invocation lands here without operator action. Per
    /// the maestro mado-default.yaml stack spec; single-window is
    /// the 90% case and shouldn't pay the IPC tax.
    #[default]
    Embedded,
    /// Over Unix socket via `tear_client::Client`. Multi-attach
    /// safe — required when ayatsuri overlay + namimado debug +
    /// remote ssh-mux need to share the same session. Operator
    /// opts in via `mado.tear.runtime = "daemon"` (or the maestro
    /// mado-shared.yaml stack spec).
    Daemon,
}

/// How mado reacts when the *displayed* session's shell `cd`s into a
/// **different** project than the one it's currently seated at — the
/// headline praça automation. See [`MadoTearConfig::auto_attach`].
///
/// Maps one-to-one onto [`praca::AttachPolicy`]:
///   * `Off` → `PickerOnly` (praca always decides `Stay`) — today's
///     behaviour: a `cd` never moves the pane. **The default.**
///   * `AutoSwitch` → `AutoSwitch` — a cross-project `cd` switches the
///     displayed pane to that project's session (spawning + naming +
///     binding it if none exists yet).
///   * `Suggest` → `SuggestOnly` — praca computes the same decision but
///     wraps it; mado surfaces it (status-line hint / log) and never
///     moves the pane behind the operator's back.
///
/// Auto-attach drives the runtime switch channel, so it additionally
/// REQUIRES `session_switching = true`. When `auto_attach != Off` but
/// `session_switching == false`, mado logs a one-time warning and
/// behaves as `Off` (the switch channel has no drainer to post into).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoAttachMode {
    /// No auto-attach. A displayed-pane `cd` never moves the pane —
    /// byte-identical to the pre-praça behaviour. **The default.**
    #[default]
    Off,
    /// A cross-project `cd` on the displayed pane auto-switches mado to
    /// that project's session, spawning + naming + binding a fresh one
    /// when the project has no session yet.
    AutoSwitch,
    /// Compute the same switch/spawn decision but only surface it (hint
    /// / log); never move the pane automatically.
    Suggest,
}

impl AutoAttachMode {
    /// The [`praca::AttachPolicy`] this mode maps to. `Off` maps to
    /// `PickerOnly` so praca's own decision is always `Stay` — the
    /// automation is off at the engine level, not just gated at the
    /// call site.
    #[must_use]
    pub fn policy(self) -> praca::AttachPolicy {
        match self {
            AutoAttachMode::Off => praca::AttachPolicy::PickerOnly,
            AutoAttachMode::AutoSwitch => praca::AttachPolicy::AutoSwitch,
            AutoAttachMode::Suggest => praca::AttachPolicy::SuggestOnly,
        }
    }

    /// Whether this mode does anything at all. `Off` is the no-op
    /// default; the event loop skips constructing the auto-attach
    /// driver entirely when this is false.
    #[must_use]
    pub fn is_active(self) -> bool {
        !matches!(self, AutoAttachMode::Off)
    }
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
    /// Double-click word-snap BOUNDARY set. Non-empty REPLACES the
    /// default rule (any non-alphanumeric/underscore is a boundary):
    /// listed characters split words, every other character is
    /// word-interior. Empty = the default rule.
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
    ///   Combined with `platform::apply_native_styling()`'s themed
    ///   transparent titlebar band ([`TitlebarStyle::Flush`];
    ///   `FullSizeContentView` only with `titlebar: overlay`), the
    ///   chrome reads as part of the canvas for a "minimal but
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
    /// macOS-specific window-chrome knobs — native window tabbing,
    /// titlebar integration, and forced appearance. Defaults bias to
    /// "just the terminal" (no OS tab strip, flush dark titlebar).
    /// Ignored on Linux/Windows. Authored under `window.macos.*` in
    /// `~/.config/mado/mado.yaml`.
    #[serde(default)]
    pub macos: MacosWindowConfig,
}

/// macOS window-chrome configuration. Every axis defaults to the
/// minimal "just the terminal" look but is operator-overridable via
/// shikumi YAML, exactly like every other `MadoConfig` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacosWindowConfig {
    /// macOS-native window tabbing: the `⌘1 / ⌘2 …` tab strip plus the
    /// `+` new-tab button that render as a grey band under the titlebar.
    /// Default `false` — mado owns sessions, panes, and windows through
    /// its integrated `tear` runtime, so the OS tab strip is redundant
    /// chrome. Set `true` to restore the native macOS tab bar.
    #[serde(default)]
    pub native_tabs: bool,
    /// How the titlebar integrates with the cell grid.
    #[serde(default)]
    pub titlebar: TitlebarStyle,
    /// Which `NSAppearance` the window is forced into.
    #[serde(default)]
    pub appearance: WindowAppearance,
}

impl Default for MacosWindowConfig {
    fn default() -> Self {
        Self {
            native_tabs: false,
            titlebar: TitlebarStyle::Flush,
            appearance: WindowAppearance::Dark,
        }
    }
}

/// Titlebar integration style on macOS.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TitlebarStyle {
    /// Keep the titled band but theme it into the canvas: transparent
    /// titlebar + hidden title + no hairline separator over a window
    /// backing tinted to the configured terminal background. The band
    /// reads as part of the terminal, the traffic-light buttons sit in
    /// their own strip, and the cell grid starts BELOW them — text is
    /// never overlapped (ghostty's `transparent` look). The default.
    #[default]
    Flush,
    /// `FullSizeContentView`: the cell grid runs flush to the window's
    /// top edge and the traffic-light buttons float OVER the first
    /// row of text. Maximum canvas; the top row sits under the
    /// buttons. Same transparent-titlebar + background tint as Flush.
    Overlay,
    /// Leave the stock macOS titlebar untouched — opaque band, hairline
    /// separator, visible title. For operators who want a conventional
    /// Mac window frame.
    Native,
}

/// Forced `NSAppearance` for the window.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowAppearance {
    /// Force the dark appearance so the titlebar material and the
    /// traffic-light glyphs render dark and blend with a dark (Nord)
    /// background. The default.
    #[default]
    Dark,
    /// Force the light appearance.
    Light,
    /// Follow the system appearance setting (no override).
    Auto,
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
    /// When `copy_on_select` auto-copies a highlight on mouse release,
    /// ALSO clear the highlight — so lifting the mouse both copies AND
    /// unhighlights and no click is ever needed to copy. `false` keeps the
    /// highlight live after the auto-copy (the pre-2026-07-06 behavior,
    /// still fully available). Inert when `copy_on_select` is off (nothing
    /// is auto-copied, so nothing is auto-cleared). Default on.
    #[serde(default = "default_deselect_on_copy")]
    pub deselect_on_copy: bool,
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
    /// M2 — rewrap the primary grid's logical lines when the window
    /// is resized to a different column count (kitty/ghostty
    /// behavior). `false` restores the legacy truncate/extend
    /// semantics. The alternate screen always truncates regardless —
    /// full-screen TUIs redraw themselves on SIGWINCH.
    #[serde(default = "default_true")]
    pub reflow_on_resize: bool,
    /// Momentum scrolling: a wheel/two-finger flick injects velocity
    /// that DECELERATES naturally to a stop, instead of a 1:1 instant
    /// jump — a weighty, gravity-like feel. `false` restores the
    /// direct line-for-line scroll (the behavior-preserving opt-out).
    #[serde(default = "default_true")]
    pub scroll_momentum: bool,
    /// Exponential friction (per second) that bleeds off scroll
    /// velocity. The glide's velocity halves every `ln2/friction ≈
    /// 0.23s` at the tuned default — a flick visibly coasts for ~0.7s
    /// then eases to a crisp stop: not an instant jump, not a
    /// sluggish-forever crawl. Larger = stops sooner; smaller = glides
    /// longer.
    #[serde(default = "default_scroll_friction")]
    pub scroll_friction: f32,
    /// Velocity cap in lines/sec. Bounds how fast even a frantic
    /// repeated flick can scroll, so momentum can't launch to the
    /// scrollback top in a single frame. ~200 lines/sec ≈ three+ screens
    /// per second at peak — fast, but always trackable by the eye.
    #[serde(default = "default_scroll_max_velocity")]
    pub scroll_max_velocity: f32,
    /// Selection auto-scroll: while dragging a text selection, if the
    /// pointer goes above the top edge or below the bottom edge the
    /// viewport scrolls in that direction and the highlight extends to
    /// the newly revealed lines (so a drag can select more than one
    /// screen). `false` freezes selection at the viewport edges.
    #[serde(default = "default_true")]
    pub selection_autoscroll: bool,
    /// Precise (trackpad / Magic Mouse) scroll behavior — see
    /// [`PreciseScrollMode`]. `Pixels` (default) is the ghostty-faithful
    /// pixel accumulator with OS-supplied inertia; `Momentum` routes the
    /// trackpad into the synthetic glide instead. Selects the precise arm of
    /// the scroll system (`ux::scroll`).
    #[serde(default)]
    pub precise_scroll_mode: PreciseScrollMode,
    /// Precise-scroll pixel gain. Physical pixel deltas are multiplied by
    /// this before the cell accumulator. `1.0` is true 1:1 finger tracking;
    /// the default `2.0` matches ghostty's effective macOS trackpad feel
    /// (its apprt's hard-coded 2× over a precision multiplier of 1). Trackpad
    /// only — the discrete wheel uses `mouse_scroll_multiplier`.
    #[serde(default = "default_precise_scroll_multiplier")]
    pub precise_scroll_multiplier: f32,
    /// Selection auto-scroll speed: sustained lines/sec per line of pointer
    /// overshoot past a viewport edge. Higher reveals faster as you drag
    /// further past the edge.
    #[serde(default = "default_autoscroll_speed")]
    pub selection_autoscroll_speed: f32,
    /// Selection auto-scroll overshoot cap (lines): the speed saturates once
    /// the pointer is this many lines past the edge.
    #[serde(default = "default_autoscroll_max_overshoot")]
    pub selection_autoscroll_max_overshoot: f32,
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

/// How a precise (trackpad / Magic Mouse) gesture scrolls — the typed
/// behavior selector for the precise path of the scroll system
/// (`ux::scroll::PreciseMode`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PreciseScrollMode {
    /// Ghostty-faithful pixel accumulation: peel whole cells from a carried
    /// sub-cell pixel remainder, apply immediately, and let the OS momentum
    /// stream supply inertia (no synthetic friction). The recommended
    /// trackpad feel; the default.
    #[default]
    Pixels,
    /// Feed precise pixels into the synthetic momentum glide instead —
    /// app-side inertia on the trackpad, for devices/platforms with weak OS
    /// momentum (or taste).
    Momentum,
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
    /// Resolve `active_profile`, if set — the ONE profile-application
    /// point shared by boot (`main.rs`) and hot-reload
    /// (`ux::config_apply::ConfigReloadSource::take_if_dirty`), so a
    /// watched-config edit sees the same effective config the boot
    /// path would have produced.
    #[must_use]
    pub fn with_active_profile(&self) -> Self {
        match &self.active_profile {
            Some(name) => self.with_profile(name),
            None => self.clone(),
        }
    }

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

    /// Boot-time spawn directory — `window.inherit_working_directory`
    /// resolved for the FIRST session (local-PTY `single_pane::spawn`
    /// and the embedded-tear `new_session`), where the inheritance
    /// source is mado's own process cwd (the shell mado was launched
    /// from). Precedence, fixed:
    ///
    /// 1. `environment.working_directory` — an explicit operator
    ///    pin always wins.
    /// 2. Knob on → `None`: the child inherits mado's process cwd
    ///    (launch-shell directory) — the inheriting behavior.
    /// 3. Knob off → `$HOME`: the session starts at the neutral
    ///    default instead of wherever mado happened to be launched.
    ///    `$HOME` unset (degenerate env) falls back to `None`.
    ///
    /// Post-boot session spawns inherit from the FOCUSED terminal's
    /// OSC-7 cwd instead — see `TermSpec::with_inherited_cwd` +
    /// `SessionRegistry::focused_cwd`.
    #[must_use]
    pub fn boot_spawn_cwd(&self) -> Option<PathBuf> {
        if let Some(wd) = &self.environment.working_directory {
            return Some(wd.clone());
        }
        if self.window.inherit_working_directory {
            None
        } else {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    }

    /// The effects section with the legacy `accessibility.colorblind`
    /// deprecation alias RESOLVED into `effects.colorblind.mode`
    /// (the effects-section mode wins; the alias applies only when
    /// it is `None`), AND the ambience preset RESOLVED — `reduce_motion`
    /// forces it to `Off`. The single resolution point — every renderer
    /// ingress consumes THIS, never the raw `effects` field, so no
    /// entry point can skip the alias (M3 review 2026-06-12: the
    /// tear-attach path did exactly that and both knobs were dead
    /// there).
    ///
    /// The composed ambience members (which effects, at which threshold
    /// params) are NOT baked into the per-effect structs here — they
    /// stay as the typed [`crate::ambience::AmbienceComposition`] the
    /// renderer reads via [`Self::ambience_composition`], so the
    /// composition is the ONE source the effect-set + uniforms derive
    /// from (per `ambience` module docs). This function only resolves
    /// the deprecation alias on the config it returns.
    #[must_use]
    pub fn resolved_effects(&self) -> MadoEffectsConfig {
        let mut effects = self.effects.clone();
        if effects.colorblind.mode == ColorblindMode::None {
            effects.colorblind.mode = self.accessibility.colorblind;
        }
        // reduce_motion is the accessibility floor for the whole
        // composed layer — force the preset Off so the renderer's
        // composition is empty (zero nodes), the same contract aurora /
        // snow / glow honour by node-omission. The renderer re-derives
        // the typed `AmbienceComposition` from this resolved preset in
        // `set_effects_config` (the single ingress), so the composition
        // is the ONE source both the effect set and the per-frame
        // uniforms read.
        if self.accessibility.reduce_motion {
            effects.ambience = crate::ambience::AmbiencePreset::Off;
        }
        effects
    }
}

// ── Defaults + tiered constructors ──────────────────────────────
//
// Per operator principle: every tier is explicit, inspectable, and
// composable. Operators (and curious developers) can ask:
//   * `MadoConfig::bare()` → minimum-viable config, zero opinions
//   * `MadoConfig::bare_plus_discovered()` → bare + auto_detect outputs
//   * `MadoConfig::default()` → bare + defaults + discovered (== what
//      ships); this is mado-as-the-developers-believe-it-should-be-used
//   * Loaded config from disk → user-overlay on top of any of the above
//
// Each tier serializes to YAML cleanly; operators see + diff them via
// `mado config-show <tier>` (M-148 follow-up CLI subcommand).

impl MadoConfig {
    /// **Tier 0 — bare**: minimum-viable / zero-opinion config.
    ///
    /// Every field's value is the deliberate "do nothing extra"
    /// floor. Strings are empty, opt-in features are off, counts +
    /// scales are at unity/zero, optionals are None. Where the type
    /// has no None/zero analogue (enums, required numerics) the
    /// most universal / least-surprising variant is picked + the
    /// rationale documented inline.
    ///
    /// The bare config IS USABLE — mado launches with it, the
    /// renderer paints, a shell spawns — but it carries zero
    /// developer opinions. Mostly used as:
    ///
    /// 1. The documented floor visible via `mado config-show bare`.
    /// 2. The diff baseline against `mado config-show default` so
    ///    operators see exactly what defaults bought them.
    /// 3. A starting point for operators who want to build their
    ///    own config from the absolute minimum.
    ///
    /// Per operator principle: no future user should ever have to
    /// guess what bare means. This function IS the answer.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            // ── Fonts ────────────────────────────────────────────
            // Empty = no preference; cosmic-text resolves a system
            // fallback face. Not "fallback constant" — that's the
            // `discovered` tier's job.
            font_family: String::new(),
            font_italic: String::new(),
            // Empty = no symbol-family preference; symbol cells shape
            // against `font_family` (cosmic-text fallback otherwise).
            font_symbols: String::new(),
            // 12.0 = smallest universally-readable point size.
            // Not 0 (would break the renderer); not auto-detected
            // (that's `discovered`).
            font_size: 12.0,
            // 1.0 = zero leading (the bare floor; matches
            // `FleetDefaults::bare().line_height`). The prescribed tier
            // pulls the ghostty rhythm (1.65) via `from_fleet`.
            line_height: 1.0,
            font: FontConfig {
                family_bold: None,
                family_italic: None,
                family_bold_italic: None,
                thicken: false,
                synthetic_style: false, // bare = no synthesized style faces
                features: Vec::new(),
                codepoint_map: HashMap::new(),
            },
            // ── Window ───────────────────────────────────────────
            window: WindowConfig {
                width: 800,
                height: 600,
                padding: 0,
                // Decorations = false: truly chromeless. Default
                // tier overrides on macOS to keep traffic lights.
                decorations: false,
                title: None,
                unfocused_split_opacity: 1.0, // no dimming
                split_divider_color: None,
                background_image: None,
                fullscreen: false,
                maximize: false,
                // bare = no assumed inheritance from parent shell.
                inherit_working_directory: false,
                inherit_font_size: false,
                padding_balance: false,
                // Chromeless even in the bare tier: no OS tab strip,
                // flush titlebar, dark material to match the black bare
                // background. "Just the terminal" all the way down.
                macos: MacosWindowConfig {
                    native_tabs: false,
                    titlebar: TitlebarStyle::Flush,
                    appearance: WindowAppearance::Dark,
                },
            },
            // ── Shell ────────────────────────────────────────────
            shell: ShellConfig {
                // None = use OS $SHELL; bare has no explicit
                // opinion. Empty args.
                command: None,
                args: Vec::new(),
            },
            // ── Appearance ───────────────────────────────────────
            appearance: AppearanceConfig {
                // Black background, white foreground = universal
                // terminal colors since the 70s. No theme overlay.
                background: "#000000".into(),
                foreground: "#ffffff".into(),
                opacity: 1.0, // opaque
                bold_is_bright: false,
                minimum_contrast: 0.0, // no contrast enforcement
                background_blur: false,
                unfocused_split_fill: None,
            },
            // ── Cursor ───────────────────────────────────────────
            cursor: CursorConfig {
                style: CursorStyle::Block,    // most universal
                blink: false,                  // no animation in bare
                blink_rate_ms: 0,              // moot since blink=false
                color: String::new(),          // empty = use foreground
                opacity: 1.0,
                text_color: None,
                click_to_move: false,
            },
            // ── Behavior ─────────────────────────────────────────
            behavior: BehaviorConfig {
                scrollback_lines: 0, // bare = no scrollback at all
                copy_on_select: false,
                deselect_on_copy: false,
                confirm_close: false,
                mouse_hide_while_typing: false,
                mouse_scroll_multiplier: 1, // no multiplication
                wait_after_command: false,
                link_url: false,        // bare = no URL detection
                mouse_reporting: false, // bare = no mouse events to apps
                mouse_shift_capture: MouseShiftCapture::False,
                reflow_on_resize: false, // bare = legacy truncate on resize
                scroll_momentum: false,  // bare = direct 1:1 scroll, no inertia
                scroll_friction: default_scroll_friction(),
                scroll_max_velocity: default_scroll_max_velocity(),
                selection_autoscroll: false, // bare = selection frozen at edges
                // bare = ghostty pixel accumulator at a literal 1:1 gain (no
                // feel-bump), auto-scroll tuning at the shared defaults (it's
                // gated off by selection_autoscroll: false anyway).
                precise_scroll_mode: PreciseScrollMode::Pixels,
                precise_scroll_multiplier: 1.0,
                selection_autoscroll_speed: default_autoscroll_speed(),
                selection_autoscroll_max_overshoot: default_autoscroll_max_overshoot(),
            },
            // ── Theme ────────────────────────────────────────────
            // Empty = no theme overlay; appearance.background +
            // foreground are the truth.
            theme: String::new(),
            // ── Profiles ─────────────────────────────────────────
            profiles: HashMap::new(),
            active_profile: None,
            // ── Shaders ──────────────────────────────────────────
            shaders: ShaderConfig {
                enabled: false,
                files: Vec::new(),
            },
            // ── Accessibility ────────────────────────────────────
            accessibility: AccessibilityConfig {
                colorblind: ColorblindMode::None,
                min_contrast: 0.0,
                font_scale: 1.0, // unity
                reduce_motion: false,
            },
            // ── Shell integration ────────────────────────────────
            shell_integration: ShellIntegrationConfig {
                enabled: false,
                features: Vec::new(),
            },
            // ── Performance ──────────────────────────────────────
            performance: PerformanceConfig {
                vsync: false, // bare = no vsync opinion (driver default)
                target_fps: None,
                fps_cap: None,
                battery_fps_cap: None,
            },
            // ── Environment ──────────────────────────────────────
            environment: EnvironmentConfig {
                vars: HashMap::new(),
                working_directory: None,
                initial_command: None,
            },
            // ── Selection ────────────────────────────────────────
            selection: SelectionConfig {
                foreground: None,
                background: None,
                word_chars: String::new(), // bare = no word-char hints
                clear_on_typing: false,
                clear_on_copy: false,
            },
            // ── Search colors ────────────────────────────────────
            search: SearchColorsConfig {
                foreground: None,
                background: None,
                selected_foreground: None,
                selected_background: None,
            },
            // ── Keybindings ──────────────────────────────────────
            // Empty custom list. KeybindManager-side (the runtime
            // dispatch) also constructs via ::new() (zero bindings)
            // in bare contexts. Per operator principle:
            // nothing bound until explicitly opted in.
            keybinds: KeybindConfig {
                custom: Vec::new(),
            },
            // ── Quick Terminal ───────────────────────────────────
            quick_terminal: QuickTerminalConfig {
                enabled: false,
                edge: QuickTerminalEdge::Top, // moot since disabled
                size_fraction: 0.4,
                animation_ms: 0, // no animation when bare
                autohide_on_blur: false,
                hotkey: String::new(),
            },
            // ── Tear ─────────────────────────────────────────────
            tear: MadoTearConfig {
                // bare = don't try tear at all. Operator opts in.
                mode: TearMode::Never,
                // If they do opt in, embedded is the lightest path —
                // zero IPC, zero daemon spawn. Picked even in bare
                // because it's structurally minimal.
                runtime: TearRuntime::Embedded,
                socket: None,
                auto_spawn: false,
                spawn_wait_ms: 0, // no wait
                session_name: None,
                pane: None,
                impose: None,
                // Runtime re-attach is opt-in everywhere; bare keeps
                // the legacy one-shot binding.
                session_switching: false,
                // Auto-attach is opt-in too; bare = no cd-driven moves.
                auto_attach: AutoAttachMode::Off,
                session_picker_anchor: PickerAnchor::default(),
                // Stripped: the picker is live-sessions-only, never badged.
                // (Moot while session_switching=false keeps the picker inert,
                // but set explicitly so the bare tier is self-describing.)
                session_picker_surface_presets: false,
                session_picker_badges: BadgeMode::Off,
            },
            // ── Effects ──────────────────────────────────────────
            // All effects disabled in bare. Snow params stay at
            // MadoEffectsConfig's own Default (which keeps `enabled
            // = false` already).
            effects: MadoEffectsConfig::default(),
            vigy: MadoVigyConfig::default(),
            suggestions: SuggestionsConfig::bare(),
            // Safra ships off at every tier; a private config layer arms it.
            safra: crate::safra::SafraConfig::default(),
            janitors: JanitorsConfig::bare(),
            // bare = no link affordances at all.
            links: MadoLinksConfig::bare(),
            // bare = no feedback flourishes, no motion easing.
            feedback: FeedbackConfig::bare(),
            display: DisplayConfig::bare(),
            notifications: NotificationsConfig::bare(),
            motion: MotionConfig::bare(),
        }
    }

    /// **Tier 1 — bare + discovered**: `bare()` with `auto_detect`
    /// outputs layered in. No prescribed mado opinions; everything
    /// the runtime can probe (display size, future: theme, font,
    /// font_size) replaces the bare floor.
    ///
    /// Surfaces the "what would mado look like with ONLY detection,
    /// no developer opinions" question.
    #[must_use]
    pub fn bare_plus_discovered() -> Self {
        let mut c = Self::bare();
        let (w, h) = crate::auto_detect::detect_window_dims_or_fallback();
        c.window.width = w;
        c.window.height = h;
        c.theme = crate::auto_detect::detect_theme_or_fallback().to_string();
        c.font_family = crate::auto_detect::detect_font_family_or_fallback().to_string();
        c.font_symbols = crate::auto_detect::detect_font_symbols_or_fallback().to_string();
        c.font_size = crate::auto_detect::detect_font_size_or_fallback();
        c.window.padding = crate::auto_detect::detect_padding_or_fallback();
        c.behavior.scrollback_lines =
            crate::auto_detect::detect_scrollback_lines_or_fallback() as usize;
        c
    }
}

/// Map a `FleetDefaults` cursor-style name onto mado's typed
/// `CursorStyle`. The names are the fleet contract (`"block"` |
/// `"block_hollow"` | `"bar"` | `"underline"`); an unknown string
/// (impossible from `FleetDefaults::prescribed()`, which ships
/// `"block"`) falls back to the most universal Block so the config
/// still loads.
fn cursor_style_from_fleet(name: &str) -> CursorStyle {
    match name {
        "block_hollow" => CursorStyle::BlockHollow,
        "bar" => CursorStyle::Bar,
        "underline" => CursorStyle::Underline,
        _ => CursorStyle::Block,
    }
}

/// The grid-cell minimum-contrast floor for a resolved fleet theme. On
/// Vellum it is the spec §5 value sourced from the theme's OWN surfaces
/// (`VellumPalette::vellum().surfaces().minimum_contrast` = 3.0) — NOT a
/// hand-pinned constant, so a future Vellum re-tune propagates here on
/// the next compile. Other themes carry no fleet contrast token, so they
/// keep the curated app default.
fn minimum_contrast_from_fleet(theme_name: &str) -> f32 {
    if theme_name.eq_ignore_ascii_case("vellum") {
        ishou_tokens::VellumPalette::vellum()
            .surfaces()
            .minimum_contrast
    } else {
        default_minimum_contrast()
    }
}

/// **The flagship `FleetThemedConfig` production impl.** mado is the
/// widest-coverage operator-facing app in the fleet, so its
/// `from_fleet` is the reference the fleet audit asked for: a complete
/// `MadoConfig` whose every visual + behavioral field that has a fleet
/// analogue is DERIVED from `FleetDefaults` (and, for the colours, from
/// the theme's BORN ishou tokens), never hand-pinned.
///
/// What derives from where:
///
/// | mado field                         | source                                   |
/// |------------------------------------|------------------------------------------|
/// | `theme`                            | `fd.theme.resolve().name`                |
/// | `font_family` / `font_italic`     | `fd.font_family` / `fd.font_italic`      |
/// | `font_size`                       | `fd.font_size`                           |
/// | `line_height`                     | `fd.line_height` (cell-height rhythm)    |
/// | `window.padding`                  | `fd.padding`                             |
/// | `window.decorations`              | `fd.decorations_macos`/`_linux` (per-OS) |
/// | `behavior.scrollback_lines`       | `fd.scrollback_lines`                    |
/// | `behavior.link_url`               | `fd.link_url_detect`                     |
/// | `behavior.mouse_reporting`        | `fd.mouse_reporting`                     |
/// | `behavior.mouse_hide_while_typing`| `fd.mouse_hide_while_typing`             |
/// | `cursor.style`/`blink`/`rate`     | `fd.cursor_style`/`cursor_blink`/`…ms`   |
/// | `appearance.background`/`foreground` | the resolved theme bg/fg (Vellum `night0` / `snow1`) |
/// | `cursor.color`                    | the resolved theme cursor (Vellum `green_bright`) |
/// | `performance.vsync`               | `fd.vsync`                               |
/// | `accessibility.reduce_motion`/`font_scale` | `fd.reduce_motion`/`fd.font_scale` |
///
/// The ANSI palette, selection-glass, and search surfaces are resolved
/// at render time from the registered theme via `Theme::by_name(&theme)`
/// — setting `theme = "vellum"` is what makes them Vellum.
///
/// App-specific fields with no fleet analogue (shell, profiles, tear,
/// shaders, effects, vigy, quick-terminal, …) inherit the prescribed
/// per-section defaults through the `*Config::default()` base, so mado
/// keeps its frostmourne shell, snow-off effects, etc.
// ── FleetThemed escape-hatch fns ──────────────────────────────────
//
// `#[derive(FleetThemed)]` on `MadoConfig` mechanizes the flat
// `FleetDefaults → field` assignments (`font_family`, `font_italic`,
// `font_size`, `line_height`). The genuinely-unique tail — theme-surface
// mapping, the per-OS decoration split, the cursor name→enum map, the
// minimum-contrast floor — lives in these named `fn(&FleetDefaults) -> T`
// fns, referenced by `#[fleet(with = …)]`. Each reproduces the
// corresponding nested-struct literal from the original flagship impl
// byte-for-byte (proven by `from_fleet_byte_identical_to_handwritten`).

/// The fleet `theme` field: the resolved theme's BORN name (Vellum
/// `vellum`). Setting `theme` is what makes the runtime resolve
/// the ANSI palette / selection-glass / search surfaces to Vellum.
fn mado_theme_name_from_fleet(fd: &ishou_tokens::FleetDefaults) -> String {
    fd.theme.resolve().name.clone()
}

/// `window`: fleet `padding` + the per-OS decoration split (macOS keeps
/// traffic-lights, tiling-WM platforms go borderless), every other field
/// from `WindowConfig::default()`.
fn mado_window_from_fleet(fd: &ishou_tokens::FleetDefaults) -> WindowConfig {
    let decorations = if cfg!(target_os = "macos") {
        fd.decorations_macos
    } else {
        fd.decorations_linux
    };
    WindowConfig {
        padding: fd.padding,
        decorations,
        ..WindowConfig::default()
    }
}

/// `appearance`: bg/fg from the resolved theme's BORN tokens (never a
/// hand-pinned palette), the Vellum grid-cell contrast floor sourced
/// from the theme's own surfaces, every other field from
/// `AppearanceConfig::default()`.
fn mado_appearance_from_fleet(fd: &ishou_tokens::FleetDefaults) -> AppearanceConfig {
    let resolved = fd.theme.resolve();
    let bg = if resolved.background.is_empty() {
        default_bg()
    } else {
        resolved.background.clone()
    };
    let fg = if resolved.foreground.is_empty() {
        default_fg()
    } else {
        resolved.foreground.clone()
    };
    AppearanceConfig {
        background: bg,
        foreground: fg,
        minimum_contrast: minimum_contrast_from_fleet(&resolved.name),
        ..AppearanceConfig::default()
    }
}

/// `cursor`: the fleet cursor-style (name→enum), blink + rate, and the
/// resolved theme's explicit cursor colour (Vellum `green_bright`;
/// empty = "follow foreground"), every other field from
/// `CursorConfig::default()`.
fn mado_cursor_from_fleet(fd: &ishou_tokens::FleetDefaults) -> CursorConfig {
    CursorConfig {
        style: cursor_style_from_fleet(&fd.cursor_style),
        blink: fd.cursor_blink,
        blink_rate_ms: fd.cursor_blink_rate_ms,
        color: fd.theme.resolve().cursor.clone(),
        ..CursorConfig::default()
    }
}

/// `behavior`: fleet scrollback floor + URL/mouse toggles, every other
/// field from `BehaviorConfig::default()`. The scrollback value here is
/// the fleet 10k floor; `mado_fleet_scrollback_floor` (the `finalize`
/// hook) promotes it to mado's "never lose anything" contract.
fn mado_behavior_from_fleet(fd: &ishou_tokens::FleetDefaults) -> BehaviorConfig {
    BehaviorConfig {
        scrollback_lines: fd.scrollback_lines,
        link_url: fd.link_url_detect,
        mouse_reporting: fd.mouse_reporting,
        mouse_hide_while_typing: fd.mouse_hide_while_typing,
        ..BehaviorConfig::default()
    }
}

/// `accessibility`: fleet reduce-motion + font-scale, every other field
/// from `AccessibilityConfig::default()`.
fn mado_accessibility_from_fleet(fd: &ishou_tokens::FleetDefaults) -> AccessibilityConfig {
    AccessibilityConfig {
        reduce_motion: fd.reduce_motion,
        font_scale: fd.font_scale,
        ..AccessibilityConfig::default()
    }
}

/// `performance`: fleet vsync, every other field from
/// `PerformanceConfig::default()`.
fn mado_performance_from_fleet(fd: &ishou_tokens::FleetDefaults) -> PerformanceConfig {
    PerformanceConfig {
        vsync: fd.vsync,
        ..PerformanceConfig::default()
    }
}

/// The `#[fleet(finalize = …)]` hook: the fleet `scrollback_lines` (10k)
/// is a RAM-cap *floor*; mado's prescribed contract is "never lose
/// anything" (`usize::MAX`). Honour the stronger contract when the fleet
/// value is the documented 10k default; a smaller deliberate fleet value
/// still flows through.
fn mado_fleet_scrollback_floor(c: &mut MadoConfig, fd: &ishou_tokens::FleetDefaults) {
    if fd.scrollback_lines == 10_000 {
        c.behavior.scrollback_lines = default_scrollback();
    }
}

/// The `#[fleet(base = …)]` ctor: supplies the per-section
/// `*Config::default()` values for every field with NO fleet analogue
/// (`font_symbols`, `font`, `shell`, `profiles`, `shaders`,
/// `shell_integration`, `environment`, `selection`, `search`,
/// `keybinds`, `quick_terminal`, `tear`, `effects`, `vigy`). The derive
/// overrides every `#[fleet(…)]`-mapped field via `..mado_fleet_base()`,
/// so the placeholder values for THOSE fields here are never observed —
/// they only need to type-check.
fn mado_fleet_base() -> MadoConfig {
    MadoConfig {
        // ── Fleet-mapped placeholders (overridden by the derive) ──
        font_family: String::new(),
        font_italic: String::new(),
        font_size: 0.0,
        line_height: 0.0,
        window: WindowConfig::default(),
        appearance: AppearanceConfig::default(),
        cursor: CursorConfig::default(),
        behavior: BehaviorConfig::default(),
        theme: String::new(),
        accessibility: AccessibilityConfig::default(),
        performance: PerformanceConfig::default(),
        // ── Untouched fields: the curated per-section defaults the
        //    original flagship impl supplied verbatim ──
        font_symbols: default_font_symbols(),
        font: FontConfig::default(),
        shell: ShellConfig::default(),
        profiles: HashMap::new(),
        active_profile: None,
        shaders: ShaderConfig::default(),
        shell_integration: ShellIntegrationConfig::default(),
        environment: EnvironmentConfig::default(),
        selection: SelectionConfig::default(),
        search: SearchColorsConfig::default(),
        keybinds: KeybindConfig::default(),
        quick_terminal: QuickTerminalConfig::default(),
        tear: MadoTearConfig::default(),
        effects: MadoEffectsConfig::default(),
        vigy: MadoVigyConfig::default(),
        suggestions: SuggestionsConfig::default(),
        safra: crate::safra::SafraConfig::default(),
        janitors: JanitorsConfig::default(),
        links: MadoLinksConfig::default(),
        feedback: FeedbackConfig::default(),
        display: DisplayConfig::default(),
        notifications: NotificationsConfig::default(),
        motion: MotionConfig::default(),
    }
}

/// Fleet-wide TieredConfig contract for MadoConfig. Operators run
/// `mado config-show bare|discovered|default` to see each tier and
/// diff via shell — the trait makes the operator surface identical
/// to every other shikumi-typed config in the pleme-io fleet.
impl shikumi::TieredConfig for MadoConfig {
    fn bare() -> Self {
        MadoConfig::bare()
    }
    fn discovered() -> Self {
        MadoConfig::bare_plus_discovered()
    }
    /// **Tier 2 — prescribed.** No longer hand-pins constants: derives
    /// from `FleetDefaults::prescribed()` via
    /// `<Self as FleetThemedConfig>::from_fleet`, exactly the move the
    /// fleet audit demands. `MadoConfig::default()` layers the same
    /// fleet base with the runtime env-adaptive overlay (window dims,
    /// DPR font size) — that overlay tier is what most operators run, so
    /// `prescribed_default` is `default()`.
    fn prescribed_default() -> Self {
        MadoConfig::default()
    }
}

impl Default for MadoConfig {
    /// **Tier 2 — bare + defaults + discovered**: mado-as-the-
    /// developers-believe-it-should-be-used. This is what operators
    /// get on first launch without writing any yaml. Layers
    /// `bare_plus_discovered` + the curated mado defaults (font,
    /// theme, behaviors). Most users will run this tier; the
    /// `mado config-show default` subcommand (M-148 followup)
    /// makes every value visible.
    fn default() -> Self {
        // DERIVE from the fleet baseline (Vellum + JetBrainsMono + the
        // prescribed cursor/behavior/window choices) via the flagship
        // `FleetThemedConfig` impl — no hand-pinned constants here. A
        // future fleet rebrand touches `FleetDefaults::prescribed()` /
        // `FleetTheme::prescribed_default()` ONCE and mado converges on
        // the next compile (pinned by the Guard test).
        use ishou_tokens::FleetThemedConfig;
        let mut c = MadoConfig::from_fleet(&ishou_tokens::FleetDefaults::prescribed());
        // Environment-adaptive overlay (prime-directive: best-fit defaults
        // out of the box). Where the runtime can probe THIS host, the
        // detected value replaces the static default just set above.
        // Detection returns None off-main-thread / headless → fallback to
        // that static default, so tests + CI are unaffected.
        let (win_w, win_h) = crate::auto_detect::detect_window_dims_or_fallback();
        c.window.width = win_w;
        c.window.height = win_h;
        c.font_size = crate::auto_detect::detect_font_size_or_fallback();
        // ── Session integration with tear (prescribed/operator tier) ──
        // Fleet default: mado is FULLY session-integrated with its embedded
        // tear runtime (runtime = Embedded by default, the only mode that
        // drives the switch channel today). Two knobs come on together:
        //   * `session_switching` — runtime pane re-attach: the `switch_session`
        //     MCP tool + the Ctrl-S picker move the displayed pane to a
        //     different live tear session in the same window (no tabs/splits).
        //   * `auto_attach = AutoSwitch` — the praça automation-first headline:
        //     a cross-project `cd` auto-attaches the pane to that project's
        //     session (spawning + naming + binding it if none exists).
        // auto_attach REQUIRES session_switching, so they're set as a pair.
        // The bare tier (`bare()`) keeps BOTH off (tear = Never), so the
        // opt-out is `MADO_TIER=bare` or an explicit `tear.*` override.
        c.tear.session_switching = true;
        c.tear.auto_attach = AutoAttachMode::AutoSwitch;
        // Scrollback is deliberately NOT overlaid: detection can only
        // *downgrade* the prescribed "never lose anything" contract
        // (`default_scrollback()` = usize::MAX, commit 752cb01) to a
        // RAM-tiered cap. Under the grows-on-demand VecDeque model a
        // cap saves no memory — it only truncates history — so the
        // RAM tiers live in the *discovered* tier only (the "what
        // would detection alone give" question).
        c
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
            macos: MacosWindowConfig::default(),
        }
    }
}

impl Default for ShellConfig {
    /// The prescribed default shell for mado is `frostmourne` — the
    /// curated pleme-io shell distribution that ships skim + atuin +
    /// the typed `(defbind :key "C-r" :action "__frost_picker_history__")`
    /// keybind out of the box. Operators who want plain `$SHELL` /
    /// `/bin/zsh` / `/bin/sh` override via `mado.yaml` or via the
    /// blackmatter-mado HM module's `programs.mado.shell.command`
    /// option. The config-derived value is PATH-guarded at shell
    /// resolution (`main.rs::resolve_shell_or_fallback`): if frostmourne
    /// isn't on PATH (e.g. a standalone release download) it falls back to
    /// `$SHELL → /bin/zsh`, so the first window always gets a real shell.
    fn default() -> Self {
        Self {
            command: Some("frostmourne".to_string()),
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
            deselect_on_copy: default_deselect_on_copy(),
            confirm_close: false,
            mouse_hide_while_typing: default_mouse_hide(),
            mouse_scroll_multiplier: default_mouse_scroll_mult(),
            wait_after_command: false,
            link_url: true,
            mouse_reporting: true,
            mouse_shift_capture: MouseShiftCapture::default(),
            reflow_on_resize: true,
            scroll_momentum: true,
            scroll_friction: default_scroll_friction(),
            scroll_max_velocity: default_scroll_max_velocity(),
            selection_autoscroll: true,
            precise_scroll_mode: PreciseScrollMode::default(),
            precise_scroll_multiplier: default_precise_scroll_multiplier(),
            selection_autoscroll_speed: default_autoscroll_speed(),
            selection_autoscroll_max_overshoot: default_autoscroll_max_overshoot(),
        }
    }
}

fn default_font_family() -> String {
    // detect_font_family probes the installed Nerd-Font ladder
    // (M1: detection stubbed, falls back to FALLBACK_FONT_FAMILY).
    // Operators with strong opinions override via mado.yaml. The
    // fallback constant lives in auto_detect.rs alongside its peers.
    crate::auto_detect::detect_font_family_or_fallback().to_string()
}

fn default_font_italic() -> String {
    // Italics now slant the SAME JetBrainsMono face (ghostty's model),
    // not a foreign calligraphic typeface — see
    // `ishou-tokens::FleetDefaults::prescribed().font_italic`, which
    // equals `font_family`. cosmic-text's `Attrs::style(Style::Italic)`
    // synthesizes the slant from that face. Sourced from the fleet so a
    // future italic-family change propagates here on the next compile.
    // Used only when a YAML config omits the field.
    ishou_tokens::FleetDefaults::prescribed().font_italic
}
fn default_font_symbols() -> String {
    // Dedicated symbols/Nerd-icon family per ghostty's model. M1:
    // detection stubbed, falls back to FALLBACK_FONT_SYMBOLS. The
    // fallback constant lives in auto_detect.rs alongside its peers.
    crate::auto_detect::detect_font_symbols_or_fallback().to_string()
}
fn default_font_size() -> f32 {
    crate::auto_detect::detect_font_size_or_fallback()
}
fn default_line_height() -> f32 {
    // Cell-height rhythm — the fleet prescribed value (ghostty's native
    // 1.32 × its +25% cell = 1.65), sourced from ishou so a fleet
    // line-height retune propagates here on the next compile rather
    // than being re-pinned. Used only when a YAML config omits the
    // field; the prescribed/from_fleet path reads `fd.line_height`.
    ishou_tokens::FleetDefaults::prescribed().line_height
}
fn default_width() -> u32 {
    crate::auto_detect::detect_window_dims_or_fallback().0
}
fn default_height() -> u32 {
    crate::auto_detect::detect_window_dims_or_fallback().1
}
fn default_padding() -> u32 {
    crate::auto_detect::detect_padding_or_fallback()
}

fn default_decorations() -> bool {
    // Platform-aware: true on macOS so traffic-light buttons
    // stay (and platform::apply_native_styling themes the titlebar
    // band into the canvas — FullSizeContentView only with
    // `titlebar: overlay`); false on Linux/Windows for pure
    // borderless. See WindowConfig::decorations doc for the
    // operator contract.
    cfg!(target_os = "macos")
}
/// The prescribed fleet theme, resolved to its BORN ishou tokens.
/// `default_bg`/`default_fg`/`default_cursor_color` read from here so
/// the appearance fallbacks carry the SAME palette as the registered
/// `vellum` theme — no hand-pinned Nord hex, no drift. (The audit's
/// complaint: `prescribed_default` hand-pinned `#2e3440`.)
fn prescribed_resolved_theme() -> ishou_tokens::ResolvedTheme {
    ishou_tokens::FleetTheme::prescribed_default().resolve()
}
fn default_bg() -> String {
    // Vellum night0 (#16140E) — derived from the BORN tokens, not
    // the legacy Nord #2e3440.
    prescribed_resolved_theme().background
}
fn default_fg() -> String {
    // Vellum snow1 (#E2DBC8) — derived, not the legacy Nord #eceff4.
    prescribed_resolved_theme().foreground
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
    // Vellum green_bright (#ADD7A3) — the §5 block cursor (inverse
    // pair ≥7.0). Derived from the BORN tokens, not the legacy Nord
    // snow #eceff4. Empty resolved cursor (the bare tier) falls back to
    // "follow foreground" semantics by returning the foreground.
    let resolved = prescribed_resolved_theme();
    if resolved.cursor.is_empty() {
        resolved.foreground
    } else {
        resolved.cursor
    }
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
    // Muscle-memory contract (operator directive 2026-06-11): a
    // highlight goes straight to the clipboard — no extra chord.
    // The bare tier still opts out (everything-off contract).
    true
}
fn default_deselect_on_copy() -> bool {
    // Lift-to-copy contract (operator directive 2026-07-06): lifting the
    // mouse after a highlight both copies AND unhighlights, so no click is
    // ever needed to copy and no highlight lingers. Expanded through config
    // (not coded in): `false` restores the copy-without-deselect behavior.
    true
}
fn default_mouse_hide() -> bool {
    true
}
fn default_mouse_scroll_mult() -> u32 {
    2
}
fn default_scroll_friction() -> f32 {
    // Tuned for a weighty-but-responsive glide. With exponential decay
    // the velocity halves every ln2/3 ≈ 0.23s; a flick coasts for
    // ~0.7s (≈3 half-lives down to the stop epsilon) and then eases to
    // a crisp halt — gravity-like, not an instant jump, not a crawl.
    3.0
}
fn default_scroll_max_velocity() -> f32 {
    // Lines/sec ceiling so a frantic repeated flick can't launch to the
    // scrollback top in one frame. ~200 lines/sec ≈ three+ 60-row
    // screens per second at peak — gives the same-direction streak
    // acceleration real headroom to feel fast, yet still eye-trackable
    // (the friction glide always eases it back down).
    200.0
}
fn default_precise_scroll_multiplier() -> f32 {
    // Physical-pixel gain for the trackpad. winit gives mado no free speed
    // bump, so to match ghostty's effective macOS trackpad feel — its apprt's
    // hard-coded 2× over a precision multiplier of 1 — we pin the gain to 2.0.
    // 1.0 would be literal 1:1 finger tracking (the bare tier uses that).
    2.0
}
fn default_autoscroll_speed() -> f32 {
    // Sustained lines/sec per line of pointer overshoot past a viewport edge:
    // one line past ⇒ a gentle 18 lines/sec crawl, scaling up to the overshoot
    // cap so the further you drag the faster history reveals.
    18.0
}
fn default_autoscroll_max_overshoot() -> f32 {
    // Overshoot cap (lines): drag more than six lines past the edge and the
    // auto-scroll speed saturates — fast, but never an uncontrollable launch.
    6.0
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
    // detect_theme probes macOS appearance (M1 stub: returns None);
    // falls back to FALLBACK_THEME = "vellum" (the prescribed
    // fleet theme). Operators override via mado.yaml. Constant lives in
    // auto_detect.rs and is pinned to the fleet theme by the convergence
    // guard so a fleet rebrand can't leave mado on a stale name.
    crate::auto_detect::detect_theme_or_fallback().to_string()
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
    // BOUNDARY set (non-empty REPLACES the not-alphanumeric rule in
    // selection::word_bounds_in_row): listed characters split words,
    // everything else is word-interior — so `/.-_@` staying unlisted
    // is what makes double-click grab whole paths/URLs (the kitty
    // contract). The leading space is load-bearing: the M3 review
    // (2026-06-12) wired the previously-dead knob into the engine,
    // and without ' ' here the prescribed default would select
    // straight across spaces.
    " \t'\"│`|:;,()[]{}<>$".into()
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

// The M3 `ConfigReloadCell` (watch callback parks a full config,
// renderer drains it) was DELETED at M4 stage 2: hot-reload now
// flows watcher → dirty flag → per-frame
// `ux::ConfigHotReload::poll_config_reload` → typed `SetterCall`
// delta against the renderer. One mechanism, both entry points.

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

    // ── Config tier model ──────────────────────────────────────

    #[test]
    fn bare_tier_has_zero_opinion_defaults() {
        let bare = MadoConfig::bare();

        // ── Fonts ──────────────────────────────────────────────
        assert_eq!(bare.font_family, "");
        assert_eq!(bare.font_italic, "");
        assert_eq!(bare.font_symbols, "");
        assert!((bare.font_size - 12.0).abs() < 0.001);
        // Bare cell rhythm = zero leading (matches FleetDefaults::bare).
        assert!((bare.line_height - 1.0).abs() < 0.001);
        assert!(bare.font.family_bold.is_none());
        assert!(bare.font.family_italic.is_none());
        assert!(bare.font.family_bold_italic.is_none());
        assert!(!bare.font.thicken);
        assert!(!bare.font.synthetic_style);
        assert!(bare.font.features.is_empty());
        assert!(bare.font.codepoint_map.is_empty());

        // ── Window ─────────────────────────────────────────────
        assert_eq!(bare.window.width, 800);
        assert_eq!(bare.window.height, 600);
        assert_eq!(bare.window.padding, 0);
        assert!(!bare.window.decorations);
        assert!(bare.window.title.is_none());
        assert!((bare.window.unfocused_split_opacity - 1.0).abs() < 0.001);
        assert!(bare.window.split_divider_color.is_none());
        assert!(bare.window.background_image.is_none());
        assert!(!bare.window.fullscreen);
        assert!(!bare.window.maximize);
        assert!(!bare.window.inherit_working_directory);
        assert!(!bare.window.inherit_font_size);
        assert!(!bare.window.padding_balance);

        // ── Shell ──────────────────────────────────────────────
        assert!(bare.shell.command.is_none());
        assert!(bare.shell.args.is_empty());

        // ── Appearance ─────────────────────────────────────────
        assert_eq!(bare.appearance.background, "#000000");
        assert_eq!(bare.appearance.foreground, "#ffffff");
        assert!((bare.appearance.opacity - 1.0).abs() < 0.001);
        assert!(!bare.appearance.bold_is_bright);
        assert!((bare.appearance.minimum_contrast - 0.0).abs() < 0.001);
        assert!(!bare.appearance.background_blur);
        assert!(bare.appearance.unfocused_split_fill.is_none());

        // ── Cursor ─────────────────────────────────────────────
        assert_eq!(bare.cursor.style, CursorStyle::Block);
        assert!(!bare.cursor.blink);
        assert_eq!(bare.cursor.blink_rate_ms, 0);
        assert_eq!(bare.cursor.color, "");
        assert!((bare.cursor.opacity - 1.0).abs() < 0.001);
        assert!(bare.cursor.text_color.is_none());
        assert!(!bare.cursor.click_to_move);

        // ── Behavior ───────────────────────────────────────────
        assert_eq!(bare.behavior.scrollback_lines, 0);
        assert!(!bare.behavior.copy_on_select);
        assert!(!bare.behavior.deselect_on_copy);
        assert!(!bare.behavior.confirm_close);
        assert!(!bare.behavior.mouse_hide_while_typing);
        assert_eq!(bare.behavior.mouse_scroll_multiplier, 1);
        assert!(!bare.behavior.wait_after_command);
        assert!(!bare.behavior.link_url);
        assert!(!bare.behavior.mouse_reporting);
        assert_eq!(bare.behavior.mouse_shift_capture, MouseShiftCapture::False);
        // bare = legacy truncate-on-resize; rewrap is an opinion.
        assert!(!bare.behavior.reflow_on_resize);

        // ── Theme / Profiles ───────────────────────────────────
        assert_eq!(bare.theme, "");
        assert!(bare.profiles.is_empty());
        assert!(bare.active_profile.is_none());

        // ── Shaders ────────────────────────────────────────────
        assert!(!bare.shaders.enabled);
        assert!(bare.shaders.files.is_empty());

        // ── Accessibility ──────────────────────────────────────
        assert_eq!(bare.accessibility.colorblind, ColorblindMode::None);
        assert!((bare.accessibility.min_contrast - 0.0).abs() < 0.001);
        assert!((bare.accessibility.font_scale - 1.0).abs() < 0.001);
        assert!(!bare.accessibility.reduce_motion);

        // ── Shell integration ──────────────────────────────────
        assert!(!bare.shell_integration.enabled);
        assert!(bare.shell_integration.features.is_empty());

        // ── Performance ────────────────────────────────────────
        assert!(!bare.performance.vsync);
        assert!(bare.performance.target_fps.is_none());
        assert!(bare.performance.fps_cap.is_none());
        assert!(bare.performance.battery_fps_cap.is_none());

        // ── Environment ────────────────────────────────────────
        assert!(bare.environment.vars.is_empty());
        assert!(bare.environment.working_directory.is_none());
        assert!(bare.environment.initial_command.is_none());

        // ── Selection ──────────────────────────────────────────
        assert!(bare.selection.foreground.is_none());
        assert!(bare.selection.background.is_none());
        assert_eq!(bare.selection.word_chars, "");
        assert!(!bare.selection.clear_on_typing);
        assert!(!bare.selection.clear_on_copy);

        // ── Search ─────────────────────────────────────────────
        assert!(bare.search.foreground.is_none());
        assert!(bare.search.background.is_none());
        assert!(bare.search.selected_foreground.is_none());
        assert!(bare.search.selected_background.is_none());

        // ── Keybindings ────────────────────────────────────────
        assert!(bare.keybinds.custom.is_empty());

        // ── Quick Terminal ─────────────────────────────────────
        assert!(!bare.quick_terminal.enabled);
        assert_eq!(bare.quick_terminal.edge, QuickTerminalEdge::Top);
        assert!((bare.quick_terminal.size_fraction - 0.4).abs() < 0.001);
        assert_eq!(bare.quick_terminal.animation_ms, 0);
        assert!(!bare.quick_terminal.autohide_on_blur);
        assert_eq!(bare.quick_terminal.hotkey, "");

        // ── Tear ───────────────────────────────────────────────
        assert_eq!(bare.tear.mode, TearMode::Never);
        // Embedded is the structurally lightest tear runtime
        // (no IPC, no daemon) — picked even in bare.
        assert_eq!(bare.tear.runtime, TearRuntime::Embedded);
        assert!(bare.tear.socket.is_none());
        assert!(!bare.tear.auto_spawn);
        assert_eq!(bare.tear.spawn_wait_ms, 0);
        assert!(bare.tear.session_name.is_none());
        assert!(bare.tear.pane.is_none());
        assert!(bare.tear.impose.is_none());

        // ── Effects ────────────────────────────────────────────
        // Every catalog effect is default-off (the clean fleet
        // look); the static knobs carry the catalog reference
        // values so enabling is a one-line config change.
        assert!(!bare.effects.snow.enabled);
        assert_eq!(bare.effects.colorblind.mode, ColorblindMode::None);
        assert!(!bare.effects.crt.enabled);
        assert!(!bare.effects.scanlines.enabled);
        assert!(!bare.effects.bloom.enabled);
        assert!(!bare.effects.glow_on_bell.enabled);
    }

    #[test]
    fn bare_plus_discovered_overrides_window_dims() {
        let bare = MadoConfig::bare();
        let discovered = MadoConfig::bare_plus_discovered();
        // The window dims came from auto_detect, not the bare floor.
        // Either we detected display dims (so they're different from
        // bare's 800x600) OR detection failed and we got the
        // FALLBACK constant (1200, 800).
        let detected_dims = (discovered.window.width, discovered.window.height);
        let bare_dims = (bare.window.width, bare.window.height);
        // discovered window is at least as big as bare on every axis
        // (auto_detect clamps to 800 minimum).
        assert!(detected_dims.0 >= bare_dims.0);
        assert!(detected_dims.1 >= bare_dims.1);
        // Theme/font come from auto_detect's FALLBACK constants in
        // M1 (detection stubs return None).
        assert_eq!(discovered.theme, crate::auto_detect::FALLBACK_THEME);
        assert_eq!(
            discovered.font_family,
            crate::auto_detect::FALLBACK_FONT_FAMILY
        );
        assert_eq!(
            discovered.font_symbols,
            crate::auto_detect::FALLBACK_FONT_SYMBOLS
        );
    }

    #[test]
    fn default_tier_is_bare_plus_defaults_plus_discovered() {
        // Default() is the operator-facing tier: bare + defaults + discovered.
        // Compared to bare_plus_discovered, it picks up the curated
        // mado opinions (e.g. snow effects config, full keybind set in
        // the typed keybinds group, etc.).
        let default_cfg = MadoConfig::default();
        // Confirm we land on the FALLBACK_THEME (since detect_theme
        // is still a stub in M1) — same value the discovered tier picks.
        assert_eq!(default_cfg.theme, crate::auto_detect::FALLBACK_THEME);
        // Confirm window dims came from auto_detect.
        assert!(default_cfg.window.width >= 800);
        assert!(default_cfg.window.height >= 600);
        // Confirm tear runtime is Embedded (the zero-IPC default).
        assert_eq!(default_cfg.tear.runtime, TearRuntime::Embedded);
    }

    #[test]
    fn tier_yaml_serialization_diff_is_visible() {
        // The fundamental contract: serializing each tier to YAML
        // yields BYTE-DIFFERENT strings (proves the tiers actually
        // differ — operators run `diff <(mado config-show bare)
        // <(mado config-show default)` to see what defaults bought
        // them).
        let bare_yaml = serde_yaml_ng::to_string(&MadoConfig::bare()).unwrap();
        let default_yaml = serde_yaml_ng::to_string(&MadoConfig::default()).unwrap();
        assert_ne!(bare_yaml, default_yaml);
    }

    #[test]
    fn mado_impls_shikumi_tiered_config() {
        use shikumi::TieredConfig;
        let b = <MadoConfig as TieredConfig>::bare();
        let d = <MadoConfig as TieredConfig>::prescribed_default();
        assert_ne!(b.font_family, d.font_family);
        // diff_against produces a non-empty diff between bare + default
        let diff = d.diff_against(&b);
        assert!(!diff.is_empty_diff(), "bare vs default must differ");
        let unified = diff.render_unified();
        // Sanity: the prescribed default's theme is in the diff.
        assert!(unified.contains("theme"));
    }

    #[test]
    fn session_picker_features_are_tiered_bare_stripped_prescribed_reasonable() {
        use shikumi::TieredConfig;
        // BARE: the union-picker features are stripped — no preset surfacing,
        // no badges (the legacy live-only picker).
        let b = <MadoConfig as TieredConfig>::bare();
        assert!(
            !b.tear.session_picker_surface_presets,
            "bare must NOT surface latent presets"
        );
        assert_eq!(b.tear.session_picker_badges, BadgeMode::Off, "bare = no badges");
        // PRESCRIBED: features on, but with REASONABLE defaults — presets
        // surface (empty until saved) + Auto badges (only when mixed), so the
        // common all-live picker stays byte-identical to legacy.
        let d = <MadoConfig as TieredConfig>::prescribed_default();
        assert!(
            d.tear.session_picker_surface_presets,
            "prescribed surfaces presets"
        );
        assert_eq!(
            d.tear.session_picker_badges,
            BadgeMode::Auto,
            "prescribed = Auto badges (minimal impact)"
        );
    }

    /// **Flagship FleetThemedConfig convergence Guard.** mado is the
    /// widest-coverage operator-facing app, so this is the reference
    /// impl the fleet audit asked for: touching `FleetDefaults` (or
    /// `FleetTheme::prescribed_default()`) now breaks mado AT TEST TIME.
    /// Pins font_family / font_size / theme + the full ANSI-16 palette
    /// against the BORN ishou tokens — the convergence guarantee made
    /// real, not asserted in prose.
    #[test]
    fn mado_converges_with_fleet_vellum() {
        use shikumi::TieredConfig;
        let d = <MadoConfig as TieredConfig>::prescribed_default();

        // ── Font + theme: the standard Guard chain ──
        // Pins family / italic / size / line-height against the BORN
        // FleetDefaults — the ghostty-aligned font (non-Mono family,
        // synthesized-slant italics on the same face, size 13, 1.65
        // cell rhythm) cannot drift from mado without failing here.
        ishou_tokens::convergence::Guard::for_app("mado")
            .expect_font_family(&d.font_family)
            .expect_font_italic(&d.font_italic)
            .expect_font_size(d.font_size)
            .expect_line_height(d.line_height)
            .run();

        // ── Theme: the config's String theme is the fleet theme's
        // resolved name (the Guard's expect_theme takes the enum; mado
        // stores the name, so we assert the resolved-name equality). ──
        let fleet_theme = ishou_tokens::FleetDefaults::prescribed().theme;
        assert_eq!(fleet_theme, ishou_tokens::FleetTheme::Vellum);
        assert_eq!(d.theme, fleet_theme.resolve().name);
        assert_eq!(d.theme, "vellum");

        // ── ANSI palette: DECOUPLED from the BORN parchment ANSI
        // (washed-out-colors fix, 2026-06-14). Vellum's CHROME stays
        // BORN, but the ANSI-16 apps paint their CONTENT with is the
        // vivid Nord set — otherwise vim/shell/autocomplete colors render
        // as the dull grey-green muted parchment tones and diverge from
        // ghostty. So the registered Vellum theme's ANSI must NOT equal
        // the muted `ResolvedTheme::vellum().ansi_16`, and it must carry
        // the vivid Nord aurora/frost values. ──
        let theme = crate::theme::Theme::by_name(&d.theme).expect("vellum theme registered");
        let resolved = ishou_tokens::ResolvedTheme::vellum();
        let muted_green = ishou_tokens::Srgb::from_hex(&resolved.ansi_16[2])
            .expect("resolved ANSI hex parses");
        assert_ne!(
            (theme.ansi[2].r, theme.ansi[2].g, theme.ansi[2].b),
            (muted_green.r, muted_green.g, muted_green.b),
            "Vellum content ANSI must be the vivid Nord set, not the muted BORN parchment ANSI",
        );
        // Vivid Nord anchors (aurora green / red, frost cyan).
        assert_eq!((theme.ansi[1].r, theme.ansi[1].g, theme.ansi[1].b), (0xBF, 0x61, 0x6A));
        assert_eq!((theme.ansi[2].r, theme.ansi[2].g, theme.ansi[2].b), (0xA3, 0xBE, 0x8C));
        assert_eq!((theme.ansi[6].r, theme.ansi[6].g, theme.ansi[6].b), (0x88, 0xC0, 0xD0));

        // ── Agent accent: the fable_violet SEMANTIC token, never a hex. ──
        let fable_violet = ishou_tokens::VellumPalette::vellum()
            .get(ishou_tokens::SemanticRoles::vellum().agent)
            .expect("fable_violet token");
        assert_eq!(
            (theme.agent_accent.r, theme.agent_accent.g, theme.agent_accent.b),
            (fable_violet.r, fable_violet.g, fable_violet.b),
        );
    }

    /// **The `#[derive(FleetThemed)]` byte-identical proof.** The
    /// flagship `from_fleet` was migrated from a hand-written
    /// `impl FleetThemedConfig` to `#[derive(FleetThemed)]` + per-field
    /// `#[fleet(…)]` attributes (the ★★ EMITTER SUBSTRATE move). This
    /// test pins that the derive reproduces the OLD hand-written body
    /// **byte-for-byte**: `from_fleet_handwritten_frozen` below is a
    /// frozen verbatim copy of the pre-migration constructor (it does
    /// NOT call the derive), and the derived `from_fleet` must serialize
    /// to identical YAML for every `FleetDefaults` tier. If the derive
    /// (or an escape-hatch fn) drifts from the flagship, this fails.
    #[test]
    fn from_fleet_byte_identical_to_handwritten() {
        for fd in [
            ishou_tokens::FleetDefaults::prescribed(),
            ishou_tokens::FleetDefaults::bare(),
        ] {
            let derived = <MadoConfig as ishou_tokens::FleetThemedConfig>::from_fleet(&fd);
            let frozen = from_fleet_handwritten_frozen(&fd);
            let derived_yaml = serde_yaml_ng::to_string(&derived).unwrap();
            let frozen_yaml = serde_yaml_ng::to_string(&frozen).unwrap();
            assert_eq!(
                derived_yaml, frozen_yaml,
                "FleetThemed derive drifted from the flagship hand-written from_fleet",
            );
        }
    }

    /// Frozen verbatim copy of the pre-`#[derive(FleetThemed)]` flagship
    /// constructor. Intentionally NOT routed through the derive — this is
    /// the independent reference the byte-identical proof compares
    /// against. Do not "simplify" by delegating to the derive; that would
    /// make the proof circular.
    fn from_fleet_handwritten_frozen(fd: &ishou_tokens::FleetDefaults) -> MadoConfig {
        let resolved = fd.theme.resolve();
        let bg = if resolved.background.is_empty() {
            default_bg()
        } else {
            resolved.background.clone()
        };
        let fg = if resolved.foreground.is_empty() {
            default_fg()
        } else {
            resolved.foreground.clone()
        };
        let cursor_color = resolved.cursor.clone();
        let decorations = if cfg!(target_os = "macos") {
            fd.decorations_macos
        } else {
            fd.decorations_linux
        };
        let mut c = MadoConfig {
            font_family: fd.font_family.clone(),
            font_italic: fd.font_italic.clone(),
            font_symbols: default_font_symbols(),
            font_size: fd.font_size,
            line_height: fd.line_height,
            font: FontConfig::default(),
            window: WindowConfig {
                padding: fd.padding,
                decorations,
                ..WindowConfig::default()
            },
            shell: ShellConfig::default(),
            appearance: AppearanceConfig {
                background: bg,
                foreground: fg,
                minimum_contrast: minimum_contrast_from_fleet(&resolved.name),
                ..AppearanceConfig::default()
            },
            cursor: CursorConfig {
                style: cursor_style_from_fleet(&fd.cursor_style),
                blink: fd.cursor_blink,
                blink_rate_ms: fd.cursor_blink_rate_ms,
                color: cursor_color,
                ..CursorConfig::default()
            },
            behavior: BehaviorConfig {
                scrollback_lines: fd.scrollback_lines,
                link_url: fd.link_url_detect,
                mouse_reporting: fd.mouse_reporting,
                mouse_hide_while_typing: fd.mouse_hide_while_typing,
                ..BehaviorConfig::default()
            },
            theme: resolved.name.clone(),
            profiles: HashMap::new(),
            active_profile: None,
            shaders: ShaderConfig::default(),
            accessibility: AccessibilityConfig {
                reduce_motion: fd.reduce_motion,
                font_scale: fd.font_scale,
                ..AccessibilityConfig::default()
            },
            shell_integration: ShellIntegrationConfig::default(),
            performance: PerformanceConfig {
                vsync: fd.vsync,
                ..PerformanceConfig::default()
            },
            environment: EnvironmentConfig::default(),
            selection: SelectionConfig::default(),
            search: SearchColorsConfig::default(),
            keybinds: KeybindConfig::default(),
            quick_terminal: QuickTerminalConfig::default(),
            tear: MadoTearConfig::default(),
            effects: MadoEffectsConfig::default(),
            vigy: MadoVigyConfig::default(),
            suggestions: SuggestionsConfig::default(),
            safra: crate::safra::SafraConfig::default(),
            janitors: JanitorsConfig::default(),
            links: MadoLinksConfig::default(),
            feedback: FeedbackConfig::default(),
        display: DisplayConfig::default(),
            notifications: NotificationsConfig::default(),
            motion: MotionConfig::default(),
        };
        if fd.scrollback_lines == 10_000 {
            c.behavior.scrollback_lines = default_scrollback();
        }
        c
    }

    #[test]
    fn prescribed_default_has_snow_off() {
        // Per the May 2026 prescribed default — snow stays OFF
        // unless the operator explicitly opts in. Matches the
        // blackmatter + stylix + nord-dark fleet aesthetic.
        let d = MadoConfig::default();
        assert!(!d.effects.snow.enabled);
    }

    #[test]
    fn links_tiers_bare_all_off_prescribed_all_on() {
        let bare = MadoLinksConfig::bare();
        assert!(!bare.enabled);
        assert!(!bare.highlight);
        assert!(!bare.open_on_click);
        assert!(!bare.pointer_cursor);

        let pres = MadoLinksConfig::prescribed();
        assert!(pres.enabled);
        assert!(pres.highlight);
        assert!(pres.open_on_click);
        assert!(pres.pointer_cursor);

        // Default == prescribed.
        assert_eq!(MadoLinksConfig::default(), MadoLinksConfig::prescribed());
    }

    #[test]
    fn links_master_config_tiers() {
        // The bare MadoConfig strips every link affordance; the prescribed
        // default turns them on.
        assert!(!MadoConfig::bare().links.enabled);
        assert!(MadoConfig::default().links.enabled);
        assert!(MadoConfig::default().links.highlight);
        assert!(MadoConfig::default().links.open_on_click);
        assert!(MadoConfig::default().links.pointer_cursor);
    }

    #[test]
    fn links_config_round_trips_through_yaml() {
        // A links section round-trips through the YAML surface (deny_unknown_fields
        // means a typo'd key is a hard parse error, not a silent default).
        let cfg = MadoLinksConfig {
            enabled: true,
            highlight: false,
            open_on_click: true,
            pointer_cursor: false,
        };
        let yaml = serde_yaml_ng::to_string(&cfg).expect("serialize links config");
        let back: MadoLinksConfig =
            serde_yaml_ng::from_str(&yaml).expect("round-trip links config");
        assert_eq!(cfg, back);
    }

    #[test]
    fn feedback_tiers_bare_all_off_prescribed_all_on() {
        let bare = FeedbackConfig::bare();
        assert!(!bare.copy_flash);
        assert!(!bare.visual_bell);
        assert!(!bare.exit_code_coloring);

        let pres = FeedbackConfig::prescribed();
        assert!(pres.copy_flash);
        assert!(pres.visual_bell);
        assert!(pres.exit_code_coloring);

        assert_eq!(FeedbackConfig::default(), FeedbackConfig::prescribed());
    }

    #[test]
    fn motion_tiers_bare_all_off_prescribed_all_on() {
        let bare = MotionConfig::bare();
        assert!(!bare.blink_ease);
        assert!(!bare.picker_animate);
        assert!(!bare.scroll_lerp);
        assert!(!bare.unfocused_dim);

        let pres = MotionConfig::prescribed();
        assert!(pres.blink_ease);
        assert!(pres.picker_animate);
        assert!(pres.scroll_lerp);
        assert!(pres.unfocused_dim);

        assert_eq!(MotionConfig::default(), MotionConfig::prescribed());
    }

    #[test]
    fn feedback_motion_master_config_tiers() {
        // The bare MadoConfig strips every flourish + easing; the
        // prescribed default turns them on.
        assert!(!MadoConfig::bare().feedback.visual_bell);
        assert!(!MadoConfig::bare().motion.unfocused_dim);
        assert!(MadoConfig::default().feedback.visual_bell);
        assert!(MadoConfig::default().feedback.copy_flash);
        assert!(MadoConfig::default().feedback.exit_code_coloring);
        assert!(MadoConfig::default().motion.blink_ease);
        assert!(MadoConfig::default().motion.unfocused_dim);
    }

    #[test]
    fn feedback_motion_round_trip_through_yaml() {
        // Both sections round-trip through the YAML surface
        // (deny_unknown_fields → a typo'd key is a hard parse error).
        let fb = FeedbackConfig {
            copy_flash: true,
            visual_bell: false,
            exit_code_coloring: true,
            exit_code_glow: true,
        };
        let yaml = serde_yaml_ng::to_string(&fb).expect("serialize feedback config");
        let back: FeedbackConfig =
            serde_yaml_ng::from_str(&yaml).expect("round-trip feedback config");
        assert_eq!(fb, back);

        let mo = MotionConfig {
            blink_ease: false,
            picker_animate: true,
            scroll_lerp: false,
            unfocused_dim: true,
        };
        let yaml = serde_yaml_ng::to_string(&mo).expect("serialize motion config");
        let back: MotionConfig =
            serde_yaml_ng::from_str(&yaml).expect("round-trip motion config");
        assert_eq!(mo, back);
    }

    #[test]
    fn prescribed_suggestions_arm_the_workflow_surface_cost_pollers_off() {
        use crate::suggest::SourceKind;
        let s = SuggestionsConfig::prescribed();
        assert!(s.enabled, "the suggestion stream is on");
        assert!(
            !s.default_enabled,
            "sources are armed by an explicit list, not a blanket default-on"
        );
        // The prescribed default arms the operator's FULL workflow surface — every
        // source degrades gracefully to an empty Vec when its dep/cred/cluster is
        // absent (suggest/source.rs contract), so arming is safe and the band fills
        // from whatever is actually reachable, live-streamed into Ctrl-S. Only the
        // three steady-cost external pollers stay opt-in so the band never spends
        // API budget the operator did not ask for.
        let armed: std::collections::BTreeSet<&str> = s
            .sources
            .iter()
            .filter(|sc| sc.enabled)
            .map(|sc| sc.kind.as_str())
            .collect();
        // EXACT-SET invariant (not a >= heuristic): prescribed arms every
        // catalogued source EXCEPT the named opt-outs — the three steady-cost
        // external pollers, plus the push-only agent lane (fed via the
        // suggest_inject MCP tool; arming a watcher for it would poll
        // nothing). A new SourceKind variant fails this test until it is
        // either armed here or added to the opt-out list — the comment can
        // never drift from the code.
        let opt_out: std::collections::BTreeSet<&str> = [
            SourceKind::AwsHealth,
            SourceKind::DatadogMonitors,
            SourceKind::CloudflareDeployments,
            SourceKind::Agent,
        ]
        .iter()
        .map(|k| k.slug())
        .collect();
        for &k in SourceKind::ALL {
            if opt_out.contains(k.slug()) {
                assert!(
                    !armed.contains(k.slug()),
                    "{} is a cost-poller and must be off by default",
                    k.slug()
                );
            } else {
                assert!(
                    armed.contains(k.slug()),
                    "{} must be armed at prescribed (or added to the opt-out set)",
                    k.slug()
                );
            }
        }
    }

    #[test]
    fn suggestion_source_overrides_merge_over_prescribed_not_replace() {
        use crate::suggest::SourceKind;
        // The exact failure this pins: a nix/yaml block supplying ONLY a
        // params override for one source (serde replaces the whole Vec) must
        // NOT disarm the other prescribed sources.
        let mut over = SuggestionSourceConfig::enable(SourceKind::JiraAssigned);
        over.params
            .insert(String::from("site"), String::from("acme.atlassian.net"));
        let cfg = SuggestionsConfig {
            sources: vec![over],
            ..SuggestionsConfig::prescribed()
        };
        let eff = cfg.effective_sources();
        let jira = eff
            .iter()
            .find(|s| s.kind == SourceKind::JiraAssigned.slug())
            .expect("jira-assigned present");
        assert_eq!(
            jira.params.get("site").map(String::as_str),
            Some("acme.atlassian.net"),
            "the override's params win for its kind"
        );
        let prescribed_armed = SuggestionsConfig::prescribed()
            .sources
            .iter()
            .filter(|s| s.enabled)
            .count();
        assert_eq!(
            eff.iter().filter(|s| s.enabled).count(),
            prescribed_armed,
            "a params-only override must not change the armed count"
        );
        // A kind unknown to the prescribed list rides along (e.g. an opt-in
        // cost poller the operator arms explicitly).
        let cfg2 = SuggestionsConfig {
            sources: vec![SuggestionSourceConfig::enable(SourceKind::AwsHealth)],
            ..SuggestionsConfig::prescribed()
        };
        assert!(
            cfg2.effective_sources()
                .iter()
                .any(|s| s.kind == SourceKind::AwsHealth.slug() && s.enabled),
            "an explicitly-armed opt-out kind is appended"
        );
        // An explicit disable override disarms exactly its kind.
        let mut off = SuggestionSourceConfig::enable(SourceKind::TodoBacklog);
        off.enabled = false;
        let cfg3 = SuggestionsConfig {
            sources: vec![off],
            ..SuggestionsConfig::prescribed()
        };
        let eff3 = cfg3.effective_sources();
        assert!(
            !eff3
                .iter()
                .find(|s| s.kind == SourceKind::TodoBacklog.slug())
                .expect("todo-backlog present")
                .enabled,
            "an explicit disable wins for its kind"
        );
        assert_eq!(
            eff3.iter().filter(|s| s.enabled).count(),
            prescribed_armed - 1,
            "only the disabled kind is disarmed"
        );
        // The escape hatch: sources_replace = true restores allow-list
        // semantics — exactly the listed sources, nothing else.
        let cfg4 = SuggestionsConfig {
            sources: vec![SuggestionSourceConfig::enable(SourceKind::JiraAssigned)],
            sources_replace: true,
            ..SuggestionsConfig::prescribed()
        };
        assert_eq!(cfg4.effective_sources().len(), 1, "replace mode is an allow-list");
    }

    /// PARITY GATE against the extracted substrate: mado's local
    /// `SuggestionsConfig::effective_sources` merge (kept local because the
    /// prescribed arm-list is baked into `prescribed()` for YAML byte-compat)
    /// must implement EXACTLY the semantics of
    /// `izumi_config::BoardConfig::effective_sources`. If either side's merge
    /// drifts (override-wins-wholesale, unknown-kind-rides-along,
    /// replace-is-an-allow-list), this fails naming the divergent entry.
    #[test]
    fn effective_sources_merge_parity_with_izumi_config() {
        use crate::suggest::SourceKind;

        fn to_entry(sc: &SuggestionSourceConfig) -> izumi_config::SourceEntry {
            let mut e = izumi_config::SourceEntry::enable(sc.kind.clone());
            e.enabled = sc.enabled;
            e.interval_secs = sc.interval_secs;
            e.max_items = sc.max_items;
            e.params = sc.params.clone();
            e
        }

        // The mado prescribed arm-list, as izumi-config sees it: the slugs of
        // the prescribed()-armed entries (izumi's merge seeds enabled=true
        // defaults from slugs; mado's prescribed entries ARE exactly
        // `enable(kind)` — asserted below so the translation stays honest).
        let prescribed = SuggestionsConfig::prescribed();
        for sc in &prescribed.sources {
            assert_eq!(
                *sc,
                SuggestionSourceConfig::enable(
                    SourceKind::from_slug(&sc.kind).expect("prescribed slug is a catalog kind")
                ),
                "prescribed entries must be plain enables for the parity translation to hold"
            );
        }
        let prescribed_slugs: Vec<&str> =
            prescribed.sources.iter().map(|s| s.kind.as_str()).collect();

        // Exercise the same shapes the merge test above pins: a params-only
        // override, an appended unknown kind, an explicit disable, and the
        // replace-mode allow-list.
        let mut params_over = SuggestionSourceConfig::enable(SourceKind::JiraAssigned);
        params_over
            .params
            .insert(String::from("site"), String::from("acme.atlassian.net"));
        let mut disable = SuggestionSourceConfig::enable(SourceKind::TodoBacklog);
        disable.enabled = false;
        let cases: Vec<SuggestionsConfig> = vec![
            SuggestionsConfig::prescribed(),
            SuggestionsConfig {
                sources: vec![
                    params_over,
                    SuggestionSourceConfig::enable(SourceKind::AwsHealth),
                    disable,
                ],
                ..SuggestionsConfig::prescribed()
            },
            SuggestionsConfig {
                sources: vec![SuggestionSourceConfig::enable(SourceKind::JiraAssigned)],
                sources_replace: true,
                ..SuggestionsConfig::prescribed()
            },
        ];
        for (i, cfg) in cases.iter().enumerate() {
            let board = izumi_config::BoardConfig {
                sources: cfg.sources.iter().map(to_entry).collect(),
                sources_replace: cfg.sources_replace,
                ..izumi_config::BoardConfig::default()
            };
            let ours = cfg.effective_sources();
            let theirs = board.effective_sources(&prescribed_slugs);
            assert_eq!(ours.len(), theirs.len(), "case {i}: merged length diverged");
            for (m, z) in ours.iter().zip(theirs.iter()) {
                assert_eq!(m.kind, z.kind, "case {i}: kind order diverged");
                assert_eq!(m.enabled, z.enabled, "case {i}: enabled diverged for {}", m.kind);
                assert_eq!(
                    m.interval_secs, z.interval_secs,
                    "case {i}: interval diverged for {}",
                    m.kind
                );
                assert_eq!(
                    m.max_items, z.max_items,
                    "case {i}: max_items diverged for {}",
                    m.kind
                );
                assert_eq!(m.params, z.params, "case {i}: params diverged for {}", m.kind);
            }
        }
    }

    /// Every effects knob round-trips through YAML and the static
    /// params mirror the engawa catalog's reference defaults — the
    /// config IS a projection of the catalog's Params surface, so a
    /// drifted default would silently change what `enabled = true`
    /// buys. Matrix-style: failures aggregate, one assert.
    #[test]
    fn effects_config_defaults_mirror_the_catalog() {
        // Expected values come from the TYPED SOURCE — the engawa
        // catalog Params defaults — never re-typed literals. The
        // prior hand-duplicated table stayed green if the catalog
        // retuned a default upstream (mado's config default AND the
        // test literal both kept the stale value), which is exactly
        // the rot the test name promises to prevent (M3 review
        // 2026-06-12). Now a catalog default change fails this build
        // mechanically until the config default_* fns follow.
        let crt = engawa_wgpu::catalog::crt::CrtParams::default();
        let scan = engawa_wgpu::catalog::scanlines::ScanlinesParams::default();
        let bloom = engawa_wgpu::catalog::bloom::BloomParams::default();
        let glow = engawa_wgpu::catalog::glow_on_bell::GlowOnBellParams::default();
        let grain = engawa_wgpu::catalog::grain::GrainParams::default();

        let e = MadoEffectsConfig::default();
        let rows: &[(&str, f32, f32)] = &[
            ("crt.curvature", e.crt.curvature, crt.curvature),
            ("crt.vignette", e.crt.vignette, crt.vignette),
            ("crt.aberration", e.crt.aberration, crt.aberration),
            ("scanlines.period_px", e.scanlines.period_px, scan.period_px),
            ("scanlines.intensity", e.scanlines.intensity, scan.intensity),
            ("bloom.threshold", e.bloom.threshold, bloom.threshold),
            ("bloom.intensity", e.bloom.intensity, bloom.intensity),
            ("bloom.radius_px", e.bloom.radius_px, bloom.radius_px),
            ("glow_on_bell.radius_px", e.glow_on_bell.radius_px, glow.radius_px),
            ("grain.opacity", e.grain.opacity, grain.opacity),
        ];
        let mut failures = Vec::new();
        for (name, got, want) in rows {
            if (got - want).abs() > f32::EPSILON {
                failures.push(format!("{name}: got {got}, want {want}"));
            }
        }
        // Round-trip: a YAML config naming only the toggles keeps
        // every other knob at the defaults (serde(default) per field).
        let yaml = "crt:\n  enabled: true\nsnow:\n  enabled: true\n";
        match serde_yaml_ng::from_str::<MadoEffectsConfig>(yaml) {
            Ok(parsed) => {
                if !parsed.crt.enabled || !parsed.snow.enabled {
                    failures.push("partial YAML did not enable crt+snow".into());
                }
                if (parsed.crt.curvature - crt.curvature).abs() > f32::EPSILON {
                    failures.push("partial YAML lost crt.curvature default".into());
                }
            }
            Err(err) => failures.push(format!("partial YAML failed to parse: {err}")),
        }
        assert!(
            failures.is_empty(),
            "{} effects-config rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn macos_window_default_is_just_the_terminal() {
        // The "just the terminal" contract: no macOS-native OS tab
        // strip (mado owns sessions/panes/windows via tear), a flush
        // titlebar, and a dark appearance so the chrome disappears into
        // the Nord content. Holds in BOTH the prescribed-default and the
        // chromeless bare tier.
        let d = MadoConfig::default().window.macos;
        assert!(!d.native_tabs, "native macOS tab bar must be OFF by default");
        assert_eq!(d.titlebar, TitlebarStyle::Flush);
        assert_eq!(d.appearance, WindowAppearance::Dark);

        let b = MadoConfig::bare().window.macos;
        assert!(!b.native_tabs, "bare tier is chromeless too");
        assert_eq!(b.titlebar, TitlebarStyle::Flush);
        assert_eq!(b.appearance, WindowAppearance::Dark);
    }

    #[test]
    fn macos_window_knobs_round_trip_through_yaml() {
        // Every chrome axis is operator-configurable via shikumi YAML.
        // Prove all three parse from `window.macos.*` so flipping them in
        // ~/.config/mado/mado.yaml actually reaches apply_native_styling.
        let yaml = "\
window:
  macos:
    native_tabs: true
    titlebar: native
    appearance: light
";
        let cfg: MadoConfig = serde_yaml_ng::from_str(yaml).expect("yaml parses");
        assert!(cfg.window.macos.native_tabs);
        assert_eq!(cfg.window.macos.titlebar, TitlebarStyle::Native);
        assert_eq!(cfg.window.macos.appearance, WindowAppearance::Light);
    }

    #[test]
    fn titlebar_style_tokens_and_config_round_trip_via_serde() {
        for (token, style) in [
            ("flush", TitlebarStyle::Flush),
            ("overlay", TitlebarStyle::Overlay),
            ("native", TitlebarStyle::Native),
        ] {
            // The operator-facing YAML token is pinned by serializing
            // the typed value itself — never format!()-composed YAML.
            let rendered = serde_yaml_ng::to_string(&style).expect("style serializes");
            assert_eq!(rendered.trim(), token, "operator token for {style:?}");

            // The nested `window.macos.titlebar` path round-trips
            // through the full typed config.
            let mut cfg = MadoConfig::default();
            cfg.window.macos.titlebar = style;
            let doc = serde_yaml_ng::to_string(&cfg).expect("config serializes");
            let back: MadoConfig = serde_yaml_ng::from_str(&doc).expect("config parses");
            assert_eq!(back.window.macos.titlebar, style, "token {token:?}");
        }
    }

    #[test]
    fn tear_runtime_default_is_embedded_for_zero_config_speed() {
        // The 90% case (single mado window, no overlay, no remote
        // ssh-mux) gets ghostty-class latency with zero operator
        // action. Multi-attach scenarios opt into Daemon via config.
        let cfg = MadoTearConfig::default();
        assert_eq!(cfg.runtime, TearRuntime::Embedded);
    }

    #[test]
    fn tear_runtime_parses_embedded_from_yaml() {
        let yaml = "mode: auto\nruntime: embedded\n";
        let cfg: MadoTearConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.runtime, TearRuntime::Embedded);
    }

    #[test]
    fn tear_runtime_round_trips_via_serde() {
        for &rt in &[TearRuntime::Embedded, TearRuntime::Daemon] {
            let s = serde_yaml_ng::to_string(&rt).unwrap();
            let back: TearRuntime = serde_yaml_ng::from_str(&s).unwrap();
            assert_eq!(rt, back);
        }
    }

    #[test]
    fn test_default_config_values() {
        let config = MadoConfig::default();
        // Ghostty-aligned fleet font (FleetDefaults::prescribed): the
        // non-Mono Nerd family, same-family synthesized-slant italics,
        // size 13, 1.65 cell rhythm.
        assert_eq!(config.font_family, "JetBrainsMono Nerd Font");
        assert_eq!(config.font_italic, "JetBrainsMono Nerd Font");
        assert_eq!(config.font_size, 13.0);
        assert!((config.line_height - 1.65).abs() < 0.001);
        // Prescribed theme is now the fleet theme (Vellum), derived
        // from FleetTheme::prescribed_default() — not the legacy "nord".
        assert_eq!(config.theme, "vellum");
        assert_eq!(
            config.theme,
            ishou_tokens::FleetTheme::prescribed_default().resolve().name,
        );
        assert!(config.active_profile.is_none());
        // Window dims are auto-detected from the focused display
        // (macOS NSScreen); range-asserted because the exact value
        // depends on the test machine's monitor.
        assert!(config.window.width >= 800 && config.window.width <= 1600);
        assert!(config.window.height >= 600 && config.window.height <= 1100);
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
        // The prescribed config now DERIVES its appearance + cursor
        // colours from the fleet theme (Vellum) via from_fleet — not
        // the legacy Nord hexes. Asserted by reference to the resolved
        // theme so a fleet rebrand propagates on the next compile.
        let resolved = ishou_tokens::FleetTheme::prescribed_default().resolve();
        assert_eq!(config.appearance.background, "#16140E"); // night0
        assert_eq!(config.appearance.foreground, "#E2DBC8"); // snow1
        assert_eq!(config.appearance.background, resolved.background);
        assert_eq!(config.appearance.foreground, resolved.foreground);
        assert_eq!(config.appearance.opacity, 1.0);
        assert!(!config.appearance.bold_is_bright);
        // The Vellum grid-cell contrast floor (§5 = 3.0), derived from
        // the resolved theme's OWN surfaces (not a hand-pinned 3.0), so a
        // Vellum re-tune propagates on the next compile.
        let surfaces = ishou_tokens::VellumPalette::vellum().surfaces();
        assert!((config.appearance.minimum_contrast - surfaces.minimum_contrast).abs() < 0.001);
        assert!((config.appearance.minimum_contrast - 3.0).abs() < 0.001);
        assert!(!config.appearance.background_blur);
        assert!(config.appearance.unfocused_split_fill.is_none());
        assert_eq!(config.cursor.style, CursorStyle::Block);
        assert!(config.cursor.blink);
        // Blink rate now follows the fleet default (500ms) via from_fleet.
        assert_eq!(config.cursor.blink_rate_ms, 500);
        assert_eq!(
            config.cursor.blink_rate_ms,
            ishou_tokens::FleetDefaults::prescribed().cursor_blink_rate_ms,
        );
        assert_eq!(config.cursor.color, "#ADD7A3"); // green_bright
        assert_eq!(config.cursor.color, resolved.cursor);
        assert!((config.cursor.opacity - 1.0).abs() < 0.001);
        assert!(config.cursor.text_color.is_none());
        assert!(!config.cursor.click_to_move);
        // Operator-facing default: "never lose anything"; host
        // RAM is the only ceiling. VecDeque grows on demand.
        assert_eq!(config.behavior.scrollback_lines, usize::MAX);
        // Muscle-memory default: highlight → clipboard, no chord.
        assert!(config.behavior.copy_on_select);
        assert!(config.behavior.deselect_on_copy);
        assert!(!config.behavior.confirm_close);
        assert!(config.behavior.mouse_hide_while_typing);
        assert_eq!(config.behavior.mouse_scroll_multiplier, 2);
        assert!(!config.behavior.wait_after_command);
        assert!(config.behavior.link_url);
        assert!(config.behavior.mouse_reporting);
        assert_eq!(config.behavior.mouse_shift_capture, MouseShiftCapture::False);
        // The kitty/ghostty default: column resizes REWRAP the
        // primary grid (M2).
        assert!(config.behavior.reflow_on_resize);
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

    /// theme-fidelity-4: the prescribed `minimum_contrast` is the Vellum
    /// §5 grid-cell floor (3.0), DERIVED from the theme's own surfaces —
    /// NOT the curated 1.0 app default and NOT a hand-pinned 3.0. A
    /// Vellum re-tune of the floor propagates here on the next compile.
    #[test]
    fn prescribed_minimum_contrast_is_the_vellum_floor() {
        // `MadoConfig::default()` IS the prescribed config (it delegates
        // to the `FleetThemedConfig::from_fleet` path); using it here
        // avoids pulling the `TieredConfig` trait into test scope.
        let config = MadoConfig::default();
        let surfaces = ishou_tokens::VellumPalette::vellum().surfaces();
        assert!(
            (config.appearance.minimum_contrast - surfaces.minimum_contrast).abs() < 0.001,
            "prescribed minimum_contrast must equal the Vellum surfaces floor"
        );
        assert!((config.appearance.minimum_contrast - 3.0).abs() < 0.001);
        // And it is NOT the curated app default (1.0) — the fleet floor
        // genuinely raised it.
        assert!((default_minimum_contrast() - 1.0).abs() < 0.001);
        assert!(config.appearance.minimum_contrast > default_minimum_contrast());
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

    /// mechanical-audit-0 (review 2026-06-12): a profile whose `window`
    /// block flips `inherit_working_directory` must resolve through
    /// `with_active_profile()` — the single resolution point the GUI,
    /// hot-reload, AND `mado mcp` all share. `with_profile` replaces
    /// `config.window` wholesale when the profile sets a window block,
    /// so a config carrying the profile but NOT resolving it drops the
    /// knob. This pins that the resolution honors it.
    #[test]
    fn active_profile_window_block_resolves_inherit_cwd() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "inherit".to_string(),
            ProfileConfig {
                window: Some(WindowConfig {
                    inherit_working_directory: true,
                    ..WindowConfig::default()
                }),
                ..ProfileConfig::default()
            },
        );
        // Base config has the knob OFF; the active profile flips it ON.
        let mut base = MadoConfig {
            profiles,
            active_profile: Some("inherit".into()),
            ..MadoConfig::default()
        };
        base.window.inherit_working_directory = false;
        // UNRESOLVED config still reads the base value (the bug).
        assert!(!base.window.inherit_working_directory);
        // RESOLVED through the shared point the MCP path now uses.
        let resolved = base.with_active_profile();
        assert!(
            resolved.window.inherit_working_directory,
            "active profile's window block must flip the knob via with_active_profile"
        );
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
        // Prescribed default: frostmourne (mado's official curated
        // shell — ships skim+atuin+Ctrl-R out of the box). Operators
        // who want $SHELL override via mado.yaml.
        let s = ShellConfig::default();
        assert_eq!(s.command.as_deref(), Some("frostmourne"));
        assert!(s.args.is_empty());
    }

    #[test]
    fn test_appearance_config_defaults() {
        let a = AppearanceConfig::default();
        // The appearance fallbacks now DERIVE from the prescribed fleet
        // theme's BORN tokens (Vellum night0 / snow1), not a legacy
        // Nord hex. Asserted against the resolved theme by reference so
        // a fleet rebrand propagates here on the next compile.
        let resolved = ishou_tokens::FleetTheme::prescribed_default().resolve();
        assert_eq!(a.background, "#16140E"); // Vellum night0
        assert_eq!(a.foreground, "#E2DBC8"); // Vellum snow1
        assert_eq!(a.background, resolved.background);
        assert_eq!(a.foreground, resolved.foreground);
        assert_eq!(a.opacity, 1.0);
        assert!(!a.bold_is_bright);
    }

    #[test]
    fn test_cursor_config_defaults() {
        let c = CursorConfig::default();
        assert_eq!(c.style, CursorStyle::Block);
        assert!(c.blink);
        assert_eq!(c.blink_rate_ms, 530);
        // Cursor colour now derives from the prescribed theme's cursor
        // (Vellum green_bright), not Nord snow.
        let resolved = ishou_tokens::FleetTheme::prescribed_default().resolve();
        assert_eq!(c.color, "#ADD7A3"); // Vellum green_bright
        assert_eq!(c.color, resolved.cursor);
    }

    #[test]
    fn test_behavior_config_defaults() {
        let b = BehaviorConfig::default();
        assert_eq!(b.scrollback_lines, usize::MAX);
        // Muscle-memory default: highlight → clipboard, no chord.
        assert!(b.copy_on_select);
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
        // The profile doesn't override theme, so the prescribed fleet
        // theme (Vellum) carries through unchanged.
        assert_eq!(applied.theme, "vellum");
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

    /// `window.inherit_working_directory` is LIVE (M4 stage 2) —
    /// boot_spawn_cwd is the knob's boot-time consumer, and every
    /// (knob, explicit-wd) combination resolves per the documented
    /// precedence. This is the dead-knob invariant for this field:
    /// the knob's tier values (bare=false, prescribed=true) are
    /// asserted in the tier tests above; THIS pins that flipping it
    /// changes behavior.
    #[test]
    fn inherit_working_directory_resolves_boot_spawn_cwd() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let pin = PathBuf::from("/pinned/workdir");
        let mut failures = Vec::new();
        let rows: &[(bool, Option<PathBuf>, Option<PathBuf>, &str)] = &[
            (true, None, None, "knob on → None (inherit mado's process cwd)"),
            (false, None, home.clone(), "knob off → $HOME neutral default"),
            (true, Some(pin.clone()), Some(pin.clone()), "explicit wd beats knob on"),
            (false, Some(pin.clone()), Some(pin.clone()), "explicit wd beats knob off"),
        ];
        for (knob, wd, expected, why) in rows {
            let mut config = MadoConfig::default();
            config.window.inherit_working_directory = *knob;
            config.environment.working_directory.clone_from(wd);
            let got = config.boot_spawn_cwd();
            if got != *expected {
                failures.push(format!("{why}: got {got:?}, want {expected:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} boot_spawn_cwd rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
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

    #[test]
    fn bell_sound_names_map_to_nssound_or_beep() {
        assert_eq!(BellSound::Beep.sound_name(), None); // → NSBeep
        assert_eq!(BellSound::Basso.sound_name(), Some("Basso"));
        assert_eq!(BellSound::Glass.sound_name(), Some("Glass"));
        assert_eq!(BellSound::default(), BellSound::Beep);
    }

    #[test]
    fn bell_config_audible_defaults_off_and_round_trips() {
        // Prescribed: audio + banner off (bells are frequent; opt-in).
        let p = BellNotifyConfig::prescribed();
        assert!(!p.audible);
        assert_eq!(p.sound, BellSound::Beep);
        // A configured audible bell round-trips through yaml, unknown-field-strict.
        let yaml = "audible: true\nsound: glass\nnotify: false\nurgency: normal\n";
        let cfg: BellNotifyConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.audible);
        assert_eq!(cfg.sound, BellSound::Glass);
        assert_eq!(cfg.sound.sound_name(), Some("Glass"));
    }
}
