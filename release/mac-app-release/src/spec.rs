//! `AppSpec` — the typed, consumer-supplied description of ONE macOS app.
//!
//! This is the config surface the whole shared primitive is driven by. A
//! consumer (mado-release, gaveta-release) builds an `AppSpec` and hands it to
//! [`crate::assemble::build_dmg`]; the layout, the Info.plist, the icon, the
//! signing, and the packaging all derive from it. The two app classes —
//! windowed (mado) vs menu-bar (gaveta) — differ by exactly one typed field
//! (`ls_ui_element`), never by a forked code path.

use std::path::PathBuf;

use crate::plist_model::InfoPlist;

/// The icon source for the bundle's `.icns`.
#[derive(Debug, Clone)]
pub enum IconSource {
    /// A path to a 1024×1024 source SVG — rasterized via `resvg` to the 10
    /// canonical iconset sizes, then assembled with `iconutil` (the substrate
    /// `mkDarwinAppBundle` pipeline, reused).
    Svg(PathBuf),
    /// A path to an already-built `.icns` (copied verbatim into Resources/).
    Icns(PathBuf),
}

/// An extra binary to place inside the bundle (e.g. a CLI seam or a daemon).
#[derive(Debug, Clone)]
pub struct ExtraBinary {
    /// The built binary on disk.
    pub src: PathBuf,
    /// Where it lands relative to `Contents/` (e.g. `MacOS/gaveta-client`,
    /// `Resources/bin/gaveta-syncd`).
    pub dest_rel: String,
}

/// A typed description of one macOS app to bundle, sign, and package.
///
/// Fields beyond the three required ones have sensible defaults; the builder
/// methods set the optional ones. This is the ONLY thing a consumer must build.
#[derive(Debug, Clone)]
pub struct AppSpec {
    /// Display name → `<AppName>.app` (e.g. `Mado` → `Mado.app`).
    pub app_name: String,
    /// `CFBundleIdentifier` (e.g. `io.pleme.mado`).
    pub bundle_id: String,
    /// The primary executable's basename inside `Contents/MacOS/`.
    pub executable: String,
    /// `CFBundleShortVersionString`.
    pub version: String,
    /// `LSMinimumSystemVersion`.
    pub min_system_version: String,
    /// `NSHumanReadableCopyright`.
    pub copyright: String,
    /// `LSUIElement` — `Some(true)` for a menu-bar/agent app (no Dock icon),
    /// `None` (the default) for a normal windowed app WITH a Dock icon.
    pub ls_ui_element: Option<bool>,
    /// The icon source (SVG or pre-built `.icns`). `None` ⇒ no icon (the
    /// assemble step errors if it needs one).
    pub icon: Option<IconSource>,
    /// Extra binaries to place inside the bundle (CLI seams, daemons).
    pub extra_binaries: Vec<ExtraBinary>,
}

impl AppSpec {
    /// Start a spec for `app_name` / `bundle_id` / `executable` with windowed
    /// defaults (NO `LSUIElement`, a real Dock icon).
    #[must_use]
    pub fn new(
        app_name: impl Into<String>,
        bundle_id: impl Into<String>,
        executable: impl Into<String>,
    ) -> Self {
        Self {
            app_name: app_name.into(),
            bundle_id: bundle_id.into(),
            executable: executable.into(),
            version: "0.1.0".to_owned(),
            min_system_version: "11.0".to_owned(),
            copyright: "Copyright © pleme-io. MIT licensed.".to_owned(),
            ls_ui_element: None,
            icon: None,
            extra_binaries: Vec::new(),
        }
    }

    /// Set the bundle version (`CFBundleShortVersionString`).
    #[must_use]
    pub fn version(mut self, v: impl Into<String>) -> Self {
        self.version = v.into();
        self
    }

    /// Set `LSMinimumSystemVersion`.
    #[must_use]
    pub fn min_system_version(mut self, v: impl Into<String>) -> Self {
        self.min_system_version = v.into();
        self
    }

