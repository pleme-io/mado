//! URL detection — find clickable URLs in terminal text.
//!
//! Uses the `linkify` crate for robust URL boundary detection,
//! including proper parenthesis handling (Wikipedia URLs) and
//! trailing-punctuation trimming.

use crate::grid_col::glyph_columns;
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
///
/// linkify reports **byte** ranges into the row's text, but the renderer
/// (`render.rs` URL underline) and the click hit-test (`url_at`) both
/// consume **display columns**. The two diverge the instant a row holds
/// a multi-byte glyph (box-drawing, an arrow, an emoji) or a wide CJK
/// cell — exactly the content a TUI paints. Aliasing byte offsets as
/// columns drew the underline shifted right of its link, landing under
/// unrelated text ("everything is underlined"). We translate byte→column
/// through [`glyph_columns`] — the crate's single source of column truth
/// (`crate::grid_col`) — so a URL's columns can never diverge from where
/// the renderer paints the glyph.
pub fn detect_urls_in_row(row: &[Cell], cols: usize, row_idx: usize) -> Vec<DetectedUrl> {
    let RowColumns {
        text,
        start_col,
        end_col,
    } = row_text_and_columns(row, cols);

    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);

    let mut urls = Vec::new();
    for link in finder.links(&text) {
        // `start_col[b]` / `end_col[b]` are the first / last grid column
        // of the cell byte `b` belongs to (they differ only for a wide
        // cell). `link.end()` is exclusive, so the URL's last byte is
        // `link.end() - 1`, and that cell's last column is the URL's
        // last occupied column.
        let col_start = start_col[link.start()];
        let col_end = end_col[link.end() - 1].max(col_start);
        urls.push(DetectedUrl {
            row: row_idx,
            col_start,
            col_end,
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
pub fn url_at(urls: &[DetectedUrl], row: usize, col: usize) -> Option<&DetectedUrl> {
    urls.iter()
        .find(|u| u.row == row && col >= u.col_start && col <= u.col_end)
}

/// The OS URL-opener program for the current platform. macOS uses the
/// absolute `/usr/bin/open` (no PATH dependence); other platforms use
/// `xdg-open`. NEVER a shell.
#[must_use]
fn os_opener() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "/usr/bin/open"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "xdg-open"
    }
}

/// Split a `file://`-stripped target into `(path, Some(line))` when it ends
/// in `:<digits>`, else `(target, None)` — the `file:///path/to/file:42`
/// editor-jump convention.
fn split_file_line(target: &str) -> (&str, Option<u32>) {
    if let Some((path, suffix)) = target.rsplit_once(':') {
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = suffix.parse::<u32>() {
                return (path, Some(n));
            }
        }
    }
    (target, None)
}

/// Build the typed `(program, args)` for opening `url`, given the resolved
/// `editor` (`$VISUAL`/`$EDITOR`/`nvim`) used for `file://` targets:
///
/// - `http`/`https`/`ftp`/… → the OS opener with the URL as its one arg.
/// - `file://path` → the editor on `path` (with a `+LINE` arg prepended
///   when the target carries a `:LINE` suffix).
///
/// PURE — no env reads, no spawn — so the argv shape is unit-testable
/// without launching a process. NO shell, NO `format!()`: the returned argv
/// is handed straight to `std::process::Command::args`.
#[must_use]
pub fn open_command(url: &str, editor: &str) -> (String, Vec<String>) {
    if let Some(rest) = url.strip_prefix("file://") {
        // `file:///path/to/file` → `/path/to/file`. Peel an optional `:LINE`.
        let (path, line) = split_file_line(rest);
        let mut args = Vec::new();
        if let Some(n) = line {
            // `+<n>` without `format!()` (TYPED EMISSION): Display via to_string.
            let mut jump = String::from("+");
            jump.push_str(&n.to_string());
            args.push(jump);
        }
        args.push(path.to_string());
        return (editor.to_string(), args);
    }
    (os_opener().to_string(), vec![url.to_string()])
}

/// Open a clickable link with a typed `std::process::Command` (NO shell):
/// an `http`/`https`/`ftp` URL through the OS opener, a `file://path:line`
/// through the operator's `$VISUAL`/`$EDITOR` (fallback `nvim`). Spawns
/// detached (never blocks); the caller logs any error. Never panics.
pub fn open_link(url: &str) -> std::io::Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "nvim".to_string());
    let (program, args) = open_command(url, &editor);
    std::process::Command::new(&program)
        .args(&args)
        .spawn()
        .map(|_child| ())
}

/// The row text linkify scans, plus the per-byte first/last grid column
/// of the cell each byte belongs to. `start_col`/`end_col` hold exactly
/// one entry per byte of `text`.
struct RowColumns {
    text: String,
    start_col: Vec<usize>,
    end_col: Vec<usize>,
}

