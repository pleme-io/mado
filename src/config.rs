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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_padding")]
    pub padding: u32,
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
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
    pub theme: Option<String>,
    pub appearance: Option<AppearanceConfig>,
    pub cursor: Option<CursorConfig>,
    pub shell: Option<ShellConfig>,
    pub behavior: Option<BehaviorConfig>,
    pub performance: Option<PerformanceConfig>,
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
        config
    }
}

// Defaults

impl Default for MadoConfig {
    fn default() -> Self {
        Self {
            font_family: default_font_family(),
            font_size: default_font_size(),
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
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            padding: default_padding(),
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
        assert_eq!(config.appearance.background, "#2e3440");
        assert_eq!(config.appearance.foreground, "#eceff4");
        assert_eq!(config.appearance.opacity, 1.0);
        assert!(!config.appearance.bold_is_bright);
        assert_eq!(config.cursor.style, CursorStyle::Block);
        assert!(config.cursor.blink);
        assert_eq!(config.cursor.blink_rate_ms, 530);
        assert_eq!(config.cursor.color, "#eceff4");
        assert_eq!(config.behavior.scrollback_lines, 10_000);
        assert!(!config.behavior.copy_on_select);
        assert!(!config.behavior.confirm_close);
        assert!(config.behavior.mouse_hide_while_typing);
        assert_eq!(config.behavior.mouse_scroll_multiplier, 2);
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
                appearance: None,
                cursor: None,
                shell: None,
                behavior: None,
                performance: None,
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
        for style in [CursorStyle::Block, CursorStyle::Bar, CursorStyle::Underline] {
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
        assert!(p.theme.is_none());
        assert!(p.appearance.is_none());
        assert!(p.cursor.is_none());
        assert!(p.shell.is_none());
        assert!(p.behavior.is_none());
        assert!(p.performance.is_none());
    }

    #[test]
    fn test_config_with_profile_font_override() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "large".to_string(),
            ProfileConfig {
                font_family: Some("Monaco".into()),
                font_size: Some(18.0),
                theme: None,
                appearance: None,
                cursor: None,
                shell: None,
                behavior: None,
                performance: None,
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
                font_family: None,
                font_size: None,
                theme: Some("solarized-light".into()),
                appearance: None,
                cursor: None,
                shell: None,
                behavior: None,
                performance: None,
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
                appearance: None,
                cursor: None,
                shell: None,
                behavior: None,
                performance: None,
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
                font_family: None,
                font_size: None,
                theme: None,
                appearance: None,
                cursor: None,
                shell: None,
                behavior: None,
                performance: Some(PerformanceConfig {
                    vsync: false,
                    target_fps: 240,
                }),
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
}
