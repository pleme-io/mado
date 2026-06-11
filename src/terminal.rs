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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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

    /// Raw bitfield — exposed for MCP snapshot serialization. The
    /// bit positions match the BOLD/ITALIC/UNDERLINE/BLINK/INVERSE/
    /// STRIKETHROUGH/DIM/HIDDEN constants above, in that order.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
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

/// Direction for an OSC 133 prompt-jump query — consumed by
/// [`Terminal::scroll_offset_for_prompt_jump`]. Kept separate from
/// the public `scroll_offset_to_{prev,next}_prompt` methods so the
/// call sites in `main.rs` stay direction-self-documenting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptJumpDirection {
    Prev,
    Next,
}

// ---------------------------------------------------------------------------
// Color
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// P32 — interned style ID. Populated on every cell write by
    /// `put_char` from Terminal's `style_table` so that adjacent
    /// cells with identical (fg, bg, attrs) share a u16 tag. The
    /// renderer's shape cache key uses this u16 instead of three
    /// raw bytes — smaller key, faster equality. Existing inline
    /// fg/bg/attrs are kept too so all read sites continue to work
    /// unchanged; the eventual Cell-shrink (drop inline fields, keep
    /// only style_id) is a follow-up that the table interning here
    /// is the prerequisite for.
    ///
    /// `0` is reserved for the default style (Color::WHITE on
    /// Color::BLACK, no attrs) so a fresh Cell::default never has to
    /// touch the table.
    pub style_id: u16,
    /// Hyperlink URL (from OSC 8). None for most cells. `Arc<str>`
    /// rather than `Box<String>` so that adjacent cells inside the same
    /// hyperlink share one allocation — printing N characters under an
    /// OSC-8 hyperlink used to allocate N strings + N boxes (~2N
    /// per-byte allocations on hyperlink-heavy `ls` output); after this
    /// change it's one Arc::clone per cell (ref-count bump).
    pub hyperlink: Option<std::sync::Arc<str>>,
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
            // style_id 0 == DEFAULT_STYLE_ID (reserved for the canonical
            // WHITE-on-BLACK no-attrs style). Cell::default never has to
            // touch the StyleTable.
            style_id: DEFAULT_STYLE_ID,
            hyperlink: None,
        }
    }
}

/// Reserved style ID for the canonical default style
/// (Color::WHITE fg, Color::BLACK bg, CellAttrs::NONE). StyleTable's
/// constructor pre-populates this entry so it's always valid.
pub const DEFAULT_STYLE_ID: u16 = 0;

/// Style (fg, bg, attrs) interned as a single value. P32. The
/// styling axes that define how a Cell renders. Cell stores both
/// the inline triple (transition compatibility) and an interned
/// u16 ID into [`StyleTable`] (lookup-friendly). Future Cell shrink
/// will drop the inline fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

