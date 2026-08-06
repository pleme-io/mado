//! OSC 1337 — iTerm2 proprietary extensions.
//!
//! iTerm2 ships a proprietary family of OSC sequences under the
//! shared prefix `ESC ] 1337 ; …`. Ghostty, kitty, foot, wezterm,
//! and alacritty all implement at least the two that matter for
//! shell ergonomics:
//!
//! | Argument                    | Semantics                              |
//! |-----------------------------|----------------------------------------|
//! | `SetMark`                   | Plant an explicit user mark at cursor  |
//! | `RequestAttention=<0 / 1>`  | Bounce dock / flash titlebar (on/off)  |
//! | `File=<args>:<base64>`      | Inline image (`imgcat`)                |
//!
//! Mado previously ignored the whole family. This module adds a
//! typed parameter parser + the two marker / attention lifecycle
//! pieces + the `File=` inline-image parse, with room for the
//! longer-tail keys (`CopyToClipboard`) to land in subsequent ticks
//! without reshaping the dispatch.
//!
//! ## User marks vs. OSC 133 prompt marks
//!
//! `OSC 133 A` is shell-emitted — the prompt itself records its
//! location. `OSC 1337 SetMark` is user-emitted, typed by a
//! `\e]1337;SetMark\e\\` echoed from a script or an inline key
//! action. Both land in the grid at the current cursor row; they
//! differ in provenance and in what jumps them in the UI. Mado
//! tracks them in separate typed histories so a "jump between
//! prompts" binding doesn't cross-contaminate "jump between user
//! marks".
//!
//! ## Parser shape
//!
//! The argument after `1337;` is either a bare identifier
//! (`SetMark`) or `key=value`. Unknown keys decode to
//! [`Osc1337Param::Unknown`] so the dispatcher can trace them
//! without corrupting typed state.
//!
//! ## `File=` — the iTerm2 inline image (`imgcat`)
//!
//! The wire form is
//! `ESC ] 1337 ; File=k=v;k=v;… : <base64> ST`. Two shapes in that
//! grammar decide the parser's signature:
//!
//! 1. **The argument list is itself `;`-separated**, and `;` is the
//!    OSC parameter separator — so vte hands the dispatcher the one
//!    logical argument as N params. [`parse_osc_1337_file`] therefore
//!    takes the whole param tail and rejoins it, where
//!    [`parse_osc_1337`] takes one param.
//! 2. **The payload follows a `:`**, which never appears in the
//!    base64 alphabet nor in any iTerm2 key or value, so the FIRST
//!    `:` is the split point unambiguously.
//!
//! A `File=` sequence decodes to [`FileOutcome`]: `Image` (a typed
//! [`InlineFile`] whose payload is already base64-decoded), or
//! `Rejected` with a typed [`FileError`] — never a partial image,
//! never a panic. `NotFile` means "this is a marker/attention
//! sequence", so the caller falls through to [`parse_osc_1337`].
//!
//! ### Bounding posture (matches the sixel path)
//!
//! `terminal.rs` bounds a sixel DCS at `SIXEL_DCS_MAX` (8 MiB) and
//! rejects the WHOLE sequence past it rather than decoding a
//! partial. [`FILE_PAYLOAD_MAX`] is the same number with the same
//! posture, checked against the *encoded* length before any decode
//! allocation, plus a cheaper pre-check on a declared `size=`.
//!
//! **The one honest difference:** sixel's cap is enforced
//! *incrementally*, in `Perform::put`, so an unterminated payload can
//! never grow the buffer. The OSC equivalent of that hook lives
//! inside vte, and vte 0.15 with its default `std` feature buffers
//! OSC bytes in an **unbounded `Vec`** (`osc_raw`) — so by the time
//! this parser sees the payload, the bytes are already resident.
//! This module bounds what gets *decoded and placed*; it cannot bound
//! what gets *accumulated*. Closing that axis means a bounded
//! `vte::Parser<N>` (a no-std build) or an OSC-level pre-filter in
//! `terminal.rs`, and neither belongs in this file.
//!
//! ### Wiring (not done here — `terminal.rs` is another owner's file)
//!
//! `handle_osc_1337_iterm2` needs one pre-dispatch branch:
//!
//! ```ignore
//! match crate::osc_1337::parse_osc_1337_file(&params[1..]) {
//!     FileOutcome::Image(f) if f.inline => match f.decode_rgba() {
//!         Ok(img) => {
//!             let id = self.next_image_id;
//!             self.next_image_id += 1;
//!             self.seqno += 1;
//!             self.store_rgba_image(id, img.pixels, img.width, img.height);
//!             self.place_decoded_image_at_cursor(id);
//!             self.dirty();
//!             return;
//!         }
//!         Err(e) => { tracing::warn!(error = %e, "OSC 1337 File= rejected"); return; }
//!     },
//!     FileOutcome::Image(_) => return,        // inline=0 ⇒ a download, not a paint
//!     FileOutcome::Rejected(e) => { tracing::warn!(error = %e, "…"); return; }
//!     FileOutcome::NotFile => {}              // fall through to the marker family
//! }
//! ```
//!
//! That is the identical `store_rgba_image` → `place_decoded_image_at_cursor`
//! pair `decode_and_place_sixel` already uses, so inline images inherit
//! the Kitty/sixel placement, re-anchoring and GC for free. Nothing new
//! is needed on the render side.

