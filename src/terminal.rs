//! Terminal emulation — VT100/xterm state machine via vte crate.
//!
//! Architecture follows Ghostty/Alacritty patterns:
//! - VecDeque-based grid for O(1) scroll operations
//! - Alternate screen buffer (for vim, less, etc.)
//! - DEC private modes (cursor visibility, autowrap, bracketed paste)
//! - Scroll regions (DECSTBM)
//! - DECSC/DECRC saved cursor state
//! - Sequence number damage tracking for efficient rendering

use std::collections::{HashMap, VecDeque};
use std::fmt;

use unicode_width::UnicodeWidthChar;

use crate::config::CursorStyle;

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
    pub const DIM: Self = Self(1 << 6);
    pub const HIDDEN: Self = Self(1 << 7);

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
    #[allow(dead_code)]
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
pub const ANSI_COLORS: [Color; 8] = [
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
pub const ANSI_BRIGHT_COLORS: [Color; 8] = [
    Color::new(102, 102, 102), // 8  bright black
    Color::new(241, 76, 76),   // 9  bright red
    Color::new(35, 209, 139),  // 10 bright green
    Color::new(245, 245, 67),  // 11 bright yellow
    Color::new(59, 142, 234),  // 12 bright blue
    Color::new(214, 112, 214), // 13 bright magenta
    Color::new(41, 184, 219),  // 14 bright cyan
    Color::new(255, 255, 255), // 15 bright white
];

/// Build the default 16-color ANSI palette from the const arrays.
#[must_use]
pub fn default_ansi_palette() -> [Color; 16] {
    let mut palette = [Color::BLACK; 16];
    palette[..8].copy_from_slice(&ANSI_COLORS);
    palette[8..].copy_from_slice(&ANSI_BRIGHT_COLORS);
    palette
}

/// If the given color matches a normal ANSI color (0-7) in the palette, return the bright variant.
/// Used by the renderer for bold-as-bright behavior.
#[must_use]
pub fn bold_bright_color(color: &Color, palette: &[Color; 16]) -> Color {
    for i in 0..8 {
        if color == &palette[i] {
            return palette[i + 8];
        }
    }
    *color
}

fn ansi_256_color(idx: u16, palette: &[Color; 16]) -> Color {
    match idx {
        0..=15 => palette[idx as usize],
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
    /// Hyperlink URL (from OSC 8). None for most cells.
    pub hyperlink: Option<Box<String>>,
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
            hyperlink: None,
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
// Kitty graphics protocol
// ---------------------------------------------------------------------------

/// Decoded image data stored in the terminal's image cache.
#[derive(Clone)]
pub struct KittyImage {
    /// Unique image ID assigned by the client or auto-generated.
    #[allow(dead_code)]
    pub id: u32,
    /// RGBA pixel data (4 bytes per pixel).
    pub data: Vec<u8>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Sequence number when this image was last modified.
    pub seqno: u64,
}

/// A placement of an image at a specific cell position.
#[derive(Clone, Debug)]
pub struct ImagePlacement {
    /// Image ID (references KittyImage in the cache).
    pub image_id: u32,
    /// Placement ID (for targeted deletion).
    pub placement_id: u32,
    /// Column where this placement starts.
    pub col: usize,
    /// Row where this placement starts (absolute grid row, not scrollback-relative).
    pub row: usize,
    /// Number of columns to display in (0 = auto from image).
    pub cols: usize,
    /// Number of rows to display in (0 = auto from image).
    pub rows: usize,
    /// Pixel offset within the cell.
    pub x_offset: u32,
    pub y_offset: u32,
    /// Source region crop (0 = full image).
    pub src_x: u32,
    pub src_y: u32,
    pub src_width: u32,
    pub src_height: u32,
    /// Z-index for layering.
    #[allow(dead_code)]
    pub z_index: i32,
}

/// Sixel image placeholder — raw data stored for future rendering via `icy_sixel`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SixelImage {
    pub data: Vec<u8>,
    pub row: usize,
    pub col: usize,
}

/// DCS handler state.
enum DcsHandler {
    /// DECRQSS — Request Setting State. Accumulates the setting identifier.
    Decrqss(Vec<u8>),
    /// Sixel image data accumulation (DCS q or DCS Ps;Ps q).
    Sixel,
}

/// Accumulator for multi-chunk Kitty image transmissions.
struct KittyPending {
    params: HashMap<u8, String>,
    data_chunks: Vec<u8>,
}

#[allow(dead_code)]
impl KittyPending {
    fn new(params: HashMap<u8, String>, data: Vec<u8>) -> Self {
        Self {
            params,
            data_chunks: data,
        }
    }

    fn param_u32(&self, key: u8) -> u32 {
        self.params
            .get(&key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn param_char(&self, key: u8) -> char {
        self.params
            .get(&key)
            .and_then(|v| v.chars().next())
            .unwrap_or('\0')
    }
}

/// Parse Kitty graphics APC payload: `key=value,key=value;base64data`
fn parse_kitty_params(payload: &[u8]) -> (HashMap<u8, String>, Vec<u8>) {
    // Find the semicolon separating params from data
    let (param_part, data_part) = match payload.iter().position(|&b| b == b';') {
        Some(pos) => (&payload[..pos], &payload[pos + 1..]),
        None => (payload, &[] as &[u8]),
    };

    let mut params = HashMap::new();
    let param_str = String::from_utf8_lossy(param_part);
    for kv in param_str.split(',') {
        if let Some((k, v)) = kv.split_once('=') {
            if let Some(key_byte) = k.bytes().next() {
                params.insert(key_byte, v.to_string());
            }
        }
    }

    // Decode base64 data
    let decoded = if data_part.is_empty() {
        Vec::new()
    } else {
        base64_decode_bytes(data_part)
    };

    (params, decoded)
}

/// Base64 decode to raw bytes (not string).
fn base64_decode_bytes(input: &[u8]) -> Vec<u8> {
    let cleaned: Vec<u8> = input
        .iter()
        .copied()
        .filter(|&b| b != b'\n' && b != b'\r' && b != b' ')
        .collect();
    data_encoding::BASE64.decode(&cleaned).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// TerminalOps trait — abstraction for testability
// ---------------------------------------------------------------------------

/// Trait abstracting terminal operations for testability.
/// Allows substituting a mock terminal in tests without requiring
/// a full VT100 parser, PTY, or grid.
#[allow(dead_code)]
pub trait TerminalOps: Send {
    fn cols(&self) -> usize;
    fn rows(&self) -> usize;
    fn cursor(&self) -> &Cursor;
    fn cell(&self, row: usize, col: usize) -> &Cell;
    fn feed(&mut self, data: &[u8]);
    fn resize(&mut self, cols: usize, rows: usize);
    fn reset(&mut self);
    fn scroll_up(&mut self, lines: usize);
    fn scroll_down(&mut self, lines: usize);
    fn scroll_to_top(&mut self);
    fn scroll_to_bottom(&mut self);
    fn scroll_offset(&self) -> usize;
    fn seqno(&self) -> u64;
    fn take_response(&mut self) -> Option<Vec<u8>>;
    fn title(&self) -> Option<&str>;
    fn mouse_mode(&self) -> MouseMode;
    fn take_bell(&mut self) -> bool;
    fn kitty_keyboard_flags(&self) -> u32;
    fn cursor_keys_mode(&self) -> bool;
    fn keypad_app_mode(&self) -> bool;
    fn bracketed_paste(&self) -> bool;
    fn sgr_mouse(&self) -> bool;
    fn focus_reporting(&self) -> bool;
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

    // Default colors (set by theme; used for SGR 0/39/49 resets)
    default_fg: Color,
    default_bg: Color,

    // Active 16-color ANSI palette (can be overridden by theme)
    ansi_colors: [Color; 16],

    // Scroll region (0-based, inclusive)
    scroll_top: usize,
    scroll_bottom: usize,

    // Mode flags
    auto_wrap: bool,
    origin_mode: bool,
    cursor_keys_mode: bool,
    bracketed_paste: bool,
    /// Insert mode (IRM): true = insert, false = replace.
    insert_mode: bool,
    /// Keypad application mode (DECKPAM): true = application, false = numeric.
    keypad_app_mode: bool,
    /// Tracks whether the cursor is past the last column (pending wrap).
    wrap_pending: bool,

    // Character set designation (G0/G1).
    // false = ASCII (B), true = DEC Special Graphics (0).
    charset_g0_graphics: bool,
    charset_g1_graphics: bool,
    /// true = GL points to G1 (shift-out), false = GL points to G0 (shift-in).
    gl_is_g1: bool,

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

    // Current working directory (from OSC 7)
    cwd: Option<String>,

    // Bell state (BEL character received, cleared after read)
    bell_pending: bool,

    // Dynamic cursor shape (DECSCUSR)
    pub cursor_style: CursorStyle,
    pub cursor_blink: bool,

    // Active hyperlink URI (from OSC 8, applied to subsequent cells)
    active_hyperlink: Option<String>,

    // OSC 52 clipboard content (set by terminal, read by main for clipboard sync)
    clipboard_content: Option<String>,

    // OSC 9 desktop notifications queued by the terminal — the main
    // event loop drains + dispatches these (typically via
    // `tsuuchi`). Each entry is one notification body; the format
    // `\x1b]9;BODY\x07` from `notify.sh` pushes a single string.
    pending_notifications: Vec<String>,

    // Content-addressed mirror of every OSC 52 payload this session
    // has seen. The system clipboard still takes the top-of-stack
    // (via `clipboard_content`), but `clipboard_store` keeps the
    // full history keyed by BLAKE3 prefix so MCP tools + escriba
    // workflows can reference a specific past copy by hash.
    clipboard_store: crate::clipboard_store::ClipboardStore,

    // Shell integration markers (from OSC 133)
    prompt_start_row: Option<usize>,

    // Kitty keyboard protocol — progressive enhancement mode stack.
    // Each entry is the flags bitmask pushed by the application.
    // Bit 0 (1):  Disambiguate escape codes
    // Bit 1 (2):  Report event types
    // Bit 2 (4):  Report alternate keys
    // Bit 3 (8):  Report all keys as escape codes
    // Bit 4 (16): Report associated text
    kitty_keyboard_stack: Vec<u32>,

    // Kitty graphics protocol — image cache and placements
    images: HashMap<u32, KittyImage>,
    image_placements: Vec<ImagePlacement>,
    next_image_id: u32,
    pending_kitty: Option<KittyPending>,

    // Sixel image storage (placeholder for future icy_sixel rendering)
    pub sixel_images: Vec<SixelImage>,
    sixel_buffer: Option<Vec<u8>>,

    // APC sequence accumulator (ESC _ ... ST)
    apc_buf: Option<Vec<u8>>,

    // DCS handler state
    dcs_handler: Option<DcsHandler>,

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
    #[allow(dead_code)]
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_scrollback(cols, rows, 10_000)
    }

    #[must_use]
    pub fn with_scrollback(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        let mut tab_stops = vec![false; cols];
        for i in (0..cols).step_by(8) {
            tab_stops[i] = true;
        }

        Self {
            primary: Grid::new(cols, rows, max_scrollback),
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
            default_fg: Color::WHITE,
            default_bg: Color::BLACK,
            ansi_colors: default_ansi_palette(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            auto_wrap: true,
            origin_mode: false,
            cursor_keys_mode: false,
            bracketed_paste: false,
            insert_mode: false,
            keypad_app_mode: false,
            wrap_pending: false,
            charset_g0_graphics: false,
            charset_g1_graphics: false,
            gl_is_g1: false,
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
            cwd: None,
            bell_pending: false,
            cursor_style: CursorStyle::Block,
            cursor_blink: true,
            active_hyperlink: None,
            clipboard_content: None,
            pending_notifications: Vec::new(),
            clipboard_store: crate::clipboard_store::ClipboardStore::new(128),
            prompt_start_row: None,
            kitty_keyboard_stack: Vec::new(),
            images: HashMap::new(),
            image_placements: Vec::new(),
            next_image_id: 1,
            pending_kitty: None,
            sixel_images: Vec::new(),
            sixel_buffer: None,
            apc_buf: None,
            dcs_handler: None,
            parser: vte::Parser::new(),
        }
    }

    /// Apply a color theme: set default fg/bg and the 16-color ANSI palette.
    /// Resets the current pen colors to the new defaults.
    pub fn apply_theme(&mut self, fg: Color, bg: Color, ansi: [Color; 16]) {
        self.default_fg = fg;
        self.default_bg = bg;
        self.pen_fg = fg;
        self.pen_bg = bg;
        self.ansi_colors = ansi;
        self.dirty();
    }

    /// The active 16-color ANSI palette (may be overridden by a theme).
    #[must_use]
    #[allow(dead_code)]
    pub fn ansi_palette(&self) -> &[Color; 16] {
        &self.ansi_colors
    }

    // ── Public API ──────────────────────────────────────────────────

    pub fn feed(&mut self, bytes: &[u8]) {
        // Intercept APC sequences (ESC _ G ... ST) for Kitty graphics.
        // vte swallows APC content without dispatching, so we parse it manually.
        let mut i = 0;
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());

        while i < bytes.len() {
            // If we're inside an APC sequence, accumulate until ST
            if let Some(ref mut buf) = self.apc_buf {
                // ST = ESC \ (0x1b 0x5c) or 0x9c
                if bytes[i] == 0x9c {
                    let payload = std::mem::take(buf);
                    self.apc_buf = None;
                    self.handle_apc(&payload);
                    i += 1;
                    continue;
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    let payload = std::mem::take(buf);
                    self.apc_buf = None;
                    self.handle_apc(&payload);
                    i += 2;
                    continue;
                }
                buf.push(bytes[i]);
                i += 1;
                continue;
            }

            // Detect APC start: ESC _ (0x1b 0x5f)
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'_' {
                self.apc_buf = Some(Vec::new());
                i += 2;
                continue;
            }

            // Find the next ESC that might start an APC
            let start = i;
            while i < bytes.len() {
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'_' {
                    break;
                }
                i += 1;
            }

            // Feed the non-APC portion to vte
            if start < i {
                parser.advance(self, &bytes[start..i]);
            }
        }

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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn keypad_app_mode(&self) -> bool {
        self.keypad_app_mode
    }

    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    #[must_use]
    #[allow(dead_code)]
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

    /// Current working directory (from OSC 7).
    #[must_use]
    #[allow(dead_code)]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Check and clear the bell flag. Returns true if BEL was received.
    pub fn take_bell(&mut self) -> bool {
        std::mem::replace(&mut self.bell_pending, false)
    }

    /// Take pending clipboard content set by OSC 52.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard_content.take()
    }

    /// Drain the OSC 9 notification queue. Each element is one
    /// notification body the terminal saw; the main loop
    /// dispatches them (tsuuchi on the fleet). Iterator-style
    /// instead of `Vec<_>` so callers can fire-and-forget each
    /// one without holding the whole batch in memory first.
    #[allow(dead_code)] // Wired by main.rs once notifier glue lands.
    pub fn drain_notifications(&mut self) -> std::vec::Drain<'_, String> {
        self.pending_notifications.drain(..)
    }

    // ── OSC dispatch helpers ────────────────────────────────────────────────
    //
    // The big `osc_dispatch` match keeps the per-code branches short; each
    // delegates to one of these methods for anything beyond a one-liner.
    // Names spell out which OSC code they handle so `grep osc_133` (etc.)
    // drops you on the implementation.

    /// OSC 52 — Clipboard manipulation.
    ///
    /// Format: `ESC ] 52 ; <clipboard> ; <data> ST`
    ///   - `clipboard`: `c` (clipboard) / `p` (primary) / `s` (secondary)
    ///   - `data`: base64-encoded string, or `?` to query.
    ///
    /// We don't surface the system clipboard's *contents* back through the
    /// pty (that'd be a privacy regression — programs shouldn't read what
    /// the user copied elsewhere), so queries answer with an empty payload.
    /// Every successful set additionally indexes into [`clipboard_store`]
    /// so the session keeps a content-addressed history callable by
    /// BLAKE3-prefix hash via the planned MCP tool.
    fn handle_osc_52_clipboard(&mut self, params: &[&[u8]]) {
        if params.len() < 3 {
            return;
        }
        let data = params[2];
        if data == b"?" {
            // Query — answer empty; keeps the protocol happy without
            // leaking host clipboard state.
            self.response_bytes.extend_from_slice(b"\x1b]52;c;\x1b\\");
            return;
        }
        if let Some(text) = base64_decode(data) {
            let kind = crate::clipboard_store::ClipboardKind::from_osc52_byte(params[1]);
            let hash = self.clipboard_store.store(text.clone(), kind);
            tracing::debug!(
                len = text.len(),
                kind = kind.label(),
                hash = %hash.to_hex(),
                "OSC 52 clipboard set"
            );
            self.clipboard_content = Some(text);
        }
    }

    /// Read-only access to the content-addressed clipboard history.
    /// Consumed by the `clipboard_list` / `clipboard_get` MCP tools
    /// so external clients can fetch a specific past copy by hash
    /// without needing OS-clipboard access.
    #[must_use]
    #[allow(dead_code)] // Wired by mcp.rs once the clipboard tool lands.
    pub fn clipboard_store(&self) -> &crate::clipboard_store::ClipboardStore {
        &self.clipboard_store
    }

    /// OSC 104 — Reset indexed ANSI palette entries.
    ///
    /// Format: `ESC ] 104 ; <idx1> ; <idx2> … ST`
    /// No indices = reset all 16 ANSI entries.
    /// Listed indices in `0..16` reset to the compiled default palette;
    /// entries outside that range are ignored (we don't yet model the
    /// extended 16..=255 color cube as overridable).
    fn handle_osc_104_palette_reset(&mut self, params: &[&[u8]]) {
        if params.len() == 1 {
            self.ansi_colors = default_ansi_palette();
            self.dirty();
            return;
        }
        for p in &params[1..] {
            if let Ok(idx_str) = std::str::from_utf8(p)
                && let Ok(idx) = idx_str.parse::<usize>()
                && idx < 16
            {
                self.ansi_colors[idx] = default_ansi_palette()[idx];
                self.dirty();
            }
        }
    }

    /// OSC 0 / 2 — Set window title.
    ///
    /// Format: `ESC ] 0 ; <title> ST` (OSC 0 sets both icon-name
    /// and window title; OSC 2 sets just the window title — mado
    /// treats them identically since we don't surface icon names).
    fn handle_osc_0_2_title(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        let title = String::from_utf8_lossy(params[1]).into_owned();
        tracing::debug!(%title, "OSC set title");
        self.title = Some(title);
        self.dirty();
    }

    /// OSC 7 — Report current working directory.
    ///
    /// Shells emit this after every `cd` via the installed
    /// shell-integration scripts. Payload is a `file://hostname/path`
    /// URI; we strip the scheme + host for the internal `cwd`
    /// field. Format: `ESC ] 7 ; file://HOST/PATH ST`.
    fn handle_osc_7_cwd(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        let uri = String::from_utf8_lossy(params[1]).into_owned();
        let path = if let Some(stripped) = uri.strip_prefix("file://") {
            // Skip the hostname — everything up to the first `/` of
            // the path component.
            stripped
                .find('/')
                .map_or(stripped, |idx| &stripped[idx..])
                .to_string()
        } else {
            // No scheme — accept as-is. Ghostty / iTerm2 also tolerate
            // this even though the spec wants the URI form.
            uri
        };
        tracing::debug!(%path, "OSC 7 set CWD");
        self.cwd = Some(path);
    }

    /// OSC 8 — Hyperlink delimiter.
    ///
    /// Format: `ESC ] 8 ; <params> ; <URI> ST`. Empty URI (or a
    /// short-form sequence with only one param) ends the active
    /// hyperlink run; subsequent cells paint without underline-style
    /// hyperlinking until the next non-empty OSC 8.
    fn handle_osc_8_hyperlink(&mut self, params: &[&[u8]]) {
        if params.len() < 3 {
            self.active_hyperlink = None;
            return;
        }
        let uri = String::from_utf8_lossy(params[2]).into_owned();
        self.active_hyperlink = if uri.is_empty() { None } else { Some(uri) };
    }

    /// OSC 9 — Desktop notification (iTerm2 / ghostty compat).
    ///
    /// Format: `ESC ] 9 ; <body> ST`  (ST = `ESC \` or BEL). Empty
    /// body is a no-op (the spec lets `ESC ] 9 ; ST` mean a
    /// "bell-like ping" — we prefer the explicit BEL for that so
    /// the notification queue only carries real messages).
    fn handle_osc_9_notification(&mut self, params: &[&[u8]]) {
        if params.len() < 2 || params[1].is_empty() {
            return;
        }
        let body = String::from_utf8_lossy(params[1]).into_owned();
        tracing::debug!(%body, "OSC 9 notification");
        self.pending_notifications.push(body);
    }

    /// OSC 110 — Reset foreground to the compiled default. Matches
    /// the xterm idiom shells use to un-do an earlier `\e]10;…` set.
    fn handle_osc_110_fg_reset(&mut self) {
        self.pen_fg = self.default_fg;
        self.dirty();
    }

    /// OSC 111 — Reset background to the compiled default. We don't
    /// store an overridden copy of the baseline bg, so the reset is
    /// a no-op beyond marking dirty; lives as a named method so the
    /// dispatch table stays symmetric with 110 / 112.
    fn handle_osc_111_bg_reset(&mut self) {
        self.dirty();
    }

    /// OSC 112 — Reset cursor color to default. Cursor color isn't
    /// separately stored yet; reset marks dirty for future
    /// consistency.
    fn handle_osc_112_cursor_reset(&mut self) {
        self.dirty();
    }

    /// OSC 4 — Set or query an indexed ANSI palette entry.
    ///
    /// Query form: `ESC ] 4 ; <idx> ; ? ST` — answers with the
    /// current RGB. Set form: `ESC ] 4 ; <idx> ; <color> ST` where
    /// `color` is either `#rrggbb` or `rgb:RR/GG/BB` / the xterm
    /// double-width variant. Indices outside 0..16 are silently
    /// ignored (we don't yet model the 16..=255 cube as mutable).
    fn handle_osc_4_palette(&mut self, params: &[&[u8]]) {
        if params.len() < 3 {
            return;
        }
        let Some(idx) = parse_palette_index(params[1]) else {
            return;
        };
        if idx >= 16 {
            return;
        }
        if params[2] == b"?" {
            let response = format!(
                "\x1b]4;{idx};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\",
                r = self.ansi_colors[idx].r,
                g = self.ansi_colors[idx].g,
                b = self.ansi_colors[idx].b,
            );
            self.response_bytes.extend_from_slice(response.as_bytes());
            return;
        }
        if let Some(c) = parse_osc_color(params[2]) {
            self.ansi_colors[idx] = c;
            self.dirty();
        }
    }

    /// OSC 10 — Query or set the default foreground color.
    /// Query with `?`, set with `#rrggbb` or `rgb:RR/GG/BB`.
    fn handle_osc_10_foreground(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        if params[1] == b"?" {
            let resp = osc_rgb_query_response(10, self.pen_fg);
            self.response_bytes.extend_from_slice(resp.as_bytes());
            return;
        }
        if let Some(c) = parse_osc_color(params[1]) {
            self.pen_fg = c;
            self.default_fg = c;
            self.dirty();
        }
    }

    /// OSC 11 — Query or set the default background color.
    fn handle_osc_11_background(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        if params[1] == b"?" {
            let resp = osc_rgb_query_response(11, self.default_bg);
            self.response_bytes.extend_from_slice(resp.as_bytes());
            return;
        }
        if let Some(c) = parse_osc_color(params[1]) {
            self.default_bg = c;
            self.dirty();
        }
    }

    /// OSC 12 — Query or set the cursor color. The cursor currently
    /// tracks `default_fg`; set-path updates both so programs that
    /// customize the cursor see the change reflected in queries.
    fn handle_osc_12_cursor(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        if params[1] == b"?" {
            let resp = osc_rgb_query_response(12, self.default_fg);
            self.response_bytes.extend_from_slice(resp.as_bytes());
            return;
        }
        if let Some(c) = parse_osc_color(params[1]) {
            self.default_fg = c;
            self.dirty();
        }
    }

    /// OSC 133 — Shell integration (semantic prompts).
    ///
    /// `A` = prompt start, `B` = command start, `C` = command output,
    /// `D` = command end. Shells emit these via the installed
    /// shell-integration scripts (see `shell-integration/mado.*`).
    /// `A` captures the prompt row so the renderer can scroll back
    /// to the *start* of the previous command on a keystroke.
    fn handle_osc_133_shell_integration(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        match params[1] {
            b"A" => {
                self.prompt_start_row = Some(self.cursor.row);
                tracing::trace!(row = self.cursor.row, "OSC 133 prompt start");
            }
            b"B" => tracing::trace!("OSC 133 command start"),
            b"C" => tracing::trace!("OSC 133 command output"),
            b"D" => tracing::trace!("OSC 133 command end"),
            _ => {}
        }
    }

    /// Row where the last shell prompt started (from OSC 133).
    #[must_use]
    #[allow(dead_code)]
    pub fn prompt_start_row(&self) -> Option<usize> {
        self.prompt_start_row
    }

    /// Full terminal reset (RIS). Preserves scrollback setting and theme colors.
    pub fn reset(&mut self) {
        let cols = self.cols;
        let rows = self.rows;
        let max_scrollback = self.primary.max_scrollback;
        let default_fg = self.default_fg;
        let default_bg = self.default_bg;
        let ansi_colors = self.ansi_colors;
        *self = Terminal::with_scrollback(cols, rows, max_scrollback);
        self.default_fg = default_fg;
        self.default_bg = default_bg;
        self.pen_fg = default_fg;
        self.pen_bg = default_bg;
        self.ansi_colors = ansi_colors;
    }

    /// Soft terminal reset (DECSTR — CSI ! p).
    /// Resets modes and attributes without clearing the screen or scrollback.
    pub fn soft_reset(&mut self) {
        self.cursor.visible = true;
        self.origin_mode = false;
        self.auto_wrap = true;
        self.insert_mode = false;
        self.keypad_app_mode = false;
        self.cursor_keys_mode = false;
        self.bracketed_paste = false;
        self.pen_fg = self.default_fg;
        self.pen_bg = self.default_bg;
        self.pen_attrs = CellAttrs::NONE;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.saved_cursor = None;
        self.saved_cursor_alt = None;
        self.charset_g0_graphics = false;
        self.charset_g1_graphics = false;
        self.gl_is_g1 = false;
        self.wrap_pending = false;
        self.kitty_keyboard_stack.clear();
        self.dirty();
    }

    /// Screen alignment test (DECALN — ESC # 8).
    /// Fills the entire screen with 'E' characters.
    fn fill_screen_with_e(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.wrap_pending = false;

        let grid = self.grid_mut();
        for row in 0..grid.visible_rows {
            for col in 0..grid.cols {
                let cell = grid.cell_mut(row, col);
                cell.ch = 'E';
                cell.fg = Color::WHITE;
                cell.bg = Color::BLACK;
                cell.attrs = CellAttrs::NONE;
                cell.width = 1;
                cell.extra = None;
                cell.hyperlink = None;
            }
        }
        self.dirty();
    }

    /// Map ASCII to DEC Special Graphics characters when the active charset uses graphics.
    fn translate_charset(&self, ch: char) -> char {
        let use_graphics = if self.gl_is_g1 {
            self.charset_g1_graphics
        } else {
            self.charset_g0_graphics
        };
        if !use_graphics {
            return ch;
        }
        // DEC Special Graphics character set (VT100 line drawing)
        match ch {
            '`' => '\u{25C6}', // ◆ diamond
            'a' => '\u{2592}', // ▒ checkerboard
            'b' => '\u{2409}', // HT symbol
            'c' => '\u{240C}', // FF symbol
            'd' => '\u{240D}', // CR symbol
            'e' => '\u{240A}', // LF symbol
            'f' => '\u{00B0}', // ° degree
            'g' => '\u{00B1}', // ± plus/minus
            'h' => '\u{2424}', // NL symbol
            'i' => '\u{240B}', // VT symbol
            'j' => '\u{2518}', // ┘ lower right corner
            'k' => '\u{2510}', // ┐ upper right corner
            'l' => '\u{250C}', // ┌ upper left corner
            'm' => '\u{2514}', // └ lower left corner
            'n' => '\u{253C}', // ┼ crossing lines
            'o' => '\u{23BA}', // scan line 1
            'p' => '\u{23BB}', // scan line 3
            'q' => '\u{2500}', // ─ horizontal line
            'r' => '\u{23BC}', // scan line 7
            's' => '\u{23BD}', // scan line 9
            't' => '\u{251C}', // ├ left tee
            'u' => '\u{2524}', // ┤ right tee
            'v' => '\u{2534}', // ┴ bottom tee
            'w' => '\u{252C}', // ┬ top tee
            'x' => '\u{2502}', // │ vertical line
            'y' => '\u{2264}', // ≤ less-or-equal
            'z' => '\u{2265}', // ≥ greater-or-equal
            '{' => '\u{03C0}', // π pi
            '|' => '\u{2260}', // ≠ not-equal
            '}' => '\u{00A3}', // £ pound sterling
            '~' => '\u{00B7}', // · middle dot
            _ => ch,
        }
    }

    /// Scroll viewport to the top of scrollback.
    pub fn scroll_to_top(&mut self) {
        let max = self.grid().scrollback_len();
        self.scroll_offset = max;
    }

    /// Scroll viewport to the bottom (live view).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Current Kitty keyboard protocol flags (0 = protocol not active).
    #[must_use]
    pub fn kitty_keyboard_flags(&self) -> u32 {
        self.kitty_keyboard_stack.last().copied().unwrap_or(0)
    }

    /// Access the image cache (image ID → decoded RGBA data).
    #[must_use]
    pub fn images(&self) -> &HashMap<u32, KittyImage> {
        &self.images
    }

    /// Access current image placements.
    #[must_use]
    pub fn image_placements(&self) -> &[ImagePlacement] {
        &self.image_placements
    }

    // ── Kitty graphics protocol ─────────────────────────────────────

    /// Handle a complete APC sequence payload.
    fn handle_apc(&mut self, payload: &[u8]) {
        // Kitty graphics: payload starts with 'G'
        if payload.first() != Some(&b'G') {
            tracing::trace!("unhandled APC sequence (not Kitty graphics)");
            return;
        }

        let (params, data) = parse_kitty_params(&payload[1..]);

        // Check if this is a continuation of a multi-chunk transmission
        let pending_complete = if let Some(ref mut pending) = self.pending_kitty {
            pending.data_chunks.extend_from_slice(&data);
            let more = params
                .get(&b'm')
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            more == 0
        } else {
            false
        };

        if pending_complete {
            let pending = self.pending_kitty.take().unwrap();
            self.process_kitty_image(&pending.params, &pending.data_chunks);
            return;
        }

        if self.pending_kitty.is_some() {
            // Still accumulating chunks
            return;
        }

        // Check 'm' param for multi-chunk
        let more = params
            .get(&b'm')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);

        if more == 1 {
            // First chunk of multi-chunk transmission
            self.pending_kitty = Some(KittyPending::new(params, data));
            return;
        }

        // Single-chunk transmission
        self.process_kitty_image(&params, &data);
    }

    /// Process a complete Kitty graphics command.
    fn process_kitty_image(&mut self, params: &HashMap<u8, String>, data: &[u8]) {
        let action = params
            .get(&b'a')
            .and_then(|v| v.chars().next())
            .unwrap_or('T');

        match action {
            't' | 'T' => self.kitty_transmit(params, data, action == 'T'),
            'p' => self.kitty_place(params),
            'd' => self.kitty_delete(params),
            'q' => {
                // Query: respond with OK for the image id
                let id = params
                    .get(&b'i')
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0);
                let resp = format!("\x1b_Gi={id};OK\x1b\\");
                self.response_bytes.extend_from_slice(resp.as_bytes());
            }
            _ => {
                tracing::trace!(action = %action, "unhandled Kitty graphics action");
            }
        }
    }

    /// Transmit (and optionally display) image data.
    fn kitty_transmit(&mut self, params: &HashMap<u8, String>, data: &[u8], display: bool) {
        let format = params
            .get(&b'f')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(32);
        let width = params
            .get(&b's')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let height = params
            .get(&b'v')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let id = params
            .get(&b'i')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or_else(|| {
                let id = self.next_image_id;
                self.next_image_id += 1;
                id
            });

        // Decode image data to RGBA
        let rgba = match format {
            100 => {
                // PNG format — decode using image crate
                match image::load_from_memory_with_format(data, image::ImageFormat::Png) {
                    Ok(img) => {
                        let rgba_img = img.to_rgba8();
                        Some((
                            rgba_img.to_vec(),
                            rgba_img.width(),
                            rgba_img.height(),
                        ))
                    }
                    Err(e) => {
                        tracing::warn!("Kitty graphics: PNG decode error: {e}");
                        None
                    }
                }
            }
            32 => {
                // Direct RGBA
                if width > 0 && height > 0 {
                    Some((data.to_vec(), width, height))
                } else {
                    None
                }
            }
            24 => {
                // Direct RGB — convert to RGBA
                if width > 0 && height > 0 {
                    let mut rgba = Vec::with_capacity(data.len() / 3 * 4);
                    for chunk in data.chunks(3) {
                        if chunk.len() == 3 {
                            rgba.extend_from_slice(chunk);
                            rgba.push(255);
                        }
                    }
                    Some((rgba, width, height))
                } else {
                    None
                }
            }
            _ => {
                tracing::trace!(format, "unsupported Kitty image format");
                None
            }
        };

        if let Some((rgba_data, w, h)) = rgba {
            let image = KittyImage {
                id,
                data: rgba_data,
                width: w,
                height: h,
                seqno: self.seqno,
            };
            self.images.insert(id, image);

            // Send OK response
            let resp = format!("\x1b_Gi={id};OK\x1b\\");
            self.response_bytes.extend_from_slice(resp.as_bytes());

            if display {
                self.kitty_place_at_cursor(id, params);
            }

            self.dirty();
            tracing::debug!(id, w, h, "Kitty image stored");
        }
    }

    /// Place a previously transmitted image.
    fn kitty_place(&mut self, params: &HashMap<u8, String>) {
        let id = params
            .get(&b'i')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        if !self.images.contains_key(&id) {
            tracing::warn!(id, "Kitty place: image not found");
            return;
        }
        self.kitty_place_at_cursor(id, params);
        self.dirty();
    }

    /// Place an image at the current cursor position.
    fn kitty_place_at_cursor(&mut self, image_id: u32, params: &HashMap<u8, String>) {
        let placement_id = params
            .get(&b'p')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let cols = params
            .get(&b'c')
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let rows = params
            .get(&b'r')
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let x_offset = params
            .get(&b'x')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let y_offset = params
            .get(&b'y')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let src_x = params
            .get(&b'X')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let src_y = params
            .get(&b'Y')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let src_width = params
            .get(&b'w')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let src_height = params
            .get(&b'h')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let z_index = params
            .get(&b'z')
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);

        let placement = ImagePlacement {
            image_id,
            placement_id,
            col: self.cursor.col,
            row: self.cursor.row,
            cols,
            rows,
            x_offset,
            y_offset,
            src_x,
            src_y,
            src_width,
            src_height,
            z_index,
        };

        self.image_placements.push(placement);
    }

    /// Delete images/placements per Kitty protocol 'd' action.
    fn kitty_delete(&mut self, params: &HashMap<u8, String>) {
        let what = params
            .get(&b'd')
            .and_then(|v| v.chars().next())
            .unwrap_or('a');
        let id = params
            .get(&b'i')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let placement_id = params
            .get(&b'p')
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);

        match what {
            'a' | 'A' => {
                // Delete all images and placements
                self.images.clear();
                self.image_placements.clear();
            }
            'i' | 'I' => {
                // Delete by image id
                if id > 0 {
                    self.images.remove(&id);
                    self.image_placements.retain(|p| p.image_id != id);
                }
            }
            'p' | 'P' => {
                // Delete by placement id within an image
                if id > 0 && placement_id > 0 {
                    self.image_placements
                        .retain(|p| !(p.image_id == id && p.placement_id == placement_id));
                }
            }
            'c' | 'C' => {
                // Delete at cursor position
                let col = self.cursor.col;
                let row = self.cursor.row;
                self.image_placements
                    .retain(|p| !(p.col == col && p.row == row));
            }
            _ => {
                tracing::trace!(what = %what, "unhandled Kitty delete type");
            }
        }

        self.dirty();
        tracing::debug!(what = %what, id, "Kitty image deleted");
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
            // Insert mode (IRM): shift existing cells to the right
            if self.insert_mode {
                let grid = self.grid_mut();
                let end = grid.cols.saturating_sub(char_width);
                let line = grid.visible_row_mut(row);
                for c in (col..end).rev() {
                    let src = line[c].clone();
                    line[c + char_width] = src;
                }
            }

            let fg = self.pen_fg;
            let bg = self.pen_bg;
            let attrs = self.pen_attrs;
            let hyperlink = self.active_hyperlink.as_ref().map(|u| Box::new(u.clone()));
            let cell = self.grid_mut().cell_mut(row, col);
            cell.ch = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.attrs = attrs;
            cell.extra = None;
            cell.width = char_width as u8;
            cell.hyperlink = hyperlink;

            // Wide chars occupy 2 cells — mark next cell as continuation
            if char_width == 2 && col + 1 < self.cols {
                let hyperlink = self.active_hyperlink.as_ref().map(|u| Box::new(u.clone()));
                let cont = self.grid_mut().cell_mut(row, col + 1);
                cont.ch = ' ';
                cont.width = 0;
                cont.fg = fg;
                cont.bg = bg;
                cont.attrs = attrs;
                cont.extra = None;
                cont.hyperlink = hyperlink;
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
                    self.pen_fg = self.default_fg;
                    self.pen_bg = self.default_bg;
                    self.pen_attrs = CellAttrs::NONE;
                }
                1 => self.pen_attrs.insert(CellAttrs::BOLD),
                2 => self.pen_attrs.insert(CellAttrs::DIM),
                3 => self.pen_attrs.insert(CellAttrs::ITALIC),
                4 => self.pen_attrs.insert(CellAttrs::UNDERLINE),
                5 => self.pen_attrs.insert(CellAttrs::BLINK),
                7 => self.pen_attrs.insert(CellAttrs::INVERSE),
                8 => self.pen_attrs.insert(CellAttrs::HIDDEN),
                9 => self.pen_attrs.insert(CellAttrs::STRIKETHROUGH),
                22 => {
                    // SGR 22 resets both bold and dim
                    self.pen_attrs.remove(CellAttrs::BOLD);
                    self.pen_attrs.remove(CellAttrs::DIM);
                }
                23 => self.pen_attrs.remove(CellAttrs::ITALIC),
                24 => self.pen_attrs.remove(CellAttrs::UNDERLINE),
                25 => self.pen_attrs.remove(CellAttrs::BLINK),
                27 => self.pen_attrs.remove(CellAttrs::INVERSE),
                28 => self.pen_attrs.remove(CellAttrs::HIDDEN),
                29 => self.pen_attrs.remove(CellAttrs::STRIKETHROUGH),
                30..=37 => self.pen_fg = self.ansi_colors[(param - 30) as usize],
                38 => self.parse_extended_color(&mut iter, true),
                39 => self.pen_fg = self.default_fg,
                40..=47 => self.pen_bg = self.ansi_colors[(param - 40) as usize],
                48 => self.parse_extended_color(&mut iter, false),
                49 => self.pen_bg = self.default_bg,
                90..=97 => self.pen_fg = self.ansi_colors[(param - 90 + 8) as usize],
                100..=107 => self.pen_bg = self.ansi_colors[(param - 100 + 8) as usize],
                _ => tracing::trace!(param, "unhandled SGR parameter"),
            }
        }
    }

    fn parse_extended_color(&mut self, iter: &mut vte::ParamsIter<'_>, is_fg: bool) {
        let Some(sub) = iter.next() else { return };
        match sub[0] {
            5 => {
                if let Some(idx_slice) = iter.next() {
                    let color = ansi_256_color(idx_slice[0], &self.ansi_colors);
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
// TerminalOps impl for Terminal
// ---------------------------------------------------------------------------

impl TerminalOps for Terminal {
    fn cols(&self) -> usize { self.cols() }
    fn rows(&self) -> usize { self.rows() }
    fn cursor(&self) -> &Cursor { Terminal::cursor(self) }
    fn cell(&self, row: usize, col: usize) -> &Cell { self.cell(row, col) }
    fn feed(&mut self, data: &[u8]) { self.feed(data) }
    fn resize(&mut self, cols: usize, rows: usize) { self.resize(cols, rows) }
    fn reset(&mut self) { self.reset() }
    fn scroll_up(&mut self, lines: usize) { self.scroll_up(lines) }
    fn scroll_down(&mut self, lines: usize) { self.scroll_down(lines) }
    fn scroll_to_top(&mut self) { self.scroll_to_top() }
    fn scroll_to_bottom(&mut self) { self.scroll_to_bottom() }
    fn scroll_offset(&self) -> usize { self.scroll_offset() }
    fn seqno(&self) -> u64 { self.seqno() }
    fn take_response(&mut self) -> Option<Vec<u8>> { self.take_response() }
    fn title(&self) -> Option<&str> { self.title() }
    fn mouse_mode(&self) -> MouseMode { self.mouse_mode() }
    fn take_bell(&mut self) -> bool { self.take_bell() }
    fn kitty_keyboard_flags(&self) -> u32 { self.kitty_keyboard_flags() }
    fn cursor_keys_mode(&self) -> bool { self.cursor_keys_mode() }
    fn keypad_app_mode(&self) -> bool { self.keypad_app_mode() }
    fn bracketed_paste(&self) -> bool { self.bracketed_paste() }
    fn sgr_mouse(&self) -> bool { self.sgr_mouse() }
    fn focus_reporting(&self) -> bool { self.focus_reporting() }
}

// ---------------------------------------------------------------------------
// vte::Perform
// ---------------------------------------------------------------------------

impl vte::Perform for Terminal {
    fn print(&mut self, ch: char) {
        // Reset scroll offset when new content arrives
        self.scroll_offset = 0;

        // Apply character set translation (DEC Special Graphics)
        let ch = self.translate_charset(ch);

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
                // Bell
                self.bell_pending = true;
                tracing::trace!("BEL");
            }
            0x0E => {
                // SO — Shift Out: switch GL to G1
                self.gl_is_g1 = true;
            }
            0x0F => {
                // SI — Shift In: switch GL to G0
                self.gl_is_g1 = false;
            }
            _ => {
                tracing::trace!(byte, "unhandled execute byte");
            }
        }
    }

    fn hook(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        // DCS — Device Control String
        // DECRQSS: DCS $ q <setting> ST → respond with DCS 1 $ r <value> ST
        if intermediates == [b'$'] && action == 'q' {
            self.dcs_handler = Some(DcsHandler::Decrqss(Vec::new()));
        } else if intermediates.is_empty() && action == 'q' {
            // Sixel: DCS q or DCS Ps ; Ps q
            self.dcs_handler = Some(DcsHandler::Sixel);
            self.sixel_buffer = Some(Vec::new());
            let _ = params;
        } else {
            tracing::trace!(?intermediates, action = %action, "unhandled DCS hook");
            let _ = params;
        }
    }
    fn put(&mut self, byte: u8) {
        match self.dcs_handler {
            Some(DcsHandler::Decrqss(ref mut buf)) => buf.push(byte),
            Some(DcsHandler::Sixel) => {
                if let Some(ref mut buf) = self.sixel_buffer {
                    buf.push(byte);
                }
            }
            None => {}
        }
    }
    fn unhook(&mut self) {
        match self.dcs_handler {
            Some(DcsHandler::Decrqss(ref query)) => {
                let response = match query.as_slice() {
                    b"m" => b"\x1bP1$r0m\x1b\\".to_vec(),
                    b"r" => {
                        let top = self.scroll_top + 1;
                        let bottom = self.scroll_bottom + 1;
                        format!("\x1bP1$r{top};{bottom}r\x1b\\").into_bytes()
                    }
                    b"\"p" => b"\x1bP1$r62;1\"p\x1b\\".to_vec(),
                    b"\"q" => b"\x1bP1$r0\"q\x1b\\".to_vec(),
                    _ => b"\x1bP0$r\x1b\\".to_vec(),
                };
                self.response_bytes.extend_from_slice(&response);
            }
            Some(DcsHandler::Sixel) => {
                if let Some(data) = self.sixel_buffer.take() {
                    if !data.is_empty() {
                        self.sixel_images.push(SixelImage {
                            data,
                            row: self.cursor.row,
                            col: self.cursor.col,
                        });
                        self.seqno += 1;
                        tracing::debug!(
                            count = self.sixel_images.len(),
                            "sixel image stored (pending decode)"
                        );
                    }
                }
            }
            None => {}
        }
        self.dcs_handler = None;
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        // Dispatch table — each branch either delegates to a named
        // `handle_osc_N_*` method (the preferred shape, grep-friendly)
        // or a single-liner field reset. Adding a new OSC code = one
        // method plus one line here.
        match params[0] {
            b"0" | b"2" => self.handle_osc_0_2_title(params),
            b"4"       => self.handle_osc_4_palette(params),
            b"7"       => self.handle_osc_7_cwd(params),
            b"8"       => self.handle_osc_8_hyperlink(params),
            b"9"       => self.handle_osc_9_notification(params),
            b"10"      => self.handle_osc_10_foreground(params),
            b"11"      => self.handle_osc_11_background(params),
            b"12"      => self.handle_osc_12_cursor(params),
            b"52"      => self.handle_osc_52_clipboard(params),
            b"104"     => self.handle_osc_104_palette_reset(params),
            b"110"     => self.handle_osc_110_fg_reset(),
            b"111"     => self.handle_osc_111_bg_reset(),
            b"112"     => self.handle_osc_112_cursor_reset(),
            b"133"     => self.handle_osc_133_shell_integration(params),
            _          => tracing::trace!(?params, "unhandled OSC sequence"),
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

        // Handle DEC private modes (CSI ? Ps h/l) and Kitty query (CSI ? u)
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
                'u' => {
                    // Kitty keyboard protocol: query current flags
                    let flags = self.kitty_keyboard_flags();
                    let response = format!("\x1b[?{flags}u");
                    self.response_bytes.extend_from_slice(response.as_bytes());
                    return;
                }
                _ => {
                    tracing::trace!(action = %action, "unhandled CSI ? sequence");
                    return;
                }
            }
        }

        // Handle CSI > ... (secondary DA or Kitty keyboard push)
        if intermediates == [b'>'] {
            match action {
                'c' => {
                    // Secondary DA: report terminal type and version
                    // Format: CSI > Pp ; Pv ; Pc c
                    // 1 = VT220, 0 = firmware version, 0 = ROM version
                    self.response_bytes.extend_from_slice(b"\x1b[>1;0;0c");
                }
                'u' => {
                    // Kitty keyboard protocol: push flags onto stack
                    let flags = params.iter().next().map_or(0, |p| p[0] as u32);
                    self.kitty_keyboard_stack.push(flags);
                    tracing::debug!(flags, depth = self.kitty_keyboard_stack.len(), "kitty keyboard push");
                }
                _ => {
                    tracing::trace!(action = %action, "unhandled CSI > sequence");
                }
            }
            return;
        }

        // Handle CSI < ... (Kitty keyboard pop)
        if intermediates == [b'<'] {
            if action == 'u' {
                let count = params.iter().next().map_or(1, |p| (p[0] as usize).max(1));
                for _ in 0..count.min(self.kitty_keyboard_stack.len()) {
                    self.kitty_keyboard_stack.pop();
                }
                tracing::debug!(depth = self.kitty_keyboard_stack.len(), "kitty keyboard pop");
            } else {
                tracing::trace!(action = %action, "unhandled CSI < sequence");
            }
            return;
        }

        // CSI ! p — DECSTR (Soft Terminal Reset)
        if intermediates == [b'!'] && action == 'p' {
            self.soft_reset();
            tracing::debug!("soft terminal reset (DECSTR)");
            return;
        }

        // CSI $ p — DECRQM ANSI modes
        if intermediates == [b'$'] && action == 'p' {
            let mode = params.iter().next().map_or(0, |p| p[0]);
            // Pm: 1=set, 2=reset, 0=not recognized
            let state = match mode {
                4 => if self.insert_mode { 1 } else { 2 },  // IRM
                20 => 2,  // LNM — always reset
                _ => 0,
            };
            let response = format!("\x1b[{mode};{state}$y");
            self.response_bytes.extend_from_slice(response.as_bytes());
            return;
        }

        // CSI ? Ps $ p — DECRQM DEC private modes
        if intermediates == [b'?', b'$'] && action == 'p' {
            let mode = params.iter().next().map_or(0, |p| p[0]);
            // Pm: 1=set, 2=reset, 0=not recognized, 3=permanently set, 4=permanently reset
            let state = match mode {
                1 => if self.cursor_keys_mode { 1 } else { 2 },    // DECCKM
                6 => if self.origin_mode { 1 } else { 2 },         // DECOM
                7 => if self.auto_wrap { 1 } else { 2 },           // DECAWM
                12 => 2,                                            // Cursor blink (always off for now)
                25 => if self.cursor.visible { 1 } else { 2 },     // DECTCEM
                47 | 1047 | 1049 => if self.use_alternate { 1 } else { 2 }, // Alt screen
                1000 => if self.mouse_mode == MouseMode::Normal { 1 } else { 2 },
                1002 => if self.mouse_mode == MouseMode::ButtonEvent { 1 } else { 2 },
                1003 => if self.mouse_mode == MouseMode::AnyEvent { 1 } else { 2 },
                1004 => if self.focus_reporting { 1 } else { 2 },
                1006 => if self.sgr_mouse { 1 } else { 2 },
                2004 => if self.bracketed_paste { 1 } else { 2 },
                2026 => if self.synchronized_output { 1 } else { 2 },
                _ => 0,
            };
            let response = format!("\x1b[?{mode};{state}$y");
            self.response_bytes.extend_from_slice(response.as_bytes());
            return;
        }

        // CSI = c — Tertiary Device Attributes (DA3)
        if intermediates == [b'='] && action == 'c' {
            // Report unit ID: DCS ! | XXXXXXXX ST
            self.response_bytes.extend_from_slice(b"\x1bP!|6D61646F\x1b\\");
            return;
        }

        // DECSCUSR — Set cursor style (CSI Ps SP q)
        if intermediates == [b' '] && action == 'q' {
            let ps = params.iter().next().map_or(0, |p| p[0]);
            match ps {
                0 | 1 => { self.cursor_style = CursorStyle::Block; self.cursor_blink = true; }
                2 => { self.cursor_style = CursorStyle::Block; self.cursor_blink = false; }
                3 => { self.cursor_style = CursorStyle::Underline; self.cursor_blink = true; }
                4 => { self.cursor_style = CursorStyle::Underline; self.cursor_blink = false; }
                5 => { self.cursor_style = CursorStyle::Bar; self.cursor_blink = true; }
                6 => { self.cursor_style = CursorStyle::Bar; self.cursor_blink = false; }
                _ => {}
            }
            self.seqno += 1;
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
            // CBT — Cursor Backward Tabulation
            'Z' => {
                let n = first_param(1);
                for _ in 0..n {
                    if self.cursor.col == 0 {
                        break;
                    }
                    self.cursor.col -= 1;
                    while self.cursor.col > 0
                        && !self.tab_stops.get(self.cursor.col).copied().unwrap_or(false)
                    {
                        self.cursor.col -= 1;
                    }
                }
                self.wrap_pending = false;
                self.dirty();
            }
            // TBC — Tab Clear
            'g' => {
                let mode = params.iter().next().map_or(0, |p| p[0]);
                match mode {
                    0 => {
                        if self.cursor.col < self.tab_stops.len() {
                            self.tab_stops[self.cursor.col] = false;
                        }
                    }
                    3 => {
                        self.tab_stops.iter_mut().for_each(|t| *t = false);
                    }
                    _ => {}
                }
            }
            // ANSI mode set (CSI Ps h) — non-DEC modes (DEC private uses ? prefix above)
            'h' => {
                for p in params.iter() {
                    match p[0] {
                        4 => self.insert_mode = true,  // IRM — Insert Mode
                        _ => tracing::trace!(mode = p[0], "unhandled ANSI mode set"),
                    }
                }
            }
            // ANSI mode reset (CSI Ps l)
            'l' => {
                for p in params.iter() {
                    match p[0] {
                        4 => self.insert_mode = false,  // IRM — Replace Mode
                        _ => tracing::trace!(mode = p[0], "unhandled ANSI mode reset"),
                    }
                }
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
                self.reset();
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
            // DECALN — Screen Alignment Display (ESC # 8)
            ([b'#'], b'8') => self.fill_screen_with_e(),
            // Character Set Designation — G0 set
            ([b'('], b'0') => self.charset_g0_graphics = true,  // DEC Special Graphics
            ([b'('], b'B') => self.charset_g0_graphics = false, // US ASCII
            ([b'('], b'A') => self.charset_g0_graphics = false, // UK ASCII (treat as US)
            // Character Set Designation — G1 set
            ([b')'], b'0') => self.charset_g1_graphics = true,  // DEC Special Graphics
            ([b')'], b'B') => self.charset_g1_graphics = false, // US ASCII
            ([b')'], b'A') => self.charset_g1_graphics = false, // UK ASCII (treat as US)
            // DECKPAM — Keypad Application Mode
            ([], b'=') => self.keypad_app_mode = true,
            // DECKPNM — Keypad Numeric Mode
            ([], b'>') => self.keypad_app_mode = false,
            _ => {
                tracing::trace!(byte, ?intermediates, "unhandled ESC dispatch");
            }
        }
    }
}

/// Base64 decoder backed by `data-encoding`.
/// Delegates to `base64_decode_bytes` and converts the result to a UTF-8 string.
fn base64_decode(input: &[u8]) -> Option<String> {
    String::from_utf8(base64_decode_bytes(input)).ok()
}

/// Build the xterm `rgb:RR/GG/BB` response for an OSC query. Used by
/// OSC 10 / 11 / 12 (foreground / background / cursor color) and any
/// future palette-query OSC that follows the same shape. The duplicated
/// `RR/RR` / `GG/GG` / `BB/BB` pattern matches xterm: each channel is
/// emitted twice so older parsers that expect 16-bit precision see
/// `RRRR/GGGG/BBBB` fall-through as two-byte values.
fn osc_rgb_query_response(osc_id: u16, c: Color) -> String {
    format!(
        "\x1b]{osc_id};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\",
        r = c.r, g = c.g, b = c.b
    )
}

/// Parse an OSC color payload into a [`Color`]. Accepts both common
/// xterm/VTE formats:
///
///   - `#rrggbb`            — HTML-style hex triplet.
///   - `rgb:RR/GG/BB`       — xterm short form.
///   - `rgb:RRRR/GGGG/BBBB` — xterm full form; we take the high byte.
///
/// Returns `None` on anything else (named colors, rgba:, cmyk:, …);
/// the OSC handler treats that as a no-op so a malformed payload
/// never corrupts the palette.
fn parse_osc_color(payload: &[u8]) -> Option<Color> {
    let s = std::str::from_utf8(payload).ok()?;
    // Hex triplet: `#rrggbb`.
    if let Some(hex) = s.strip_prefix('#')
        && hex.len() == 6
    {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::new(r, g, b));
    }
    // xterm `rgb:RR/GG/BB` and `rgb:RRRR/GGGG/BBBB`.
    if let Some(rest) = s.strip_prefix("rgb:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        let channel = |p: &str| -> Option<u8> {
            match p.len() {
                2 => u8::from_str_radix(p, 16).ok(),
                // 4-digit form: take the high byte (xterm docs say
                // the two values are equivalent precision-wise).
                4 => u8::from_str_radix(&p[0..2], 16).ok(),
                _ => None,
            }
        };
        return Some(Color::new(
            channel(parts[0])?,
            channel(parts[1])?,
            channel(parts[2])?,
        ));
    }
    None
}

