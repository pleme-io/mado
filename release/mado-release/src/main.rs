//! mado-release — the typed macOS release tool for Mado.app.
//!
//! A THIN consumer of the shared `mac-app-release` primitive: it owns only
//! mado-SPECIFIC bits — building the `mado` binary with `cargo build --release`,
//! the windowed `AppSpec` (a real Dock icon, NO `LSUIElement`, unlike gaveta's
//! menu-bar app), and the GitHub-Releases publish path. Everything else
//! (assembly, Info.plist, SVG→.icns, codesign, DMG/zip, autobump tags) comes
//! from the shared crate.
//!
//! Per the pleme-io NO-SHELL law, the only subprocesses are the unavoidable
//! Mac-native tools (codesign/hdiutil/ditto/iconutil, inside the shared crate)
//! plus `cargo` (the build) and `gh` (the publish) — each a typed `Command`.

mod publish;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use mac_app_release::cmd::Tool;
use mac_app_release::{assemble, git, host_arch_suffix, AppSpec, Layout, ReleaseTags};

use crate::publish::GhRelease;

/// Build, sign, package, and (optionally) publish Mado.app — typed, no shell.
#[derive(Debug, Parser)]
#[command(name = "mado-release", version, about)]
struct Args {
    /// The mado repo root. Defaults to `$MADO_DIR`, else the git toplevel of the
    /// cwd, else the cwd.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// The `owner/repo` to publish the GitHub Release under.
    #[arg(long, default_value = "pleme-io/mado")]
    gh_repo: String,

    /// Publish to GitHub Releases after building (needs `gh` authed). When
    /// omitted, the tool builds + signs + packages locally and stops.
    #[arg(long)]
    publish: bool,

    /// Mark the published release a prerelease (for the rolling `…-latest`).
    #[arg(long)]
    prerelease: bool,

    /// Explicit release tag. Defaults to the autobump exact tag
    /// `macos-<arch>-<utc>-<sha>`.
    #[arg(long)]
    tag: Option<String>,

    /// CI run number for the sortable `r<n>` exact tag. When unset, the local
    /// timestamp form is used.
    #[arg(long, env = "GITHUB_RUN_NUMBER")]
    run_number: Option<u64>,

    /// Skip the `cargo build` and reuse an already-built binary at this path.
    #[arg(long)]
    prebuilt_binary: Option<PathBuf>,

    /// Publish the DMG/zip under STABLE `Mado-latest-<arch>.{dmg,zip}` asset
    /// names (instead of the versioned `Mado-<version>-<arch>` names). Used for
    /// the moving `macos-<arch>-latest` prerelease so the Homebrew cask + a
    /// `releases/latest` link have a stable download URL.
    #[arg(long)]
    latest_asset: bool,

    /// Resolve the plan (tags, artifact names, .app name) and print it, then
    /// exit — no cargo build, no codesign, no upload. Works on any OS.
    #[arg(long)]
    dry_run: bool,
}

const APP_NAME: &str = "Mado";
const BUNDLE_ID: &str = "io.pleme.mado";
const EXE: &str = "mado";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .without_time()
        .init();

    let args = Args::parse();
    run(&args)
}

