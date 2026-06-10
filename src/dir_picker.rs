//! Reader-only directory-frecency picker state (轍 wadachi).
//!
//! Mirrors `search::SearchState`: pure data + the typed wadachi *reader* calls.
//! The renderer (`render.rs`) reads it via a shared `Arc<Mutex<_>>`; the input
//! handler (`main.rs`) drives it. mado **never records** — it calls only the
//! `pleme_io_wadachi` reader API (`top_n` / `resolve`); the shell owns recording
//! (single-recorder rule). On select, the input handler injects `cd <dir>\n`
//! into the PTY — a keystroke, not a recording.

use std::path::PathBuf;

/// The modal state of the in-terminal directory-frecency overlay.
#[derive(Default)]
pub struct DirPickerState {
    /// Whether the overlay is open (gates all rendering + input capture).
    pub open: bool,
    /// The filter needle typed so far.
    pub query: String,
    /// Frecency-ranked `(path, score)` results. Plain tuples so no wadachi type
    /// leaks into the render/state layer — keeps the dependency surface minimal.
    pub results: Vec<(PathBuf, f64)>,
    /// Index of the highlighted row.
    pub selected: usize,
}

impl DirPickerState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the overlay, seeding with the top frecency dirs (empty needle).
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.recompute();
    }

    /// Close + reset.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.results.clear();
        self.selected = 0;
    }

    /// Append typed text to the needle + re-rank.
    pub fn push_str(&mut self, s: &str) {
        self.query.push_str(s);
        self.recompute();
    }

    /// Delete the last needle char + re-rank.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.recompute();
    }

    /// Move the highlight down (wraps).
    pub fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
        }
    }

    /// Move the highlight up (wraps).
    pub fn move_up(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// The currently-highlighted path, if any.
    #[must_use]
    pub fn selected_path(&self) -> Option<&PathBuf> {
        self.results.get(self.selected).map(|(p, _)| p)
    }

    /// Re-rank against the current needle via the wadachi reader. Best-effort:
    /// a reader error just yields an empty list (never panics, never records).
    fn recompute(&mut self) {
        self.selected = 0;
        self.results = match pleme_io_wadachi::top_n(&self.query, 15) {
            Ok(v) => v.into_iter().map(|d| (d.path, d.score)).collect(),
            Err(e) => {
                tracing::warn!("wadachi top_n: {e}");
                Vec::new()
            }
        };
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
