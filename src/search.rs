//! Scrollback search — find text in terminal history.
//!
//! Supports literal and case-insensitive matching across the terminal
//! grid (scrollback + visible area). Returns match positions for
//! rendering highlights and navigation.

use crate::terminal::Cell;

/// A single match location in the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    /// Row index in the visible rows slice.
    pub row: usize,
    /// Starting column (inclusive).
    pub col_start: usize,
    /// Ending column (inclusive).
    pub col_end: usize,
}

/// Search state machine.
pub struct SearchState {
    /// Whether search is currently active/visible.
    pub active: bool,
    /// Current search query.
    pub query: String,
    /// All matches found in the current grid.
    pub matches: Vec<SearchMatch>,
    /// Index of the currently focused match.
    pub current: usize,
    /// Case-insensitive search.
    pub ignore_case: bool,
}

impl SearchState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            ignore_case: true,
        }
    }

    /// Open search mode.
    pub fn open(&mut self) {
        self.active = true;
    }

    /// Close search mode and clear results.
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.current = 0;
    }

    /// Update the query and re-search the grid.
    pub fn set_query(&mut self, query: &str, rows: &[Vec<Cell>], cols: usize) {
        self.query = query.to_string();
        self.matches.clear();
        self.current = 0;

        if query.is_empty() {
            return;
        }

        let needle = if self.ignore_case {
            query.to_lowercase()
        } else {
            query.to_string()
        };

        for (row_idx, row) in rows.iter().enumerate() {
            let line = row_to_string(row, cols);
            let haystack = if self.ignore_case {
                line.to_lowercase()
            } else {
                line.clone()
            };

            let mut search_start = 0;
            while let Some(pos) = haystack[search_start..].find(&needle) {
                let col_start = search_start + pos;
                let col_end = col_start + needle.len() - 1;
                self.matches.push(SearchMatch {
                    row: row_idx,
                    col_start,
                    col_end: col_end.min(cols.saturating_sub(1)),
                });
                search_start = col_start + 1;
            }
        }
    }

    /// Navigate to the next match.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    /// Navigate to the previous match.
    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.current = if self.current == 0 {
                self.matches.len() - 1
            } else {
                self.current - 1
            };
        }
    }

    /// Get the currently focused match.
    #[must_use]
    pub fn current_match(&self) -> Option<&SearchMatch> {
        self.matches.get(self.current)
    }

    /// Total number of matches.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Check if a cell position is within any match.
    #[must_use]
    pub fn is_match(&self, row: usize, col: usize) -> bool {
        self.matches
            .iter()
            .any(|m| m.row == row && col >= m.col_start && col <= m.col_end)
    }

    /// Check if a cell position is within the current (focused) match.
    #[must_use]
    pub fn is_current_match(&self, row: usize, col: usize) -> bool {
        self.matches.get(self.current).is_some_and(|m| {
            m.row == row && col >= m.col_start && col <= m.col_end
        })
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a row of cells to a string for searching.
fn row_to_string(row: &[Cell], cols: usize) -> String {
    let mut s = String::with_capacity(cols);
    for cell in row.iter().take(cols) {
        if cell.width == 0 {
            continue; // skip continuation cells
        }
        cell.write_to(&mut s);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Cell;

    fn make_row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                ch,
                ..Cell::default()
            })
            .collect()
    }

    #[test]
    fn basic_search() {
        let rows = vec![
            make_row("hello world"),
            make_row("hello again"),
            make_row("goodbye world"),
        ];
        let mut state = SearchState::new();
        state.set_query("hello", &rows, 13);
        assert_eq!(state.match_count(), 2);
        assert_eq!(state.matches[0].row, 0);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[0].col_end, 4);
        assert_eq!(state.matches[1].row, 1);
    }

    #[test]
    fn case_insensitive() {
        let rows = vec![make_row("Hello HELLO hello")];
        let mut state = SearchState::new();
        state.ignore_case = true;
        state.set_query("hello", &rows, 17);
        assert_eq!(state.match_count(), 3);
    }

    #[test]
    fn case_sensitive() {
        let rows = vec![make_row("Hello HELLO hello")];
        let mut state = SearchState::new();
        state.ignore_case = false;
        state.set_query("hello", &rows, 17);
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 12);
    }

    #[test]
    fn navigate_matches() {
        let rows = vec![
            make_row("aaa"),
            make_row("aaa"),
            make_row("aaa"),
        ];
        let mut state = SearchState::new();
        state.set_query("aaa", &rows, 3);
        assert_eq!(state.current, 0);

        state.next();
        assert_eq!(state.current, 1);
        state.next();
        assert_eq!(state.current, 2);
        state.next();
        assert_eq!(state.current, 0); // wraps

        state.prev();
        assert_eq!(state.current, 2); // wraps back
    }

    #[test]
    fn empty_query_no_matches() {
        let rows = vec![make_row("hello")];
        let mut state = SearchState::new();
        state.set_query("", &rows, 5);
        assert_eq!(state.match_count(), 0);
    }

    #[test]
    fn is_match_check() {
        let rows = vec![make_row("hello world")];
        let mut state = SearchState::new();
        state.set_query("world", &rows, 11);
        assert!(state.is_match(0, 6));
        assert!(state.is_match(0, 10));
        assert!(!state.is_match(0, 5));
        assert!(!state.is_match(1, 6));
    }

    #[test]
    fn close_clears_state() {
        let rows = vec![make_row("hello")];
        let mut state = SearchState::new();
        state.open();
        state.set_query("hello", &rows, 5);
        assert!(state.active);
        assert_eq!(state.match_count(), 1);

        state.close();
        assert!(!state.active);
        assert!(state.query.is_empty());
        assert_eq!(state.match_count(), 0);
    }

    #[test]
    fn multiple_matches_same_row() {
        let rows = vec![make_row("aaaa")];
        let mut state = SearchState::new();
        state.set_query("aa", &rows, 4);
        // "aa" in "aaaa" should find overlapping matches at 0, 1, 2
        assert_eq!(state.match_count(), 3);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[1].col_start, 1);
        assert_eq!(state.matches[2].col_start, 2);
    }

    #[test]
    fn is_current_match_identifies_focused() {
        let rows = vec![
            make_row("hello"),
            make_row("hello"),
        ];
        let mut state = SearchState::new();
        state.set_query("hello", &rows, 5);
        assert_eq!(state.match_count(), 2);
        // Current is 0
        assert!(state.is_current_match(0, 0));
        assert!(state.is_current_match(0, 4));
        assert!(!state.is_current_match(1, 0));
        // Navigate to next
        state.next();
        assert!(!state.is_current_match(0, 0));
        assert!(state.is_current_match(1, 0));
    }

    #[test]
    fn current_match_navigation() {
        let rows = vec![
            make_row("abc"),
            make_row("abc"),
            make_row("abc"),
        ];
        let mut state = SearchState::new();
        state.set_query("abc", &rows, 3);
        assert_eq!(state.match_count(), 3);

        let m0 = *state.current_match().unwrap();
        assert_eq!(m0.row, 0);

        state.next();
        let m1 = *state.current_match().unwrap();
        assert_eq!(m1.row, 1);

        state.next();
        let m2 = *state.current_match().unwrap();
        assert_eq!(m2.row, 2);

        state.next();
        let m_wrap = *state.current_match().unwrap();
        assert_eq!(m_wrap.row, 0);

        state.prev();
        let m_prev = *state.current_match().unwrap();
        assert_eq!(m_prev.row, 2);
    }

    #[test]
    fn open_and_close_lifecycle() {
        let mut state = SearchState::new();
        assert!(!state.active);

        state.open();
        assert!(state.active);

        let rows = vec![make_row("test")];
        state.set_query("test", &rows, 4);
        assert_eq!(state.match_count(), 1);

        state.close();
        assert!(!state.active);
        assert!(state.query.is_empty());
        assert_eq!(state.match_count(), 0);
        assert_eq!(state.current, 0);
    }

    #[test]
    fn search_no_match() {
        let rows = vec![
            make_row("hello world"),
            make_row("foo bar"),
        ];
        let mut state = SearchState::new();
        state.set_query("xyz", &rows, 11);
        assert_eq!(state.match_count(), 0);
        assert!(state.current_match().is_none());
    }

    #[test]
    fn test_set_query_updates_existing() {
        let rows = vec![make_row("hello world"), make_row("foo bar")];
        let mut state = SearchState::new();
        state.set_query("hello", &rows, 11);
        assert_eq!(state.match_count(), 1);

        state.set_query("foo", &rows, 11);
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[0].row, 1);
    }

    #[test]
    fn test_navigate_empty_matches() {
        let rows = vec![make_row("hello")];
        let mut state = SearchState::new();
        state.set_query("xyz", &rows, 5);
        assert_eq!(state.match_count(), 0);

        state.next();
        state.prev();
        assert!(state.current_match().is_none());
    }

    #[test]
    fn test_is_match_boundaries() {
        let rows = vec![make_row("hello")];
        let mut state = SearchState::new();
        state.set_query("ell", &rows, 5);
        assert!(state.is_match(0, 1));
        assert!(state.is_match(0, 2));
        assert!(state.is_match(0, 3));
        assert!(!state.is_match(0, 0));
        assert!(!state.is_match(0, 4));
    }
}
