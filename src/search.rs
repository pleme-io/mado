//! Scrollback search — find text in terminal history.
//!
//! Two matchers, ONE case rule — now held by a TEST rather than by a
//! shared function, and that is a real weakening worth stating plainly.
//! [`SearchMatcher::Literal`] still decides "is this character the same
//! as that one?" through [`char_eq`]. [`SearchMatcher::Regex`] is the
//! `regex` crate, which must fold at COMPILE time
//! (`RegexBuilder::case_insensitive`) and so cannot route through a
//! per-character predicate at all. The two implementations are kept
//! honest by `ignore_case_means_the_same_thing_in_both_matchers`, which
//! runs the same toggle over the same grid through both matchers and
//! asserts the match lists are equal — so a divergence is a red test,
//! not a silent second meaning.
//!
//! Matching runs across the terminal grid (scrollback + visible area
//! — the engine feeds `Terminal::search_rows`, capped at the most
//! recent `SEARCH_SCROLLBACK_CAP` history rows for cost). Match rows
//! are ABSOLUTE grid indices (scrollback origin 0), so they stay
//! valid while the viewport scrolls; navigation adjusts the viewport
//! to bring the active match into view and the renderer maps absolute
//! rows onto the current viewport at draw time.
//!
//! **An invalid regex REFUSES; it never degrades to literal.** A
//! pattern that does not compile leaves `matches` empty and records a
//! typed [`PatternError`] on the state ([`SearchState::pattern_error`]).
//! Quietly re-reading `a(b` as the four literal characters `a`, `(`,
//! `b` would answer a question the operator did not ask, and answer it
//! with matches that look exactly like real ones — a search that
//! silently changes meaning is worse than one that refuses.
//!
//! **Match columns are CHARACTER indices into the row's text**, not
//! byte offsets (that aliasing is the bug `url.rs` documents at
//! length). They are still not display columns for a wide CJK cell —
//! closing that needs `grid_col::glyph_columns`, the same fix `url.rs`
//! took, and it is not this module's change to make today.

use crate::terminal::Cell;

/// A single match location in the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    /// ABSOLUTE row index: 0 = oldest retained scrollback row;
    /// `scrollback_total()` = first live-screen row. Stable across
    /// viewport scrolling (a viewport-relative row went stale the
    /// moment the view moved — hunt finding 2026-06-11).
    pub row: usize,
    /// Starting column (inclusive).
    pub col_start: usize,
    /// Ending column (inclusive).
    pub col_end: usize,
}

/// How the query text is interpreted. Literal is the default; regex
/// is opt-in, so no existing caller changes meaning by upgrading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)] // `Regex` is opt-in; no production caller selects it yet.
pub enum SearchMatcher {
    /// The query is a plain substring. Matches may OVERLAP (`aa` finds
    /// three matches in `aaaa`) — the historical behaviour, kept.
    #[default]
    Literal,
    /// The query is a regular expression. Matches are NON-overlapping
    /// and leftmost-FIRST — `regex`'s preference-order semantics, the
    /// same as Perl/PCRE and every other mainstream tool, where the
    /// hand-rolled engine this replaced was POSIX leftmost-longest.
    /// They agree on everything greedy (`(v1)?\.2` still takes the
    /// optional group); they differ on an alternation whose earlier
    /// branch is shorter, so `a|ab` now stops at `a`.
    Regex,
}

/// Why a regex query would not compile. Typed rather than a message
/// string so a caller can branch on the cause, and `Display` gives the
/// status line something to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternError {
    /// `(` with no matching `)`.
    UnclosedGroup,
    /// `)` with no `(` to close.
    UnopenedGroup,
    /// `[` with no matching `]`.
    UnclosedClass,
    /// `*`, `+` or `?` with nothing in front of it to repeat.
    NothingToRepeat,
    /// A trailing `\` that escapes nothing.
    TrailingEscape,
    /// A negated shorthand (`\D`, `\W`, `\S`) inside `[...]`.
    ///
    /// **This is now a deliberate self-imposed restriction, not a
    /// limitation.** The hand-rolled engine held a class as a flat list
    /// of ranges plus one `negated` bit, which genuinely could not
    /// represent "everything except a digit, unioned with the rest of
    /// this class" — so it refused rather than misread `[\D]` as a
    /// literal `D`. `regex` has nested classes and accepts `[\D]` fine.
    /// The refusal is kept because it is the pinned contract
    /// (`invalid_regex_causes_are_typed`); relaxing it is a behaviour
    /// change for an operator to make on purpose, not a side effect of
    /// swapping engines. Enforced by [`preflight`].
    /// Nested past [`MAX_GROUP_DEPTH`]. The hand-rolled parser descended
    /// once per `(` and would have overflowed the stack; `regex` reaches
    /// the same refusal through `RegexBuilder::nest_limit`.
    TooDeeplyNested,
    /// Invalid for a reason with no dedicated variant above — an
    /// out-of-order repetition count (`a{3,1}`), a reversed class range
    /// (`[z-a]`), an unknown Unicode property, a program that exceeds
    /// `regex`'s size limit.
    ///
    /// New with the `regex` swap, and unavoidable: the hand-rolled
    /// grammar had exactly six failure modes, so an exhaustive typed
    /// list was possible. A real regex parser has dozens. Rounding them
    /// into a neighbouring variant would name the wrong cause, which is
    /// worse than admitting the cause is not enumerated.
    Invalid,
}

impl PatternError {
    /// Operator-facing text. `&'static str` so rendering it never
    /// allocates and never needs `format!()` (★★ TYPED EMISSION).
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::UnclosedGroup => "unclosed `(`",
            Self::UnopenedGroup => "unmatched `)`",
            Self::UnclosedClass => "unclosed `[`",
            Self::NothingToRepeat => "nothing to repeat",
            Self::TrailingEscape => "trailing `\\`",
            Self::TooDeeplyNested => "groups nested too deeply",
            Self::Invalid => "invalid pattern",
        }
    }
}

