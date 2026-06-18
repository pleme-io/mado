//! Assemble → sign → package an `AppSpec` into a signed `.app` + DMG + zip.
//!
//! The `.app` layout, the Info.plist write, the PkgInfo, the DMG staging dir +
//! the `/Applications` symlink are pure Rust `std::fs` (no `cp` / `mkdir` /
//! `ln`). codesign / hdiutil / ditto / iconutil run through typed `Command`s.
//!
//! Build-agnostic: the consumer supplies the already-built primary binary path
//! (it knows whether that came from `cargo build`, `swift build`, a Nix
//! derivation, …). The shared primitive owns everything DOWNSTREAM of "the
//! binary exists" — which is the part both consumers duplicated.

use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use crate::cmd::Tool;
use crate::error::{IoPathExt, ReleaseError};
use crate::icon;
use crate::spec::AppSpec;

/// The resolved on-disk output layout (a `dist/` and a scratch `build/`).
#[derive(Debug, Clone)]
pub struct Layout {
    /// The output dir for the final DMG/zip.
    pub dist: PathBuf,
    /// The scratch staging dir (wiped each run).
    pub build: PathBuf,
}

impl Layout {
    /// `<root>/dist` + `<root>/dist/build`.
    #[must_use]
    pub fn under(root: &Path) -> Self {
        let dist = root.join("dist");
        let build = dist.join("build");
        Self { dist, build }
    }
}

/// The artifacts a successful package run produced.
#[derive(Debug, Clone)]
pub struct Artifacts {
    /// The drag-to-Applications `.dmg`.
    pub dmg: PathBuf,
    /// The bare `.app` `.zip`.
    pub zip: PathBuf,
    /// The assembled (signed) `.app` bundle directory.
    pub app: PathBuf,
}

/// Assemble `Contents/{MacOS,Resources}`, write the typed Info.plist + PkgInfo,
/// generate the `.icns`, and place the primary + extra binaries. Returns the
/// `.app` path (UN-signed). Pure-fs layout; `iconutil` for the icns only.
fn assemble_app(spec: &AppSpec, layout: &Layout, primary_bin: &Path) -> Result<PathBuf, ReleaseError> {
    tracing::info!("[1/3] assembling {}.app", spec.app_name);
    let app = layout.build.join(spec.bundle_dir_name());
    let contents = app.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos_dir).at(&macos_dir)?;
    std::fs::create_dir_all(&resources).at(&resources)?;

    if !primary_bin.is_file() {
        return Err(ReleaseError::MissingBinary(primary_bin.to_path_buf()));
    }
    copy_exec(primary_bin, &macos_dir.join(&spec.executable))?;

    // Extra binaries (CLI seams, daemons) at their typed dest paths.
    for extra in &spec.extra_binaries {
        if !extra.src.is_file() {
            return Err(ReleaseError::MissingBinary(extra.src.clone()));
        }
        let dest = contents.join(&extra.dest_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        copy_exec(&extra.src, &dest)?;
    }

    // Info.plist — typed struct → XML plist (no plutil/heredoc).
    let plist_path = contents.join("Info.plist");
    std::fs::write(&plist_path, spec.info_plist().to_xml_bytes()?).at(&plist_path)?;

    // PkgInfo
    let pkginfo = contents.join("PkgInfo");
    std::fs::write(&pkginfo, b"APPL????").at(&pkginfo)?;

    // Icon → Resources/<AppName>.icns (resvg → iconutil, or copy a .icns).
    tracing::info!("[2/3] generating {}", spec.icns_name());
    let icon_src = spec.icon.as_ref().ok_or(ReleaseError::NoIcon)?;
    let iconset = layout.build.join("AppIcon.iconset");
    let icns = resources.join(spec.icns_name());
    icon::build_icns(icon_src, &iconset, &icns)?;

    Ok(app)
}

