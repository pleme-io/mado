use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MadoConfig {
    #[serde(default = "default_font_family")]
    pub font_family: String,
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
    #[serde(default = "default_true")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    #[serde(default = "default_vsync")]
    pub vsync: bool,
    #[serde(default = "default_target_fps")]
    pub target_fps: u32,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            vsync: default_vsync(),
            target_fps: default_target_fps(),
        }
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
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            padding: default_padding(),
            decorations: true,
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
    "JetBrains Mono".into()
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
    8
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
    10_000
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
fn default_target_fps() -> u32 {
    120
}
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
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values() {
        let config = MadoConfig::default();
        assert_eq!(config.font_family, "JetBrains Mono");
        assert_eq!(config.font_size, 14.0);
        assert_eq!(config.theme, "nord");
        assert!(config.active_profile.is_none());
        assert_eq!(config.window.width, 1200);
        assert_eq!(config.window.height, 800);
        assert_eq!(config.window.padding, 8);
        assert!(config.window.decorations);
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
        assert_eq!(config.behavior.scrollback_lines, 10_000);
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
        assert_eq!(config.performance.target_fps, 120);
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
        let config: MadoConfig = serde_yaml::from_str(yaml).unwrap();
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
        assert_eq!(w.padding, 8);
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
        assert_eq!(b.scrollback_lines, 10_000);
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
        assert_eq!(p.target_fps, 120);
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
                    target_fps: 240,
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
        assert_eq!(applied.performance.target_fps, 240);
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
        let config: MadoConfig = serde_yaml::from_str(yaml).unwrap();
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
        let f: FontConfig = serde_yaml::from_str(yaml).unwrap();
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
        let s: SelectionConfig = serde_yaml::from_str(yaml).unwrap();
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
        let s: SearchColorsConfig = serde_yaml::from_str(yaml).unwrap();
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
        let k: KeybindConfig = serde_yaml::from_str(yaml).unwrap();
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
        assert!(w.decorations);
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
        let e: EnvironmentConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: MadoConfig = serde_yaml::from_str(yaml).unwrap();
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
        let qt: QuickTerminalConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(qt.enabled);
        assert_eq!(qt.edge, QuickTerminalEdge::Bottom);
        assert!((qt.size_fraction - 0.35).abs() < 1e-6);
        assert_eq!(qt.hotkey, "cmd+`");
        // Non-specified fields fall back to defaults.
        assert_eq!(qt.animation_ms, 150);
        assert!(qt.autohide_on_blur);
    }
}
