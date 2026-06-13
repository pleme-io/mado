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
use crate::ux::side_effects::{PendingNotification, ProgressState, Urgency};

/// Default for the Terminal's injectable clock seam — real UNIX wall
/// clock in milliseconds. A pre-epoch system clock degrades to 0
/// rather than panicking inside the feed path.
fn wall_clock_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Re-join OSC payload params that vte split on `;` — the payload of
/// OSC 9 / 777 / 99 is free text where `;` is data, not structure.
/// Lossy UTF-8 per the established OSC text handling in this module.
fn join_osc_params(params: &[&[u8]]) -> String {
    let mut out = String::new();
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        out.push_str(&String::from_utf8_lossy(p));
    }
    out
}

/// In-flight kitty OSC 99 multi-part notification (`d=0` chain).
/// Fragments accumulate per payload kind; `d=1` finalizes into one
/// [`PendingNotification`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Osc99Pending {
    /// `i=<id>` — chain identity; fragments with a different id drop
    /// the chain (traced, never silent).
    id: Option<String>,
    /// Accumulated `p=title` payload fragments.
    title: Option<String>,
    /// Accumulated `p=body` payload fragments.
    body: Option<String>,
    /// Urgency from the chain's metadata — `None` until a fragment
    /// carries `u=` (an explicit `u=0` Low must be distinguishable
    /// from the unset default); multiple fragments merge highest-wins.
    urgency: Option<Urgency>,
}

impl Osc99Pending {
    /// Finalize the chain into the typed queue entry. kitty's default
    /// payload kind is `title`, so a chain may legitimately carry a
    /// title and no body — body degrades to empty, mirroring OSC 9's
    /// body-only inverse.
    fn into_notification(self) -> PendingNotification {
        PendingNotification {
            title: self.title,
            body: self.body.unwrap_or_default(),
            urgency: self.urgency.unwrap_or_default(),
            group: self.id,
        }
    }
}

// ---------------------------------------------------------------------------
// Cell attributes (bitflags-style)
// ---------------------------------------------------------------------------

/// FROZEN legacy u8 attribute bit layout — the MCP `CellSnapshot.attrs`
/// / scenario `attrs:` wire surface. Since M2 the live attribute storage
/// is the wide typed [`Attrs`] (inside the interned [`Style`]); this
/// type exists ONLY to name the historical bit positions that
/// [`Attrs::to_legacy_bits`] projects onto. Never grow it — new
/// attribute axes go on [`Attrs`] and surface as typed snapshot fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CellAttrs(u8);

#[allow(dead_code)] // The constants anchor the wire layout; tests use the rest.
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

    /// Raw bitfield. The bit positions match the BOLD/ITALIC/
    /// UNDERLINE/BLINK/INVERSE/STRIKETHROUGH/DIM/HIDDEN constants
    /// above, in that order.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Wide typed attributes (M2 — live INSIDE the interned Style, so widening
// costs 0 bytes per Cell; the legacy CellAttrs u8 above remains ONLY as the
// MCP/scenario wire bit-layout, produced via Attrs::to_legacy_bits)
// ---------------------------------------------------------------------------

/// Boolean attribute flags — u16 bitset. The non-underline half of the
/// old `CellAttrs` plus OVERLINE (SGR 53), which the u8 had no room for.
/// Underline is NOT a flag here: its style is the typed
/// [`UnderlineStyle`] on [`Attrs`] (SGR 4 / 4:N / 21 sub-param wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct AttrFlags(u16);

impl AttrFlags {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const INVERSE: Self = Self(1 << 2);
    pub const DIM: Self = Self(1 << 3);
    pub const HIDDEN: Self = Self(1 << 4);
    pub const STRIKETHROUGH: Self = Self(1 << 5);
    pub const OVERLINE: Self = Self(1 << 6);
    pub const BLINK: Self = Self(1 << 7);

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
    #[allow(dead_code)] // Test/consumer surface — parity with the old CellAttrs API.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Mechanical `(flag, SGR set-code)` registry — co-located with
    /// the bit consts so ONE list owns flag identity + wire code.
    /// `SgrReport` (DECRQSS `m`) iterates THIS, and the round-trip
    /// test exercises every row, so a new flag lands with its wire
    /// code + DECRQSS coverage in the same change (M3 review
    /// 2026-06-12: the former `FLAG_PARAMS` local was a hand dual of
    /// the const set — a ninth flag would have been silently omitted
    /// from DECRQSS replies).
    pub const ALL: [(Self, &'static str); 8] = [
        (Self::BOLD, "1"),
        (Self::DIM, "2"),
        (Self::ITALIC, "3"),
        (Self::BLINK, "5"),
        (Self::INVERSE, "7"),
        (Self::HIDDEN, "8"),
        (Self::STRIKETHROUGH, "9"),
        (Self::OVERLINE, "53"),
    ];
}

// M3-C2 — engawa owns the ONE definition of the underline vocabulary.
// The mado-local enums these re-exports replaced were deleted (not
// mirrored): variant set/order, derive set, `as_str` wire names, and
// the `Display` impls were matched upstream byte-for-byte, so the MCP
// `CellSnapshot.underline`/`underline_color` wire is unchanged.
// `UnderlineColor::Rgb` carries [`engawa::Rgb`] (field-compatible with
// [`Color`]); the SGR-58 parse sites construct `Rgb::new(..)`.
pub use engawa::{Rgb, UnderlineColor, UnderlineStyle};

/// Wide typed cell attributes — flags + typed underline style + typed
/// underline colour. Lives INSIDE the interned [`Style`] (per-Cell cost
/// is the existing `style_id: u16`, not per-cell bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Attrs {
    pub flags: AttrFlags,
    pub underline: UnderlineStyle,
    pub underline_color: UnderlineColor,
}

impl Attrs {
    pub const NONE: Self = Self {
        flags: AttrFlags::NONE,
        underline: UnderlineStyle::None,
        underline_color: UnderlineColor::Default,
    };

    /// Project the wide attrs down to the legacy [`CellAttrs`] u8 bit
    /// layout (the MCP `CellSnapshot.attrs` / scenario `attrs:` wire
    /// surface). Any non-None underline style maps to the single
    /// legacy UNDERLINE bit; OVERLINE and the underline colour have no
    /// u8 representation and are carried by the new snapshot fields.
    #[must_use]
    pub const fn to_legacy_bits(self) -> u8 {
        let mut bits = 0u8;
        if self.flags.contains(AttrFlags::BOLD) {
            bits |= CellAttrs::BOLD.0;
        }
        if self.flags.contains(AttrFlags::ITALIC) {
            bits |= CellAttrs::ITALIC.0;
        }
        if !matches!(self.underline, UnderlineStyle::None) {
            bits |= CellAttrs::UNDERLINE.0;
        }
        if self.flags.contains(AttrFlags::BLINK) {
            bits |= CellAttrs::BLINK.0;
        }
        if self.flags.contains(AttrFlags::INVERSE) {
            bits |= CellAttrs::INVERSE.0;
        }
        if self.flags.contains(AttrFlags::STRIKETHROUGH) {
            bits |= CellAttrs::STRIKETHROUGH.0;
        }
        if self.flags.contains(AttrFlags::DIM) {
            bits |= CellAttrs::DIM.0;
        }
        if self.flags.contains(AttrFlags::HIDDEN) {
            bits |= CellAttrs::HIDDEN.0;
        }
        bits
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

/// Build the full default 256-entry xterm palette: 16 base ANSI
/// colours + the 6×6×6 colour cube (16..=231) + the 24-step grayscale
/// ramp (232..=255). Generated programmatically — the cube/ramp
/// formulas are the standard xterm ones (the same maths the old
/// `ansi_256_color` computed on the fly before OSC 4 made indices
/// 16..=255 mutable in M2).
#[must_use]
pub fn default_palette_256() -> [Color; 256] {
    let mut palette = [Color::BLACK; 256];
    palette[..8].copy_from_slice(&ANSI_COLORS);
    palette[8..16].copy_from_slice(&ANSI_BRIGHT_COLORS);
    for idx in 16u16..=231 {
        let i = idx - 16;
        let r_idx = i / 36;
        let g_idx = (i % 36) / 6;
        let b_idx = i % 6;
        let to_byte = |v: u16| -> u8 {
            if v == 0 { 0 } else { (55 + 40 * v) as u8 }
        };
        palette[idx as usize] = Color::new(to_byte(r_idx), to_byte(g_idx), to_byte(b_idx));
    }
    for idx in 232u16..=255 {
        let v = (8 + 10 * (idx - 232)) as u8;
        palette[idx as usize] = Color::new(v, v, v);
    }
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

/// Resolve a 256-colour index against the live palette. Since M2 the
/// terminal's palette carries all 256 entries (OSC 4 can override any
/// of them), so this is a bounds-checked index — the cube/grayscale
/// formulas live in [`default_palette_256`].
fn ansi_256_color(idx: u16, palette: &[Color; 256]) -> Color {
    palette.get(idx as usize).copied().unwrap_or(Color::WHITE)
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

/// The shrunk M2 cell — 24 bytes (down from 40 pre-shrink). All
/// styling resolves through the owner's [`StyleTable`] via `style_id`
/// (P32 interning made the shrink prerequisite-complete); the
/// hyperlink resolves through the owner's [`LinkTable`] via `link_id`.
/// Both tables are owned by [`Terminal`] (next to each other) and
/// exposed via [`Terminal::styles`] / [`Terminal::links`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Extra codepoints for combining characters. None for the common case.
    pub extra: Option<Box<Vec<char>>>,
    /// Display width: 1 = normal, 2 = wide (CJK), 0 = continuation of wide char.
    pub width: u8,
    /// Interned style ID into the owning Terminal's [`StyleTable`].
    /// Adjacent cells with identical (fg, bg, attrs) share a u16 tag.
    ///
    /// `0` ([`DEFAULT_STYLE_ID`]) is reserved for the default style
    /// (Color::WHITE on Color::BLACK, no attrs) so a fresh
    /// Cell::default never has to touch the table.
    pub style_id: u16,
    /// Interned hyperlink ID into the owning Terminal's [`LinkTable`]
    /// (from OSC 8). `0` ([`NO_LINK_ID`]) = no hyperlink.
    pub link_id: u16,
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

    /// Resolved style triple — one table lookup. Hot paths (render
    /// snapshot walk) use this once per cell; the per-field accessors
    /// below are the convenience surface for single-field reads.
    /// Polymorphic over [`StyleLookup`] so read paths work against
    /// the live [`StyleTable`] AND the renderer's per-frame
    /// [`StyleSnapshot`] without conversion.
    #[must_use]
    pub fn style(&self, styles: &impl StyleLookup) -> Style {
        styles.lookup(self.style_id)
    }

    /// Foreground colour, resolved through the owning [`StyleTable`].
    #[must_use]
    #[allow(dead_code)] // Per-field accessor surface — tests + future consumers.
    pub fn fg(&self, styles: &impl StyleLookup) -> Color {
        self.style(styles).fg
    }

    /// Background colour, resolved through the owning [`StyleTable`].
    #[must_use]
    #[allow(dead_code)] // Per-field accessor surface — tests + future consumers.
    pub fn bg(&self, styles: &impl StyleLookup) -> Color {
        self.style(styles).bg
    }

    /// Wide typed attributes, resolved through the owning [`StyleTable`].
    #[must_use]
    #[allow(dead_code)] // Per-field accessor surface — tests + future consumers.
    pub fn attrs(&self, styles: &impl StyleLookup) -> Attrs {
        self.style(styles).attrs
    }

    /// Hyperlink URI, resolved through the owning [`LinkTable`].
    #[must_use]
    #[allow(dead_code)] // Read surface for the upcoming link-hover/click path.
    pub fn hyperlink<'a>(&self, links: &'a LinkTable) -> Option<&'a str> {
        links.lookup(self.link_id)
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            extra: None,
            width: 1,
            // style_id 0 == DEFAULT_STYLE_ID (reserved for the canonical
            // WHITE-on-BLACK no-attrs style). Cell::default never has to
            // touch the StyleTable.
            style_id: DEFAULT_STYLE_ID,
            link_id: NO_LINK_ID,
        }
    }
}

/// Reserved style ID for the canonical default style
/// (Color::WHITE fg, Color::BLACK bg, CellAttrs::NONE). StyleTable's
/// constructor pre-populates this entry so it's always valid.
pub const DEFAULT_STYLE_ID: u16 = 0;

/// Style (fg, bg, attrs) interned as a single value. P32 + M2. The
/// styling axes that define how a Cell renders. After the M2 Cell
/// shrink, the interned `style_id: u16` on [`Cell`] is the ONLY
/// styling storage — every read resolves through [`StyleTable`].
/// `attrs` is the wide typed [`Attrs`] (flags + underline style +
/// underline colour), so widening attributes costs 0 per-Cell bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

/// Interning table mapping `Style` ↔ `u16` ID. Each Terminal owns
/// one. `intern(style)` returns the existing ID or allocates a new
/// one; `lookup(id)` resolves an ID back to the Style. Capacity is
/// bounded at `u16::MAX - 1` styles (more than enough for any
/// realistic terminal session — typical sessions have &lt;50 unique
/// styles).
///
/// **Overflow policy (M2 — the table is load-bearing post-shrink).**
/// The shrink removed the inline fg/bg/attrs from Cell, so silent
/// saturation to `DEFAULT_STYLE_ID` would now ALIAS new styles to
/// white-on-black. Instead:
///   1. [`StyleTable::try_intern`] returns `None` on a full table —
///      the Terminal reacts by garbage-collecting ([`StyleTable::gc`])
///      against the set of style ids still referenced by live cells
///      (both grids + scrollback), remapping every cell, and retrying.
///   2. If the table is STILL full after gc (pathological: more live
///      styles than `u16::MAX - 1`), [`StyleTable::intern`] falls back
///      to the most recently interned id — a *near* style from the
///      same output burst, never the DEFAULT — and `tracing::warn`s
///      once per table lifetime.
#[derive(Debug, Clone)]
pub struct StyleTable {
    styles: Vec<Style>,
    by_style: std::collections::HashMap<Style, u16>,
    /// Warn-once latch for the saturation fallback (policy step 2).
    saturation_warned: bool,
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
            attrs: Attrs::NONE,
        };
        let mut by_style = std::collections::HashMap::new();
        by_style.insert(default, DEFAULT_STYLE_ID);
        Self {
            styles: vec![default],
            by_style,
            saturation_warned: false,
        }
    }

    /// True when no further distinct style can be allocated.
    #[must_use]
    #[allow(dead_code)] // Overflow-policy API surface; exercised by tests.
    pub fn is_full(&self) -> bool {
        self.styles.len() >= u16::MAX as usize
    }

    /// Intern a style: return the existing ID or allocate a new one.
    /// Returns `None` when the table is full AND the style is not
    /// already present — the caller's signal to gc + retry.
    pub fn try_intern(&mut self, style: Style) -> Option<u16> {
        if let Some(&id) = self.by_style.get(&style) {
            return Some(id);
        }
        let id = self.styles.len();
        if id >= u16::MAX as usize {
            return None;
        }
        let id = id as u16;
        self.styles.push(style);
        self.by_style.insert(style, id);
        Some(id)
    }

    /// Intern with the saturation fallback (overflow-policy step 2):
    /// on a full table the LAST interned id is returned — a nearby
    /// style from the same output burst — never `DEFAULT_STYLE_ID`,
    /// and a `tracing::warn` fires once per table lifetime.
    pub fn intern(&mut self, style: Style) -> u16 {
        if let Some(id) = self.try_intern(style) {
            return id;
        }
        if !self.saturation_warned {
            self.saturation_warned = true;
            tracing::warn!(
                capacity = u16::MAX as usize - 1,
                "StyleTable saturated after gc — aliasing to the most \
                 recently interned style (NOT the default)"
            );
        }
        (self.styles.len() - 1) as u16
    }

    /// Rebuild the table keeping only `live` ids (plus the default
    /// entry). Returns the old-id → new-id remap the owner applies to
    /// every live cell's `style_id`. Ids absent from the remap were
    /// not in `live` and must no longer be referenced.
    pub fn gc(
        &mut self,
        live: &std::collections::HashSet<u16>,
    ) -> std::collections::HashMap<u16, u16> {
        let old_styles = std::mem::take(&mut self.styles);
        self.by_style.clear();
        self.saturation_warned = false;

        // Re-seed the default entry at id 0.
        let default = old_styles[DEFAULT_STYLE_ID as usize];
        self.styles.push(default);
        self.by_style.insert(default, DEFAULT_STYLE_ID);

        let mut remap = std::collections::HashMap::with_capacity(live.len() + 1);
        remap.insert(DEFAULT_STYLE_ID, DEFAULT_STYLE_ID);

        // Deterministic rebuild order (sorted old ids) so two gc runs
        // over the same live set produce identical tables.
        let mut ids: Vec<u16> = live.iter().copied().collect();
        ids.sort_unstable();
        for old_id in ids {
            if old_id == DEFAULT_STYLE_ID {
                continue;
            }
            let Some(style) = old_styles.get(old_id as usize).copied() else {
                continue;
            };
            // Re-intern (dedups styles that collapsed to the same
            // value). The rebuilt table holds ≤ live.len() entries,
            // so try_intern cannot fail here unless live itself
            // exceeds capacity — in which case intern's fallback
            // policy applies.
            let new_id = self.intern(style);
            remap.insert(old_id, new_id);
        }
        remap
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

    /// Lightweight read-only capture for the per-frame render path:
    /// JUST the id → [`Style`] vector. The `by_style` intern index
    /// (the expensive half of a full clone — per-entry heap layout,
    /// SwissTable control bytes, load-factor slack) is producer-side
    /// state the snapshot consumer never reads, and a style-heavy
    /// stream can park the table near its 65,535-entry cap until the
    /// NEXT saturation gc — so a full clone per frame ratchets up
    /// and never comes back down.
    #[must_use]
    pub fn snapshot(&self) -> StyleSnapshot {
        StyleSnapshot {
            styles: self.styles.clone(),
        }
    }
}

/// Read-only resolution surface shared by the live [`StyleTable`]
/// and the per-frame [`StyleSnapshot`] — [`Cell::style`] /
/// [`Cell::fg`] / [`Cell::bg`] / [`Cell::attrs`] accept either.
pub trait StyleLookup {
    /// Resolve an interned id back to its [`Style`] (default style
    /// for out-of-bounds ids, defensively).
    fn lookup(&self, id: u16) -> Style;
}

impl StyleLookup for StyleTable {
    fn lookup(&self, id: u16) -> Style {
        StyleTable::lookup(self, id)
    }
}

/// Per-frame immutable capture of a [`StyleTable`]'s id → [`Style`]
/// mapping — see [`StyleTable::snapshot`]. This is what the render
/// [`Snapshot`](crate::render) stores: lock-free reads at exactly
/// the clone cost the frame needs.
#[derive(Debug, Clone)]
pub struct StyleSnapshot {
    styles: Vec<Style>,
}

impl StyleLookup for StyleSnapshot {
    fn lookup(&self, id: u16) -> Style {
        self.styles
            .get(id as usize)
            .copied()
            .unwrap_or_default()
    }
}

/// Interning table mapping hyperlink URIs (OSC 8) ↔ `u16` link ids.
/// `0` is reserved for "no link" so a fresh [`Cell`] never touches
/// the table; real ids start at 1.
///
/// Carries forward the Arc-sharing rationale from the pre-M2 per-Cell
/// `Option<Arc<str>>`: printing N characters under one OSC-8 hyperlink
/// used to allocate N strings + N boxes (~2N per-byte allocations on
/// hyperlink-heavy `ls` output). Interning goes one further — the URI
/// is stored ONCE and every cell carries a 2-byte id.
#[derive(Debug, Clone, Default)]
pub struct LinkTable {
    /// id N (≥ 1) lives at index N-1.
    links: Vec<std::sync::Arc<str>>,
    by_uri: std::collections::HashMap<std::sync::Arc<str>, u16>,
}

/// Reserved link id meaning "no hyperlink".
pub const NO_LINK_ID: u16 = 0;

impl LinkTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a URI: return the existing id or allocate a new one.
    /// Returns `None` when the table is full AND the URI is not
    /// already present — the owner's signal to gc + retry (mirror of
    /// [`StyleTable::try_intern`]).
    pub fn try_intern(&mut self, uri: &str) -> Option<u16> {
        if let Some(&id) = self.by_uri.get(uri) {
            return Some(id);
        }
        let next = self.links.len() + 1;
        if next > u16::MAX as usize {
            return None;
        }
        let arc: std::sync::Arc<str> = std::sync::Arc::from(uri);
        self.links.push(std::sync::Arc::clone(&arc));
        self.by_uri.insert(arc, next as u16);
        Some(next as u16)
    }

    /// Intern a URI with the saturation degradation: a full table
    /// (> u16::MAX - 1 distinct URIs alive) yields NO_LINK_ID — the
    /// cell renders unlinked rather than mislinked. The Terminal's
    /// [`Terminal::intern_link`] gc-then-retry wrapper runs FIRST, so
    /// this terminal fallback only engages when the live set itself
    /// exceeds capacity.
    pub fn intern(&mut self, uri: &str) -> u16 {
        self.try_intern(uri).unwrap_or(NO_LINK_ID)
    }

    /// Rebuild the table keeping only `live` ids. Returns the
    /// old-id → new-id remap the owner applies to every live cell's
    /// `link_id` (mirror of [`StyleTable::gc`] — without it, one
    /// `ls --hyperlink` over a 65K-file tree permanently disabled
    /// new hyperlinks for the session). NO_LINK_ID maps to itself;
    /// ids absent from the remap must no longer be referenced.
    pub fn gc(
        &mut self,
        live: &std::collections::HashSet<u16>,
    ) -> std::collections::HashMap<u16, u16> {
        let old_links = std::mem::take(&mut self.links);
        self.by_uri.clear();

        let mut remap = std::collections::HashMap::with_capacity(live.len() + 1);
        remap.insert(NO_LINK_ID, NO_LINK_ID);

        // Deterministic rebuild order (sorted old ids) so two gc runs
        // over the same live set produce identical tables.
        let mut ids: Vec<u16> = live
            .iter()
            .copied()
            .filter(|&id| id != NO_LINK_ID)
            .collect();
        ids.sort_unstable();
        for old_id in ids {
            let Some(arc) = old_links.get(old_id as usize - 1) else {
                continue;
            };
            let next = self.links.len() + 1;
            if next > u16::MAX as usize {
                // Live set itself exceeds capacity — the remaining
                // ids degrade to NO_LINK_ID via the remap miss path.
                break;
            }
            self.links.push(std::sync::Arc::clone(arc));
            self.by_uri.insert(std::sync::Arc::clone(arc), next as u16);
            remap.insert(old_id, next as u16);
        }
        remap
    }

    /// Resolve a link id back to its URI. `NO_LINK_ID` (and any
    /// out-of-range id, defensively) resolves to `None`.
    #[must_use]
    pub fn lookup(&self, id: u16) -> Option<&str> {
        if id == NO_LINK_ID {
            return None;
        }
        self.links.get(id as usize - 1).map(|arc| &**arc)
    }

    /// Number of distinct interned URIs.
    #[must_use]
    #[allow(dead_code)] // Introspection surface; exercised by tests.
    pub fn len(&self) -> usize {
        self.links.len()
    }

    #[must_use]
    #[allow(dead_code)] // Introspection surface; exercised by tests.
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
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
    attrs: Attrs,
    origin_mode: bool,
}

// ---------------------------------------------------------------------------
// Line + LogicalLineId — physical row with logical-line identity (M2 stage 2)
// ---------------------------------------------------------------------------

/// Monotonically-stamped identity of a LOGICAL line — the unit of
/// text between hard newlines, however many physical rows it
/// soft-wraps across. Stamped by the owning [`Grid`] (fresh id per
/// hard line at `new()` / scroll-appended blank / inserted blank);
/// the put_char soft-wrap path propagates the SAME id onto the
/// continuation row. Rewrap-on-resize preserves ids, which is what
/// makes prompt/user marks invariant under reflow (they re-anchor to
/// `(LogicalLineId, intra-line row offset)` — see [`MarkAnchor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalLineId(u64);

/// One physical grid row. `cells` is the exact pre-M2 `Vec<Cell>` row
/// payload — the external boundary (`visible_rows()` /
/// `viewport_rows()` / `rows_from()`) keeps returning `&[Cell]` row
/// slices so search/selection/render/MCP consumers see byte-identical
/// shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Cell payload — always exactly `Grid::cols` wide.
    pub cells: Vec<Cell>,
    /// Soft-wrap marker: this physical row CONTINUES the same logical
    /// line on the NEXT row. A hard newline leaves it `false`.
    pub wrapped: bool,
    /// Identity of the logical line this physical row belongs to.
    pub logical_id: LogicalLineId,
}

impl Line {
    fn blank(cols: usize, logical_id: LogicalLineId) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
            logical_id,
        }
    }

    /// True when every cell is the default cell AND the row does not
    /// continue a logical line — the trim test the rewrap pass uses
    /// for trailing blank rows.
    fn is_blank_unwrapped(&self) -> bool {
        !self.wrapped && self.cells.iter().all(|c| *c == Cell::default())
    }
}

