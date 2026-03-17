//! URL detection — find clickable URLs in terminal text.
//!
//! Uses the `linkify` crate for robust URL boundary detection,
//! including proper parenthesis handling (Wikipedia URLs) and
//! trailing-punctuation trimming.

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

    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);

    for link in finder.links(&text) {
        urls.push(DetectedUrl {
            row: row_idx,
            col_start: link.start(),
            col_end: link.end().saturating_sub(1),
            url: link.as_str().to_string(),
        });
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
#[allow(dead_code)]
pub fn url_at(urls: &[DetectedUrl], row: usize, col: usize) -> Option<&DetectedUrl> {
    urls.iter()
        .find(|u| u.row == row && col >= u.col_start && col <= u.col_end)
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

    #[test]
    fn test_detect_urls_empty() {
        let row = make_row("");
        let urls = detect_urls_in_row(&row, 0, 0);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_detect_urls_no_urls() {
        let row = make_row("no links here at all");
        let urls = detect_urls_in_row(&row, 20, 0);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_detect_http_url() {
        let text = "visit http://example.com now";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "http://example.com");
        assert_eq!(urls[0].col_start, 6);
    }

    #[test]
    fn test_detect_https_url() {
        let text = "see https://example.com/path";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com/path");
    }

    #[test]
    fn test_detect_multiple_urls() {
        let text = "go to http://one.com and https://two.com/x done";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 2);
        let found: Vec<&str> = urls.iter().map(|u| u.url.as_str()).collect();
        assert!(found.contains(&"http://one.com"));
        assert!(found.contains(&"https://two.com/x"));
    }

    #[test]
    fn test_detect_file_url() {
        let row = make_row("open file:///path/to/file for editing");
        let urls = detect_urls_in_row(&row, 40, 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "file:///path/to/file");
    }

    #[test]
    fn test_url_with_query_params() {
        let row = make_row("see https://example.com?key=value&foo=bar here");
        let urls = detect_urls_in_row(&row, 50, 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com?key=value&foo=bar");
    }

    #[test]
    fn test_url_with_unicode_path() {
        let text = "see https://example.com/café/naïve here";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.chars().count(), 0);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].url.starts_with("https://example.com/"));
    }

    #[test]
    fn test_url_with_fragment() {
        let text = "see https://example.com/page#section here";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].url.starts_with("https://example.com/page"));
    }

    #[test]
    fn test_url_with_port() {
        let text = "http://localhost:3000/api";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "http://localhost:3000/api");
    }

    #[test]
    fn test_url_ftp_detected() {
        let text = "ftp://files.example.com/pub";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        // linkify treats ftp:// as a valid URL scheme
        assert_eq!(urls.len(), 1);
        assert!(urls[0].url.starts_with("ftp://"));
    }

    #[test]
    fn test_url_bare_domain() {
        let text = "go to example.com for info";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        // LinkKind::Url only — bare domains without scheme are not detected
        assert!(urls.is_empty());
    }

    #[test]
    fn test_url_in_angle_brackets() {
        let text = "visit <https://example.com> now";
        let row = make_row(text);
        let urls = detect_urls_in_row(&row, text.len(), 0);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com");
    }
}