/// Typed OSC 1337 parameter — output of [`parse_osc_1337`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc1337Param {
    /// `SetMark` — user requests a mark at the current cursor row.
    SetMark,
    /// `RequestAttention=<0|1>` — flip the attention request flag.
    /// `true` means "get the user's attention" (bounce dock);
    /// `false` cancels a pending request.
    RequestAttention(bool),
    /// Unknown key — carries the raw text for tracing. The
    /// dispatcher logs and ignores these.
    Unknown(String),
}

/// Parse the argument portion of an OSC 1337 sequence. The caller
/// has already stripped the leading `1337;` and split into param
/// segments by the OSC-level parser; this function takes the first
/// (and usually only) parameter byte slice.
///
/// Returns [`Osc1337Param::Unknown`] for unparseable UTF-8 — we
/// don't error, so a weird sequence can't corrupt the terminal
/// state.
#[must_use]
pub fn parse_osc_1337(arg: &[u8]) -> Osc1337Param {
    let Ok(s) = std::str::from_utf8(arg) else {
        return Osc1337Param::Unknown(String::from_utf8_lossy(arg).into_owned());
    };
    if s == "SetMark" {
        return Osc1337Param::SetMark;
    }
    if let Some(value) = s.strip_prefix("RequestAttention=") {
        return Osc1337Param::RequestAttention(iterm2_truthy(value));
    }
    Osc1337Param::Unknown(s.to_string())
}

/// iTerm2's boolean vocabulary, in ONE place: `0` / `false` / `no`
/// (any case) are off, anything else is on. Both `RequestAttention`
/// and the `File=` flags (`inline`, `preserveAspectRatio`) read it,
/// so "what counts as off" is a single edit, not two drifting lists.
fn iterm2_truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no"
    )
}

// ---------------------------------------------------------------------------
// `File=` — iTerm2 inline images
// ---------------------------------------------------------------------------

/// Hard bound on a `File=` base64 payload, in ENCODED bytes. Same
/// number and same posture as `terminal.rs`'s `SIXEL_DCS_MAX`: past
/// the bound the whole sequence is rejected with a typed reason, and
/// nothing is decoded — never a partial image. See the module doc for
/// the one axis this bound does NOT cover (vte's unbounded OSC
/// accumulation).
pub const FILE_PAYLOAD_MAX: usize = 8 * 1024 * 1024;

/// The `File=` key that opens an inline-image sequence.
const FILE_PREFIX: &[u8] = b"File=";

/// A `width=` / `height=` value. iTerm2 spells four units; each is a
/// distinct typed variant so the renderer never has to re-parse a
/// string, and an unrecognised spelling degrades to
/// [`ImageDim::Auto`] rather than to a wrong number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageDim {
    /// Absent or unparseable — the renderer derives the extent from
    /// the decoded texture.
    #[default]
    Auto,
    /// A bare integer: N character cells.
    Cells(u32),
    /// `Npx`: N pixels.
    Pixels(u32),
    /// `N%`: N percent of the session's width / height.
    Percent(u32),
}

impl ImageDim {
    /// Parse one `width=` / `height=` value. Case-insensitive on the
    /// `px` suffix and on `auto`; never errors.
    fn parse(raw: &str) -> Self {
        let lower = raw.trim().to_ascii_lowercase();
        if lower.is_empty() || lower == "auto" {
            return Self::Auto;
        }
        if let Some(n) = lower.strip_suffix("px") {
            return n.parse::<u32>().map_or(Self::Auto, Self::Pixels);
        }
        if let Some(n) = lower.strip_suffix('%') {
            return n.parse::<u32>().map_or(Self::Auto, Self::Percent);
        }
        lower.parse::<u32>().map_or(Self::Auto, Self::Cells)
    }
}