/// Build the linkify text and, in lockstep, the per-byte first/last grid
/// column — sourced from [`glyph_columns`], the crate's single source of
/// column truth, so URL columns share the renderer's exact notion of
/// where each glyph sits. The width-0 continuation cells of wide glyphs
/// are skipped by `glyph_columns` and a wide lead cell's `end_col` is
/// `start + width - 1`, so a URL ending in a wide glyph stays covered.
fn row_text_and_columns(row: &[Cell], cols: usize) -> RowColumns {
    let mut text = String::with_capacity(cols);
    let mut start_col = Vec::with_capacity(cols);
    let mut end_col = Vec::with_capacity(cols);
    for (gc, cell) in glyph_columns(row, cols) {
        let first = gc.idx();
        let last = first + (cell.width as usize).saturating_sub(1);
        let mut buf = [0u8; 4];
        let encoded = cell.ch.encode_utf8(&mut buf);
        for _ in 0..encoded.len() {
            start_col.push(first);
            end_col.push(last);
        }
        text.push_str(encoded);
    }
    RowColumns {
        text,
        start_col,
        end_col,
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

    /// THE invariant: `col_start`/`col_end` are DISPLAY COLUMNS, never
    /// linkify's byte offsets. Multi-byte-but-single-column box-drawing
    /// glyphs (3 bytes, 1 column each) precede the URL, so the byte
    /// offset (10) and the display column (4) diverge. Before the fix the
    /// underline/click rect used the byte offset and smeared right of the
    /// link — the "everything is underlined" bug. This row is the kind a
    /// TUI (e.g. Claude Code) paints constantly.
    #[test]
    fn url_columns_are_display_columns_not_byte_offsets() {
        let prefix = "│─→ "; // 3 box glyphs (3 bytes each) + space → 4 cols, 10 bytes
        let url = "https://example.com";
        let row = make_row(&format!("{prefix}{url}"));
        let cols = prefix.chars().count() + url.chars().count();

        let urls = detect_urls_in_row(&row, cols, 0);

        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, url);
        assert_eq!(
            urls[0].col_start, 4,
            "col_start must be the display column (4), not the byte offset (10)"
        );
        assert_eq!(
            urls[0].col_end,
            4 + url.chars().count() - 1,
            "col_end must be the display column of the URL's last cell"
        );
        // url_at — the click hit-test — must agree with the columns. The
        // old code put col_start at the byte offset (10), so the URL's
        // TRUE start column (4) would not hit; the fix makes it hit, and
        // the blank column before the URL (3) stays a miss.
        assert!(
            url_at(&urls, 0, 4).is_some(),
            "the URL's true start column must hit"
        );
        assert!(
            url_at(&urls, 0, 3).is_none(),
            "the space before the URL must not hit"
        );
    }

    /// The click hit-test must hit columns strictly INSIDE the URL span
    /// (a click in the middle of a link opens it), not just the boundaries.
    #[test]
    fn url_at_hits_interior_column_for_click() {
        let row = make_row("go to https://example.com/path now");
        let urls = detect_urls_in_row(&row, 34, 7);
        assert_eq!(urls.len(), 1);
        let u = &urls[0];
        let mid = (u.col_start + u.col_end) / 2;
        assert!(mid > u.col_start && mid < u.col_end, "mid must be interior");
        assert!(url_at(&urls, 7, mid).is_some(), "interior column must hit");
        // A different row never hits, and the cell just before the URL misses.
        assert!(url_at(&urls, 8, mid).is_none(), "wrong row must miss");
        assert!(url_at(&urls, 7, u.col_start.saturating_sub(1)).is_none());
    }

    #[test]
    fn open_command_http_uses_os_opener_with_url_arg() {
        let (prog, args) = open_command("https://example.com/x?y=1", "nvim");
        assert_eq!(args, vec!["https://example.com/x?y=1".to_string()]);
        #[cfg(target_os = "macos")]
        assert_eq!(prog, "/usr/bin/open");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(prog, "xdg-open");
    }

    #[test]
    fn open_command_ftp_uses_os_opener() {
        let (_prog, args) = open_command("ftp://files.example.com/pub", "nvim");
        assert_eq!(args, vec!["ftp://files.example.com/pub".to_string()]);
    }

    #[test]
    fn open_command_file_with_line_jumps_in_editor() {
        let (prog, args) = open_command("file:///home/u/src/main.rs:42", "nvim");
        assert_eq!(prog, "nvim");
        assert_eq!(
            args,
            vec!["+42".to_string(), "/home/u/src/main.rs".to_string()],
            "file://path:line opens the editor at +LINE then the path",
        );
    }

    #[test]
    fn open_command_file_without_line_opens_editor_on_path() {
        let (prog, args) = open_command("file:///home/u/src/main.rs", "hx");
        assert_eq!(prog, "hx");
        assert_eq!(args, vec!["/home/u/src/main.rs".to_string()]);
    }

    /// Wide (2-column) cells before the URL advance the display column by
    /// 2 while contributing one char + a width-0 continuation cell. The
    /// column map must honour the cell width, not the char count.
    #[test]
    fn url_columns_account_for_wide_cells_before_the_url() {
        let url = "https://a.co";
        let mut row = vec![
            Cell {
                ch: '🚀',
                width: 2,
                ..Cell::default()
            }, // cols 0..=1
            Cell {
                ch: ' ',
                width: 0,
                ..Cell::default()
            }, // wide continuation — no column
            Cell {
                ch: ' ',
                width: 1,
                ..Cell::default()
            }, // col 2
        ];
        row.extend(url.chars().map(|ch| Cell {
            ch,
            width: 1,
            ..Cell::default()
        }));
        let cols = row.len();

        let urls = detect_urls_in_row(&row, cols, 0);

        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, url);
        assert_eq!(
            urls[0].col_start, 3,
            "a wide cell before the URL must advance 2 display columns"
        );
        assert_eq!(urls[0].col_end, 3 + url.chars().count() - 1);
    }
}
