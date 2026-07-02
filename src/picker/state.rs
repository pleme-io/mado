//! [`FuzzyPicker<Row>`] — the one modal picker state both the Ctrl-S
//! session picker and the Ctrl-T directory picker are expressions of.
//!
//! Pre-base, `SessionPickerState` and `DirPickerState` were near-byte
//! duplicates (open / query / results / selected + open / close /
//! set_results / move_up / move_down / selected_*). This is that shape,
//! once, generic over the row type. Each picker keeps only its delta:
//! the `Row` it ranks and the [`PickerSource`] that produces + accepts
//! it.
//!
//! Pure data: the renderer reads it through a shared `Arc<Mutex<_>>`,
//! the input engine drives it through the overlay FSM. No I/O, no locks
//! taken here.

/// The modal state of any fuzzy-filter picker overlay, generic over the
/// displayed `Row`. Drives the renderer (read-only) and is driven by the
/// overlay FSM (open / edit / navigate / accept / close).
#[derive(Default)]
pub struct FuzzyPicker<Row> {
    /// Whether the overlay is open (gates rendering + input capture).
    pub open: bool,
    /// `true` when the picker is open but its action is disabled (e.g.
    /// session-switching off: no bridge). Distinct from
    /// `results.is_empty()` so the render + FSM tell "disabled" from "no
    /// matches".
    pub disabled: bool,
    /// The fuzzy-filter needle typed so far.
    pub query: String,
    /// Ranked rows for the current needle (populated by the engine from
    /// the [`PickerSource`] on open + every edit).
    pub results: Vec<Row>,
    /// Index of the highlighted row.
    pub selected: usize,
    /// A one-line operator-facing notice (e.g. "could not start that —
    /// it may have just resolved"). Set by a failed accept, cleared on
    /// open/close and on the next query edit; survives autonomous
    /// re-lists so the operator actually sees it.
    pub notice: Option<String>,
}

impl<Row> FuzzyPicker<Row> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            disabled: false,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            notice: None,
        }
    }

    /// Open the overlay, seeding the list from `rows`. `disabled` flags
    /// the no-action case (e.g. no bridge) so the renderer can show a
    /// hint and accept is inert.
    pub fn open(&mut self, rows: Vec<Row>, disabled: bool) {
        self.open = true;
        self.disabled = disabled;
        self.query.clear();
        self.selected = 0;
        self.results = rows;
        self.notice = None;
    }

    /// Close + reset.
    pub fn close(&mut self) {
        self.open = false;
        self.disabled = false;
        self.query.clear();
        self.results.clear();
        self.selected = 0;
        self.notice = None;
    }

    /// Replace the ranked rows after a query edit (the engine recomputes
    /// via the source). Resets the highlight to the top.
    pub fn set_results(&mut self, rows: Vec<Row>) {
        self.selected = 0;
        self.results = rows;
    }

    /// Replace the ranked rows while keeping the highlight on the SAME row by
    /// identity (`key_of`), so an autonomous data-flow re-list can refresh the
    /// board *while the operator navigates* without yanking the cursor — the
    /// "keep flowing even when searching / driving across problems" behavior.
    /// If the previously-highlighted row is gone from the new set, the
    /// highlight clamps to the top. Returns `true` when the highlighted row
    /// vanished (clamped to a row the operator did not choose) — the caller
    /// stamps the positional-stability window so a same-instant Enter is
    /// swallowed.
    pub fn set_results_preserving<K: PartialEq>(
        &mut self,
        rows: Vec<Row>,
        key_of: impl Fn(&Row) -> K,
    ) -> bool {
        let prev_key = self.results.get(self.selected).map(&key_of);
        self.results = rows;
        match prev_key.and_then(|k| self.results.iter().position(|r| key_of(r) == k)) {
            Some(idx) => {
                self.selected = idx;
                false
            }
            None => {
                // The highlighted row is gone — clamp to the top. A vanish is
                // a shift the caller should guard against a stray Enter.
                let had_selection = !self.results.is_empty();
                self.selected = 0;
                had_selection
            }
        }
    }

    /// `true` when the picker is open AND the highlight is still at the top
    /// (the operator hasn't navigated yet). The suggestion stream's live
    /// re-list only fires while resting, so a refresh never yanks the cursor
    /// out from under someone mid-selection.
    #[must_use]
    pub fn is_resting(&self) -> bool {
        self.open && self.selected == 0
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

    /// The currently-highlighted row, if any.
    #[must_use]
    pub fn selected_row(&self) -> Option<&Row> {
        self.results.get(self.selected)
    }
}