/// Parse a palette index byte slice (`b"3"` → `Some(3)`). `None`
/// when the payload isn't a decimal integer.
fn parse_palette_index(payload: &[u8]) -> Option<usize> {
    std::str::from_utf8(payload).ok()?.parse().ok()
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
    fn sgr_dim() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[2mX");
        let cell = term.cell(0, 0);
        assert!(cell.attrs.contains(CellAttrs::DIM));
        // SGR 22 resets both bold and dim
        term.feed(b"\x1b[22mY");
        let cell = term.cell(0, 1);
        assert!(!cell.attrs.contains(CellAttrs::DIM));
    }

    #[test]
    fn sgr_hidden() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[8mX");
        let cell = term.cell(0, 0);
        assert!(cell.attrs.contains(CellAttrs::HIDDEN));
        // SGR 28 resets hidden
        term.feed(b"\x1b[28mY");
        let cell = term.cell(0, 1);
        assert!(!cell.attrs.contains(CellAttrs::HIDDEN));
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
        assert_eq!(term.cell(0, 0).fg, ansi_256_color(196, &default_ansi_palette()));
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
    fn bold_bright_substitution() {
        let palette = default_ansi_palette();
        // Standard ANSI red → bright red
        let red = ANSI_COLORS[1];
        let bright_red = bold_bright_color(&red, &palette);
        assert_eq!(bright_red, ANSI_BRIGHT_COLORS[1]);

        // Custom color (not in ANSI palette) → unchanged
        let custom = Color::new(42, 42, 42);
        assert_eq!(bold_bright_color(&custom, &palette), custom);
    }

    #[test]
    fn cursor_backward_tabulation() {
        let mut term = Terminal::new(80, 24);
        // Move to column 20 (past tab stops at 0, 8, 16)
        term.feed(b"\x1b[1;21H"); // col 20 (0-indexed)
        assert_eq!(term.cursor().col, 20);

        // CBT — move back 1 tab stop
        term.feed(b"\x1b[Z");
        assert_eq!(term.cursor().col, 16);

        // CBT — move back 2 tab stops
        term.feed(b"\x1b[2Z");
        assert_eq!(term.cursor().col, 0);
    }

    #[test]
    fn tab_clear() {
        let mut term = Terminal::new(80, 24);
        // Move to column 8 (tab stop)
        term.feed(b"\x1b[1;9H");
        assert_eq!(term.cursor().col, 8);

        // Clear tab stop at current position
        term.feed(b"\x1b[g");

        // Tab from column 0 should skip column 8
        term.feed(b"\x1b[1;1H");
        term.feed(b"\t");
        // With tab stop at 8 cleared, next stop is 16
        assert_ne!(term.cursor().col, 8);
    }

    #[test]
    fn tab_clear_all() {
        let mut term = Terminal::new(80, 24);
        // Clear all tab stops
        term.feed(b"\x1b[3g");

        // Tab should go to end of line
        term.feed(b"\t");
        assert_eq!(term.cursor().col, 79);
    }

    #[test]
    fn osc_7_cwd() {
        let mut term = Terminal::new(80, 24);
        assert!(term.cwd().is_none());
        term.feed(b"\x1b]7;file://localhost/home/user/code\x1b\\");
        assert_eq!(term.cwd(), Some("/home/user/code"));
    }

    #[test]
    fn with_scrollback_custom() {
        let term = Terminal::with_scrollback(80, 24, 500);
        assert_eq!(term.cols(), 80);
        assert_eq!(term.rows(), 24);
        // Fill beyond visible to test scrollback limit
        // (500 is the max, not tested here for brevity)
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

    #[test]
    fn bell_pending() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.take_bell());
        // Send BEL character
        term.feed(b"\x07");
        assert!(term.take_bell());
        // Should be cleared after take
        assert!(!term.take_bell());
    }

    #[test]
    fn osc_8_hyperlink() {
        let mut term = Terminal::new(80, 24);
        // Start hyperlink
        term.feed(b"\x1b]8;;https://example.com\x1b\\");
        term.feed(b"link");
        // End hyperlink
        term.feed(b"\x1b]8;;\x1b\\");
        term.feed(b" text");

        // Cells within the hyperlink should have the URL
        assert_eq!(
            term.cell(0, 0).hyperlink.as_deref().map(String::as_str),
            Some("https://example.com")
        );
        assert_eq!(
            term.cell(0, 3).hyperlink.as_deref().map(String::as_str),
            Some("https://example.com")
        );
        // Cell after the hyperlink should not
        assert!(term.cell(0, 5).hyperlink.is_none());
    }

    #[test]
    fn osc_52_clipboard_set() {
        let mut term = Terminal::new(80, 24);
        // "hello" base64-encoded = "aGVsbG8="
        term.feed(b"\x1b]52;c;aGVsbG8=\x1b\\");
        let content = term.take_clipboard();
        assert_eq!(content, Some("hello".to_string()));
        // Second call returns None
        assert!(term.take_clipboard().is_none());
    }

    #[test]
    fn osc_133_prompt_marker() {
        let mut term = Terminal::new(80, 24);
        assert!(term.prompt_start_row().is_none());
        // Send prompt start marker
        term.feed(b"\x1b]133;A\x1b\\");
        assert_eq!(term.prompt_start_row(), Some(0));
    }

    #[test]
    fn osc_10_query_fg_color() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]10;?\x1b\\");
        let response = term.take_response().unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.starts_with("\x1b]10;rgb:"));
    }

    #[test]
    fn osc_11_query_bg_color() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]11;?\x1b\\");
        let response = term.take_response().unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.starts_with("\x1b]11;rgb:"));
    }

    #[test]
    fn base64_decode_basic() {
        assert_eq!(base64_decode(b"aGVsbG8="), Some("hello".to_string()));
        assert_eq!(base64_decode(b"d29ybGQ="), Some("world".to_string()));
        assert_eq!(base64_decode(b""), Some(String::new()));
    }

    #[test]
    fn reset_preserves_scrollback() {
        let mut term = Terminal::with_scrollback(80, 24, 500);
        term.feed(b"Hello");
        assert_eq!(term.cell(0, 0).ch, 'H');
        term.reset();
        assert_eq!(term.cell(0, 0).ch, ' ');
        assert_eq!(term.cursor().row, 0);
        assert_eq!(term.cursor().col, 0);
        // Scrollback setting is preserved
        assert_eq!(term.primary.max_scrollback, 500);
    }

    #[test]
    fn scroll_to_top_and_bottom() {
        let mut term = Terminal::new(10, 3);
        for i in 0..10 {
            let line = format!("LINE{i}\r\n");
            term.feed(line.as_bytes());
        }
        let sb_len = term.primary.scrollback_len();
        assert!(sb_len > 0);

        term.scroll_to_top();
        assert_eq!(term.scroll_offset(), sb_len);

        term.scroll_to_bottom();
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn kitty_keyboard_push_pop() {
        let mut term = Terminal::new(80, 24);
        assert_eq!(term.kitty_keyboard_flags(), 0);

        // Push flags=1 (disambiguate)
        term.feed(b"\x1b[>1u");
        assert_eq!(term.kitty_keyboard_flags(), 1);

        // Push flags=3 (disambiguate + report event types)
        term.feed(b"\x1b[>3u");
        assert_eq!(term.kitty_keyboard_flags(), 3);

        // Pop one
        term.feed(b"\x1b[<u");
        assert_eq!(term.kitty_keyboard_flags(), 1);

        // Pop remaining
        term.feed(b"\x1b[<u");
        assert_eq!(term.kitty_keyboard_flags(), 0);
    }

    #[test]
    fn kitty_keyboard_query() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[>5u"); // push flags=5
        term.feed(b"\x1b[?u");  // query
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[?5u");
    }

    #[test]
    fn kitty_keyboard_pop_multiple() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[>1u");
        term.feed(b"\x1b[>2u");
        term.feed(b"\x1b[>3u");
        assert_eq!(term.kitty_keyboard_flags(), 3);

        // Pop 2
        term.feed(b"\x1b[<2u");
        assert_eq!(term.kitty_keyboard_flags(), 1);
    }

    #[test]
    fn kitty_graphics_direct_rgba() {
        let mut term = Terminal::new(80, 24);
        // 2x2 red RGBA image, direct transmission + display
        // 4 pixels * 4 bytes = 16 bytes of RGBA data
        let rgba = [
            255, 0, 0, 255, 255, 0, 0, 255,
            255, 0, 0, 255, 255, 0, 0, 255,
        ];
        // Base64 encode the RGBA data
        let b64 = base64_encode(&rgba);
        let apc = format!("\x1b_Ga=T,f=32,s=2,v=2,i=1;{b64}\x1b\\");
        term.feed(apc.as_bytes());

        assert!(term.images().contains_key(&1));
        let img = &term.images()[&1];
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.data.len(), 16);
        assert_eq!(term.image_placements().len(), 1);
        assert_eq!(term.image_placements()[0].image_id, 1);
    }

    #[test]
    fn kitty_graphics_multi_chunk() {
        let mut term = Terminal::new(80, 24);
        // Send a 1x1 RGBA image in two chunks
        let rgba = [0, 255, 0, 255]; // green pixel
        let b64 = base64_encode(&rgba);
        let (first_half, second_half) = b64.split_at(b64.len() / 2);

        // First chunk: m=1 (more coming)
        let apc1 = format!("\x1b_Ga=T,f=32,s=1,v=1,i=42,m=1;{first_half}\x1b\\");
        term.feed(apc1.as_bytes());
        assert!(!term.images().contains_key(&42)); // Not yet complete

        // Second chunk: m=0 (last)
        let apc2 = format!("\x1b_Gm=0;{second_half}\x1b\\");
        term.feed(apc2.as_bytes());
        assert!(term.images().contains_key(&42));
        assert_eq!(term.images()[&42].data, rgba);
    }

    #[test]
    fn kitty_graphics_delete() {
        let mut term = Terminal::new(80, 24);
        let rgba = [255, 255, 255, 255];
        let b64 = base64_encode(&rgba);

        // Create two images
        let apc1 = format!("\x1b_Ga=T,f=32,s=1,v=1,i=1;{b64}\x1b\\");
        let apc2 = format!("\x1b_Ga=T,f=32,s=1,v=1,i=2;{b64}\x1b\\");
        term.feed(apc1.as_bytes());
        term.feed(apc2.as_bytes());
        assert_eq!(term.images().len(), 2);
        assert_eq!(term.image_placements().len(), 2);

        // Delete image 1
        term.feed(b"\x1b_Ga=d,d=i,i=1;\x1b\\");
        assert_eq!(term.images().len(), 1);
        assert!(!term.images().contains_key(&1));
        assert_eq!(term.image_placements().len(), 1);

        // Delete all
        term.feed(b"\x1b_Ga=d,d=a;\x1b\\");
        assert!(term.images().is_empty());
        assert!(term.image_placements().is_empty());
    }

    #[test]
    fn kitty_graphics_query() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b_Ga=q,i=99;\x1b\\");
        let response = term.take_response().unwrap();
        assert_eq!(std::str::from_utf8(&response).unwrap(), "\x1b_Gi=99;OK\x1b\\");
    }

    #[test]
    fn apc_does_not_interfere_with_normal_text() {
        let mut term = Terminal::new(80, 24);
        // Normal text before and after APC
        term.feed(b"AB\x1b_Ga=q,i=1;\x1b\\CD");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, 'B');
        assert_eq!(term.cell(0, 2).ch, 'C');
        assert_eq!(term.cell(0, 3).ch, 'D');
    }

    #[test]
    fn test_ansi_256_greyscale() {
        let p = default_ansi_palette();
        let c232 = ansi_256_color(232, &p);
        assert_eq!(c232, Color::new(8, 8, 8));

        let c243 = ansi_256_color(243, &p);
        let v = (8 + 10 * (243u16 - 232)) as u8;
        assert_eq!(c243, Color::new(v, v, v));

        let c255 = ansi_256_color(255, &p);
        let v = (8 + 10 * (255u16 - 232)) as u8;
        assert_eq!(c255, Color::new(v, v, v));
    }

    #[test]
    fn test_ansi_256_rgb_cube() {
        let p = default_ansi_palette();
        assert_eq!(ansi_256_color(16, &p), Color::new(0, 0, 0));
        assert_eq!(ansi_256_color(196, &p), Color::new(255, 0, 0));
        assert_eq!(ansi_256_color(21, &p), Color::new(0, 0, 255));
    }

    #[test]
    fn test_ansi_256_standard() {
        let p = default_ansi_palette();
        for idx in 0..8u16 {
            assert_eq!(ansi_256_color(idx, &p), ANSI_COLORS[idx as usize]);
        }
    }

    #[test]
    fn test_ansi_256_bright() {
        let p = default_ansi_palette();
        for idx in 8..16u16 {
            assert_eq!(ansi_256_color(idx, &p), ANSI_BRIGHT_COLORS[(idx - 8) as usize]);
        }
    }

    #[test]
    fn test_ansi_256_out_of_range() {
        let p = default_ansi_palette();
        assert_eq!(ansi_256_color(256, &p), Color::WHITE);
        assert_eq!(ansi_256_color(999, &p), Color::WHITE);
    }

    #[test]
    fn test_cell_push_combining() {
        let mut cell = Cell::default();
        assert!(cell.extra.is_none());

        cell.push_combining('\u{0301}');
        assert!(cell.extra.is_some());
        assert_eq!(cell.extra.as_ref().unwrap().len(), 1);

        cell.push_combining('\u{0327}');
        assert_eq!(cell.extra.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_cell_write_to() {
        let mut cell = Cell::default();
        cell.ch = 'e';
        cell.push_combining('\u{0301}');

        let mut buf = String::new();
        cell.write_to(&mut buf);
        assert_eq!(buf, "e\u{0301}");
    }

    #[test]
    fn test_cell_default() {
        let cell = Cell::default();
        assert_eq!(cell.ch, ' ');
        assert!(cell.extra.is_none());
        assert_eq!(cell.width, 1);
        assert_eq!(cell.fg, Color::WHITE);
        assert_eq!(cell.bg, Color::BLACK);
        assert_eq!(cell.attrs, CellAttrs::NONE);
        assert!(cell.hyperlink.is_none());
    }

    #[test]
    fn test_cursor_default() {
        let cursor = Cursor::default();
        assert_eq!(cursor.row, 0);
        assert_eq!(cursor.col, 0);
        assert!(cursor.visible);
    }

    #[test]
    fn test_mouse_mode_default() {
        assert_eq!(MouseMode::default(), MouseMode::Off);
    }

    /// Simple base64 encoder for tests.
    fn base64_encode(data: &[u8]) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).map_or(0, |&b| b as u32);
            let b2 = chunk.get(2).map_or(0, |&b| b as u32);
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(CHARS[((n >> 18) & 63) as usize] as char);
            out.push(CHARS[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(CHARS[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(CHARS[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    // ── MockTerminal ───────────────────────────────────────────────────

    pub struct MockTerminal {
        pub cols: usize,
        pub rows: usize,
        pub cur: Cursor,
        cells: Vec<Vec<Cell>>,
        pub title_str: Option<String>,
        pub mouse: MouseMode,
        pub bell_flag: bool,
        seqno_val: u64,
        #[allow(dead_code)]
        pub response: Option<Vec<u8>>,
    }

    impl MockTerminal {
        pub fn new(cols: usize, rows: usize) -> Self {
            let cells = vec![vec![Cell::default(); cols]; rows];
            Self {
                cols,
                rows,
                cur: Cursor::default(),
                cells,
                title_str: None,
                mouse: MouseMode::Off,
                bell_flag: false,
                seqno_val: 0,
                response: None,
            }
        }

        pub fn set_cell(&mut self, row: usize, col: usize, ch: char) {
            if row < self.rows && col < self.cols {
                self.cells[row][col].ch = ch;
                self.seqno_val += 1;
            }
        }
    }

    impl TerminalOps for MockTerminal {
        fn cols(&self) -> usize { self.cols }
        fn rows(&self) -> usize { self.rows }
        fn cursor(&self) -> &Cursor { &self.cur }
        fn cell(&self, row: usize, col: usize) -> &Cell { &self.cells[row][col] }
        fn feed(&mut self, _data: &[u8]) { self.seqno_val += 1; }
        fn resize(&mut self, cols: usize, rows: usize) {
            self.cols = cols;
            self.rows = rows;
            self.cells = vec![vec![Cell::default(); cols]; rows];
            self.seqno_val += 1;
        }
        fn reset(&mut self) {
            self.cells = vec![vec![Cell::default(); self.cols]; self.rows];
            self.cur = Cursor::default();
            self.seqno_val += 1;
        }
        fn scroll_up(&mut self, _lines: usize) { self.seqno_val += 1; }
        fn scroll_down(&mut self, _lines: usize) { self.seqno_val += 1; }
        fn scroll_to_top(&mut self) {}
        fn scroll_to_bottom(&mut self) {}
        fn scroll_offset(&self) -> usize { 0 }
        fn seqno(&self) -> u64 { self.seqno_val }
        fn take_response(&mut self) -> Option<Vec<u8>> { self.response.take() }
        fn title(&self) -> Option<&str> { self.title_str.as_deref() }
        fn mouse_mode(&self) -> MouseMode { self.mouse }
        fn take_bell(&mut self) -> bool { std::mem::replace(&mut self.bell_flag, false) }
        fn kitty_keyboard_flags(&self) -> u32 { 0 }
        fn cursor_keys_mode(&self) -> bool { false }
        fn keypad_app_mode(&self) -> bool { false }
        fn bracketed_paste(&self) -> bool { false }
        fn sgr_mouse(&self) -> bool { false }
        fn focus_reporting(&self) -> bool { false }
    }

    #[test]
    fn test_mock_terminal_new() {
        let mut mock = MockTerminal::new(80, 24);
        assert_eq!(mock.cols(), 80);
        assert_eq!(mock.rows(), 24);
        assert_eq!(mock.cursor().row, 0);
        assert_eq!(mock.cursor().col, 0);
        assert!(mock.cursor().visible);
        assert_eq!(mock.cell(0, 0).ch, ' ');
        assert_eq!(mock.seqno(), 0);
        assert_eq!(mock.title(), None);
        assert_eq!(mock.mouse_mode(), MouseMode::Off);
        assert!(!mock.take_bell());
    }

    #[test]
    fn test_mock_terminal_set_cell() {
        let mut mock = MockTerminal::new(80, 24);
        mock.set_cell(0, 0, 'A');
        assert_eq!(mock.cell(0, 0).ch, 'A');
        assert_eq!(mock.seqno(), 1);

        mock.set_cell(5, 10, 'Z');
        assert_eq!(mock.cell(5, 10).ch, 'Z');
        assert_eq!(mock.seqno(), 2);

        // Out-of-bounds write is a no-op
        mock.set_cell(100, 0, 'X');
        assert_eq!(mock.seqno(), 2);
    }

    #[test]
    fn test_mock_terminal_resize() {
        let mut mock = MockTerminal::new(80, 24);
        mock.set_cell(0, 0, 'A');
        mock.resize(40, 12);
        assert_eq!(mock.cols(), 40);
        assert_eq!(mock.rows(), 12);
        assert_eq!(mock.cell(0, 0).ch, ' ');
    }

    #[test]
    fn test_mock_terminal_reset() {
        let mut mock = MockTerminal::new(80, 24);
        mock.set_cell(0, 0, 'A');
        mock.cur.row = 5;
        mock.cur.col = 10;
        mock.reset();
        assert_eq!(mock.cell(0, 0).ch, ' ');
        assert_eq!(mock.cursor().row, 0);
        assert_eq!(mock.cursor().col, 0);
    }

    #[test]
    fn test_mock_terminal_ops_trait() {
        let mock: Box<dyn TerminalOps> = Box::new(MockTerminal::new(80, 24));
        assert_eq!(mock.cols(), 80);
        assert_eq!(mock.rows(), 24);
        assert_eq!(mock.cell(0, 0).ch, ' ');
        assert_eq!(mock.cursor().row, 0);
        assert_eq!(mock.seqno(), 0);
    }

    #[test]
    fn test_apply_theme() {
        let mut term = Terminal::new(80, 24);
        let fg = Color::new(200, 200, 200);
        let bg = Color::new(30, 30, 30);
        let mut palette = default_ansi_palette();
        palette[0] = Color::new(10, 10, 10);
        term.apply_theme(fg, bg, palette);
        assert_eq!(term.default_fg, fg);
        assert_eq!(term.default_bg, bg);
        assert_eq!(term.pen_fg, fg);
        assert_eq!(term.pen_bg, bg);
        assert_eq!(term.ansi_colors[0], Color::new(10, 10, 10));
    }

    #[test]
    fn test_terminal_title_set_via_osc2() {
        let mut term = Terminal::new(80, 24);
        assert_eq!(term.title(), None);
        term.feed(b"\x1b]2;custom title\x1b\\");
        assert_eq!(term.title(), Some("custom title"));
    }

    #[test]
    fn test_terminal_bell_via_bel() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.take_bell());
        term.feed(b"\x07");
        assert!(term.take_bell());
        assert!(!term.take_bell());
    }

    #[test]
    fn test_terminal_cursor_movement_cup() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[10;20H");
        assert_eq!(term.cursor().row, 9);
        assert_eq!(term.cursor().col, 19);
    }

    #[test]
    fn test_terminal_erase_display_full() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"AAAAAAAAAA");
        term.feed(b"BBBBBBBBBB");
        term.feed(b"CCCCCCCCCC");
        // ED 2 = erase entire display
        term.feed(b"\x1b[2J");
        for row in 0..3 {
            for col in 0..10 {
                assert_eq!(term.cell(row, col).ch, ' ');
            }
        }
    }

    #[test]
    fn test_terminal_insert_characters() {
        let mut term = Terminal::new(10, 1);
        term.feed(b"ABCDE");
        // Move cursor to col 1
        term.feed(b"\x1b[1;2H");
        // ICH 2: insert 2 blanks at cursor, shifting right
        term.feed(b"\x1b[2@");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, ' ');
        assert_eq!(term.cell(0, 2).ch, ' ');
        assert_eq!(term.cell(0, 3).ch, 'B');
        assert_eq!(term.cell(0, 4).ch, 'C');
    }

    #[test]
    fn test_terminal_delete_characters() {
        let mut term = Terminal::new(10, 1);
        term.feed(b"ABCDE");
        // Move cursor to col 1
        term.feed(b"\x1b[1;2H");
        // DCH 2: delete 2 chars at cursor, shifting left
        term.feed(b"\x1b[2P");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, 'D');
        assert_eq!(term.cell(0, 2).ch, 'E');
    }

    #[test]
    fn test_terminal_scroll_region_behavior() {
        let mut term = Terminal::new(10, 5);
        for i in 0..5 {
            let line = format!("LINE{i}");
            term.feed(line.as_bytes());
            if i < 4 { term.feed(b"\r\n"); }
        }
        // Set scroll region to rows 2-4 (1-based)
        term.feed(b"\x1b[2;4r");
        assert_eq!(term.scroll_top, 1);
        assert_eq!(term.scroll_bottom, 3);
        // Move to bottom of scroll region and scroll
        term.feed(b"\x1b[4;1H");
        term.feed(b"\n");
        // Row 1 (0-indexed) should have scrolled up within region
        // The first row (outside region) should be unchanged
        assert_eq!(term.cell(0, 0).ch, 'L');
    }

    #[test]
    fn test_terminal_alternate_screen_round_trip() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"HELLO");
        assert_eq!(term.cell(0, 0).ch, 'H');
        assert!(!term.use_alternate);

        // Enter alternate screen
        term.feed(b"\x1b[?1049h");
        assert!(term.use_alternate);
        assert_eq!(term.cell(0, 0).ch, ' ');

        // Write on alt screen
        term.feed(b"ALT");
        assert_eq!(term.cell(0, 0).ch, 'A');

        // Exit alternate screen: primary content restored
        term.feed(b"\x1b[?1049l");
        assert!(!term.use_alternate);
        assert_eq!(term.cell(0, 0).ch, 'H');
    }

    #[test]
    fn test_terminal_bracketed_paste_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.bracketed_paste());
        term.feed(b"\x1b[?2004h");
        assert!(term.bracketed_paste());
        term.feed(b"\x1b[?2004l");
        assert!(!term.bracketed_paste());
    }

    #[test]
    fn test_terminal_focus_reporting_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.focus_reporting());
        term.feed(b"\x1b[?1004h");
        assert!(term.focus_reporting());
        term.feed(b"\x1b[?1004l");
        assert!(!term.focus_reporting());
    }

    #[test]
    fn test_terminal_cursor_keys_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.cursor_keys_mode());
        term.feed(b"\x1b[?1h");
        assert!(term.cursor_keys_mode());
        term.feed(b"\x1b[?1l");
        assert!(!term.cursor_keys_mode());
    }

    #[test]
    fn test_terminal_sgr_mouse_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.sgr_mouse());
        term.feed(b"\x1b[?1006h");
        assert!(term.sgr_mouse());
        term.feed(b"\x1b[?1006l");
        assert!(!term.sgr_mouse());
    }

    #[test]
    fn test_terminal_reset_clears_modes() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[?2004h"); // bracketed paste
        term.feed(b"\x1b[?1004h"); // focus reporting
        term.feed(b"\x1b[?1h");    // cursor keys mode
        term.feed(b"\x1b[?1006h"); // SGR mouse
        assert!(term.bracketed_paste());
        assert!(term.focus_reporting());
        assert!(term.cursor_keys_mode());
        assert!(term.sgr_mouse());
        term.reset();
        assert!(!term.bracketed_paste());
        assert!(!term.focus_reporting());
        assert!(!term.cursor_keys_mode());
        assert!(!term.sgr_mouse());
    }

    #[test]
    fn test_terminal_feed_utf8() {
        let mut term = Terminal::new(80, 24);
        term.feed("日本語".as_bytes());
        assert_eq!(term.cell(0, 0).ch, '日');
        assert_eq!(term.cell(0, 0).width, 2);
        assert_eq!(term.cell(0, 2).ch, '本');
        assert_eq!(term.cell(0, 4).ch, '語');
    }

    #[test]
    fn test_terminal_line_wrap_cursor() {
        let mut term = Terminal::new(5, 3);
        term.feed(b"ABCDE");
        // Cursor is at col 4 (last col), wrap_pending
        term.feed(b"F");
        assert_eq!(term.cursor().row, 1);
        assert_eq!(term.cell(1, 0).ch, 'F');
    }

    #[test]
    fn test_terminal_osc_52_clipboard() {
        let mut term = Terminal::new(80, 24);
        // "test" base64 = "dGVzdA=="
        term.feed(b"\x1b]52;c;dGVzdA==\x1b\\");
        let content = term.take_clipboard();
        assert_eq!(content, Some("test".to_string()));
    }

    #[test]
    fn test_terminal_dsr_response() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[10;5H");
        term.feed(b"\x1b[6n");
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[10;5R");
    }

    #[test]
    fn test_terminal_da_response() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[c");
        let response = term.take_response().unwrap();
        assert_eq!(response, b"\x1b[?62;22c");
    }

    #[test]
    fn test_color_from_ansi() {
        assert_eq!(Color::WHITE, Color { r: 255, g: 255, b: 255 });
        assert_eq!(Color::BLACK, Color { r: 0, g: 0, b: 0 });
        let c = Color::new(128, 64, 32);
        assert_eq!(c.r, 128);
        assert_eq!(c.g, 64);
        assert_eq!(c.b, 32);
    }

    #[test]
    fn test_terminal_seqno_increments() {
        let mut term = Terminal::new(80, 24);
        let s0 = term.seqno();
        term.feed(b"A");
        let s1 = term.seqno();
        assert!(s1 > s0);
        term.feed(b"B");
        let s2 = term.seqno();
        assert!(s2 > s1);
    }

    #[test]
    fn test_soft_reset_preserves_content() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"Hello");
        term.feed(b"\x1b[?2004h"); // bracketed paste on
        term.soft_reset();
        assert!(!term.bracketed_paste());
        assert_eq!(term.cell(0, 0).ch, 'H');
    }

    #[test]
    fn test_erase_display_above_cursor() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"AAAAAAAAAA");
        term.feed(b"BBBBBBBBBB");
        term.feed(b"CCCCCCCCCC");
        // Move to row 2, col 5 and erase above
        term.feed(b"\x1b[2;6H\x1b[1J");
        assert_eq!(term.cell(0, 0).ch, ' ');
        assert_eq!(term.cell(1, 4).ch, ' ');
        assert_eq!(term.cell(1, 5).ch, ' ');
    }

    #[test]
    fn test_delete_lines() {
        let mut term = Terminal::new(5, 4);
        term.feed(b"AAAAA");
        term.feed(b"BBBBB");
        term.feed(b"CCCCC");
        term.feed(b"DDDDD");
        // Move to row 2, delete 1 line
        term.feed(b"\x1b[2;1H\x1b[1M");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(1, 0).ch, 'C');
        assert_eq!(term.cell(2, 0).ch, 'D');
    }

    #[test]
    fn test_cursor_forward_backward() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[10C");
        assert_eq!(term.cursor().col, 10);
        term.feed(b"\x1b[3D");
        assert_eq!(term.cursor().col, 7);
    }

    #[test]
    fn test_cursor_up_down() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[10;1H");
        assert_eq!(term.cursor().row, 9);
        term.feed(b"\x1b[3A");
        assert_eq!(term.cursor().row, 6);
        term.feed(b"\x1b[2B");
        assert_eq!(term.cursor().row, 8);
    }

    #[test]
    fn test_decaln_fill_screen_with_e() {
        let mut term = Terminal::new(5, 3);
        // DECALN: ESC # 8
        term.feed(b"\x1b#8");
        for row in 0..3 {
            for col in 0..5 {
                assert_eq!(term.cell(row, col).ch, 'E');
            }
        }
    }

    #[test]
    fn test_keypad_app_mode() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.keypad_app_mode());
        // DECKPAM: ESC =
        term.feed(b"\x1b=");
        assert!(term.keypad_app_mode());
        // DECKPNM: ESC >
        term.feed(b"\x1b>");
        assert!(!term.keypad_app_mode());
    }

    #[test]
    fn test_erase_characters() {
        let mut term = Terminal::new(10, 1);
        term.feed(b"ABCDEFGHIJ");
        // Move to col 2 and erase 3 characters
        term.feed(b"\x1b[1;3H\x1b[3X");
        assert_eq!(term.cell(0, 0).ch, 'A');
        assert_eq!(term.cell(0, 1).ch, 'B');
        assert_eq!(term.cell(0, 2).ch, ' ');
        assert_eq!(term.cell(0, 3).ch, ' ');
        assert_eq!(term.cell(0, 4).ch, ' ');
        assert_eq!(term.cell(0, 5).ch, 'F');
    }

    #[test]
    fn test_resize_zero_is_noop() {
        let mut term = Terminal::new(80, 24);
        term.resize(0, 0);
        assert_eq!(term.cols(), 80);
        assert_eq!(term.rows(), 24);
    }

    #[test]
    fn test_scroll_region_down() {
        let mut term = Terminal::new(10, 5);
        for i in 0..5 {
            let line = format!("LINE{i}");
            term.feed(line.as_bytes());
            if i < 4 { term.feed(b"\r\n"); }
        }
        // Set scroll region rows 2-4 (1-based)
        term.feed(b"\x1b[2;4r");
        // Move cursor to top of scroll region and do reverse index
        term.feed(b"\x1b[2;1H");
        term.feed(b"\x1bM");
        // First row outside region should be unchanged
        assert_eq!(term.cell(0, 0).ch, 'L');
    }

    #[test]
    fn test_osc_7_current_directory() {
        let mut term = Terminal::new(80, 24);
        assert!(term.cwd().is_none());
        term.feed(b"\x1b]7;file:///path/to/file\x1b\\");
        assert_eq!(term.cwd(), Some("/path/to/file"));
    }

    #[test]
    fn test_scrollback_offset_zero_initially() {
        let term = Terminal::new(80, 24);
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn test_scroll_up_then_down() {
        let mut term = Terminal::new(10, 3);
        for i in 0..15 {
            let line = format!("LINE{i}\r\n");
            term.feed(line.as_bytes());
        }
        let sb_len = term.primary.scrollback_len();
        assert!(sb_len >= 5);

        term.scroll_up(5);
        assert_eq!(term.scroll_offset(), 5);

        term.scroll_down(3);
        assert_eq!(term.scroll_offset(), 2);
    }

    #[test]
    fn test_terminal_large_resize() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"hello");
        term.resize(400, 200);
        assert_eq!(term.cols(), 400);
        assert_eq!(term.rows(), 200);
        assert_eq!(term.cell(0, 0).ch, 'h');

        term.resize(10, 5);
        assert_eq!(term.cols(), 10);
        assert_eq!(term.rows(), 5);
    }

    #[test]
    fn test_feed_empty_data() {
        let mut term = Terminal::new(80, 24);
        term.feed(&[]);
        term.feed(b"");
        assert_eq!(term.cursor().row, 0);
        assert_eq!(term.cursor().col, 0);
    }

    #[test]
    fn test_feed_partial_escape() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b");
        term.feed(b"[");
        term.feed(b"2");
        assert_eq!(term.cursor().row, 0);
        term.feed(b"J");
        assert_eq!(term.cell(0, 0).ch, ' ');
    }

    #[test]
    fn test_cursor_save_restore() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"abc");
        assert_eq!(term.cursor().col, 3);
        term.feed(b"\x1b7");
        term.feed(b"\x1b[5C");
        assert_eq!(term.cursor().col, 8);
        term.feed(b"\x1b8");
        assert_eq!(term.cursor().col, 3);
        assert_eq!(term.cursor().row, 0);
    }

    #[test]
    fn test_reverse_index() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"\x1b[1;1H");
        assert_eq!(term.cursor().row, 0);
        term.feed(b"X");
        term.feed(b"\x1bM");
        assert_eq!(term.cursor().row, 0);
        assert_eq!(term.cell(0, 0).ch, ' ');
        assert_eq!(term.cell(1, 0).ch, 'X');
    }

    #[test]
    fn test_newline() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"A");
        assert_eq!(term.cursor().row, 0);
        term.feed(b"\n");
        assert_eq!(term.cursor().row, 1);
        assert_eq!(term.cell(0, 0).ch, 'A');
    }

    #[test]
    fn test_osc_7_file_url_variant() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]7;file://localhost/opt/project\x1b\\");
        assert_eq!(term.cwd(), Some("/opt/project"));
    }

    #[test]
    fn test_scroll_down_at_zero_noop() {
        let mut term = Terminal::new(80, 24);
        term.scroll_down(10);
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn test_cursor_save_restore_csi_form() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[10;5H");
        term.feed(b"\x1b[s");
        term.feed(b"\x1b[1;1H");
        term.feed(b"\x1b[u");
        assert_eq!(term.cursor().row, 9);
        assert_eq!(term.cursor().col, 4);
    }

    #[test]
    fn test_ind_index_down() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"X");
        term.feed(b"\x1bD");
        assert_eq!(term.cursor().row, 1);
        assert_eq!(term.cell(0, 0).ch, 'X');
    }

    // ── DECSCUSR cursor shape tests ──────────────────────────────────

    #[test]
    fn test_decscusr_block() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[1 q");
        assert_eq!(term.cursor_style, CursorStyle::Block);
        assert!(term.cursor_blink);
    }

    #[test]
    fn test_decscusr_steady_block() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[2 q");
        assert_eq!(term.cursor_style, CursorStyle::Block);
        assert!(!term.cursor_blink);
    }

    #[test]
    fn test_decscusr_blinking_underline() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[3 q");
        assert_eq!(term.cursor_style, CursorStyle::Underline);
        assert!(term.cursor_blink);
    }

    #[test]
    fn test_decscusr_steady_underline() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[4 q");
        assert_eq!(term.cursor_style, CursorStyle::Underline);
        assert!(!term.cursor_blink);
    }

    #[test]
    fn test_decscusr_blinking_bar() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[5 q");
        assert_eq!(term.cursor_style, CursorStyle::Bar);
        assert!(term.cursor_blink);
    }

    #[test]
    fn test_decscusr_steady_bar() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[6 q");
        assert_eq!(term.cursor_style, CursorStyle::Bar);
        assert!(!term.cursor_blink);
    }

    #[test]
    fn test_decscusr_default() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[6 q"); // set to bar first
        assert_eq!(term.cursor_style, CursorStyle::Bar);
        term.feed(b"\x1b[0 q"); // reset to default
        assert_eq!(term.cursor_style, CursorStyle::Block);
        assert!(term.cursor_blink);
    }

    #[test]
    fn test_decscusr_reset_preserves() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[5 q"); // bar + blink
        assert_eq!(term.cursor_style, CursorStyle::Bar);
        term.reset();
        assert_eq!(term.cursor_style, CursorStyle::Block);
        assert!(term.cursor_blink);
    }

    // ── Sixel infrastructure tests ───────────────────────────────────

    #[test]
    fn test_sixel_images_empty_initially() {
        let term = Terminal::new(80, 24);
        assert!(term.sixel_images.is_empty());
    }

    #[test]
    fn test_sixel_buffer_none_initially() {
        let term = Terminal::new(80, 24);
        assert!(term.sixel_buffer.is_none());
    }

    // ── base64 decode tests ──────────────────────────────────────────

    #[test]
    fn test_base64_decode_bytes_valid() {
        let result = base64_decode_bytes(b"aGVsbG8=");
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_base64_decode_bytes_empty() {
        let result = base64_decode_bytes(b"");
        assert!(result.is_empty());
    }

    #[test]
    fn test_base64_decode_bytes_with_newlines() {
        let result = base64_decode_bytes(b"aGVs\nbG8=");
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_base64_decode_bytes_invalid() {
        let result = base64_decode_bytes(b"!!!invalid!!!");
        assert!(result.is_empty());
    }

    // ── OSC themed color response tests ──────────────────────────────

    #[test]
    fn test_osc_11_returns_themed_bg() {
        let mut term = Terminal::new(80, 24);
        let fg = Color::new(200, 200, 200);
        let bg = Color::new(0x2e, 0x34, 0x40);
        term.apply_theme(fg, bg, default_ansi_palette());
        term.feed(b"\x1b]11;?\x1b\\");
        let response = term.take_response().unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.starts_with("\x1b]11;rgb:"));
        assert!(response_str.contains("2e2e/3434/4040"));
    }

    #[test]
    fn test_osc_12_returns_themed_fg() {
        let mut term = Terminal::new(80, 24);
        let fg = Color::new(0xec, 0xef, 0xf4);
        let bg = Color::new(0x2e, 0x34, 0x40);
        term.apply_theme(fg, bg, default_ansi_palette());
        term.feed(b"\x1b]12;?\x1b\\");
        let response = term.take_response().unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.starts_with("\x1b]12;rgb:"));
        assert!(response_str.contains("ecec/efef/f4f4"));
    }

    #[test]
    fn test_osc_9_queues_notification() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]9;Build finished\x07");
        let notifs: Vec<String> = term.drain_notifications().collect();
        assert_eq!(notifs, vec!["Build finished".to_string()]);
        // Drain consumed the queue — second call returns empty.
        assert_eq!(term.drain_notifications().count(), 0);
    }

    #[test]
    fn test_osc_9_empty_body_is_ignored() {
        // ESC ] 9 ; ST  with no body — spec allows it, we treat as no-op
        // since the useful notifications always carry a message.
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]9;\x07");
        assert_eq!(term.drain_notifications().count(), 0);
    }

    #[test]
    fn test_osc_9_multiple_notifications_preserve_order() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]9;one\x07");
        term.feed(b"\x1b]9;two\x07");
        term.feed(b"\x1b]9;three\x07");
        let notifs: Vec<String> = term.drain_notifications().collect();
        assert_eq!(notifs, vec!["one".to_string(), "two".into(), "three".into()]);
    }

    #[test]
    fn test_osc_104_resets_specific_palette_index() {
        let mut term = Terminal::new(80, 24);
        // Override palette index 1 to something unusual.
        let mut ansi = default_ansi_palette();
        let original = ansi[1];
        ansi[1] = Color::new(0xaa, 0xbb, 0xcc);
        term.apply_theme(term.pen_fg, term.default_bg, ansi);
        assert_eq!(term.ansi_palette()[1], Color::new(0xaa, 0xbb, 0xcc));

        // OSC 104 with explicit index resets just that one.
        term.feed(b"\x1b]104;1\x07");
        assert_eq!(term.ansi_palette()[1], original);
    }

    #[test]
    fn test_parse_osc_color_accepts_hex_and_rgb_forms() {
        // Hex triplet: `#rrggbb`.
        assert_eq!(
            parse_osc_color(b"#ff8000"),
            Some(Color::new(0xff, 0x80, 0x00))
        );
        // xterm short: `rgb:RR/GG/BB`.
        assert_eq!(
            parse_osc_color(b"rgb:ff/80/00"),
            Some(Color::new(0xff, 0x80, 0x00))
        );
        // xterm full: `rgb:RRRR/GGGG/BBBB` — high byte wins.
        assert_eq!(
            parse_osc_color(b"rgb:ffff/8080/0000"),
            Some(Color::new(0xff, 0x80, 0x00))
        );
        // Invalid payloads return None (OSC handler treats as no-op).
        assert_eq!(parse_osc_color(b"red"), None);
        assert_eq!(parse_osc_color(b"#zzzzzz"), None);
        assert_eq!(parse_osc_color(b"rgb:ff/80"), None);
    }

    #[test]
    fn test_osc_10_set_foreground() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]10;#aabbcc\x1b\\");
        assert_eq!(term.pen_fg, Color::new(0xaa, 0xbb, 0xcc));
    }

    #[test]
    fn test_osc_11_set_background() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]11;rgb:11/22/33\x1b\\");
        assert_eq!(term.default_bg, Color::new(0x11, 0x22, 0x33));
    }

    #[test]
    fn test_osc_12_set_cursor_color() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]12;rgb:44/55/66\x1b\\");
        assert_eq!(term.default_fg, Color::new(0x44, 0x55, 0x66));
    }

    #[test]
    fn test_osc_4_set_palette_index() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]4;3;#deadbe\x1b\\");
        assert_eq!(term.ansi_palette()[3], Color::new(0xde, 0xad, 0xbe));
    }

    #[test]
    fn test_osc_4_set_ignored_for_out_of_range_index() {
        // Indices 16..=255 aren't modeled as mutable yet; OSC 4 set
        // on those should be a silent no-op (not a panic, not a
        // partial overwrite of the 0..16 range).
        let mut term = Terminal::new(80, 24);
        let before = term.ansi_palette()[0];
        term.feed(b"\x1b]4;200;#112233\x1b\\");
        assert_eq!(term.ansi_palette()[0], before);
    }

    #[test]
    fn test_osc_10_malformed_payload_is_noop() {
        // Unparseable color string → handler returns early, no panic,
        // pen_fg unchanged.
        let mut term = Terminal::new(80, 24);
        let before = term.pen_fg;
        term.feed(b"\x1b]10;not-a-color\x1b\\");
        assert_eq!(term.pen_fg, before);
    }

    #[test]
    fn test_osc_104_without_indices_resets_all() {
        let mut term = Terminal::new(80, 24);
        let mut ansi = default_ansi_palette();
        ansi[0] = Color::new(0x11, 0x22, 0x33);
        ansi[15] = Color::new(0x99, 0x88, 0x77);
        term.apply_theme(term.pen_fg, term.default_bg, ansi);

        term.feed(b"\x1b]104\x07");
        let restored = term.ansi_palette();
        let defaults = default_ansi_palette();
        assert_eq!(restored, &defaults);
    }
}