/// Logical anchor of a prompt/user mark: which logical line the mark
/// sits on plus the physical-row offset INTO that line (0 = the
/// line's first physical row). Invariant under rewrap — reflow
/// changes how many physical rows a logical line occupies, but the
/// anchor re-resolves through [`Grid::physical_row_of`] after the
/// new layout lands.
///
/// `run_index` disambiguates marker-broken lines: an erase that
/// reaches the right edge breaks the soft-wrap marker
/// ([`Grid::erase_cells`]) while both halves keep the shared
/// logical id — the rewrap then treats them as two separate
/// ADJACENT runs. Without the run index, an anchor on the second
/// half would resolve onto the (erased) first half after a reflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MarkAnchor {
    pub(crate) logical_id: LogicalLineId,
    /// Which marker-contiguous run of `logical_id` the row sits in
    /// (0 = the id's first run; see [`Grid::line_runs`]).
    pub(crate) run_index: usize,
    pub(crate) row_offset: usize,
}

/// Cell-precise sibling of [`MarkAnchor`] — anchors a `(row, col)`
/// position (the cursor, the DECSC saved cursor) through a rewrap by
/// its CELL offset into the logical line. Cell precision beats
/// row-only: when the column count changes, `row_offset * old_cols +
/// col` re-derives both the new physical row AND the new column from
/// the new width ([`Grid::resolve_cell_anchor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellAnchor {
    logical_id: LogicalLineId,
    /// Same run disambiguation as [`MarkAnchor::run_index`].
    run_index: usize,
    /// `row_offset_in_line * cols + col` at capture time.
    cell_offset: usize,
}

/// Which screen buffer a [`SelectionAnchor`] was captured on.
/// [`LogicalLineId`]s are stamped PER GRID (both counters start at
/// 0), so a primary-screen id numerically aliases an unrelated
/// alt-screen line — the tag rejects cross-screen resolution instead
/// of silently highlighting the wrong buffer's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenBuffer {
    Primary,
    Alternate,
}

/// Content-anchored selection endpoint — the public face of
/// [`CellAnchor`] plus the screen tag and the grid epoch. Opaque
/// outside this module by construction: the only producer is
/// [`Terminal::selection_anchor_at`] and the only consumer is
/// [`Terminal::resolve_selection_anchor`], so a selection endpoint
/// fabricated from raw viewport coordinates has no expressible path.
/// Anchors survive rewrap-on-resize (same mechanism as the cursor's
/// [`CellAnchor`]) and streaming scrollback growth (ids never move);
/// they resolve to `None` once the content is evicted or the grids
/// were rebuilt — tier: parse-time-rejected at the resolution
/// boundary, never stale coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionAnchor {
    /// [`Terminal::grid_epoch`] at capture time. RIS (`reset`)
    /// replaces both grids and restarts the id counters at 0 — an
    /// epoch mismatch rejects the anchor before an aliased id can
    /// resolve onto unrelated post-reset content.
    epoch: u64,
    screen: ScreenBuffer,
    anchor: CellAnchor,
}

/// Memo slot for [`Terminal::resolve_selection_span`] — the resolved
/// span is valid exactly while `seqno` (bumped on every terminal
/// mutation) and the anchor pair are unchanged. See the
/// `selection_span_memo` field doc for the hot-path rationale.
#[derive(Debug, Clone, Copy)]
struct SelectionSpanMemo {
    seqno: u64,
    a: SelectionAnchor,
    b: SelectionAnchor,
    resolved: Option<((usize, usize), (usize, usize))>,
}

// ---------------------------------------------------------------------------
// Grid — VecDeque-based terminal grid with O(1) scroll
// ---------------------------------------------------------------------------

/// VecDeque-of-[`Line`] terminal grid: scrollback at the front,
/// visible rows at the back. Scrollback eviction and every read path
/// go through methods (`rows_from` / `visible_rows_iter` /
/// `viewport_rows`), so the future `Arc<Line>` CoW swap for paged
/// scrollback is a one-type change behind the same surface.
///
/// The M2 field design is co-shaped for the M7 threading protocol —
/// per-row damage (`GridDamage`/`DirtyRegion`) and the bounded
/// `ParseMailbox` between the PTY reader and the parser. See
/// `docs/GRID-THREADING-CONTRACT.md` (Stream G contract) and the
/// typed stubs in `crate::grid_damage`.
struct Grid {
    /// All rows: scrollback at front, visible at back.
    rows: VecDeque<Line>,
    cols: usize,
    visible_rows: usize,
    max_scrollback: usize,
    /// Monotonic stamp source for [`LogicalLineId`]. Never reused —
    /// u64 cannot realistically wrap within a session.
    next_logical_id: u64,
}

impl Grid {
    fn new(cols: usize, visible_rows: usize, max_scrollback: usize) -> Self {
        // Capacity is a hint only — clamp the scrollback term so an
        // "unlimited" (usize::MAX) cap cannot overflow the addition
        // or demand an absurd up-front allocation.
        let mut rows =
            VecDeque::with_capacity(visible_rows + max_scrollback.min(4096));
        let mut next_logical_id = 0u64;
        for _ in 0..visible_rows {
            let id = LogicalLineId(next_logical_id);
            next_logical_id += 1;
            rows.push_back(Line::blank(cols, id));
        }
        Self {
            rows,
            cols,
            visible_rows,
            max_scrollback,
            next_logical_id,
        }
    }

    /// Allocate a fresh logical-line identity.
    fn fresh_id(&mut self) -> LogicalLineId {
        let id = LogicalLineId(self.next_logical_id);
        self.next_logical_id += 1;
        id
    }

    /// Number of scrollback lines available.
    /// Iterator over ALL rows (scrollback + visible) starting at the
    /// absolute index `from` — the scrollback-search row source.
    fn rows_from(&self, from: usize) -> impl Iterator<Item = &[Cell]> {
        self.rows.iter().skip(from).map(|l| l.cells.as_slice())
    }

    fn scrollback_len(&self) -> usize {
        self.rows.len().saturating_sub(self.visible_rows)
    }

    /// Access a visible row (0 = top of visible area).
    fn visible_row(&self, idx: usize) -> &[Cell] {
        let offset = self.scrollback_len();
        &self.rows[offset + idx].cells
    }

    /// Full [`Line`] (cells + wrap marker + logical id) at an
    /// ABSOLUTE row index (scrollback origin 0) — the soft-wrap-aware
    /// extraction walk reads the marker per row.
    fn line(&self, abs_row: usize) -> Option<&Line> {
        self.rows.get(abs_row)
    }

    /// Mutable access to a visible row's cell payload.
    fn visible_row_mut(&mut self, idx: usize) -> &mut Vec<Cell> {
        let offset = self.scrollback_len();
        &mut self.rows[offset + idx].cells
    }

    /// Mutable access to a visible row's full [`Line`] (cells + wrap
    /// marker + logical id) — the soft-wrap stamping path uses this.
    fn visible_line_mut(&mut self, idx: usize) -> &mut Line {
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

    /// The marker-contiguous physical runs of logical line `id`, in
    /// order: each `(first, last)` is one maximal group of rows
    /// sharing `id` whose non-final rows carry `wrapped == true`.
    /// Multiple runs of one id exist when an erase broke the
    /// soft-wrap marker mid-line ([`Self::erase_cells`]) — the
    /// rewrap keeps them as separate ADJACENT row groups, so anchor
    /// resolution must walk the same run boundaries. This is the ONE
    /// run-walk shared by [`Self::anchor_at`] /
    /// [`Self::physical_row_of`] / [`Self::resolve_cell_anchor`] —
    /// capture and resolution cannot disagree about run boundaries
    /// (id match AND marker-contiguity) because there is only one
    /// definition of them.
    fn line_runs(&self, id: LogicalLineId) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut start: Option<usize> = None;
        // FULL scan, no early break (M3 review 2026-06-12): same-id
        // runs are NOT always physically adjacent — scroll_region_up's
        // partial-region path removes the region-top row and inserts a
        // fresh blank at the region bottom, which can land BETWEEN the
        // rows of a soft-wrapped logical line straddling the region
        // edge. The former "once past the span, break" rule dropped
        // the orphaned tail run, so anchors captured there resolved to
        // the FIRST run's last cell (wrong highlight + wrong copy)
        // instead of their own row. Hot-path cost is bounded by the
        // seqno-keyed span memo in `resolve_selection_span`.
        for (i, line) in self.rows.iter().enumerate() {
            if line.logical_id == id {
                if start.is_none() {
                    start = Some(i);
                }
                if !line.wrapped {
                    runs.push((start.take().expect("run start set above"), i));
                }
            } else if let Some(s) = start.take() {
                // Dangling wrap marker at the span end (scroll-
                // region surgery) — close the run at its last row.
                runs.push((s, i - 1));
            }
        }
        if let Some(s) = start {
            // Dangling wrap marker at the buffer end.
            runs.push((s, self.rows.len() - 1));
        }
        runs
    }

    /// Logical anchor of the physical row `abs_row` — `(logical id,
    /// run index, offset of abs_row within its run)`. `None` when
    /// the row is out of bounds.
    fn anchor_at(&self, abs_row: usize) -> Option<MarkAnchor> {
        if abs_row >= self.rows.len() {
            return None;
        }
        let id = self.rows[abs_row].logical_id;
        let runs = self.line_runs(id);
        let run_index = runs
            .iter()
            .position(|&(f, l)| f <= abs_row && abs_row <= l)
            .unwrap_or(0);
        let start = runs.get(run_index).map_or(abs_row, |&(f, _)| f);
        Some(MarkAnchor {
            logical_id: id,
            run_index,
            row_offset: abs_row - start,
        })
    }

    /// Cell-precise anchor of `(abs_row, col)` — see [`CellAnchor`].
    fn cell_anchor_at(&self, abs_row: usize, col: usize) -> Option<CellAnchor> {
        let a = self.anchor_at(abs_row)?;
        Some(CellAnchor {
            logical_id: a.logical_id,
            run_index: a.run_index,
            cell_offset: a.row_offset * self.cols + col.min(self.cols),
        })
    }

    /// Resolve a [`MarkAnchor`] back to an absolute physical row.
    /// O(rows) scan — rows are bounded by visible + scrollback cap.
    /// A run index past the surviving runs clamps to the id's LAST
    /// run; an offset past the run's surviving rows clamps to the
    /// run's LAST physical row (the line head may have been
    /// evicted). Returns `None` when no physical row with that
    /// logical id remains — the caller garbage-collects the mark.
    fn physical_row_of(&self, anchor: MarkAnchor) -> Option<usize> {
        let runs = self.line_runs(anchor.logical_id);
        let &(first, last) = runs.get(anchor.run_index).or_else(|| runs.last())?;
        Some((first + anchor.row_offset).min(last))
    }

    /// Resolve a [`CellAnchor`] back to an absolute `(row, col)`
    /// under the CURRENT column width — the cell offset re-derives
    /// both coordinates, which is what carries the cursor through a
    /// rewrap. Clamps like [`Self::physical_row_of`]; an offset past
    /// the run's surviving cells parks at the run's last cell.
    fn resolve_cell_anchor(&self, anchor: CellAnchor) -> Option<(usize, usize)> {
        let runs = self.line_runs(anchor.logical_id);
        let &(first, last) = runs.get(anchor.run_index).or_else(|| runs.last())?;
        let row = first + anchor.cell_offset / self.cols.max(1);
        if row <= last {
            Some((row, anchor.cell_offset % self.cols.max(1)))
        } else {
            Some((last, self.cols.saturating_sub(1)))
        }
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
            let id = self.fresh_id();
            self.rows.push_back(Line::blank(self.cols, id));
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
            let id = self.fresh_id();
            self.rows.insert(insert_idx, Line::blank(self.cols, id));
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
        let id = self.fresh_id();
        self.rows.insert(sb_offset + top, Line::blank(self.cols, id));
    }

    /// Clear a range of cells in a visible row.
    fn erase_cells(&mut self, row: usize, start: usize, end: usize) {
        let end = end.min(self.cols);
        let reaches_line_end = end == self.cols;
        let line = self.visible_line_mut(row);
        for col in start..end {
            line.cells[col] = Cell::default();
        }
        // An erase that reaches the right edge breaks the soft wrap:
        // the row no longer has content flowing into the next row, so
        // a later rewrap must not join them (M2 wrap hygiene).
        if reaches_line_end {
            line.wrapped = false;
        }
    }

    /// Clear the entire visible area.
    fn clear_visible(&mut self) {
        for i in 0..self.visible_rows {
            let line = self.visible_line_mut(i);
            for cell in line.cells.iter_mut() {
                *cell = Cell::default();
            }
            // Cleared rows carry no soft-wrap continuation.
            line.wrapped = false;
        }
    }

    /// Resize the grid.
    ///
    /// `rewrap = false` (the ALT grid always; the primary grid when
    /// `reflow_on_resize` is off): the pre-M2 truncate/extend
    /// semantics — every row is cut or blank-padded to the new width.
    /// Full-screen TUIs redraw themselves on SIGWINCH, so the alt
    /// grid never reflows.
    ///
    /// `rewrap = true` (primary grid, reflow on): column changes
    /// REWRAP — logical lines (scrollback + visible joined as one
    /// continuous sequence) are unwrapped and reflowed to the new
    /// width, preserving every cell and every [`LogicalLineId`].
    /// Row-count changes keep the legacy semantics on both paths
    /// (blank rows appended / popped at the bottom) so cursor and
    /// scroll-region behavior match the pre-M2 contract.
    fn resize(&mut self, cols: usize, visible_rows: usize, rewrap: bool) {
        // Row-count change first (legacy semantics, both paths) so the
        // rewrap pass below pads/trims against the NEW visible height.
        match visible_rows.cmp(&self.visible_rows) {
            std::cmp::Ordering::Greater => {
                let extra = visible_rows - self.visible_rows;
                for _ in 0..extra {
                    let id = self.fresh_id();
                    self.rows.push_back(Line::blank(self.cols, id));
                }
            }
            std::cmp::Ordering::Less => {
                // Remove rows from the bottom of visible area
                let remove = self.visible_rows - visible_rows;
                for _ in 0..remove {
                    self.rows.pop_back();
                }
                // A popped row may have been the continuation of a
                // wrapped line — the new back row must not advertise
                // a continuation that no longer exists.
                if let Some(back) = self.rows.back_mut() {
                    back.wrapped = false;
                }
            }
            std::cmp::Ordering::Equal => {}
        }
        self.visible_rows = visible_rows;

        if cols != self.cols {
            if rewrap {
                self.rewrap_to_cols(cols);
            } else {
                // Legacy truncate/extend.
                for line in &mut self.rows {
                    line.cells.resize(cols, Cell::default());
                }
                self.cols = cols;
            }
        }
    }

    /// REWRAP (M2 stage 2): re-flow every logical line to a new
    /// column width. Scrollback and visible rows participate as one
    /// continuous sequence, so logical lines spanning the
    /// scrollback/visible boundary join correctly.
    ///
    /// Algorithm:
    ///   1. UNWRAP — walk all rows front-to-back, joining each
    ///      maximal `wrapped == true` run into one logical cell run.
    ///      Wrapped (non-final) rows contribute ALL their cells; the
    ///      final row of each run is trimmed of trailing default
    ///      cells (they are padding, not content).
    ///   2. REFLOW — split each run into physical rows of the new
    ///      width. A width-2 lead and its width-0 continuation are
    ///      kept adjacent (the pair never splits across rows). All
    ///      produced rows but the last get `wrapped = true`; every
    ///      row keeps the run's [`LogicalLineId`].
    ///   3. SETTLE — pad blank rows (fresh ids) at the back until the
    ///      visible area is full; trim trailing blank rows beyond the
    ///      visible area ONLY while the pre-rewrap viewport-top line
    ///      stays at the viewport top (so blank tails the reflow
    ///      displaced don't masquerade as scrollback, while the trim
    ///      can never pull scrollback content into the viewport —
    ///      the post-`clear` blanks below the cursor are legitimate
    ///      screen rows); evict from the FRONT if the scrollback cap
    ///      is exceeded.
    fn rewrap_to_cols(&mut self, new_cols: usize) {
        // SETTLE anchor: the live viewport-top row's logical anchor,
        // captured BEFORE the reflow. The trim below stops once this
        // line is back at the viewport top — trimming further would
        // backfill the viewport from scrollback (clear-then-resize
        // corruption, review finding 2026-06-12).
        let settle_anchor = self.anchor_at(self.scrollback_len());

        let old_rows = std::mem::take(&mut self.rows);
        let mut new_rows: VecDeque<Line> = VecDeque::with_capacity(old_rows.len());

        let mut iter = old_rows.into_iter();
        let mut pending: Option<Line> = iter.next();
        while let Some(first) = pending.take() {
            let logical_id = first.logical_id;
            let mut run: Vec<Cell> = Vec::new();
            let mut cur = first;
            loop {
                if cur.wrapped {
                    // Non-final row of the run: the payload is content,
                    // EXCEPT a single trailing default cell left by the
                    // wide-char early wrap (put_char wraps a width-2
                    // glyph that doesn't fit, never writing the last
                    // column). Detect by the continuation row leading
                    // with a width-2 cell; drop AT MOST ONE spacer so
                    // it can't become interior content in the reflow.
                    match iter.next() {
                        // Continuations share the logical id by
                        // construction; an id mismatch means scroll-
                        // region surgery left a dangling wrap marker —
                        // end the run, the next row starts its own.
                        Some(next) if next.logical_id == logical_id => {
                            let mut cells = cur.cells;
                            if next.cells.first().is_some_and(|c| c.width == 2)
                                && cells.last().is_some_and(|c| *c == Cell::default())
                            {
                                cells.pop();
                            }
                            run.extend(cells);
                            cur = next;
                        }
                        Some(next) => {
                            run.extend(cur.cells);
                            pending = Some(next);
                            break;
                        }
                        // Dangling wrap marker at the buffer end —
                        // treat the cells as the whole run.
                        None => {
                            run.extend(cur.cells);
                            break;
                        }
                    }
                } else {
                    // Final row: trailing default cells are padding.
                    let mut cells = cur.cells;
                    while cells.last().is_some_and(|c| *c == Cell::default()) {
                        cells.pop();
                    }
                    run.extend(cells);
                    pending = iter.next();
                    break;
                }
            }
            Self::push_reflowed_line(&mut new_rows, run, new_cols, logical_id);
        }

        self.rows = new_rows;
        self.cols = new_cols;

        // SETTLE: visible-area + scrollback-cap consistency.
        while self.rows.len() < self.visible_rows {
            let id = self.fresh_id();
            self.rows.push_back(Line::blank(new_cols, id));
        }
        // Trim floor: keep the pre-rewrap viewport-top line AT the
        // viewport top. rows.len() may not drop below anchor_row +
        // visible_rows, so the trim only drops blank rows the reflow
        // pushed PAST the viewport (content grew into them) — it can
        // never promote scrollback into view. When the anchor can't
        // resolve (its line was evicted), fall back to the plain
        // visible floor.
        let trim_floor = settle_anchor
            .and_then(|a| self.physical_row_of(a))
            .map_or(self.visible_rows, |row| row + self.visible_rows);
        while self.rows.len() > self.visible_rows.max(trim_floor)
            && self.rows.back().is_some_and(Line::is_blank_unwrapped)
        {
            self.rows.pop_back();
        }
        while self.scrollback_len() > self.max_scrollback {
            self.rows.pop_front();
        }
    }

    /// Reflow one logical cell run to `cols`-wide physical rows and
    /// append them to `out`. All rows but the last are marked
    /// `wrapped`; every row carries `id`. An empty run still emits
    /// one blank row (a logical line never vanishes in a reflow).
    fn push_reflowed_line(
        out: &mut VecDeque<Line>,
        run: Vec<Cell>,
        cols: usize,
        id: LogicalLineId,
    ) {
        let mut rows_for_line: Vec<Vec<Cell>> = Vec::new();
        let mut cur: Vec<Cell> = Vec::with_capacity(cols.min(512));
        for cell in run {
            // A width-2 lead needs room for itself + its width-0
            // continuation — never split the pair across rows.
            let needed = if cell.width == 2 { 2usize } else { 1 };
            // The `cur.is_empty() && needed > cols` guard forces
            // progress on the degenerate cols == 1 grid (a wide pair
            // can never fit; place the lead alone rather than loop).
            if cur.len() + needed > cols && !(cur.is_empty() && needed > cols) {
                rows_for_line.push(std::mem::take(&mut cur));
            }
            cur.push(cell);
        }
        rows_for_line.push(cur);

        let last = rows_for_line.len() - 1;
        for (k, mut cells) in rows_for_line.into_iter().enumerate() {
            cells.resize(cols, Cell::default());
            out.push_back(Line {
                cells,
                wrapped: k != last,
                logical_id: id,
            });
        }
    }

    /// Iterator over visible rows.
    fn visible_rows_iter(&self) -> impl Iterator<Item = &[Cell]> {
        let offset = self.scrollback_len();
        self.rows.range(offset..).map(|l| l.cells.as_slice())
    }