/// The seam between the mode-agnostic input engine and a concrete data
/// backend. `list` is the per-edit recompute both pickers share — the
/// engine calls it on open + every keystroke; the source produces the
/// ranked rows. The *accept* action is the per-picker delta and lives on
/// the concrete source (the engine's effect arm calls it), because the
/// two accepts are genuinely different I/O (dir injects `cd` into the
/// PTY; session posts a pane / spawns a session).
///
/// Time is injected (`now` unix-seconds) so the ranking stays
/// deterministic + testable — the engine supplies its single clock-read.
pub trait PickerSource {
    /// The row type this source produces.
    type Row;

    /// Ranked rows for the given fuzzy `query` (empty → the default
    /// view). `now` is unix-seconds, injected for any frecency
    /// tie-break.
    fn list(&self, query: &str, now: u64) -> Vec<Self::Row>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(rows: &[&str]) -> FuzzyPicker<String> {
        let mut p = FuzzyPicker::new();
        p.open(rows.iter().map(|s| (*s).to_owned()).collect(), false);
        p
    }

    #[test]
    fn open_seeds_rows_and_resets_cursor() {
        let mut p = picker(&["a", "b"]);
        p.selected = 1;
        p.open(vec!["c".to_owned()], false);
        assert!(p.open);
        assert!(!p.disabled);
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_row().map(String::as_str), Some("c"));
    }

    #[test]
    fn open_disabled_flags_the_hint() {
        let mut p: FuzzyPicker<String> = FuzzyPicker::new();
        p.open(vec![], true);
        assert!(p.open && p.disabled);
        assert!(p.selected_row().is_none());
    }

    #[test]
    fn move_wraps_both_ways() {
        let mut p = picker(&["a", "b", "c"]);
        assert_eq!(p.selected, 0);
        p.move_up();
        assert_eq!(p.selected, 2, "wrap up from top → last");
        p.move_down();
        assert_eq!(p.selected, 0, "wrap down from bottom → first");
    }

    #[test]
    fn move_on_empty_is_a_noop() {
        let mut p: FuzzyPicker<String> = FuzzyPicker::new();
        p.open(vec![], false);
        p.move_down();
        p.move_up();
        assert_eq!(p.selected, 0);
        assert!(p.selected_row().is_none());
    }

    #[test]
    fn set_results_resets_highlight() {
        let mut p = picker(&["a", "b"]);
        p.move_down();
        assert_eq!(p.selected, 1);
        p.set_results(vec!["c".to_owned()]);
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_row().map(String::as_str), Some("c"));
    }

    #[test]
    fn set_results_preserving_keeps_cursor_on_its_row() {
        // Identity here is the string itself. The operator is on "b"; a live
        // re-list reorders + inserts rows — the cursor must follow "b", so the
        // board can keep flowing while they navigate.
        let mut p = picker(&["a", "b", "c"]);
        p.move_down(); // on "b"
        assert_eq!(p.selected, 1);
        let vanished = p.set_results_preserving(
            vec!["x".to_owned(), "a".to_owned(), "b".to_owned(), "c".to_owned()],
            |r: &String| r.clone(),
        );
        assert!(!vanished, "row still present → no vanish");
        assert_eq!(p.selected_row().map(String::as_str), Some("b"), "cursor followed its row");

        // When the highlighted row is gone, clamp to the top + report the vanish.
        let vanished = p.set_results_preserving(
            vec!["a".to_owned(), "c".to_owned()],
            |r: &String| r.clone(),
        );
        assert!(vanished, "'b' vanished → caller must stamp the stability window");
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn close_resets_everything() {
        let mut p = picker(&["a"]);
        p.query.push_str("ma");
        p.close();
        assert!(!p.open && !p.disabled);
        assert!(p.query.is_empty() && p.results.is_empty());
        assert_eq!(p.selected, 0);
    }
}
