//! Terminal emulation — VT100/xterm state machine via vte crate.
//!
//! Architecture follows Ghostty/Alacritty patterns:
//! - VecDeque-based grid for O(1) scroll operations
//! - Alternate screen buffer (for vim, less, etc.)
//! - DEC private modes (cursor visibility, autowrap, bracketed paste)
//! - Scroll regions (DECSTBM)
//! - DECSC/DECRC saved cursor state
//! - Sequence number damage tracking for efficient rendering

use std::collections::VecDeque;
use std::fmt;

use unicode_width::UnicodeWidthChar;

// ---------------------------------------------------------------------------
// Cell attributes (bitflags-style)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellAttrs(u8);

impl CellAttrs {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const UNDERLINE: Self = Self(1 << 2);
    pub const BLINK: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);
    pub const STRIKETHROUGH: Self = Self(1 << 5);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

// ---------------------------------------------------------------------------
// Mouse tracking modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    /// No mouse tracking.
    #[default]
    Off,
    /// Mode 1000: Normal tracking (press/release).
    Normal,
    /// Mode 1002: Button-event tracking (press/release/motion while pressed).
    ButtonEvent,
    /// Mode 1003: Any-event tracking (all motion).
    AnyEvent,
}

// ---------------------------------------------------------------------------
// Color
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const WHITE: Self = Self { r: 255, g: 255, b: 255 };
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::WHITE
    }
}

/// Standard 8-color ANSI palette (normal intensity).
const ANSI_COLORS: [Color; 8] = [
    Color::new(0, 0, 0),       // 0 black
    Color::new(205, 49, 49),   // 1 red
    Color::new(13, 188, 121),  // 2 green
    Color::new(229, 229, 16),  // 3 yellow
    Color::new(36, 114, 200),  // 4 blue
    Color::new(188, 63, 188),  // 5 magenta
    Color::new(17, 168, 205),  // 6 cyan
    Color::new(229, 229, 229), // 7 white
];

/// Bright ANSI palette (indices 8-15).
const ANSI_BRIGHT_COLORS: [Color; 8] = [
    Color::new(102, 102, 102), // 8  bright black
    Color::new(241, 76, 76),   // 9  bright red
    Color::new(35, 209, 139),  // 10 bright green
    Color::new(245, 245, 67),  // 11 bright yellow
    Color::new(59, 142, 234),  // 12 bright blue
    Color::new(214, 112, 214), // 13 bright magenta
    Color::new(41, 184, 219),  // 14 bright cyan
    Color::new(255, 255, 255), // 15 bright white
];

