//! Tiny typed git helpers (short sha, toplevel) for release tooling.
//!
//! Lifted from gaveta-release's `main.rs` so both consumers share one
//! implementation instead of re-deriving the `git rev-parse` shape.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `git rev-parse --show-toplevel`, or `None` if not in a repo.
#[must_use]
pub fn toplevel() -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(s.trim()))
}

/// `git -C <repo> rev-parse --short HEAD`, or `nogit` if unavailable.
#[must_use]
pub fn short_sha(repo: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("nogit"))
}