impl std::fmt::Display for PatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
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
    /// Case-insensitive search. Applies in BOTH matchers and means the
    /// same thing in both — [`char_eq`] for literal,
    /// `RegexBuilder::case_insensitive` for regex, held to one meaning
    /// by the cross-matcher agreement test (see the module doc).
    pub ignore_case: bool,
    /// How [`Self::query`] is read. Literal by default.
    pub matcher: SearchMatcher,
    /// Set when the query is a regex that would not compile. While
    /// this is `Some`, `matches` is EMPTY — the search refused rather
    /// than falling back to literal.
    pattern_error: Option<PatternError>,
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
            matcher: SearchMatcher::default(),
            pattern_error: None,
        }
    }

    /// The compile error for the current regex query, if any. `None`
    /// in literal mode and whenever the pattern compiled.
    ///
    /// A caller rendering the search status MUST surface this: zero
    /// matches from a broken pattern and zero matches from a pattern
    /// that simply is not there look identical otherwise.
    #[must_use]
    #[allow(dead_code)]
    pub fn pattern_error(&self) -> Option<PatternError> {
        self.pattern_error
    }

    /// Switch matcher and re-run against the same grid, so a toggle can
    /// never leave matches from the other interpretation on screen.
    #[allow(dead_code)]
    pub fn set_matcher(
        &mut self,
        matcher: SearchMatcher,
        rows: &[Vec<Cell>],
        cols: usize,
        first_abs: usize,
    ) {
        self.matcher = matcher;
        let query = self.query.clone();
        self.set_query(&query, rows, cols, first_abs);
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
        self.pattern_error = None;
    }

    /// Update the query and re-search the grid. `rows` is the slice
    /// [`crate::terminal::Terminal::search_rows`] returns;
    /// `first_abs` is the absolute index of `rows[0]` so matches get
    /// absolute addresses.
    ///
    /// In [`SearchMatcher::Regex`] mode a pattern that does not compile
    /// leaves `matches` empty and records the reason on
    /// [`Self::pattern_error`]; it is NEVER retried as a literal.
    pub fn set_query(&mut self, query: &str, rows: &[Vec<Cell>], cols: usize, first_abs: usize) {
        self.query = query.to_string();
        self.matches.clear();
        self.current = 0;
        self.pattern_error = None;

        if query.is_empty() {
            return;
        }

        let ignore_case = self.ignore_case;
        self.matches = match self.matcher {
            SearchMatcher::Literal => {
                let needle: Vec<char> = query.chars().collect();
                scan_rows(rows, cols, first_abs, |line| {
                    literal_spans(line, &needle, ignore_case)
                })
            }
            // `ignore_case` is baked into the compiled program rather
            // than passed per row: `regex` folds at build time, so the
            // toggle has to reach `RegexBuilder`, not the scan.
            SearchMatcher::Regex => match Pattern::compile(query, ignore_case) {
                Ok(pattern) => scan_rows(rows, cols, first_abs, |line| pattern.spans(line)),
                Err(err) => {
                    self.pattern_error = Some(err);
                    return;
                }
            },
        };
    }

    /// Append typed text to the live query and re-run the search.
    /// Shared by both event loops' search-overlay input routing
    /// (`main.rs` local-PTY, `gui_tear_attach.rs` embedded-tear) so
    /// the query-edit semantics cannot drift.
    pub fn append_query(&mut self, text: &str, rows: &[Vec<Cell>], cols: usize, first_abs: usize) {
        let mut query = self.query.clone();
        query.push_str(text);
        self.set_query(&query, rows, cols, first_abs);
    }

    /// Remove the last query character (Backspace) and re-run the
    /// search. Counterpart of [`Self::append_query`].
    pub fn backspace_query(&mut self, rows: &[Vec<Cell>], cols: usize, first_abs: usize) {
        let mut query = self.query.clone();
        query.pop();
        self.set_query(&query, rows, cols, first_abs);
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
    #[allow(dead_code)]
    pub fn current_match(&self) -> Option<&SearchMatch> {
        self.matches.get(self.current)
    }

    /// Total number of matches.
    #[must_use]
    #[allow(dead_code)]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Check if a cell position is within any match.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_match(&self, row: usize, col: usize) -> bool {
        self.matches
            .iter()
            .any(|m| m.row == row && col >= m.col_start && col <= m.col_end)
    }

    /// Check if a cell position is within the current (focused) match.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_current_match(&self, row: usize, col: usize) -> bool {
        self.matches
            .get(self.current)
            .is_some_and(|m| m.row == row && col >= m.col_start && col <= m.col_end)
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait abstracting search queries for testability.
#[allow(dead_code)]
pub trait SearchQuery {
    fn is_active(&self) -> bool;
    fn is_match(&self, row: usize, col: usize) -> bool;
    fn is_current_match(&self, row: usize, col: usize) -> bool;
    fn match_count(&self) -> usize;
}

impl SearchQuery for SearchState {
    fn is_active(&self) -> bool {
        self.active
    }

    fn is_match(&self, row: usize, col: usize) -> bool {
        SearchState::is_match(self, row, col)
    }

    fn is_current_match(&self, row: usize, col: usize) -> bool {
        SearchState::is_current_match(self, row, col)
    }