fn ansi_256_color(idx: u16) -> Color {
    match idx {
        0..=7 => ANSI_COLORS[idx as usize],
        8..=15 => ANSI_BRIGHT_COLORS[(idx - 8) as usize],
        16..=231 => {
            let idx = idx - 16;
            let r_idx = idx / 36;
            let g_idx = (idx % 36) / 6;
            let b_idx = idx % 6;
            let to_byte = |i: u16| -> u8 {
                if i == 0 { 0 } else { (55 + 40 * i) as u8 }
            };
            Color::new(to_byte(r_idx), to_byte(g_idx), to_byte(b_idx))
        }
        232..=255 => {
            let v = (8 + 10 * (idx - 232)) as u8;
            Color::new(v, v, v)
        }
        _ => Color::WHITE,
    }
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Extra codepoints for combining characters. None for the common case.
    pub extra: Option<Box<Vec<char>>>,
    /// Display width: 1 = normal, 2 = wide (CJK), 0 = continuation of wide char.
    pub width: u8,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Cell {
    /// Append a combining character to this cell.
    pub fn push_combining(&mut self, ch: char) {
        match &mut self.extra {
            Some(v) => v.push(ch),
            None => self.extra = Some(Box::new(vec![ch])),
        }
    }

    /// Write this cell's full text content to a string buffer.
    pub fn write_to(&self, buf: &mut String) {
        buf.push(self.ch);
        if let Some(ref extra) = self.extra {
            for &c in extra.iter() {
                buf.push(c);
            }
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            extra: None,
            width: 1,
            fg: Color::WHITE,
            bg: Color::BLACK,
            attrs: CellAttrs::NONE,
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self { row: 0, col: 0, visible: true }
    }
}

/// Saved cursor state for DECSC/DECRC.
#[derive(Debug, Clone)]
struct SavedCursor {
    row: usize,
    col: usize,
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
    origin_mode: bool,
}

// ---------------------------------------------------------------------------
// Grid — VecDeque-based terminal grid with O(1) scroll
// ---------------------------------------------------------------------------

struct Grid {
    /// All rows: scrollback at front, visible at back.
    rows: VecDeque<Vec<Cell>>,
    cols: usize,
    visible_rows: usize,
    max_scrollback: usize,
}

impl Grid {
    fn new(cols: usize, visible_rows: usize, max_scrollback: usize) -> Self {
        let mut rows = VecDeque::with_capacity(visible_rows + max_scrollback);
        for _ in 0..visible_rows {
            rows.push_back(vec![Cell::default(); cols]);
        }
        Self { rows, cols, visible_rows, max_scrollback }
    }

    /// Number of scrollback lines available.
    fn scrollback_len(&self) -> usize {
        self.rows.len().saturating_sub(self.visible_rows)
    }

    /// Access a visible row (0 = top of visible area).
    fn visible_row(&self, idx: usize) -> &[Cell] {
        let offset = self.scrollback_len();
        &self.rows[offset + idx]
    }

    /// Mutable access to a visible row.
    fn visible_row_mut(&mut self, idx: usize) -> &mut Vec<Cell> {
        let offset = self.scrollback_len();
        &mut self.rows[offset + idx]
    }

    /// Access a cell by visible row and column.
    fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.visible_row(row)[col]
    }

    /// Mutable access to a cell.
    fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        &mut self.visible_row_mut(row)[col]
    }

    /// Scroll the region [top..=bottom] up by one line.
    /// Top row is pushed to scrollback (only if top == 0).
    /// Bottom row becomes blank.
    fn scroll_region_up(&mut self, top: usize, bottom: usize) {
        let sb_offset = self.scrollback_len();

        if top == 0 && bottom == self.visible_rows - 1 {
            // Full-screen scroll: push top to scrollback, append blank
            self.rows.push_back(vec![Cell::default(); self.cols]);
            // Evict oldest scrollback if over limit
            while self.scrollback_len() > self.max_scrollback {
                self.rows.pop_front();
            }
        } else {
            // Partial scroll region: remove the top row, insert blank at bottom
            let remove_idx = sb_offset + top;
            self.rows.remove(remove_idx);
            let insert_idx = sb_offset + bottom;
            // After removal, indexes shifted down, so insert at the same logical position
            self.rows.insert(insert_idx, vec![Cell::default(); self.cols]);
        }
    }

    /// Scroll the region [top..=bottom] down by one line.
    /// Bottom row is discarded, blank line inserted at top.
    fn scroll_region_down(&mut self, top: usize, bottom: usize) {
        let sb_offset = self.scrollback_len();
        let remove_idx = sb_offset + bottom;
        if remove_idx < self.rows.len() {
            self.rows.remove(remove_idx);
        }
        self.rows.insert(sb_offset + top, vec![Cell::default(); self.cols]);
    }

    /// Clear a range of cells in a visible row.
    fn erase_cells(&mut self, row: usize, start: usize, end: usize) {
        let end = end.min(self.cols);
        let r = self.visible_row_mut(row);
        for col in start..end {
            r[col] = Cell::default();
        }
    }

    /// Clear the entire visible area.
    fn clear_visible(&mut self) {
        for i in 0..self.visible_rows {
            let row = self.visible_row_mut(i);
            for cell in row.iter_mut() {
                *cell = Cell::default();
            }
        }
    }

    /// Resize the grid.
    fn resize(&mut self, cols: usize, visible_rows: usize) {
        // Resize column width for all rows
        if cols != self.cols {
            for row in &mut self.rows {
                row.resize(cols, Cell::default());
            }
            self.cols = cols;
        }

        // Adjust visible rows
        match visible_rows.cmp(&self.visible_rows) {
            std::cmp::Ordering::Greater => {
                let extra = visible_rows - self.visible_rows;
                for _ in 0..extra {
                    self.rows.push_back(vec![Cell::default(); cols]);
                }
            }
            std::cmp::Ordering::Less => {
                // Remove rows from the bottom of visible area
                let remove = self.visible_rows - visible_rows;
                for _ in 0..remove {
                    self.rows.pop_back();
                }
            }
            std::cmp::Ordering::Equal => {}
        }
        self.visible_rows = visible_rows;
    }

    /// Iterator over visible rows.
    fn visible_rows_iter(&self) -> impl Iterator<Item = &[Cell]> {
        let offset = self.scrollback_len();
        self.rows.range(offset..).map(Vec::as_slice)
    }

    /// Iterator over scrollback rows at a given viewport offset.
    /// Returns `visible_rows` rows starting from the scroll position.
    fn viewport_rows(&self, scroll_offset: usize) -> impl Iterator<Item = &[Cell]> {
        let sb_len = self.scrollback_len();
        let offset = scroll_offset.min(sb_len);
        let start = sb_len - offset;
        self.rows.range(start..start + self.visible_rows).map(Vec::as_slice)
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

pub struct Terminal {
    primary: Grid,
    alternate: Grid,
    use_alternate: bool,

    cursor: Cursor,
    saved_cursor: Option<SavedCursor>,
    saved_cursor_alt: Option<SavedCursor>,

    cols: usize,
    rows: usize,

    // Pen state
    pen_fg: Color,
    pen_bg: Color,
    pen_attrs: CellAttrs,

    // Scroll region (0-based, inclusive)
    scroll_top: usize,
    scroll_bottom: usize,

    // Mode flags
    auto_wrap: bool,
    origin_mode: bool,
    cursor_keys_mode: bool,
    bracketed_paste: bool,
    /// Tracks whether the cursor is past the last column (pending wrap).
    wrap_pending: bool,

    // Mouse tracking
    mouse_mode: MouseMode,
    /// SGR extended mouse mode (mode 1006).
    sgr_mouse: bool,

    // Viewport scroll offset for user scrolling through history
    scroll_offset: usize,

    // Damage tracking
    seqno: u64,

    // Tab stops
    tab_stops: Vec<bool>,

    // Response bytes to send back to the PTY (for DSR, DA, etc.)
    response_bytes: Vec<u8>,

    // Synchronized output (CSI ? 2026) — batch drawing
    synchronized_output: bool,

    // Focus reporting (CSI ? 1004)
    focus_reporting: bool,

    // Last printed character (for REP — CSI b)
    last_char: char,

    // Window title (from OSC 0/2)
    title: Option<String>,

    // VT parser
    parser: vte::Parser,
}

impl fmt::Debug for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Terminal")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("cursor", &self.cursor)
            .field("seqno", &self.seqno)
            .field("use_alternate", &self.use_alternate)
            .field("scrollback_len", &self.primary.scrollback_len())
            .finish()
    }
}

impl Terminal {
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut tab_stops = vec![false; cols];
        for i in (0..cols).step_by(8) {
            tab_stops[i] = true;
        }