/// Ad-hoc codesign nested binaries first, then `--deep` the bundle, then verify.
fn codesign(spec: &AppSpec, app: &Path) -> Result<(), ReleaseError> {
    tracing::info!("[3/3] ad-hoc codesigning");
    let contents = app.join("Contents");
    for extra in &spec.extra_binaries {
        Tool::new("codesign")
            .arg("-s")
            .arg("-")
            .arg("--force")
            .arg("--timestamp=none")
            .arg(contents.join(&extra.dest_rel))
            .run()?;
    }
    Tool::new("codesign")
        .arg("-s")
        .arg("-")
        .arg("--force")
        .arg("--deep")
        .arg("--timestamp=none")
        .arg(app)
        .run()?;
    Tool::new("codesign")
        .arg("--verify")
        .arg("--deep")
        .arg("--strict")
        .arg(app)
        .run()
}

/// Package: a `.zip` (ditto) + a `.dmg` (hdiutil over a staging dir with the
/// drag-to-`/Applications` affordance).
fn package(spec: &AppSpec, layout: &Layout, app: &Path, suffix: &str) -> Result<(PathBuf, PathBuf), ReleaseError> {
    let dmg = layout.dist.join(spec.artifact_name(suffix, "dmg"));
    let zip = layout.dist.join(spec.artifact_name(suffix, "zip"));

    // .zip of the bare bundle (ditto preserves the bundle + xattrs).
    let _ = std::fs::remove_file(&zip);
    Tool::new("ditto")
        .arg("-c")
        .arg("-k")
        .arg("--keepParent")
        .arg(app)
        .arg(&zip)
        .run()?;

    // .dmg staging: copy the bundle in + a /Applications symlink (pure fs).
    let stage = layout.build.join("dmg-stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).at(&stage)?;
    Tool::new("ditto")
        .arg(app)
        .arg(stage.join(spec.bundle_dir_name()))
        .run()?;
    let apps_link = stage.join("Applications");
    unix_fs::symlink("/Applications", &apps_link).at(&apps_link)?;

    let _ = std::fs::remove_file(&dmg);
    Tool::new("hdiutil")
        .arg("create")
        .arg("-volname")
        .arg(&spec.app_name)
        .arg("-srcfolder")
        .arg(&stage)
        .arg("-ov")
        .arg("-format")
        .arg("UDZO")
        .arg(&dmg)
        .quiet()
        .run()?;

    if !dmg.is_file() {
        return Err(ReleaseError::NoDmg(dmg));
    }
    Ok((dmg, zip))
}

/// Run the whole assemble → sign → package pipeline for `spec`, given the
/// already-built primary binary. Returns the produced artifacts.
///
/// # Errors
/// Returns a typed [`ReleaseError`] at the first failing step. Only runs on
/// macOS (callers should gate on [`is_macos`]).
pub fn build_dmg(spec: &AppSpec, layout: &Layout, primary_bin: &Path) -> Result<Artifacts, ReleaseError> {
    // Fresh scratch dirs (rm -rf build; mkdir -p dist build) — pure fs.
    let _ = std::fs::remove_dir_all(&layout.build);
    std::fs::create_dir_all(&layout.dist).at(&layout.dist)?;
    std::fs::create_dir_all(&layout.build).at(&layout.build)?;

    let app = assemble_app(spec, layout, primary_bin)?;
    codesign(spec, &app)?;
    let suffix = crate::spec::host_arch_suffix();
    let (dmg, zip) = package(spec, layout, &app, suffix)?;
    Ok(Artifacts { dmg, zip, app })
}

/// Copy a built binary and mark it executable (replaces `cp` + `chmod +x`).
fn copy_exec(src: &Path, dst: &Path) -> Result<(), ReleaseError> {
    std::fs::copy(src, dst).at(dst)?;
    let mut perm = std::fs::metadata(dst).at(dst)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(dst, perm).at(dst)
}

/// Whether the host is macOS (the pipeline needs codesign/hdiutil/iconutil).
#[must_use]
pub fn is_macos() -> bool {
    std::env::consts::OS == "macos"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_under_root() {
        let l = Layout::under(Path::new("/tmp/mado"));
        assert_eq!(l.dist, PathBuf::from("/tmp/mado/dist"));
        assert_eq!(l.build, PathBuf::from("/tmp/mado/dist/build"));
    }
}
