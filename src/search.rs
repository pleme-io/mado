//! Scrollback search — find text in terminal history.
//!
//! Two matchers, ONE case rule. [`SearchMatcher::Literal`] (the
//! default) and [`SearchMatcher::Regex`] both decide "is this
//! character the same as that one?" through the single [`char_eq`]
//! predicate, so `ignore_case` cannot come to mean two things — the
//! toggle is defined once and both engines read that definition.
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
    /// The query is a regular expression. Matches are leftmost-longest
    /// and NON-overlapping, as in every other regex search tool.
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
    /// A negated shorthand (`\D`, `\W`, `\S`) inside `[...]`. Rejected
    /// rather than silently read as a literal letter — the whole point
    /// of this type.
    NegatedShorthandInClass,
    /// Groups nested past [`MAX_GROUP_DEPTH`], which would otherwise
    /// recurse the parser deep enough to overflow the stack.
    TooDeeplyNested,
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
            Self::NegatedShorthandInClass => "`\\D`/`\\W`/`\\S` not allowed inside `[...]`",
            Self::TooDeeplyNested => "groups nested too deeply",
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
    /// same thing in both — see [`char_eq`], the one definition.
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
            SearchMatcher::Regex => match Pattern::compile(query) {
                Ok(pattern) => scan_rows(rows, cols, first_abs, |line| {
                    pattern.spans(line, ignore_case)
                }),
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

/// THE definition of `ignore_case`, for BOTH matchers. The literal
/// scanner and every character-consuming regex instruction call this and
/// nothing else, so the toggle is one rule with one meaning rather than
/// two implementations that agree until they don't.
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
fn scan_rows<F>(rows: &[Vec<Cell>], cols: usize, first_abs: usize, mut spans: F) -> Vec<SearchMatch>
where
    F: FnMut(&[char]) -> Vec<(usize, usize)>,
{
    let last_col = cols.saturating_sub(1);
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let line: Vec<char> = row_to_string(row, cols).chars().collect();
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
fn literal_spans(line: &[char], needle: &[char], ignore_case: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
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
// Hand-rolled because mado does not declare the `regex` crate (it is in
// `Cargo.lock` transitively, which makes it BUILT, not USABLE — the
// source-vs-build distinction the repo's own dependency table warns
// about). Adding `regex = "1"` to `Cargo.toml` is the load-bearing fix
// and is a one-line change; this module is written so the swap is local
// to `Pattern`.
//
// It is a Thompson NFA simulation, not a backtracker, deliberately: the
// thread set holds each program counter at most once per input position,
// so `(a*)*` against a long row costs O(len × instructions) instead of
// hanging the render thread. A pattern that is merely nasty cannot be a
// denial of service, without a step budget that would silently truncate
// results.

/// Groups nested deeper than this are rejected — the parser descends
/// once per `(`, and a 10k-deep pattern would otherwise overflow.
const MAX_GROUP_DEPTH: u32 = 64;

/// A set of character ranges, optionally negated (`[^...]`).
#[derive(Debug, Clone)]
struct CharClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl CharClass {
    fn in_ranges(&self, ch: char) -> bool {
        self.ranges.iter().any(|&(lo, hi)| ch >= lo && ch <= hi)
    }

    /// Membership under the shared case rule: with `ignore_case`, a
    /// character is in the class if EITHER case form is. Negation is
    /// applied after the fold, so `[^a]` correctly refuses `'A'`.
    fn contains(&self, ch: char, ignore_case: bool) -> bool {
        let hit = self.in_ranges(ch)
            || (ignore_case
                && (ch.to_lowercase().any(|c| self.in_ranges(c))
                    || ch.to_uppercase().any(|c| self.in_ranges(c))));
        hit != self.negated
    }
}

/// Parsed pattern, before lowering to the NFA program.
#[derive(Debug)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(CharClass),
    Start,
    End,
    Concat(Vec<Node>),
    Alt(Box<Node>, Box<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Opt(Box<Node>),
}

/// One NFA instruction. `Split`/`Jmp`/`Assert*` are epsilon
/// transitions, expanded by `add_thread`; the rest consume a character.
#[derive(Debug, Clone, Copy)]
enum Inst {
    Char(char),
    Any,
    Class(usize),
    Split(usize, usize),
    Jmp(usize),
    AssertStart,
    AssertEnd,
    Match,
}

/// A compiled pattern.
struct Pattern {
    insts: Vec<Inst>,
    classes: Vec<CharClass>,
}

impl Pattern {
    fn compile(pattern: &str) -> Result<Self, PatternError> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut parser = Parser {
            pat: &chars,
            pos: 0,
            depth: 0,
        };
        let ast = parser.parse_alt()?;
        if parser.pos != chars.len() {
            // `parse_concat` stops at a `)` only when it did not open one.
            return Err(PatternError::UnopenedGroup);
        }
        let mut out = Self {
            insts: Vec::new(),
            classes: Vec::new(),
        };
        out.emit(&ast);
        out.insts.push(Inst::Match);
        Ok(out)
    }

    fn emit(&mut self, node: &Node) {
        match node {
            Node::Empty => {}
            Node::Char(c) => self.insts.push(Inst::Char(*c)),
            Node::Any => self.insts.push(Inst::Any),
            Node::Class(class) => {
                self.classes.push(class.clone());
                self.insts.push(Inst::Class(self.classes.len() - 1));
            }
            Node::Start => self.insts.push(Inst::AssertStart),
            Node::End => self.insts.push(Inst::AssertEnd),
            Node::Concat(items) => {
                for item in items {
                    self.emit(item);
                }
            }
            Node::Alt(lhs, rhs) => {
                let split = self.placeholder();
                self.emit(lhs);
                let jmp = self.placeholder();
                let rhs_at = self.insts.len();
                self.emit(rhs);
                let after = self.insts.len();
                self.insts[split] = Inst::Split(split + 1, rhs_at);
                self.insts[jmp] = Inst::Jmp(after);
            }
            Node::Star(inner) => {
                let split = self.placeholder();
                self.emit(inner);
                self.insts.push(Inst::Jmp(split));
                let after = self.insts.len();
                self.insts[split] = Inst::Split(split + 1, after);
            }
            Node::Plus(inner) => {
                let body = self.insts.len();
                self.emit(inner);
                let split = self.placeholder();
                let after = self.insts.len();
                self.insts[split] = Inst::Split(body, after);
            }
            Node::Opt(inner) => {
                let split = self.placeholder();
                self.emit(inner);
                let after = self.insts.len();
                self.insts[split] = Inst::Split(split + 1, after);
            }
        }
    }

    /// Reserve a slot to be back-patched once the branch target is known.
    fn placeholder(&mut self) -> usize {
        let at = self.insts.len();
        self.insts.push(Inst::Jmp(at));
        at
    }

    /// Add `pc` (and everything reachable from it by epsilon) to `list`,
    /// remembering the earliest `start` each pc was reached with. That
    /// bookkeeping is also the loop guard: a pc already reached with an
    /// equal-or-earlier start is not re-expanded, so `(a*)*` terminates.
    fn add_thread(
        &self,
        pc: usize,
        start: usize,
        pos: usize,
        len: usize,
        list: &mut Vec<(usize, usize)>,
        seen: &mut [usize],
    ) {
        let mut stack = vec![pc];
        while let Some(pc) = stack.pop() {
            if seen[pc] <= start {
                continue;
            }
            seen[pc] = start;
            match self.insts[pc] {
                Inst::Jmp(target) => stack.push(target),
                Inst::Split(a, b) => {
                    stack.push(b);
                    stack.push(a);
                }
                Inst::AssertStart => {
                    if pos == 0 {
                        stack.push(pc + 1);
                    }
                }
                Inst::AssertEnd => {
                    if pos == len {
                        stack.push(pc + 1);
                    }
                }
                _ => list.push((pc, start)),
            }
        }
    }

    /// The leftmost-longest match at or after `from`, as
    /// `(start, end_exclusive)`.
    fn next_match(&self, input: &[char], from: usize, ignore_case: bool) -> Option<(usize, usize)> {
        let n = self.insts.len();
        let mut cur: Vec<(usize, usize)> = Vec::new();
        let mut next: Vec<(usize, usize)> = Vec::new();
        let mut seen_cur = vec![usize::MAX; n];
        let mut seen_next = vec![usize::MAX; n];
        let mut best: Option<(usize, usize)> = None;
        let mut pos = from;

        loop {
            // Seed a fresh thread at this position — but only while no
            // match is in hand, since a later start can never be more
            // leftmost than one we already have.
            if best.is_none() {
                self.add_thread(0, pos, pos, input.len(), &mut cur, &mut seen_cur);
            }
            for &(pc, start) in &cur {
                if matches!(self.insts[pc], Inst::Match) {
                    best = match best {
                        None => Some((start, pos)),
                        Some((bs, be)) if start < bs || (start == bs && pos > be) => {
                            Some((start, pos))
                        }
                        keep => keep,
                    };
                }
            }
            if pos >= input.len() || cur.is_empty() {
                break;
            }

            let ch = input[pos];
            next.clear();
            seen_next.iter_mut().for_each(|s| *s = usize::MAX);
            for &(pc, start) in &cur {
                // Threads that began after the best match's start can no
                // longer win; dropping them is what ends the sweep.
                if best.is_some_and(|(bs, _)| start > bs) {
                    continue;
                }
                let consumed = match self.insts[pc] {
                    Inst::Char(c) => char_eq(ch, c, ignore_case),
                    Inst::Any => true,
                    Inst::Class(idx) => self.classes[idx].contains(ch, ignore_case),
                    _ => false,
                };
                if consumed {
                    self.add_thread(
                        pc + 1,
                        start,
                        pos + 1,
                        input.len(),
                        &mut next,
                        &mut seen_next,
                    );
                }
            }
            std::mem::swap(&mut cur, &mut next);
            std::mem::swap(&mut seen_cur, &mut seen_next);
            pos += 1;
        }
        best
    }

    /// Every non-overlapping leftmost-longest match in `line`. An empty
    /// match (`a*` against `bbb`) advances one character rather than
    /// looping forever, and `scan_rows` discards it.
    fn spans(&self, line: &[char], ignore_case: bool) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut from = 0;
        while from <= line.len() {
            let Some((start, end)) = self.next_match(line, from, ignore_case) else {
                break;
            };
            if end > start {
                out.push((start, end));
                from = end;
            } else {
                from = start + 1;
            }
        }
        out
    }
}

/// Recursive-descent parser over the pattern's characters.
///
/// ```text
/// alt    := concat ('|' concat)*
/// concat := repeat*
/// repeat := atom ('*' | '+' | '?')*
/// atom   := '(' alt ')' | '[' class ']' | '.' | '^' | '$' | '\' esc | char
/// ```
struct Parser<'a> {
    pat: &'a [char],
    pos: usize,
    depth: u32,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.pat.get(self.pos).copied()
    }

    fn parse_alt(&mut self) -> Result<Node, PatternError> {
        let mut node = self.parse_concat()?;
        while self.peek() == Some('|') {
            self.pos += 1;
            let rhs = self.parse_concat()?;
            node = Node::Alt(Box::new(node), Box::new(rhs));
        }
        Ok(node)
    }

    fn parse_concat(&mut self) -> Result<Node, PatternError> {
        let mut items = Vec::new();
        while !matches!(self.peek(), None | Some('|') | Some(')')) {
            items.push(self.parse_repeat()?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn parse_repeat(&mut self) -> Result<Node, PatternError> {
        let mut node = self.parse_atom()?;
        loop {
            node = match self.peek() {
                Some('*') => Node::Star(Box::new(node)),
                Some('+') => Node::Plus(Box::new(node)),
                Some('?') => Node::Opt(Box::new(node)),
                _ => return Ok(node),
            };
            self.pos += 1;
        }
    }

    fn parse_atom(&mut self) -> Result<Node, PatternError> {
        let Some(ch) = self.peek() else {
            return Ok(Node::Empty);
        };
        self.pos += 1;
        match ch {
            '(' => {
                self.depth += 1;
                if self.depth > MAX_GROUP_DEPTH {
                    return Err(PatternError::TooDeeplyNested);
                }
                let inner = self.parse_alt()?;
                if self.peek() != Some(')') {
                    return Err(PatternError::UnclosedGroup);
                }
                self.pos += 1;
                self.depth -= 1;
                Ok(inner)
            }
            '[' => self.parse_class(),
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            // A quantifier reached as an ATOM had nothing in front of it.
            '*' | '+' | '?' => Err(PatternError::NothingToRepeat),
            '\\' => {
                let Some(esc) = self.peek() else {
                    return Err(PatternError::TrailingEscape);
                };
                self.pos += 1;
                Ok(escape_node(esc))
            }
            other => Ok(Node::Char(other)),
        }
    }

    /// `[abc]`, `[^a-z]`, `[\d_]`. A `]` in first position is a literal
    /// `]`, as everywhere else.
    fn parse_class(&mut self) -> Result<Node, PatternError> {
        let mut negated = false;
        if self.peek() == Some('^') {
            negated = true;
            self.pos += 1;
        }
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        loop {
            let Some(ch) = self.peek() else {
                return Err(PatternError::UnclosedClass);
            };
            if ch == ']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            self.pos += 1;

            let lo = if ch == '\\' {
                let Some(esc) = self.peek() else {
                    return Err(PatternError::TrailingEscape);
                };
                self.pos += 1;
                match shorthand_ranges(esc) {
                    Some(Shorthand::Positive(mut r)) => {
                        ranges.append(&mut r);
                        continue;
                    }
                    // `[\D]` would have to mean "everything except a
                    // digit, unioned with the rest of this class" — a
                    // set operation this representation cannot hold.
                    // Refuse rather than read it as a literal `D`.
                    Some(Shorthand::Negated) => {
                        return Err(PatternError::NegatedShorthandInClass);
                    }
                    None => literal_escape(esc),
                }
            } else {
                ch
            };

            // `a-z`, but a trailing `-` before `]` is a literal `-`.
            if self.peek() == Some('-') && self.pat.get(self.pos + 1).is_some_and(|&c| c != ']') {
                self.pos += 1;
                let Some(hi) = self.peek() else {
                    return Err(PatternError::UnclosedClass);
                };
                self.pos += 1;
                let hi = if hi == '\\' {
                    let Some(esc) = self.peek() else {
                        return Err(PatternError::TrailingEscape);
                    };
                    self.pos += 1;
                    literal_escape(esc)
                } else {
                    hi
                };
                ranges.push(if lo <= hi { (lo, hi) } else { (hi, lo) });
            } else {
                ranges.push((lo, lo));
            }
        }
        Ok(Node::Class(CharClass { negated, ranges }))
    }
}

/// What a `\x` shorthand denotes.
enum Shorthand {
    Positive(Vec<(char, char)>),
    Negated,
}

fn shorthand_ranges(esc: char) -> Option<Shorthand> {
    let ranges = match esc {
        'd' => vec![('0', '9')],
        'w' => vec![('0', '9'), ('a', 'z'), ('A', 'Z'), ('_', '_')],
        's' => vec![
            (' ', ' '),
            ('\t', '\t'),
            ('\n', '\n'),
            ('\r', '\r'),
            ('\u{b}', '\u{c}'),
        ],
        'D' | 'W' | 'S' => return Some(Shorthand::Negated),
        _ => return None,
    };
    Some(Shorthand::Positive(ranges))
}

/// The character a non-shorthand escape stands for. `\.` is `.`, `\n`
/// is a newline, and an escape of anything else is that thing —
/// escaping a plain character is never an error.
fn literal_escape(esc: char) -> char {
    match esc {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        other => other,
    }
}

fn escape_node(esc: char) -> Node {
    match shorthand_ranges(esc) {
        Some(Shorthand::Positive(ranges)) => Node::Class(CharClass {
            negated: false,
            ranges,
        }),
        Some(Shorthand::Negated) => {
            // `\D` is `\d` negated; same for `\W` / `\S`.
            let positive = esc.to_ascii_lowercase();
            let ranges = match shorthand_ranges(positive) {
                Some(Shorthand::Positive(r)) => r,
                _ => Vec::new(),
            };
            Node::Class(CharClass {
                negated: true,
                ranges,
            })
        }
        None => Node::Char(literal_escape(esc)),
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
    fn invalid_regex_causes_are_typed() {
        let rows = vec![make_row("abcdefg [x] (y) 123")];
        let cases = [
            ("a(b", PatternError::UnclosedGroup),
            ("ab)", PatternError::UnopenedGroup),
            ("[abc", PatternError::UnclosedClass),
            ("*abc", PatternError::NothingToRepeat),
            ("|+", PatternError::NothingToRepeat),
            ("ab\\", PatternError::TrailingEscape),
            ("[\\D]", PatternError::NegatedShorthandInClass),
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

    #[test]
    fn pattern_error_renders_without_allocating_a_format() {
        let mut shown = String::new();
        std::fmt::Write::write_fmt(&mut shown, format_args!("{}", PatternError::UnclosedGroup))
            .unwrap();
        assert_eq!(shown, "unclosed `(`");
        assert_eq!(PatternError::UnclosedClass.message(), "unclosed `[`");
    }
}