/// One parsed `File=` inline image: the typed argument list plus the
/// already-base64-decoded payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFile {
    /// `name=` — base64 in the wire form; decoded here when it IS
    /// base64-of-UTF-8, kept verbatim otherwise (some emitters send a
    /// plain filename).
    pub name: Option<String>,
    /// `size=` — the byte count the sender claims the payload has.
    /// Advisory: the payload is authoritative, this is a sanity hint.
    pub declared_size: Option<u64>,
    /// `width=`.
    pub width: ImageDim,
    /// `height=`.
    pub height: ImageDim,
    /// `preserveAspectRatio=` — iTerm2 defaults it ON.
    pub preserve_aspect_ratio: bool,
    /// `inline=` — iTerm2 defaults it OFF, and OFF means "this is a
    /// file transfer, do not paint it". `imgcat` sends `inline=1`.
    pub inline: bool,
    /// Decoded payload bytes (an encoded image, typically PNG).
    pub payload: Vec<u8>,
}

impl Default for InlineFile {
    fn default() -> Self {
        Self {
            name: None,
            declared_size: None,
            width: ImageDim::Auto,
            height: ImageDim::Auto,
            // iTerm2's documented defaults — NOT `bool::default()`,
            // which would silently flip preserveAspectRatio off.
            preserve_aspect_ratio: true,
            inline: false,
            payload: Vec::new(),
        }
    }
}

/// Decoded pixels ready for `terminal.rs`'s `store_rgba_image` — the
/// same `(rgba, width, height)` triple the Kitty and sixel producers
/// hand it, so inline images are a third producer on ONE upload path,
/// never a parallel one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    /// Row-major RGBA8, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
}

/// Why a `File=` sequence could not be honoured. Every failure mode
/// is a named variant so the dispatcher traces a reason instead of a
/// silent drop — and so a future variant is a compile-time fan-out,
/// not a new `_ =>` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// No `:` separating the argument list from the payload.
    MissingPayload,
    /// The `:` was there but nothing followed it.
    EmptyPayload,
    /// Encoded payload longer than [`FILE_PAYLOAD_MAX`]. Rejected
    /// BEFORE the decode allocation.
    PayloadTooLarge {
        /// Length of the base64 payload as received.
        encoded_len: usize,
        /// The bound it passed.
        max: usize,
    },
    /// `size=` alone already claims more than [`FILE_PAYLOAD_MAX`].
    /// The cheapest possible reject — no scan, no decode.
    DeclaredSizeTooLarge {
        /// The claimed decoded size.
        declared: u64,
        /// The bound it passed.
        max: usize,
    },
    /// The payload is not valid base64 (padded or unpadded).
    InvalidBase64,
    /// The payload decoded, but is not an image mado can decode. Only
    /// the `png` codec is compiled into the `image` dependency, so a
    /// JPEG/GIF `imgcat` lands here rather than on screen.
    UndecodableImage,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPayload => f.write_str("no ':' separating arguments from payload"),
            Self::EmptyPayload => f.write_str("empty payload"),
            Self::PayloadTooLarge { encoded_len, max } => {
                write!(f, "base64 payload {encoded_len} B exceeds bound {max} B")
            }
            Self::DeclaredSizeTooLarge { declared, max } => {
                write!(f, "declared size {declared} B exceeds bound {max} B")
            }
            Self::InvalidBase64 => f.write_str("payload is not valid base64"),
            Self::UndecodableImage => f.write_str("payload is not a decodable image (PNG only)"),
        }
    }
}

/// Outcome of routing an OSC 1337 argument list through the `File=`
/// parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOutcome {
    /// Not a `File=` sequence at all — the caller falls through to
    /// [`parse_osc_1337`] for the marker / attention family.
    NotFile,
    /// A well-formed inline image.
    Image(Box<InlineFile>),
    /// A `File=` sequence that cannot be honoured, with the reason.
    Rejected(FileError),
}

