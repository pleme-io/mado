//! URL detection — find clickable URLs in terminal text.
//!
//! Simple state-machine-based URL finder (no regex dependency).
//! Detects http://, https://, and file:// URLs.

use crate::terminal::Cell;

/// A detected URL in a terminal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedUrl {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub url: String,
}

/// Scan a row of cells for URLs.
pub fn detect_urls_in_row(row: &[Cell], cols: usize, row_idx: usize) -> Vec<DetectedUrl> {
    let text = row_to_string(row, cols);
    let mut urls = Vec::new();

    for prefix in &["https://", "http://", "file://"] {
        let mut start = 0;
        while let Some(pos) = text[start..].find(prefix) {
            let abs_start = start + pos;
            let end = find_url_end(text.as_bytes(), abs_start + prefix.len());
            if end > abs_start + prefix.len() {
                urls.push(DetectedUrl {
                    row: row_idx,
                    col_start: abs_start,
                    col_end: end - 1,
                    url: text[abs_start..end].to_string(),
                });
            }
            start = if end > abs_start { end } else { abs_start + 1 };
        }
    }

    urls
}

/// Scan multiple rows for URLs.
pub fn detect_urls(rows: &[Vec<Cell>], cols: usize) -> Vec<DetectedUrl> {
    let mut all = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        all.extend(detect_urls_in_row(row, cols, row_idx));
    }
    all
}

/// Check if a cell position is within a detected URL.
#[must_use]
pub fn url_at(urls: &[DetectedUrl], row: usize, col: usize) -> Option<&DetectedUrl> {
    urls.iter()
        .find(|u| u.row == row && col >= u.col_start && col <= u.col_end)
}

/// Find the end of a URL starting from the given position (after the prefix).
fn find_url_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    let mut paren_depth: i32 = 0;

    while end < bytes.len() {
        let ch = bytes[end];
        match ch {
            // Whitespace terminates
            b' ' | b'\t' | b'\n' | b'\r' => break,
            // Quotes and angle brackets terminate
            b'"' | b'\'' | b'<' | b'>' => break,
            // Track parentheses (for URLs like Wikipedia)
            b'(' => {
                paren_depth += 1;
                end += 1;
            }
            b')' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                    end += 1;
                } else {
                    break;
                }
            }
            _ => end += 1,
        }
    }

    // Trim trailing punctuation
    while end > start && matches!(bytes[end - 1], b'.' | b',' | b';' | b':' | b'!' | b'?') {
        end -= 1;
    }

    end
}

fn row_to_string(row: &[Cell], cols: usize) -> String {
    let mut s = String::with_capacity(cols);
    for cell in row.iter().take(cols) {
        if cell.width == 0 {
            continue;
        }
        s.push(cell.ch);
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
    fn detect_https_url() {
        let row = make_row("visit https://example.com/path for info");
        let urls = detect_urls_in_row(&row, 40, 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com/path");
        assert_eq!(urls[0].col_start, 6);
    }

    #[test]
    fn detect_http_url() {
        let text = "http://localhost:8080/api";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "http://localhost:8080/api");
    }

    #[test]
    fn url_with_parens() {
        let row = make_row("see https://en.wikipedia.org/wiki/Rust_(programming_language) ok");
        let urls = detect_urls_in_row(&row, 65, 0);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].url.contains("Rust_(programming_language)"));
    }

    #[test]
    fn url_with_trailing_punctuation() {
        let row = make_row("check https://example.com.");
        let urls = detect_urls_in_row(&row, 27, 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com");
    }

    #[test]
    fn multiple_urls() {
        let row = make_row("https://a.com and https://b.com");
        let urls = detect_urls_in_row(&row, 31, 0);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "https://a.com");
        assert_eq!(urls[1].url, "https://b.com");
    }

    #[test]
    fn no_urls() {
        let row = make_row("just plain text");
        let urls = detect_urls_in_row(&row, 15, 0);
        assert!(urls.is_empty());
    }

    #[test]
    fn url_at_position() {
        let row = make_row("see https://example.com here");
        let urls = detect_urls_in_row(&row, 28, 0);
        assert!(url_at(&urls, 0, 5).is_some());
        assert!(url_at(&urls, 0, 22).is_some());
        assert!(url_at(&urls, 0, 3).is_none());
        assert!(url_at(&urls, 0, 24).is_none());
    }
}