    fn match_count(&self) -> usize {
        SearchState::match_count(self)
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

// ---------------------------------------------------------------------------
// The one case rule
// ---------------------------------------------------------------------------

/// The definition of `ignore_case` for the LITERAL matcher. The regex
/// matcher folds inside `regex` (`RegexBuilder::case_insensitive`, which
/// is Unicode *simple case folding*) because it has to fold at compile
/// time; the cross-matcher agreement test is what holds the two to one
/// meaning. See the module doc.
///
/// Folding per character (rather than lowercasing whole strings, which
/// is what the literal path used to do) also keeps the column count
/// honest: `'İ'.to_lowercase()` yields TWO chars, so a whole-string fold
/// silently shifts every column to its right.
fn char_eq(a: char, b: char, ignore_case: bool) -> bool {
    a == b || (ignore_case && a.to_lowercase().eq(b.to_lowercase()))
}

// ---------------------------------------------------------------------------
// Row scanning
// ---------------------------------------------------------------------------

/// Run `spans` over every row and lift the `(start, end_exclusive)`
/// character ranges it reports into absolute-addressed [`SearchMatch`]es.
/// Zero-width spans are dropped: they highlight nothing, and
/// `end - 1` on one would underflow.
/// `spans` receives the row as a `&str` (what `regex` searches) and
/// returns CHARACTER ranges — the conversion out of `regex`'s byte
/// offsets happens inside [`Pattern::spans`], so nothing downstream ever
/// sees a byte offset.
fn scan_rows<F>(rows: &[Vec<Cell>], cols: usize, first_abs: usize, mut spans: F) -> Vec<SearchMatch>
where
    F: FnMut(&str) -> Vec<(usize, usize)>,
{
    let last_col = cols.saturating_sub(1);
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let line = row_to_string(row, cols);
        for (start, end) in spans(&line) {
            if end <= start {
                continue;
            }
            out.push(SearchMatch {
                row: first_abs + row_idx,
                col_start: start,
                col_end: (end - 1).min(last_col),
            });
        }
    }
    out
}

/// Every substring occurrence of `needle` in `line`, OVERLAPPING — the
/// historical literal behaviour (`aa` reports three hits in `aaaa`).
fn literal_spans(line: &str, needle: &[char], ignore_case: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    // Indexed by CHARACTER throughout — the row is collected rather than
    // scanned as bytes so `start` is already the column the renderer wants.
    let line: Vec<char> = line.chars().collect();
    if needle.is_empty() || needle.len() > line.len() {
        return out;
    }
    for start in 0..=(line.len() - needle.len()) {
        if needle
            .iter()
            .enumerate()
            .all(|(i, &nc)| char_eq(line[start + i], nc, ignore_case))
        {
            out.push((start, start + needle.len()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The regex engine
// ---------------------------------------------------------------------------
//
// The `regex` crate, reached through exactly two methods — [`Pattern::compile`]
// and [`Pattern::spans`]. This module previously carried a ~570-line
// hand-rolled Thompson NFA (parser, AST, instruction set, thread-list
// simulation) written only because `regex` sat in `Cargo.lock` transitively:
// BUILT but not USABLE, since nothing declared it in `Cargo.toml`. Declaring it
// deleted the engine; the confinement to `compile`/`spans` is what made that a
// small change rather than a rewrite.
//
// Two obligations survive the swap, because `regex` does not hand them over:
//
//   1. `regex` reports BYTE offsets. Every consumer here reads CHARACTER
//      indices (the renderer multiplies a column by `cell_width`), so
//      `spans` converts. `columns_are_char_indices_not_byte_offsets` is the
//      pin, and it is not decorative: a row of 3-byte box-drawing glyphs
//      reports a match three columns per glyph too far right if this is
//      skipped.
//   2. `regex::Error` carries a rendered human-readable string, not a cause an
//      operator's UI can branch on. [`classify`] re-parses through
//      `regex_syntax`'s AST parser, which DOES expose a typed kind, and maps
//      it onto [`PatternError`].
//
// The pathological-pattern guarantee is now the crate's rather than ours:
// `regex` is a finite automaton with no backtracking, so `(a*)*b` is linear in
// the row length. `pathological_patterns_terminate` still guards it — if that
// ever regresses the test does not fail, it hangs, and that is the signal.

use regex::{Regex, RegexBuilder};
use regex_syntax::ast;

/// Nesting past this is refused. The hand-rolled parser descended once per `(`
/// and a 500-deep pattern overflowed its stack.
///
/// Enforced from BOTH sides, because neither alone reproduces the old
/// behaviour. `RegexBuilder::nest_limit` counts all AST nesting (groups,
/// alternations, repetitions) but only via a visitor over a *completed* AST,
/// so it never runs on a pattern that fails to parse — `"(".repeat(500)` dies
/// at EOF as an unclosed group instead. [`preflight`] therefore counts group
/// depth up front, which is where the old parser checked it.
const MAX_GROUP_DEPTH: u32 = 64;

/// A compiled pattern. `ignore_case` is folded IN here rather than applied per
/// row, because `regex` decides case at build time.
struct Pattern {
    re: Regex,
}

impl Pattern {
    fn compile(pattern: &str, ignore_case: bool) -> Result<Self, PatternError> {
        if let Some(err) = preflight(pattern) {
            return Err(err);
        }
        RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .nest_limit(MAX_GROUP_DEPTH)
            .build()
            .map(|re| Self { re })
            .map_err(|_| classify(pattern))
    }

    /// Every non-overlapping match in `text`, as `(start, end_exclusive)`
    /// CHARACTER indices.
    ///
    /// An empty match (`a*` against `bbb`) is reported as a zero-width span and
    /// dropped by [`scan_rows`] — `find_iter` advances past it itself, so
    /// there is no loop to break here the way the hand-rolled `spans` had to.
    fn spans(&self, text: &str) -> Vec<(usize, usize)> {
        let mut cursor = CharCursor::new(text);
        self.re
            .find_iter(text)
            .map(|m| {
                (
                    cursor.char_index_at(m.start()),
                    cursor.char_index_at(m.end()),
                )
            })
            .collect()
    }
}

/// Byte offset → character index, walked forward exactly once per row.
///
/// `find_iter` yields non-overlapping matches in increasing byte order, so
/// every boundary asked about is at or after the previous one and a single
/// cursor answers them all without allocating a table.
struct CharCursor<'a> {
    rest: std::str::Chars<'a>,
    byte: usize,
    idx: usize,
}

impl<'a> CharCursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            rest: text.chars(),
            byte: 0,
            idx: 0,
        }
    }

    fn char_index_at(&mut self, byte: usize) -> usize {
        while self.byte < byte {
            let Some(ch) = self.rest.next() else { break };
            self.byte += ch.len_utf8();
            self.idx += 1;
        }
        self.idx
    }
}

/// The two refusals `regex` will not make for us, checked in ONE left-to-right
/// scan before the pattern is handed over. Scanning left to right is what makes
/// the precedence between them right: the old recursive-descent parser reported
/// whichever it reached first, and so does this.
///
/// **`[\D]` used to be refused here, and no longer is.** The hand-rolled class
/// was a range list plus a `negated` bit, which could not represent `[\D]`'s
/// set union, so it refused rather than misread it as a literal `D`. That was a
/// LIMITATION of the old engine, and a test pinned it — so the engine swap
/// briefly re-implemented the refusal, ~20 lines of scan whose only job was to
/// keep a capability out. `regex` has nested classes and accepts `[\D]`; the
/// refusal and its pinning test are gone. A test that pins an implementation's
/// shortcoming as a contract will outlive the implementation if you let it.
///
/// **Group depth — a refusal `regex` makes too LATE.** `regex_syntax` enforces
/// its nest limit with a visitor over the *completed* AST, so `((((…` 500 deep
/// fails at EOF as an unclosed group and the depth is never reached; the old
/// parser checked depth as it descended, so depth won. Counting here restores
/// that precedence. (`RegexBuilder::nest_limit` is still set — it catches
/// non-group nesting, like stacked repetitions, that this scan does not count.)
///
/// Scans conservatively: a nested class (`[\d[a-z]]`) ends the tracked class
/// early, so a shorthand after it is missed. That direction is safe — the miss
/// means `regex` handles the pattern, which is the better answer anyway.
fn preflight(pattern: &str) -> Option<PatternError> {
    let mut chars = pattern.chars();
    let mut in_class = false;
    // Characters consumed since `[`, so the `]`-in-first-position-is-a-literal
    // rule (and `[^]...]`) reads correctly rather than closing the class.
    let mut class_len = 0usize;
    let mut depth: u32 = 0;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Consume the escaped char so it cannot be read as a class
                // delimiter; the escape itself needs no verdict here.
                let _ = chars.next();
                class_len += usize::from(in_class);
            }
            '[' if !in_class => {
                in_class = true;
                class_len = 0;
            }
            // A leading `^` is the negation marker, not class content.
            '^' if in_class && class_len == 0 => {}
            ']' if in_class && class_len > 0 => in_class = false,
            '(' if !in_class => {
                depth += 1;
                if depth > MAX_GROUP_DEPTH {
                    return Some(PatternError::TooDeeplyNested);
                }
            }
            ')' if !in_class => depth = depth.saturating_sub(1),
            _ => class_len += usize::from(in_class),
        }
    }
    None
}