/// Parse the `File=` form from the OSC parameter tail (everything
/// after the `1337` code).
///
/// `args` is `&params[1..]` from `Perform::osc_dispatch`: vte splits
/// the OSC on `;`, and a `File=` argument list contains `;`, so the
/// one logical argument arrives as several params and is rejoined
/// here. A single-param `File=…` works identically (a join of one).
///
/// Returns [`FileOutcome::NotFile`] when `args` doesn't open with
/// `File=`, so this can sit in front of [`parse_osc_1337`] without
/// changing any existing behaviour. Malformed input is always a typed
/// [`FileOutcome::Rejected`] — this function has no panic path.
#[must_use]
#[allow(dead_code)] // Typed surface for the pending terminal.rs wiring (module doc §Wiring).
pub fn parse_osc_1337_file(args: &[&[u8]]) -> FileOutcome {
    let Some(first) = args.first() else {
        return FileOutcome::NotFile;
    };
    if !first.starts_with(FILE_PREFIX) {
        return FileOutcome::NotFile;
    }
    let joined = args.join(&b';');
    let body = &joined[FILE_PREFIX.len()..];

    // The payload split. ':' is outside the base64 alphabet and
    // outside every iTerm2 key/value, so the FIRST one is the
    // boundary — no escaping to unwind.
    let Some(colon) = body.iter().position(|&b| b == b':') else {
        return FileOutcome::Rejected(FileError::MissingPayload);
    };
    let (arg_bytes, payload_b64) = (&body[..colon], &body[colon + 1..]);

    let mut file = InlineFile::default();
    // Lossy on purpose: a non-UTF-8 byte in the argument list turns
    // into one unknown key that is ignored, never a rejected image.
    for kv in String::from_utf8_lossy(arg_bytes).split(';') {
        let Some((key, value)) = kv.split_once('=') else {
            continue; // A bare token in the key list — ignore, don't reject.
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => file.name = Some(decode_name(value)),
            "size" => file.declared_size = value.parse().ok(),
            "width" => file.width = ImageDim::parse(value),
            "height" => file.height = ImageDim::parse(value),
            "preserveaspectratio" => file.preserve_aspect_ratio = iterm2_truthy(value),
            "inline" => file.inline = iterm2_truthy(value),
            // Unknown keys are ignored, exactly as an unknown OSC 1337
            // parameter is: a newer dialect must not corrupt state.
            _ => {}
        }
    }

    // Bounds, cheapest first: the declared size needs no scan at all.
    let max64 = u64::try_from(FILE_PAYLOAD_MAX).unwrap_or(u64::MAX);
    if let Some(declared) = file.declared_size {
        if declared > max64 {
            return FileOutcome::Rejected(FileError::DeclaredSizeTooLarge {
                declared,
                max: FILE_PAYLOAD_MAX,
            });
        }
    }
    if payload_b64.len() > FILE_PAYLOAD_MAX {
        return FileOutcome::Rejected(FileError::PayloadTooLarge {
            encoded_len: payload_b64.len(),
            max: FILE_PAYLOAD_MAX,
        });
    }

    let cleaned: Vec<u8> = payload_b64
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if cleaned.is_empty() {
        return FileOutcome::Rejected(FileError::EmptyPayload);
    }
    let Some(payload) = decode_base64(&cleaned) else {
        return FileOutcome::Rejected(FileError::InvalidBase64);
    };
    file.payload = payload;
    FileOutcome::Image(Box::new(file))
}

impl InlineFile {
    /// Decode the payload into RGBA8 pixels for the shared texture
    /// upload path. Returns [`FileError::UndecodableImage`] for
    /// anything the compiled codecs can't read — never a panic, so a
    /// hostile payload costs a trace line and nothing else.
    #[allow(dead_code)] // Typed surface for the pending terminal.rs wiring.
    pub fn decode_rgba(&self) -> Result<DecodedImage, FileError> {
        let img =
            image::load_from_memory(&self.payload).map_err(|_| FileError::UndecodableImage)?;
        let rgba = img.to_rgba8();
        let (width, height) = (rgba.width(), rgba.height());
        if width == 0 || height == 0 {
            return Err(FileError::UndecodableImage);
        }
        Ok(DecodedImage {
            pixels: rgba.into_raw(),
            width,
            height,
        })
    }
}

/// Base64 with or without padding — `imgcat` pads, some emitters
/// don't. One helper so both call sites agree on what "is base64"
/// means.
fn decode_base64(cleaned: &[u8]) -> Option<Vec<u8>> {
    data_encoding::BASE64
        .decode(cleaned)
        .or_else(|_| data_encoding::BASE64_NOPAD.decode(cleaned))
        .ok()
}

