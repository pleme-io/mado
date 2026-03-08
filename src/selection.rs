//! Text selection state machine.
//!
//! Tracks mouse-based text selection (click-drag → cell range → highlight).
//! Used by the renderer to highlight selected cells and by the input handler
//! to extract selected text for clipboard operations.

use crate::terminal::Cell;

/// A cell position in the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPos {
    pub row: usize,
    pub col: usize,
}

/// Selection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    None,
    Selecting { start: CellPos, end: CellPos },
    Selected { start: CellPos, end: CellPos },
}

/// Text selection manager.
pub struct Selection {
    state: State,
}

impl Selection {
    pub fn new() -> Self {
        Self {
            state: State::None,
        }
    }

    /// Begin a new selection at the given cell position.
    pub fn start(&mut self, pos: CellPos) {
        self.state = State::Selecting {
            start: pos,
            end: pos,
        };
    }

    /// Update the selection endpoint as the mouse moves.
    pub fn update(&mut self, pos: CellPos) {
        if let State::Selecting { start, .. } = self.state {
            self.state = State::Selecting { start, end: pos };
        }
    }

    /// Finalize the selection (mouse released).
    pub fn finish(&mut self) {
        if let State::Selecting { start, end } = self.state {
            if start == end {
                self.state = State::None;
            } else {
                self.state = State::Selected { start, end };
            }
        }
    }

    /// Clear the selection.
    pub fn clear(&mut self) {
        self.state = State::None;
    }