/// Turn a `regex` compile failure into a typed cause.
///
/// `regex::Error::Syntax` carries only a rendered multi-line message, so
/// branching on it would mean matching on prose. `regex_syntax`'s AST parser
/// answers the same question with a typed `ErrorKind`, so the pattern is
/// re-parsed here purely to classify. It runs only on the failure path.
///
/// `ErrorKind` is `#[non_exhaustive]` and much wider than the old grammar's six
/// failure modes; anything unmapped is [`PatternError::Invalid`] rather than a
/// nearby variant that would name the wrong cause.
fn classify(pattern: &str) -> PatternError {
    let parsed = ast::parse::ParserBuilder::new()
        .nest_limit(MAX_GROUP_DEPTH)
        .build()
        .parse(pattern);
    let Err(err) = parsed else {
        // Parsed as an AST but failed to build: a translate-time or
        // size-limit refusal, which has no syntactic cause to report.
        return PatternError::Invalid;
    };
    match err.kind() {
        ast::ErrorKind::GroupUnclosed => PatternError::UnclosedGroup,
        ast::ErrorKind::GroupUnopened => PatternError::UnopenedGroup,
        ast::ErrorKind::ClassUnclosed => PatternError::UnclosedClass,
        ast::ErrorKind::RepetitionMissing => PatternError::NothingToRepeat,
        ast::ErrorKind::EscapeUnexpectedEof => PatternError::TrailingEscape,
        ast::ErrorKind::NestLimitExceeded(_) => PatternError::TooDeeplyNested,
        _ => PatternError::Invalid,
    }
}

#[cfg(test)]
pub struct MockSearchState {
    pub active: bool,
    pub matches: Vec<(usize, usize, usize)>, // (row, col_start, col_end)
    pub current_idx: usize,
}

#[cfg(test)]
impl SearchQuery for MockSearchState {
    fn is_active(&self) -> bool {
        self.active
    }

    fn is_match(&self, row: usize, col: usize) -> bool {
        self.matches
            .iter()
            .any(|&(r, cs, ce)| r == row && col >= cs && col <= ce)
    }

    fn is_current_match(&self, row: usize, col: usize) -> bool {
        self.matches
            .get(self.current_idx)
            .is_some_and(|&(r, cs, ce)| r == row && col >= cs && col <= ce)
    }

    fn match_count(&self) -> usize {
        self.matches.len()
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
    fn basic_search() {
        let rows = vec![
            make_row("hello world"),
            make_row("hello again"),
            make_row("goodbye world"),
        ];
        let mut state = SearchState::new();
        state.set_query("hello", &rows, 13, 0);
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
        state.set_query("hello", &rows, 17, 0);
        assert_eq!(state.match_count(), 3);
    }

    #[test]
    fn case_sensitive() {
        let rows = vec![make_row("Hello HELLO hello")];
        let mut state = SearchState::new();
        state.ignore_case = false;
        state.set_query("hello", &rows, 17, 0);
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 12);
    }

    #[test]
    fn navigate_matches() {
        let rows = vec![make_row("aaa"), make_row("aaa"), make_row("aaa")];
        let mut state = SearchState::new();
        state.set_query("aaa", &rows, 3, 0);
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
        state.set_query("", &rows, 5, 0);
        assert_eq!(state.match_count(), 0);
    }