    /// Iterator over scrollback rows at a given viewport offset.
    /// Returns `visible_rows` rows starting from the scroll position.
    fn viewport_rows(&self, scroll_offset: usize) -> impl Iterator<Item = &[Cell]> {
        let sb_len = self.scrollback_len();
        let offset = scroll_offset.min(sb_len);
        let start = sb_len - offset;
        self.rows
            .range(start..start + self.visible_rows)
            .map(|l| l.cells.as_slice())
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
    /// Row where this placement starts, in **visible-viewport coordinates**
    /// (row 0 = top visible row), NOT absolute scrollback coordinates.
    /// M2 review LAW (2026-06-12): the rewrap bridge re-anchors placements
    /// against the visible grid (`reflow_lines` walks `placement_anchors`
    /// in primary-visible space), and `draw_kitty_images` adds
    /// `row * cell_height` to the viewport origin — both consume this as a
    /// visible-row index. The earlier "absolute grid row" doc was a lie:
    /// no code path treats it as scrollback-absolute. Fixed the doc, not
    /// the semantics, because the de-facto contract is correct and load-bearing.
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
    /// Z-index for layering. Render splits placements into two bands:
    /// `z < 0` draws BELOW the text scene (after cell backgrounds,
    /// before glyphs); `z >= 0` draws ABOVE it (after glyphs, before
    /// the engawa effect chain composites). Within a band, ordering is
    /// stable: ascending `z_index`, then transmission (insertion) order.
    pub z_index: i32,
}

impl ImagePlacement {
    /// True when this placement layers ABOVE the text scene (`z >= 0`).
    /// `z < 0` layers below — the Kitty graphics z-ordering contract.
    #[must_use]
    pub fn is_above_text(&self) -> bool {
        self.z_index >= 0
    }
}

/// Partition placements into the two render bands and return each band
/// in stable draw order: ascending `z_index`, then transmission order.
///
/// This is the pure seam the render path consumes — `draw_kitty_images`
/// is called once per returned band, so the instance-buffer fill order
/// (and therefore the GPU draw order) is exactly the returned order.
/// Asserting on this Vec ordering pins z-layering mechanically without a
/// headless GPU device (pixel-asserting the composite is the brittle
/// alternative the M3 goldens cover for the single-band case).
#[must_use]
pub fn partition_placements_by_z(
    placements: &[ImagePlacement],
) -> (Vec<ImagePlacement>, Vec<ImagePlacement>) {
    let mut below: Vec<ImagePlacement> = Vec::new();
    let mut above: Vec<ImagePlacement> = Vec::new();
    for p in placements {
        if p.is_above_text() {
            above.push(p.clone());
        } else {
            below.push(p.clone());
        }
    }
    // sort_by_key is stable, so equal-z placements keep transmission order.
    below.sort_by_key(|p| p.z_index);
    above.sort_by_key(|p| p.z_index);
    (below, above)
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
    pen_attrs: Attrs,

    // Default colors (set by theme; used for SGR 0/39/49 resets)
    default_fg: Color,
    default_bg: Color,

    // Active 256-entry ANSI palette (M2): 0..16 settable by theme,
    // any entry settable by OSC 4 / resettable by OSC 104.
    ansi_colors: [Color; 256],

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
    /// M2 — rewrap-on-resize. When true (the kitty/ghostty default,
    /// wired from `behavior.reflow_on_resize`), a column resize
    /// REWRAPS the primary grid's logical lines instead of
    /// truncating. The alternate grid always truncates (full-screen
    /// TUIs redraw themselves).
    reflow_on_resize: bool,

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

    /// Grid-geometry generation — bumped by every effective
    /// [`Self::resize`] (rewrap or truncate; both renumber absolute
    /// rows). Consumers caching absolute grid rows (the search
    /// engine's match list, future placement caches) compare this and
    /// re-derive on change. Monotonic per Terminal value; a RIS
    /// rebuild restarts it at 0, which still reads as "changed" to
    /// any consumer holding a non-zero generation.
    grid_generation: u64,

    /// Grid-identity epoch — bumped ONLY when the grids are rebuilt
    /// from scratch (RIS via [`Self::reset`]), restarting the
    /// [`LogicalLineId`] counters at 0. A [`SelectionAnchor`] carries
    /// the epoch it was captured under; a mismatch rejects resolution
    /// before an aliased id lands on unrelated post-reset content.
    /// Distinct from `grid_generation` on purpose: anchors SURVIVE
    /// resize/rewrap (that is their whole point) but never a rebuild.
    grid_epoch: u64,

    /// One-slot seqno-keyed memo for [`Self::resolve_selection_span`]
    /// (M3 review 2026-06-12). Resolution walks the row deque
    /// (`Grid::line_runs` is O(rows)) and the render path resolves
    /// the live span on EVERY vsync plus every engine redraw tick —
    /// with a committed selection over a large scrollback that was
    /// multiple near-full deque scans per frame. `seqno` bumps on
    /// every terminal mutation ([`Self::dirty`]), so a hit can never
    /// serve a resolution the grid has moved under; idle frames with
    /// a standing selection cost O(1), and a streaming frame
    /// resolves once, shared by the renderer snapshot AND the
    /// engine's `reconcile_selection` (both call the memoized span).
    /// Interior mutability (Mutex, uncontended single-slot) because
    /// resolution happens under the shared read lock.
    selection_span_memo: std::sync::Mutex<Option<SelectionSpanMemo>>,

    // Tab stops
    tab_stops: Vec<bool>,

    // Response bytes to send back to the PTY (for DSR, DA, etc.)
    response_bytes: Vec<u8>,

    // Synchronized output (CSI ? 2026) — batch drawing
    synchronized_output: bool,


    /// P32 + M2 — style ID interning table. Maps (fg, bg, attrs)
    /// triples to a u16 tag stored on every Cell. Post-shrink this is
    /// the ONLY styling storage: every Cell read resolves through it
    /// (see [`Cell::fg`] / [`Cell::bg`] / [`Cell::attrs`]). ID
    /// equality implies styling equality.
    pub(crate) style_table: StyleTable,

    /// M2 — hyperlink URI interning table (OSC 8), sibling of
    /// `style_table`. Cells carry `link_id: u16`; `0` = no link.
    pub(crate) link_table: LinkTable,

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

    // Title changed since the last drain_side_effects() — the drain
    // emits the title ONLY on change so adapters never re-apply an
    // unchanged title per frame (replaces the per-loop last_title
    // diff the M4 drain deleted).
    title_changed: bool,

    // Current working directory (from OSC 7)
    cwd: Option<String>,

    // CWD changed since the last drain_side_effects() — same
    // change-edge contract as `title_changed`.
    cwd_changed: bool,

    // Bell state (BEL character received, cleared after read)
    bell_pending: bool,

    // Dynamic cursor shape (DECSCUSR)
    pub cursor_style: CursorStyle,
    pub cursor_blink: bool,

    // Active hyperlink link-id (from OSC 8, applied to subsequent
    // cells). Interned into `link_table` once per OSC 8 — the
    // per-character paint path copies a u16, no allocation, no
    // ref-count traffic. (Pre-M2 this was an Option<Arc<str>> cloned
    // per cell; interning subsumes that Arc-sharing optimization.)
    // NO_LINK_ID = no active hyperlink.
    active_link_id: u16,

    // OSC 52 clipboard content (set by terminal, read by main for clipboard sync)
    clipboard_content: Option<String>,

    // Typed desktop notifications queued by the terminal (OSC 9 /
    // OSC 777;notify / OSC 99) — the event loops drain + dispatch
    // these via `tsuuchi`. ConEmu progress (OSC 9;4) is NOT in this
    // queue: it lives in the `progress` lane below, so a progress
    // update firing a notification is unrepresentable.
    pending_notifications: Vec<PendingNotification>,

    // ConEmu OSC 9;4 progress — latest-wins typed lane, drained
    // separately from notifications. `None` = no update since the
    // last drain.
    progress: Option<ProgressState>,

    // Kitty OSC 99 multi-part accumulator (metadata `d=0` chains
    // title/body fragments across escapes; `d=1` finalizes). Single
    // slot: kitty serializes chains per id, and a chain interrupted
    // by a different id is dropped WITH a trace (never silently).
    pending_osc99: Option<Osc99Pending>,

    // Wall-clock seam — the Terminal owns no clock; mark timestamps
    // (PromptMark::at_unix_ms) read through this injectable fn so
    // tests pin deterministic stamps. Defaults to the real UNIX
    // wall clock; survives RIS like `reflow_on_resize` does
    // (environmental wiring, not VT state).
    clock_unix_ms: fn() -> u128,

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
    // LEVEL state: readable any time via attention_requested() (MCP
    // attention_get) and writable via RequestAttention=0/1.
    attention_requested: bool,

    // RequestAttention RISING EDGE since the last
    // drain_side_effects() — the drain consumes this (dock bounce +
    // Critical dispatch fire once per request, not once per frame
    // while the level stays high).
    attention_edge: bool,

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

    // Sixel decode goes straight through the shared image path (unhook
    // → icy_sixel → store_rgba_image → `images` + `image_placements`).
    // No raw-payload audit Vec is retained — it was unbounded dead
    // weight that cloned every DCS payload alongside the decoded
    // texture (review 2026-06-12, correctness-1).
    sixel_buffer: Option<Vec<u8>>,
    // Set once a sixel DCS payload passes SIXEL_DCS_MAX in `put()`:
    // the partial buffer is dropped and every further byte no-ops
    // until `unhook` rejects the whole sequence with a typed trace.
    // Mirrors the kitty APC_MAX guard — an unterminated/giant sixel
    // must not grow `sixel_buffer` without bound (review 2026-06-12,
    // critic-1). Cleared at hook time for the next sequence.
    sixel_buffer_overflow: bool,
    // DCS numeric params (P1 aspect, P2 background, P3 grid) captured at
    // `hook` time so `unhook` can build icy_sixel's DcsSettings faithfully.
    sixel_dcs_params: (Option<u16>, Option<u16>, Option<u16>),

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
            pen_attrs: Attrs::NONE,
            default_fg: Color::WHITE,
            default_bg: Color::BLACK,
            ansi_colors: default_palette_256(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            auto_wrap: true,
            origin_mode: false,
            cursor_keys_mode: false,
            bracketed_paste: false,
            insert_mode: false,
            keypad_app_mode: false,
            wrap_pending: false,
            reflow_on_resize: true,
            charset_g0_graphics: false,
            charset_g1_graphics: false,
            gl_is_g1: false,
            mouse_mode: MouseMode::Off,
            sgr_mouse: false,
            scroll_offset: 0,
            seqno: 0,
            grid_generation: 0,
            grid_epoch: 0,
            selection_span_memo: std::sync::Mutex::new(None),
            tab_stops,
            response_bytes: Vec::new(),
            synchronized_output: false,
            style_table: StyleTable::new(),
            link_table: LinkTable::new(),
            cached_style: None,
            cached_style_id: DEFAULT_STYLE_ID,
            focus_reporting: false,
            last_char: ' ',
            title: None,
            title_changed: false,
            cwd: None,
            cwd_changed: false,
            bell_pending: false,
            cursor_style: CursorStyle::Block,
            cursor_blink: true,
            active_link_id: NO_LINK_ID,
            clipboard_content: None,
            pending_notifications: Vec::new(),
            progress: None,
            pending_osc99: None,
            clock_unix_ms: wall_clock_unix_ms,
            clipboard_store: crate::clipboard_store::ClipboardStore::new(128),
            prompt_marks: crate::prompt_mark::PromptHistory::with_capacity(
                max_scrollback.max(256),
            ),
            pointer_shape: crate::pointer_shape::PointerShape::default(),
            user_marks: crate::osc_1337::UserMarkHistory::with_capacity(
                max_scrollback.max(256),
            ),
            attention_requested: false,
            attention_edge: false,
            kitty_keyboard_stack: Vec::new(),
            images: HashMap::new(),
            image_placements: Vec::new(),
            next_image_id: 1,
            pending_kitty: None,
            sixel_buffer: None,
            sixel_buffer_overflow: false,
            sixel_dcs_params: (None, None, None),
            apc_buf: None,
            pending_esc: PendingEsc::None,
            utf8_tail: Vec::new(),
            dcs_handler: None,
            parser: vte::Parser::new(),
        }
    }

    /// Apply a color theme: set default fg/bg and the 16-color ANSI palette.
    /// Resets the current pen colors to the new defaults. The extended
    /// 16..=255 cube/grayscale entries are untouched (themes own the
    /// base 16; OSC 4 owns per-index overrides).
    pub fn apply_theme(&mut self, fg: Color, bg: Color, ansi: [Color; 16]) {
        self.default_fg = fg;
        self.default_bg = bg;
        self.pen_fg = fg;
        self.pen_bg = bg;
        self.ansi_colors[..16].copy_from_slice(&ansi);
        self.dirty();
    }

    /// The active 256-entry ANSI palette (base 16 may be overridden by
    /// a theme; any entry by OSC 4).
    #[must_use]
    #[allow(dead_code)]
    pub fn ansi_palette(&self) -> &[Color; 256] {
        &self.ansi_colors
    }

    /// The style interning table every [`Cell::style_id`] resolves
    /// through. Read sites pass this to [`Cell::fg`] / [`Cell::bg`] /
    /// [`Cell::attrs`].
    #[must_use]
    pub fn styles(&self) -> &StyleTable {
        &self.style_table
    }

    /// The hyperlink interning table every [`Cell::link_id`] resolves
    /// through (see [`Cell::hyperlink`]).
    #[must_use]
    #[allow(dead_code)] // Read surface for the upcoming link-hover/click path.
    pub fn links(&self) -> &LinkTable {
        &self.link_table
    }

    /// Intern a style with the M2 overflow policy: when the table
    /// saturates, garbage-collect it against the style ids still
    /// referenced by live cells (both grids incl. scrollback), remap
    /// every cell, and retry. Only if the LIVE set itself exceeds
    /// capacity does [`StyleTable::intern`]'s warn-once last-id
    /// fallback engage — never an alias to the default style.
    fn intern_style(&mut self, style: Style) -> u16 {
        if let Some(id) = self.style_table.try_intern(style) {
            return id;
        }
        self.gc_style_table();
        self.style_table.intern(style)
    }

    /// Rebuild the style table from the ids referenced by live cells
    /// and remap every cell's `style_id` accordingly. The single-slot
    /// pen cache is invalidated (its id may have been remapped).
    fn gc_style_table(&mut self) {
        let mut live: std::collections::HashSet<u16> =
            std::collections::HashSet::new();
        for grid in [&self.primary, &self.alternate] {
            for row in &grid.rows {
                for cell in &row.cells {
                    live.insert(cell.style_id);
                }
            }
        }
        let remap = self.style_table.gc(&live);
        for grid in [&mut self.primary, &mut self.alternate] {
            for row in &mut grid.rows {
                for cell in row.cells.iter_mut() {
                    if let Some(&new_id) = remap.get(&cell.style_id) {
                        cell.style_id = new_id;
                    } else {
                        // Defensive — a live id is always in the remap.
                        cell.style_id = DEFAULT_STYLE_ID;
                    }
                }
            }
        }
        self.cached_style = None;
        self.cached_style_id = DEFAULT_STYLE_ID;
        tracing::debug!(
            live_styles = self.style_table.len(),
            "style table gc — rebuilt from live grid references"
        );
    }

    /// Intern a hyperlink URI with the gc-then-retry overflow policy
    /// (mirror of [`Self::intern_style`]): on a saturated table,
    /// garbage-collect against the link ids still referenced by live
    /// cells (both grids incl. scrollback), remap every cell, and
    /// retry. Only if the LIVE set itself exceeds capacity does the
    /// NO_LINK_ID degradation engage — unlinked, never mislinked.
    fn intern_link(&mut self, uri: &str) -> u16 {
        if let Some(id) = self.link_table.try_intern(uri) {
            return id;
        }
        self.gc_link_table();
        self.link_table.intern(uri)
    }

    /// Rebuild the link table from the ids referenced by live cells
    /// (plus the active pen link) and remap every cell's `link_id`
    /// accordingly.
    fn gc_link_table(&mut self) {
        let mut live: std::collections::HashSet<u16> =
            std::collections::HashSet::new();
        for grid in [&self.primary, &self.alternate] {
            for row in &grid.rows {
                for cell in &row.cells {
                    if cell.link_id != NO_LINK_ID {
                        live.insert(cell.link_id);
                    }
                }
            }
        }
        if self.active_link_id != NO_LINK_ID {
            live.insert(self.active_link_id);
        }
        let remap = self.link_table.gc(&live);
        for grid in [&mut self.primary, &mut self.alternate] {
            for row in &mut grid.rows {
                for cell in row.cells.iter_mut() {
                    if cell.link_id != NO_LINK_ID {
                        cell.link_id = remap
                            .get(&cell.link_id)
                            .copied()
                            .unwrap_or(NO_LINK_ID);
                    }
                }
            }
        }
        if self.active_link_id != NO_LINK_ID {
            self.active_link_id = remap
                .get(&self.active_link_id)
                .copied()
                .unwrap_or(NO_LINK_ID);
        }
        tracing::debug!(
            live_links = self.link_table.len(),
            "link table gc — rebuilt from live grid references"
        );
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

        // M2 rewrap: the primary grid reflows on column changes when
        // the knob is on; the ALT grid always truncates. Marks are
        // re-anchored around the reflow — their anchors are refreshed
        // from the CURRENT physical layout first (the cached grid_row
        // is exact truth right now; stored anchors may have gone
        // stale across partial-line evictions), then re-resolved
        // against the new layout after. The cursor, the DECSC saved
        // cursor, Kitty image placements, and a scrolled-up viewport
        // ride the SAME bridge — every grid-row-referencing state
        // crosses the rewrap as (LogicalLineId, run, offset), never
        // as a raw row number.
        let rewrap = self.reflow_on_resize && cols != self.cols;
        let mut cursor_anchor: Option<CellAnchor> = None;
        let mut saved_cursor_anchor: Option<CellAnchor> = None;
        let mut placement_anchors: Option<Vec<Option<MarkAnchor>>> = None;
        let mut view_anchor: Option<MarkAnchor> = None;
        if rewrap {
            let grid = &self.primary;
            self.prompt_marks.refresh_anchors(|row| grid.anchor_at(row));
            self.user_marks.refresh_anchors(|row| grid.anchor_at(row));
            // Cursor + placements live in PRIMARY visible coordinates;
            // when the alt screen is active they belong to the alt
            // grid (which truncates, keeping its row numbering) and
            // keep the numeric clamp below.
            if !self.use_alternate {
                cursor_anchor = grid
                    .cell_anchor_at(grid.scrollback_len() + self.cursor.row, self.cursor.col);
                placement_anchors = Some(
                    self.image_placements
                        .iter()
                        .map(|p| grid.anchor_at(grid.scrollback_len() + p.row))
                        .collect(),
                );
            }
            // The primary DECSC saved cursor always re-anchors (it is
            // primary-screen state even while the alt screen shows);
            // the alt's saved cursor never does — the alt grid
            // truncates.
            saved_cursor_anchor = self.saved_cursor.as_ref().and_then(|s| {
                grid.cell_anchor_at(grid.scrollback_len() + s.row, s.col)
            });
            // Content-pin a scrolled-up viewport: anchor the viewport
            // TOP row so the operator keeps reading the same content
            // after the reflow renumbers every physical row (same
            // contract as scroll_grid_up's streaming-output pinning).
            if self.scroll_offset > 0 {
                view_anchor =
                    grid.anchor_at(grid.scrollback_len().saturating_sub(self.scroll_offset));
            }
        }

        self.primary.resize(cols, rows, self.reflow_on_resize);
        self.alternate.resize(cols, rows, false);

        // Image ids whose placement was pruned this rewrap — collected
        // while `grid` is borrowed, GC'd after that borrow ends (the
        // GC needs `&mut self`; review 2026-06-12, critic-2).
        let mut dropped_image_ids: Vec<u32> = Vec::new();
        if rewrap {
            let grid = &self.primary;
            self.prompt_marks.reanchor(|a| grid.physical_row_of(a));
            self.user_marks.reanchor(|a| grid.physical_row_of(a));
            let sb = grid.scrollback_len();
            if let Some((abs_row, col)) =
                cursor_anchor.and_then(|a| grid.resolve_cell_anchor(a))
            {
                self.cursor.row = abs_row.saturating_sub(sb);
                self.cursor.col = col;
            }
            if let Some((abs_row, col)) =
                saved_cursor_anchor.and_then(|a| grid.resolve_cell_anchor(a))
            {
                if let Some(s) = self.saved_cursor.as_mut() {
                    s.row = abs_row.saturating_sub(sb);
                    s.col = col.min(cols.saturating_sub(1));
                }
            }
            if let Some(anchors) = placement_anchors {
                let mut anchors = anchors.into_iter();
                self.image_placements.retain_mut(|p| {
                    match anchors
                        .next()
                        .flatten()
                        .and_then(|a| grid.physical_row_of(a))
                    {
                        // Placement rows are visible-relative (the
                        // creation site stores cursor.row; the render
                        // path draws at row * cell_height) — drop
                        // placements whose line vanished or slid out
                        // of the visible area.
                        Some(abs_row) if abs_row >= sb && abs_row - sb < rows => {
                            p.row = abs_row - sb;
                            true
                        }
                        _ => {
                            dropped_image_ids.push(p.image_id);
                            false
                        }
                    }
                });
            }
            if let Some(row) = view_anchor.and_then(|a| grid.physical_row_of(a)) {
                self.scroll_offset = sb.saturating_sub(row);
            }
        }
        // `grid` borrow ended with the `if rewrap` block — free any
        // decoded texture whose last placement was just pruned.
        self.gc_orphaned_images(&dropped_image_ids);

        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);

        // Resize tab stops
        self.tab_stops.resize(cols, false);
        for i in (0..cols).step_by(8) {
            self.tab_stops[i] = true;
        }

        // Clamp cursor (bounds the re-anchored values too)
        self.cursor.row = self.cursor.row.min(rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(cols.saturating_sub(1));
        self.wrap_pending = false;
        // Rewrap can change the scrollback row count out from under a
        // scrolled-up viewport — clamp so the offset stays addressable.
        // ALWAYS against the PRIMARY grid: the offset is primary-
        // viewport state, and the alt grid's scrollback_len() is a
        // constant 0 — clamping against it while the alt screen is
        // active would zero a pinned reading position (review finding
        // 2026-06-12).
        self.scroll_offset = self.scroll_offset.min(self.primary.scrollback_len());
        // Geometry changed — absolute-row consumers (search matches,
        // anything caching grid rows) must re-derive. See
        // `grid_generation`.
        self.grid_generation = self.grid_generation.wrapping_add(1);
        self.dirty();

        tracing::debug!(cols, rows, rewrap, "terminal resized");
    }

