//! Vendored `xterm-ghostty` terminfo — the availability half of
//! capability honesty (M3 review 2026-06-12).
//!
//! `caps::advertised_term()` projects `xterm-ghostty` once styled
//! underlines are implemented — but that entry is NOT in any system
//! ncurses database (it ships inside Ghostty's app bundle). On a
//! host without Ghostty, or a mado launched from Finder/launchd
//! without an inherited `TERMINFO`, vim/less/htop fail with
//! "missing or unsuitable terminal: xterm-ghostty". A
//! capability-honest advertisement is worthless if the entry is
//! unavailable, so mado VENDORS the compiled entry and materializes
//! it into a per-user cache dir, exported via `TERMINFO` alongside
//! `TERM` at every PTY spawn. The M5 mado-specific terminfo
//! generated from `TerminalCaps` replaces this borrowed entry; the
//! materialization seam stays.
//!
//! The blob is Ghostty's compiled `xterm-ghostty` (MIT,
//! github.com/ghostty-org/ghostty), byte-identical to the entry
//! Ghostty installs — apps resolve the same capabilities either way.

use std::path::PathBuf;

/// Compiled `xterm-ghostty` terminfo (ncurses legacy format,
/// magic 0o432). Vendored from Ghostty 1.3.1 (MIT).
pub const XTERM_GHOSTTY_COMPILED: &[u8] =
    include_bytes!("../assets/terminfo/xterm-ghostty");

/// The `(subdir, filename)` layout terminfo resolvers expect:
/// Linux ncurses uses first-letter dirs (`x/`, `g/`), macOS ncurses
/// uses first-letter-hex dirs (`78/`, `67/`). `ghostty` is the
/// alias name the entry also answers to. All four point at the
/// same bytes.
const LAYOUT: [(&str, &str); 4] = [
    ("78", "xterm-ghostty"),
    ("67", "ghostty"),
    ("x", "xterm-ghostty"),
    ("g", "ghostty"),
];

/// Materialize the vendored entry into the per-user cache and
/// return the directory to export as `TERMINFO`. Idempotent: an
/// existing file of the right length is left untouched (ncurses
/// re-reads per process, so no invalidation concern beyond a
/// version bump changing the blob length).
pub fn ensure_terminfo_dir() -> std::io::Result<PathBuf> {
    let base = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mado")
        .join("terminfo");
    for (subdir, name) in LAYOUT {
        let dir = base.join(subdir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        let fresh = std::fs::metadata(&path)
            .map(|m| m.len() != XTERM_GHOSTTY_COMPILED.len() as u64)
            .unwrap_or(true);
        if fresh {
            std::fs::write(&path, XTERM_GHOSTTY_COMPILED)?;
        }
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored blob is a real compiled terminfo entry — ncurses
    /// legacy magic 0o432 (0x1a 0x01 little-endian) — not an empty
    /// or source-format file.
    #[test]
    fn vendored_blob_is_compiled_terminfo() {
        assert!(XTERM_GHOSTTY_COMPILED.len() > 1024, "blob suspiciously small");
        assert_eq!(
            &XTERM_GHOSTTY_COMPILED[..2],
            &[0x1a, 0x01],
            "blob must carry the ncurses compiled-terminfo magic"
        );
    }

    /// Materialization produces every resolver layout (Linux letter
    /// dirs + macOS hex dirs, primary + alias names) with the exact
    /// vendored bytes — the availability proof behind advertising
    /// TERM=xterm-ghostty (M3 review 2026-06-12: the advertisement
    /// was capability-honest but availability-dishonest; hosts
    /// without Ghostty had no resolvable entry).
    #[test]
    fn ensure_terminfo_dir_materializes_every_resolver_layout() {
        let base = ensure_terminfo_dir().expect("materialize terminfo");
        let mut failures: Vec<String> = Vec::new();
        for (subdir, name) in LAYOUT {
            let path = base.join(subdir).join(name);
            match std::fs::read(&path) {
                Ok(bytes) if bytes == XTERM_GHOSTTY_COMPILED => {}
                Ok(bytes) => failures.push(format!(
                    "{}: {} bytes, expected the {}-byte vendored blob",
                    path.display(),
                    bytes.len(),
                    XTERM_GHOSTTY_COMPILED.len()
                )),
                Err(e) => failures.push(format!("{}: {e}", path.display())),
            }
        }
        assert!(
            failures.is_empty(),
            "{} terminfo layouts missing:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
        // Idempotent: a second call is a no-op, not a failure.
        ensure_terminfo_dir().expect("second materialization");
    }
}