/// `name=` is documented as base64. Decode it when it round-trips to
/// UTF-8; otherwise keep the literal text, so an emitter that sends a
/// plain filename still gets a readable name instead of mojibake.
fn decode_name(value: &str) -> String {
    decode_base64(value.as_bytes())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| value.to_owned())
}

/// One user-emitted mark from `OSC 1337 SetMark`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserMark {
    /// Grid-internal row index — same coord space OSC 133
    /// [`PromptHistory`](crate::prompt_mark::PromptHistory) uses.
    pub grid_row: usize,
}

/// Bounded FIFO of user marks. Shape mirrors `PromptHistory` so
/// the scrollback-eviction bookkeeping stays consistent.
#[derive(Debug, Clone)]
pub struct UserMarkHistory {
    marks: std::collections::VecDeque<UserMark>,
    cap: usize,
    /// M2 rewrap bridge — see
    /// [`PromptHistory`](crate::prompt_mark::PromptHistory) for the
    /// full contract; this mirrors it field-for-field. Transient:
    /// captured by [`Self::refresh_anchors`] before a rewrap resize,
    /// consumed by [`Self::reanchor`] after.
    anchors: Vec<Option<crate::terminal::MarkAnchor>>,
}

impl UserMarkHistory {
    /// Fresh history with a hard cap on the mark count.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            marks: std::collections::VecDeque::with_capacity(cap.min(1024)),
            cap: cap.max(1),
            anchors: Vec::new(),
        }
    }

    /// Capture each mark's logical anchor from the CURRENT physical
    /// layout. Same contract as
    /// [`PromptHistory::refresh_anchors`](crate::prompt_mark::PromptHistory::refresh_anchors).
    pub(crate) fn refresh_anchors(
        &mut self,
        anchor_at: impl Fn(usize) -> Option<crate::terminal::MarkAnchor>,
    ) {
        self.anchors = self.marks.iter().map(|m| anchor_at(m.grid_row)).collect();
    }

    /// Resolve captured anchors against the POST-rewrap layout,
    /// garbage-collecting marks whose logical line vanished. Same
    /// contract as
    /// [`PromptHistory::reanchor`](crate::prompt_mark::PromptHistory::reanchor).
    pub(crate) fn reanchor(
        &mut self,
        resolve: impl Fn(crate::terminal::MarkAnchor) -> Option<usize>,
    ) {
        let anchors = std::mem::take(&mut self.anchors);
        let old = std::mem::take(&mut self.marks);
        for (i, mut mark) in old.into_iter().enumerate() {
            match anchors.get(i).copied().flatten().and_then(&resolve) {
                Some(row) => {
                    mark.grid_row = row;
                    self.marks.push_back(mark);
                }
                None => {}
            }
        }
    }

    /// Record a new mark at `grid_row`. Back-to-back duplicates are
    /// dropped so a script that echoes `SetMark` twice doesn't
    /// double-pin the same row.
    pub fn record(&mut self, grid_row: usize) {
        if let Some(last) = self.marks.back() {
            if last.grid_row == grid_row {
                return;
            }
        }
        self.marks.push_back(UserMark { grid_row });
        while self.marks.len() > self.cap {
            self.marks.pop_front();
        }
    }

    /// Shift every mark when the grid evicts `n` rows from the front.
    /// Marks that would go negative are dropped. Same contract
    /// [`PromptHistory::shift_on_evict`](crate::prompt_mark::PromptHistory::shift_on_evict)
    /// already uses.
    pub fn shift_on_evict(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        while let Some(first) = self.marks.front() {
            if first.grid_row < n {
                self.marks.pop_front();
            } else {
                break;
            }
        }
        for m in self.marks.iter_mut() {
            m.grid_row -= n;
        }
    }

    /// Count of tracked marks.
    #[must_use]
    #[allow(dead_code)] // Surface for pending MCP prompt-history-style tooling.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// True when empty.
    #[must_use]
    #[allow(dead_code)] // Idiomatic alongside `len()`; consumed by tests.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// Drop every mark — used by terminal reset (RIS).
    #[allow(dead_code)] // Pairs with PromptHistory::clear for reset paths.
    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// Iterator over marks in insertion order.
    pub fn iter(&self) -> std::collections::vec_deque::Iter<'_, UserMark> {
        self.marks.iter()
    }
}

impl Default for UserMarkHistory {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_recognizes_set_mark() {
        assert_eq!(parse_osc_1337(b"SetMark"), Osc1337Param::SetMark);
    }

