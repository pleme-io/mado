use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::config::LinkEditor;
use crate::grid_col::glyph_columns;
use crate::terminal::{Cell, NO_LINK_ID};

static URL_FINDER: LazyLock<linkify::LinkFinder> = LazyLock::new(|| {
    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    finder
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    File(FileLocation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLocation {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

impl FileLocation {
    #[must_use]
    pub fn at(path: impl Into<PathBuf>, line: Option<u32>, col: Option<u32>) -> Self {
        Self {
            path: path.into(),
            line,
            col,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellPoint {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Segment {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverLink {
    pub segments: Vec<Segment>,
    pub seqno: u64,
    pub scroll_offset: usize,
}

impl HoverLink {
    #[must_use]
    pub fn current(&self, seqno: u64, scroll_offset: usize) -> bool {
        self.seqno == seqno && self.scroll_offset == scroll_offset
    }
}

#[must_use]
pub fn decode_cwd(osc7_path: &str) -> String {
    percent_decode(osc7_path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub start: CellPoint,
    pub end: CellPoint,
    pub target: LinkTarget,
}

impl Link {
    #[must_use]
    pub fn segments(&self, cols: usize) -> Vec<Segment> {
        let last_col = cols.saturating_sub(1);
        (self.start.row..=self.end.row)
            .map(|row| Segment {
                row,
                col_start: if row == self.start.row {
                    self.start.col
                } else {
                    0
                },
                col_end: if row == self.end.row {
                    self.end.col
                } else {
                    last_col
                },
            })
            .collect()
    }

    #[cfg(test)]
    #[must_use]
    pub fn contains(&self, p: CellPoint) -> bool {
        p >= self.start && p <= self.end
    }
}

pub struct PathContext<'a> {
    pub cwd: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub exists: &'a dyn Fn(&Path) -> bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub range: Range<usize>,
    pub target: LinkTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CandidateKind {
    Url(String),
    Path {
        raw: String,
        line: Option<u32>,
        col: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    range: Range<usize>,
    kind: CandidateKind,
}

#[cfg(test)]
#[must_use]
pub fn detect_in_text(text: &str, ctx: &PathContext<'_>, detect_paths: bool) -> Vec<Detected> {
    let mut out: Vec<Detected> = Vec::new();
    for c in candidates(text, detect_paths) {
        if out.iter().any(|d| overlaps(&d.range, &c.range)) {
            continue;
        }
        if let Some(target) = resolve(&c, ctx) {
            out.push(Detected {
                range: c.range,
                target,
            });
        }
    }
    out.sort_by_key(|d| d.range.start);
    out
}

#[must_use]
pub fn detect_at_byte(
    text: &str,
    byte: usize,
    ctx: &PathContext<'_>,
    detect_paths: bool,
) -> Option<Detected> {
    candidates(text, detect_paths)
        .into_iter()
        .filter(|c| c.range.contains(&byte))
        .find_map(|c| {
            resolve(&c, ctx).map(|target| Detected {
                range: c.range,
                target,
            })
        })
}

#[must_use]
pub fn classify_uri(uri: &str) -> LinkTarget {
    match file_uri_location(uri) {
        Some(loc) => LinkTarget::File(loc),
        None => LinkTarget::Url(uri.to_string()),
    }
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn candidates(text: &str, detect_paths: bool) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = URL_FINDER
        .links(text)
        .map(|l| Candidate {
            range: l.start()..l.end(),
            kind: CandidateKind::Url(l.as_str().to_string()),
        })
        .collect();
    if !detect_paths {
        return out;
    }
    let url_ranges: Vec<Range<usize>> = out.iter().map(|c| c.range.clone()).collect();
    let free = |r: &Range<usize>| !url_ranges.iter().any(|u| overlaps(u, r));
    out.extend(
        quoted_candidates(text)
            .into_iter()
            .filter(|c| free(&c.range)),
    );
    out.extend(
        token_candidates(text)
            .into_iter()
            .filter(|c| free(&c.range)),
    );
    out
}

fn resolve(c: &Candidate, ctx: &PathContext<'_>) -> Option<LinkTarget> {
    match &c.kind {
        CandidateKind::Url(u) => Some(classify_uri(u)),
        CandidateKind::Path { raw, line, col } => {
            resolve_path(raw, ctx).map(|path| LinkTarget::File(FileLocation::at(path, *line, *col)))
        }
    }
}

fn resolve_path(raw: &str, ctx: &PathContext<'_>) -> Option<PathBuf> {
    if !plausible_path(raw) {
        return None;
    }
    if raw.starts_with('/') {
        let p = PathBuf::from(raw);
        return (ctx.exists)(&p).then_some(p);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let p = ctx.home?.join(rest);
        return (ctx.exists)(&p).then_some(p);
    }
    let cwd = ctx.cwd?;
    let rel = raw.strip_prefix("./").unwrap_or(raw);
    let p = cwd.join(rel);
    if (ctx.exists)(&p) {
        return Some(p);
    }
    let diff_side = rel.strip_prefix("a/").or_else(|| rel.strip_prefix("b/"))?;
    let p = cwd.join(diff_side);
    (ctx.exists)(&p).then_some(p)
}

fn plausible_path(raw: &str) -> bool {
    !raw.is_empty()
        && !matches!(raw, "/" | "~" | "~/" | "." | ".." | "./" | "../")
        && !raw.contains("://")
        && raw.chars().any(|c| c.is_alphanumeric() || c == '_')
}

fn is_token_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | ';')
}

fn token_candidates(text: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (is_token_delim(c), start) {
            (true, Some(s)) => {
                if let Some(cand) = token_candidate(text, s, i) {
                    out.push(cand);
                }
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    out
}

fn token_candidate(text: &str, mut s: usize, e: usize) -> Option<Candidate> {
    let mut tok = &text[s..e];
    if let Some(eq) = tok.rfind('=') {
        let after = &tok[eq + 1..];
        if after.starts_with(['/', '~', '.']) {
            s += eq + 1;
            tok = after;
        }
    }
    let lead = tok.len() - tok.trim_start_matches(['(', '[', '{']).len();
    s += lead;
    tok = &tok[lead..];
    let (path_len, line, col, end_len) = parse_location(tok);
    if path_len == 0 {
        return None;
    }
    Some(Candidate {
        range: s..s + end_len,
        kind: CandidateKind::Path {
            raw: tok[..path_len].to_string(),
            line,
            col,
        },
    })
}

fn quoted_candidates(text: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find(['"', '\'']) {
        let open = i + off;
        let q = &text[open..=open];
        let Some(close_off) = text[open + 1..].find(q) else {
            break;
        };
        let close = open + 1 + close_off;
        let inner = &text[open + 1..close];
        if inner.contains(' ') && (inner.starts_with(['/', '~', '.']) || inner.contains('/')) {
            let (path_len, line, col, end_len) = parse_location(inner);
            if path_len > 0 {
                out.push(Candidate {
                    range: open + 1..open + 1 + end_len,
                    kind: CandidateKind::Path {
                        raw: inner[..path_len].to_string(),
                        line,
                        col,
                    },
                });
            }
        }
        i = close + 1;
    }
    out
}

fn digits_at(s: &str, at: usize) -> Option<(u32, usize)> {
    let len = s[at..].bytes().take_while(u8::is_ascii_digit).count();
    if len == 0 {
        return None;
    }
    s[at..at + len].parse().ok().map(|n| (n, at + len))
}

fn parse_location(tok: &str) -> (usize, Option<u32>, Option<u32>, usize) {
    for (i, b) in tok.bytes().enumerate() {
        if i == 0 {
            continue;
        }
        let found = match b {
            b':' => colon_location(tok, i),
            b'(' => paren_location(tok, i),
            _ => None,
        };
        if let Some((line, col, end)) = found {
            return (i, Some(line), col, end);
        }
    }
    let trimmed = trim_trailing(tok);
    (trimmed, None, None, trimmed)
}

fn colon_location(tok: &str, i: usize) -> Option<(u32, Option<u32>, usize)> {
    let (line, after_line) = digits_at(tok, i + 1)?;
    if tok[after_line..].starts_with(':')
        && let Some((col, after_col)) = digits_at(tok, after_line + 1)
    {
        return Some((line, Some(col), after_col));
    }
    Some((line, None, after_line))
}

fn paren_location(tok: &str, i: usize) -> Option<(u32, Option<u32>, usize)> {
    let (line, after_line) = digits_at(tok, i + 1)?;
    let (col, after) = if tok[after_line..].starts_with(',') {
        digits_at(tok, after_line + 1).map_or((None, after_line), |(c, a)| (Some(c), a))
    } else {
        (None, after_line)
    };
    tok[after..]
        .starts_with(')')
        .then_some((line, col, after + 1))
}

fn trim_trailing(tok: &str) -> usize {
    let mut end = tok.len();
    loop {
        let Some(last) = tok[..end].chars().next_back() else {
            return 0;
        };
        let strip = match last {
            '.' | ',' | ':' | '!' | '?' => true,
            ')' => unbalanced(&tok[..end], '(', ')'),
            ']' => unbalanced(&tok[..end], '[', ']'),
            '}' => unbalanced(&tok[..end], '{', '}'),
            _ => false,
        };
        if !strip {
            return end;
        }
        end -= last.len_utf8();
    }
}

fn unbalanced(s: &str, open: char, close: char) -> bool {
    s.chars().filter(|&c| c == close).count() > s.chars().filter(|&c| c == open).count()
}

fn file_uri_location(uri: &str) -> Option<FileLocation> {
    let rest = uri.strip_prefix("file://")?;
    let path_part = &rest[rest.find('/')?..];
    let decoded = percent_decode(path_part);
    let (path_len, line, col, _) = parse_location(&decoded);
    let path = if line.is_some() {
        &decoded[..path_len]
    } else {
        decoded.as_str()
    };
    Some(FileLocation::at(path, line, col))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|_| bytes[i] == b'%')
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        if let Some(v) = hex {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct LogicalLine {
    text: String,
    first: Vec<CellPoint>,
    last_col: Vec<usize>,
    link_id: Vec<u16>,
}

impl LogicalLine {
    #[must_use]
    pub fn around(rows: &[&[Cell]], wrapped: &[bool], cols: usize, row: usize) -> Option<Self> {
        if row >= rows.len() {
            return None;
        }
        let is_wrapped = |r: usize| wrapped.get(r).copied().unwrap_or(false);
        let mut top = row;
        while top > 0 && is_wrapped(top - 1) {
            top -= 1;
        }
        let mut bottom = row;
        while bottom + 1 < rows.len() && is_wrapped(bottom) {
            bottom += 1;
        }
        let mut line = Self {
            text: String::with_capacity(cols * (bottom - top + 1)),
            first: Vec::new(),
            last_col: Vec::new(),
            link_id: Vec::new(),
        };
        for (r, cells) in rows.iter().enumerate().take(bottom + 1).skip(top) {
            for (gc, cell) in glyph_columns(cells, cols) {
                let col = gc.idx();
                let last = col + usize::from(cell.width).saturating_sub(1);
                let before = line.text.len();
                cell.write_to(&mut line.text);
                for _ in before..line.text.len() {
                    line.first.push(CellPoint { row: r, col });
                    line.last_col.push(last);
                    line.link_id.push(cell.link_id);
                }
            }
        }
        Some(line)
    }

    #[cfg(test)]
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    fn byte_at(&self, p: CellPoint) -> Option<usize> {
        self.first
            .iter()
            .zip(&self.last_col)
            .position(|(f, &l)| f.row == p.row && p.col >= f.col && p.col <= l)
    }

    fn link_over(&self, range: &Range<usize>, target: LinkTarget) -> Option<Link> {
        let start = *self.first.get(range.start)?;
        let last_byte = range.end.checked_sub(1)?;
        let end = CellPoint {
            row: self.first.get(last_byte)?.row,
            col: *self.last_col.get(last_byte)?,
        };
        Some(Link { start, end, target })
    }

    #[must_use]
    pub fn hyperlink_at<'a>(
        &self,
        p: CellPoint,
        lookup: impl Fn(u16) -> Option<&'a str>,
    ) -> Option<Link> {
        let b = self.byte_at(p)?;
        let id = self.link_id[b];
        if id == NO_LINK_ID {
            return None;
        }
        let uri = lookup(id)?;
        let lo = self.link_id[..b]
            .iter()
            .rposition(|&x| x != id)
            .map_or(0, |i| i + 1);
        let hi = self.link_id[b..]
            .iter()
            .position(|&x| x != id)
            .map_or(self.link_id.len(), |i| b + i);
        self.link_over(&(lo..hi), classify_uri(uri))
    }

    #[must_use]
    pub fn detected_at(
        &self,
        p: CellPoint,
        ctx: &PathContext<'_>,
        detect_paths: bool,
    ) -> Option<Link> {
        let b = self.byte_at(p)?;
        let d = detect_at_byte(&self.text, b, ctx, detect_paths)?;
        self.link_over(&d.range, d.target)
    }

    #[cfg(test)]
    #[must_use]
    pub fn detect_all(&self, ctx: &PathContext<'_>, detect_paths: bool) -> Vec<Link> {
        detect_in_text(&self.text, ctx, detect_paths)
            .into_iter()
            .filter_map(|d| self.link_over(&d.range, d.target))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlan {
    pub program: String,
    pub args: Vec<String>,
}

impl OpenPlan {
    fn opener(arg: String) -> Self {
        Self {
            program: crate::url::os_opener().to_string(),
            args: vec![arg],
        }
    }

    pub fn spawn(&self) -> std::io::Result<()> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        crate::spawn::detached(&mut cmd)
    }
}

#[must_use]
pub fn open_plan(
    target: &LinkTarget,
    editor: &LinkEditor,
    is_dir: &dyn Fn(&Path) -> bool,
    env_editor: &str,
) -> OpenPlan {
    let loc = match target {
        LinkTarget::Url(u) => return OpenPlan::opener(u.clone()),
        LinkTarget::File(loc) => loc,
    };
    let path = loc.path.to_string_lossy();
    if is_dir(&loc.path) {
        return OpenPlan::opener(path.into_owned());
    }
    let line = loc.line.unwrap_or(1);
    let col = loc.col.unwrap_or(1);
    match editor {
        LinkEditor::UrlTemplate { template } => {
            OpenPlan::opener(expand(template, &url_path(&path), line, col))
        }
        LinkEditor::Command { argv } => match argv.split_first() {
            Some((program, rest)) => OpenPlan {
                program: program.clone(),
                args: rest.iter().map(|a| expand(a, &path, line, col)).collect(),
            },
            None => OpenPlan::opener(path.into_owned()),
        },
        LinkEditor::System {} => OpenPlan::opener(path.into_owned()),
        LinkEditor::Env {} => {
            let mut args = Vec::new();
            if let Some(n) = loc.line {
                let mut jump = String::from("+");
                jump.push_str(&n.to_string());
                args.push(jump);
            }
            args.push(path.into_owned());
            OpenPlan {
                program: env_editor.to_string(),
                args,
            }
        }
    }
}

fn expand(template: &str, path: &str, line: u32, col: u32) -> String {
    template
        .replace("{path}", path)
        .replace("{line}", &line.to_string())
        .replace("{col}", &col.to_string())
}

fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct Fs {
        files: HashSet<PathBuf>,
        cwd: Option<PathBuf>,
        home: PathBuf,
    }

    impl Fs {
        fn new(files: &[&str]) -> Self {
            Self {
                files: files.iter().map(PathBuf::from).collect(),
                cwd: Some(PathBuf::from("/work/repo")),
                home: PathBuf::from("/home/op"),
            }
        }

        fn without_cwd(mut self) -> Self {
            self.cwd = None;
            self
        }

        fn detect(&self, text: &str) -> Vec<(String, LinkTarget)> {
            let exists = |p: &Path| self.files.contains(p);
            let ctx = PathContext {
                cwd: self.cwd.as_deref(),
                home: Some(&self.home),
                exists: &exists,
            };
            detect_in_text(text, &ctx, true)
                .into_iter()
                .map(|d| (text[d.range].to_string(), d.target))
                .collect()
        }
    }

    fn url(u: &str) -> LinkTarget {
        LinkTarget::Url(u.to_string())
    }

    fn file(p: &str, line: Option<u32>, col: Option<u32>) -> LinkTarget {
        LinkTarget::File(FileLocation::at(p, line, col))
    }

    fn one(fs: &Fs, text: &str) -> (String, LinkTarget) {
        let found = fs.detect(text);
        assert_eq!(found.len(), 1, "{text:?} -> {found:?}");
        found.into_iter().next().unwrap()
    }

    #[test]
    fn urls_http_and_https() {
        let fs = Fs::new(&[]);
        assert_eq!(
            one(&fs, "see https://example.com/a?b=1#c now"),
            (
                "https://example.com/a?b=1#c".into(),
                url("https://example.com/a?b=1#c")
            )
        );
        assert_eq!(
            one(&fs, "http://localhost:8080/api").1,
            url("http://localhost:8080/api")
        );
    }

    #[test]
    fn urls_drop_trailing_punctuation() {
        let fs = Fs::new(&[]);
        for text in [
            "go to https://example.com.",
            "go to https://example.com, then",
            "is it https://example.com?",
            "go https://example.com!",
            "end https://example.com;",
            "end https://example.com:",
        ] {
            assert_eq!(one(&fs, text).0, "https://example.com", "{text}");
        }
    }

    #[test]
    fn urls_balance_parentheses() {
        let fs = Fs::new(&[]);
        assert_eq!(
            one(&fs, "(see https://example.com/x)").0,
            "https://example.com/x"
        );
        assert_eq!(
            one(
                &fs,
                "read https://en.wikipedia.org/wiki/Rust_(programming_language) ok"
            )
            .0,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        assert_eq!(
            one(&fs, "[link](https://example.com/doc)").0,
            "https://example.com/doc"
        );
    }

    #[test]
    fn urls_stop_at_quotes_and_brackets() {
        let fs = Fs::new(&[]);
        assert_eq!(
            one(&fs, "\"https://example.com/q\"").0,
            "https://example.com/q"
        );
        assert_eq!(
            one(&fs, "'https://example.com/q'").0,
            "https://example.com/q"
        );
        assert_eq!(
            one(&fs, "<https://example.com/q>").0,
            "https://example.com/q"
        );
        assert_eq!(
            one(&fs, "href=\"https://example.com/q\">x").0,
            "https://example.com/q"
        );
    }

    #[test]
    fn urls_bare_domains_are_not_links() {
        let fs = Fs::new(&[]);
        assert!(fs.detect("go to example.com for info").is_empty());
    }

    #[test]
    fn file_urls_become_file_targets() {
        let fs = Fs::new(&[]);
        assert_eq!(
            one(&fs, "open file:///tmp/a.rs now").1,
            file("/tmp/a.rs", None, None)
        );
        assert_eq!(
            one(&fs, "file:///tmp/a.rs:42").1,
            file("/tmp/a.rs", Some(42), None)
        );
        assert_eq!(
            one(&fs, "file://myhost/tmp/My%20File.txt").1,
            file("/tmp/My File.txt", None, None)
        );
    }

    #[test]
    fn classify_uri_splits_file_from_web() {
        assert_eq!(classify_uri("https://x.io/a"), url("https://x.io/a"));
        assert_eq!(
            classify_uri("file://host.local/Users/op/a%23b.md"),
            file("/Users/op/a#b.md", None, None)
        );
        assert_eq!(
            classify_uri("file:///src/main.rs:12:5"),
            file("/src/main.rs", Some(12), Some(5))
        );
    }

    #[test]
    fn paths_absolute_existing_only() {
        let fs = Fs::new(&["/etc/hosts"]);
        assert_eq!(one(&fs, "cat /etc/hosts").1, file("/etc/hosts", None, None));
        assert!(fs.detect("cat /etc/missing").is_empty());
        assert!(fs.detect("GET /api/v1/users 200").is_empty());
    }

    #[test]
    fn paths_home_relative() {
        let fs = Fs::new(&["/home/op/.zshrc"]);
        assert_eq!(
            one(&fs, "edit ~/.zshrc").1,
            file("/home/op/.zshrc", None, None)
        );
        assert!(fs.detect("edit ~/.bashrc").is_empty());
    }

    #[test]
    fn paths_relative_resolve_under_cwd_and_must_exist() {
        let fs = Fs::new(&["/work/repo/src/main.rs", "/work/repo/Makefile"]);
        assert_eq!(
            one(&fs, "M src/main.rs").1,
            file("/work/repo/src/main.rs", None, None)
        );
        assert_eq!(
            one(&fs, "./src/main.rs").1,
            file("/work/repo/src/main.rs", None, None)
        );
        assert_eq!(one(&fs, "see Makefile").0, "Makefile");
        assert!(fs.detect("M src/gone.rs").is_empty());
        assert!(fs.detect("hello world").is_empty());
    }

    #[test]
    fn paths_relative_need_a_known_cwd() {
        let fs = Fs::new(&["/work/repo/src/main.rs", "/etc/hosts"]).without_cwd();
        assert!(fs.detect("src/main.rs").is_empty());
        assert_eq!(one(&fs, "src/main.rs /etc/hosts").0, "/etc/hosts");
    }

    #[test]
    fn paths_git_diff_prefixes_fall_back_to_the_real_path() {
        let fs = Fs::new(&["/work/repo/src/main.rs"]);
        let targets: Vec<LinkTarget> = fs
            .detect("--- a/src/main.rs\n+++ b/src/main.rs")
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert_eq!(
            targets,
            vec![
                file("/work/repo/src/main.rs", None, None),
                file("/work/repo/src/main.rs", None, None)
            ]
        );
    }

    #[test]
    fn paths_after_an_equals_sign() {
        let fs = Fs::new(&["/work/repo/Cargo.toml"]);
        assert_eq!(
            one(&fs, "--manifest-path=/work/repo/Cargo.toml").0,
            "/work/repo/Cargo.toml"
        );
    }

    #[test]
    fn paths_trim_trailing_punctuation_and_unbalanced_brackets() {
        let fs = Fs::new(&["/work/repo/src/main.rs"]);
        for text in [
            "see src/main.rs.",
            "see src/main.rs,",
            "(src/main.rs)",
            "[src/main.rs]",
            "{src/main.rs}",
            "is it src/main.rs?",
            "src/main.rs:",
        ] {
            assert_eq!(one(&fs, text).0, "src/main.rs", "{text}");
        }
    }

    #[test]
    fn locations_rustc_style() {
        let fs = Fs::new(&["/work/repo/src/gui_tear_attach.rs"]);
        assert_eq!(
            one(&fs, "  --> src/gui_tear_attach.rs:1424:9"),
            (
                "src/gui_tear_attach.rs:1424:9".into(),
                file("/work/repo/src/gui_tear_attach.rs", Some(1424), Some(9))
            )
        );
        assert_eq!(
            one(&fs, "at src/gui_tear_attach.rs:1424").1,
            file("/work/repo/src/gui_tear_attach.rs", Some(1424), None)
        );
    }

    #[test]
    fn locations_go_and_eslint_style() {
        let fs = Fs::new(&["/work/repo/main.go", "/work/repo/web/app.js"]);
        assert_eq!(
            one(&fs, "./main.go:12:5: undefined: x"),
            (
                "./main.go:12:5".into(),
                file("/work/repo/main.go", Some(12), Some(5))
            )
        );
        assert_eq!(
            one(&fs, "/work/repo/web/app.js:3:14  error  no-undef").1,
            file("/work/repo/web/app.js", Some(3), Some(14))
        );
    }

    #[test]
    fn locations_grep_style() {
        let fs = Fs::new(&["/work/repo/src/lib.rs"]);
        assert_eq!(
            one(&fs, "src/lib.rs:88:pub fn main() {"),
            (
                "src/lib.rs:88".into(),
                file("/work/repo/src/lib.rs", Some(88), None)
            )
        );
        assert_eq!(
            one(&fs, "src/lib.rs:88:5:pub fn main() {").1,
            file("/work/repo/src/lib.rs", Some(88), Some(5))
        );
    }

    #[test]
    fn locations_tsc_paren_style() {
        let fs = Fs::new(&["/work/repo/src/a.ts"]);
        assert_eq!(
            one(&fs, "src/a.ts(12,5): error TS2304"),
            (
                "src/a.ts(12,5)".into(),
                file("/work/repo/src/a.ts", Some(12), Some(5))
            )
        );
        assert_eq!(
            one(&fs, "src/a.ts(7)").1,
            file("/work/repo/src/a.ts", Some(7), None)
        );
    }

    #[test]
    fn locations_of_missing_files_are_not_links() {
        let fs = Fs::new(&[]);
        assert!(fs.detect("src/nope.rs:12:5").is_empty());
        assert!(fs.detect("localhost:8080").is_empty());
    }

    #[test]
    fn spaces_only_inside_quotes() {
        let fs = Fs::new(&["/work/repo/My Notes/todo.md"]);
        assert!(fs.detect("open My Notes/todo.md").is_empty());
        assert_eq!(
            one(&fs, "open \"My Notes/todo.md\" now"),
            (
                "My Notes/todo.md".into(),
                file("/work/repo/My Notes/todo.md", None, None)
            )
        );
        assert_eq!(
            one(&fs, "open 'My Notes/todo.md:4'").1,
            file("/work/repo/My Notes/todo.md", Some(4), None)
        );
    }

    #[test]
    fn url_and_path_on_one_line_do_not_overlap() {
        let fs = Fs::new(&["/work/repo/README.md"]);
        assert_eq!(
            fs.detect("README.md https://example.com/README.md"),
            vec![
                ("README.md".into(), file("/work/repo/README.md", None, None)),
                (
                    "https://example.com/README.md".into(),
                    url("https://example.com/README.md")
                )
            ]
        );
    }

    #[test]
    fn detect_at_byte_picks_the_span_under_the_byte() {
        let files: HashSet<PathBuf> = [PathBuf::from("/work/repo/a.rs")].into();
        let exists = |p: &Path| files.contains(p);
        let ctx = PathContext {
            cwd: Some(Path::new("/work/repo")),
            home: None,
            exists: &exists,
        };
        let text = "a.rs:3 https://x.io/y";
        let at = |b| detect_at_byte(text, b, &ctx, true).map(|d| d.target);
        assert_eq!(at(0), Some(file("/work/repo/a.rs", Some(3), None)));
        assert_eq!(at(6), None);
        assert_eq!(at(10), Some(url("https://x.io/y")));
        assert_eq!(detect_at_byte(text, 0, &ctx, false).map(|d| d.target), None);
    }

    fn cells(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|ch| Cell {
                ch,
                ..Cell::default()
            })
            .collect()
    }

    fn nothing_exists(_: &Path) -> bool {
        false
    }

    fn bare_ctx() -> PathContext<'static> {
        PathContext {
            cwd: None,
            home: None,
            exists: &nothing_exists,
        }
    }

    #[test]
    fn wrapped_url_spans_both_rows() {
        let r0 = cells("see https://exam");
        let r1 = cells("ple.com/x  done ");
        let rows: Vec<&[Cell]> = vec![&r0, &r1];
        let line = LogicalLine::around(&rows, &[true, false], 16, 1).unwrap();
        let link = line
            .detected_at(CellPoint { row: 1, col: 2 }, &bare_ctx(), true)
            .unwrap();
        assert_eq!(link.target, url("https://example.com/x"));
        assert_eq!(link.start, CellPoint { row: 0, col: 4 });
        assert_eq!(link.end, CellPoint { row: 1, col: 8 });
        assert_eq!(
            link.segments(16),
            vec![
                Segment {
                    row: 0,
                    col_start: 4,
                    col_end: 15
                },
                Segment {
                    row: 1,
                    col_start: 0,
                    col_end: 8
                }
            ]
        );
        assert!(link.contains(CellPoint { row: 0, col: 15 }));
        assert!(!link.contains(CellPoint { row: 1, col: 9 }));
    }

    #[test]
    fn unwrapped_rows_are_separate_lines() {
        let r0 = cells("see https://exam");
        let r1 = cells("ple.com/x  done ");
        let rows: Vec<&[Cell]> = vec![&r0, &r1];
        let line = LogicalLine::around(&rows, &[false, false], 16, 1).unwrap();
        assert_eq!(line.text(), "ple.com/x  done ");
    }

    #[test]
    fn wrapped_path_with_location_spans_rows() {
        let r0 = cells("--> src/gui_t");
        let r1 = cells("ear.rs:14:2  ");
        let rows: Vec<&[Cell]> = vec![&r0, &r1];
        let line = LogicalLine::around(&rows, &[true, false], 13, 0).unwrap();
        let files: HashSet<PathBuf> = [PathBuf::from("/w/src/gui_tear.rs")].into();
        let exists = |p: &Path| files.contains(p);
        let ctx = PathContext {
            cwd: Some(Path::new("/w")),
            home: None,
            exists: &exists,
        };
        let link = line
            .detected_at(CellPoint { row: 0, col: 6 }, &ctx, true)
            .unwrap();
        assert_eq!(link.target, file("/w/src/gui_tear.rs", Some(14), Some(2)));
        assert_eq!(link.start, CellPoint { row: 0, col: 4 });
        assert_eq!(link.end, CellPoint { row: 1, col: 10 });
    }

    #[test]
    fn hyperlink_span_follows_the_link_id_across_a_wrap() {
        let mut r0 = cells("ab LINKTE");
        let mut r1 = cells("XT cd    ");
        for c in &mut r0[3..] {
            c.link_id = 1;
        }
        for c in &mut r1[..2] {
            c.link_id = 1;
        }
        let rows: Vec<&[Cell]> = vec![&r0, &r1];
        let line = LogicalLine::around(&rows, &[true, false], 9, 0).unwrap();
        let lookup = |id: u16| (id == 1).then_some("file://host/tmp/x%20y.txt");
        let link = line
            .hyperlink_at(CellPoint { row: 1, col: 0 }, lookup)
            .unwrap();
        assert_eq!(link.target, file("/tmp/x y.txt", None, None));
        assert_eq!(link.start, CellPoint { row: 0, col: 3 });
        assert_eq!(link.end, CellPoint { row: 1, col: 1 });
        assert!(
            line.hyperlink_at(CellPoint { row: 1, col: 3 }, lookup)
                .is_none()
        );
    }

    #[test]
    fn wide_glyphs_keep_display_columns() {
        let mut row = vec![
            Cell {
                ch: '🚀',
                width: 2,
                ..Cell::default()
            },
            Cell {
                ch: ' ',
                width: 0,
                ..Cell::default()
            },
        ];
        row.extend(cells(" https://a.co"));
        let rows: Vec<&[Cell]> = vec![&row];
        let line = LogicalLine::around(&rows, &[false], row.len(), 0).unwrap();
        let links = line.detect_all(&bare_ctx(), true);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].start, CellPoint { row: 0, col: 3 });
        assert!(
            line.detected_at(CellPoint { row: 0, col: 1 }, &bare_ctx(), true)
                .is_none()
        );
    }

    fn not_dir(_: &Path) -> bool {
        false
    }

    #[test]
    fn open_plan_url_uses_the_os_opener() {
        let plan = open_plan(
            &url("https://example.com/x"),
            &LinkEditor::default(),
            &not_dir,
            "nvim",
        );
        assert_eq!(plan.program, crate::url::os_opener());
        assert_eq!(plan.args, vec!["https://example.com/x".to_string()]);
    }

    #[test]
    fn open_plan_default_editor_is_the_cursor_url() {
        let plan = open_plan(
            &file("/w/My Notes/a#1.rs", Some(14), Some(2)),
            &LinkEditor::default(),
            &not_dir,
            "nvim",
        );
        assert_eq!(plan.program, crate::url::os_opener());
        assert_eq!(
            plan.args,
            vec!["cursor://file/w/My%20Notes/a%231.rs:14:2".to_string()]
        );
        let no_line = open_plan(
            &file("/w/a.rs", None, None),
            &LinkEditor::default(),
            &not_dir,
            "nvim",
        );
        assert_eq!(no_line.args, vec!["cursor://file/w/a.rs:1:1".to_string()]);
    }

    #[test]
    fn open_plan_command_expands_each_argument() {
        let editor = LinkEditor::Command {
            argv: vec!["zed".into(), "{path}:{line}:{col}".into()],
        };
        let plan = open_plan(&file("/w/a b.rs", Some(3), None), &editor, &not_dir, "nvim");
        assert_eq!(plan.program, "zed");
        assert_eq!(plan.args, vec!["/w/a b.rs:3:1".to_string()]);
        let empty = LinkEditor::Command { argv: vec![] };
        let fallback = open_plan(&file("/w/a.rs", None, None), &empty, &not_dir, "nvim");
        assert_eq!(fallback.program, crate::url::os_opener());
    }

    #[test]
    fn open_plan_system_env_and_directories() {
        let sys = open_plan(
            &file("/w/a.rs", Some(9), None),
            &LinkEditor::System {},
            &not_dir,
            "nvim",
        );
        assert_eq!(sys.program, crate::url::os_opener());
        assert_eq!(sys.args, vec!["/w/a.rs".to_string()]);
        let env = open_plan(
            &file("/w/a.rs", Some(9), None),
            &LinkEditor::Env {},
            &not_dir,
            "hx",
        );
        assert_eq!(env.program, "hx");
        assert_eq!(env.args, vec!["+9".to_string(), "/w/a.rs".to_string()]);
        let dir = open_plan(
            &file("/w/src", None, None),
            &LinkEditor::default(),
            &|_: &Path| true,
            "nvim",
        );
        assert_eq!(dir.program, crate::url::os_opener());
        assert_eq!(dir.args, vec!["/w/src".to_string()]);
    }
}