        Self {
            primary: Grid::new(cols, rows, 10_000),
            alternate: Grid::new(cols, rows, 0),
            use_alternate: false,
            cursor: Cursor::default(),
            saved_cursor: None,
            saved_cursor_alt: None,
            cols,
            rows,
            pen_fg: Color::WHITE,
            pen_bg: Color::BLACK,
            pen_attrs: CellAttrs::NONE,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            auto_wrap: true,
            origin_mode: false,
            cursor_keys_mode: false,
            bracketed_paste: false,
            wrap_pending: false,
            mouse_mode: MouseMode::Off,
            sgr_mouse: false,
            scroll_offset: 0,
            seqno: 0,
            tab_stops,
            response_bytes: Vec::new(),
            synchronized_output: false,
            focus_reporting: false,
            last_char: ' ',
            title: None,
            parser: vte::Parser::new(),
        }
    }

    // ── Public API ──────────────────────────────────────────────────

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
        parser.advance(self, bytes);
        self.parser = parser;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 {
            return;
        }

        self.primary.resize(cols, rows);
        self.alternate.resize(cols, rows);

        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);

        // Resize tab stops
        self.tab_stops.resize(cols, false);
        for i in (0..cols).step_by(8) {
            self.tab_stops[i] = true;
        }

        // Clamp cursor
        self.cursor.row = self.cursor.row.min(rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(cols.saturating_sub(1));
        self.wrap_pending = false;
        self.dirty();

        tracing::debug!(cols, rows, "terminal resized");
    }

    #[must_use]
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        self.grid().cell(row, col)
    }

    #[must_use]
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub fn seqno(&self) -> u64 {
        self.seqno
    }

    #[must_use]
    pub fn cursor_keys_mode(&self) -> bool {
        self.cursor_keys_mode
    }

    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Iterator over visible rows, accounting for scroll offset.
    pub fn visible_rows(&self) -> Box<dyn Iterator<Item = &[Cell]> + '_> {
        let grid = self.grid();
        if self.scroll_offset == 0 {
            Box::new(grid.visible_rows_iter())
        } else {
            Box::new(grid.viewport_rows(self.scroll_offset))
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let max = self.grid().scrollback_len();
        self.scroll_offset = (self.scroll_offset + lines).min(max);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    /// Take any pending response bytes (for DSR, DA, etc.).
    /// Returns `None` if no response is pending.
    pub fn take_response(&mut self) -> Option<Vec<u8>> {
        if self.response_bytes.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.response_bytes))
        }
    }

    /// Current mouse tracking mode.
    #[must_use]
    pub fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    /// Whether SGR extended mouse encoding is active.
    #[must_use]
    pub fn sgr_mouse(&self) -> bool {
        self.sgr_mouse
    }

    /// Whether focus reporting is enabled (mode 1004).
    #[must_use]
    pub fn focus_reporting(&self) -> bool {
        self.focus_reporting
    }

    /// Current window title (from OSC 0/2).
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn grid(&self) -> &Grid {
        if self.use_alternate { &self.alternate } else { &self.primary }
    }

    fn grid_mut(&mut self) -> &mut Grid {
        if self.use_alternate { &mut self.alternate } else { &mut self.primary }
    }

    fn dirty(&mut self) {
        self.seqno = self.seqno.wrapping_add(1);
    }

    fn scroll_grid_up(&mut self) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        self.grid_mut().scroll_region_up(top, bottom);
        self.dirty();
    }

    fn scroll_grid_down(&mut self) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        self.grid_mut().scroll_region_down(top, bottom);
        self.dirty();
    }

    fn newline(&mut self) {
        if self.cursor.row >= self.scroll_bottom {
            self.scroll_grid_up();
        } else {
            self.cursor.row += 1;
        }
        self.dirty();
    }

    fn put_char(&mut self, ch: char) {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(1);

        // Handle pending wrap from previous character at end of line
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor.col = 0;
            self.newline();
        }

        // Wide chars need 2 columns — wrap early if they won't fit
        if char_width == 2 && self.cursor.col + 1 >= self.cols {
            if self.auto_wrap {
                self.cursor.col = 0;
                self.newline();
            } else {
                self.dirty();
                return;
            }
        }

        let row = self.cursor.row;
        let col = self.cursor.col;
        if col < self.cols && row < self.rows {
            let fg = self.pen_fg;
            let bg = self.pen_bg;
            let attrs = self.pen_attrs;
            let cell = self.grid_mut().cell_mut(row, col);
            cell.ch = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.attrs = attrs;
            cell.extra = None;
            cell.width = char_width as u8;

            // Wide chars occupy 2 cells — mark next cell as continuation
            if char_width == 2 && col + 1 < self.cols {
                let cont = self.grid_mut().cell_mut(row, col + 1);
                cont.ch = ' ';
                cont.width = 0;
                cont.fg = fg;
                cont.bg = bg;
                cont.attrs = attrs;
                cont.extra = None;
            }
        }

        self.last_char = ch;

        let advance = char_width.max(1);
        if self.cursor.col + advance >= self.cols {
            if self.auto_wrap {
                self.wrap_pending = true;
            }
        } else {
            self.cursor.col += advance;
        }
        self.dirty();
    }

    fn erase_cells(&mut self, row: usize, start: usize, end: usize) {
        self.grid_mut().erase_cells(row, start, end);
        self.dirty();
    }

    fn save_cursor(&mut self) {
        let saved = SavedCursor {
            row: self.cursor.row,
            col: self.cursor.col,
            fg: self.pen_fg,
            bg: self.pen_bg,
            attrs: self.pen_attrs,
            origin_mode: self.origin_mode,
        };
        if self.use_alternate {
            self.saved_cursor_alt = Some(saved);
        } else {
            self.saved_cursor = Some(saved);
        }
    }

    fn restore_cursor(&mut self) {
        let saved = if self.use_alternate {
            self.saved_cursor_alt.take()
        } else {
            self.saved_cursor.take()
        };
        if let Some(s) = saved {
            self.cursor.row = s.row.min(self.rows.saturating_sub(1));
            self.cursor.col = s.col.min(self.cols.saturating_sub(1));
            self.pen_fg = s.fg;
            self.pen_bg = s.bg;
            self.pen_attrs = s.attrs;
            self.origin_mode = s.origin_mode;
            self.wrap_pending = false;
            self.dirty();
        }
    }

    fn enter_alternate_screen(&mut self) {
        if !self.use_alternate {
            self.save_cursor();
            self.use_alternate = true;
            self.alternate.clear_visible();
            self.cursor = Cursor::default();
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
            self.wrap_pending = false;
            self.dirty();
        }
    }

    fn exit_alternate_screen(&mut self) {
        if self.use_alternate {
            self.use_alternate = false;
            self.restore_cursor();
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
            self.wrap_pending = false;
            self.dirty();
        }
    }

    /// Handle DEC private mode set (CSI ? Ps h).
    fn dec_set(&mut self, mode: u16) {
        match mode {
            1 => self.cursor_keys_mode = true,    // DECCKM
            6 => {
                // DECOM — Origin Mode
                self.origin_mode = true;
                self.cursor.row = self.scroll_top;
                self.cursor.col = 0;
                self.wrap_pending = false;
                self.dirty();
            }
            7 => self.auto_wrap = true,            // DECAWM
            25 => {
                self.cursor.visible = true;        // DECTCEM
                self.dirty();
            }
            47 | 1047 => self.enter_alternate_screen(),
            1000 => self.mouse_mode = MouseMode::Normal,
            1002 => self.mouse_mode = MouseMode::ButtonEvent,
            1003 => self.mouse_mode = MouseMode::AnyEvent,
            1004 => self.focus_reporting = true,
            1006 => self.sgr_mouse = true,
            1049 => {
                self.save_cursor();
                self.enter_alternate_screen();
            }
            2004 => self.bracketed_paste = true,
            2026 => self.synchronized_output = true,
            _ => tracing::trace!(mode, "unhandled DECSET"),
        }
    }

    /// Handle DEC private mode reset (CSI ? Ps l).
    fn dec_reset(&mut self, mode: u16) {
        match mode {
            1 => self.cursor_keys_mode = false,
            6 => {
                self.origin_mode = false;
                self.cursor.row = 0;
                self.cursor.col = 0;
                self.wrap_pending = false;
                self.dirty();
            }
            7 => self.auto_wrap = false,
            25 => {
                self.cursor.visible = false;
                self.dirty();
            }
            47 | 1047 => self.exit_alternate_screen(),
            1000 | 1002 | 1003 => self.mouse_mode = MouseMode::Off,
            1004 => self.focus_reporting = false,
            1006 => self.sgr_mouse = false,
            1049 => {
                self.exit_alternate_screen();
                self.restore_cursor();
            }
            2004 => self.bracketed_paste = false,
            2026 => self.synchronized_output = false,
            _ => tracing::trace!(mode, "unhandled DECRST"),
        }
    }

    // ── SGR (colors/attributes) ─────────────────────────────────────

    fn handle_sgr(&mut self, params: &vte::Params) {
        let mut iter = params.iter();

        loop {
            let param = match iter.next() {
                Some(slice) => slice[0],
                None => break,
            };

            match param {
                0 => {
                    self.pen_fg = Color::WHITE;
                    self.pen_bg = Color::BLACK;
                    self.pen_attrs = CellAttrs::NONE;
                }
                1 => self.pen_attrs.insert(CellAttrs::BOLD),
                3 => self.pen_attrs.insert(CellAttrs::ITALIC),
                4 => self.pen_attrs.insert(CellAttrs::UNDERLINE),
                5 => self.pen_attrs.insert(CellAttrs::BLINK),
                7 => self.pen_attrs.insert(CellAttrs::INVERSE),
                9 => self.pen_attrs.insert(CellAttrs::STRIKETHROUGH),
                22 => self.pen_attrs.remove(CellAttrs::BOLD),
                23 => self.pen_attrs.remove(CellAttrs::ITALIC),
                24 => self.pen_attrs.remove(CellAttrs::UNDERLINE),
                25 => self.pen_attrs.remove(CellAttrs::BLINK),
                27 => self.pen_attrs.remove(CellAttrs::INVERSE),
                29 => self.pen_attrs.remove(CellAttrs::STRIKETHROUGH),
                30..=37 => self.pen_fg = ANSI_COLORS[(param - 30) as usize],
                38 => self.parse_extended_color(&mut iter, true),
                39 => self.pen_fg = Color::WHITE,
                40..=47 => self.pen_bg = ANSI_COLORS[(param - 40) as usize],
                48 => self.parse_extended_color(&mut iter, false),
                49 => self.pen_bg = Color::BLACK,
                90..=97 => self.pen_fg = ANSI_BRIGHT_COLORS[(param - 90) as usize],
                100..=107 => self.pen_bg = ANSI_BRIGHT_COLORS[(param - 100) as usize],
                _ => tracing::trace!(param, "unhandled SGR parameter"),
            }
        }
    }

    fn parse_extended_color(&mut self, iter: &mut vte::ParamsIter<'_>, is_fg: bool) {
        let Some(sub) = iter.next() else { return };
        match sub[0] {
            5 => {
                if let Some(idx_slice) = iter.next() {
                    let color = ansi_256_color(idx_slice[0]);
                    if is_fg { self.pen_fg = color; } else { self.pen_bg = color; }
                }
            }
            2 => {
                let r = iter.next().map_or(0, |s| s[0] as u8);
                let g = iter.next().map_or(0, |s| s[0] as u8);
                let b = iter.next().map_or(0, |s| s[0] as u8);
                let color = Color::new(r, g, b);
                if is_fg { self.pen_fg = color; } else { self.pen_bg = color; }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// vte::Perform
// ---------------------------------------------------------------------------

impl vte::Perform for Terminal {
    fn print(&mut self, ch: char) {
        // Reset scroll offset when new content arrives
        self.scroll_offset = 0;

        // Combining characters (zero-width) append to the previous cell
        if UnicodeWidthChar::width(ch) == Some(0) {
            let prev_col = if self.wrap_pending {
                self.cols.saturating_sub(1)
            } else if self.cursor.col > 0 {
                // Walk back past any continuation cells (wide char tails)
                let mut c = self.cursor.col - 1;
                while c > 0 && self.grid().cell(self.cursor.row, c).width == 0 {
                    c -= 1;
                }
                c
            } else {
                return; // No previous cell to combine with
            };
            let row = self.cursor.row;
            if prev_col < self.cols && row < self.rows {
                self.grid_mut().cell_mut(row, prev_col).push_combining(ch);
                self.dirty();
            }
            return;
        }

        self.put_char(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0B | 0x0C => {
                // LF, VT, FF all act as newline
                self.wrap_pending = false;
                self.newline();
            }
            b'\r' => {
                self.cursor.col = 0;
                self.wrap_pending = false;
                self.dirty();
            }
            b'\t' => {
                // Advance to next tab stop
                let start = self.cursor.col + 1;
                let stop = (start..self.cols)
                    .find(|&c| self.tab_stops.get(c).copied().unwrap_or(false))
                    .unwrap_or(self.cols.saturating_sub(1));
                self.cursor.col = stop;
                self.wrap_pending = false;
                self.dirty();
            }
            0x08 => {
                // Backspace
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.wrap_pending = false;
                self.dirty();
            }
            0x07 => {
                // Bell — no-op for now
                tracing::trace!("BEL");
            }
            _ => {
                tracing::trace!(byte, "unhandled execute byte");
            }
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        match params[0] {
            b"0" | b"2" => {
                if params.len() > 1 {
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    tracing::debug!(%title, "OSC set title");
                    self.title = Some(title);
                    self.dirty();
                }
            }
            _ => tracing::trace!(?params, "unhandled OSC sequence"),
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let first_param = |default: usize| -> usize {
            params.iter().next().map_or(default, |p| (p[0] as usize).max(1))
        };

        // Handle DEC private modes (CSI ? Ps h/l)
        if intermediates == [b'?'] {
            match action {
                'h' => {
                    for p in params.iter() {
                        self.dec_set(p[0]);
                    }
                    return;
                }
                'l' => {
                    for p in params.iter() {
                        self.dec_reset(p[0]);
                    }
                    return;
                }
                _ => {
                    tracing::trace!(action = %action, "unhandled CSI ? sequence");
                    return;
                }
            }
        }

        // Handle secondary DA (CSI > c)
        if intermediates == [b'>'] {
            if action == 'c' {
                // Secondary DA: report terminal type and version
                // Format: CSI > Pp ; Pv ; Pc c
                // 1 = VT220, 0 = firmware version, 0 = ROM version
                self.response_bytes.extend_from_slice(b"\x1b[>1;0;0c");
            } else {
                tracing::trace!(action = %action, "unhandled CSI > sequence");
            }
            return;
        }

        // Skip other sequences with unhandled intermediates
        if !intermediates.is_empty() {
            tracing::trace!(action = %action, ?intermediates, "CSI with intermediates (ignored)");
            return;
        }

        match action {
            // CUU — Cursor Up
            'A' => {
                let n = first_param(1);
                self.cursor.row = self.cursor.row.saturating_sub(n);
                self.wrap_pending = false;
                self.dirty();
            }
            // CUD — Cursor Down
            'B' => {
                let n = first_param(1);
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.wrap_pending = false;
                self.dirty();
            }
            // CUF — Cursor Forward
            'C' => {
                let n = first_param(1);
                self.cursor.col = (self.cursor.col + n).min(self.cols.saturating_sub(1));
                self.wrap_pending = false;
                self.dirty();
            }
            // CUB — Cursor Backward
            'D' => {
                let n = first_param(1);
                self.cursor.col = self.cursor.col.saturating_sub(n);
                self.wrap_pending = false;
                self.dirty();
            }
            // CNL — Cursor Next Line
            'E' => {
                let n = first_param(1);
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.cursor.col = 0;
                self.wrap_pending = false;
                self.dirty();
            }
            // CPL — Cursor Previous Line
            'F' => {
                let n = first_param(1);
                self.cursor.row = self.cursor.row.saturating_sub(n);
                self.cursor.col = 0;
                self.wrap_pending = false;
                self.dirty();
            }
            // CHA — Cursor Horizontal Absolute
            'G' => {
                let col = first_param(1);
                self.cursor.col = (col - 1).min(self.cols.saturating_sub(1));
                self.wrap_pending = false;
                self.dirty();
            }
            // CUP / HVP — Cursor Position
            'H' | 'f' => {
                let mut piter = params.iter();
                let row = piter.next().map_or(1, |p| (p[0] as usize).max(1));
                let col = piter.next().map_or(1, |p| (p[0] as usize).max(1));
                self.cursor.row = (row - 1).min(self.rows.saturating_sub(1));
                self.cursor.col = (col - 1).min(self.cols.saturating_sub(1));
                self.wrap_pending = false;
                self.dirty();
            }
            // ED — Erase in Display
            'J' => {
                let mode = params.iter().next().map_or(0, |p| p[0]);
                match mode {
                    0 => {
                        self.erase_cells(self.cursor.row, self.cursor.col, self.cols);
                        for r in (self.cursor.row + 1)..self.rows {
                            self.erase_cells(r, 0, self.cols);
                        }
                    }
                    1 => {
                        for r in 0..self.cursor.row {
                            self.erase_cells(r, 0, self.cols);
                        }
                        self.erase_cells(self.cursor.row, 0, self.cursor.col + 1);
                    }
                    2 | 3 => {
                        for r in 0..self.rows {
                            self.erase_cells(r, 0, self.cols);
                        }
                    }
                    _ => {}
                }
            }
            // EL — Erase in Line
            'K' => {
                let mode = params.iter().next().map_or(0, |p| p[0]);
                let row = self.cursor.row;
                match mode {
                    0 => self.erase_cells(row, self.cursor.col, self.cols),
                    1 => self.erase_cells(row, 0, self.cursor.col + 1),
                    2 => self.erase_cells(row, 0, self.cols),
                    _ => {}
                }
            }
            // IL — Insert Lines
            'L' => {
                let n = first_param(1);
                let cursor_row = self.cursor.row;
                let bottom = self.scroll_bottom;
                for _ in 0..n.min(bottom - cursor_row + 1) {
                    self.grid_mut().scroll_region_down(cursor_row, bottom);
                }
                self.dirty();
            }
            // DL — Delete Lines
            'M' => {
                let n = first_param(1);
                let cursor_row = self.cursor.row;
                let bottom = self.scroll_bottom;
                for _ in 0..n.min(bottom - cursor_row + 1) {
                    self.grid_mut().scroll_region_up(cursor_row, bottom);
                }
                self.dirty();
            }
            // DCH — Delete Characters
            'P' => {
                let n = first_param(1);
                let row = self.cursor.row;
                let col = self.cursor.col;
                let cols = self.cols;
                let r = self.grid_mut().visible_row_mut(row);
                for _ in 0..n.min(cols - col) {
                    if col < r.len() {
                        r.remove(col);
                        r.push(Cell::default());
                    }
                }
                self.dirty();
            }
            // SU — Scroll Up
            'S' => {
                let n = first_param(1);
                for _ in 0..n {
                    self.scroll_grid_up();
                }
            }
            // SD — Scroll Down
            'T' => {
                let n = first_param(1);
                for _ in 0..n {
                    self.scroll_grid_down();
                }
            }
            // ECH — Erase Characters
            'X' => {
                let n = first_param(1);
                let row = self.cursor.row;
                let col = self.cursor.col;
                self.erase_cells(row, col, col + n);
            }
            // REP — Repeat preceding graphic character
            'b' => {
                let n = first_param(1);
                let ch = self.last_char;
                for _ in 0..n {
                    self.put_char(ch);
                }
            }
            // ICH — Insert Characters
            '@' => {
                let n = first_param(1);
                let row = self.cursor.row;
                let col = self.cursor.col;
                let cols = self.cols;
                let r = self.grid_mut().visible_row_mut(row);
                for _ in 0..n.min(cols - col) {
                    r.insert(col, Cell::default());
                    r.truncate(cols);
                }
                self.dirty();
            }
            // VPA — Vertical Position Absolute
            'd' => {
                let row = first_param(1);
                self.cursor.row = (row - 1).min(self.rows.saturating_sub(1));
                self.wrap_pending = false;
                self.dirty();
            }
            // SGR — Select Graphic Rendition
            'm' => {
                if params.iter().next().is_none() {
                    self.pen_fg = Color::WHITE;
                    self.pen_bg = Color::BLACK;
                    self.pen_attrs = CellAttrs::NONE;
                } else {
                    self.handle_sgr(params);
                }
            }
            // DSR — Device Status Report
            'n' => {
                let mode = params.iter().next().map_or(0, |p| p[0]);
                match mode {
                    5 => {
                        // Status report: terminal OK
                        self.response_bytes.extend_from_slice(b"\x1b[0n");
                    }
                    6 => {
                        // CPR: report cursor position (1-based)
                        let response = format!(
                            "\x1b[{};{}R",
                            self.cursor.row + 1,
                            self.cursor.col + 1
                        );
                        self.response_bytes.extend_from_slice(response.as_bytes());
                    }
                    _ => tracing::trace!(mode, "unhandled DSR"),
                }
            }
            // DECSTBM — Set Top and Bottom Margins (scroll region)
            'r' => {
                let mut piter = params.iter();
                let top = piter.next().map_or(1, |p| (p[0] as usize).max(1));
                let bottom = piter.next().map_or(self.rows, |p| (p[0] as usize).max(1));
                let top = (top - 1).min(self.rows.saturating_sub(1));
                let bottom = (bottom - 1).min(self.rows.saturating_sub(1));
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    // Cursor moves to home position
                    self.cursor.row = if self.origin_mode { top } else { 0 };
                    self.cursor.col = 0;
                    self.wrap_pending = false;
                    self.dirty();
                }
            }
            // DA — Device Attributes
            'c' => {
                // Report VT220 compatible terminal
                self.response_bytes.extend_from_slice(b"\x1b[?62;22c");
            }
            // DECSC — Save Cursor (alternate form)
            's' => self.save_cursor(),
            // DECRC — Restore Cursor (alternate form)
            'u' => self.restore_cursor(),
            _ => {
                tracing::trace!(action = %action, "unhandled CSI action");
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            // RIS — Full reset
            ([], b'c') => {
                let cols = self.cols;
                let rows = self.rows;
                *self = Terminal::new(cols, rows);
                tracing::debug!("terminal reset (RIS)");
            }
            // IND — Index (move cursor down, scroll if at bottom)
            ([], b'D') => {
                self.newline();
            }
            // NEL — Next Line
            ([], b'E') => {
                self.cursor.col = 0;
                self.wrap_pending = false;
                self.newline();
            }
            // HTS — Horizontal Tab Set
            ([], b'H') => {
                if self.cursor.col < self.tab_stops.len() {
                    self.tab_stops[self.cursor.col] = true;
                }
            }
            // RI — Reverse Index
            ([], b'M') => {
                if self.cursor.row <= self.scroll_top {
                    self.scroll_grid_down();
                } else {
                    self.cursor.row -= 1;
                    self.dirty();
                }
            }
            // DECSC — Save Cursor
            ([], b'7') => self.save_cursor(),
            // DECRC — Restore Cursor
            ([], b'8') => self.restore_cursor(),
            _ => {
                tracing::trace!(byte, ?intermediates, "unhandled ESC dispatch");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_terminal_has_empty_grid() {
        let term = Terminal::new(80, 24);
        assert_eq!(term.cols(), 80);
        assert_eq!(term.rows(), 24);
        assert_eq!(term.cell(0, 0).ch, ' ');
        assert_eq!(term.cursor().row, 0);
        assert_eq!(term.cursor().col, 0);
    }

    #[test]
    fn print_characters() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"Hello");
        assert_eq!(term.cell(0, 0).ch, 'H');
        assert_eq!(term.cell(0, 1).ch, 'e');
        assert_eq!(term.cell(0, 2).ch, 'l');
        assert_eq!(term.cell(0, 3).ch, 'l');
        assert_eq!(term.cell(0, 4).ch, 'o');
        assert_eq!(term.cursor().col, 5);
    }

    #[test]
    fn newline_and_carriage_return() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"AB\r\nCD");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, 'B');
        assert_eq!(term.cell(1, 0).ch, 'C');
        assert_eq!(term.cell(1, 1).ch, 'D');
    }

    #[test]
    fn tab_stops() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"A\tB");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 8).ch, 'B');
    }

    #[test]
    fn backspace() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"AB\x08C");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, 'C');
    }

    #[test]
    fn cursor_movement_csi() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[6;11H");
        assert_eq!(term.cursor().row, 5);
        assert_eq!(term.cursor().col, 10);

        term.feed(b"\x1b[2A");
        assert_eq!(term.cursor().row, 3);

        term.feed(b"\x1b[5C");
        assert_eq!(term.cursor().col, 15);
    }

    #[test]
    fn erase_in_display() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"AAAAAAAAAA");
        term.feed(b"BBBBBBBBBB");
        term.feed(b"CCCCCCCCCC");

        term.feed(b"\x1b[2;6H\x1b[0J");

        assert_eq!(term.cell(1, 0).ch, 'B');
        assert_eq!(term.cell(1, 4).ch, 'B');
        assert_eq!(term.cell(1, 5).ch, ' ');
        assert_eq!(term.cell(2, 0).ch, ' ');
    }

    #[test]
    fn erase_in_line() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"ABCDEFGHIJ");
        term.feed(b"\x1b[1;6H\x1b[0K");
        assert_eq!(term.cell(0, 4).ch, 'E');
        assert_eq!(term.cell(0, 5).ch, ' ');
        assert_eq!(term.cell(0, 9).ch, ' ');
    }

    #[test]
    fn sgr_bold_and_color() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[1;31mX");
        let cell = term.cell(0, 0);
        assert_eq!(cell.ch, 'X');
        assert!(cell.attrs.contains(CellAttrs::BOLD));
        assert_eq!(cell.fg, ANSI_COLORS[1]);
    }

    #[test]
    fn sgr_reset() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[1;31mA\x1b[0mB");
        let a = term.cell(0, 0);
        assert!(a.attrs.contains(CellAttrs::BOLD));
        let b = term.cell(0, 1);
        assert!(b.attrs.is_empty());
        assert_eq!(b.fg, Color::WHITE);
    }

    #[test]
    fn sgr_truecolor() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[38;2;100;150;200mX");
        assert_eq!(term.cell(0, 0).fg, Color::new(100, 150, 200));
    }

    #[test]
    fn sgr_256color() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[38;5;196mX");
        assert_eq!(term.cell(0, 0).fg, ansi_256_color(196));
    }

    #[test]
    fn scrollback_on_overflow() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"LINE1\r\n");
        term.feed(b"LINE2\r\n");
        term.feed(b"LINE3\r\n");
        assert!(term.primary.scrollback_len() >= 1);
    }

    #[test]
    fn scroll_viewport() {
        let mut term = Terminal::new(10, 3);
        for i in 0..6 {
            let line = format!("LINE{i}\r\n");
            term.feed(line.as_bytes());
        }

        let sb_len = term.primary.scrollback_len();
        assert!(sb_len > 0);

        term.scroll_up(2);
        assert_eq!(term.scroll_offset(), 2);

        term.scroll_down(1);
        assert_eq!(term.scroll_offset(), 1);

        term.scroll_down(100);
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn resize_clamps_cursor() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[20;70H");
        term.resize(40, 10);
        assert_eq!(term.cursor().row, 9);
        assert_eq!(term.cursor().col, 39);
    }

    #[test]
    fn line_wrap() {
        let mut term = Terminal::new(5, 3);
        term.feed(b"ABCDEFG");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 4).ch, 'E');
        assert_eq!(term.cell(1, 0).ch, 'F');
        assert_eq!(term.cell(1, 1).ch, 'G');
    }

    #[test]
    fn cell_attrs_bitflag_operations() {
        let mut attrs = CellAttrs::NONE;
        assert!(attrs.is_empty());
        attrs.insert(CellAttrs::BOLD);
        attrs.insert(CellAttrs::ITALIC);
        assert!(attrs.contains(CellAttrs::BOLD));
        assert!(attrs.contains(CellAttrs::ITALIC));
        assert!(!attrs.contains(CellAttrs::UNDERLINE));
        attrs.remove(CellAttrs::BOLD);
        assert!(!attrs.contains(CellAttrs::BOLD));
        assert!(attrs.contains(CellAttrs::ITALIC));
    }

    #[test]
    fn esc_reverse_index() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"AAA\r\nBBB\r\nCCC");
        term.feed(b"\x1bM");
        assert_eq!(term.cursor().row, 1);
    }

    #[test]
    fn esc_full_reset() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"HELLO");
        term.feed(b"\x1bc");
        assert_eq!(term.cell(0, 0).ch, ' ');
        assert_eq!(term.cursor().row, 0);
        assert_eq!(term.cursor().col, 0);
    }

    #[test]
    fn visible_rows_no_scroll() {
        let mut term = Terminal::new(5, 3);
        term.feed(b"AAAAA");
        term.feed(b"BBBBB");
        term.feed(b"CCCCC");

        let rows: Vec<_> = term.visible_rows().collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0].ch, 'A');
        assert_eq!(rows[1][0].ch, 'B');
        assert_eq!(rows[2][0].ch, 'C');
    }

    #[test]
    fn insert_and_delete_lines() {
        let mut term = Terminal::new(5, 3);
        term.feed(b"AAAAA");
        term.feed(b"BBBBB");
        term.feed(b"CCCCC");

        term.feed(b"\x1b[2;1H\x1b[1L");
        assert_eq!(term.cell(1, 0).ch, ' ');
        assert_eq!(term.cell(2, 0).ch, 'B');
    }

    #[test]
    fn cursor_visibility() {
        let mut term = Terminal::new(80, 24);
        assert!(term.cursor().visible);

        // CSI ? 25 l — hide cursor
        term.feed(b"\x1b[?25l");
        assert!(!term.cursor().visible);

        // CSI ? 25 h — show cursor
        term.feed(b"\x1b[?25h");
        assert!(term.cursor().visible);
    }

    #[test]
    fn alternate_screen() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"PRIMARY");
        assert_eq!(term.cell(0, 0).ch, 'P');

        // Enter alternate screen
        term.feed(b"\x1b[?1049h");
        assert!(term.use_alternate);
        assert_eq!(term.cell(0, 0).ch, ' ');
        assert_eq!(term.cursor().row, 0);

        term.feed(b"ALT");
        assert_eq!(term.cell(0, 0).ch, 'A');

        // Exit alternate screen
        term.feed(b"\x1b[?1049l");
        assert!(!term.use_alternate);
        assert_eq!(term.cell(0, 0).ch, 'P');
    }

    #[test]
    fn scroll_region() {
        let mut term = Terminal::new(10, 5);
        for i in 0..5 {
            let line = format!("LINE{i}");
            term.feed(line.as_bytes());
            if i < 4 {
                term.feed(b"\r\n");
            }
        }

        // Set scroll region to rows 2-4 (1-based: 2;4)
        term.feed(b"\x1b[2;4r");
        assert_eq!(term.scroll_top, 1);
        assert_eq!(term.scroll_bottom, 3);
    }

    #[test]
    fn save_restore_cursor() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[5;10H"); // Move to row 4, col 9
        term.feed(b"\x1b7");      // Save cursor (ESC 7)
        term.feed(b"\x1b[1;1H");  // Move to home
        assert_eq!(term.cursor().row, 0);
        term.feed(b"\x1b8");      // Restore cursor (ESC 8)
        assert_eq!(term.cursor().row, 4);
        assert_eq!(term.cursor().col, 9);
    }

    #[test]
    fn damage_tracking() {
        let mut term = Terminal::new(10, 3);
        let s0 = term.seqno();
        term.feed(b"A");
        assert!(term.seqno() > s0);

        let s1 = term.seqno();
        // Reading doesn't change seqno
        let _ = term.cell(0, 0);
        assert_eq!(term.seqno(), s1);
    }

    #[test]
    fn bracketed_paste_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.bracketed_paste());
        term.feed(b"\x1b[?2004h");
        assert!(term.bracketed_paste());
        term.feed(b"\x1b[?2004l");
        assert!(!term.bracketed_paste());
    }

    #[test]
    fn wide_char_occupies_two_cells() {
        let mut term = Terminal::new(10, 3);
        // '中' is a CJK character with width 2
        term.feed("中".as_bytes());
        assert_eq!(term.cell(0, 0).ch, '中');
        assert_eq!(term.cell(0, 0).width, 2);
        // Next cell is continuation
        assert_eq!(term.cell(0, 1).width, 0);
        // Cursor advances by 2
        assert_eq!(term.cursor().col, 2);
    }

    #[test]
    fn wide_char_wraps_at_edge() {
        let mut term = Terminal::new(5, 3);
        // Fill 4 columns, then try a wide char that needs 2
        term.feed(b"ABCD");
        assert_eq!(term.cursor().col, 4);
        // Wide char at col 4 of 5 cols can't fit — wraps to next line
        term.feed("中".as_bytes());
        assert_eq!(term.cell(1, 0).ch, '中');
        assert_eq!(term.cell(1, 0).width, 2);
        assert_eq!(term.cursor().row, 1);
    }

    #[test]
    fn dsr_cursor_position_report() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[5;10H");
        assert!(term.take_response().is_none());
        // DSR 6 = report cursor position
        term.feed(b"\x1b[6n");
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[5;10R");
        // Second call returns None
        assert!(term.take_response().is_none());
    }

    #[test]
    fn dsr_status_report() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[5n");
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[0n");
    }

    #[test]
    fn device_attributes_response() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[c");
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[?62;22c");
    }

    #[test]
    fn mouse_mode_tracking() {
        let mut term = Terminal::new(80, 24);
        assert_eq!(term.mouse_mode(), MouseMode::Off);

        term.feed(b"\x1b[?1000h");
        assert_eq!(term.mouse_mode(), MouseMode::Normal);

        term.feed(b"\x1b[?1002h");
        assert_eq!(term.mouse_mode(), MouseMode::ButtonEvent);

        term.feed(b"\x1b[?1003h");
        assert_eq!(term.mouse_mode(), MouseMode::AnyEvent);

        term.feed(b"\x1b[?1003l");
        assert_eq!(term.mouse_mode(), MouseMode::Off);
    }

    #[test]
    fn sgr_mouse_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.sgr_mouse());

        term.feed(b"\x1b[?1006h");
        assert!(term.sgr_mouse());

        term.feed(b"\x1b[?1006l");
        assert!(!term.sgr_mouse());
    }

    #[test]
    fn origin_mode() {
        let mut term = Terminal::new(80, 24);
        // Set scroll region to rows 5-15
        term.feed(b"\x1b[5;15r");
        // Enable origin mode
        term.feed(b"\x1b[?6h");
        // Cursor should be at scroll region top
        assert_eq!(term.cursor().row, 4); // 0-indexed row 4 = 1-based row 5

        // Disable origin mode
        term.feed(b"\x1b[?6l");
        assert_eq!(term.cursor().row, 0);
    }

    #[test]
    fn synchronized_output_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.synchronized_output);
        term.feed(b"\x1b[?2026h");
        assert!(term.synchronized_output);
        term.feed(b"\x1b[?2026l");
        assert!(!term.synchronized_output);
    }

    #[test]
    fn focus_reporting_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.focus_reporting());
        term.feed(b"\x1b[?1004h");
        assert!(term.focus_reporting());
        term.feed(b"\x1b[?1004l");
        assert!(!term.focus_reporting());
    }

    #[test]
    fn rep_repeat_character() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"A\x1b[3b");
        // Should repeat 'A' 3 more times
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, 'A');
        assert_eq!(term.cell(0, 2).ch, 'A');
        assert_eq!(term.cell(0, 3).ch, 'A');
        assert_eq!(term.cursor().col, 4);
    }

    #[test]
    fn secondary_device_attributes() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[>c");
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[>1;0;0c");
    }

    #[test]
    fn osc_sets_title() {
        let mut term = Terminal::new(80, 24);
        // OSC 0 ; title ST
        term.feed(b"\x1b]0;my terminal\x1b\\");
        assert_eq!(term.title(), Some("my terminal"));
    }

    #[test]
    fn cell_write_to_with_combining() {
        let mut cell = Cell::default();
        cell.ch = 'e';
        cell.push_combining('\u{0301}'); // combining acute accent
        let mut buf = String::new();
        cell.write_to(&mut buf);
        // Decomposed form: base char + combining char
        assert_eq!(buf, "e\u{0301}");
        assert_eq!(buf.chars().count(), 2);
    }
}