    #[test]
    fn append_query_builds_incrementally() {
        let rows = vec![make_row("hello world")];
        let mut state = SearchState::new();
        state.open();
        state.append_query("he", &rows, 11, 0);
        assert_eq!(state.query, "he");
        assert_eq!(state.match_count(), 1);
        state.append_query("llo", &rows, 11, 0);
        assert_eq!(state.query, "hello");
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[0].col_end, 4);
    }

    #[test]
    fn backspace_query_pops_and_researches() {
        let rows = vec![make_row("ab abc")];
        let mut state = SearchState::new();
        state.open();
        state.append_query("abc", &rows, 6, 0);
        assert_eq!(state.match_count(), 1);
        state.backspace_query(&rows, 6, 0);
        assert_eq!(state.query, "ab");
        assert_eq!(state.match_count(), 2);
        // Backspacing past empty stays empty (no matches, no panic).
        state.backspace_query(&rows, 6, 0);
        state.backspace_query(&rows, 6, 0);
        state.backspace_query(&rows, 6, 0);
        assert!(state.query.is_empty());
        assert_eq!(state.match_count(), 0);
    }

    #[test]
    fn is_match_check() {
        let rows = vec![make_row("hello world")];
        let mut state = SearchState::new();
        state.set_query("world", &rows, 11, 0);
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
        state.set_query("hello", &rows, 5, 0);
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
        state.set_query("aa", &rows, 4, 0);
        // "aa" in "aaaa" should find overlapping matches at 0, 1, 2
        assert_eq!(state.match_count(), 3);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[1].col_start, 1);
        assert_eq!(state.matches[2].col_start, 2);
    }

    #[test]
    fn is_current_match_identifies_focused() {
        let rows = vec![make_row("hello"), make_row("hello")];
        let mut state = SearchState::new();
        state.set_query("hello", &rows, 5, 0);
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
        let rows = vec![make_row("abc"), make_row("abc"), make_row("abc")];
        let mut state = SearchState::new();
        state.set_query("abc", &rows, 3, 0);
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
        state.set_query("test", &rows, 4, 0);
        assert_eq!(state.match_count(), 1);

        state.close();
        assert!(!state.active);
        assert!(state.query.is_empty());
        assert_eq!(state.match_count(), 0);
        assert_eq!(state.current, 0);
    }

    #[test]
    fn search_no_match() {
        let rows = vec![make_row("hello world"), make_row("foo bar")];
        let mut state = SearchState::new();
        state.set_query("xyz", &rows, 11, 0);
        assert_eq!(state.match_count(), 0);
        assert!(state.current_match().is_none());
    }

    #[test]
    fn test_set_query_updates_existing() {
        let rows = vec![make_row("hello world"), make_row("foo bar")];
        let mut state = SearchState::new();
        state.set_query("hello", &rows, 11, 0);
        assert_eq!(state.match_count(), 1);

        state.set_query("foo", &rows, 11, 0);
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[0].row, 1);
    }

    #[test]
    fn test_navigate_empty_matches() {
        let rows = vec![make_row("hello")];
        let mut state = SearchState::new();
        state.set_query("xyz", &rows, 5, 0);
        assert_eq!(state.match_count(), 0);

        state.next();
        state.prev();
        assert!(state.current_match().is_none());
    }

    #[test]
    fn test_is_match_boundaries() {
        let rows = vec![make_row("hello")];
        let mut state = SearchState::new();
        state.set_query("ell", &rows, 5, 0);
        assert!(state.is_match(0, 1));
        assert!(state.is_match(0, 2));
        assert!(state.is_match(0, 3));
        assert!(!state.is_match(0, 0));
        assert!(!state.is_match(0, 4));
    }

    #[test]
    fn test_mock_search_is_match() {
        let mock = MockSearchState {
            active: true,
            matches: vec![(0, 2, 4), (1, 0, 3)],
            current_idx: 0,
        };
        assert!(mock.is_active());
        assert!(mock.is_match(0, 2));
        assert!(mock.is_match(0, 3));
        assert!(mock.is_match(0, 4));
        assert!(!mock.is_match(0, 1));
        assert!(!mock.is_match(0, 5));
        assert!(mock.is_match(1, 0));
        assert!(mock.is_match(1, 3));
        assert!(!mock.is_match(1, 4));
        assert!(!mock.is_match(2, 0));
    }

    #[test]
    fn test_mock_search_is_current_match() {
        let mock = MockSearchState {
            active: true,
            matches: vec![(0, 2, 4), (1, 0, 3)],
            current_idx: 0,
        };
        assert!(mock.is_current_match(0, 2));
        assert!(mock.is_current_match(0, 4));
        assert!(!mock.is_current_match(1, 0));

        let mock2 = MockSearchState {
            active: true,
            matches: vec![(0, 2, 4), (1, 0, 3)],
            current_idx: 1,
        };
        assert!(!mock2.is_current_match(0, 2));
        assert!(mock2.is_current_match(1, 0));
        assert!(mock2.is_current_match(1, 3));
    }

    #[test]
    fn test_mock_search_empty() {
        let mock = MockSearchState {
            active: false,
            matches: vec![],
            current_idx: 0,
        };
        assert!(!mock.is_active());
        assert_eq!(mock.match_count(), 0);
        assert!(!mock.is_match(0, 0));
        assert!(!mock.is_current_match(0, 0));
    }

    #[test]
    fn test_search_query_trait_on_real_state() {
        let rows = vec![make_row("hello world")];
        let mut state = SearchState::new();
        state.set_query("world", &rows, 11, 0);

        let query: &dyn SearchQuery = &state;
        assert!(!query.is_active());
        assert_eq!(query.match_count(), 1);
        assert!(query.is_match(0, 6));
        assert!(!query.is_match(0, 5));
    }

    #[test]
    fn test_search_query_is_active_on_state() {
        let mut state = SearchState::new();
        let query: &dyn SearchQuery = &state;
        assert!(!query.is_active());

        state.open();
        let query: &dyn SearchQuery = &state;
        assert!(query.is_active());
    }

    #[test]
    fn test_search_query_match_count() {
        let rows = vec![make_row("foo bar foo"), make_row("baz foo qux")];
        let mut state = SearchState::new();
        state.set_query("foo", &rows, 11, 0);

        let query: &dyn SearchQuery = &state;
        assert_eq!(query.match_count(), 3);
    }

    #[test]
    fn test_search_case_sensitive_mode() {
        let rows = vec![make_row("Foo FOO foo")];
        let mut state = SearchState::new();
        state.ignore_case = false;
        state.set_query("Foo", &rows, 11, 0);
        assert_eq!(state.match_count(), 1);
    }

    #[test]
    fn test_search_case_insensitive_default() {
        let rows = vec![make_row("Foo FOO foo")];
        let mut state = SearchState::new();
        state.set_query("foo", &rows, 11, 0);
        assert_eq!(state.match_count(), 3);
    }

    #[test]
    fn test_search_adjacent_matches() {
        let rows = vec![make_row("aaaaaa")];
        let mut state = SearchState::new();
        state.set_query("aa", &rows, 6, 0);
        assert!(state.match_count() >= 1);
    }

    #[test]
    fn test_search_empty_query_no_matches() {
        let rows = vec![make_row("hello world")];
        let mut state = SearchState::new();
        state.set_query("", &rows, 11, 0);
        assert_eq!(state.match_count(), 0);
    }

    #[test]
    fn test_search_next_wraps() {
        let rows = vec![make_row("aXa"), make_row("aXa")];
        let mut state = SearchState::new();
        state.set_query("X", &rows, 3, 0);
        let count = state.match_count();
        assert_eq!(count, 2);
        for _ in 0..count + 1 {
            state.next();
        }
        assert!(state.current_match().is_some());
    }

    #[test]
    fn test_search_prev_wraps() {
        let rows = vec![make_row("aXa"), make_row("aXa")];
        let mut state = SearchState::new();
        state.set_query("X", &rows, 3, 0);
        state.prev();
        assert!(state.current_match().is_some());
    }

    // -----------------------------------------------------------------
    // Regex matcher
    // -----------------------------------------------------------------

    /// Literal is the default, so no existing caller changes meaning.
    #[test]
    fn literal_is_the_default_matcher() {
        assert_eq!(SearchState::new().matcher, SearchMatcher::Literal);
        assert_eq!(SearchMatcher::default(), SearchMatcher::Literal);
    }

    /// The headline case from the brief: `error.*timeout` is a pattern,
    /// not a substring.
    #[test]
    fn regex_matches_a_real_pattern() {
        let rows = vec![
            make_row("error: connection timeout"),
            make_row("error: bad request"),
            make_row("warn: timeout ignored"),
        ];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.set_query("error.*timeout", &rows, 25, 0);

        assert_eq!(state.pattern_error(), None);
        assert_eq!(state.match_count(), 1, "only row 0 has error…timeout");
        assert_eq!(state.matches[0].row, 0);
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(
            state.matches[0].col_end, 24,
            "greedy `.*` runs to the end of `timeout`"
        );

        // The same query as a LITERAL finds nothing — proving the regex
        // hit came from pattern semantics and not from substring luck.
        let mut lit = SearchState::new();
        lit.set_query("error.*timeout", &rows, 25, 0);
        assert_eq!(lit.match_count(), 0);
    }

    /// `a(b` is a perfectly good literal and a broken regex. Literal
    /// mode must keep finding it verbatim.
    #[test]
    fn literal_mode_matches_text_that_would_be_a_broken_regex() {
        let rows = vec![make_row("fn a(b, c)")];
        let mut state = SearchState::new();
        state.set_query("a(b", &rows, 10, 0);
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 3);
        assert_eq!(state.matches[0].col_end, 5);
        assert_eq!(
            state.pattern_error(),
            None,
            "literal mode never compiles a pattern, so it never errors"
        );
    }

    /// THE honesty test. An invalid regex must not panic, must report a
    /// typed reason, and must NOT quietly re-read itself as a literal —
    /// the row below CONTAINS the text `a(b`, so a literal fallback
    /// would hand back one confident, wrong match.
    #[test]
    fn invalid_regex_refuses_instead_of_falling_back_to_literal() {
        let rows = vec![make_row("fn a(b, c)")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.set_query("a(b", &rows, 10, 0);

        assert_eq!(
            state.pattern_error(),
            Some(PatternError::UnclosedGroup),
            "the failure must be reported, not swallowed"
        );
        assert_eq!(
            state.match_count(),
            0,
            "a literal fallback would have found `a(b` at column 3 — \
             silently answering a question the operator did not ask"
        );
        assert!(state.current_match().is_none());
        // The query survives so the operator can keep editing it.
        assert_eq!(state.query, "a(b");
    }

    /// Every rejection path: no panic, zero matches, a typed cause.
    #[test]
    fn negated_shorthand_inside_a_class_is_accepted_not_refused() {
        // `[\D]` is valid regex. The hand-rolled engine could not represent it
        // and refused; a test pinned that refusal, and the engine swap almost
        // carried the limitation forward as a feature. It matches non-digits.
        let rows = vec![make_row("ab12cd")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.set_query("[\\D]+", &rows, 6, 0);
        assert_eq!(
            state.pattern_error(),
            None,
            "[\\D] must compile, not refuse"
        );
        assert_eq!(
            state.match_count(),
            2,
            "[\\D]+ matches the two non-digit runs, `ab` and `cd`"
        );
    }

    #[test]
    fn invalid_regex_causes_are_typed() {
        let rows = vec![make_row("abcdefg [x] (y) 123")];
        let cases = [
            ("a(b", PatternError::UnclosedGroup),
            ("ab)", PatternError::UnopenedGroup),
            ("[abc", PatternError::UnclosedClass),
            ("*abc", PatternError::NothingToRepeat),
            ("|+", PatternError::NothingToRepeat),
            ("ab\\", PatternError::TrailingEscape),
        ];
        for (pattern, want) in cases {
            let mut state = SearchState::new();
            state.matcher = SearchMatcher::Regex;
            state.set_query(pattern, &rows, 19, 0);
            assert_eq!(state.pattern_error(), Some(want), "pattern {pattern:?}");
            assert_eq!(state.match_count(), 0, "pattern {pattern:?}");
            assert!(!state.pattern_error().unwrap().message().is_empty());
        }
        // Deep nesting is refused rather than overflowing the parser.
        let deep = "(".repeat(500);
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.set_query(&deep, &rows, 19, 0);
        assert_eq!(
            state.pattern_error(),
            Some(PatternError::TooDeeplyNested),
            "a 500-deep pattern must be rejected, not recursed"
        );
    }

    /// A later valid query clears a stale error — otherwise the status
    /// line keeps complaining about a pattern that is no longer typed.
    #[test]
    fn a_valid_query_clears_a_stale_pattern_error() {
        let rows = vec![make_row("hello world")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.set_query("wor(ld", &rows, 11, 0);
        assert!(state.pattern_error().is_some());

        state.set_query("wor.d", &rows, 11, 0);
        assert_eq!(state.pattern_error(), None);
        assert_eq!(state.match_count(), 1);

        state.set_query("wor(ld", &rows, 11, 0);
        assert!(state.pattern_error().is_some());
        state.set_query("", &rows, 11, 0);
        assert_eq!(
            state.pattern_error(),
            None,
            "an empty query is not an error"
        );
        state.set_query("wor(ld", &rows, 11, 0);
        state.close();
        assert_eq!(state.pattern_error(), None, "close() clears the error");
    }

    /// `ignore_case` means ONE thing. The same toggle, over the same
    /// grid, must select the same rows in both matchers.
    #[test]
    fn ignore_case_means_the_same_thing_in_both_matchers() {
        let rows = vec![
            make_row("Hello HELLO hello"),
            make_row("nothing here"),
            make_row("HeLLo"),
        ];

        for ignore_case in [true, false] {
            let mut literal = SearchState::new();
            literal.ignore_case = ignore_case;
            literal.set_query("hello", &rows, 17, 0);

            let mut regex = SearchState::new();
            regex.ignore_case = ignore_case;
            regex.matcher = SearchMatcher::Regex;
            regex.set_query("hello", &rows, 17, 0);

            assert_eq!(
                literal.matches, regex.matches,
                "ignore_case={ignore_case}: the two matchers disagreed"
            );
        }

        // And the toggle actually does something in EACH mode.
        let mut on = SearchState::new();
        on.matcher = SearchMatcher::Regex;
        on.ignore_case = true;
        on.set_query("hello", &rows, 17, 0);
        let mut off = SearchState::new();
        off.matcher = SearchMatcher::Regex;
        off.ignore_case = false;
        off.set_query("hello", &rows, 17, 0);
        assert_eq!(on.match_count(), 4);
        assert_eq!(off.match_count(), 1);
    }

    /// Case folding must reach inside `[...]` too, including negation:
    /// with the toggle on, `[^a]` refuses `A`.
    #[test]
    fn ignore_case_reaches_character_classes() {
        let rows = vec![make_row("A b C")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.ignore_case = true;
        state.set_query("[ac]", &rows, 5, 0);
        assert_eq!(state.match_count(), 2, "`[ac]` must fold onto `A` and `C`");

        state.ignore_case = false;
        state.set_query("[ac]", &rows, 5, 0);
        assert_eq!(
            state.match_count(),
            0,
            "case-sensitive `[ac]` matches neither"
        );

        state.ignore_case = true;
        state.set_query("[^ac ]", &rows, 5, 0);
        assert_eq!(
            state.match_count(),
            1,
            "negation applies AFTER folding, so only `b` survives"
        );
    }

    /// Classes, escapes, shorthands and anchors — the vocabulary a user
    /// reaching for regex expects to be there.
    #[test]
    fn regex_vocabulary_classes_escapes_and_anchors() {
        let rows = vec![make_row("id=42 v1.2 end")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;

        state.set_query("[0-9]+", &rows, 14, 0);
        assert_eq!(state.match_count(), 3, "42, 1, 2");

        state.set_query("\\d+", &rows, 14, 0);
        assert_eq!(state.match_count(), 3);

        state.set_query("v\\d\\.\\d", &rows, 14, 0);
        assert_eq!(state.match_count(), 1, "`\\.` is a literal dot");
        assert_eq!(state.matches[0].col_start, 6);
        assert_eq!(state.matches[0].col_end, 9);

        state.set_query("^id", &rows, 14, 0);
        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].col_start, 0);
        state.set_query("^42", &rows, 14, 0);
        assert_eq!(state.match_count(), 0, "`^` is row start, not anywhere");

        state.set_query("end$", &rows, 14, 0);
        assert_eq!(state.match_count(), 1);
        state.set_query("v1$", &rows, 14, 0);
        assert_eq!(state.match_count(), 0);

        state.set_query("id|end", &rows, 14, 0);
        assert_eq!(state.match_count(), 2);

        state.set_query("(v1)?\\.2", &rows, 14, 0);
        assert_eq!(state.match_count(), 1);
        assert_eq!(
            state.matches[0].col_start, 6,
            "leftmost-LONGEST takes the optional group"
        );
    }

    /// Regex matches are non-overlapping (as in every regex tool) while
    /// literal matches overlap (as they always have here). Pinned so the
    /// difference is a documented decision, not a discovery.
    #[test]
    fn regex_is_non_overlapping_literal_is_overlapping() {
        let rows = vec![make_row("aaaa")];

        let mut literal = SearchState::new();
        literal.set_query("aa", &rows, 4, 0);
        assert_eq!(literal.match_count(), 3, "0,1,2 — overlapping");

        let mut regex = SearchState::new();
        regex.matcher = SearchMatcher::Regex;
        regex.set_query("aa", &rows, 4, 0);
        assert_eq!(regex.match_count(), 2, "0,2 — non-overlapping");
        assert_eq!(regex.matches[0].col_start, 0);
        assert_eq!(regex.matches[1].col_start, 2);
    }

    /// A pattern that can match nothing must not report a zero-width
    /// "match" (nothing to highlight, and `end - 1` would underflow)
    /// and must not spin forever trying.
    #[test]
    fn empty_matches_are_dropped_not_reported() {
        let rows = vec![make_row("bbb")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;

        state.set_query("a*", &rows, 3, 0);
        assert_eq!(state.pattern_error(), None);
        assert_eq!(state.match_count(), 0);

        state.set_query("b*", &rows, 3, 0);
        assert_eq!(state.match_count(), 1, "the one non-empty match");
        assert_eq!(state.matches[0].col_start, 0);
        assert_eq!(state.matches[0].col_end, 2);
    }

    /// The reason this is an NFA simulation and not a backtracker: the
    /// classic catastrophic patterns cost linear time here. If this ever
    /// regresses to backtracking the test does not fail — it hangs, and
    /// that is the signal.
    #[test]
    fn pathological_patterns_terminate() {
        let rows = vec![make_row(&"a".repeat(64))];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;

        // Nested quantifiers over an epsilon-loopable body.
        state.set_query("(a*)*b", &rows, 64, 0);
        assert_eq!(state.pattern_error(), None);
        assert_eq!(state.match_count(), 0);

        state.set_query("(a|a)*b", &rows, 64, 0);
        assert_eq!(state.match_count(), 0);

        state.set_query("(a*)*", &rows, 64, 0);
        assert_eq!(state.match_count(), 1);
    }

    /// Toggling the matcher re-runs, so matches from the previous
    /// interpretation can never be left on screen.
    #[test]
    fn set_matcher_reruns_the_search() {
        let rows = vec![make_row("a.c abc")];
        let mut state = SearchState::new();
        state.set_query("a.c", &rows, 7, 0);
        assert_eq!(state.match_count(), 1, "literal: the dot is a dot");
        assert_eq!(state.matches[0].col_start, 0);

        state.set_matcher(SearchMatcher::Regex, &rows, 7, 0);
        assert_eq!(state.match_count(), 2, "regex: the dot is any char");

        state.set_matcher(SearchMatcher::Literal, &rows, 7, 0);
        assert_eq!(state.match_count(), 1);
    }

    /// Columns are CHARACTER indices. Before the shared-fold rewrite the
    /// literal path indexed a `String` by BYTE, so a row of multi-byte
    /// box-drawing glyphs reported a match three columns per glyph too
    /// far right — the same aliasing `url.rs` documents.
    #[test]
    fn columns_are_char_indices_not_byte_offsets() {
        // 3 box glyphs, 3 bytes each: byte offset 9, char index 3.
        let rows = vec![make_row("│─→ok")];
        for matcher in [SearchMatcher::Literal, SearchMatcher::Regex] {
            let mut state = SearchState::new();
            state.matcher = matcher;
            state.set_query("ok", &rows, 5, 0);
            assert_eq!(state.match_count(), 1, "{matcher:?}");
            assert_eq!(
                state.matches[0].col_start, 3,
                "{matcher:?}: char index 3, not byte offset 9"
            );
            assert_eq!(state.matches[0].col_end, 4, "{matcher:?}");
        }
    }

    /// The byte→char conversion is a stateful forward walk REUSED across
    /// every match in a row, so one match cannot catch a cursor that goes
    /// stale or double-counts. Several matches behind several multi-byte
    /// glyphs can.
    #[test]
    fn char_indices_stay_correct_across_multiple_matches_on_a_multibyte_row() {
        // `→` is 3 bytes wide: the `a`s sit at BYTE 3, 7, 11 but at
        // COLUMN 1, 3, 5 — and the renderer multiplies columns by
        // cell_width, so handing it a byte offset draws in the wrong place.
        let rows = vec![make_row("→a→a→a")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;
        state.set_query("a", &rows, 6, 0);

        assert_eq!(state.match_count(), 3);
        let starts: Vec<usize> = state.matches.iter().map(|m| m.col_start).collect();
        assert_eq!(starts, vec![1, 3, 5], "byte offsets would read 3, 7, 11");
        assert!(
            state.matches.iter().all(|m| m.col_end == m.col_start),
            "each match is one char wide"
        );
    }

    /// `regex` has far more ways to be invalid than the hand-rolled
    /// grammar's six, so an unmapped cause is typed [`PatternError::Invalid`]
    /// rather than rounded into a neighbouring variant that would name the
    /// wrong thing. It must still REFUSE: typed cause, zero matches.
    #[test]
    fn an_unmapped_compile_failure_is_typed_invalid_and_still_refuses() {
        let rows = vec![make_row("aaa [z] 123")];
        for pattern in ["a{3,1}", "[z-a]"] {
            let mut state = SearchState::new();
            state.matcher = SearchMatcher::Regex;
            state.set_query(pattern, &rows, 11, 0);
            assert_eq!(
                state.pattern_error(),
                Some(PatternError::Invalid),
                "pattern {pattern:?}"
            );
            assert_eq!(state.match_count(), 0, "pattern {pattern:?}");
        }
        assert_eq!(PatternError::Invalid.message(), "invalid pattern");
    }

    /// What the swap actually bought. The hand-rolled parser had no
    /// `{n,m}` and no `\b`: `{` was an ordinary character and `\b` fell
    /// through to a literal `b`, so `a{2,3}` searched for the seven-
    /// character text `a{2,3}` and `\bcat\b` searched for `bcatb`. Both
    /// found nothing, silently, while looking like working regexes.
    #[test]
    fn regex_vocabulary_the_hand_rolled_engine_could_not_express() {
        let rows = vec![make_row("a aa aaa cat concat")];
        let mut state = SearchState::new();
        state.matcher = SearchMatcher::Regex;

        state.set_query("a{2,3}", &rows, 19, 0);
        assert_eq!(state.pattern_error(), None);
        assert_eq!(state.match_count(), 2, "`aa` and `aaa`, not literal text");
        assert_eq!(state.matches[0].col_start, 2);
        assert_eq!(state.matches[0].col_end, 3);
        assert_eq!(state.matches[1].col_start, 5);
        assert_eq!(state.matches[1].col_end, 7);

        state.set_query("\\bcat\\b", &rows, 19, 0);
        assert_eq!(state.pattern_error(), None);
        assert_eq!(
            state.match_count(),
            1,
            "the `cat` inside `concat` has no word boundary"
        );
        assert_eq!(state.matches[0].col_start, 9);
    }

    #[test]
    fn pattern_error_renders_without_allocating_a_format() {
        let mut shown = String::new();
        std::fmt::Write::write_fmt(&mut shown, format_args!("{}", PatternError::UnclosedGroup))
            .unwrap();
        assert_eq!(shown, "unclosed `(`");
        assert_eq!(PatternError::UnclosedClass.message(), "unclosed `[`");
    }
}
