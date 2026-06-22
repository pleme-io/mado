//! Ctrl-T directory-frecency picker (轍 wadachi) — the dir expression of
//! the shared fuzzy-picker base.
//!
//! Same shape as the session switcher: the modal state is the generic
//! [`crate::picker::state::FuzzyPicker`] over [`DirRow`], and this file
//! owns only the dir delta — the [`DirPickerSource`] (`list` ranks dirs
//! via the typed `pleme_io_wadachi` reader) and the `cd`-injection accept
//! (in the engine's effect arm). mado **never records** — it calls only
//! the reader API (`top_n`); the shell owns recording (single-recorder
//! rule). On select, the engine injects `cd <dir>\n` into the PTY.

use std::path::PathBuf;

use crate::picker::state::PickerSource;

/// One directory row: the path + its frecency score. Plain tuple so no
/// wadachi type leaks into the render/state layer.
pub type DirRow = (PathBuf, f64);

/// The modal state of the Ctrl-T overlay — the generic fuzzy picker over
/// [`DirRow`], shared with the session switcher.
pub type DirPickerState = crate::picker::state::FuzzyPicker<DirRow>;

/// The dir picker's [`PickerSource`]: ranks directories by frecency via
/// the typed wadachi reader. Stateless (the reader is a free function),
/// so it carries nothing — but implementing the trait keeps the dir
/// picker an expression of the same base the session picker uses.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirPickerSource;

impl PickerSource for DirPickerSource {
    type Row = DirRow;

    /// Frecency-ranked dirs for `query` via the wadachi reader.
    /// Best-effort: a reader error yields an empty list (never panics,
    /// never records). `now` is unused — wadachi owns its own recency.
    fn list(&self, query: &str, _now: u64) -> Vec<DirRow> {
        match pleme_io_wadachi::top_n(query, 15) {
            Ok(v) => v.into_iter().map(|d| (d.path, d.score)).collect(),
            Err(e) => {
                tracing::warn!("wadachi top_n: {e}");
                Vec::new()
            }
        }
    }
}

/// POSIX single-quote a path so `cd <path>` survives spaces / specials when
/// injected into the PTY.
#[must_use]
pub fn shell_quote_path(p: &str) -> String {
    if !p.contains('\'') {
        return format!("'{p}'");
    }
    let mut out = String::from("'");
    for ch in p.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}