    #[test]
    fn parser_decodes_request_attention_boolean() {
        // Ghostty / iTerm2 both accept `0`/`false`/`no` as off; any
        // other value is treated as "request attention". Pin the
        // off-vocab so adding a new "falsy" literal is a conscious
        // edit.
        assert_eq!(
            parse_osc_1337(b"RequestAttention=0"),
            Osc1337Param::RequestAttention(false),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=false"),
            Osc1337Param::RequestAttention(false),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=no"),
            Osc1337Param::RequestAttention(false),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=1"),
            Osc1337Param::RequestAttention(true),
        );
        assert_eq!(
            parse_osc_1337(b"RequestAttention=true"),
            Osc1337Param::RequestAttention(true),
        );
    }

    #[test]
    fn parser_returns_unknown_for_unrecognized_keys() {
        match parse_osc_1337(b"CopyToClipboard") {
            Osc1337Param::Unknown(s) => assert_eq!(s, "CopyToClipboard"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        // key=value shape with unknown key.
        match parse_osc_1337(b"File=abc") {
            Osc1337Param::Unknown(s) => assert_eq!(s, "File=abc"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parser_handles_invalid_utf8_gracefully() {
        // Malformed UTF-8 — must not panic or error. Falls through
        // to Unknown with a lossy rendering.
        let bad = &[0xff, 0xfe, b'a'];
        match parse_osc_1337(bad) {
            Osc1337Param::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn mark_history_dedupes_identical_back_to_back_marks() {
        let mut h = UserMarkHistory::default();
        h.record(5);
        h.record(5);
        h.record(5);
        assert_eq!(h.len(), 1);
        // Different row → new entry.
        h.record(6);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn mark_history_enforces_capacity() {
        let mut h = UserMarkHistory::with_capacity(3);
        for row in 0..5 {
            h.record(row);
        }
        assert_eq!(h.len(), 3);
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        assert_eq!(rows, vec![2, 3, 4]);
    }

    #[test]
    fn mark_history_shift_on_evict_drops_underflow() {
        let mut h = UserMarkHistory::default();
        h.record(2);
        h.record(5);
        h.record(10);
        h.shift_on_evict(3);
        let rows: Vec<_> = h.iter().map(|m| m.grid_row).collect();
        // Row 2 dropped (would go negative); 5-3=2, 10-3=7.
        assert_eq!(rows, vec![2, 7]);

        // Huge eviction wipes everything.
        h.shift_on_evict(1000);
        assert!(h.is_empty());
    }

    #[test]
    fn mark_history_shift_on_evict_zero_is_noop() {
        let mut h = UserMarkHistory::default();
        h.record(5);
        h.shift_on_evict(0);
        assert_eq!(h.len(), 1);
    }

    // -----------------------------------------------------------------
    // `File=` — iTerm2 inline images (imgcat)
    // -----------------------------------------------------------------

    /// Unwrap a `FileOutcome` to its image, with the rejection reason
    /// in the panic message — a failing parse test should say WHY.
    fn image(outcome: FileOutcome) -> Box<InlineFile> {
        match outcome {
            FileOutcome::Image(f) => f,
            other => panic!("expected an image, got {other:?}"),
        }
    }

    fn rejection(outcome: FileOutcome) -> FileError {
        match outcome {
            FileOutcome::Rejected(e) => e,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn file_form_parses_typed_args_and_decodes_the_payload() {
        // The shape `imgcat` emits, with every documented key set.
        // `Zm9vLnBuZw==` is base64 "foo.png"; `aGVsbG8=` is "hello".
        let arg = b"File=name=Zm9vLnBuZw==;size=5;width=10;height=20px;\
                    preserveAspectRatio=0;inline=1:aGVsbG8=";
        let f = image(parse_osc_1337_file(&[arg.as_slice()]));
        assert_eq!(f.name.as_deref(), Some("foo.png"));
        assert_eq!(f.declared_size, Some(5));
        assert_eq!(f.width, ImageDim::Cells(10));
        assert_eq!(f.height, ImageDim::Pixels(20));
        assert!(!f.preserve_aspect_ratio);
        assert!(f.inline);
        assert_eq!(f.payload, b"hello");
    }

    #[test]
    fn file_args_split_by_vte_are_rejoined() {
        // vte splits the OSC on ';', so the ONE logical argument
        // arrives as N params — the shape a real `imgcat` produces.
        // Parsing must not depend on which side of the split a key
        // landed on.
        let split: [&[u8]; 4] = [
            b"File=name=Zm9vLnBuZw==",
            b"size=5",
            b"width=50%",
            b"inline=1:aGVsbG8=",
        ];
        let f = image(parse_osc_1337_file(&split));
        assert_eq!(f.name.as_deref(), Some("foo.png"));
        assert_eq!(f.width, ImageDim::Percent(50));
        assert!(f.inline);
        assert_eq!(f.payload, b"hello");
    }

    #[test]
    fn file_dimensions_carry_their_units() {
        let cases: &[(&str, ImageDim)] = &[
            ("7", ImageDim::Cells(7)),
            ("7px", ImageDim::Pixels(7)),
            ("7PX", ImageDim::Pixels(7)),
            ("7%", ImageDim::Percent(7)),
            ("auto", ImageDim::Auto),
            ("AUTO", ImageDim::Auto),
            // Unparseable degrades to Auto — never to a wrong number.
            ("banana", ImageDim::Auto),
            ("-3", ImageDim::Auto),
            ("", ImageDim::Auto),
        ];
        for (raw, want) in cases {
            assert_eq!(ImageDim::parse(raw), *want, "width={raw}");
        }
    }

    #[test]
    fn file_defaults_match_iterm2_not_bool_default() {
        // Absent `preserveAspectRatio` is ON and absent `inline` is
        // OFF. A `#[derive(Default)]` would have flipped the first.
        let f = image(parse_osc_1337_file(&[b"File=size=5:aGVsbG8=".as_slice()]));
        assert!(f.preserve_aspect_ratio);
        assert!(!f.inline);
        assert_eq!(f.width, ImageDim::Auto);
        assert_eq!(f.height, ImageDim::Auto);
        assert_eq!(f.name, None);
    }

    #[test]
    fn file_boolean_vocabulary_matches_the_attention_arm() {
        for off in ["0", "false", "no", "NO", "False"] {
            let arg = alloc_arg(&["inline=", off, ":aGVsbG8="]);
            assert!(!image(parse_osc_1337_file(&[&arg])).inline, "off: {off}");
        }
        for on in ["1", "true", "yes"] {
            let arg = alloc_arg(&["inline=", on, ":aGVsbG8="]);
            assert!(image(parse_osc_1337_file(&[&arg])).inline, "on: {on}");
        }
    }

    /// Build a `File=…` argument from pieces without a `format!` —
    /// this crate's TYPED-EMISSION rule holds in tests too.
    fn alloc_arg(parts: &[&str]) -> Vec<u8> {
        let mut out = FILE_PREFIX.to_vec();
        for p in parts {
            out.extend_from_slice(p.as_bytes());
        }
        out
    }

    #[test]
    fn a_non_file_argument_is_not_a_file() {
        assert_eq!(
            parse_osc_1337_file(&[b"SetMark".as_slice()]),
            FileOutcome::NotFile
        );
        assert_eq!(
            parse_osc_1337_file(&[b"RequestAttention=1".as_slice()]),
            FileOutcome::NotFile,
        );
        assert_eq!(parse_osc_1337_file(&[]), FileOutcome::NotFile);
        // Prefix-adjacent but not the key.
        assert_eq!(
            parse_osc_1337_file(&[b"Files=1:AAAA".as_slice()]),
            FileOutcome::NotFile
        );
    }

    #[test]
    fn file_without_the_payload_separator_is_rejected() {
        assert_eq!(
            rejection(parse_osc_1337_file(&[b"File=inline=1".as_slice()])),
            FileError::MissingPayload,
        );
    }

    #[test]
    fn file_with_an_empty_payload_is_rejected() {
        assert_eq!(
            rejection(parse_osc_1337_file(&[b"File=inline=1:".as_slice()])),
            FileError::EmptyPayload,
        );
        // Whitespace-only is empty too, once cleaned.
        assert_eq!(
            rejection(parse_osc_1337_file(&[b"File=inline=1: \r\n".as_slice()])),
            FileError::EmptyPayload,
        );
    }

    #[test]
    fn file_with_a_malformed_payload_is_rejected_not_panicked() {
        // '!' and '@' are outside the base64 alphabet.
        assert_eq!(
            rejection(parse_osc_1337_file(&[b"File=inline=1:!!!@@@".as_slice()])),
            FileError::InvalidBase64,
        );
        // Right alphabet, wrong length (5 chars can't be a base64 group).
        assert_eq!(
            rejection(parse_osc_1337_file(&[b"File=inline=1:aGVsb".as_slice()])),
            FileError::InvalidBase64,
        );
        // Non-UTF-8 in the ARGUMENT list is survivable — it becomes an
        // unknown key, and the image still parses.
        let mut arg = b"File=name=".to_vec();
        arg.extend_from_slice(&[0xff, 0xfe]);
        arg.extend_from_slice(b";inline=1:aGVsbG8=");
        assert_eq!(image(parse_osc_1337_file(&[&arg])).payload, b"hello");
    }

    #[test]
    fn file_payload_is_bounded_the_way_the_sixel_path_bounds_its_own() {
        // terminal.rs rejects a sixel DCS past SIXEL_DCS_MAX (8 MiB)
        // WHOLE — never a partial decode. Same number, same posture,
        // and the check lands BEFORE the decode allocation.
        let mut arg = FILE_PREFIX.to_vec();
        arg.extend_from_slice(b"inline=1:");
        arg.extend(std::iter::repeat_n(b'A', FILE_PAYLOAD_MAX + 1));
        assert_eq!(
            rejection(parse_osc_1337_file(&[&arg])),
            FileError::PayloadTooLarge {
                encoded_len: FILE_PAYLOAD_MAX + 1,
                max: FILE_PAYLOAD_MAX,
            },
        );
    }

    #[test]
    fn file_declared_size_past_the_bound_is_rejected_without_scanning() {
        // The cheapest reject: `size=` alone disqualifies the sequence,
        // so a lying header costs no decode at all.
        let arg = alloc_arg(&["size=99999999999;inline=1:aGVsbG8="]);
        assert_eq!(
            rejection(parse_osc_1337_file(&[&arg])),
            FileError::DeclaredSizeTooLarge {
                declared: 99_999_999_999,
                max: FILE_PAYLOAD_MAX,
            },
        );
    }

    #[test]
    fn unpadded_base64_payloads_are_accepted() {
        // `aGVsbG8` is "hello" with the '=' stripped — some emitters
        // do that, and rejecting it would look like a mado bug.
        let f = image(parse_osc_1337_file(&[b"File=inline=1:aGVsbG8".as_slice()]));
        assert_eq!(f.payload, b"hello");
    }

    #[test]
    fn a_plain_unencoded_name_survives_verbatim() {
        // `name=` is documented as base64, but a plain filename is not
        // base64 ('.' is outside the alphabet) — keep it readable
        // rather than dropping it.
        let f = image(parse_osc_1337_file(&[
            b"File=name=cat.png;inline=1:aGVsbG8=".as_slice(),
        ]));
        assert_eq!(f.name.as_deref(), Some("cat.png"));
    }

    #[test]
    fn a_well_formed_png_payload_decodes_to_rgba() {
        // The end-to-end shape the wiring consumes: base64 PNG on the
        // wire → InlineFile::decode_rgba → the (pixels, w, h) triple
        // `store_rgba_image` already takes from Kitty and sixel.
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255u8; 16], 2, 2, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        let mut arg = FILE_PREFIX.to_vec();
        arg.extend_from_slice(b"inline=1:");
        arg.extend_from_slice(data_encoding::BASE64.encode(&png).as_bytes());

        let f = image(parse_osc_1337_file(&[&arg]));
        let decoded = f.decode_rgba().expect("PNG decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.pixels.len(), 2 * 2 * 4);
        assert_eq!(decoded.pixels, vec![255u8; 16]);
    }

    #[test]
    fn a_payload_that_is_not_an_image_is_a_typed_error_not_a_panic() {
        // Valid base64, not an image — the failure surfaces as a typed
        // error the dispatcher can trace.
        let f = image(parse_osc_1337_file(&[b"File=inline=1:aGVsbG8=".as_slice()]));
        assert_eq!(f.decode_rgba(), Err(FileError::UndecodableImage));
    }

    #[test]
    fn file_errors_render_through_display() {
        // The dispatcher logs these with `%e`; pin the text so a
        // reworded variant is a conscious edit.
        assert_eq!(
            FileError::MissingPayload.to_string(),
            "no ':' separating arguments from payload",
        );
        assert_eq!(
            FileError::PayloadTooLarge {
                encoded_len: 9,
                max: 8,
            }
            .to_string(),
            "base64 payload 9 B exceeds bound 8 B",
        );
    }
}
