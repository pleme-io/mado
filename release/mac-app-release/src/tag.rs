//! Autobump tag/key computation — the sortable-exact + moving-latest pair.
//!
//! Generalized from gaveta-release's `UploadKeys`. The exact tag is sortable
//! (timestamp-led) + traceable (carries the git sha + arch); the latest tag is
//! the stable human pointer an environment / a `releases/latest` page resolves.
//! Per the ★★ AUTO-RELEASE / AUTOBUMP rule: a publish is an exact immutable tag
//! plus a moving `latest` pointer — never a `:latest`-only deploy source.

use chrono::{DateTime, Utc};

/// The two release tags: a sortable exact one + a moving `latest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseTags {
    /// e.g. `macos-arm64-r412-9c1f2a3` (run-number sortable + sha traceable)
    /// or, when no run number is known, `macos-arm64-20260617T210430-9c1f2a3`.
    pub exact: String,
    /// `macos-arm64-latest` — the moving human pointer.
    pub latest: String,
}

/// The arch-scoped tag prefix, e.g. `macos-arm64`.
fn arch_prefix(os: &str, arch_suffix: &str) -> String {
    let mut s = String::from(os);
    s.push('-');
    s.push_str(arch_suffix);
    s
}

impl ReleaseTags {
    /// Compute tags from a CI run number (sortable `r<n>`) + git sha + arch.
    /// This is the canonical CI path — `r<n>` is numerically orderable so a
    /// subscriber can pick "newest" (the AUTOBUMP `ImagePolicy` shape, applied
    /// to GitHub releases).
    #[must_use]
    pub fn from_run(os: &str, arch_suffix: &str, run_number: u64, sha: &str) -> Self {
        let prefix = arch_prefix(os, arch_suffix);
        let mut exact = prefix.clone();
        exact.push_str("-r");
        exact.push_str(&run_number.to_string());
        exact.push('-');
        exact.push_str(sha);

        let mut latest = prefix;
        latest.push_str("-latest");

        Self { exact, latest }
    }

    /// Compute tags from a UTC timestamp (sortable) + git sha + arch — the
    /// local-build path where no CI run number exists.
    #[must_use]
    pub fn from_time(os: &str, arch_suffix: &str, now: DateTime<Utc>, sha: &str) -> Self {
        let prefix = arch_prefix(os, arch_suffix);
        let ts = now.format("%Y%m%dT%H%M%S").to_string();
        let mut exact = prefix.clone();
        exact.push('-');
        exact.push_str(&ts);
        exact.push('-');
        exact.push_str(sha);

        let mut latest = prefix;
        latest.push_str("-latest");

        Self { exact, latest }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn run_tag_is_sortable_and_traceable() {
        let t = ReleaseTags::from_run("macos", "arm64", 412, "9c1f2a3");
        assert_eq!(t.exact, "macos-arm64-r412-9c1f2a3");
        assert_eq!(t.latest, "macos-arm64-latest");
    }

    #[test]
    fn time_tag_is_sortable_and_traceable() {
        let now = Utc.with_ymd_and_hms(2026, 6, 17, 21, 4, 30).unwrap();
        let t = ReleaseTags::from_time("macos", "arm64", now, "9c1f2a3");
        assert_eq!(t.exact, "macos-arm64-20260617T210430-9c1f2a3");
        assert_eq!(t.latest, "macos-arm64-latest");
    }
}