    /// Wire the `behavior.reflow_on_resize` config knob (M2 — kills
    /// the dead knob). `true` = column resizes REWRAP the primary
    /// grid's logical lines; `false` = legacy truncate on both grids.
    pub fn set_reflow_on_resize(&mut self, on: bool) {
        self.reflow_on_resize = on;
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

    /// Grid-geometry generation — see the field doc. The UX engine's
    /// per-tick reconciler re-runs the active search when this
    /// changes (absolute match rows are stale after any resize).
    #[must_use]
    pub fn grid_generation(&self) -> u64 {
        self.grid_generation
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

    /// The screen buffer anchors capture against right now.
    fn active_screen(&self) -> ScreenBuffer {
        if self.use_alternate {
            ScreenBuffer::Alternate
        } else {
            ScreenBuffer::Primary
        }
    }

    /// Capture a content anchor at a VIEWPORT cell (`row` 0 = top of
    /// the view under the current scroll offset — the coordinate
    /// space mouse gestures and the renderer share). `None` when the
    /// row is out of bounds. Capture at gesture time, resolve at use
    /// time ([`Self::resolve_selection_anchor`]): the anchor tracks
    /// CONTENT through streaming scrollback growth and
    /// rewrap-on-resize, where a `(row, col)` pair goes stale the
    /// moment the grid moves under it.
    #[must_use]
    pub fn selection_anchor_at(&self, row: usize, col: usize) -> Option<SelectionAnchor> {
        let grid = self.grid();
        let top_abs = grid.scrollback_len().saturating_sub(self.scroll_offset);
        let anchor = grid.cell_anchor_at(top_abs + row, col)?;
        Some(SelectionAnchor {
            epoch: self.grid_epoch,
            screen: self.active_screen(),
            anchor,
        })
    }

    /// Resolve an anchor to an ABSOLUTE `(row, col)` (scrollback
    /// origin 0) under the current grid. `None` = the content is
    /// gone: the logical line was evicted from scrollback, the
    /// anchor belongs to the inactive screen buffer, or the grids
    /// were rebuilt (epoch mismatch). Callers treat `None` as "this
    /// selection no longer exists" — never as coordinates.
    #[must_use]
    pub fn resolve_selection_anchor(&self, a: SelectionAnchor) -> Option<(usize, usize)> {
        if a.epoch != self.grid_epoch || a.screen != self.active_screen() {
            return None;
        }
        self.grid().resolve_cell_anchor(a.anchor)
    }

    /// Resolve both endpoints of a selection and normalize to
    /// reading order (start ≤ end). `None` when EITHER endpoint's
    /// content is gone — a half-resolvable selection is rejected
    /// whole rather than clamped to garbage.
    #[must_use]
    pub fn resolve_selection_span(
        &self,
        a: SelectionAnchor,
        b: SelectionAnchor,
    ) -> Option<((usize, usize), (usize, usize))> {
        // Seqno-keyed memo — see the `selection_span_memo` field doc.
        // The renderer snapshot resolves the live span every vsync
        // and the engine reconciler every redraw tick; without the
        // memo each resolve was an O(rows) deque scan per anchor.
        if let Some(m) = *self.selection_span_memo.lock().unwrap()
            && m.seqno == self.seqno
            && m.a == a
            && m.b == b
        {
            return m.resolved;
        }
        let resolved = (|| {
            let pa = self.resolve_selection_anchor(a)?;
            let pb = self.resolve_selection_anchor(b)?;
            Some(if pb < pa { (pb, pa) } else { (pa, pb) })
        })();
        *self.selection_span_memo.lock().unwrap() = Some(SelectionSpanMemo {
            seqno: self.seqno,
            a,
            b,
            resolved,
        });
        resolved
    }

    /// Extract the text between two anchors, soft-wrap aware (the
    /// kitty/ghostty copy contract):
    ///
    /// * rows joined by the soft-wrap marker concatenate WITHOUT a
    ///   newline — a wrapped 100-char command copies as one line;
    /// * a hard line end trims that row's trailing blanks, then
    ///   emits `\n`;
    /// * wide-char continuation spacers (`width == 0`) are skipped,
    ///   never emitted as spaces.
    ///
    /// `None` when the anchors no longer resolve or the selected
    /// region holds only whitespace.
    #[must_use]
    pub fn extract_selection_text(
        &self,
        a: SelectionAnchor,
        b: SelectionAnchor,
    ) -> Option<String> {
        let (start, end) = self.resolve_selection_span(a, b)?;
        let grid = self.grid();
        let mut out = String::new();
        for row in start.0..=end.0 {
            let Some(line) = grid.line(row) else { break };
            let c0 = if row == start.0 { start.1 } else { 0 };
            let c1 = if row == end.0 {
                end.1.min(self.cols.saturating_sub(1))
            } else {
                self.cols.saturating_sub(1)
            };
            let seg_start = out.len();
            for cell in line.cells.iter().take(c1 + 1).skip(c0) {
                if cell.width == 0 {
                    continue;
                }
                cell.write_to(&mut out);
            }
            if !line.wrapped {
                // Trim THIS row's trailing blanks only — a wrapped
                // row's trailing cells are mid-logical-line content
                // and must survive the join.
                let trimmed = out[seg_start..].trim_end().len();
                out.truncate(seg_start + trimmed);
                if row < end.0 {
                    out.push('\n');
                }
            }
        }
        let trimmed_len = out.trim_end().len();
        out.truncate(trimmed_len);
        if out.is_empty() { None } else { Some(out) }
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

    /// Current window title (from OSC 0/2) — the queryable LEVEL.
    /// Frame consumers read title change-edges off
    /// [`drain_side_effects`](Self::drain_side_effects) instead.
    #[must_use]
    #[allow(dead_code)] // Typed read surface (XTWINOPS title-report + MCP exposure pending); tests exercise it.
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

    /// Drain the typed notification queue (OSC 9 / 777 / 99). Each
    /// element is one [`PendingNotification`] the terminal saw; the
    /// event loops dispatch them (tsuuchi on the fleet).
    /// Iterator-style instead of `Vec<_>` so callers can
    /// fire-and-forget each one without holding the whole batch in
    /// memory first.
    #[allow(dead_code)] // Wired into drain_side_effects in M4 stage 2; tests exercise it now.
    pub fn drain_notifications(&mut self) -> std::vec::Drain<'_, PendingNotification> {
        self.pending_notifications.drain(..)
    }

    /// Take the latest `ConEmu` OSC 9;4 progress update, if one arrived
    /// since the last take. Latest-wins (not a queue): the consumer
    /// renders current state, not history.
    pub fn take_progress(&mut self) -> Option<ProgressState> {
        self.progress.take()
    }

    /// THE M4 drain — one atomic, typed transfer of every side effect
    /// the VT engine accumulated since the previous drain. Both event
    /// loops call this once per frame and route the payload through
    /// `ux::apply_side_effects` (the single shared consumer); per-loop
    /// `take_bell` / `take_clipboard` / title-diff polling is banned
    /// by `tests/ux_unification.rs`.
    ///
    /// Pure state transfer: the same pre-state always drains the same
    /// value, and an immediately repeated drain yields
    /// `TerminalSideEffects::default()` (pinned by
    /// `drain_is_pure_state_transfer_and_second_drain_is_empty`).
    /// Title/cwd carry change-edges (`Some` only when set since the
    /// last drain); attention carries the `RequestAttention` rising
    /// edge while the queryable level stays on
    /// [`attention_requested`](Self::attention_requested).
    pub fn drain_side_effects(&mut self) -> crate::ux::TerminalSideEffects {
        crate::ux::TerminalSideEffects {
            title: if std::mem::replace(&mut self.title_changed, false) {
                self.title.clone()
            } else {
                None
            },
            bell: self.take_bell(),
            clipboard: self.take_clipboard(),
            notifications: std::mem::take(&mut self.pending_notifications),
            progress: self.take_progress(),
            cwd: if std::mem::replace(&mut self.cwd_changed, false) {
                self.cwd.clone()
            } else {
                None
            },
            attention: std::mem::replace(&mut self.attention_edge, false),
        }
    }

    /// Inject a deterministic wall-clock for mark timestamps. The
    /// production default is the real UNIX clock; tests pin a fixed
    /// fn so `PromptMark::at_unix_ms` is assertable. Survives RIS.
    #[cfg(test)]
    pub fn set_clock(&mut self, clock: fn() -> u128) {
        self.clock_unix_ms = clock;
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
    /// No indices = reset ALL 256 entries (M2 grew the palette from
    /// 16 to the full xterm 256). Listed indices in `0..256` reset to
    /// the compiled default palette; out-of-range entries are ignored.
    fn handle_osc_104_palette_reset(&mut self, params: &[&[u8]]) {
        if params.len() == 1 {
            self.ansi_colors = default_palette_256();
            self.dirty();
            return;
        }
        for p in &params[1..] {
            if let Ok(idx_str) = std::str::from_utf8(p)
                && let Ok(idx) = idx_str.parse::<usize>()
                && idx < 256
            {
                self.ansi_colors[idx] = default_palette_256()[idx];
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
        self.title_changed = true;
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
        self.cwd_changed = true;
    }

    /// OSC 8 — Hyperlink delimiter.
    ///
    /// Format: `ESC ] 8 ; <params> ; <URI> ST`. Empty URI (or a
    /// short-form sequence with only one param) ends the active
    /// hyperlink run; subsequent cells paint without underline-style
    /// hyperlinking until the next non-empty OSC 8.
    fn handle_osc_8_hyperlink(&mut self, params: &[&[u8]]) {
        if params.len() < 3 {
            self.active_link_id = NO_LINK_ID;
            return;
        }
        let uri = String::from_utf8_lossy(params[2]);
        self.active_link_id = if uri.is_empty() {
            NO_LINK_ID
        } else {
            let uri = uri.into_owned();
            self.intern_link(&uri)
        };
    }

    /// THE single notification-enqueue chokepoint. Every OSC 9 / 777 /
    /// 99 path routes here so the queue has ONE bound site (review
    /// 2026-06-12, critic-0). The queue is drained once per frame and
    /// EACH entry spawns an OS notification process on dispatch
    /// (`osascript` on macOS) — an unbounded queue under an OSC-flood
    /// (`printf '\e]9;x\a'` in a tight loop) would spawn thousands of
    /// processes + reaper threads in one drain, a fork-bomb-adjacent
    /// DoS that freezes the host. The cap (drop-oldest, keep newest)
    /// makes the queue length input-rate-independent, mirroring the
    /// kitty APC_MAX / sixel DCS bounds.
    fn push_notification(&mut self, notification: PendingNotification) {
        const MAX_PENDING_NOTIFICATIONS: usize = 64;
        if self.pending_notifications.len() >= MAX_PENDING_NOTIFICATIONS {
            // Drop the oldest so a flood can't grow the queue, but the
            // operator still sees the most recent N messages.
            self.pending_notifications.remove(0);
            tracing::warn!(
                cap = MAX_PENDING_NOTIFICATIONS,
                "notification queue at cap — dropped oldest pending notification"
            );
        }
        self.pending_notifications.push(notification);
    }

    /// OSC 9 — Desktop notification (iTerm2 / ghostty compat) plus
    /// the `ConEmu` `9;4` progress carve-out.
    ///
    /// Notification format: `ESC ] 9 ; <body> ST` (ST = `ESC \` or
    /// BEL). Empty body is a no-op (the spec lets `ESC ] 9 ; ST`
    /// mean a "bell-like ping" — we prefer the explicit BEL for that
    /// so the notification queue only carries real messages). vte
    /// splits the payload on `;`, so the body is re-joined from every
    /// remaining param — `ESC ] 9 ; a;b ST` notifies `a;b`.
    ///
    /// Progress format (`ConEmu`): `ESC ] 9 ; 4 ; st ; pr ST` — routed
    /// to the typed [`ProgressState`] lane, NEVER the notification
    /// queue (separate field; no constructor from one to the other).
    /// Other `ConEmu` `9;N` verbs (sleep, message, guimacro, …) keep
    /// the historical mado behavior: treated as a notification body.
    fn handle_osc_9_notification(&mut self, params: &[&[u8]]) {
        if params.len() < 2 || params[1].is_empty() {
            return;
        }
        // Route the `9;4` progress namespace BEFORE any length check.
        // A truncated `ESC]9;4 ST` (params `[b"9", b"4"]`, len 2) MUST
        // NOT fall through to the notification lane as body "4" — the
        // length-gated guard did exactly that (review 2026-06-12,
        // determinism-unrep-0 / cross-path-parity-0). The progress
        // handler trace-drops a missing/unknown state via its `other`
        // arm, so a bare `9;4` becomes a clean no-op, never a banner.
        if params[1] == b"4" {
            self.handle_osc_9_4_progress(params.get(2..).unwrap_or(&[]));
            return;
        }
        let body = join_osc_params(&params[1..]);
        tracing::debug!(%body, "OSC 9 notification");
        self.push_notification(PendingNotification {
            title: None,
            body,
            urgency: Urgency::Normal,
            group: None,
        });
    }

    /// `ConEmu` OSC 9;4 progress — `rest` is `[]`, `[st]`, or
    /// `[st, pr]`. `st`: 0=remove, 1=set, 2=error, 3=indeterminate,
    /// 4=paused. `pr`: integer percent, clamped to 100 at this parse
    /// boundary. An empty/unknown state (incl. the truncated `ESC]9;4`
    /// form) traces + drops via the `other` arm — never a silently-
    /// wrong value, and never the "4" notification leak the caller's
    /// length-gated routing once produced (review 2026-06-12).
    fn handle_osc_9_4_progress(&mut self, rest: &[&[u8]]) {
        let pct = rest
            .get(1)
            .and_then(|p| std::str::from_utf8(p).ok())
            .and_then(|s| s.parse::<u16>().ok())
            .map(|v| u8::try_from(v.min(100)).unwrap_or(100));
        let state = match rest.first().copied() {
            Some(b"0") => ProgressState::Remove,
            Some(b"1") => ProgressState::Set { pct: pct.unwrap_or(0) },
            Some(b"2") => ProgressState::Error { pct },
            Some(b"3") => ProgressState::Indeterminate,
            Some(b"4") => ProgressState::Paused { pct },
            other => {
                tracing::trace!(?other, "OSC 9;4: unknown progress state, ignoring");
                return;
            }
        };
        tracing::trace!(?state, "OSC 9;4 progress");
        self.progress = Some(state);
    }

    /// OSC 777 — urxvt extension dispatch; only the `notify` verb is
    /// implemented: `ESC ] 777 ; notify ; <title> ; <body> ST`
    /// (rxvt-unicode / foot / ghostty compat). Other 777 verbs are
    /// trace-dropped. The body is re-joined from the remaining params
    /// so a `;` inside the body survives vte's split.
    fn handle_osc_777_notify(&mut self, params: &[&[u8]]) {
        if params.len() < 3 || params[1] != b"notify" {
            tracing::trace!(?params, "OSC 777: non-notify verb, ignoring");
            return;
        }
        let title = String::from_utf8_lossy(params[2]).into_owned();
        let body = join_osc_params(&params[3..]);
        tracing::debug!(%title, %body, "OSC 777 notification");
        self.push_notification(PendingNotification {
            title: Some(title),
            body,
            urgency: Urgency::Normal,
            group: None,
        });
    }

    /// OSC 99 — kitty desktop notification protocol:
    /// `ESC ] 99 ; <metadata> ; <payload> ST` where metadata is
    /// colon-separated `k=v` pairs. Typed honestly: `i=` (id →
    /// group/chain identity), `d=` (done flag, default 1 — `d=0`
    /// chains fragments across escapes), `p=` (payload kind: `title`
    /// default / `body`), `u=` (urgency 0/1/2), `e=1` (base64
    /// payload). Every other key — and unknown payload kinds like
    /// `close`/`icon`/actions — is trace-ignored, never guessed at.
    fn handle_osc_99_kitty(&mut self, params: &[&[u8]]) {
        let metadata = params.get(1).copied().unwrap_or(b"");
        let mut id: Option<String> = None;
        let mut done = true;
        let mut payload_kind: &[u8] = b"title";
        let mut urgency: Option<Urgency> = None;
        let mut base64_payload = false;
        for pair in metadata.split(|&b| b == b':').filter(|p| !p.is_empty()) {
            let mut kv = pair.splitn(2, |&b| b == b'=');
            let (Some(key), Some(value)) = (kv.next(), kv.next()) else {
                tracing::trace!(pair = %String::from_utf8_lossy(pair), "OSC 99: bare metadata key, ignoring");
                continue;
            };
            match key {
                b"i" => id = Some(String::from_utf8_lossy(value).into_owned()),
                b"d" => done = value != b"0",
                b"p" => payload_kind = value,
                b"u" => {
                    urgency = match value {
                        b"0" => Some(Urgency::Low),
                        b"1" => Some(Urgency::Normal),
                        b"2" => Some(Urgency::Critical),
                        other => {
                            tracing::trace!(u = %String::from_utf8_lossy(other), "OSC 99: unknown urgency, ignoring");
                            None
                        }
                    };
                }
                b"e" => base64_payload = value == b"1",
                other => {
                    tracing::trace!(key = %String::from_utf8_lossy(other), "OSC 99: unimplemented metadata key, ignoring");
                }
            }
        }
        let raw_payload = join_osc_params(&params[2..]);
        let payload = if base64_payload {
            let Some(decoded) = base64_decode(raw_payload.as_bytes()) else {
                tracing::trace!("OSC 99: invalid base64 payload, dropping escape");
                return;
            };
            decoded
        } else {
            raw_payload
        };

        // Resume or open the chain slot. A fragment naming a
        // different id than the in-flight chain drops the old chain
        // WITH a trace — kitty serializes chains, so an interleave is
        // a misbehaving client, not silent data to merge.
        let mut pending = match self.pending_osc99.take() {
            Some(p) if p.id == id => p,
            Some(p) => {
                tracing::trace!(dropped_id = ?p.id, new_id = ?id, "OSC 99: chain interrupted by new id, dropping old chain");
                Osc99Pending { id, ..Osc99Pending::default() }
            }
            None => Osc99Pending { id, ..Osc99Pending::default() },
        };
        if let Some(u) = urgency {
            pending.urgency = Some(pending.urgency.map_or(u, |cur| cur.max(u)));
        }
        // Bound each accumulated chain field: a `d=0` chain that never
        // sends `d=1` (`\e]99;d=0:p=body;<chunk>\e\\` forever) would
        // grow `pending.body` without limit and hold it across every
        // feed — unbounded memory for a notification that may never
        // fire, and one the per-frame drain never sees (review
        // 2026-06-12, critic-3). Once a field passes the cap, further
        // fragments for it trace-drop; the chain still finalizes on a
        // later `d=1` with whatever fit.
        const MAX_OSC99_FIELD: usize = 16 * 1024;
        let append_capped = |field: &mut Option<String>, frag: &str, which: &str| {
            let cur = field.get_or_insert_with(String::new);
            if cur.len() >= MAX_OSC99_FIELD {
                tracing::warn!(cap = MAX_OSC99_FIELD, which, "OSC 99 chain field at cap — dropping fragment");
            } else {
                cur.push_str(frag);
            }
        };
        match payload_kind {
            b"title" => {
                if !payload.is_empty() {
                    append_capped(&mut pending.title, &payload, "title");
                }
            }
            b"body" => {
                if !payload.is_empty() {
                    append_capped(&mut pending.body, &payload, "body");
                }
            }
            other => {
                tracing::trace!(p = %String::from_utf8_lossy(other), "OSC 99: unimplemented payload kind, ignoring fragment");
            }
        }
        if done {
            let notification = pending.into_notification();
            if notification.title.is_none() && notification.body.is_empty() {
                tracing::trace!("OSC 99: chain finalized empty, dropping");
                return;
            }
            tracing::debug!(?notification, "OSC 99 notification");
            self.push_notification(notification);
        } else {
            self.pending_osc99 = Some(pending);
        }
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
    /// double-width variant. The full 0..256 range is mutable since
    /// M2; indices ≥ 256 are silently ignored.
    fn handle_osc_4_palette(&mut self, params: &[&[u8]]) {
        if params.len() < 3 {
            return;
        }
        let Some(idx) = parse_palette_index(params[1]) else {
            return;
        };
        if idx >= 256 {
            return;
        }
        if params[2] == b"?" {
            let resp = osc4_rgb_query_response(idx, self.ansi_colors[idx]);
            self.response_bytes.extend_from_slice(resp.as_bytes());
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
                // Rising edge feeds the drain exactly once per
                // request; RequestAttention=0 clears the level but
                // never un-fires an already-queued edge.
                self.attention_edge |= flag;
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
        self.prompt_marks.record(grid_row, kind, (self.clock_unix_ms)());
        // `OSC 133 ; D ; <code>` — the optional third param is the
        // command's exit status; stamp the D mark + back-fill the
        // zone-opening C mark. Non-numeric codes trace + drop.
        if kind == crate::prompt_mark::PromptKind::CommandEnd
            && let Some(raw) = params.get(2).filter(|p| !p.is_empty())
        {
            if let Some(code) = std::str::from_utf8(raw).ok().and_then(|s| s.parse::<i32>().ok()) {
                self.prompt_marks.apply_exit_status(code);
            } else {
                tracing::trace!(code = %String::from_utf8_lossy(raw), "OSC 133 D: non-numeric exit code, ignoring");
            }
        }
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
        // RIS palette policy: only the theme-owned base 16 survive —
        // indices 16..=255 return to the computed cube/grayscale
        // defaults, so an app's OSC 4 overrides of the extended
        // palette cannot outlive `reset` (xterm/kitty restore the
        // configured palette on RIS; an operator's `reset` after a
        // crashed app must actually restore colors).
        let mut ansi_colors = default_palette_256();
        ansi_colors[..16].copy_from_slice(&self.ansi_colors[..16]);
        // Operator config, not VT state — survives RIS like the
        // scrollback cap does (M2: behavior.reflow_on_resize).
        let reflow_on_resize = self.reflow_on_resize;
        // Environmental wiring, not VT state — an injected test clock
        // must keep ticking across a RIS mid-scenario.
        let clock_unix_ms = self.clock_unix_ms;
        // The rebuild restarts both grids' LogicalLineId counters at
        // 0 — bump the epoch so pre-reset SelectionAnchors can never
        // alias-resolve onto post-reset lines.
        let grid_epoch = self.grid_epoch.wrapping_add(1);
        *self = Terminal::with_scrollback(cols, rows, max_scrollback);
        self.default_fg = default_fg;
        self.default_bg = default_bg;
        self.pen_fg = default_fg;
        self.pen_bg = default_bg;
        self.ansi_colors = ansi_colors;
        self.reflow_on_resize = reflow_on_resize;
        self.clock_unix_ms = clock_unix_ms;
        self.grid_epoch = grid_epoch;
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
        self.pen_attrs = Attrs::NONE;
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
                cell.width = 1;
                cell.extra = None;
                // Default style (WHITE on BLACK, no attrs) + no link.
                cell.style_id = DEFAULT_STYLE_ID;
                cell.link_id = NO_LINK_ID;
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
            self.store_rgba_image(id, rgba_data, w, h);

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

    /// The single texture-upload entry point: store decoded RGBA pixels
    /// as a `KittyImage` keyed by `id`. Both producers — Kitty graphics
    /// transmission and sixel decode — funnel through here, so the GPU
    /// upload path (`sync_kitty_images`) has exactly one image source of
    /// truth. Solve-once: sixel is a second producer, not a parallel path.
    /// Free decoded RGBA for images whose LAST placement was just
    /// pruned. Without this, a scrolled-off sixel (or kitty image)
    /// orphans its W×H×4 texture in `images` forever — the placement
    /// is pruned on rewrap but the map entry is never removed, and an
    /// auto-assigned sixel id has no deletable handle, so no `delete`
    /// escape can ever target it (review 2026-06-12, critic-2).
    ///
    /// Scoped to ids whose placement was DROPPED this pass: a kitty
    /// image transmitted-without-display (`t`, never `p`) is never in
    /// any placement, so it never appears here and is never evicted —
    /// the gap between transmit and a later place is preserved.
    fn gc_orphaned_images(&mut self, dropped_image_ids: &[u32]) {
        for id in dropped_image_ids {
            if !self.image_placements.iter().any(|p| p.image_id == *id) {
                self.images.remove(id);
            }
        }
    }

    fn store_rgba_image(&mut self, id: u32, rgba: Vec<u8>, width: u32, height: u32) {
        self.images.insert(
            id,
            KittyImage {
                id,
                data: rgba,
                width,
                height,
                seqno: self.seqno,
            },
        );
    }

    /// Decode a sixel DCS payload into RGBA and feed the shared texture
    /// upload path (`store_rgba_image`) + place it at the cursor. A
    /// malformed payload is rejected with a typed trace — NEVER a panic;
    /// the typed error is the only failure surface (no `unwrap`, no
    /// `todo!`). Per Kitty placement semantics, the image lands at the
    /// cursor and rides the same `image_placements` re-anchoring the Kitty
    /// path uses, so scrolling moves it identically.
    fn decode_and_place_sixel(&mut self, payload: &[u8]) {
        let (p1, p2, p3) = self.sixel_dcs_params;
        let settings = icy_sixel::DcsSettings::new(p1, p2, p3);
        let img = match icy_sixel::SixelImage::decode_from_dcs(payload, settings) {
            Ok(img) => img,
            Err(e) => {
                tracing::warn!(error = %e, "sixel decode rejected (malformed payload)");
                return;
            }
        };
        // icy_sixel caps dimensions to SIXEL_{WIDTH,HEIGHT}_LIMIT, so the
        // usize→u32 narrowing always fits; try_from makes that explicit and
        // routes an impossible over-limit value to the same typed-reject
        // path rather than silently truncating.
        let (Ok(w), Ok(h)) = (u32::try_from(img.width), u32::try_from(img.height)) else {
            tracing::warn!(
                w = img.width,
                h = img.height,
                "sixel dimensions exceed u32 — rejected"
            );
            return;
        };
        if w == 0 || h == 0 || img.pixels.is_empty() {
            tracing::warn!(w, h, "sixel decoded to empty image — not placed");
            return;
        }
        let id = self.next_image_id;
        self.next_image_id += 1;
        self.seqno += 1;
        self.store_rgba_image(id, img.pixels, w, h);
        self.place_decoded_image_at_cursor(id);
        self.dirty();
        tracing::debug!(id, w, h, "sixel decoded + placed");
    }

    /// Push a default placement (cols/rows auto from image, z=0) for a
    /// decoded image at the current cursor. The sixel path has no Kitty
    /// `c=`/`r=`/`z=` params, so every geometry field defaults — the
    /// render derives display size from the texture dimensions.
    ///
    /// The cursor is NOT advanced past the image (no sixel-scrolling).
    /// Converting image-height-px to rows needs cell-height-in-px, which
    /// is renderer-owned and not threaded into this VT engine — so text
    /// emitted right after a sixel can overdraw it. Scope documented at
    /// `caps::SIXEL_GRAPHICS_IMPLEMENTED` (review 2026-06-12,
    /// correctness-0).
    fn place_decoded_image_at_cursor(&mut self, image_id: u32) {
        self.image_placements.push(ImagePlacement {
            image_id,
            placement_id: 0,
            col: self.cursor.col,
            row: self.cursor.row,
            cols: 0,
            rows: 0,
            x_offset: 0,
            y_offset: 0,
            src_x: 0,
            src_y: 0,
            src_width: 0,
            src_height: 0,
            z_index: 0,
        });
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

    /// M2 soft-wrap stamping: the cursor just moved onto a
    /// continuation row via DECAWM autowrap (put_char's two wrap
    /// sites — pending-wrap consumption and the wide-char early
    /// wrap). Mark the physical row ABOVE the cursor `wrapped =
    /// true` and propagate its [`LogicalLineId`] onto the cursor's
    /// row, so the two physical rows are ONE logical line for
    /// rewrap-on-resize and mark re-anchoring. Hard newlines never
    /// come through here, so they keep `wrapped = false` and the
    /// continuation row's own fresh id — exactly the "fresh id per
    /// hard line" contract on [`LogicalLineId`].
    ///
    /// Works in grid-absolute indices: when the wrap scrolled
    /// (cursor was on the last row), the wrapped-from row now sits
    /// in scrollback and `cursor.row` stayed put — `abs - 1` still
    /// addresses it.
    fn stamp_soft_wrap(&mut self) {
        let row = self.cursor.row;
        let grid = self.grid_mut();
        let cur_abs = grid.scrollback_len() + row;
        let Some(prev_abs) = cur_abs.checked_sub(1) else {
            // rows == 1 with zero scrollback: the wrapped-from row
            // was evicted outright — nothing left to stamp.
            return;
        };
        let id = grid.rows[prev_abs].logical_id;
        grid.rows[prev_abs].wrapped = true;
        grid.rows[cur_abs].logical_id = id;
    }

    /// DECAWM autowrap row advance — `newline()` plus the soft-wrap
    /// stamp, gated on the cursor actually reaching a fresh next row.
    /// The stamp's contract is "mark the row the text wrapped FROM
    /// onto the row it wraps TO": that holds when the cursor advanced
    /// one row, and when a scroll at the region bottom made room (the
    /// wrapped-from row now sits directly above the cursor). When the
    /// cursor sits BELOW an active DECSTBM region, `newline()` scrolls
    /// the region WITHOUT moving the cursor — stamping there would
    /// mark an unrelated row above the cursor wrapped and overwrite
    /// the cursor row's logical id (review finding 2026-06-12).
    fn wrap_to_next_row(&mut self) {
        let row_before = self.cursor.row;
        self.newline();
        let advanced = self.cursor.row == row_before + 1;
        let scrolled_at_bottom =
            self.cursor.row == row_before && row_before == self.scroll_bottom;
        if advanced || scrolled_at_bottom {
            self.stamp_soft_wrap();
        }
    }

    fn put_char(&mut self, ch: char) {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(1);

        // Handle pending wrap from previous character at end of line
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor.col = 0;
            self.wrap_to_next_row();
        }

        // Wide chars need 2 columns — wrap early if they won't fit
        if char_width == 2 && self.cursor.col + 1 >= self.cols {
            if self.auto_wrap {
                self.cursor.col = 0;
                self.wrap_to_next_row();
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
            // P32 + M2 — intern the current pen state into the style
            // table. Adjacent cells with identical pen state share
            // a u16 ID (the table dedups). Post-shrink, style_id is
            // the cell's ONLY styling storage.
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
                let id = self.intern_style(style);
                self.cached_style = Some(style);
                self.cached_style_id = id;
                id
            };
            let link_id = self.active_link_id;
            let cell = self.grid_mut().cell_mut(row, col);
            cell.ch = ch;
            cell.style_id = style_id;
            cell.link_id = link_id;
            cell.extra = None;
            cell.width = char_width as u8;

            // Wide chars occupy 2 cells — mark next cell as continuation
            if char_width == 2 && col + 1 < self.cols {
                let cont = self.grid_mut().cell_mut(row, col + 1);
                cont.ch = ' ';
                cont.width = 0;
                cont.style_id = style_id;
                cont.link_id = link_id;
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
            // vte groups colon sub-params with their parameter: `4:3`
            // arrives as ONE slice `[4, 3]`, `58:2::255:0:0` as
            // `[58, 2, 0, 255, 0, 0]`. Semicolon-separated params come
            // as separate single-element slices.
            let slice = match iter.next() {
                Some(slice) => slice,
                None => break,
            };
            let param = slice[0];

            match param {
                0 => {
                    self.pen_fg = self.default_fg;
                    self.pen_bg = self.default_bg;
                    self.pen_attrs = Attrs::NONE;
                }
                1 => self.pen_attrs.flags.insert(AttrFlags::BOLD),
                2 => self.pen_attrs.flags.insert(AttrFlags::DIM),
                3 => self.pen_attrs.flags.insert(AttrFlags::ITALIC),
                4 => {
                    // SGR 4 / 4:N — underline style sub-param wire
                    // (kitty/xterm extension): 4:0 none, 4:1 single,
                    // 4:2 double, 4:3 curly, 4:4 dotted, 4:5 dashed.
                    // Plain `4` (no sub-param) = single. Unknown
                    // sub-params degrade to Single (an underline WAS
                    // requested; the style refinement is best-effort).
                    self.pen_attrs.underline = match slice.get(1).copied() {
                        None => UnderlineStyle::Single,
                        Some(0) => UnderlineStyle::None,
                        Some(1) => UnderlineStyle::Single,
                        Some(2) => UnderlineStyle::Double,
                        Some(3) => UnderlineStyle::Curly,
                        Some(4) => UnderlineStyle::Dotted,
                        Some(5) => UnderlineStyle::Dashed,
                        Some(_) => UnderlineStyle::Single,
                    };
                }
                5 => self.pen_attrs.flags.insert(AttrFlags::BLINK),
                7 => self.pen_attrs.flags.insert(AttrFlags::INVERSE),
                8 => self.pen_attrs.flags.insert(AttrFlags::HIDDEN),
                9 => self.pen_attrs.flags.insert(AttrFlags::STRIKETHROUGH),
                // SGR 21 — double underline (ECMA-48; kitty wire).
                // Sibling of the 4:2 sub-param form above.
                21 => self.pen_attrs.underline = UnderlineStyle::Double,
                22 => {
                    // SGR 22 resets both bold and dim
                    self.pen_attrs.flags.remove(AttrFlags::BOLD);
                    self.pen_attrs.flags.remove(AttrFlags::DIM);
                }
                23 => self.pen_attrs.flags.remove(AttrFlags::ITALIC),
                24 => self.pen_attrs.underline = UnderlineStyle::None,
                25 => self.pen_attrs.flags.remove(AttrFlags::BLINK),
                27 => self.pen_attrs.flags.remove(AttrFlags::INVERSE),
                28 => self.pen_attrs.flags.remove(AttrFlags::HIDDEN),
                29 => self.pen_attrs.flags.remove(AttrFlags::STRIKETHROUGH),
                30..=37 => self.pen_fg = self.ansi_colors[(param - 30) as usize],
                38 => self.parse_extended_color(&mut iter, true),
                39 => self.pen_fg = self.default_fg,
                40..=47 => self.pen_bg = self.ansi_colors[(param - 40) as usize],
                48 => self.parse_extended_color(&mut iter, false),
                49 => self.pen_bg = self.default_bg,
                53 => self.pen_attrs.flags.insert(AttrFlags::OVERLINE),
                55 => self.pen_attrs.flags.remove(AttrFlags::OVERLINE),
                58 => {
                    // SGR 58 — underline colour (mirrors 38/48's
                    // colour grammar). Malformed payloads degrade to
                    // UnderlineColor::Default, never corrupt the pen.
                    self.pen_attrs.underline_color =
                        parse_underline_color(slice, &mut iter);
                }
                59 => self.pen_attrs.underline_color = UnderlineColor::Default,
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

/// Parse the SGR 58 underline-colour payload — the sibling of
/// `parse_extended_color` for the `58` arm. Handles BOTH wire forms:
///
///   - **Colon sub-params** (`58:5:N`, `58:2::r:g:b`, `58:2:r:g:b`):
///     vte delivers everything in ONE slice — `[58, 5, N]` /
///     `[58, 2, 0, r, g, b]` (an empty colorspace sub-param parses as
///     0) / `[58, 2, r, g, b]`.
///   - **Semicolon params** (`58;5;N`, `58;2;r;g;b`): the mode and
///     channels arrive as the FOLLOWING params — consumed from `iter`,
///     mirroring `parse_extended_color`.
///
/// Malformed payloads degrade to [`UnderlineColor::Default`] (the
/// `map_or`-style defensiveness of `parse_extended_color`): the pen
/// keeps rendering, just without the colour refinement.
fn parse_underline_color(
    slice: &[u16],
    iter: &mut vte::ParamsIter<'_>,
) -> UnderlineColor {
    if slice.len() >= 2 {
        // Colon sub-param form — everything is in `slice`.
        match slice[1] {
            5 => slice
                .get(2)
                .map_or(UnderlineColor::Default, |&n| UnderlineColor::Indexed(n as u8)),
            2 => {
                let rgb = &slice[2..];
                match rgb.len() {
                    // `58:2::r:g:b` — leading colorspace-id sub-param
                    // (empty → 0). Skip it.
                    4.. => UnderlineColor::Rgb(Rgb::new(
                        rgb[1] as u8,
                        rgb[2] as u8,
                        rgb[3] as u8,
                    )),
                    // `58:2:r:g:b` — no colorspace id.
                    3 => UnderlineColor::Rgb(Rgb::new(
                        rgb[0] as u8,
                        rgb[1] as u8,
                        rgb[2] as u8,
                    )),
                    _ => UnderlineColor::Default,
                }
            }
            _ => UnderlineColor::Default,
        }
    } else {
        // Semicolon form — mode + channels are the following params.
        match iter.next().map(|s| s[0]) {
            Some(5) => iter
                .next()
                .map_or(UnderlineColor::Default, |s| UnderlineColor::Indexed(s[0] as u8)),
            Some(2) => {
                let r = iter.next().map_or(0, |s| s[0] as u8);
                let g = iter.next().map_or(0, |s| s[0] as u8);
                let b = iter.next().map_or(0, |s| s[0] as u8);
                UnderlineColor::Rgb(Rgb::new(r, g, b))
            }
            _ => UnderlineColor::Default,
        }
    }
}

/// DECRQSS `m` (SGR) report payload — the pen state rendered as SGR
/// parameters, trailing final byte `m` included. The `Display` impl
/// IS the typed wire emitter (TYPED EMISSION: the format strings are
/// the serialization contract, not free-form composition).
///
/// fg/bg report as direct-RGB (`38:2::r:g:b`) because the pen
/// resolves indexed colours to RGB at SGR-parse time — the report is
/// the pen's truth, not a reconstruction of the original wire.
/// `None` = the default colour, which emits no parameter (matching
/// xterm: the leading `0` already implies defaults).
struct SgrReport {
    fg: Option<Color>,
    bg: Option<Color>,
    attrs: Attrs,
}

impl fmt::Display for SgrReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0")?;
        // The ONE flag/code registry (AttrFlags::ALL) drives the
        // reply — the former local FLAG_PARAMS dual is deleted (M3
        // review 2026-06-12).
        for (flag, code) in AttrFlags::ALL {
            if self.attrs.flags.contains(flag) {
                write!(f, ";{code}")?;
            }
        }
        // 4:N sub-param wire — the styled-underline probe's target.
        match self.attrs.underline {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => f.write_str(";4")?,
            UnderlineStyle::Double => f.write_str(";4:2")?,
            UnderlineStyle::Curly => f.write_str(";4:3")?,
            UnderlineStyle::Dotted => f.write_str(";4:4")?,
            UnderlineStyle::Dashed => f.write_str(";4:5")?,
        }
        match self.attrs.underline_color {
            UnderlineColor::Default => {}
            UnderlineColor::Indexed(n) => write!(f, ";58:5:{n}")?,
            UnderlineColor::Rgb(c) => write!(f, ";58:2::{}:{}:{}", c.r, c.g, c.b)?,
        }
        if let Some(c) = self.fg {
            write!(f, ";38:2::{}:{}:{}", c.r, c.g, c.b)?;
        }
        if let Some(c) = self.bg {
            write!(f, ";48:2::{}:{}:{}", c.r, c.g, c.b)?;
        }
        f.write_str("m")
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
            // Sixel: DCS P1 ; P2 ; P3 q  — P1 = pixel aspect ratio,
            // P2 = background mode, P3 = grid size. Captured here so
            // unhook can hand icy_sixel a faithful DcsSettings.
            self.dcs_handler = Some(DcsHandler::Sixel);
            self.sixel_buffer = Some(Vec::new());
            self.sixel_buffer_overflow = false;
            let mut it = params.iter();
            let first = |o: Option<&[u16]>| o.and_then(|s| s.first().copied());
            self.sixel_dcs_params = (first(it.next()), first(it.next()), first(it.next()));
        } else {
            tracing::trace!(?intermediates, action = %action, "unhandled DCS hook");
            let _ = params;
        }
    }
    fn put(&mut self, byte: u8) {
        match self.dcs_handler {
            Some(DcsHandler::Decrqss(ref mut buf)) => buf.push(byte),
            Some(DcsHandler::Sixel) => {
                // Bound the pre-decode accumulation: a giant or never-
                // `unhook`'d sixel DCS must not grow `sixel_buffer`
                // without limit. 8 MiB is far beyond any legitimate
                // decoded frame (icy_sixel caps decoded dims well below
                // this), so passing it means a misbehaving stream —
                // drop the partial, poison the sequence, and let
                // `unhook` reject it (review 2026-06-12, critic-1;
                // mirrors APC_MAX).
                const SIXEL_DCS_MAX: usize = 8 * 1024 * 1024;
                if let Some(ref mut buf) = self.sixel_buffer {
                    if buf.len() >= SIXEL_DCS_MAX {
                        tracing::warn!(
                            len = buf.len(),
                            "sixel DCS payload exceeded bound — dropping sequence"
                        );
                        self.sixel_buffer = None;
                        self.sixel_buffer_overflow = true;
                    } else {
                        buf.push(byte);
                    }
                }
            }
            None => {}
        }
    }
    fn unhook(&mut self) {
        match self.dcs_handler {
            Some(DcsHandler::Decrqss(ref query)) => {
                let response = match query.as_slice() {
                    // DECRQSS `m` (SGR report) — PEN-DERIVED (M3).
                    // The standard styled-underline probe is "send
                    // SGR 4:3, DECRQSS m, look for 4:3 in the reply";
                    // this report renders the live pen (flags, 4:N
                    // underline style, 58 underline colour, 53
                    // overline, non-default fg/bg) through the typed
                    // SgrReport Display surface, so the
                    // STYLED_UNDERLINE_IMPLEMENTED cap is backed by
                    // real engine behaviour.
                    b"m" => {
                        let report = SgrReport {
                            fg: (self.pen_fg != self.default_fg).then_some(self.pen_fg),
                            bg: (self.pen_bg != self.default_bg).then_some(self.pen_bg),
                            attrs: self.pen_attrs,
                        };
                        let mut out = Vec::with_capacity(48);
                        out.extend_from_slice(b"\x1bP1$r");
                        out.extend_from_slice(report.to_string().as_bytes());
                        out.extend_from_slice(b"\x1b\\");
                        out
                    }
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
                if std::mem::take(&mut self.sixel_buffer_overflow) {
                    // The payload blew past SIXEL_DCS_MAX in `put()` —
                    // the partial was already dropped. Reject the whole
                    // sequence with one typed trace, never a partial
                    // decode (review 2026-06-12, critic-1).
                    tracing::warn!("sixel DCS rejected — payload exceeded bound");
                    self.sixel_buffer = None;
                } else if let Some(data) = self.sixel_buffer.take() {
                    if !data.is_empty() {
                        // The decode path (`images` + `image_placements`)
                        // is the sole source of truth — no raw-payload
                        // audit Vec is retained (review 2026-06-12,
                        // correctness-1: it was dead weight that cloned
                        // every payload alongside the decoded texture).
                        self.decode_and_place_sixel(&data);
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
            b"99"      => self.handle_osc_99_kitty(params),
            b"104"     => self.handle_osc_104_palette_reset(params),
            b"110"     => self.handle_osc_110_fg_reset(),
            b"111"     => self.handle_osc_111_bg_reset(),
            b"112"     => self.handle_osc_112_cursor_reset(),
            b"133"     => self.handle_osc_133_shell_integration(params),
            b"777"     => self.handle_osc_777_notify(params),
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
                    self.pen_attrs = Attrs::NONE;
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

/// Build the OSC 4 palette-query response — same `rgb:` doubling as
/// [`osc_rgb_query_response`] but with the palette index echoed
/// between the OSC id and the colour, per xterm:
/// `ESC ] 4 ; <idx> ; rgb:RRRR/GGGG/BBBB ESC \`.
fn osc4_rgb_query_response(idx: usize, c: Color) -> String {
    format!(
        "\x1b]4;{idx};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\",
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
        assert!(cell.attrs(term.styles()).flags.contains(AttrFlags::BOLD));
        assert_eq!(cell.fg(term.styles()), ANSI_COLORS[1]);
    }

    #[test]
    fn sgr_reset() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[1;31mA\x1b[0mB");
        let a = term.cell(0, 0);
        assert!(a.attrs(term.styles()).flags.contains(AttrFlags::BOLD));
        let b = term.cell(0, 1);
        assert_eq!(b.attrs(term.styles()), Attrs::NONE);
        assert_eq!(b.fg(term.styles()), Color::WHITE);
    }

    #[test]
    fn sgr_dim() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[2mX");
        let cell = term.cell(0, 0);
        assert!(cell.attrs(term.styles()).flags.contains(AttrFlags::DIM));
        // SGR 22 resets both bold and dim
        term.feed(b"\x1b[22mY");
        let cell = term.cell(0, 1);
        assert!(!cell.attrs(term.styles()).flags.contains(AttrFlags::DIM));
    }

    #[test]
    fn sgr_hidden() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[8mX");
        let cell = term.cell(0, 0);
        assert!(cell.attrs(term.styles()).flags.contains(AttrFlags::HIDDEN));
        // SGR 28 resets hidden
        term.feed(b"\x1b[28mY");
        let cell = term.cell(0, 1);
        assert!(!cell.attrs(term.styles()).flags.contains(AttrFlags::HIDDEN));
    }

    #[test]
    fn sgr_truecolor() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[38;2;100;150;200mX");
        assert_eq!(term.cell(0, 0).fg(term.styles()), Color::new(100, 150, 200));
    }

    #[test]
    fn sgr_256color() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[38;5;196mX");
        assert_eq!(
            term.cell(0, 0).fg(term.styles()),
            ansi_256_color(196, &default_palette_256())
        );
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
            .cells
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
    fn attr_flags_bitflag_operations() {
        let mut flags = AttrFlags::NONE;
        assert!(flags.is_empty());
        flags.insert(AttrFlags::BOLD);
        flags.insert(AttrFlags::ITALIC);
        assert!(flags.contains(AttrFlags::BOLD));
        assert!(flags.contains(AttrFlags::ITALIC));
        assert!(!flags.contains(AttrFlags::OVERLINE));
        flags.remove(AttrFlags::BOLD);
        assert!(!flags.contains(AttrFlags::BOLD));
        assert!(flags.contains(AttrFlags::ITALIC));
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
        // Single source of truth: caps::TerminalCaps::PRIMARY_DA, now
        // ?62;4;22c — the `4` advertises sixel since the decode path landed.
        assert_eq!(
            response.as_slice(),
            crate::caps::TerminalCaps::PRIMARY_DA
        );
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
            term.cell(0, 0).hyperlink(term.links()),
            Some("https://example.com")
        );
        assert_eq!(
            term.cell(0, 3).hyperlink(term.links()),
            Some("https://example.com")
        );
        // Cell after the hyperlink should not
        assert!(term.cell(0, 5).hyperlink(term.links()).is_none());
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

    /// Construct a placement carrying just the fields the z-band split
    /// reads. `id` doubles as the transmission-order marker so a test can
    /// assert stable ordering within a band.
    fn placement_with_z(id: u32, z: i32) -> ImagePlacement {
        ImagePlacement {
            image_id: id,
            placement_id: 0,
            col: 0,
            row: 0,
            cols: 0,
            rows: 0,
            x_offset: 0,
            y_offset: 0,
            src_x: 0,
            src_y: 0,
            src_width: 0,
            src_height: 0,
            z_index: z,
        }
    }

    #[test]
    fn z_band_split_below_above_and_stable_within_band() {
        // M3-C3: two placements over the same cells, one z=-1 (below the
        // text scene) and one z=2 (above). The render path draws each band
        // separately — `below` between the bg-rect pass and the glyph pass,
        // `above` after the glyph pass — so asserting the partition's two
        // Vecs pins the draw-order seam mechanically (no headless GPU).
        // Pixel-asserting the composited frame is the brittle alternative;
        // the band partition IS the instance-buffer fill order, which IS the
        // GPU draw order, so this is a load-bearing seam, not a proxy.
        let placements = vec![
            placement_with_z(10, 2),  // above
            placement_with_z(11, -1), // below
        ];
        let (below, above) = partition_placements_by_z(&placements);
        assert_eq!(below.len(), 1, "exactly one below-text placement");
        assert_eq!(below[0].image_id, 11);
        assert_eq!(above.len(), 1, "exactly one above-text placement");
        assert_eq!(above[0].image_id, 10);
    }

    #[test]
    fn z_band_split_orders_within_band_by_z_then_transmission() {
        // z=0 is ABOVE (>= 0 boundary); equal-z keeps transmission order.
        let placements = vec![
            placement_with_z(1, 5),   // above, higher z
            placement_with_z(2, 0),   // above, lower z
            placement_with_z(3, -3),  // below, lower z
            placement_with_z(4, -1),  // below, higher z
            placement_with_z(5, 0),   // above, same z as id=2 — drawn AFTER it
        ];
        let (below, above) = partition_placements_by_z(&placements);

        // below: ascending z (-3 then -1)
        assert_eq!(
            below.iter().map(|p| p.image_id).collect::<Vec<_>>(),
            vec![3, 4],
            "below band ordered by ascending z"
        );
        // above: ascending z (0,0 then 5); the two z=0 keep transmission order (2 before 5)
        assert_eq!(
            above.iter().map(|p| p.image_id).collect::<Vec<_>>(),
            vec![2, 5, 1],
            "above band ordered by ascending z, equal-z stable in transmission order"
        );
    }

    #[test]
    fn z_band_split_from_parsed_apc_z_param() {
        // End-to-end: z= APC param parses into z_index and lands in the
        // correct band. z=-1 below, z=2 above.
        let mut term = Terminal::new(80, 24);
        let rgba = [255, 0, 0, 255];
        let b64 = base64_encode(&rgba);
        let below = format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,z=-1;{b64}\x1b\\");
        let above = format!("\x1b_Ga=T,f=32,s=1,v=1,i=2,z=2;{b64}\x1b\\");
        term.feed(below.as_bytes());
        term.feed(above.as_bytes());
        let (b, a) = partition_placements_by_z(term.image_placements());
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].image_id, 1, "z=-1 placement below text");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].image_id, 2, "z=2 placement above text");
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
        let p = default_palette_256();
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
        let p = default_palette_256();
        assert_eq!(ansi_256_color(16, &p), Color::new(0, 0, 0));
        assert_eq!(ansi_256_color(196, &p), Color::new(255, 0, 0));
        assert_eq!(ansi_256_color(21, &p), Color::new(0, 0, 255));
    }

    #[test]
    fn test_ansi_256_standard() {
        let p = default_palette_256();
        for idx in 0..8u16 {
            assert_eq!(ansi_256_color(idx, &p), ANSI_COLORS[idx as usize]);
        }
    }

    #[test]
    fn test_ansi_256_bright() {
        let p = default_palette_256();
        for idx in 8..16u16 {
            assert_eq!(ansi_256_color(idx, &p), ANSI_BRIGHT_COLORS[(idx - 8) as usize]);
        }
    }

    #[test]
    fn test_ansi_256_out_of_range() {
        let p = default_palette_256();
        assert_eq!(ansi_256_color(256, &p), Color::WHITE);
        assert_eq!(ansi_256_color(999, &p), Color::WHITE);
    }

    /// The first 16 entries of the 256 palette mirror the 16-color
    /// default palette; the cube + grayscale follow the xterm formulas
    /// (spot-checked above).
    #[test]
    fn test_default_palette_256_base_matches_16() {
        let p256 = default_palette_256();
        let p16 = default_ansi_palette();
        assert_eq!(&p256[..16], &p16[..]);
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
        let styles = StyleTable::new();
        let links = LinkTable::new();
        assert_eq!(cell.ch, ' ');
        assert!(cell.extra.is_none());
        assert_eq!(cell.width, 1);
        assert_eq!(cell.style_id, DEFAULT_STYLE_ID);
        assert_eq!(cell.link_id, NO_LINK_ID);
        assert_eq!(cell.fg(&styles), Color::WHITE);
        assert_eq!(cell.bg(&styles), Color::BLACK);
        assert_eq!(cell.attrs(&styles), Attrs::NONE);
        assert!(cell.hyperlink(&links).is_none());
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
        assert_eq!(
            response.as_slice(),
            crate::caps::TerminalCaps::PRIMARY_DA
        );
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

    /// critic-1 (review 2026-06-12): an oversized sixel DCS payload
    /// must NOT grow `sixel_buffer` without bound. `put()` poisons the
    /// sequence past SIXEL_DCS_MAX (8 MiB); the buffer is dropped and
    /// `unhook` rejects with no image placed. The buffer never holds
    /// more than the cap + 1.
    #[test]
    fn sixel_dcs_oversized_payload_is_bounded_and_rejected() {
        let mut term = Terminal::new(80, 24);
        // Open a sixel DCS, then stream > 8 MiB of payload bytes
        // without ever sending ST. `feed` chunks them through `put`.
        term.feed(b"\x1bPq");
        let chunk = vec![b'?'; 1024 * 1024]; // 1 MiB of harmless sixel band data
        for _ in 0..9 {
            term.feed(&chunk);
        }
        // The buffer was dropped on overflow — it never grew unbounded.
        assert!(
            term.sixel_buffer.is_none(),
            "oversized sixel payload must drop the buffer, not retain it"
        );
        assert!(term.sixel_buffer_overflow, "overflow flag must be set");
        // Terminating the sequence places no image (poisoned reject).
        let before = term.image_placements().len();
        term.feed(b"\x1b\\");
        assert_eq!(
            term.image_placements().len(),
            before,
            "a rejected oversized sixel must not place an image"
        );
        assert!(!term.sixel_buffer_overflow, "overflow flag clears at unhook");
    }

    #[test]
    fn test_sixel_buffer_none_initially() {
        let term = Terminal::new(80, 24);
        assert!(term.sixel_buffer.is_none());
    }

    /// M3-C3 slice 2: a known small sixel payload decodes to its expected
    /// pixel dimensions and lands exactly one placement, fed through the
    /// SAME path Kitty images use (`store_rgba_image` → `images` +
    /// `image_placements`). The payload is a 2-wide, full-6-row red column
    /// (`#0;2;100;0;0` defines color 0 = red, `#0` selects it, `~~` paints
    /// two full-height columns → 2×6 px).
    #[test]
    fn sixel_payload_decodes_to_expected_dims_and_one_placement() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1bP0;0;0q#0;2;100;0;0#0~~\x1b\\");

        assert_eq!(term.images().len(), 1, "exactly one decoded image stored");
        assert_eq!(
            term.image_placements().len(),
            1,
            "exactly one placement landed"
        );
        let img = term.images().values().next().unwrap();
        assert_eq!((img.width, img.height), (2, 6), "decoded to 2×6 px");
        // RGBA: 2*6 pixels * 4 bytes.
        assert_eq!(img.data.len(), 2 * 6 * 4);
        // Placement references the stored image and sits at the cursor.
        let p = &term.image_placements()[0];
        assert_eq!(p.image_id, img.id);
        assert_eq!((p.col, p.row), (0, 0), "placed at the cursor origin");
        assert_eq!(p.z_index, 0, "sixel placements default to the z=0 band");
    }

    /// critic-2 (review 2026-06-12): a sixel scrolled off the visible
    /// area must FREE its decoded RGBA, not orphan it in `images`
    /// forever. The placement is pruned on the rewrap path; the GC then
    /// drops the now-unreferenced texture. A sixel's auto-assigned id
    /// has no deletable handle, so without this GC the leak is
    /// permanent.
    #[test]
    fn scrolled_off_sixel_texture_is_gc_d_when_placement_pruned() {
        // Small geometry: 40 cols × 6 rows. Fill rows 0..5 with
        // full-width lines, then place a sixel on the last visible row.
        let mut term = Terminal::with_scrollback(40, 6, 200);
        let full = "x".repeat(40);
        for _ in 0..5 {
            term.feed(full.as_bytes());
            term.feed(b"\r\n");
        }
        // Cursor is now on the last visible row (5). Place the sixel.
        term.feed(b"\x1bP0;0;0q#0;2;100;0;0#0~~\x1b\\");
        assert_eq!(term.images().len(), 1, "decoded texture stored");
        assert_eq!(term.image_placements().len(), 1, "placement landed");
        let img_id = term.image_placements()[0].image_id;
        // Halve columns → every full-width line above DOUBLES its row
        // count via soft-wrap, pushing the sixel's logical line far
        // below the 6-row visible window. The rewrap reanchor prunes
        // the now-off-screen placement, and the GC must free its
        // orphaned texture (an auto-id sixel has no deletable handle).
        term.resize(20, 6);
        assert!(
            term.image_placements().iter().all(|p| p.image_id != img_id),
            "the off-screen placement must be pruned by the rewrap"
        );
        assert!(
            !term.images().contains_key(&img_id),
            "the orphaned sixel texture must be freed, not leaked in `images`"
        );
    }

    /// critic-2 GC scope guard: a kitty image TRANSMITTED but never
    /// PLACED (`a=t`, no display) is in no placement, so the GC must
    /// NOT evict it across a rewrap — the transmit→later-place gap is
    /// preserved. (Without scoping the GC to dropped placements, this
    /// image would be wrongly freed.)
    #[test]
    fn transmitted_unplaced_image_survives_rewrap_gc() {
        let mut term = Terminal::with_scrollback(40, 6, 200);
        // Transmit-only: a=t (NOT T), so the image is stored but no
        // placement is created.
        let rgba = [0u8, 0, 255, 255]; // 1×1 blue pixel
        let b64 = base64_encode(&rgba);
        let apc = format!("\x1b_Ga=t,f=32,s=1,v=1,i=42;{b64}\x1b\\");
        term.feed(apc.as_bytes());
        assert!(term.images().contains_key(&42), "transmit stores the image");
        assert!(
            term.image_placements().is_empty(),
            "transmit-only creates no placement"
        );
        // A column rewrap runs the GC — the unplaced image must survive
        // (it was never in `dropped_image_ids`).
        term.resize(20, 6);
        assert!(
            term.images().contains_key(&42),
            "a transmitted-but-unplaced image must survive the rewrap GC"
        );
    }

    /// A malformed sixel payload is rejected with a typed trace — NEVER a
    /// panic. The decode path returns early on `Err`, so no image and no
    /// placement appear; the engine keeps running. (This pins the
    /// UNREPRESENTABILITY-adjacent contract: the only failure surface is a
    /// typed `Result::Err`, not a process abort.)
    #[test]
    fn sixel_malformed_payload_is_rejected_without_panic_or_placement() {
        let mut term = Terminal::new(80, 24);
        // An over-long RLE repeat count (`!999…`) is structurally malformed
        // sixel — icy_sixel returns a typed `Err`, the decode path logs and
        // bails. (Arbitrary high bytes are tolerated by the decoder as
        // empty pixels, so the rejection case must be a structural fault.)
        term.feed(b"\x1bP0;0;0q!999999999999999~\x1b\\");
        // The raw-audit record may capture the bytes, but NOTHING gets
        // decoded into the shared upload path.
        assert!(
            term.images().is_empty(),
            "malformed payload decodes to no image"
        );
        assert!(
            term.image_placements().is_empty(),
            "malformed payload lands no placement"
        );
        // The terminal is still alive and processing — feed normal text.
        term.feed(b"X");
        assert_eq!(term.cell(0, 0).ch, 'X');
    }

    /// A sixel placement rides the same z-band split as Kitty images: with
    /// default z=0 it lands in the ABOVE-text band.
    #[test]
    fn decoded_sixel_lands_in_above_text_band() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1bP0;0;0q#0;2;100;0;0#0~~\x1b\\");
        let (below, above) = partition_placements_by_z(term.image_placements());
        assert!(below.is_empty());
        assert_eq!(above.len(), 1, "z=0 sixel placement is above text");
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
        let notifs: Vec<PendingNotification> = term.drain_notifications().collect();
        assert_eq!(
            notifs,
            vec![PendingNotification {
                title: None,
                body: "Build finished".into(),
                urgency: Urgency::Normal,
                group: None,
            }]
        );
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
        let bodies: Vec<String> = term.drain_notifications().map(|n| n.body).collect();
        assert_eq!(bodies, vec!["one".to_string(), "two".into(), "three".into()]);
    }

    /// Matrix over every notification-bearing OSC dialect: each
    /// variant enqueues EXACTLY ONE typed entry with the dialect's
    /// title/urgency/group shape. Failures aggregate so one run
    /// reports every broken dialect.
    #[test]
    fn notification_osc_matrix_enqueues_one_typed_entry_each() {
        struct Row {
            name: &'static str,
            bytes: &'static [u8],
            expect: PendingNotification,
        }
        let matrix = [
            Row {
                name: "OSC 9 body-only",
                bytes: b"\x1b]9;tests passed\x07",
                expect: PendingNotification {
                    title: None,
                    body: "tests passed".into(),
                    urgency: Urgency::Normal,
                    group: None,
                },
            },
            Row {
                name: "OSC 9 body containing semicolons",
                bytes: b"\x1b]9;a;b;c\x07",
                expect: PendingNotification {
                    title: None,
                    body: "a;b;c".into(),
                    urgency: Urgency::Normal,
                    group: None,
                },
            },
            Row {
                name: "OSC 777;notify title+body",
                bytes: b"\x1b]777;notify;Build;finished ok\x07",
                expect: PendingNotification {
                    title: Some("Build".into()),
                    body: "finished ok".into(),
                    urgency: Urgency::Normal,
                    group: None,
                },
            },
            Row {
                name: "OSC 99 default payload kind is title",
                bytes: b"\x1b]99;;Hello\x07",
                expect: PendingNotification {
                    title: Some("Hello".into()),
                    body: String::new(),
                    urgency: Urgency::Normal,
                    group: None,
                },
            },
            Row {
                name: "OSC 99 low urgency + id group",
                bytes: b"\x1b]99;i=ci:u=0:p=body;done\x07",
                expect: PendingNotification {
                    title: None,
                    body: "done".into(),
                    urgency: Urgency::Low,
                    group: Some("ci".into()),
                },
            },
            Row {
                name: "OSC 99 critical urgency",
                bytes: b"\x1b]99;u=2:p=title;Disk full\x07",
                expect: PendingNotification {
                    title: Some("Disk full".into()),
                    body: String::new(),
                    urgency: Urgency::Critical,
                    group: None,
                },
            },
        ];
        let mut failures: Vec<String> = Vec::new();
        for row in &matrix {
            let mut term = Terminal::new(80, 24);
            term.feed(row.bytes);
            let got: Vec<PendingNotification> = term.drain_notifications().collect();
            if got.len() != 1 {
                failures.push(format!(
                    "{}: expected exactly 1 notification, got {}",
                    row.name,
                    got.len()
                ));
                continue;
            }
            if got[0] != row.expect {
                failures.push(format!(
                    "{}: expected {:?}, got {:?}",
                    row.name,
                    row.expect,
                    got[0]
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} notification dialects failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn osc_99_multipart_chain_accumulates_then_finalizes() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]99;i=1:d=0:p=title;Hel\x07");
        term.feed(b"\x1b]99;i=1:d=0:p=title;lo\x07");
        // Nothing enqueued while the chain is open.
        assert_eq!(term.drain_notifications().count(), 0);
        term.feed(b"\x1b]99;i=1:d=1:p=body;World\x07");
        let got: Vec<PendingNotification> = term.drain_notifications().collect();
        assert_eq!(
            got,
            vec![PendingNotification {
                title: Some("Hello".into()),
                body: "World".into(),
                urgency: Urgency::Normal,
                group: Some("1".into()),
            }]
        );
    }

    #[test]
    fn osc_99_unimplemented_payload_kind_enqueues_nothing() {
        // p=close manipulates displayed notifications — unimplemented,
        // trace-ignored, and must NOT surface as a garbage entry.
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]99;i=1:p=close;\x07");
        assert_eq!(term.drain_notifications().count(), 0);
    }

    /// critic-0 (review 2026-06-12): an OSC 9 flood must NOT grow the
    /// notification queue without bound. Each drained entry spawns an
    /// OS notification process on dispatch, so an unbounded queue is a
    /// fork-bomb-adjacent DoS. The `push_notification` chokepoint caps
    /// the queue (drop-oldest, keep newest) regardless of input rate.
    #[test]
    fn notification_flood_is_bounded_at_the_queue() {
        const CAP: usize = 64;
        let mut term = Terminal::new(80, 24);
        // Stream far more notifications than the cap WITHOUT draining.
        for i in 0..(CAP * 10) {
            // OSC 9 body carries the index so we can prove drop-oldest.
            let esc = format!("\x1b]9;n{i}\x07");
            term.feed(esc.as_bytes());
        }
        let got: Vec<PendingNotification> = term.drain_notifications().collect();
        assert_eq!(got.len(), CAP, "queue must be bounded at the cap, not grow with input");
        // Drop-oldest keeps the NEWEST entries: the last body must be
        // the final index fed, the first the (total - cap)-th.
        let total = CAP * 10;
        assert_eq!(got.last().unwrap().body, format!("n{}", total - 1));
        assert_eq!(got.first().unwrap().body, format!("n{}", total - CAP));
    }

    /// critic-3 (review 2026-06-12): an OSC 99 `d=0` chain that never
    /// finalizes must NOT grow `pending.title`/`body` without bound.
    /// Each field caps at MAX_OSC99_FIELD; further fragments trace-drop
    /// but the chain still finalizes on a later `d=1` with what fit.
    #[test]
    fn osc_99_unbounded_chain_field_is_capped() {
        const FIELD_CAP: usize = 16 * 1024;
        let mut term = Terminal::new(80, 24);
        // Each fragment adds 1 KiB to the body; stream well past the cap.
        let chunk = "x".repeat(1024);
        for _ in 0..64 {
            let esc = format!("\x1b]99;i=1:d=0:p=body;{chunk}\x07");
            term.feed(esc.as_bytes());
        }
        // Finalize — the body must be capped, not 64 KiB.
        term.feed(b"\x1b]99;i=1:d=1:p=body;\x07");
        let got: Vec<PendingNotification> = term.drain_notifications().collect();
        assert_eq!(got.len(), 1, "the chain finalizes exactly once");
        assert!(
            got[0].body.len() <= FIELD_CAP + 1024,
            "chain body must be bounded near the cap, got {} bytes",
            got[0].body.len()
        );
    }

    /// `ConEmu` OSC 9;4 matrix: every progress state lands in the typed
    /// progress lane and NEVER enqueues a notification (separate
    /// field by construction — this pins the parse routing).
    #[test]
    fn conemu_progress_matrix_sets_lane_and_never_notifies() {
        // `expect` is the typed progress lane outcome — `None` means
        // the parse trace-drops with no progress AND no notification
        // (the truncated/empty-state rows). EVERY row asserts zero
        // notifications: the `9;4` namespace can never leak a banner.
        let matrix: [(&str, &[u8], Option<ProgressState>); 7] = [
            ("remove", b"\x1b]9;4;0\x07", Some(ProgressState::Remove)),
            ("set 50%", b"\x1b]9;4;1;50\x07", Some(ProgressState::Set { pct: 50 })),
            ("error with pct", b"\x1b]9;4;2;30\x07", Some(ProgressState::Error { pct: Some(30) })),
            ("indeterminate", b"\x1b]9;4;3\x07", Some(ProgressState::Indeterminate)),
            ("paused bare", b"\x1b]9;4;4\x07", Some(ProgressState::Paused { pct: None })),
            // Truncated `ESC]9;4 ST` — no state byte. Routes to the
            // progress handler's `other` arm: trace-drop, NOT a "4"
            // notification (review 2026-06-12, determinism-unrep-0).
            ("bare 9;4 truncated", b"\x1b]9;4\x07", None),
            // `ESC]9;4; ST` — empty state param. Same trace-drop arm.
            ("empty state 9;4;", b"\x1b]9;4;\x07", None),
        ];
        let mut failures: Vec<String> = Vec::new();
        for (name, bytes, expect) in &matrix {
            let mut term = Terminal::new(80, 24);
            term.feed(bytes);
            let notifs = term.drain_notifications().count();
            if notifs != 0 {
                failures.push(format!("{name}: progress leaked {notifs} notification(s)"));
            }
            if term.take_progress() != *expect {
                failures.push(format!("{name}: expected progress {expect:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} progress states failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn conemu_progress_is_latest_wins_and_take_clears() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]9;4;1;10\x07");
        term.feed(b"\x1b]9;4;1;90\x07");
        assert_eq!(term.take_progress(), Some(ProgressState::Set { pct: 90 }));
        // Second take: no update since the last one.
        assert_eq!(term.take_progress(), None);
        // Out-of-range percent clamps at the parse boundary.
        term.feed(b"\x1b]9;4;1;250\x07");
        assert_eq!(term.take_progress(), Some(ProgressState::Set { pct: 100 }));
    }

    /// M4 drain determinism: the drain is pure state transfer — two
    /// terminals in the same pre-state drain the SAME typed value,
    /// and an immediately repeated drain yields the default payload.
    #[test]
    fn drain_is_pure_state_transfer_and_second_drain_is_empty() {
        let feed_all = |term: &mut Terminal| {
            term.feed(b"\x1b]0;build shell\x07");
            term.feed(b"\x07");
            term.feed(b"\x1b]52;c;aGVsbG8=\x07"); // OSC 52 "hello"
            term.feed(b"\x1b]9;done\x07");
            term.feed(b"\x1b]9;4;1;50\x07");
            term.feed(b"\x1b]7;file://host/tmp\x07");
            term.feed(b"\x1b]1337;RequestAttention=1\x07");
        };
        let mut a = Terminal::new(80, 24);
        let mut b = Terminal::new(80, 24);
        feed_all(&mut a);
        feed_all(&mut b);
        let drained_a = a.drain_side_effects();
        let drained_b = b.drain_side_effects();
        assert_eq!(drained_a, drained_b, "same pre-state must drain the same value");
        assert_eq!(drained_a.title.as_deref(), Some("build shell"));
        assert!(drained_a.bell);
        assert_eq!(drained_a.clipboard.as_deref(), Some("hello"));
        assert_eq!(drained_a.notifications.len(), 1);
        assert_eq!(drained_a.progress, Some(ProgressState::Set { pct: 50 }));
        assert_eq!(drained_a.cwd.as_deref(), Some("/tmp"));
        assert!(drained_a.attention);
        // Second immediate drain: nothing accumulated since.
        assert_eq!(a.drain_side_effects(), crate::ux::TerminalSideEffects::default());
        // The attention LEVEL survives the drain (MCP attention_get
        // reads it); only the edge was consumed.
        assert!(a.attention_requested());
    }

    /// Title/cwd are change-edges: an un-changed title drains as None
    /// on the next frame; a re-set title drains again.
    #[test]
    fn drain_title_is_a_change_edge_not_a_level() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]0;one\x07");
        assert_eq!(term.drain_side_effects().title.as_deref(), Some("one"));
        assert_eq!(term.drain_side_effects().title, None);
        term.feed(b"\x1b]0;two\x07");
        assert_eq!(term.drain_side_effects().title.as_deref(), Some("two"));
    }

    #[test]
    fn osc_133_d_exit_status_round_trip_with_deterministic_clock() {
        fn fixed_clock() -> u128 {
            42_000
        }
        let mut term = Terminal::new(80, 24);
        term.set_clock(fixed_clock);
        term.feed(b"\x1b]133;A\x07");
        term.feed(b"echo hi\r\n");
        term.feed(b"\x1b]133;C\x07");
        term.feed(b"hi\r\n");
        term.feed(b"\x1b]133;D;1\x07");
        let marks: Vec<crate::prompt_mark::PromptMark> =
            term.prompt_marks().iter().copied().collect();
        let by_kind = |k: crate::prompt_mark::PromptKind| {
            marks.iter().find(|m| m.kind == k).copied().unwrap()
        };
        let c = by_kind(crate::prompt_mark::PromptKind::CommandOutput);
        let d = by_kind(crate::prompt_mark::PromptKind::CommandEnd);
        assert_eq!(c.exit_status, Some(1), "C mark back-filled with exit status");
        assert_eq!(d.exit_status, Some(1), "D mark stamped with exit status");
        for m in &marks {
            assert_eq!(m.at_unix_ms, 42_000, "{:?} stamped via the clock seam", m.kind);
        }
        // The zone read composes the two: span + status in one value.
        let zone = term
            .prompt_marks()
            .last_command_zone(usize::MAX)
            .expect("zone exists");
        assert_eq!(zone.start, c.grid_row);
        assert_eq!(zone.exit_status, Some(1));
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
    fn test_osc_4_set_extended_index() {
        // M2 — the full 0..256 range is mutable; OSC 4 on a cube
        // index overrides just that entry.
        let mut term = Terminal::new(80, 24);
        let before = term.ansi_palette()[0];
        term.feed(b"\x1b]4;200;#112233\x1b\\");
        assert_eq!(term.ansi_palette()[200], Color::new(0x11, 0x22, 0x33));
        // Neighbouring entries untouched.
        assert_eq!(term.ansi_palette()[0], before);
        assert_eq!(term.ansi_palette()[201], default_palette_256()[201]);
    }

    #[test]
    fn test_osc_4_set_ignored_for_out_of_range_index() {
        // Indices ≥ 256 are out of range; OSC 4 set on those is a
        // silent no-op (not a panic, not a partial overwrite).
        let mut term = Terminal::new(80, 24);
        let before = *term.ansi_palette();
        term.feed(b"\x1b]4;300;#112233\x1b\\");
        assert_eq!(term.ansi_palette(), &before);
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
        // M2 — extended entries reset too.
        term.feed(b"\x1b]4;200;#445566\x1b\\");

        term.feed(b"\x1b]104\x07");
        let restored = term.ansi_palette();
        let defaults = default_palette_256();
        assert_eq!(restored, &defaults);
    }

    // ── M2 — wide interned Attrs / Cell shrink / SGR wire / palette ──

    /// Matrix: every SGR attribute flag round-trips through the
    /// StyleTable intern → cell.style_id → lookup path (post-shrink,
    /// the table IS the only storage, so this is the load-bearing
    /// round trip). Aggregated failures — one run reports every
    /// broken arm.
    #[test]
    fn every_sgr_attr_round_trips_through_intern_lookup() {
        fn with_flag(flag: AttrFlags) -> Attrs {
            let mut a = Attrs::NONE;
            a.flags.insert(flag);
            a
        }
        fn with_underline(style: UnderlineStyle) -> Attrs {
            Attrs { underline: style, ..Attrs::NONE }
        }
        let cases: &[(&[u8], Attrs, &str)] = &[
            (b"\x1b[1m", with_flag(AttrFlags::BOLD), "1 bold"),
            (b"\x1b[2m", with_flag(AttrFlags::DIM), "2 dim"),
            (b"\x1b[3m", with_flag(AttrFlags::ITALIC), "3 italic"),
            (b"\x1b[4m", with_underline(UnderlineStyle::Single), "4 underline"),
            (b"\x1b[5m", with_flag(AttrFlags::BLINK), "5 blink"),
            (b"\x1b[7m", with_flag(AttrFlags::INVERSE), "7 inverse"),
            (b"\x1b[8m", with_flag(AttrFlags::HIDDEN), "8 hidden"),
            (b"\x1b[9m", with_flag(AttrFlags::STRIKETHROUGH), "9 strike"),
            (b"\x1b[53m", with_flag(AttrFlags::OVERLINE), "53 overline"),
            (
                b"\x1b[58:5:9m",
                Attrs { underline_color: UnderlineColor::Indexed(9), ..Attrs::NONE },
                "58:5:9 underline color",
            ),
        ];
        let mut failures = Vec::new();
        for (esc, want, name) in cases {
            let mut term = Terminal::new(20, 4);
            term.feed(esc);
            term.feed(b"X");
            let got = term.cell(0, 0).attrs(term.styles());
            if got != *want {
                failures.push(format!("{name}: got {got:?}, want {want:?}"));
            }
            // Round trip the same Attrs through a fresh table directly.
            let mut table = StyleTable::new();
            let style = Style { fg: Color::WHITE, bg: Color::BLACK, attrs: *want };
            let id = table.intern(style);
            if table.lookup(id) != style {
                failures.push(format!("{name}: intern/lookup mangled {style:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} SGR arms failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// `Attrs::to_legacy_bits` matches the frozen `CellAttrs` u8 bit
    /// layout for every overlapping attribute (the MCP CellSnapshot
    /// back-compat surface).
    #[test]
    fn to_legacy_bits_matches_cellattrs_layout() {
        let flag_cases: &[(AttrFlags, CellAttrs, &str)] = &[
            (AttrFlags::BOLD, CellAttrs::BOLD, "bold"),
            (AttrFlags::ITALIC, CellAttrs::ITALIC, "italic"),
            (AttrFlags::BLINK, CellAttrs::BLINK, "blink"),
            (AttrFlags::INVERSE, CellAttrs::INVERSE, "inverse"),
            (AttrFlags::STRIKETHROUGH, CellAttrs::STRIKETHROUGH, "strike"),
            (AttrFlags::DIM, CellAttrs::DIM, "dim"),
            (AttrFlags::HIDDEN, CellAttrs::HIDDEN, "hidden"),
        ];
        let mut failures = Vec::new();
        for (flag, legacy, name) in flag_cases {
            let mut a = Attrs::NONE;
            a.flags.insert(*flag);
            if a.to_legacy_bits() != legacy.bits() {
                failures.push(format!(
                    "{name}: got {:08b}, want {:08b}",
                    a.to_legacy_bits(),
                    legacy.bits()
                ));
            }
        }
        // Every non-None underline style sets the single legacy
        // UNDERLINE bit; None sets nothing.
        for style in [
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ] {
            let a = Attrs { underline: style, ..Attrs::NONE };
            if a.to_legacy_bits() != CellAttrs::UNDERLINE.bits() {
                failures.push(format!("underline {style}: missing legacy bit"));
            }
        }
        if Attrs::NONE.to_legacy_bits() != 0 {
            failures.push("Attrs::NONE: nonzero legacy bits".to_string());
        }
        // OVERLINE and underline_color have no u8 representation.
        let mut a = Attrs::NONE;
        a.flags.insert(AttrFlags::OVERLINE);
        a.underline_color = UnderlineColor::Indexed(3);
        if a.to_legacy_bits() != 0 {
            failures.push("overline/underline_color leaked into legacy bits".to_string());
        }
        assert!(
            failures.is_empty(),
            "{} legacy-bit mappings failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// SGR 4:N sub-param wire — all six styles + plain 4 + 24/4:0
    /// resets (M2 acceptance matrix).
    #[test]
    fn sgr_underline_style_subparams() {
        let cases: &[(&[u8], UnderlineStyle)] = &[
            (b"\x1b[4m", UnderlineStyle::Single),
            (b"\x1b[4:0m", UnderlineStyle::None),
            (b"\x1b[4:1m", UnderlineStyle::Single),
            (b"\x1b[4:2m", UnderlineStyle::Double),
            (b"\x1b[4:3m", UnderlineStyle::Curly),
            (b"\x1b[4:4m", UnderlineStyle::Dotted),
            (b"\x1b[4:5m", UnderlineStyle::Dashed),
        ];
        let mut failures = Vec::new();
        for (esc, want) in cases {
            let mut term = Terminal::new(20, 4);
            term.feed(esc);
            term.feed(b"A");
            let got = term.cell(0, 0).attrs(term.styles()).underline;
            if got != *want {
                failures.push(format!(
                    "{:?}: got {got}, want {want}",
                    String::from_utf8_lossy(esc)
                ));
            }
        }
        // SGR 24 resets any active underline style.
        let mut term = Terminal::new(20, 4);
        term.feed(b"\x1b[4:3mA\x1b[24mB");
        let a = term.cell(0, 0).attrs(term.styles()).underline;
        let b = term.cell(0, 1).attrs(term.styles()).underline;
        if a != UnderlineStyle::Curly {
            failures.push(format!("pre-24 cell: got {a}, want curly"));
        }
        if b != UnderlineStyle::None {
            failures.push(format!("post-24 cell: got {b}, want none"));
        }
        assert!(
            failures.is_empty(),
            "{} underline-style arms failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    fn decrqss_m_reply(setup: &[u8]) -> String {
        let mut term = Terminal::new(20, 4);
        term.feed(setup);
        term.feed(b"\x1bP$qm\x1b\\");
        String::from_utf8_lossy(&term.take_response().unwrap_or_default()).into_owned()
    }

    /// DECRQSS `m` is PEN-DERIVED (M3): a default pen reports `0m`,
    /// and every pen axis (flags, 4:N underline, 58 colour, 53
    /// overline, non-default fg) echoes back. The curly row is the
    /// kitty/neovim undercurl support probe — the in-band proof
    /// behind caps::STYLED_UNDERLINE_IMPLEMENTED. Matrix-style:
    /// failures aggregate, one assert.
    #[test]
    fn decrqss_sgr_report_is_pen_derived() {
        struct Row {
            setup: &'static [u8],
            expect: &'static [&'static str],
            name: &'static str,
        }
        let rows: &[Row] = &[
            Row { setup: b"", expect: &["\x1bP1$r0m\x1b\\"], name: "default pen" },
            Row { setup: b"\x1b[1;3m", expect: &[";1", ";3"], name: "bold+italic" },
            Row { setup: b"\x1b[4:3m", expect: &["4:3"], name: "undercurl probe" },
            Row { setup: b"\x1b[4:5m", expect: &["4:5"], name: "dashed" },
            Row { setup: b"\x1b[4m", expect: &[";4m", ";4"], name: "single underline" },
            Row {
                setup: b"\x1b[58:2::240:100:30m\x1b[4m",
                expect: &["58:2::240:100:30"],
                name: "underline colour rgb",
            },
            Row { setup: b"\x1b[58:5:9m", expect: &["58:5:9"], name: "underline colour indexed" },
            Row { setup: b"\x1b[53m", expect: &[";53"], name: "overline" },
            // Semicolon form — the pen resolves it to RGB; the report
            // re-emits the colon sub-param shape (the pen's truth).
            Row { setup: b"\x1b[38;2;10;20;30m", expect: &["38:2::10:20:30"], name: "rgb fg" },
            Row {
                setup: b"\x1b[4:3m\x1b[0m",
                expect: &["\x1bP1$r0m\x1b\\"],
                name: "reset returns to 0m",
            },
        ];
        let mut failures = Vec::new();
        for row in rows {
            let reply = decrqss_m_reply(row.setup);
            if !reply.starts_with("\x1bP1$r0") || !reply.ends_with("m\x1b\\") {
                failures.push(format!(
                    "{}: reply {:?} not framed as DCS 1 $ r 0…m ST",
                    row.name, reply
                ));
                continue;
            }
            for needle in row.expect {
                if !reply.contains(needle) {
                    failures.push(format!(
                        "{}: reply {:?} missing {:?}",
                        row.name, reply, needle
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} DECRQSS rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// MECHANICAL registry round-trip (M3 review 2026-06-12): every
    /// `(flag, code)` row in `AttrFlags::ALL` drives one SGR set +
    /// DECRQSS `m` reply. The reply path iterates the SAME registry,
    /// so a new flag added to ALL gets reporting + coverage in one
    /// change, and a flag whose SGR parse or report is broken fails
    /// here per-row — the former hand-duplicated `FLAG_PARAMS` local
    /// would have silently omitted a ninth flag from DECRQSS.
    #[test]
    fn every_attr_flag_in_the_registry_round_trips_through_decrqss() {
        let mut failures = Vec::new();
        for (flag, code) in AttrFlags::ALL {
            let setup = format!("\x1b[{code}m");
            let reply = decrqss_m_reply(setup.as_bytes());
            let needle = format!(";{code}");
            if !reply.contains(&needle) {
                failures.push(format!(
                    "flag {flag:?} (SGR {code}): reply {reply:?} missing {needle:?}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} registry flags failed the DECRQSS round-trip:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// SGR 58/59 underline-colour wire — colon + semicolon forms,
    /// reset, malformed degradation (M2 acceptance matrix).
    #[test]
    fn sgr_underline_color_wire() {
        let cases: &[(&[u8], UnderlineColor, &str)] = &[
            (
                b"\x1b[58:2::255:0:0m",
                UnderlineColor::Rgb(Rgb::new(255, 0, 0)),
                "58:2::r:g:b colon+colorspace",
            ),
            (
                b"\x1b[58:2:10:20:30m",
                UnderlineColor::Rgb(Rgb::new(10, 20, 30)),
                "58:2:r:g:b colon",
            ),
            (b"\x1b[58:5:9m", UnderlineColor::Indexed(9), "58:5:N colon"),
            (b"\x1b[58;5;9m", UnderlineColor::Indexed(9), "58;5;N semicolon"),
            (
                b"\x1b[58;2;10;20;30m",
                UnderlineColor::Rgb(Rgb::new(10, 20, 30)),
                "58;2;r;g;b semicolon",
            ),
            // Malformed: unknown colour mode degrades to Default.
            (b"\x1b[58:9:9m", UnderlineColor::Default, "58:9:9 malformed mode"),
            // Malformed: truncated RGB degrades to Default.
            (b"\x1b[58:2:10m", UnderlineColor::Default, "58:2:r truncated"),
        ];
        let mut failures = Vec::new();
        for (esc, want, name) in cases {
            let mut term = Terminal::new(20, 4);
            term.feed(esc);
            term.feed(b"A");
            let style = term.cell(0, 0).style(term.styles());
            if style.attrs.underline_color != *want {
                failures.push(format!(
                    "{name}: got {:?}, want {want:?}",
                    style.attrs.underline_color
                ));
            }
            // The underline colour is its own axis — fg must be
            // untouched (the spec's "distinct from fg" acceptance).
            if style.fg != Color::WHITE {
                failures.push(format!("{name}: fg corrupted to {:?}", style.fg));
            }
        }
        // SGR 59 resets a previously set underline colour.
        let mut term = Terminal::new(20, 4);
        term.feed(b"\x1b[58:5:9mA\x1b[59mB");
        let b = term.cell(0, 1).attrs(term.styles()).underline_color;
        if b != UnderlineColor::Default {
            failures.push(format!("post-59 cell: got {b:?}, want Default"));
        }
        assert!(
            failures.is_empty(),
            "{} underline-colour arms failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// M2 memory guard: the shrunk Cell is strictly smaller than the
    /// pre-refactor layout. Pre-M2: ch(4) + Option<Box<Vec<char>>>(8)
    /// + width(1) + fg(3) + bg(3) + CellAttrs(1) + style_id(2) +
    /// Option<Arc<str>>(16) = 38 → 40 with padding. Post-shrink the
    /// budget is 24 (ptr + char + 2×u16 + u8, padded).
    #[test]
    fn cell_size_shrunk_below_pre_m2_layout() {
        assert!(
            std::mem::size_of::<Cell>() <= 24,
            "Cell grew to {} bytes (budget 24, pre-M2 was 40)",
            std::mem::size_of::<Cell>()
        );
    }

    /// M2 memory guard: interning absorbs the wider Attrs — an
    /// `ls --color`-like stream (the captured byte stream from
    /// tests/scenarios/ls-color.scenario.yaml) interns a handful of
    /// styles, not one per cell.
    #[test]
    fn style_table_stays_small_on_ls_color_stream() {
        let mut term = Terminal::new(80, 20);
        term.feed(b"total 4302228\r\n\x1b[0;38;2;76;86;106m\x1b[0;38;2;76;86;106mdrwxrwxrwt 471 root   wheel      15072 May 12 19:36 \x1b[m\x1b[1;38;2;143;188;187m.\x1b[0;38;2;76;86;106m\r\n\x1b[0;38;2;76;86;106mdrwxr-xr-x   6 root   wheel        192 Apr  1  1976 \x1b[m\x1b[1;38;2;143;188;187m..\x1b[0;38;2;76;86;106m\r\n\x1b[0;38;2;76;86;106m-rw-r--r--   1 drzzln wheel         15 May  9 00:12 \x1b[m\x1b[0;38;2;180;142;173magent.html\x1b[0;38;2;76;86;106m\r\n\x1b[0;38;2;76;86;106m-rw-r--r--   1 drzzln wheel       8263 May 12 15:59 \x1b[m\x1b[0;38;2;180;142;173makeyless-repos.json\x1b[0;38;2;76;86;106m\r\n");
        assert!(
            term.styles().len() < 50,
            "ls --color interned {} styles (budget < 50)",
            term.styles().len()
        );
    }

    /// StyleTable::gc rebuilds from the live set with NO aliasing to
    /// the default style: every live id remaps to an id that resolves
    /// to the identical Style.
    #[test]
    fn style_table_gc_remaps_live_ids_without_default_aliasing() {
        fn color_style(i: u32) -> Style {
            Style {
                fg: Color::new((i >> 16) as u8, (i >> 8) as u8, i as u8),
                bg: Color::BLACK,
                attrs: Attrs::NONE,
            }
        }
        let mut table = StyleTable::new();
        let ids: Vec<u16> = (1..=1000u32).map(|i| table.intern(color_style(i))).collect();
        assert_eq!(table.len(), 1001);

        // Keep every 100th id live.
        let live: std::collections::HashSet<u16> =
            ids.iter().step_by(100).copied().collect();
        let before: Vec<(u16, Style)> =
            live.iter().map(|&id| (id, table.lookup(id))).collect();

        let remap = table.gc(&live);

        let mut failures = Vec::new();
        for (old_id, style) in before {
            let Some(&new_id) = remap.get(&old_id) else {
                failures.push(format!("live id {old_id} missing from remap"));
                continue;
            };
            if table.lookup(new_id) != style {
                failures.push(format!(
                    "id {old_id}→{new_id}: style mangled to {:?}",
                    table.lookup(new_id)
                ));
            }
            if new_id == DEFAULT_STYLE_ID {
                failures.push(format!("id {old_id} aliased to DEFAULT_STYLE_ID"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} gc remaps failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
        // Table shrank to live set + default.
        assert_eq!(table.len(), live.len() + 1);
        // Default stays pinned at id 0.
        assert_eq!(
            table.lookup(DEFAULT_STYLE_ID),
            Style { fg: Color::WHITE, bg: Color::BLACK, attrs: Attrs::NONE }
        );
    }

    /// Saturation fallback (overflow-policy step 2): a full table
    /// returns the LAST interned id for a novel style — never
    /// DEFAULT_STYLE_ID.
    #[test]
    fn style_table_saturation_falls_back_to_last_id_not_default() {
        fn color_style(i: u32) -> Style {
            Style {
                fg: Color::new((i >> 16) as u8, (i >> 8) as u8, i as u8),
                bg: Color::BLACK,
                attrs: Attrs::NONE,
            }
        }
        let mut table = StyleTable::new();
        // Fill to capacity: 1 default + 65534 = 65535 = u16::MAX.
        for i in 1..u32::from(u16::MAX) {
            table.try_intern(color_style(i));
        }
        assert!(table.is_full());

        // A novel style cannot be allocated…
        let novel = Style {
            fg: Color::new(1, 2, 3),
            bg: Color::new(4, 5, 6),
            attrs: Attrs::NONE,
        };
        assert_eq!(table.try_intern(novel), None);
        // …and the fallback aliases to the LAST interned id.
        let id = table.intern(novel);
        assert_ne!(id, DEFAULT_STYLE_ID);
        assert_eq!(id as usize, table.len() - 1);
        // Existing styles still intern to their exact ids when full.
        assert_eq!(table.try_intern(color_style(1)), Some(1));
    }

    /// Terminal-level gc: after compaction, every cell still resolves
    /// to its original colours (the remap walk covers both grids).
    #[test]
    fn terminal_gc_preserves_cell_styles() {
        let mut term = Terminal::new(40, 5);
        term.feed(b"\x1b[38;2;10;20;30mAB\x1b[38;2;40;50;60mCD");
        // Inflate the table with styles no cell references.
        for i in 0..500u32 {
            let _ = term.style_table.intern(Style {
                fg: Color::new((i >> 16) as u8, (i >> 8) as u8, i as u8),
                bg: Color::new(9, 9, 9),
                attrs: Attrs::NONE,
            });
        }
        let len_before = term.style_table.len();
        term.gc_style_table();
        assert!(term.style_table.len() < len_before);
        assert_eq!(term.cell(0, 0).fg(term.styles()), Color::new(10, 20, 30));
        assert_eq!(term.cell(0, 1).fg(term.styles()), Color::new(10, 20, 30));
        assert_eq!(term.cell(0, 2).fg(term.styles()), Color::new(40, 50, 60));
        assert_eq!(term.cell(0, 3).fg(term.styles()), Color::new(40, 50, 60));
        // Untouched cells still resolve to the default style.
        assert_eq!(term.cell(1, 0).fg(term.styles()), Color::WHITE);
    }

    /// LinkTable interning: one URI = one id across N cells; a second
    /// URI gets its own id; ending the hyperlink (OSC 8 with empty
    /// URI) leaves subsequent cells unlinked.
    #[test]
    fn link_table_interns_uris_per_session() {
        let mut term = Terminal::new(40, 4);
        term.feed(b"\x1b]8;;https://a.example\x1b\\aa\x1b]8;;\x1b\\");
        term.feed(b"\x1b]8;;https://b.example\x1b\\b\x1b]8;;\x1b\\");
        term.feed(b"\x1b]8;;https://a.example\x1b\\c\x1b]8;;\x1b\\");
        let id_a0 = term.cell(0, 0).link_id;
        let id_a1 = term.cell(0, 1).link_id;
        let id_b = term.cell(0, 2).link_id;
        let id_a2 = term.cell(0, 3).link_id;
        assert_eq!(id_a0, id_a1, "same run shares one id");
        assert_eq!(id_a0, id_a2, "same URI re-opened reuses the id");
        assert_ne!(id_a0, id_b, "distinct URIs get distinct ids");
        assert_eq!(term.links().len(), 2, "two URIs interned once each");
        assert_eq!(
            term.cell(0, 0).hyperlink(term.links()),
            Some("https://a.example")
        );
        assert_eq!(
            term.cell(0, 2).hyperlink(term.links()),
            Some("https://b.example")
        );
    }

    /// OSC 4 query on an extended index returns the value a prior
    /// OSC 4 set wrote (the >=16 early-return is gone — M2
    /// acceptance).
    #[test]
    fn osc_4_query_returns_set_extended_index() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b]4;200;#112233\x1b\\");
        term.feed(b"\x1b]4;200;?\x1b\\");
        let response = term.take_response().expect("OSC 4 query answered");
        assert_eq!(
            std::str::from_utf8(&response).unwrap(),
            "\x1b]4;200;rgb:1111/2222/3333\x1b\\"
        );
    }

    // ── M2 stage 2 — Line/LogicalLineId, wrap stamping, rewrap ─────

    /// Trimmed text of every visible row.
    fn visible_text(term: &Terminal) -> Vec<String> {
        term.visible_rows()
            .map(|r| {
                r.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// 100 distinguishable ASCII chars (digits cycling) — wraps once
    /// at 80 cols, twice at 40.
    fn hundred_chars() -> String {
        (0..100u32)
            .map(|i| char::from_digit(i % 10, 10).unwrap())
            .collect()
    }

    /// Wrap stamping (M2 acceptance): feeding exactly cols + 10
    /// chars soft-wraps once — the wrapped-from row is marked
    /// `wrapped` and SHARES its [`LogicalLineId`] with the
    /// continuation row; a hard `\r\n` afterwards starts a line with
    /// a FRESH id.
    #[test]
    fn soft_wrap_stamps_wrap_flag_and_shares_logical_id() {
        let mut term = Terminal::new(80, 24);
        let text: String = hundred_chars().chars().take(90).collect();
        term.feed(text.as_bytes());

        let r0 = &term.primary.rows[0];
        let r1 = &term.primary.rows[1];
        assert!(r0.wrapped, "wrapped-from row carries the wrap marker");
        assert_eq!(
            r0.logical_id, r1.logical_id,
            "continuation row joins the same logical line"
        );
        assert!(!r1.wrapped, "the line's final row does not continue");

        // Hard newline → fresh logical id on the next line.
        term.feed(b"\r\nnext");
        let r1 = &term.primary.rows[1];
        let r2 = &term.primary.rows[2];
        assert!(!r1.wrapped, "\\r\\n is a hard break, not a soft wrap");
        assert_ne!(
            r1.logical_id, r2.logical_id,
            "a hard newline starts a logical line with a fresh id"
        );
    }

    /// Blank rows scrolled in at the bottom are their own logical
    /// lines — fresh id each, never a continuation.
    #[test]
    fn scrolled_in_blank_rows_get_fresh_logical_ids() {
        let mut term = Terminal::new(10, 3);
        term.feed(b"a\r\nb\r\nc\r\nd"); // last \r\n scrolls once
        let ids: Vec<LogicalLineId> =
            term.primary.rows.iter().map(|l| l.logical_id).collect();
        assert_eq!(term.primary.rows.len(), 4, "one row entered scrollback");
        for (i, a) in ids.iter().enumerate() {
            for (j, b) in ids.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "rows {i} and {j} must have distinct ids");
            }
        }
        assert!(term.primary.rows.iter().all(|l| !l.wrapped));
    }

    /// Rewrap-on-resize round trip (M2 acceptance): a 100-char
    /// logical line at 80 cols (2 physical rows) reflows to 3 rows
    /// at 40 cols with every cell preserved, then reflows BACK to
    /// the original layout losslessly.
    #[test]
    fn reflow_roundtrip_preserves_wrapped_logical_line() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let text = hundred_chars();
        term.feed(text.as_bytes());

        let before = visible_text(&term);
        assert_eq!(before[0], text[..80]);
        assert_eq!(before[1], text[80..]);
        let id = term.primary.rows[0].logical_id;

        term.resize(40, 24);
        let narrow = visible_text(&term);
        assert_eq!(narrow[0], text[..40], "row 0 after narrow reflow");
        assert_eq!(narrow[1], text[40..80], "row 1 after narrow reflow");
        assert_eq!(narrow[2], text[80..], "row 2 after narrow reflow");
        for (i, want_wrapped) in [(0, true), (1, true), (2, false)] {
            assert_eq!(
                term.primary.rows[i].wrapped, want_wrapped,
                "row {i} wrap marker after narrow reflow"
            );
            assert_eq!(
                term.primary.rows[i].logical_id, id,
                "row {i} keeps the logical id through reflow"
            );
        }

        term.resize(80, 24);
        let after = visible_text(&term);
        assert_eq!(after, before, "80→40→80 round trip is lossless");
        assert!(term.primary.rows[0].wrapped);
        assert!(!term.primary.rows[1].wrapped);
        assert_eq!(term.primary.rows[0].logical_id, id);
    }

    /// Rewrap treats scrollback + visible rows as ONE continuous
    /// sequence: a logical line whose head already scrolled out of
    /// view re-joins correctly when widening makes it fit again.
    #[test]
    fn reflow_joins_scrollback_and_visible_rows_of_one_logical_line() {
        let mut term = Terminal::with_scrollback(10, 3, 100);
        // 22 chars at 10 cols → 3 physical rows of one logical line.
        term.feed(b"aaaaaaaaaabbbbbbbbbbcc");
        // Two hard lines push the line's head + middle into scrollback.
        term.feed(b"\r\nx\r\ny");
        assert_eq!(term.primary.scrollback_len(), 2);
        assert!(term.primary.rows[0].wrapped, "head row is in scrollback");

        term.resize(22, 3);
        assert_eq!(
            visible_text(&term),
            vec!["aaaaaaaaaabbbbbbbbbbcc", "x", "y"],
            "the whole logical line re-joined across the boundary"
        );
        assert_eq!(term.primary.scrollback_len(), 0);
        assert!(!term.primary.rows[0].wrapped);
    }

    /// Truncation matrix (M2 acceptance): the ALT grid always
    /// truncates on resize (full-screen TUIs redraw themselves), and
    /// `reflow_on_resize = false` restores legacy truncation on the
    /// primary grid. In both cases the middle of the logical line
    /// (chars 40..80) is LOST — that is what distinguishes truncate
    /// from rewrap.
    #[test]
    fn resize_truncation_matrix() {
        struct MatrixRow {
            name: &'static str,
            alt_screen: bool,
            reflow: bool,
        }
        const MATRIX: &[MatrixRow] = &[
            MatrixRow {
                name: "alt screen truncates even with reflow on",
                alt_screen: true,
                reflow: true,
            },
            MatrixRow {
                name: "reflow_on_resize=false truncates the primary",
                alt_screen: false,
                reflow: false,
            },
        ];

        let mut failures: Vec<String> = Vec::new();
        for case in MATRIX {
            let mut term = Terminal::with_scrollback(80, 24, 100);
            term.set_reflow_on_resize(case.reflow);
            if case.alt_screen {
                term.feed(b"\x1b[?1049h");
            }
            let text = hundred_chars();
            term.feed(text.as_bytes());
            term.resize(40, 24);
            let rows = visible_text(&term);
            if rows[0] != text[..40] {
                failures.push(format!(
                    "{}: row 0 = {:?}, want first 40 chars",
                    case.name, rows[0]
                ));
            }
            if rows[1] != text[80..] {
                failures.push(format!(
                    "{}: row 1 = {:?}, want last 20 chars",
                    case.name, rows[1]
                ));
            }
            // Truncate keeps 40 (row 0) + 20 (row 1) = 60 chars; the
            // middle 40 are gone. (A rewrap would keep all 100 — the
            // digit cycle repeats every 10, so substring checks can't
            // distinguish; the surviving CELL COUNT can.)
            let kept: usize = rows.iter().map(String::len).sum();
            if kept != 60 {
                failures.push(format!(
                    "{}: {kept} chars survived, want exactly 60 (middle 40 lost)",
                    case.name
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} truncation cases failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Mark re-anchoring (M2 acceptance): an OSC 133 A prompt mark
    /// on a soft-wrapped logical line resolves to the SAME logical
    /// line after a reflow that changes the physical row count —
    /// and a mark BELOW the wrapped line shifts by the row delta.
    #[test]
    fn prompt_mark_on_wrapped_line_resolves_to_same_logical_line_after_reflow() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        term.feed(b"first\r\n"); // row 0
        term.feed(b"\x1b]133;A\x1b\\"); // mark at row 1
        let text = hundred_chars();
        term.feed(text.as_bytes()); // wraps rows 1-2
        term.feed(b"\r\n"); // cursor → row 3
        term.feed(b"\x1b]133;A\x1b\\"); // mark at row 3
        term.feed(b"second prompt");

        let id_long = term.primary.rows[1].logical_id;
        let marks: Vec<usize> =
            term.prompt_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(marks, vec![1, 3]);

        term.resize(40, 24);
        // The long line now occupies rows 1..=3, pushing the second
        // prompt to row 4.
        let marks: Vec<usize> =
            term.prompt_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(
            marks,
            vec![1, 4],
            "marks re-anchor through (LogicalLineId, offset), not raw rows"
        );
        assert_eq!(
            term.primary.rows[marks[0]].logical_id, id_long,
            "first mark still heads the SAME logical line"
        );
        let rows = visible_text(&term);
        assert_eq!(rows[marks[0]], text[..40]);
        assert!(
            rows[marks[1]].starts_with("second prompt"),
            "second mark follows its line to its new physical row: {rows:?}"
        );

        // Widen back — both marks return to their original rows.
        term.resize(80, 24);
        let marks: Vec<usize> =
            term.prompt_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(marks, vec![1, 3], "round trip restores mark rows");
    }

    /// A user mark (OSC 1337 SetMark) re-anchors through the same
    /// logical-line bridge as prompt marks.
    #[test]
    fn user_mark_reanchors_after_reflow() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let text = hundred_chars();
        term.feed(text.as_bytes()); // rows 0-1, one logical line
        term.feed(b"\r\n");
        term.feed(b"\x1b]1337;SetMark\x07"); // mark at row 2
        term.feed(b"marked line");

        let marks: Vec<usize> =
            term.user_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(marks, vec![2]);

        term.resize(40, 24); // long line grows to 3 rows
        let marks: Vec<usize> =
            term.user_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(marks, vec![3], "user mark shifted by the reflow delta");
        assert!(visible_text(&term)[marks[0]].starts_with("marked line"));
    }

    /// Cursor re-anchoring (M2 review wave): the cursor rides the
    /// same logical-line bridge the marks do — cell-precise, so a
    /// cursor sitting just after the END of a wrapped 100-char line
    /// lands just after the same char at every width.
    #[test]
    fn cursor_reanchors_to_cell_position_across_rewrap() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        term.feed(b"first\r\n"); // row 0
        let text = hundred_chars();
        term.feed(text.as_bytes()); // rows 1-2 at 80 cols
        assert_eq!((term.cursor().row, term.cursor().col), (2, 20));

        term.resize(40, 24); // the line now spans rows 1..=3
        assert_eq!(
            (term.cursor().row, term.cursor().col),
            (3, 20),
            "cursor sits just after the line's last char at 40 cols"
        );

        term.resize(80, 24);
        assert_eq!(
            (term.cursor().row, term.cursor().col),
            (2, 20),
            "round trip restores the cursor position"
        );
    }

    /// The DECSC saved cursor (primary) crosses a rewrap through the
    /// same cell-precise bridge, so DECRC after a column resize
    /// restores onto the same content.
    #[test]
    fn saved_cursor_reanchors_across_rewrap() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let text = hundred_chars();
        term.feed(text.as_bytes()); // rows 0-1; cursor (1, 20)
        term.feed(b"\x1b7"); // DECSC at (1, 20)
        term.feed(b"\r\nnext line");

        term.resize(40, 24); // the line now spans rows 0..=2
        term.feed(b"\x1b8"); // DECRC
        assert_eq!(
            (term.cursor().row, term.cursor().col),
            (2, 20),
            "DECRC restores just after the line's last char"
        );
    }

    /// SETTLE trim bound (M2 review wave): the canonical post-`clear`
    /// state — prompt at the viewport top, blank rows below, real
    /// scrollback behind — must survive a column resize untouched.
    /// The pre-fix trim popped every trailing blank while rows
    /// exceeded the visible count, sliding scrollback into the
    /// viewport and dropping the prompt to the bottom.
    #[test]
    fn rewrap_settle_keeps_cleared_viewport_anchored() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        for i in 0..30 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        // kitty-style `clear`: erase display + home (scrollback kept).
        term.feed(b"\x1b[2J\x1b[H");
        term.feed(b"prompt$");
        let sb_before = term.primary.scrollback_len();
        assert!(sb_before > 0, "scrollback survives the clear");
        assert_eq!(visible_text(&term)[0], "prompt$");

        term.resize(120, 24); // column-only resize → rewrap
        assert_eq!(
            visible_text(&term)[0],
            "prompt$",
            "prompt stays at the viewport top"
        );
        assert_eq!(
            term.primary.scrollback_len(),
            sb_before,
            "no scrollback backfill into the cleared viewport"
        );

        term.resize(40, 24); // narrow too
        assert_eq!(visible_text(&term)[0], "prompt$");
        assert_eq!(term.primary.scrollback_len(), sb_before);
    }

    /// The SETTLE trim still drops blank tails the reflow itself
    /// displaced: content growing past the visible area pushes its
    /// head out of view, NOT the blanks' fault — the blank rows
    /// below shrink to make room (no scrollback masquerade).
    #[test]
    fn rewrap_settle_still_trims_blanks_displaced_by_content_growth() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let text = hundred_chars();
        term.feed(text.as_bytes()); // 2 content rows + 22 blanks
        assert_eq!(term.primary.scrollback_len(), 0);

        term.resize(40, 24); // content grows to 3 rows
        assert_eq!(
            term.primary.scrollback_len(),
            0,
            "blank tail shrank instead of masquerading as scrollback"
        );
        let rows = visible_text(&term);
        assert_eq!(rows[0], text[..40]);
        assert_eq!(rows[2], text[80..]);
    }

    /// Alt-screen resize must not zero the PRIMARY grid's pinned
    /// scroll offset (the alt grid's scrollback_len() is a constant
    /// 0 — clamping against the ACTIVE grid wiped a reading
    /// position). Extends the
    /// alt_screen_output_does_not_disturb_primary_scroll_offset
    /// family with the resize edge.
    #[test]
    fn alt_screen_resize_preserves_primary_scroll_offset() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        for i in 0..40 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        term.scroll_up(5);
        assert_eq!(term.scroll_offset(), 5);

        term.feed(b"\x1b[?1049h"); // enter alt (vim/less)
        term.resize(100, 24); // window resized while the TUI runs
        term.feed(b"\x1b[?1049l"); // exit alt

        assert_eq!(
            term.scroll_offset(),
            5,
            "primary reading position survives an alt-screen resize"
        );
    }

    /// scroll_offset content-pinning (M2 review wave): a scrolled-up
    /// viewport keeps showing the SAME content after a rewrap
    /// renumbers physical rows — the same contract streaming output
    /// honours via scroll_grid_up's lockstep offset growth. The
    /// wrapped line sits BELOW the viewport top, so its row-count
    /// change shifts the bottom-relative numeric offset — exactly
    /// the case a plain clamp gets wrong.
    #[test]
    fn scrolled_viewport_content_pinned_across_rewrap() {
        let mut term = Terminal::with_scrollback(80, 24, 1000);
        for i in 0..20 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        let text = hundred_chars();
        term.feed(text.as_bytes()); // 2 physical rows at 80 cols
        term.feed(b"\r\n");
        for i in 20..40 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        // Scroll up so the viewport top is a short line ABOVE the
        // wrapped region.
        term.scroll_up(12);
        let top_before = visible_text(&term)[0].clone();
        assert!(
            top_before.starts_with("line"),
            "viewport top is a known content row: {top_before:?}"
        );

        term.resize(40, 24); // wrapped line below grows by one row
        assert_eq!(
            visible_text(&term)[0],
            top_before,
            "viewport top shows the same logical content after rewrap"
        );

        term.resize(80, 24);
        assert_eq!(visible_text(&term)[0], top_before);
    }

    /// stamp_soft_wrap gating (M2 review wave): an autowrap with the
    /// cursor BELOW an active DECSTBM region scrolls the region
    /// without moving the cursor — the wrap stamp must NOT fire (it
    /// would mark the unrelated row above wrapped and overwrite the
    /// cursor row's logical id).
    #[test]
    fn autowrap_below_scroll_region_does_not_stamp_wrap_marker() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[1;10r"); // DECSTBM rows 1-10 (0-indexed 0..=9)
        term.feed(b"\x1b[19;1H"); // CUP to row 19 (0-indexed 18) — below the region
        let id_above_before = term.primary.rows[17].logical_id;
        let id_cursor_before = term.primary.rows[18].logical_id;
        assert_ne!(id_above_before, id_cursor_before);

        let text: String = hundred_chars().chars().take(90).collect();
        term.feed(text.as_bytes()); // autowraps once at col 80

        assert!(
            term.primary.rows.iter().all(|l| !l.wrapped),
            "no row gains a wrap marker from a region-scroll wrap"
        );
        assert_eq!(
            term.primary.rows[17].logical_id, id_above_before,
            "the row above the cursor keeps its own logical id"
        );
        assert_ne!(
            term.primary.rows[17].logical_id,
            term.primary.rows[18].logical_id,
            "the cursor row does not get joined to the row above"
        );
    }

    /// Autowrap at the bottom of a full-screen region still stamps
    /// (the wrapped-from row sits directly above the cursor after
    /// the scroll) — the gate must not break the normal wrap path.
    #[test]
    fn autowrap_at_screen_bottom_still_stamps_wrap_marker() {
        let mut term = Terminal::with_scrollback(10, 3, 100);
        term.feed(b"\x1b[3;1H"); // bottom row
        term.feed(b"aaaaaaaaaabb"); // 12 chars: wraps, scrolls once
        // The wrapped-from row is now directly above the cursor.
        let sb = term.primary.scrollback_len();
        assert_eq!(sb, 1);
        assert!(
            term.primary.rows[sb + 1].wrapped,
            "wrapped-from row carries the marker after the scroll"
        );
        assert_eq!(
            term.primary.rows[sb + 1].logical_id,
            term.primary.rows[sb + 2].logical_id,
            "continuation row joined the logical line"
        );
    }

    /// Wide-char early wrap leaves a never-written spacer cell in
    /// the wrapped row's last column; the rewrap must not splice it
    /// into the logical line as interior content (phantom blank
    /// between char 79 and the CJK char).
    #[test]
    fn wide_char_early_wrap_spacer_is_not_interior_content_after_rewrap() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let prefix: String = (0..79u32)
            .map(|i| char::from_digit(i % 10, 10).unwrap())
            .collect();
        term.feed(prefix.as_bytes());
        term.feed("你".as_bytes()); // width 2 at col 79 → early wrap
        assert!(term.primary.rows[0].wrapped, "early wrap stamped the marker");

        term.resize(100, 24); // whole line fits on one row now
        assert_eq!(
            visible_text(&term)[0],
            format!("{prefix}你"),
            "no phantom space between char 79 and the wide char"
        );
    }

    /// Marker-broken lines (M2 review wave): an erase to the right
    /// edge breaks the soft-wrap marker while both halves keep the
    /// shared logical id. A mark on the SECOND half must re-anchor
    /// onto the second run after a rewrap — not the erased first
    /// half (`physical_row_of` walks the same marker-contiguous runs
    /// `anchor_at` does).
    #[test]
    fn mark_on_second_half_of_marker_broken_line_stays_on_its_run() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let text = hundred_chars();
        term.feed(text.as_bytes()); // rows 0-1, one logical line
        // Erase row 0 from col 5 to the EOL — breaks the marker.
        term.feed(b"\x1b[1;6H\x1b[K");
        assert!(!term.primary.rows[0].wrapped, "erase to EOL breaks the marker");
        assert_eq!(
            term.primary.rows[0].logical_id,
            term.primary.rows[1].logical_id,
            "both halves keep the shared id"
        );
        // Mark the SECOND half (row 1).
        term.feed(b"\x1b[2;1H\x1b]1337;SetMark\x07");
        let marks: Vec<usize> =
            term.user_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(marks, vec![1]);

        term.resize(40, 24);
        let marks: Vec<usize> =
            term.user_marks().iter().map(|m| m.grid_row).collect();
        assert_eq!(
            visible_text(&term)[marks[0]],
            text[80..].to_string(),
            "mark resolved onto the second run's content, not the erased half"
        );
    }

    /// Kitty image placements ride the rewrap bridge: a placement on
    /// a row below a wrapped line follows its logical line when the
    /// line's physical row count changes.
    #[test]
    fn image_placement_reanchors_across_rewrap() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        let text = hundred_chars();
        term.feed(text.as_bytes()); // rows 0-1
        term.feed(b"\r\n"); // cursor → row 2
        let rgba = [255, 0, 0, 255]; // 1×1 red pixel
        let b64 = base64_encode(&rgba);
        let apc = format!("\x1b_Ga=T,f=32,s=1,v=1,i=7;{b64}\x1b\\");
        term.feed(apc.as_bytes());
        assert_eq!(term.image_placements()[0].row, 2);

        term.resize(40, 24); // the line above grows to 3 rows
        assert_eq!(
            term.image_placements()[0].row,
            3,
            "placement follows its logical line down"
        );

        term.resize(80, 24);
        assert_eq!(term.image_placements()[0].row, 2, "round trip restores");
    }

    /// LinkTable gc parity (M2 review wave): saturating the table
    /// with dead URIs must not disable hyperlinks for the session —
    /// the gc-then-retry path rebuilds from live cells, remaps their
    /// ids, and re-interns the new URI.
    #[test]
    fn link_table_gc_remaps_live_cells_on_saturation() {
        let mut term = Terminal::new(80, 24);
        // Paint one linked cell (id 1) that must survive the gc.
        term.feed(b"\x1b]8;;https://keep.example\x1b\\K\x1b]8;;\x1b\\");
        // Saturate with URIs nothing references.
        for i in 1..(u16::MAX as usize) {
            term.link_table.intern(&format!("file:///dead/{i}"));
        }
        assert!(
            term.link_table.try_intern("https://fresh.example").is_none(),
            "table saturated"
        );
        // A fresh OSC 8 link forces the gc-then-retry path.
        term.feed(b"\x1b]8;;https://fresh.example\x1b\\F\x1b]8;;\x1b\\");
        assert_eq!(
            term.cell(0, 0).hyperlink(term.links()),
            Some("https://keep.example"),
            "live cell remapped, not orphaned"
        );
        assert_eq!(
            term.cell(0, 1).hyperlink(term.links()),
            Some("https://fresh.example"),
            "fresh URI interned after gc"
        );
        assert_eq!(term.links().len(), 2, "every dead URI was collected");
    }

    /// SGR 21 — double underline (ECMA-48 / kitty wire), the
    /// semicolon-form sibling of the 4:2 sub-param.
    #[test]
    fn sgr_21_sets_double_underline() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"\x1b[21mD");
        assert_eq!(
            term.cell(0, 0).attrs(term.styles()).underline,
            UnderlineStyle::Double
        );
        term.feed(b"\x1b[24mn"); // SGR 24 clears underline
        assert_eq!(
            term.cell(0, 1).attrs(term.styles()).underline,
            UnderlineStyle::None
        );
    }

    /// RIS palette policy (M2 review wave): `reset` restores the
    /// extended 16..=255 cube/grayscale entries to the computed
    /// defaults — an app's OSC 4 overrides must not outlive ESC c —
    /// while the theme-owned base 16 slots survive as-is.
    #[test]
    fn ris_restores_extended_palette_preserving_base_16() {
        let mut term = Terminal::new(80, 24);
        // App overrides a cube entry + a base entry via OSC 4.
        term.feed(b"\x1b]4;200;#112233\x1b\\");
        term.feed(b"\x1b]4;1;#445566\x1b\\");
        assert_eq!(term.ansi_palette()[200], Color::new(0x11, 0x22, 0x33));

        term.feed(b"\x1bc"); // RIS
        assert_eq!(
            term.ansi_palette()[200],
            default_palette_256()[200],
            "cube entry restored to the computed default"
        );
        assert_eq!(
            term.ansi_palette()[1],
            Color::new(0x44, 0x55, 0x66),
            "base-16 slot carries across RIS as-is"
        );
    }

    /// StyleSnapshot is a drop-in read surface for the render path:
    /// lookups agree with the owning table, including the defensive
    /// out-of-bounds fallback.
    #[test]
    fn style_snapshot_lookup_matches_table() {
        let mut table = StyleTable::new();
        let style = Style {
            fg: Color::new(1, 2, 3),
            bg: Color::new(4, 5, 6),
            attrs: Attrs::NONE,
        };
        let id = table.intern(style);
        let snap = table.snapshot();
        assert_eq!(StyleLookup::lookup(&snap, id), table.lookup(id));
        assert_eq!(
            StyleLookup::lookup(&snap, DEFAULT_STYLE_ID),
            table.lookup(DEFAULT_STYLE_ID)
        );
        assert_eq!(StyleLookup::lookup(&snap, 9999), table.lookup(9999));
    }

    /// Every effective resize bumps the grid generation (the search
    /// re-anchoring seam); a same-dims resize is a no-op and must
    /// not.
    #[test]
    fn grid_generation_bumps_on_effective_resize_only() {
        let mut term = Terminal::new(80, 24);
        let g0 = term.grid_generation();
        term.resize(80, 24); // same dims — no-op
        assert_eq!(term.grid_generation(), g0);
        term.resize(100, 24);
        assert_eq!(term.grid_generation(), g0 + 1);
        term.resize(100, 30);
        assert_eq!(term.grid_generation(), g0 + 2);
    }

    // ── content-anchored selection (M2 bridge consumers) ──────────

    /// Streaming output must slide UNDER an anchored selection
    /// without changing what it selects — matrix over "scrollback
    /// still under the cap" and "eviction already started but the
    /// selected content survives". (Pre-anchor, row-addressed
    /// selections silently re-pointed at whatever content scrolled
    /// into their rows.)
    #[test]
    fn streaming_under_selection_extracted_text_is_stable() {
        use std::fmt::Write as _;
        struct Variant {
            name: &'static str,
            cap: usize,
            prelude_lines: usize,
            follow_lines: usize,
            expect_eviction: bool,
        }
        let variants = [
            Variant {
                name: "below eviction threshold",
                cap: 100,
                prelude_lines: 0,
                follow_lines: 30,
                expect_eviction: false,
            },
            Variant {
                name: "past eviction threshold, selection survives",
                cap: 50,
                prelude_lines: 40,
                follow_lines: 60,
                expect_eviction: true,
            },
        ];
        let mut failures: Vec<String> = Vec::new();
        for v in &variants {
            let mut term = Terminal::with_scrollback(80, 24, v.cap);
            for _ in 0..v.prelude_lines {
                term.feed(b"filler\r\n");
            }
            // Target stays under the cursor (no trailing newline) so
            // its viewport row is exactly cursor.row at capture time.
            term.feed(b"alpha bravo");
            let row = term.cursor().row;
            let a = term.selection_anchor_at(row, 0).expect("anchor start");
            let b = term.selection_anchor_at(row, 10).expect("anchor end");
            let before = term.extract_selection_text(a, b);
            if before.as_deref() != Some("alpha bravo") {
                let mut m = String::new();
                let _ = write!(m, "{}: pre-stream extract = {before:?}", v.name);
                failures.push(m);
                continue;
            }
            for _ in 0..v.follow_lines {
                term.feed(b"\r\nfiller");
            }
            if v.expect_eviction && term.scrollback_total() < v.cap {
                let mut m = String::new();
                let _ = write!(
                    m,
                    "{}: harness bug — wanted eviction, scrollback {} < cap {}",
                    v.name,
                    term.scrollback_total(),
                    v.cap
                );
                failures.push(m);
            }
            let after = term.extract_selection_text(a, b);
            if after != before {
                let mut m = String::new();
                let _ = write!(
                    m,
                    "{}: extract drifted under streaming: {before:?} → {after:?}",
                    v.name
                );
                failures.push(m);
            }
        }
        assert!(
            failures.is_empty(),
            "{} streaming variants failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// A selection across a soft-wrapped line survives a rewrap
    /// round trip (narrower then back) selecting the SAME content —
    /// the anchors re-derive (row, col) from the cell offset at
    /// whatever the current width is.
    #[test]
    fn selection_rewrap_round_trip_preserves_content() {
        let mut term = Terminal::new(80, 24);
        let long: String = (0..100u32)
            .map(|i| char::from_digit(i % 10, 10).expect("digit"))
            .collect();
        term.feed(long.as_bytes());
        // Span crossing the soft-wrap boundary: cells 70..=85.
        let a = term.selection_anchor_at(0, 70).expect("anchor start");
        let b = term.selection_anchor_at(1, 5).expect("anchor end");
        let want = &long[70..=85];
        assert_eq!(term.extract_selection_text(a, b).as_deref(), Some(want));
        term.resize(40, 24);
        assert_eq!(
            term.extract_selection_text(a, b).as_deref(),
            Some(want),
            "narrow rewrap must keep the same selected content"
        );
        term.resize(80, 24);
        assert_eq!(
            term.extract_selection_text(a, b).as_deref(),
            Some(want),
            "round trip back must keep the same selected content"
        );
    }

    /// Once the selected content is evicted from scrollback, the
    /// anchors resolve to None — never to clamped garbage rows.
    #[test]
    fn eviction_resolves_selection_to_none() {
        let mut term = Terminal::with_scrollback(80, 24, 5);
        term.feed(b"doomed line");
        let a = term.selection_anchor_at(0, 0).expect("anchor start");
        let b = term.selection_anchor_at(0, 10).expect("anchor end");
        assert!(term.resolve_selection_span(a, b).is_some());
        for _ in 0..60 {
            term.feed(b"\r\nfiller");
        }
        assert_eq!(
            term.resolve_selection_span(a, b),
            None,
            "evicted content must resolve to None"
        );
        assert_eq!(term.extract_selection_text(a, b), None);
    }

    /// Partial scroll-region surgery can split a soft-wrapped logical
    /// line into NON-adjacent same-id runs: `scroll_region_up`'s
    /// partial path removes the region-top row and inserts a fresh
    /// blank at the region bottom — when the wrapped pair straddles
    /// the region edge, the blank lands BETWEEN head and tail. The
    /// pre-fix `line_runs` early-break dropped the orphaned tail run,
    /// so an anchor captured there resolved to the head run's last
    /// cell (wrong highlight + wrong copy) instead of its own cell
    /// (M3 review 2026-06-12). Reachable with any DECSTBM app
    /// scrolling a primary screen holding earlier soft-wrapped output.
    #[test]
    fn anchor_in_scroll_region_orphaned_wrap_tail_resolves_to_its_own_cell() {
        let mut term = Terminal::with_scrollback(10, 6, 100);
        // Soft-wrap a 20-char line across rows 3 and 4 (0-based).
        term.feed(b"\x1b[4;1H");
        term.feed(b"XXXXXXXXXXYYYYYYYYYY");
        // DECSTBM region rows 1..=4 (1-based) — bottom bisects the
        // wrapped pair (head in-region at row 3, tail outside at 4).
        // LF at the region bottom scrolls the region: row 0 removed,
        // fresh blank inserted at row 3, head shifts up to row 2.
        term.feed(b"\x1b[1;4r");
        term.feed(b"\x1b[4;1H\n");

        // Viewport row 4 col 5 is the tail's 'Y' band — capture and
        // resolve must round-trip to the SAME absolute cell.
        let a = term.selection_anchor_at(4, 5).expect("tail anchor");
        assert_eq!(
            term.resolve_selection_anchor(a),
            Some((4, 5)),
            "anchor in the orphaned tail run must resolve to its own cell, \
             not clamp to the head run"
        );
        // And the span over the tail extracts tail content, not head.
        let b = term.selection_anchor_at(4, 9).expect("tail end anchor");
        assert_eq!(
            term.extract_selection_text(a, b).as_deref(),
            Some("YYYYY"),
            "copy from the orphaned tail must yield tail bytes"
        );
    }

    /// The seqno-keyed span memo serves only fresh resolutions: a
    /// memoized span from before a content shift must be recomputed,
    /// never replayed (M3 review 2026-06-12 — the memo exists because
    /// the render path resolves the live span every vsync, which was
    /// O(scrollback) per anchor per frame).
    #[test]
    fn selection_span_memo_invalidates_on_any_write() {
        let mut term = Terminal::with_scrollback(20, 4, 100);
        term.feed(b"anchor me");
        let a = term.selection_anchor_at(0, 0).expect("start");
        let b = term.selection_anchor_at(0, 5).expect("end");
        let first = term.resolve_selection_span(a, b).expect("resolves");
        // Memo hit: identical state, identical answer.
        assert_eq!(term.resolve_selection_span(a, b), Some(first));
        // Content shift: four scrolled lines move the anchored row
        // into scrollback — the resolved rows MUST move with it.
        term.feed(b"\r\n1\r\n2\r\n3\r\n4");
        let shifted = term.resolve_selection_span(a, b).expect("still resolvable");
        assert_eq!(
            (shifted.0 .0, shifted.1 .0),
            (first.0 .0, first.1 .0),
            "absolute rows are scrollback-origin-stable while content survives"
        );
        // Different anchors immediately after a memoized pair must
        // not replay the previous pair's answer.
        let c = term.selection_anchor_at(1, 0).expect("other row");
        let other = term.resolve_selection_span(a, c).expect("resolves");
        assert_ne!(other.1, first.1, "different end anchor, different span");
    }

    /// Soft-wrap-aware extraction matrix (the kitty/ghostty copy
    /// contract): wrap junctions join without a newline, hard line
    /// ends trim trailing blanks then emit one, wide-char
    /// continuation spacers vanish.
    #[test]
    fn extract_selection_text_matrix() {
        use std::fmt::Write as _;
        struct Case {
            name: &'static str,
            feed: Vec<u8>,
            start: (usize, usize),
            end: (usize, usize),
            want: &'static str,
        }
        let hundred: String = (0..100u32)
            .map(|i| char::from_digit(i % 10, 10).expect("digit"))
            .collect();
        let hundred_static: &'static str = hundred.clone().leak();
        let cases = [
            Case {
                name: "100-char wrapped line copies with NO interior newline",
                feed: hundred.into_bytes(),
                start: (0, 0),
                end: (1, 19),
                want: hundred_static,
            },
            Case {
                name: "hard newline between logical lines survives",
                feed: b"first\r\nsecond".to_vec(),
                start: (0, 0),
                end: (1, 5),
                want: "first\nsecond",
            },
            Case {
                name: "wide-char line round-trips, spacers skipped",
                feed: "日本語".as_bytes().to_vec(),
                start: (0, 0),
                end: (0, 5),
                want: "日本語",
            },
            Case {
                name: "trailing blanks trimmed at the hard line end",
                feed: b"hi  \r\nworld".to_vec(),
                start: (0, 0),
                end: (1, 4),
                want: "hi\nworld",
            },
            Case {
                name: "spaces at a soft-wrap junction are content, not trim fodder",
                feed: {
                    // 78 'a' + 2 spaces fill the row (soft wrap), then
                    // 'bb' continues the logical line.
                    let mut f = vec![b'a'; 78];
                    f.extend_from_slice(b"  bb");
                    f
                },
                start: (0, 70),
                end: (1, 1),
                want: "aaaaaaaa  bb",
            },
        ];
        let mut failures: Vec<String> = Vec::new();
        for c in &cases {
            let mut term = Terminal::new(80, 24);
            term.feed(&c.feed);
            let span = term
                .selection_anchor_at(c.start.0, c.start.1)
                .zip(term.selection_anchor_at(c.end.0, c.end.1));
            let Some((a, b)) = span else {
                let mut m = String::new();
                let _ = write!(m, "{}: span failed to anchor", c.name);
                failures.push(m);
                continue;
            };
            let got = term.extract_selection_text(a, b);
            if got.as_deref() != Some(c.want) {
                let mut m = String::new();
                let _ = write!(m, "{}: want {:?}, got {got:?}", c.name, c.want);
                failures.push(m);
            }
        }
        assert!(
            failures.is_empty(),
            "{} extraction variants failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// RIS rebuilds both grids and restarts the `LogicalLineId`
    /// counters — the epoch tag must reject pre-reset anchors before
    /// an aliased id resolves onto unrelated post-reset content.
    #[test]
    fn reset_rejects_pre_reset_anchors() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"hello");
        let a = term.selection_anchor_at(0, 0).expect("anchor start");
        let b = term.selection_anchor_at(0, 4).expect("anchor end");
        assert!(term.resolve_selection_span(a, b).is_some());
        term.reset();
        term.feed(b"after-reset content");
        assert_eq!(
            term.resolve_selection_span(a, b),
            None,
            "pre-reset anchors must not alias post-reset lines"
        );
    }

    /// Anchors are screen-tagged: a primary-screen selection goes
    /// dormant (resolves None) while the alternate screen is active
    /// — ids alias across the two grids, so cross-screen resolution
    /// would highlight unrelated TUI content.
    #[test]
    fn alt_screen_suspends_primary_anchors() {
        let mut term = Terminal::new(80, 24);
        term.feed(b"primary text");
        let a = term.selection_anchor_at(0, 0).expect("anchor start");
        let b = term.selection_anchor_at(0, 6).expect("anchor end");
        assert!(term.resolve_selection_span(a, b).is_some());
        term.feed(b"\x1b[?1049h"); // TUI opens
        assert_eq!(
            term.resolve_selection_span(a, b),
            None,
            "primary anchors must not resolve against the alt grid"
        );
        term.feed(b"\x1b[?1049l"); // TUI exits
        assert!(
            term.resolve_selection_span(a, b).is_some(),
            "primary anchors resolve again once the primary screen returns"
        );
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
            let mut term = Terminal::new(80, 24);
            term.feed(&input);
            let a = term.selection_anchor_at(0, 0);
            let b = term.selection_anchor_at(term.rows() - 1, term.cols() - 1);
            prop_assert!(a.is_some() && b.is_some(),
                "visible viewport corners must always anchor");
            if let Some(text) = term.extract_selection_text(
                a.expect("asserted above"),
                b.expect("asserted above"),
            ) {
                for (i, byte) in text.bytes().enumerate() {
                    prop_assert!(
                        byte == b'\n' || byte >= 0x20,
                        "control byte 0x{byte:02x} at offset {i} leaked into \
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