fn run(args: &Args) -> Result<()> {
    let repo = resolve_repo_root(args.repo.clone());
    let version = read_version(&repo).context("reading mado version from Cargo.toml")?;
    let suffix = host_arch_suffix();
    let sha = git::short_sha(&repo);

    let tags = match args.run_number {
        Some(n) => ReleaseTags::from_run("macos", suffix, n, &sha),
        None => ReleaseTags::from_time("macos", suffix, chrono::Utc::now(), &sha),
    };
    let tag = args.tag.clone().unwrap_or_else(|| tags.exact.clone());

    // The typed mado AppSpec: a NORMAL WINDOWED app — a Dock icon, NO
    // LSUIElement (the bright-line difference from gaveta's menu-bar bundle).
    let spec = AppSpec::new(APP_NAME, BUNDLE_ID, EXE)
        .version(&version)
        .min_system_version("11.0")
        .copyright("Copyright © pleme-io. MIT licensed.")
        .icon_svg(repo.join("assets/mado-icon.svg"));

    if args.dry_run {
        tracing::info!("dry-run plan:");
        tracing::info!("  repo        : {}", repo.display());
        tracing::info!("  version     : {version}");
        tracing::info!("  arch suffix : {suffix}");
        tracing::info!("  app bundle  : {}", spec.bundle_dir_name());
        tracing::info!("  windowed    : LSUIElement absent (Dock icon) = {}", spec.ls_ui_element.is_none());
        tracing::info!("  dmg         : {}", spec.artifact_name(suffix, "dmg"));
        tracing::info!("  zip         : {}", spec.artifact_name(suffix, "zip"));
        tracing::info!("  exact tag   : {}", tags.exact);
        tracing::info!("  latest tag  : {}", tags.latest);
        tracing::info!("  publish     : {} (repo {})", args.publish, args.gh_repo);
        return Ok(());
    }

    if !assemble::is_macos() {
        anyhow::bail!("mado-release only runs on macOS (needs codesign/hdiutil/iconutil)");
    }

    // 1) Build the mado binary (unless a prebuilt path was supplied).
    let binary = match &args.prebuilt_binary {
        Some(p) => p.clone(),
        None => build_mado(&repo)?,
    };

    // 2) Assemble → sign → DMG/zip via the shared primitive.
    let layout = Layout::under(&repo);
    let artifacts = assemble::build_dmg(&spec, &layout, &binary).context("building Mado.app DMG")?;
    tracing::info!("DMG: {}", artifacts.dmg.display());
    tracing::info!("zip: {}", artifacts.zip.display());

    if !args.publish {
        tracing::info!("--publish not set; built locally only");
        tracing::info!("  exact tag would be : {}", tags.exact);
        return Ok(());
    }

    // 3) Publish to GitHub Releases. For the moving-latest, copy the artifacts
    //    to stable `Mado-latest-<arch>.{dmg,zip}` names so the cask URL is fixed.
    let (dmg, zip) = if args.latest_asset {
        let stable_dmg = layout.dist.join(stable_asset_name(APP_NAME, suffix, "dmg"));
        let stable_zip = layout.dist.join(stable_asset_name(APP_NAME, suffix, "zip"));
        std::fs::copy(&artifacts.dmg, &stable_dmg).context("staging stable-named DMG")?;
        std::fs::copy(&artifacts.zip, &stable_zip).context("staging stable-named zip")?;
        (stable_dmg, stable_zip)
    } else {
        (artifacts.dmg.clone(), artifacts.zip.clone())
    };

    let release = GhRelease {
        repo: args.gh_repo.clone(),
        tag: tag.clone(),
        title: release_title(APP_NAME, &tag),
        prerelease: args.prerelease,
        clobber: true,
    };
    tracing::info!("publishing to GitHub Release {} ({})", args.gh_repo, tag);
    release
        .publish(&[&dmg, &zip])
        .context("publishing to GitHub Releases via gh")?;
    tracing::info!("published: https://github.com/{}/releases/tag/{}", args.gh_repo, tag);
    Ok(())
}

/// `cargo build --release --bin mado` in the repo, returning the binary path.
fn build_mado(repo: &Path) -> Result<PathBuf> {
    tracing::info!("building mado (cargo build --release)");
    Tool::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--bin")
        .arg(EXE)
        .cwd(repo)
        .run()
        .context("cargo build --release --bin mado")?;
    let bin = repo.join("target/release").join(EXE);
    anyhow::ensure!(bin.is_file(), "built binary missing at {}", bin.display());
    Ok(bin)
}

/// Resolve the mado repo root: `--repo`, else `$MADO_DIR`, else git toplevel,
/// else the cwd.
fn resolve_repo_root(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    if let Ok(env) = std::env::var("MADO_DIR") {
        return PathBuf::from(env);
    }
    if let Some(top) = git::toplevel() {
        return top;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Read `version = "X.Y.Z"` from the repo's root `Cargo.toml` `[package]` block.
fn read_version(repo: &Path) -> Result<String> {
    let toml = std::fs::read_to_string(repo.join("Cargo.toml"))
        .with_context(|| "reading Cargo.toml")?;
    for line in toml.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let v = rest.trim().trim_matches('"').to_owned();
                if !v.is_empty() {
                    return Ok(v);
                }
            }
        }
    }
    anyhow::bail!("no `version = \"…\"` found in Cargo.toml")
}

/// The GitHub Release title (typed, kept out of `format!`).
fn release_title(app: &str, tag: &str) -> String {
    let mut s = String::from(app);
    s.push(' ');
    s.push_str(tag);
    s
}

/// `Mado-latest-<arch>.<ext>` — the STABLE asset name for the moving-latest
/// release (so the Homebrew cask URL is fixed). Kept out of `format!`.
fn stable_asset_name(app: &str, suffix: &str, ext: &str) -> String {
    let mut s = String::from(app);
    s.push_str("-latest-");
    s.push_str(suffix);
    s.push('.');
    s.push_str(ext);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_title_is_app_and_tag() {
        assert_eq!(release_title("Mado", "macos-arm64-r5-abc1234"), "Mado macos-arm64-r5-abc1234");
    }

    #[test]
    fn stable_asset_name_is_fixed() {
        assert_eq!(stable_asset_name("Mado", "arm64", "dmg"), "Mado-latest-arm64.dmg");
        assert_eq!(stable_asset_name("Mado", "arm64", "zip"), "Mado-latest-arm64.zip");
    }

    #[test]
    fn reads_version_from_a_package_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"mado\"\nversion = \"0.1.9\"\nedition = \"2024\"\n",
        )
        .unwrap();
        assert_eq!(read_version(dir.path()).unwrap(), "0.1.9");
    }
}
