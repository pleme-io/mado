//! Typed `Info.plist` — a Rust struct serialized to an XML plist via the
//! `plist` crate, NOT `plutil` / `PlistBuddy` / a shell heredoc.
//!
//! Generalized from gaveta-release's `plist_model` so BOTH a menu-bar app
//! (gaveta: `LSUIElement = true`) and a normal windowed app (mado: no
//! `LSUIElement`, a real Dock icon) render from the same typed surface. The
//! `LSUIElement` key is `Option<bool>` and is SKIPPED entirely when `None`, so a
//! windowed app's plist never carries it — the bright-line difference between
//! the two app classes is one typed field, not two diverged code paths.

use std::io::Write;

use serde::Serialize;

use crate::error::ReleaseError;

/// The `Info.plist` for an app bundle, as a typed value.
///
/// Construct via [`InfoPlist::from_spec`] — it maps the consumer-supplied
/// [`crate::spec::AppSpec`] into the CFBundle*/LS*/NS* key set verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct InfoPlist {
    #[serde(rename = "CFBundleIdentifier")]
    pub bundle_identifier: String,
    #[serde(rename = "CFBundleName")]
    pub bundle_name: String,
    #[serde(rename = "CFBundleDisplayName")]
    pub bundle_display_name: String,
    #[serde(rename = "CFBundleExecutable")]
    pub bundle_executable: String,
    #[serde(rename = "CFBundlePackageType")]
    pub bundle_package_type: String,
    #[serde(rename = "CFBundleShortVersionString")]
    pub bundle_short_version_string: String,
    #[serde(rename = "CFBundleVersion")]
    pub bundle_version: String,
    #[serde(rename = "CFBundleInfoDictionaryVersion")]
    pub bundle_info_dictionary_version: String,
    #[serde(rename = "CFBundleIconFile")]
    pub bundle_icon_file: String,
    /// `true` for a menu-bar / agent app (gaveta); SKIPPED for a windowed app
    /// with a Dock icon (mado). `None` ⇒ the key is absent from the plist.
    #[serde(rename = "LSUIElement", skip_serializing_if = "Option::is_none")]
    pub ls_ui_element: Option<bool>,
    #[serde(rename = "LSMinimumSystemVersion")]
    pub ls_minimum_system_version: String,
    #[serde(rename = "NSHighResolutionCapable")]
    pub ns_high_resolution_capable: bool,
    #[serde(rename = "NSHumanReadableCopyright")]
    pub ns_human_readable_copyright: String,
}

impl InfoPlist {
    /// Serialize the typed plist to XML bytes (the `plist` crate is the typed
    /// AST renderer — TYPED EMISSION compliant).
    ///
    /// # Errors
    /// Returns [`ReleaseError::Plist`] / [`ReleaseError::Io`] on a serialization
    /// or buffer write failure.
    pub fn to_xml_bytes(&self) -> Result<Vec<u8>, ReleaseError> {
        let mut buf: Vec<u8> = Vec::new();
        plist::to_writer_xml(&mut buf, self)?;
        // plist::to_writer_xml omits the trailing newline some linters expect.
        buf.write_all(b"\n").map_err(|e| ReleaseError::Io {
            path: "Info.plist".into(),
            source: e,
        })?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use crate::spec::AppSpec;

    fn windowed() -> AppSpec {
        AppSpec::new("Mado", "io.pleme.mado", "mado")
            .version("0.1.9")
            .min_system_version("11.0")
    }

    #[test]
    fn windowed_plist_omits_lsuielement() {
        let p = windowed().info_plist();
        let xml = String::from_utf8(p.to_xml_bytes().unwrap()).unwrap();
        // A normal windowed app must NOT carry LSUIElement — it has a Dock icon.
        assert!(!xml.contains("LSUIElement"), "windowed app leaked LSUIElement\n{xml}");
        for key in [
            "CFBundleIdentifier",
            "CFBundleName",
            "CFBundleExecutable",
            "CFBundlePackageType",
            "CFBundleShortVersionString",
            "CFBundleIconFile",
            "LSMinimumSystemVersion",
            "NSHighResolutionCapable",
        ] {
            assert!(xml.contains(key), "plist missing key {key}\n{xml}");
        }
        assert!(xml.contains("io.pleme.mado"));
        assert!(xml.contains("Mado"));
        assert!(xml.contains("APPL"));
        assert!(xml.contains("0.1.9"));
        assert!(xml.contains("<plist version="));
    }

    #[test]
    fn menubar_plist_carries_lsuielement_true() {
        let spec = AppSpec::new("Gaveta", "io.pleme.gaveta", "GavetaApp")
            .version("0.1.0")
            .ls_ui_element(true);
        let xml = String::from_utf8(spec.info_plist().to_xml_bytes().unwrap()).unwrap();
        assert!(xml.contains("LSUIElement"));
        // A real <true/>, not the string "true".
        assert!(xml.contains("<true/>"));
    }
}