/// Interning table mapping `Style` ↔ `u16` ID. Each Terminal owns
/// one. `intern(style)` returns the existing ID or allocates a new
/// one; `lookup(id)` resolves an ID back to the Style. Capacity is
/// bounded at `u16::MAX - 1` styles (more than enough for any
/// realistic terminal session — typical sessions have &lt;50 unique
/// styles).
#[derive(Debug, Clone)]
pub struct StyleTable {
    styles: Vec<Style>,
    by_style: std::collections::HashMap<Style, u16>,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTable {
    /// Construct a fresh table with the default style pre-interned
    /// at index `DEFAULT_STYLE_ID` (= 0).
    #[must_use]
    pub fn new() -> Self {
        let default = Style {
            fg: Color::WHITE,
            bg: Color::BLACK,
            attrs: CellAttrs::NONE,
        };
        let mut by_style = std::collections::HashMap::new();
        by_style.insert(default, DEFAULT_STYLE_ID);
        Self {
            styles: vec![default],
            by_style,
        }
    }

    /// Intern a style: return the existing ID or allocate a new one.
    /// Capacity bounded at `u16::MAX - 1`; beyond that the default
    /// is returned (silent saturation rather than panic — the table
    /// is renderer-hint, not load-bearing for correctness because
    /// Cell still carries the inline fg/bg/attrs triple).
    pub fn intern(&mut self, style: Style) -> u16 {
        if let Some(&id) = self.by_style.get(&style) {
            return id;
        }
        let id = self.styles.len();
        if id >= u16::MAX as usize {
            return DEFAULT_STYLE_ID;
        }
        let id = id as u16;
        self.styles.push(style);
        self.by_style.insert(style, id);
        id
    }

    /// Resolve an ID back to its Style. Returns the default style if
    /// `id` is out of bounds (defensive — should never happen with
    /// IDs allocated by this table).
    #[must_use]
    pub fn lookup(&self, id: u16) -> Style {
        self.styles
            .get(id as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Number of distinct interned styles (≥ 1 — the default style
    /// is always present at index 0).
    #[must_use]
    pub fn len(&self) -> usize {
        self.styles.len()
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
    /// Iterator over ALL rows (scrollback + visible) starting at the
    /// absolute index `from` — the scrollback-search row source.
    fn rows_from(&self, from: usize) -> impl Iterator<Item = &[Cell]> {
        self.rows.iter().skip(from).map(Vec::as_slice)
    }

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
    ///
    /// Returns the number of rows evicted from the front of the
    /// scrollback this call — callers (e.g., `Terminal::scroll_grid_up`)
    /// use this to shift prompt-mark indices and any other
    /// grid-row-referencing state.
    fn scroll_region_up(&mut self, top: usize, bottom: usize) -> usize {
        let sb_offset = self.scrollback_len();
        let mut evicted = 0;

        if top == 0 && bottom == self.visible_rows - 1 {
            // Full-screen scroll: push top to scrollback, append blank
            self.rows.push_back(vec![Cell::default(); self.cols]);
            // Evict oldest scrollback if over limit
            while self.scrollback_len() > self.max_scrollback {
                self.rows.pop_front();
                evicted += 1;
            }
        } else {
            // Partial scroll region: remove the top row, insert blank at bottom
            let remove_idx = sb_offset + top;
            self.rows.remove(remove_idx);
            let insert_idx = sb_offset + bottom;
            // After removal, indexes shifted down, so insert at the same logical position
            self.rows.insert(insert_idx, vec![Cell::default(); self.cols]);
        }
        evicted
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

/// A lone `ESC` carried across a [`Terminal::feed`] boundary.
///
/// mado intercepts APC (`ESC _ … ST`) in `feed()` before vte sees it,
/// so the two-byte `ESC _` introducer and `ESC \` ST terminator must
/// be reassembled when a `feed()` chunk ends exactly on the `ESC`.
/// The variant records the context the trailing `ESC` appeared in so
/// the next feed's first byte can complete (or reject) the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingEsc {
    /// No carried ESC.
    None,
    /// A trailing `ESC` in ground state — could begin `ESC _` (APC
    /// start) or any other ESC-initiated sequence vte owns.
    Ground,
    /// A trailing `ESC` while inside the APC accumulator — could begin
    /// the `ESC \` ST terminator, or be literal APC payload.
    InApc,
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

/// Length of an incomplete UTF-8 sequence at the END of `bytes`.
///
/// Returns the number of trailing bytes that form the start of a
/// multi-byte UTF-8 codepoint whose continuation bytes have not all
/// arrived yet (so `bytes[..len - tail]` is valid UTF-8 up to a
/// codepoint boundary, or up to an *invalid* byte we leave for vte to
/// turn into a replacement char). Returns `0` when `bytes` ends on a
/// complete codepoint or on an invalid byte (vte handles those in the
/// same advance() call). A lead byte indicates its own length via its
/// high bits; we only treat the tail as incomplete when fewer
/// continuation bytes than the lead promises are present. Caps the
/// scan at the last 3 bytes — no UTF-8 codepoint exceeds 4 bytes.
fn incomplete_utf8_tail_len(bytes: &[u8]) -> usize {
    // Walk back over continuation bytes (0b10xx_xxxx) to find the lead.
    let n = bytes.len();
    let mut cont = 0usize;
    while cont < 3 && cont < n && (bytes[n - 1 - cont] & 0b1100_0000) == 0b1000_0000 {
        cont += 1;
    }
    if cont == n {
        // The whole (short) buffer is continuation bytes with no lead —
        // not our incomplete-tail case (vte emits replacements). Leave it.
        return 0;
    }
    let lead_idx = n - 1 - cont;
    let lead = bytes[lead_idx];
    // Expected total length encoded by the lead byte's high bits.
    let expected = if lead & 0b1000_0000 == 0 {
        1 // ASCII — complete.
    } else if lead & 0b1110_0000 == 0b1100_0000 {
        2
    } else if lead & 0b1111_0000 == 0b1110_0000 {
        3
    } else if lead & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        // Not a valid lead byte (stray continuation / 0xF8+) — let vte
        // emit the replacement char; nothing incomplete to hold.
        return 0;
    };
    let have = cont + 1; // continuation bytes + the lead.
    if have < expected {
        // Incomplete: hold the lead + the continuation bytes we have.
        have
    } else {
        0
    }
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


    /// P32 — style ID interning table. Maps (fg, bg, attrs) triples
    /// to a u16 tag stored on every Cell. The renderer's shape cache
    /// can key on style_id u16 instead of three raw bytes; ID equality
    /// implies styling equality (cheaper than triple-byte comparison
    /// in the hash + on lookup).
    pub(crate) style_table: StyleTable,

    /// Single-slot cache for the most recent (style, style_id) pair
    /// looked up via `style_table.intern`. Streaming output (e.g.
    /// `ls -ltra` with --color) writes long runs of cells that share
    /// the exact same pen state; without this cache, every cell would
    /// pay a SipHash + HashMap probe (~50–200 ns/cell — adds up to
    /// several ms over a screen of output). When the current pen
    /// matches the cached style, we skip the table lookup entirely.
    /// Hit rate on real workloads is ~95%+ (pen changes only on SGR
    /// transitions, not per cell).
    cached_style: Option<Style>,
    cached_style_id: u16,

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

    // Active hyperlink URI (from OSC 8, applied to subsequent cells).
    // Arc<str> so that the per-character paint path can clone the Arc
    // (ref-count bump) instead of allocating a fresh String + Box per
    // cell. One OSC-8 hyperlink over N characters used to allocate
    // ~2N strings; with Arc it's one allocation for the URI plus N
    // ref-count bumps.
    active_hyperlink: Option<std::sync::Arc<str>>,

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

    // Shell integration markers (from OSC 133). Typed history —
    // see `prompt_mark::PromptHistory` for the jump API.
    prompt_marks: crate::prompt_mark::PromptHistory,

    // OSC 22 — shell-requested mouse pointer shape. Default until
    // an app opts in via `ESC ] 22 ; <css-cursor-name> ST`.
    pointer_shape: crate::pointer_shape::PointerShape,

    // OSC 1337 SetMark — user-emitted grid-row marks. Parallel to
    // `prompt_marks` but with different provenance (script-echoed
    // vs shell-emitted).
    user_marks: crate::osc_1337::UserMarkHistory,

    // OSC 1337 RequestAttention — flag the window manager should
    // bounce the dock / flash the titlebar until focus returns.
    attention_requested: bool,

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

    // Carried-over ESC state across feed() boundaries. mado intercepts
    // APC (ESC _ … ST) BEFORE vte sees it (vte silently swallows APC
    // content), so the two-byte introducer (ESC _) and the two-byte
    // ST terminator (ESC \) must survive a feed() split. A lone ESC at
    // the end of a feed can't peek its successor — we record which
    // context it appeared in and resolve it on the next feed's first
    // byte. Without this, a multi-byte char (e.g. an em-dash) that
    // follows a split ESC \ ST gets swallowed into the never-terminated
    // APC buffer. See split_esc_st_across_feeds regression test.
    pending_esc: PendingEsc,

    // Incomplete trailing UTF-8 bytes carried across feed() boundaries.
    // vte 0.15's own partial-UTF-8 completion (advance_partial_utf8)
    // silently DROPS any valid bytes that follow the completed
    // codepoint inside its 4-byte window — e.g. feeding `C2 A1 41` after
    // a partial `C2` completes `¡` but discards the `A`. We sidestep
    // that by never leaving a partial codepoint inside vte: feed() holds
    // back any incomplete UTF-8 tail of a ground run and prepends it to
    // the next feed, so vte always receives whole codepoints in one
    // advance() and never enters the lossy partial path. ESC/APC bytes
    // are never part of a multi-byte sequence, so a held tail is always
    // pure ground content safe to prepend. See
    // split_multibyte_char_across_feeds + the well-formed proptest.
    utf8_tail: Vec<u8>,

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
            style_table: StyleTable::new(),
            cached_style: None,
            cached_style_id: DEFAULT_STYLE_ID,
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
            prompt_marks: crate::prompt_mark::PromptHistory::with_capacity(
                max_scrollback.max(256),
            ),
            pointer_shape: crate::pointer_shape::PointerShape::default(),
            user_marks: crate::osc_1337::UserMarkHistory::with_capacity(
                max_scrollback.max(256),
            ),
            attention_requested: false,
            kitty_keyboard_stack: Vec::new(),
            images: HashMap::new(),
            image_placements: Vec::new(),
            next_image_id: 1,
            pending_kitty: None,
            sixel_images: Vec::new(),
            sixel_buffer: None,
            apc_buf: None,
            pending_esc: PendingEsc::None,
            utf8_tail: Vec::new(),
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

    pub fn feed(&mut self, input: &[u8]) {
        // Intercept APC sequences (ESC _ G ... ST) for Kitty graphics.
        // vte swallows APC content without dispatching, so we parse it manually.
        let mut i = 0;
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());

        // Prepend any incomplete UTF-8 tail held back from the previous
        // feed() so vte sees whole codepoints (see `utf8_tail`). A held
        // tail is pure ground content (ESC/APC bytes are never UTF-8
        // continuations), so this prepend can't disturb the APC/ESC
        // index logic below. utf8_tail and pending_esc are mutually
        // exclusive — a trailing ESC can't be a UTF-8 continuation byte —
        // so we never need to combine the two carries.
        let combined: Vec<u8>;
        let bytes: &[u8] = if self.utf8_tail.is_empty() {
            input
        } else {
            combined = self.utf8_tail.drain(..).chain(input.iter().copied()).collect();
            &combined
        };

        // Resolve any ESC carried from the previous feed() against this
        // chunk's first byte. A lone trailing ESC is ambiguous until its
        // successor arrives — `ESC _` (APC start) and `ESC \` (APC ST)
        // are the two pairs mado reassembles itself; anything else
        // belongs to vte. Without this, a split `ESC \` ST never
        // terminates the APC and silently eats whatever follows (e.g. a
        // multi-byte char). See split_esc_st_across_feeds.
        // An empty chunk can't disambiguate a carried ESC — keep the
        // carry untouched and return. (Common: a flush with no new PTY
        // bytes must not force-resolve the pending ESC.)
        if bytes.is_empty() {
            self.parser = parser;
            return;
        }
        match self.pending_esc {
            PendingEsc::None => {}
            PendingEsc::InApc => {
                self.pending_esc = PendingEsc::None;
                if bytes[0] == b'\\' {
                    // Carried ESC + `\` = ST — terminate the APC now.
                    if let Some(buf) = self.apc_buf.take() {
                        self.handle_apc(&buf);
                    }
                    i = 1;
                } else {
                    // Anywhere-ESC rule (DEC/vte state machine): ESC followed
                    // by anything but `\` ABORTS the string sequence — it is
                    // never literal payload. Treating it as payload turned an
                    // unterminated APC into a permanent black hole that
                    // swallowed every later byte (including `ESC[6n` cursor
                    // queries — the shell-killing class; see the CPR-liveness
                    // test). Kitty APC payloads are base64/key-value and
                    // never contain raw ESC, so aborting loses nothing real.
                    self.apc_buf = None;
                    // The carried ESC belongs to vte (or starts a new APC if
                    // byte 0 is `_` — the ground path below handles both).
                    if bytes[0] == b'_' {
                        self.apc_buf = Some(Vec::new());
                        i = 1;
                    } else {
                        parser.advance(self, &[0x1b]);
                    }
                }
            }
            PendingEsc::Ground => {
                self.pending_esc = PendingEsc::None;
                if bytes[0] == b'_' {
                    // Carried ESC + `_` = APC introducer.
                    self.apc_buf = Some(Vec::new());
                    i = 1;
                } else {
                    // The ESC belongs to vte — hand it over so vte's own
                    // (chunk-boundary-preserving) parser resolves it.
                    parser.advance(self, &[0x1b]);
                }
            }
        }

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
                if bytes[i] == 0x1b {
                    if i + 1 < bytes.len() {
                        if bytes[i + 1] == b'\\' {
                            let payload = std::mem::take(buf);
                            self.apc_buf = None;
                            self.handle_apc(&payload);
                            i += 2;
                            continue;
                        }
                        // Anywhere-ESC rule: ESC + non-`\` ABORTS the APC —
                        // it is never literal payload (see the carried-ESC
                        // arm above for the full rationale). Reprocess the
                        // ESC in ground state without consuming it.
                        self.apc_buf = None;
                        continue;
                    }
                    // Trailing ESC inside the APC — carry it; the next
                    // feed decides whether it completes the `ESC \` ST.
                    self.pending_esc = PendingEsc::InApc;
                    i += 1;
                    continue;
                }
                // Bound the payload: an APC whose ST never arrives must not
                // accumulate without limit (kitty image payloads are large
                // but chunked; 8 MiB is far beyond any legitimate chunk).
                const APC_MAX: usize = 8 * 1024 * 1024;
                if buf.len() >= APC_MAX {
                    tracing::warn!(len = buf.len(), "APC payload exceeded bound — aborting sequence");
                    self.apc_buf = None;
                    continue;
                }
                buf.push(bytes[i]);
                i += 1;
                continue;
            }

            // Detect APC start: ESC _ (0x1b 0x5f)
            if bytes[i] == 0x1b {
                if i + 1 < bytes.len() {
                    if bytes[i + 1] == b'_' {
                        self.apc_buf = Some(Vec::new());
                        i += 2;
                        continue;
                    }
                    // ESC + non-`_`: a vte-owned sequence. Fall through
                    // to the ground scan below, which begins AT this ESC
                    // and hands the run (ESC + payload) to vte.
                } else {
                    // Trailing ESC in ground — carry it; the next feed
                    // decides whether it begins an `ESC _` APC start.
                    self.pending_esc = PendingEsc::Ground;
                    i += 1;
                    continue;
                }
            }

            // Accumulate a ground run for vte. The run always includes
            // the current byte (which may be a vte-owned ESC we just
            // cleared as a non-APC introducer) and extends until the
            // NEXT ESC, which the loop head re-examines (peeking its
            // successor for `ESC _`, or carrying it across the feed
            // boundary). Starting the scan one byte in guarantees forward
            // progress even when `bytes[i]` is itself an ESC.
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != 0x1b {
                i += 1;
            }

            // Feed the non-APC portion to vte. vte 0.15 SIMD-fast-paths
            // printable-ASCII runs internally (advance_ground via memchr)
            // and preserves mid-CSI state across advance() calls because
            // we reuse the same Parser. Chunk-boundary independence is
            // the load-bearing invariant: a styled line whose SGR intro
            // splits across two feed() calls must render identically to
            // the whole-stream feed. See the split_csi_* /
            // split_esc_st_across_feeds tests + the well-formed proptest.
            //
            // If this run ran to the end of the chunk (not cut by an ESC)
            // and ends on an incomplete UTF-8 lead, hold those tail bytes
            // back for the next feed rather than letting vte buffer them:
            // vte's advance_partial_utf8 DROPS valid bytes that trail the
            // completed codepoint inside its 4-byte window (e.g. the `A`
            // in `C2 A1 41`). Holding the tail at this layer keeps every
            // codepoint whole within a single advance() so vte never
            // enters that lossy path. An ESC-terminated run is always
            // complete (ESC is never a UTF-8 continuation), so we only
            // trim when the run reaches the chunk end.
            let mut run_end = i;
            if i == bytes.len() {
                let tail = incomplete_utf8_tail_len(&bytes[start..run_end]);
                if tail > 0 {
                    self.utf8_tail.extend_from_slice(&bytes[run_end - tail..run_end]);
                    run_end -= tail;
                }
            }
            if start < run_end {
                parser.advance(self, &bytes[start..run_end]);
            }
        }

        self.parser = parser;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 {
            return;
        }
        // Same-dims resize is a no-op — without this guard a redundant
        // resize (e.g. an event-loop reconciler re-confirming the grid)
        // silently resets DECSTBM scroll regions, tab stops, and
        // wrap_pending out from under a running TUI.
        if cols == self.cols && rows == self.rows {
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

    /// Whether the application has set DEC mode 2026 (synchronized
    /// output / BSU pending). When true the renderer is expected to
    /// hold off on painting until the matching DECRST clears the
    /// flag — eliminates tearing during full-screen TUI redraws
    /// (helix, lazygit, btop). Kitty measured +20–50% throughput
    /// on TUI apps that emit this.
    #[must_use]
    pub fn synchronized_output(&self) -> bool {
        self.synchronized_output
    }

    /// Whether the alternate-screen buffer is active (DECSET 47 / 1047
    /// / 1049). TUI apps (vim, helix, lazygit, btop, top) switch into
    /// the alt-screen on launch and back out on exit. While here, the
    /// renderer can skip URL detection (TUI apps don't render
    /// hyperlinks) and is unlikely to see Kitty graphics — both
    /// elide-able snapshots' work.
    #[must_use]
    pub fn on_alt_screen(&self) -> bool {
        self.use_alternate
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
    /// True while the alternate screen (DECSET 47/1047/1049) is
    /// active — i.e. a full-screen TUI owns the viewport. Scrollback
    /// navigation is meaningless there; bare PageUp/PageDown must
    /// reach the application as ESC[5~/[6~ instead.
    pub fn is_alternate_screen(&self) -> bool {
        self.use_alternate
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    #[must_use]
    #[allow(dead_code)]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Total scrollback rows currently held (active grid). The
    /// renderer's history indicator sizes its thumb from this.
    #[must_use]
    pub fn scrollback_total(&self) -> usize {
        self.grid().scrollback_len()
    }

    /// Rows for scrollback search: the most recent `cap` history rows
    /// plus the live screen, with the ABSOLUTE index of the first
    /// returned row (scrollback origin 0) so match addresses stay
    /// valid while the viewport scrolls. The cap bounds the per-edit
    /// scan cost on unbounded-scrollback configs.
    #[must_use]
    pub fn search_rows(&self, cap: usize) -> (Vec<Vec<Cell>>, usize) {
        let grid = self.grid();
        let sb_len = grid.scrollback_len();
        let first_abs = sb_len.saturating_sub(cap);
        let rows: Vec<Vec<Cell>> = grid
            .rows_from(first_abs)
            .map(<[Cell]>::to_vec)
            .collect();
        (rows, first_abs)
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
                kind = kind.as_str(),
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
        let uri = String::from_utf8_lossy(params[2]);
        self.active_hyperlink = if uri.is_empty() {
            None
        } else {
            Some(std::sync::Arc::from(uri.as_ref()))
        };
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

    /// OSC 22 — Mouse pointer shape control.
    ///
    /// `ESC ] 22 ; <css-cursor-name> ST` — set the shape. The name
    /// vocabulary is CSS Basic UI cursor keywords (`text`,
    /// `pointer`, `wait`, …). Unknown names are dropped silently so
    /// a shell that speaks a newer revision of the protocol can't
    /// corrupt the typed state.
    ///
    /// `ESC ] 22 ; ? ST` — query the current shape. Respond with
    /// `ESC ] 22 ; <current-shape-name> ST` echoing the typed
    /// value's canonical name.
    fn handle_osc_22_pointer_shape(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        let arg = params[1];
        if arg == b"?" {
            // Query form — emit current shape name.
            let name = self.pointer_shape.as_str();
            let mut resp = Vec::with_capacity(8 + name.len());
            resp.extend_from_slice(b"\x1b]22;");
            resp.extend_from_slice(name.as_bytes());
            resp.extend_from_slice(b"\x1b\\");
            self.response_bytes.extend_from_slice(&resp);
            return;
        }
        let Ok(s) = std::str::from_utf8(arg) else {
            tracing::trace!(?arg, "OSC 22: non-UTF8 shape name");
            return;
        };
        match crate::pointer_shape::PointerShape::from_str_kind(s) {
            Some(shape) => {
                self.pointer_shape = shape;
                tracing::trace!(shape = s, "OSC 22: pointer shape set");
            }
            None => {
                tracing::trace!(shape = s, "OSC 22: unknown shape, ignoring");
            }
        }
    }

    /// Currently active mouse pointer shape (from OSC 22).
    /// Default until a shell opts in. Consumed by the input / cursor
    /// rendering layer to drive the actual platform cursor.
    #[must_use]
    #[allow(dead_code)] // Typed surface for the pending renderer wire-up.
    pub fn pointer_shape(&self) -> crate::pointer_shape::PointerShape {
        self.pointer_shape
    }

    /// OSC 1337 — iTerm2 proprietary extensions. Parses the
    /// parameter into [`Osc1337Param`](crate::osc_1337::Osc1337Param)
    /// and dispatches. `SetMark` records the cursor row into the
    /// typed [`user_marks`](Self::user_marks) history.
    /// `RequestAttention=<0|1>` flips the
    /// [`attention_requested`](Self::attention_requested) flag the
    /// platform layer reads to drive dock / titlebar notifications.
    /// Unknown parameters log + ignore so a shell speaking a newer
    /// dialect can't corrupt typed state.
    fn handle_osc_1337_iterm2(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        match crate::osc_1337::parse_osc_1337(params[1]) {
            crate::osc_1337::Osc1337Param::SetMark => {
                if self.use_alternate {
                    // Don't record marks on the alt screen — same
                    // guarantee as OSC 133 prompt marks.
                    return;
                }
                let grid_row = self.primary.scrollback_len() + self.cursor.row;
                self.user_marks.record(grid_row);
                tracing::trace!(grid_row, "OSC 1337 SetMark recorded");
            }
            crate::osc_1337::Osc1337Param::RequestAttention(flag) => {
                self.attention_requested = flag;
                tracing::trace!(flag, "OSC 1337 RequestAttention");
            }
            crate::osc_1337::Osc1337Param::Unknown(s) => {
                tracing::trace!(key = %s, "OSC 1337 unknown parameter, ignoring");
            }
        }
    }

    /// Read-only handle on the OSC 1337 user-mark history.
    #[must_use]
    #[allow(dead_code)] // Typed surface for the pending MCP + jump API.
    pub fn user_marks(&self) -> &crate::osc_1337::UserMarkHistory {
        &self.user_marks
    }

    /// Current OSC 1337 RequestAttention state. The platform layer
    /// (dock bounce on macOS, urgency hint on X11) reads this to
    /// drive its attention-signal behavior.
    #[must_use]
    #[allow(dead_code)] // Typed surface for the pending platform wire-up.
    pub fn attention_requested(&self) -> bool {
        self.attention_requested
    }

    /// OSC 133 — Shell integration (semantic prompts).
    ///
    /// `A` = prompt start, `B` = command start, `C` = command output,
    /// `D` = command end. Shells emit these via the installed
    /// shell-integration scripts (see `shell-integration/mado.*`).
    /// Every mark is recorded in the typed [`prompt_mark::PromptHistory`]
    /// so the user can jump between prompts with a keybind (see
    /// [`Terminal::scroll_offset_to_prev_prompt`] /
    /// [`Terminal::scroll_offset_to_next_prompt`]).
    fn handle_osc_133_shell_integration(&mut self, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        let Some(kind) = crate::prompt_mark::PromptKind::from_osc_param(params[1]) else {
            return;
        };
        // Only record marks on the primary screen — shells don't
        // emit OSC 133 from inside full-screen TUIs (vim, less, …)
        // that use the alternate screen, and mark rows wouldn't
        // mean anything if they did.
        if self.use_alternate {
            return;
        }
        let grid_row = self.primary.scrollback_len() + self.cursor.row;
        self.prompt_marks.record(grid_row, kind);
        tracing::trace!(
            row = self.cursor.row,
            grid_row,
            kind = ?kind,
            "OSC 133 mark recorded",
        );
    }

    /// Grid-internal row of the most recent prompt start, if any.
    /// Preserved under the old name for the existing unit test +
    /// any MCP consumer — the richer [`prompt_marks`](Self::prompt_marks)
    /// accessor is the canonical new surface.
    #[must_use]
    #[allow(dead_code)]
    pub fn prompt_start_row(&self) -> Option<usize> {
        self.prompt_marks
            .iter()
            .rev()
            .find(|m| m.kind == crate::prompt_mark::PromptKind::Start)
            .map(|m| m.grid_row)
    }

    /// Read-only handle to the OSC 133 mark history. Exposed for
    /// unit tests + MCP tools that want to render a "jump to past
    /// prompt" picker.
    #[must_use]
    #[allow(dead_code)]
    pub fn prompt_marks(&self) -> &crate::prompt_mark::PromptHistory {
        &self.prompt_marks
    }

    /// Compute the scroll offset that would bring the nearest
    /// Start-kind prompt *above* the current viewport top into the
    /// top row of the viewport. Returns `None` when no such mark
    /// is recorded yet.
    ///
    /// The coordinate math mirrors [`Terminal::scroll_up`]: a
    /// `scroll_offset` of 0 shows the live bottom; offset = N shows
    /// the view shifted up by N rows.
    #[must_use]
    pub fn scroll_offset_to_prev_prompt(&self) -> Option<usize> {
        self.scroll_offset_for_prompt_jump(PromptJumpDirection::Prev)
    }

    /// Mirror of [`Self::scroll_offset_to_prev_prompt`] walking the
    /// opposite direction — finds the nearest prompt *below* the
    /// viewport top. Returning `Some(0)` is legal (next prompt is
    /// already in the live bottom view).
    #[must_use]
    pub fn scroll_offset_to_next_prompt(&self) -> Option<usize> {
        self.scroll_offset_for_prompt_jump(PromptJumpDirection::Next)
    }

    /// Shared geometry for [`Self::scroll_offset_to_prev_prompt`] and
    /// [`Self::scroll_offset_to_next_prompt`]. Both methods walk the
    /// same math — compute the current viewport-top grid row, look
    /// up the nearest prompt mark in `direction`, convert its grid
    /// row back into a scroll-offset. This helper is the single
    /// source of truth for that conversion; the two public methods
    /// are thin direction dispatchers so the call sites in `main.rs`
    /// stay readable.
    fn scroll_offset_for_prompt_jump(&self, direction: PromptJumpDirection) -> Option<usize> {
        let grid = &self.primary;
        let base = grid.rows.len().saturating_sub(grid.visible_rows);
        let view_top = base.saturating_sub(self.scroll_offset);
        let target = match direction {
            PromptJumpDirection::Prev => self.prompt_marks.prev_prompt(view_top)?,
            PromptJumpDirection::Next => self.prompt_marks.next_prompt(view_top)?,
        };
        Some(base.saturating_sub(target))
    }

    /// Block-aware rendering helper: viewport-relative row
    /// indices where an OSC 133 `A` (prompt-start) mark sits.
    /// The render layer draws a faint horizontal separator at
    /// each of these rows so the operator sees discrete blocks
    /// without needing a sidebar.
    ///
    /// Alt-screen TUIs (vim, helix, btop) don't have block
    /// boundaries — those screens are atomic. Returns an empty
    /// vec when alt is active.
    #[must_use]
    pub fn block_separator_viewport_rows(&self) -> Vec<usize> {
        if self.use_alternate {
            return Vec::new();
        }
        let grid = &self.primary;
        let base = grid.rows.len().saturating_sub(grid.visible_rows);
        let view_top = base.saturating_sub(self.scroll_offset);
        let view_bottom = view_top + grid.visible_rows;
        self.prompt_marks
            .iter()
            .filter(|m| {
                m.kind == crate::prompt_mark::PromptKind::Start
                    && m.grid_row >= view_top
                    && m.grid_row < view_bottom
            })
            .map(|m| m.grid_row - view_top)
            .collect()
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
        let use_alt = self.use_alternate;
        let evicted = self.grid_mut().scroll_region_up(top, bottom);
        // Content-pinning: a full-screen scroll on the primary grid
        // pushed one line into scrollback, shifting all content one
        // row away from the live bottom. When the operator is
        // scrolled up, grow the offset in lockstep so the SAME
        // content stays in view — streaming output never drags the
        // reader (kitty/ghostty behavior; operator report
        // 2026-06-11). At offset 0 the view tail-follows as before.
        // Partial-region scrolls don't touch scrollback; the
        // alternate screen has none.
        let pushed_to_scrollback = !use_alt && top == 0 && bottom == self.rows - 1;
        if pushed_to_scrollback && self.scroll_offset > 0 {
            let max = self.grid().scrollback_len();
            self.scroll_offset = (self.scroll_offset + 1).min(max);
        }
        // Only primary-grid evictions invalidate prompt / user
        // marks — alternate has zero scrollback and doesn't record.
        if !use_alt && evicted > 0 {
            self.prompt_marks.shift_on_evict(evicted);
            self.user_marks.shift_on_evict(evicted);
        }
        self.dirty();
    }

    fn scroll_grid_down(&mut self) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        self.grid_mut().scroll_region_down(top, bottom);
        self.dirty();
    }

    fn newline(&mut self) {
        // No scroll_offset reset here: a line feed is OUTPUT, and
        // output never moves the view (see print()). The "pressing
        // Enter doesn't follow the prompt down" expectation is the
        // INPUT layer's job — the engine snaps to bottom when the
        // operator's keystroke bytes go to the PTY.
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
            // Wide-char overwrite orphaning (pinned 2026-06-11,
            // COMPETITIVE.md §4): overwriting ONE half of a width-2
            // glyph must clear the partner cell. Without this, the
            // stale half survives — either a width-0 continuation
            // with no lead (renderer skips it → ghost column) or a
            // width-2 lead whose continuation now holds an unrelated
            // glyph (renderer draws half a CJK glyph under the new
            // char). Only the overwrite path orphans; IRM shifts
            // cells instead of overwriting them.
            if !self.insert_mode {
                // Left edge: the target cell is a continuation → its
                // lead (directly left) would orphan. Clear it.
                if col > 0
                    && self.grid().cell(row, col).width == 0
                    && self.grid().cell(row, col - 1).width == 2
                {
                    *self.grid_mut().cell_mut(row, col - 1) = Cell::default();
                }
                // Right edge: the last cell this write covers is a
                // wide lead → its continuation (directly right)
                // would orphan. Clear it.
                let last = col + char_width - 1;
                if last < self.cols
                    && self.grid().cell(row, last).width == 2
                    && last + 1 < self.cols
                {
                    *self.grid_mut().cell_mut(row, last + 1) = Cell::default();
                }
            }

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
            // P32 — intern the current pen state into the style
            // table. Adjacent cells with identical pen state share
            // a u16 ID (the table dedups). cell.style_id lets the
            // renderer's shape cache key on a u16 instead of the
            // raw (fg, bg, attrs) triple.
            //
            // Fast path: streaming output overwhelmingly writes runs
            // of cells with the same pen — check the single-slot
            // cache first to skip the HashMap probe + SipHash on
            // those cells. Pen changes only on SGR transitions, so
            // the cache miss rate is ~5% on real `ls --color` /
            // colored-log workloads.
            let style = Style { fg, bg, attrs };
            let style_id = if self.cached_style == Some(style) {
                self.cached_style_id
            } else {
                let id = self.style_table.intern(style);
                self.cached_style = Some(style);
                self.cached_style_id = id;
                id
            };
            let hyperlink = self.active_hyperlink.clone();
            let cell = self.grid_mut().cell_mut(row, col);
            cell.ch = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.attrs = attrs;
            cell.style_id = style_id;
            cell.extra = None;
            cell.width = char_width as u8;
            cell.hyperlink = hyperlink;

            // Wide chars occupy 2 cells — mark next cell as continuation
            if char_width == 2 && col + 1 < self.cols {
                let hyperlink = self.active_hyperlink.clone();
                let cont = self.grid_mut().cell_mut(row, col + 1);
                cont.ch = ' ';
                cont.width = 0;
                cont.fg = fg;
                cont.bg = bg;
                cont.attrs = attrs;
                cont.style_id = style_id;
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
        // Output must NEVER move the operator's view — the old
        // scroll_offset reset here yanked the viewport to the bottom
        // on EVERY printed character (a status-bar clock tick was
        // enough), making scrollback unreadable under any streaming
        // output (operator report 2026-06-11). The viewport is pinned
        // to CONTENT by scroll_grid_up; snap-to-bottom belongs to the
        // operator's own input (ux::InputEngine::write_key_input).

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
            b"22"      => self.handle_osc_22_pointer_shape(params),
            b"52"      => self.handle_osc_52_clipboard(params),
            b"104"     => self.handle_osc_104_palette_reset(params),
            b"110"     => self.handle_osc_110_fg_reset(),
            b"111"     => self.handle_osc_111_bg_reset(),
            b"112"     => self.handle_osc_112_cursor_reset(),
            b"133"     => self.handle_osc_133_shell_integration(params),
            b"1337"    => self.handle_osc_1337_iterm2(params),
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
                    // Secondary DA (DA2): CSI > Pp ; Pv ; Pc c.
                    // Single source of truth: TerminalCaps::SECONDARY_DA.
                    self.response_bytes
                        .extend_from_slice(crate::caps::TerminalCaps::SECONDARY_DA);
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
                // Bump seqno so the renderer's idle-skip path (P2/P28)
                // re-renders. Without this, a clear-screen (CSI 2J)
                // emitted without follow-up output would leave the
                // previous frame's pixels on screen until the next
                // write — a class of "shadow trail" symptom.
                self.dirty();
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
                let use_alt = self.use_alternate;
                let mut evicted = 0;
                for _ in 0..n.min(bottom - cursor_row + 1) {
                    evicted += self.grid_mut().scroll_region_up(cursor_row, bottom);
                }
                if !use_alt && evicted > 0 {
                    self.prompt_marks.shift_on_evict(evicted);
                    self.user_marks.shift_on_evict(evicted);
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
            // DA — Device Attributes (DA1). Single source of truth:
            // crate::caps::TerminalCaps::PRIMARY_DA.
            'c' => {
                self.response_bytes
                    .extend_from_slice(crate::caps::TerminalCaps::PRIMARY_DA);
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
            // XTWINOPS — window manipulation / report.
            //
            // Modern TUIs (vim/nvim powerline tablines, fzf, lazygit)
            // probe the text-area size to lay out their UI; a terminal
            // that ignores the probe leaves the TUI guessing and can
            // make it withhold whole rows (the missing-tabline symptom).
            // We answer the SIZE REPORTS the substrate has the data for
            // (character-cell dimensions), and ignore the manipulation
            // ops (resize/move/raise/iconify) — mado is the size
            // authority, a TUI can't drive our window geometry.
            //
            // CSI 18 t  → report text-area size in characters:
            //             CSI 8 ; <rows> ; <cols> t
            //
            // Built through typed byte-vector construction (escape
            // framing as byte literals, the numeric dimensions as their
            // decimal value) — never a `format!()` of the escape syntax,
            // per the ★★ TYPED EMISSION rule. Pixel-size reports
            // (CSI 14 t / CSI 16 t) are intentionally NOT answered here:
            // the Terminal grid model holds character dimensions only;
            // cell pixel metrics live in the renderer. Wiring those is a
            // separate change that threads cell_width/cell_height down.
            't' => {
                let op = params.iter().next().map_or(0, |p| p[0]);
                if op == 18 {
                    // u16 fits any realistic terminal dimension; the
                    // decimal string is a VALUE, not escape syntax.
                    let rows = u16::try_from(self.rows).unwrap_or(u16::MAX);
                    let cols = u16::try_from(self.cols).unwrap_or(u16::MAX);
                    let mut resp = Vec::with_capacity(16);
                    resp.extend_from_slice(b"\x1b[8;");
                    resp.extend_from_slice(rows.to_string().as_bytes());
                    resp.push(b';');
                    resp.extend_from_slice(cols.to_string().as_bytes());
                    resp.push(b't');
                    self.response_bytes.extend_from_slice(&resp);
                } else {
                    tracing::trace!(op, "unhandled XTWINOPS (CSI Ps t)");
                }
            }
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

    /// **Invariant: output never moves the operator's view**
    /// (2026-06-11 — "scrolling up while a TUI writes steals focus
    /// and sends me down"). While scrolled up, streaming output must
    /// keep the SAME content in the viewport: the offset grows in
    /// lockstep with lines entering scrollback. Only the operator's
    /// own input snaps to the live tail (engine layer).
    #[test]
    fn output_while_scrolled_keeps_view_pinned_to_content() {
        let mut term = Terminal::with_scrollback(20, 4, 100);
        for i in 0..20 {
            term.feed(b"line");
            term.feed(i.to_string().as_bytes());
            term.feed(b"\r\n");
        }
        term.scroll_up(8);
        let before: Vec<String> = term
            .visible_rows()
            .map(|r| r.iter().map(|c| c.ch).collect::<String>())
            .collect();

        // A TUI / streaming program writes more output.
        for i in 20..30 {
            term.feed(b"line");
            term.feed(i.to_string().as_bytes());
            term.feed(b"\r\n");
        }

        let after: Vec<String> = term
            .visible_rows()
            .map(|r| r.iter().map(|c| c.ch).collect::<String>())
            .collect();
        assert_eq!(
            before, after,
            "view must stay pinned to the content being read while output streams"
        );
        assert!(term.scroll_offset() > 8, "offset grows with scrollback");
    }

    /// At the bottom (offset 0) the view tail-follows output — the
    /// pinning above must not freeze the live view.
    #[test]
    fn output_at_bottom_tail_follows() {
        let mut term = Terminal::with_scrollback(20, 4, 100);
        for i in 0..30 {
            term.feed(b"line");
            term.feed(i.to_string().as_bytes());
            term.feed(b"\r\n");
        }
        assert_eq!(term.scroll_offset(), 0);
        let bottom: Vec<String> = term
            .visible_rows()
            .map(|r| r.iter().map(|c| c.ch).collect::<String>())
            .collect();
        assert!(
            bottom.iter().any(|r| r.starts_with("line29")),
            "live view follows the newest output: {bottom:?}"
        );
    }

    /// Alternate-screen output never disturbs the primary-screen
    /// scroll offset — a TUI redraw while the operator had scrolled
    /// the primary screen must not move their saved position.
    #[test]
    fn alt_screen_output_does_not_disturb_primary_scroll_offset() {
        let mut term = Terminal::with_scrollback(20, 4, 100);
        for i in 0..20 {
            term.feed(b"line");
            term.feed(i.to_string().as_bytes());
            term.feed(b"\r\n");
        }
        term.scroll_up(5);
        let pinned = term.scroll_offset();
        // Enter alt screen (1049), stream a full-screen redraw, leave.
        term.feed(b"\x1b[?1049h");
        for _ in 0..10 {
            term.feed(b"tui frame\r\n");
        }
        term.feed(b"\x1b[?1049l");
        assert_eq!(
            term.scroll_offset(),
            pinned,
            "alt-screen TUI output must not move the primary viewport"
        );
    }

    /// Same-dims resize is a no-op (invariant, 2026-06-11): the
    /// event-loop grid reconcilers may re-confirm the current grid;
    /// that must NEVER reset DECSTBM scroll regions / tab stops /
    /// wrap_pending out from under a running TUI.
    #[test]
    fn same_dims_resize_preserves_scroll_region_and_tabs() {
        let mut term = Terminal::new(80, 24);
        // App sets a scroll region (DECSTBM rows 5..10) and a custom
        // tab stop at column 3.
        term.feed(b"\x1b[5;10r");
        term.feed(b"\x1b[1;4H\x1bH"); // CUP col 4 + HTS
        let (top, bottom) = (term.scroll_top, term.scroll_bottom);
        assert_eq!((top, bottom), (4, 9), "precondition: region set");
        assert!(term.tab_stops[3], "precondition: tab stop set");

        // Same-dims resize: everything must survive.
        term.resize(80, 24);
        assert_eq!(
            (term.scroll_top, term.scroll_bottom),
            (top, bottom),
            "same-dims resize must not reset the scroll region"
        );
        assert!(
            term.tab_stops[3],
            "same-dims resize must not reset tab stops"
        );

        // A REAL resize still resets (existing contract).
        term.resize(100, 30);
        assert_eq!((term.scroll_top, term.scroll_bottom), (0, 29));
    }

    /// CPR-liveness invariant (class-killer, 2026-06-10): for EVERY prefix of
    /// a real captured frostmourne stream (one full prompt → Enter → re-prompt
    /// cycle), the terminal must still answer `ESC[6n`. A VT pre-parser state
    /// that swallows input (e.g. an unterminated APC accumulating forever)
    /// fails this for every prefix entering the bad state — and a shell whose
    /// CPR goes unanswered dies (reedline fatal timeout). The prefix itself
    /// may legitimately enqueue responses (the corpus contains the shell's own
    /// queries), so those are drained before the probe.
    #[test]
    fn cpr_liveness_for_every_prefix_of_a_real_shell_stream() {
        let corpus: &[u8] =
            include_bytes!("../tests/fixtures/frostmourne-enter-cycle.bin");
        let mut failures = Vec::new();
        for cut in 0..=corpus.len() {
            let mut term = Terminal::new(80, 24);
            term.feed(&corpus[..cut]);
            let _ = term.take_response();
            term.feed(b"\x1b[6n");
            if term.take_response().is_none() {
                failures.push(cut);
            }
        }
        assert!(
            failures.is_empty(),
            "CPR unanswered after feeding corpus prefix(es) of length {:?} — \
             a parser state is swallowing input",
            failures
        );
    }

    /// Same invariant under adversarial APC prefixes — unterminated kitty
    /// graphics openers, carried trailing ESCs, anywhere-ESC aborts. Before
    /// the anywhere-ESC fix, `ESC _` with no ST swallowed every later byte.
    #[test]
    fn cpr_liveness_survives_adversarial_apc_prefixes() {
        let cases: &[&[u8]] = &[
            b"\x1b_",                  // bare APC introducer, never terminated
            b"\x1b_G",                 // kitty graphics opener, unterminated
            b"\x1b_Gf=100,a=T;QUJD",   // kitty payload, no ST
            b"\x1b_G;x\x1b",           // unterminated + trailing ESC carried
            b"\x1b",                   // lone trailing ESC in ground
            b"\x1b_x\x1b[31m",         // APC aborted mid-payload by a CSI
        ];
        let mut failures = Vec::new();
        for (idx, prefix) in cases.iter().enumerate() {
            let mut term = Terminal::new(80, 24);
            term.feed(prefix);
            let _ = term.take_response();
            term.feed(b"\x1b[6n");
            if term.take_response().is_none() {
                failures.push(idx);
            }
        }
        assert!(
            failures.is_empty(),
            "CPR unanswered after adversarial APC prefix case(s) {:?}",
            failures
        );
    }

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

    /// HELD-ENTER SPACING isolation: the VT core must converge to the
    /// SAME visible grid regardless of how the PTY byte stream is split
    /// across `feed()` calls. A reedline/frostmourne empty-prompt cycle
    /// (submit `\r\n`, erase `ESC[J`, seki's leading `\n`, re-print the
    /// `❄ ` prompt) under a held-Enter burst arrives split across many
    /// chunks on the engate consumer thread. If this test PASSES (it
    /// should), the inconsistent inter-prompt spacing operators saw is
    /// NOT a feed()/newline() corruption — it's the render thread
    /// sampling un-converged intermediate grid states between feeds. The
    /// fix then belongs in the render/present path (drain-then-render or
    /// a quiescence defer), not in the VT core, which this pins as sound.
    #[test]
    fn held_enter_repaint_converges_identically_across_chunk_splits() {
        // One reedline-style empty-prompt cycle.
        // \r\n (submit) ; ESC[J (erase to end of display) ;
        // \n (seki add_newline) ; "❄ " (prompt).
        let cycle: &[u8] = b"\r\n\x1b[J\n\xe2\x9d\x84 ";
        let mut stream = Vec::new();
        for _ in 0..30 {
            stream.extend_from_slice(cycle);
        }

        // Reference: whole-stream feed in one shot.
        let mut whole = Terminal::with_scrollback(80, 24, 10_000);
        whole.feed(&stream);
        let want: Vec<String> = whole
            .visible_rows()
            .map(|r| r.iter().map(|c| c.ch).collect())
            .collect();

        // Every split point must converge to the same visible grid.
        for split in [1usize, 2, 3, 5, 7, 11, 13, 64, 128] {
            let mut t = Terminal::with_scrollback(80, 24, 10_000);
            for chunk in stream.chunks(split) {
                t.feed(chunk);
            }
            let got: Vec<String> = t
                .visible_rows()
                .map(|r| r.iter().map(|c| c.ch).collect())
                .collect();
            assert_eq!(got, want, "converged grid differs at chunk size {split}");
        }
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

    /// Regression (mado embedded-tear SGR corruption): a heavily-styled
    /// line whose CSI intro is SPLIT across two `feed()` calls — as
    /// tear-core's per-read chunking produces — must render identically
    /// to a whole-stream feed; the SGR params must reach vte, never be
    /// printed as literal text. The removed P33 fast path left a stale
    /// `vte_in_ground` flag after a chunk ended mid-CSI, so the next
    /// chunk's printable-looking params (`;2;215;…m1m`) were written as
    /// cells → `❯ ;2;Yes,1I3trust1this folder`.
    #[test]
    fn split_csi_across_feeds_does_not_leak_sgr_params_as_text() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        // Chunk 1 ends mid-CSI: a complete reset (fires csi_dispatch,
        // which used to set the stale flag) + the start of a truecolor
        // intro that ends exactly at the chunk boundary.
        term.feed(b"\xe2\x9d\xaf \x1b[0m\x1b[38");
        // Chunk 2: the rest of the truecolor intro + bold + menu text.
        term.feed(b";2;215;215;215;1m1. \x1b[22mYes, I trust this folder");
        let row: String = (0..term.cols()).map(|c| term.cell(0, c).ch).collect();
        assert_eq!(
            row.trim_end(),
            "\u{276f} 1. Yes, I trust this folder",
            "SGR params leaked into the rendered row: {:?}",
            row.trim_end()
        );
    }

    /// Superseded contract (2026-06-11): newline output used to
    /// re-pin the viewport to the bottom — that "fix" was the root of
    /// the "output steals my scroll position" report (ANY streaming
    /// output yanked the reader down). The corrected model: output
    /// pins the view to CONTENT (`output_while_scrolled_keeps_view_
    /// pinned_to_content`); the operator's own keystrokes snap to the
    /// live tail at the INPUT layer (`ux::engine` write_key_input),
    /// which also covers the original "Enter doesn't follow" report.
    #[test]
    fn output_newline_grows_offset_keeping_content_pinned() {
        let mut term = Terminal::with_scrollback(80, 4, 100);
        for _ in 0..20 {
            term.feed(b"line\r\n");
        }
        term.scroll_up(5);
        assert_eq!(term.scroll_offset(), 5, "precondition: scrolled into history");
        term.feed(b"\n"); // output → view stays pinned to content
        assert_eq!(
            term.scroll_offset(),
            6,
            "output grows the offset so the read content stays in view"
        );
    }

    #[test]
    fn scrollback_on_overflow() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"LINE1\r\n");
        term.feed(b"LINE2\r\n");
        term.feed(b"LINE3\r\n");
        assert!(term.primary.scrollback_len() >= 1);
    }

    /// **Invariant: the scrollback ring never exceeds its cap**
    /// (pinned 2026-06-11, COMPETITIVE.md §4 "pinned-but-holed").
    /// The eviction loop in `Grid::scroll_region_up` admits in-code
    /// it was never tested — this feeds 10× the cap and asserts the
    /// bound holds after EVERY scroll, plus that eviction drops the
    /// OLDEST rows (front of the ring), not the newest.
    #[test]
    fn scrollback_ring_never_exceeds_cap() {
        let cap = 16;
        let mut term = Terminal::with_scrollback(20, 4, cap);
        let mut failures: Vec<String> = Vec::new();
        for i in 0..(cap * 10) {
            term.feed(format!("line{i}\r\n").as_bytes());
            let len = term.primary.scrollback_len();
            if len > cap {
                failures.push(format!("after line{i}: scrollback {len} > cap {cap}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} cap violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
        // Ring is saturated exactly at the cap (we fed far more than
        // cap + visible lines), and the survivor at the front is a
        // LATE line — line0 was evicted first.
        assert_eq!(term.primary.scrollback_len(), cap);
        let front: String = term.primary.rows[0]
            .iter()
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string();
        assert!(
            front.starts_with("line") && front != "line0",
            "front of ring should be a late line (oldest evicted), got {front:?}"
        );
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

    /// **Invariant: overwriting a wide char's LEAD cell clears the
    /// continuation cell** (pinned 2026-06-11, COMPETITIVE.md §4
    /// "wide-char overwrite orphaning"). Without the clear, the
    /// width-0 continuation survives with no lead — the renderer
    /// skips width-0 cells, so the stale half renders as a ghost
    /// column that selection/extract_text still walks.
    #[test]
    fn overwriting_wide_char_lead_clears_continuation_cell() {
        let mut term = Terminal::new(10, 3);
        term.feed("漢".as_bytes());
        // Sanity: lead at (0,0), continuation at (0,1).
        assert_eq!(term.cell(0, 0).width, 2);
        assert_eq!(term.cell(0, 1).width, 0);
        // CUP back onto the lead cell, overwrite with a narrow char.
        term.feed(b"\x1b[1;1HX");
        assert_eq!(term.cell(0, 0).ch, 'X');
        assert_eq!(term.cell(0, 0).width, 1);
        // The partner continuation cell must be cleared, not left
        // as a leadless width-0 orphan.
        assert_eq!(term.cell(0, 1).width, 1, "continuation cell orphaned");
        assert_eq!(term.cell(0, 1).ch, ' ', "continuation cell orphaned");
    }

    /// **Invariant: overwriting a wide char's CONTINUATION cell
    /// clears the lead cell** (pinned 2026-06-11, COMPETITIVE.md §4).
    /// Without the clear, the width-2 lead survives and the renderer
    /// draws half a CJK glyph underneath the newly written char.
    #[test]
    fn overwriting_wide_char_continuation_clears_lead_cell() {
        let mut term = Terminal::new(10, 3);
        term.feed("漢".as_bytes());
        assert_eq!(term.cell(0, 0).width, 2);
        assert_eq!(term.cell(0, 1).width, 0);
        // CUP onto the continuation cell, overwrite with a narrow char.
        term.feed(b"\x1b[1;2HX");
        assert_eq!(term.cell(0, 1).ch, 'X');
        assert_eq!(term.cell(0, 1).width, 1);
        // The lead half must be cleared too.
        assert_eq!(term.cell(0, 0).width, 1, "lead cell orphaned");
        assert_eq!(term.cell(0, 0).ch, ' ', "lead cell orphaned");
    }

    /// **Invariant: a wide write whose continuation lands on another
    /// wide char's LEAD clears that glyph's continuation** — the
    /// second-order orphan case: `漢漢` then overwrite at col 1 with
    /// a new wide char covers cols 1–2; col 2 was the second 漢's
    /// lead, so its continuation at col 3 must clear.
    #[test]
    fn wide_overwrite_covering_second_lead_clears_its_continuation() {
        let mut term = Terminal::new(10, 3);
        term.feed("漢漢".as_bytes()); // leads at cols 0 and 2
        assert_eq!(term.cell(0, 2).width, 2);
        assert_eq!(term.cell(0, 3).width, 0);
        // CUP to col 2 (the continuation of the first 漢), write 中:
        // covers cols 1–2, orphaning BOTH the first lead (col 0) and
        // the second glyph's continuation (col 3).
        term.feed("\x1b[1;2H中".as_bytes());
        assert_eq!(term.cell(0, 1).ch, '中');
        assert_eq!(term.cell(0, 1).width, 2);
        assert_eq!(term.cell(0, 0).width, 1, "first lead orphaned");
        assert_eq!(term.cell(0, 0).ch, ' ', "first lead orphaned");
        assert_eq!(term.cell(0, 3).width, 1, "second continuation orphaned");
        assert_eq!(term.cell(0, 3).ch, ' ', "second continuation orphaned");
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

    /// XTWINOPS `CSI 18 t` — report text-area size in characters. The
    /// answer is `CSI 8 ; <rows> ; <cols> t`. Modern TUIs (vim/nvim
    /// powerline tablines) probe this to lay out their UI; a terminal
    /// that ignores it can make the TUI withhold rows (missing-tabline).
    #[test]
    fn xtwinops_18_reports_text_area_char_size() {
        // Terminal::new(cols, rows) → here cols=80, rows=24.
        let mut term = Terminal::with_scrollback(80, 24, 100);
        term.feed(b"\x1b[18t");
        let response = term.take_response().unwrap();
        assert_eq!(response.as_slice(), &b"\x1b[8;24;80t"[..]);
        assert!(term.take_response().is_none());
    }

    /// The report tracks the live grid size after a resize.
    #[test]
    fn xtwinops_18_tracks_resize() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        term.resize(120, 40);
        term.feed(b"\x1b[18t");
        let response = term.take_response().unwrap();
        assert_eq!(response.as_slice(), &b"\x1b[8;40;120t"[..]);
    }

    /// Unrecognised XTWINOPS ops (resize/move/raise/iconify, pixel-size
    /// reports we don't yet wire) are silently ignored — never answered
    /// with a malformed or guessed reply.
    #[test]
    fn xtwinops_other_ops_emit_no_response() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        // CSI 14 t (pixel size) — not answered at the grid layer.
        term.feed(b"\x1b[14t");
        assert!(term.take_response().is_none());
        // CSI 8 ; 30 ; 100 t (resize request) — mado is the size
        // authority; ignored, no response.
        term.feed(b"\x1b[8;30;100t");
        assert!(term.take_response().is_none());
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
            term.cell(0, 0).hyperlink.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            term.cell(0, 3).hyperlink.as_deref(),
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
    fn block_separator_viewport_rows_returns_visible_start_marks() {
        let mut term = Terminal::new(80, 24);
        // No marks → empty separators.
        assert!(term.block_separator_viewport_rows().is_empty());

        // Two prompt-start marks at row 0 and row 2 (advance
        // cursor between them).
        term.feed(b"\x1b]133;A\x1b\\");
        term.feed(b"$ ls\r\n");          // row 0 → row 1
        term.feed(b"file1 file2\r\n");   // row 1 → row 2
        term.feed(b"\x1b]133;A\x1b\\");

        let seps = term.block_separator_viewport_rows();
        assert_eq!(seps.len(), 2, "expected two viewport-visible separators, got {seps:?}");
        // First mark at the top (row 0), second after the 2
        // lines of output (row 2).
        assert!(seps.contains(&0), "first separator should land at row 0: {seps:?}");
        assert!(seps.contains(&2), "second separator should land at row 2: {seps:?}");
    }

    #[test]
    fn block_separator_viewport_rows_is_empty_on_alt_screen() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]133;A\x1b\\");
        // Enter alt screen (DEC mode 1049).
        term.feed(b"\x1b[?1049h");
        assert!(term.block_separator_viewport_rows().is_empty());
    }

    #[test]
    fn osc_133_records_typed_history_across_lifecycle() {
        // Each OSC 133 letter lands as a [`PromptKind`] in the
        // typed history — the jump API reads back only the Start
        // marks, but A/B/C/D all go into the record.
        let mut term = Terminal::new(80, 24);
        assert!(term.prompt_marks().is_empty());

        term.feed(b"\x1b]133;A\x1b\\");
        term.feed(b"\x1b]133;B\x1b\\");
        term.feed(b"\x1b]133;C\x1b\\");
        term.feed(b"\x1b]133;D\x1b\\");
        assert_eq!(term.prompt_marks().len(), 4);

        // Unknown letter is ignored — no mark recorded.
        term.feed(b"\x1b]133;Z\x1b\\");
        assert_eq!(term.prompt_marks().len(), 4);
    }

    #[test]
    fn osc_133_skipped_on_alternate_screen() {
        // Shells never emit OSC 133 from inside the alt screen —
        // if something malicious does, we silently drop it rather
        // than polluting the jump history with rows that don't
        // mean anything.
        let mut term = Terminal::new(80, 24);
        // DECSET 1049 — switch to alt screen.
        term.feed(b"\x1b[?1049h");
        term.feed(b"\x1b]133;A\x1b\\");
        assert!(term.prompt_marks().is_empty());
    }

    #[test]
    fn scroll_offset_to_prev_prompt_walks_backwards() {
        let mut term = Terminal::new(80, 24);
        // Emit a prompt on the first line, then scroll the grid up
        // enough that the prompt is in scrollback.
        term.feed(b"\x1b]133;A\x1b\\");
        // Fill the rest of the screen + some scrollback by newlines.
        for _ in 0..50 {
            term.feed(b"\n");
        }
        // Emit a second prompt. Now there's a prev prompt ~50 rows up.
        term.feed(b"\x1b]133;A\x1b\\");
        // No scroll yet — calling prev should resolve to the earlier mark.
        let off = term.scroll_offset_to_prev_prompt();
        assert!(off.is_some());
        let off = off.unwrap();
        // Two marks, so prev from cursor must scroll non-zero.
        assert!(off > 0, "expected non-zero scroll offset, got {off}");
    }

    #[test]
    fn prev_prompt_none_when_no_marks_recorded() {
        let term = Terminal::new(80, 24);
        assert!(term.scroll_offset_to_prev_prompt().is_none());
        assert!(term.scroll_offset_to_next_prompt().is_none());
    }

    #[test]
    fn osc_22_set_pointer_shape_updates_typed_state() {
        use crate::pointer_shape::PointerShape;
        let mut term = Terminal::new(80, 24);
        assert_eq!(term.pointer_shape(), PointerShape::Default);

        // Set to `text` (editor caret).
        term.feed(b"\x1b]22;text\x1b\\");
        assert_eq!(term.pointer_shape(), PointerShape::Text);

        // Set to `pointer` (clickable).
        term.feed(b"\x1b]22;pointer\x1b\\");
        assert_eq!(term.pointer_shape(), PointerShape::Pointer);

        // Set to a hyphenated name.
        term.feed(b"\x1b]22;col-resize\x1b\\");
        assert_eq!(term.pointer_shape(), PointerShape::ColResize);
    }

    #[test]
    fn osc_22_unknown_shape_is_silently_dropped() {
        use crate::pointer_shape::PointerShape;
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]22;text\x1b\\");
        assert_eq!(term.pointer_shape(), PointerShape::Text);

        // An unknown name from a future protocol revision must
        // leave the existing state intact, not fall back to Default.
        term.feed(b"\x1b]22;laser\x1b\\");
        assert_eq!(term.pointer_shape(), PointerShape::Text);
    }

    #[test]
    fn osc_22_query_responds_with_current_shape_name() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]22;wait\x1b\\");
        term.feed(b"\x1b]22;?\x1b\\");
        let response = term.take_response().expect("query should emit a response");
        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.starts_with("\x1b]22;wait"),
            "unexpected response: {response_str:?}",
        );
        assert!(
            response_str.ends_with("\x1b\\"),
            "response should terminate with ST (ESC \\): {response_str:?}",
        );
    }

    #[test]
    fn osc_1337_set_mark_records_cursor_row() {
        let mut term = Terminal::new(80, 24);
        assert!(term.user_marks().is_empty());

        // Emit SetMark — the current cursor row becomes a mark.
        term.feed(b"\x1b]1337;SetMark\x1b\\");
        assert_eq!(term.user_marks().len(), 1);

        // Advance cursor with newlines + another SetMark.
        term.feed(b"\n\n\n\x1b]1337;SetMark\x1b\\");
        assert_eq!(term.user_marks().len(), 2);
    }

    #[test]
    fn osc_1337_set_mark_skipped_on_alternate_screen() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[?1049h"); // alt screen
        term.feed(b"\x1b]1337;SetMark\x1b\\");
        assert!(term.user_marks().is_empty());
    }

    #[test]
    fn osc_1337_request_attention_flips_flag() {
        let mut term = Terminal::new(80, 24);
        assert!(!term.attention_requested());

        term.feed(b"\x1b]1337;RequestAttention=1\x1b\\");
        assert!(term.attention_requested());

        term.feed(b"\x1b]1337;RequestAttention=0\x1b\\");
        assert!(!term.attention_requested());

        // Truthy alternates — `true`, `yes`, bare anything other
        // than the off-vocab all request attention.
        term.feed(b"\x1b]1337;RequestAttention=true\x1b\\");
        assert!(term.attention_requested());

        term.feed(b"\x1b]1337;RequestAttention=no\x1b\\");
        assert!(!term.attention_requested());
    }

    #[test]
    fn osc_1337_unknown_key_logs_and_leaves_state_alone() {
        // CopyToClipboard / File= are real iTerm2 params mado
        // hasn't implemented. They must not corrupt user_marks or
        // attention state.
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]1337;SetMark\x1b\\");
        let pre_mark_count = term.user_marks().len();
        let pre_attention = term.attention_requested();

        term.feed(b"\x1b]1337;CopyToClipboard=abc\x1b\\");
        term.feed(b"\x1b]1337;File=base64:blah\x1b\\");

        assert_eq!(term.user_marks().len(), pre_mark_count);
        assert_eq!(term.attention_requested(), pre_attention);
    }

    #[test]
    fn osc_22_query_on_default_state_echoes_default_name() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]22;?\x1b\\");
        let response = term.take_response().unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.starts_with("\x1b]22;default"),
            "unexpected response: {response_str:?}",
        );
    }

    #[test]
    fn prompt_jump_dispatchers_share_one_geometry_helper() {
        // Both scroll_offset_to_{prev,next}_prompt must resolve to
        // scroll offsets symmetric around the current viewport-top
        // for a pair of marks equidistant around it. This pins the
        // unified helper's contract: a future inlining regression
        // (where one method drifts from the other) would fail here.
        let mut term = Terminal::new(80, 10);
        // Fill scrollback with 40 blank lines + a prompt, then 10
        // more, then another prompt. The two prompts flank the
        // viewport when we scroll to the midpoint.
        term.feed(b"\x1b]133;A\x1b\\"); // prompt @ row 0
        for _ in 0..30 {
            term.feed(b"\n");
        }
        term.feed(b"\x1b]133;A\x1b\\"); // prompt ~30 lines later
        for _ in 0..10 {
            term.feed(b"\n");
        }

        // From the live bottom view (offset 0), `prev` should land
        // on the most-recent prompt (non-zero offset). `next`
        // should be None (we're already below every mark).
        let prev = term.scroll_offset_to_prev_prompt();
        let next = term.scroll_offset_to_next_prompt();
        assert!(prev.is_some(), "prev should see the recent mark");
        assert!(next.is_none(), "next is None — nothing below the view");
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
    fn split_multibyte_char_across_feeds_renders_one_grapheme() {
        // A multi-byte UTF-8 char split at every internal byte boundary
        // across two feed() calls must render as the single correct
        // grapheme. vte 0.15's `partial_utf8` buffer survives between
        // advance() calls because feed() reuses the same Parser — this
        // pins that invariant for 3-byte (em-dash) and 4-byte (emoji)
        // codepoints.
        for s in ["—", "😀", "本"] {
            let raw = s.as_bytes();
            let expected = s.chars().next().unwrap();
            for split in 1..raw.len() {
                let mut term = Terminal::new(80, 24);
                term.feed(&raw[..split]);
                term.feed(&raw[split..]);
                assert_eq!(
                    term.cell(0, 0).ch,
                    expected,
                    "{s:?} split at byte {split} should render as {expected:?}"
                );
            }
        }
    }

    #[test]
    fn split_esc_st_across_feeds_terminates_apc_and_renders_next_char() {
        // ─── REGRESSION GUARD (Base-1 chunk-boundary crack) ──────────
        // An APC sequence whose `ESC \` ST terminator splits across two
        // feed() calls (the `ESC` ends chunk 1, the `\` begins chunk 2)
        // must still terminate the APC — otherwise the never-closed APC
        // buffer silently swallows everything after it, including the
        // following printable char. Before the pending_esc carry fix the
        // em-dash below rendered as a blank cell.
        let stream = b"\x1b_Gfoo\x1b\\\xe2\x80\x94"; // APC `Gfoo` + ST + em-dash
        let split = 7; // chunk 1 ends on the ST's ESC: [ESC _ G f o o ESC]
        let mut whole = Terminal::new(80, 24);
        whole.feed(stream);
        let mut term = Terminal::new(80, 24);
        term.feed(&stream[..split]);
        term.feed(&stream[split..]);
        assert_eq!(whole.cell(0, 0).ch, '—', "whole-feed sanity");
        assert_eq!(
            term.cell(0, 0).ch,
            '—',
            "split ESC \\ ST must terminate the APC so the em-dash renders"
        );
        // The full chunk-boundary sweep over a mix of APC + multi-byte
        // streams must be split-independent at every offset.
        let streams: &[&[u8]] = &[
            "a—😀本z".as_bytes(),
            b"x\xf0\x9f\x98\x80y",
            b"\x1b_Gfoo\x1b\\\xe2\x80\x94",
            b"\xe2\x80\x94\x1b_Gfoo\x1b\\",
        ];
        for s in streams {
            let mut w = Terminal::new(80, 24);
            w.feed(s);
            for off in 0..=s.len() {
                let mut t = Terminal::new(80, 24);
                t.feed(&s[..off]);
                t.feed(&s[off..]);
                for r in 0..2 {
                    for c in 0..10 {
                        assert_eq!(
                            w.cell(r, c).ch,
                            t.cell(r, c).ch,
                            "stream {s:?} split at {off}: cell ({r},{c}) differs"
                        );
                    }
                }
            }
        }
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

// ---------------------------------------------------------------------------
// Property tests (proptest)
//
// Invariants every VT100 / xterm core must hold under arbitrary input.
// Each property is a class of bugs we provably can't ship — if a
// regression breaks one, the proptest shrinker hands us the minimal
// failing byte sequence, and we add it as a scenario.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// **Invariant: parsing is chunk-boundary independent (7-bit).**
        ///
        /// Feeding any byte stream whole must produce the identical
        /// rendered grid as feeding it split at an arbitrary offset.
        /// This is the load-bearing property the removed P33 fast path
        /// violated: an escape sequence split across `feed()` calls must
        /// not corrupt. tear-core delivers one chunk per `read()`
        /// syscall, so mid-sequence splits are the common case.
        ///
        /// Scope: the 7-bit range (ASCII + C0 controls + ESC) — where
        /// the P33 / SGR-leak bug lived. The multi-byte UTF-8 + APC-ST
        /// split class (the Base-1 crack now fixed by [`PendingEsc`]) is
        /// guarded by the dedicated, well-formed-input properties below
        /// (`valid_multibyte_*` / `apc_st_split_*`) plus the
        /// `split_esc_st_across_feeds` / `split_multibyte_char` unit
        /// regressions. The generator is deliberately NOT widened to
        /// `any::<u8>()`: arbitrary *invalid* UTF-8 split at a boundary
        /// can legitimately differ in vte 0.15's replacement-character
        /// resync (a maximal-subpart property of the third-party
        /// streaming decoder, not mado's `feed()` layer), so a full-byte
        /// generator would assert a property the decoder does not hold.
        #[test]
        fn parsing_is_chunk_boundary_independent(
            stream in proptest::collection::vec(0x00u8..=0x7Fu8, 0..512),
            raw_split in 0usize..512,
        ) {
            let split = if stream.is_empty() { 0 } else { raw_split % (stream.len() + 1) };
            let mut whole = Terminal::new(80, 24);
            whole.feed(&stream);
            let mut split_term = Terminal::new(80, 24);
            split_term.feed(&stream[..split]);
            split_term.feed(&stream[split..]);
            for r in 0..whole.rows() {
                for c in 0..whole.cols() {
                    prop_assert_eq!(
                        whole.cell(r, c).ch, split_term.cell(r, c).ch,
                        "cell ({},{}) differs: whole={:?} split={:?} (split at {})",
                        r, c, whole.cell(r, c).ch, split_term.cell(r, c).ch, split
                    );
                }
            }
        }

        /// **Invariant: well-formed streams (ASCII + valid multi-byte
        /// UTF-8 + APC sequences) are chunk-boundary independent at the
        /// FULL byte level.**
        ///
        /// This is the property the [`PendingEsc`] carry fix makes hold:
        /// any stream built from printable ASCII chars, arbitrary VALID
        /// multi-byte codepoints, and complete `ESC _ … ESC \` APC
        /// sequences renders identically whether fed whole or split at
        /// any offset — including splits that land mid-codepoint or
        /// inside the two-byte APC introducer / ST terminator. The input
        /// is well-formed by construction (the generator emits whole
        /// codepoints + whole APC frames), so the only variable is WHERE
        /// the bytes are split — exactly the chunk-boundary axis.
        #[test]
        fn wellformed_multibyte_and_apc_streams_are_chunk_boundary_independent(
            tokens in proptest::collection::vec(
                prop_oneof![
                    // Printable ASCII char.
                    (0x41u8..=0x7Eu8).prop_map(|b| vec![b]),
                    // A valid non-ASCII codepoint (BMP + astral) as its
                    // UTF-8 bytes.
                    any::<char>().prop_filter("non-ascii", |c| !c.is_ascii())
                        .prop_map(|c| c.to_string().into_bytes()),
                    // A complete APC frame: ESC _ <payload> ESC \.
                    proptest::collection::vec(0x41u8..=0x7Eu8, 0..6)
                        .prop_map(|p| {
                            let mut v = vec![0x1b, b'_'];
                            v.extend_from_slice(&p);
                            v.extend_from_slice(&[0x1b, b'\\']);
                            v
                        }),
                ],
                0..40,
            ),
            raw_split in 0usize..2048,
        ) {
            let stream: Vec<u8> = tokens.into_iter().flatten().collect();
            let split = if stream.is_empty() { 0 } else { raw_split % (stream.len() + 1) };
            let mut whole = Terminal::new(80, 24);
            whole.feed(&stream);
            let mut split_term = Terminal::new(80, 24);
            split_term.feed(&stream[..split]);
            split_term.feed(&stream[split..]);
            for r in 0..whole.rows() {
                for c in 0..whole.cols() {
                    prop_assert_eq!(
                        whole.cell(r, c).ch, split_term.cell(r, c).ch,
                        "cell ({},{}) differs: whole={:?} split={:?} (split at {})",
                        r, c, whole.cell(r, c).ch, split_term.cell(r, c).ch, split
                    );
                }
            }
        }

        /// **Invariant: parser never panics, cursor stays in bounds.**
        ///
        /// Random byte streams of up to 1 KiB are fed to a fresh
        /// terminal. After every feed, the cursor must be inside the
        /// grid bounds. If proptest ever finds a panic or
        /// out-of-bounds cursor, the shrunk input becomes the next
        /// scenario.
        #[test]
        fn random_byte_stream_never_panics_and_keeps_cursor_in_bounds(
            input in proptest::collection::vec(any::<u8>(), 0..1024)
        ) {
            let mut term = Terminal::new(80, 24);
            term.feed(&input);
            let cur = term.cursor();
            // Cursor may sit at `cols` exactly (pending-wrap state)
            // but must not exceed it; rows must stay inside the grid.
            prop_assert!(cur.col <= term.cols(),
                "cursor.col={} > cols={}", cur.col, term.cols());
            prop_assert!(cur.row < term.rows(),
                "cursor.row={} >= rows={}", cur.row, term.rows());
        }

        /// **Invariant: resize preserves grid consistency.**
        ///
        /// After resizing to any (cols, rows) in [1, 200] × [1, 100],
        /// the reported dimensions match exactly and the cursor sits
        /// inside the new bounds.
        #[test]
        fn resize_preserves_dimensions_and_cursor_bounds(
            cols in 1u16..200,
            rows in 1u16..100,
        ) {
            let mut term = Terminal::new(80, 24);
            term.feed(b"hello world\nsecond line\n");
            term.resize(cols as usize, rows as usize);
            prop_assert_eq!(term.cols(), cols as usize);
            prop_assert_eq!(term.rows(), rows as usize);
            prop_assert!(term.cursor().col <= term.cols());
            prop_assert!(term.cursor().row < term.rows());
        }

        /// **Invariant: ASCII printable strings advance cursor by their
        /// byte length (mod wrap).**
        ///
        /// For any printable-ASCII string shorter than one row, after
        /// feeding it from a fresh terminal the cursor either sits at
        /// `(0, len)` or — for strings of exactly `cols` — at the
        /// pending-wrap edge `(0, cols)`. Wider-than-cols strings are
        /// excluded so this stays a clean linear invariant.
        #[test]
        fn ascii_print_advances_cursor_linearly(
            s in proptest::string::string_regex("[A-Za-z0-9 ]{0,79}").unwrap()
        ) {
            let mut term = Terminal::new(80, 24);
            term.feed(s.as_bytes());
            let cur = term.cursor();
            prop_assert_eq!(cur.row, 0);
            prop_assert_eq!(cur.col, s.len(),
                "want col={}, got col={}, str={:?}", s.len(), cur.col, s);
        }

        /// **Invariant: selection extraction is control-byte free.**
        ///
        /// COMPETITIVE.md §4 "Selection sanitization" (was untested).
        /// Sibling of the 2026-06-11 skim-CPR incident class: control
        /// bytes leaking into a selection pipe corrupted the consumer
        /// downstream (that fix landed in frost/skim-tab; this pins
        /// mado's own surface). Under ARBITRARY byte feeds — including
        /// raw ESC, C0 controls, and broken UTF-8 — a select-all
        /// `Selection::extract_text` over the visible grid must never
        /// emit ESC (0x1b) or any C0 control byte other than the `\n`
        /// row separator extract_text itself inserts. The VT engine
        /// must execute/discard controls, never store them in cells.
        #[test]
        fn selection_extract_text_never_leaks_control_bytes(
            input in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            use crate::selection::{CellPos, Selection};
            let mut term = Terminal::new(80, 24);
            term.feed(&input);
            let rows: Vec<Vec<Cell>> =
                term.visible_rows().map(<[Cell]>::to_vec).collect();
            let cols = term.cols();
            let mut sel = Selection::new();
            sel.start(CellPos { row: 0, col: 0 });
            sel.update(CellPos { row: rows.len() - 1, col: cols - 1 });
            sel.finish();
            if let Some(text) = sel.extract_text(&rows, cols) {
                for (i, b) in text.bytes().enumerate() {
                    prop_assert!(
                        b == b'\n' || b >= 0x20,
                        "control byte 0x{b:02x} at offset {i} leaked into \
                         extracted selection (ESC/C0 other than \\n are \
                         forbidden): {text:?}"
                    );
                }
            }
        }

        /// **Invariant: wide-char cells consume exactly 2 columns.**
        ///
        /// For any Hiragana string (each char is East-Asian Wide),
        /// feeding `n` chars advances the cursor by `2n` (mod wrap)
        /// and every emitted glyph's cell has `width == 2`.
        #[test]
        fn wide_chars_consume_two_columns(
            n in 0usize..30,
        ) {
            // Hiragana あ U+3042 is East-Asian-Wide — 2 columns per
            // glyph per Unicode East Asian Width.
            let s: String = std::iter::repeat_n('あ', n).collect();
            let mut term = Terminal::new(80, 24);
            term.feed(s.as_bytes());
            prop_assert_eq!(term.cursor().row, 0);
            prop_assert_eq!(term.cursor().col, 2 * n);
            for i in 0..n {
                let cell = term.cell(0, 2 * i);
                prop_assert_eq!(cell.ch, 'あ');
                prop_assert_eq!(cell.width, 2);
                // continuation cell width=0
                let cont = term.cell(0, 2 * i + 1);
                prop_assert_eq!(cont.width, 0);
            }
        }
    }
}