    /// Whether a selection is currently active (selecting or selected).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::None)
    }

    /// Get the normalized selection range (start always before end).
    #[must_use]
    pub fn range(&self) -> Option<(CellPos, CellPos)> {
        let (start, end) = match self.state {
            State::None => return None,
            State::Selecting { start, end } | State::Selected { start, end } => (start, end),
        };

        if start.row < end.row || (start.row == end.row && start.col <= end.col) {
            Some((start, end))
        } else {
            Some((end, start))
        }
    }

    /// Check if a cell is within the current selection.
    #[must_use]
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let Some((start, end)) = self.range() else {
            return false;
        };
        if row < start.row || row > end.row {
            return false;
        }
        if start.row == end.row {
            return col >= start.col && col <= end.col;
        }
        if row == start.row {
            return col >= start.col;
        }
        if row == end.row {
            return col <= end.col;
        }
        true
    }

    /// Select the word containing the given cell position.
    ///
    /// Words are delimited by non-alphanumeric/non-underscore characters.
    pub fn select_word(&mut self, pos: CellPos, rows: &[Vec<Cell>], cols: usize) {
        if pos.row >= rows.len() {
            return;
        }
        let row = &rows[pos.row];
        let col = pos.col.min(cols.saturating_sub(1));

        let is_word = |c: char| c.is_alphanumeric() || c == '_';

        let ch = if col < row.len() { row[col].ch } else { ' ' };
        if !is_word(ch) {
            // Click on non-word char: select just that character
            self.state = State::Selected {
                start: CellPos {
                    row: pos.row,
                    col,
                },
                end: CellPos {
                    row: pos.row,
                    col,
                },
            };
            return;
        }

        // Scan left
        let mut start = col;
        while start > 0 && start - 1 < row.len() && is_word(row[start - 1].ch) {
            start -= 1;
        }

        // Scan right
        let mut end = col;
        while end + 1 < cols && end + 1 < row.len() && is_word(row[end + 1].ch) {
            end += 1;
        }

        self.state = State::Selected {
            start: CellPos {
                row: pos.row,
                col: start,
            },
            end: CellPos {
                row: pos.row,
                col: end,
            },
        };
    }

    /// Select the entire line at the given row.
    pub fn select_line(&mut self, row: usize, cols: usize) {
        self.state = State::Selected {
            start: CellPos { row, col: 0 },
            end: CellPos {
                row,
                col: cols.saturating_sub(1),
            },
        };
    }

    /// Extract selected text from terminal rows.
    ///
    /// Returns the selected text as a string, with newlines between rows.
    pub fn extract_text(&self, rows: &[Vec<Cell>], cols: usize) -> Option<String> {
        let (start, end) = self.range()?;
        let mut result = String::new();

        for row_idx in start.row..=end.row {
            if row_idx >= rows.len() {
                break;
            }
            let row = &rows[row_idx];

            let col_start = if row_idx == start.row {
                start.col
            } else {
                0
            };
            let col_end = if row_idx == end.row {
                end.col.min(cols.saturating_sub(1))
            } else {
                cols.saturating_sub(1)
            };

            for col in col_start..=col_end {
                if col < row.len() {
                    row[col].write_to(&mut result);
                }
            }

            // Trim trailing spaces from each line
            if row_idx < end.row {
                let trimmed_len = result.trim_end().len();
                result.truncate(trimmed_len);
                result.push('\n');
            }
        }

        // Trim trailing whitespace from the final line
        let trimmed_len = result.trim_end().len();
        result.truncate(trimmed_len);

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

impl Default for Selection {
    fn default() -> Self {
        Self::new()
    }
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
    fn empty_selection() {
        let sel = Selection::new();
        assert!(!sel.is_active());
        assert!(sel.range().is_none());
        assert!(!sel.contains(0, 0));
    }

    #[test]
    fn single_cell_click_clears() {
        let mut sel = Selection::new();
        sel.start(CellPos { row: 1, col: 3 });
        sel.finish();
        assert!(!sel.is_active());
    }

    #[test]
    fn select_range_on_one_row() {
        let mut sel = Selection::new();
        sel.start(CellPos { row: 0, col: 2 });
        sel.update(CellPos { row: 0, col: 5 });
        sel.finish();

        assert!(sel.is_active());
        assert!(sel.contains(0, 2));
        assert!(sel.contains(0, 3));
        assert!(sel.contains(0, 5));
        assert!(!sel.contains(0, 1));
        assert!(!sel.contains(0, 6));
        assert!(!sel.contains(1, 3));
    }

    #[test]
    fn select_range_multi_row() {
        let mut sel = Selection::new();
        sel.start(CellPos { row: 1, col: 5 });
        sel.update(CellPos { row: 3, col: 2 });
        sel.finish();

        // Row 1: col 5 and beyond
        assert!(!sel.contains(1, 4));
        assert!(sel.contains(1, 5));
        assert!(sel.contains(1, 10));

        // Row 2: all cells
        assert!(sel.contains(2, 0));
        assert!(sel.contains(2, 50));

        // Row 3: up to col 2
        assert!(sel.contains(3, 0));
        assert!(sel.contains(3, 2));
        assert!(!sel.contains(3, 3));

        // Outside rows
        assert!(!sel.contains(0, 5));
        assert!(!sel.contains(4, 0));
    }

    #[test]
    fn reverse_selection_normalizes() {
        let mut sel = Selection::new();
        sel.start(CellPos { row: 3, col: 8 });
        sel.update(CellPos { row: 1, col: 2 });
        sel.finish();

        let (start, end) = sel.range().unwrap();
        assert_eq!(start, CellPos { row: 1, col: 2 });
        assert_eq!(end, CellPos { row: 3, col: 8 });
    }

    #[test]
    fn extract_text_single_row() {
        let rows = vec![make_row("Hello World!")];
        let mut sel = Selection::new();
        sel.start(CellPos { row: 0, col: 6 });
        sel.update(CellPos { row: 0, col: 10 });
        sel.finish();

        let text = sel.extract_text(&rows, 12).unwrap();
        assert_eq!(text, "World");
    }

    #[test]
    fn extract_text_multi_row() {
        let rows = vec![
            make_row("First line  "),
            make_row("Second line "),
            make_row("Third line  "),
        ];
        let mut sel = Selection::new();
        sel.start(CellPos { row: 0, col: 6 });
        sel.update(CellPos { row: 2, col: 4 });
        sel.finish();

        let text = sel.extract_text(&rows, 12).unwrap();
        assert_eq!(text, "line\nSecond line\nThird");
    }

    #[test]
    fn select_word() {
        let rows = vec![make_row("hello world_test foo")];
        let mut sel = Selection::new();
        sel.select_word(CellPos { row: 0, col: 7 }, &rows, 20);

        assert!(sel.is_active());
        let text = sel.extract_text(&rows, 20).unwrap();
        assert_eq!(text, "world_test");
    }

    #[test]
    fn select_word_on_space() {
        let rows = vec![make_row("hello world")];
        let mut sel = Selection::new();
        sel.select_word(CellPos { row: 0, col: 5 }, &rows, 11);

        // Clicking on a space selects that single cell, but extract_text
        // trims whitespace so it returns None — that's correct.
        assert!(sel.is_active());
        assert!(sel.extract_text(&rows, 11).is_none());
    }

    #[test]
    fn select_line() {
        let rows = vec![
            make_row("first line "),
            make_row("second line"),
            make_row("third line "),
        ];
        let mut sel = Selection::new();
        sel.select_line(1, 11);

        assert!(sel.is_active());
        let text = sel.extract_text(&rows, 11).unwrap();
        assert_eq!(text, "second line");
    }

    #[test]
    fn clear_selection() {
        let mut sel = Selection::new();
        sel.start(CellPos { row: 0, col: 0 });
        sel.update(CellPos { row: 0, col: 5 });
        sel.finish();
        assert!(sel.is_active());

        sel.clear();
        assert!(!sel.is_active());
    }
}