    /// Set `NSHumanReadableCopyright`.
    #[must_use]
    pub fn copyright(mut self, v: impl Into<String>) -> Self {
        self.copyright = v.into();
        self
    }

    /// Mark this a menu-bar / agent app (`LSUIElement = <bool>`). A windowed app
    /// NEVER calls this — `None` keeps the key out of the plist entirely.
    #[must_use]
    pub fn ls_ui_element(mut self, on: bool) -> Self {
        self.ls_ui_element = Some(on);
        self
    }

    /// Use an SVG icon source.
    #[must_use]
    pub fn icon_svg(mut self, path: impl Into<PathBuf>) -> Self {
        self.icon = Some(IconSource::Svg(path.into()));
        self
    }

    /// Use a pre-built `.icns` icon source.
    #[must_use]
    pub fn icon_icns(mut self, path: impl Into<PathBuf>) -> Self {
        self.icon = Some(IconSource::Icns(path.into()));
        self
    }

    /// Add an extra binary at `dest_rel` (relative to `Contents/`).
    #[must_use]
    pub fn extra_binary(mut self, src: impl Into<PathBuf>, dest_rel: impl Into<String>) -> Self {
        self.extra_binaries.push(ExtraBinary {
            src: src.into(),
            dest_rel: dest_rel.into(),
        });
        self
    }

    /// `<AppName>.app` — the bundle directory name (kept out of `format!`).
    #[must_use]
    pub fn bundle_dir_name(&self) -> String {
        let mut s = self.app_name.clone();
        s.push_str(".app");
        s
    }

    /// The `<AppName>.icns` filename referenced by `CFBundleIconFile`.
    #[must_use]
    pub fn icns_name(&self) -> String {
        let mut s = self.app_name.clone();
        s.push_str(".icns");
        s
    }

    /// `<AppName>-<version>-<arch>.<ext>` — the typed artifact filename.
    #[must_use]
    pub fn artifact_name(&self, suffix: &str, ext: &str) -> String {
        let mut s = self.app_name.clone();
        s.push('-');
        s.push_str(&self.version);
        s.push('-');
        s.push_str(suffix);
        s.push('.');
        s.push_str(ext);
        s
    }

    /// Build the typed [`InfoPlist`] for this spec.
    #[must_use]
    pub fn info_plist(&self) -> InfoPlist {
        InfoPlist {
            bundle_identifier: self.bundle_id.clone(),
            bundle_name: self.app_name.clone(),
            bundle_display_name: self.app_name.clone(),
            bundle_executable: self.executable.clone(),
            bundle_package_type: "APPL".to_owned(),
            bundle_short_version_string: self.version.clone(),
            bundle_version: "1".to_owned(),
            bundle_info_dictionary_version: "6.0".to_owned(),
            bundle_icon_file: self.icns_name(),
            ls_ui_element: self.ls_ui_element,
            ls_minimum_system_version: self.min_system_version.clone(),
            ns_high_resolution_capable: true,
            ns_human_readable_copyright: self.copyright.clone(),
        }
    }
}

/// Map the host arch to the artifact suffix (`arm64` / `x86_64`).
#[must_use]
pub fn host_arch_suffix() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "x86_64"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names() {
        let spec = AppSpec::new("Mado", "io.pleme.mado", "mado").version("0.1.9");
        assert_eq!(spec.artifact_name("arm64", "dmg"), "Mado-0.1.9-arm64.dmg");
        assert_eq!(spec.artifact_name("x86_64", "zip"), "Mado-0.1.9-x86_64.zip");
    }

    #[test]
    fn bundle_and_icns_names() {
        let spec = AppSpec::new("Mado", "io.pleme.mado", "mado");
        assert_eq!(spec.bundle_dir_name(), "Mado.app");
        assert_eq!(spec.icns_name(), "Mado.icns");
    }

    #[test]
    fn windowed_default_has_no_lsuielement() {
        let spec = AppSpec::new("Mado", "io.pleme.mado", "mado");
        assert_eq!(spec.ls_ui_element, None);
    }
}
