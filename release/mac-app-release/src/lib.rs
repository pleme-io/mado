//! `mac-app-release` — the SHARED, app-agnostic macOS `.app`/DMG release core.
//!
//! Extracted from gaveta-client's `gaveta-release` so that mado AND gaveta (and
//! any future fleet GUI app) drive ONE typed pipeline for the repeated pattern:
//! assemble a signed, icon-bearing, drag-installable macOS app bundle + DMG,
//! with a sortable-exact / moving-latest release tag — and NOTHING shell-string
//! composed (every unavoidable Mac tool — codesign / hdiutil / ditto / iconutil
//! — runs through the typed [`cmd::Tool`] builder).
//!
//! ## What's shared (this crate)
//! - [`cmd::Tool`] — the typed `Command` builder (the sanctioned subprocess
//!   exception).
//! - [`spec::AppSpec`] — the typed config a consumer drives the pipeline with.
//!   The windowed-vs-menubar difference is ONE field (`ls_ui_element`).
//! - [`plist_model::InfoPlist`] — typed Info.plist → XML (the `plist` crate).
//! - [`icon`] — SVG → 10-slot iconset → `.icns` (pure-Rust `resvg` + `iconutil`).
//! - [`assemble`] — assemble → ad-hoc codesign → DMG + zip (pure-fs layout).
//! - [`tag::ReleaseTags`] — the sortable-exact + moving-latest autobump tags.
//! - [`git`] — short-sha + toplevel helpers.
//!
//! ## What's NOT shared (the consumer owns)
//! - HOW the binary is built (`cargo build` vs `swift build` vs a Nix drv).
//! - The bundle's extra-binary layout BEYOND the typed `AppSpec.extra_binaries`.
//! - The PUBLISH backend: gaveta → armázem S3; mado → GitHub Releases via `gh`.
//!   Each consumer keeps its own publish module + its own publish error type.
//!
//! ## Adoption (the other repo)
//! gaveta-client's `gaveta-release` becomes a thin consumer: replace its
//! `cmd` / `error` / `plist_model` / `icon` / `assemble` / (the `UploadKeys`
//! part of) `publish` with this crate, keeping only its armázem upload +
//! Sparkle appcast modules and a `gaveta()` `AppSpec` builder. See README.md.

pub mod assemble;
pub mod cmd;
pub mod error;
pub mod git;
pub mod icon;
pub mod plist_model;
pub mod spec;
pub mod tag;

pub use assemble::{build_dmg, is_macos, Artifacts, Layout};
pub use error::{IoPathExt, ReleaseError};
pub use spec::{host_arch_suffix, AppSpec, ExtraBinary, IconSource};
pub use tag::ReleaseTags;
