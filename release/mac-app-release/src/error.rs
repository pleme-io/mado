//! Typed error surface for the shared mac-app-release primitive.
//!
//! Every fallible boundary returns a `ReleaseError` so failures are typed +
//! surfaced (no `echo … >&2; exit 1` shell error handling). Consumer-specific
//! publish failures live in the consumer's own error type, never here.

use std::path::PathBuf;
use std::process::ExitStatus;

/// All the ways an app-bundle / DMG step can fail, typed.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// A wrapped Mac-native subprocess exited non-zero.
    #[error("`{program}` exited with {status} (args: {args:?})")]
    Subprocess {
        program: String,
        status: ExitStatus,
        args: Vec<String>,
    },

    /// A wrapped subprocess could not be spawned at all (tool missing on host).
    #[error("failed to spawn `{program}` (is it on PATH? codesign/hdiutil/iconutil come from Xcode CLT): {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    /// A built binary the bundle requires is absent or not a file.
    #[error("expected built binary is missing: {0}")]
    MissingBinary(PathBuf),

    /// This tool only runs on macOS (needs Xcode CLT / codesign / hdiutil).
    #[error("the macOS app/DMG pipeline only runs on macOS (needs codesign/hdiutil/iconutil)")]
    NotMacos,

    /// A filesystem operation failed, with the path that triggered it.
    #[error("filesystem op failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The Info.plist could not be serialized.
    #[error("Info.plist serialization failed: {0}")]
    Plist(#[from] plist::Error),

    /// The icon PNG could not be written.
    #[error("icon PNG generation failed: {0}")]
    Icon(#[from] image::ImageError),

    /// The source SVG could not be parsed / rasterized.
    #[error("SVG rasterization failed: {0}")]
    Svg(String),

    /// No DMG was produced by the packaging stage.
    #[error("no DMG was produced at {0}")]
    NoDmg(PathBuf),

    /// No icon source was supplied for an icon-requiring step.
    #[error("no icon source supplied (set AppSpec.icon)")]
    NoIcon,
}

/// Convenience: attach a path to an `io::Result`.
pub trait IoPathExt<T> {
    /// Map an `io::Error` into a typed [`ReleaseError::Io`] carrying `path`.
    ///
    /// # Errors
    /// Returns [`ReleaseError::Io`] when the underlying result is `Err`.
    fn at(self, path: impl Into<PathBuf>) -> Result<T, ReleaseError>;
}

impl<T> IoPathExt<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T, ReleaseError> {
        self.map_err(|source| ReleaseError::Io {
            path: path.into(),
            source,
        })
    }
}
