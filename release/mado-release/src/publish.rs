//! Publish the Mado DMG + zip to GitHub Releases — the easy, open-source target.
//!
//! mado is a PUBLIC repo, so the frictionless download path is a GitHub Release
//! (no self-hosted S3, no auth a downloader needs). We drive the `gh` CLI
//! through the shared typed [`Tool`] builder: create-or-update a release for a
//! tag, then upload the assets with `--clobber` so a re-run is idempotent.
//!
//! This is the mado-SPECIFIC half the shared `mac-app-release` crate
//! deliberately does NOT own — gaveta-release's peer is its armázem-S3 module.

use std::path::Path;

use mac_app_release::cmd::Tool;
use mac_app_release::error::ReleaseError;

/// A GitHub Release target: the repo + the tag + whether it is a prerelease.
#[derive(Debug, Clone)]
pub struct GhRelease {
    /// `owner/repo`, e.g. `pleme-io/mado`.
    pub repo: String,
    /// The git tag the release is attached to (created if absent).
    pub tag: String,
    /// The release title.
    pub title: String,
    /// Mark this a prerelease (used for the moving `…-latest` rolling release).
    pub prerelease: bool,
    /// Replace the tag's existing release/assets (idempotent re-runs).
    pub clobber: bool,
}

impl GhRelease {
    /// Create-or-update the GitHub Release for this tag, then upload `assets`.
    ///
    /// Uses `gh release create --target … || gh release upload …` semantics via
    /// two typed `gh` invocations: first ensure the release exists, then upload
    /// the assets with `--clobber`. Both go through the typed [`Tool`] builder.
    ///
    /// # Errors
    /// Returns [`ReleaseError::Subprocess`] / [`ReleaseError::Spawn`] when `gh`
    /// is missing or returns non-zero.
    pub fn publish(&self, assets: &[&Path]) -> Result<(), ReleaseError> {
        // 1) Ensure the release exists. `gh release create` fails if the
        //    release already exists, so we tolerate that and fall through to
        //    upload. We detect existence first with a quiet `gh release view`.
        if !self.exists()? {
            let mut create = Tool::new("gh")
                .arg("release")
                .arg("create")
                .arg(&self.tag)
                .arg("--repo")
                .arg(&self.repo)
                .arg("--title")
                .arg(&self.title)
                .arg("--notes")
                .arg(&self.notes());
            if self.prerelease {
                create = create.arg("--prerelease");
            }
            // Attach the assets in the create call itself (one shot).
            for a in assets {
                create = create.arg(a);
            }
            return create.run();
        }

        // 2) Release exists → upload (clobber) the assets onto it.
        let mut upload = Tool::new("gh")
            .arg("release")
            .arg("upload")
            .arg(&self.tag)
            .arg("--repo")
            .arg(&self.repo);
        for a in assets {
            upload = upload.arg(a);
        }
        if self.clobber {
            upload = upload.arg("--clobber");
        }
        upload.run()
    }

    /// Whether a release already exists for this tag (`gh release view`).
    fn exists(&self) -> Result<bool, ReleaseError> {
        let out = std::process::Command::new("gh")
            .arg("release")
            .arg("view")
            .arg(&self.tag)
            .arg("--repo")
            .arg(&self.repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|source| ReleaseError::Spawn {
                program: "gh".to_owned(),
                source,
            })?;
        Ok(out.success())
    }

    /// The release notes body (typed, kept out of `format!`).
    fn notes(&self) -> String {
        let mut s = String::from(
            "Mado (窓) — GPU-rendered terminal emulator.\n\nmacOS: download the .dmg, \
             drag Mado.app to Applications. First launch: right-click → Open (ad-hoc \
             signed, not notarized), or `xattr -dr com.apple.quarantine /Applications/Mado.app`.\n\n\
             Exact build: ",
        );
        s.push_str(&self.tag);
        s
    }
}
