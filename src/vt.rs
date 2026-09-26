//! Typed VT control-sequence responses — the ONE typed surface every
//! escape sequence mado writes back to the PTY is built through.
//!
//! Terminal query replies (DECRQM, kitty-keyboard flags, kitty-graphics
//! acknowledgements, DECRQSS) are control sequences with a precise
//! grammar: a CSI / DCS / APC envelope wrapping typed parameters. Hand
//! composing them as `format!("\x1b[?{flags}u")` strings scatters that
//! grammar across the VT state machine and is the kind of free-form
//! string emission the ★★ TYPED EMISSION rule forbids. These builders
//! put the envelope grammar in one tested place; the call site declares
//! the *parameters*, never the escape bytes.
//!
//! Every builder is byte-exact (the migration off `format!` is a
//! no-behaviour-change refactor, pinned by the tests below).

use std::io::Write as _;

/// A CSI reply: `ESC [` · optional `?` private marker · `;`-joined
/// numeric params · intermediate bytes · one final byte.
///
/// Examples (the responses mado actually emits):
/// * kitty-keyboard flags `ESC [ ? <flags> u` — `csi(true, &[flags], "", b'u')`
/// * DECRQM `ESC [ <mode> ; <state> $ y` — `csi(false, &[mode, state], "$", b'y')`
/// * DECRQM (private) `ESC [ ? <mode> ; <state> $ y` — `csi(true, &[mode, state], "$", b'y')`
#[must_use]
pub fn csi(private: bool, params: &[u32], intermediates: &str, final_byte: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + params.len() * 4 + intermediates.len());
    out.extend_from_slice(b"\x1b[");
    if private {
        out.push(b'?');
    }
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        let _ = write!(out, "{p}");
    }
    out.extend_from_slice(intermediates.as_bytes());
    out.push(final_byte);
    out
}

/// A DCS reply: `ESC P` · body · `ESC \` (ST). Used for DECRQSS
/// (`ESC P 1 $ r … r ESC \`). The body is the already-formatted
/// payload between the envelope (the envelope is what this types).
#[must_use]
pub fn dcs(body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(b"\x1bP");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// An APC reply: `ESC _` · body · `ESC \` (ST). Used for the Kitty
/// graphics protocol acknowledgements (`ESC _ G i=<id>;OK ESC \`).
#[must_use]
pub fn apc(body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(b"\x1b_");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

// The OSC envelope and the notification / attention / prompt-mark
// builders live in egaku::vt (lifted from here, 2026-09-26) so every fleet
// program that writes to a terminal emits the same bytes mado parses. Re-
// exported under the same names, so `crate::vt::osc(…)` et al. are unchanged;
// the byte-exact tests below now guard the re-export.
pub use egaku::vt::{
    Osc99Part, Osc133Mark, OscTerminator, osc, osc9_notify, osc99_notify, osc133,
    osc777_notify, osc1337_request_attention,
};

/// An OSC color-reply body: `rgb:RRRR/GGGG/BBBB`, each channel byte doubled
/// per xterm so 16-bit-precision parsers read `RR` as `RRRR`. This is a typed
/// *value* payload (built with `write!`, not `format!` of the escape envelope)
/// shared by the OSC 4 / 10 / 11 / 12 color-query replies below.
fn rgb_body(r: u8, g: u8, b: u8) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(19);
    let _ = write!(s, "rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}");
    s
}

/// OSC 10 / 11 / 12 color-query reply: `ESC ] <code> ; rgb:RRRR/GGGG/BBBB ST`.
/// The outbound half of the color-query protocol `terminal.rs` parses — the
/// envelope grammar lives in [`osc`], the channel doubling in [`rgb_body`].
/// The peer of the OSC 9/777/99/1337/133 emitters above.
#[must_use]
pub fn osc_color_reply(code: u16, r: u8, g: u8, b: u8) -> Vec<u8> {
    osc(code, &[&rgb_body(r, g, b)], OscTerminator::St)
}

/// OSC 4 palette-query reply: `ESC ] 4 ; <idx> ; rgb:RRRR/GGGG/BBBB ST` — the
/// palette index echoed before the color body, per xterm.
#[must_use]
pub fn osc4_color_reply(idx: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut idx_s = String::with_capacity(3);
    let _ = write!(idx_s, "{idx}");
    osc(4, &[&idx_s, &rgb_body(r, g, b)], OscTerminator::St)
}

/// The typed CSI-command AST — a `vte::Params` + final byte parsed into
/// intent (parse-don't-validate), so the VT state machine's giant
/// `match action` becomes data: [`parse_csi_action`] decides *which*
/// command (pure, no terminal state), and the interpreter
/// (`Terminal::apply_csi`) does the state mutation. This is the
/// TYPED-SPEC + INTERPRETER TRIPLET applied to the VT dispatch — the
/// same shape as engawa's IR and sui-spec's domains.
///
/// Only the non-intermediate standard commands live here; the
/// intermediate-prefixed sequences (`?`/`>`/`<`/`!`/`$`/space) and SGR
/// (`m`, param-heavy + truecolor) keep their own paths. Counts that
/// default to 1 are pre-clamped (`max(1)`) exactly as the legacy
/// `first_param` helper did; raw modes (`EraseDisplay`/`EraseLine`/…)
/// default to 0 with no clamp, also as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsiCommand {
    CursorUp(usize),
    CursorDown(usize),
    CursorForward(usize),
    CursorBack(usize),
    CursorNextLine(usize),
    CursorPrevLine(usize),
    CursorColumn(usize),
    CursorPosition {
        row: usize,
        col: usize,
    },
    EraseDisplay(u16),
    EraseLine(u16),
    InsertLines(usize),
    DeleteLines(usize),
    DeleteChars(usize),
    InsertChars(usize),
    EraseChars(usize),
    RepeatChar(usize),
    ScrollUp(usize),
    ScrollDown(usize),
    VerticalPosition(usize),
    DeviceStatusReport(u16),
    /// `bottom = None` means "default to the screen's bottom row" (the
    /// legacy `map_or(self.rows, …)` — a state-dependent default the
    /// interpreter fills in).
    SetScrollRegion {
        top: usize,
        bottom: Option<usize>,
    },
    PrimaryDeviceAttributes,
    CursorBackwardTab(usize),
    TabClear(u16),
    SetAnsiMode(Vec<u16>),
    ResetAnsiMode(Vec<u16>),
    SaveCursor,
    RestoreCursor,
    WindowOp(u16),
    /// An action the AST doesn't model (the legacy `_ =>` trace arm).
    Unknown(char),
}

/// Parse a non-intermediate standard CSI (`params` + final `action`)
/// into a typed [`CsiCommand`]. Pure — reads only the params, never
/// terminal state. The caller handles SGR (`'m'`) and the
/// intermediate-prefixed sequences before reaching here.
#[must_use]
pub fn parse_csi_action(params: &vte::Params, action: char) -> CsiCommand {
    // Count param defaulting to 1, clamped to ≥1 (legacy `first_param`).
    let n1 = || -> usize { params.iter().next().map_or(1, |p| (p[0] as usize).max(1)) };
    // Raw first param defaulting to 0 (legacy `map_or(0, |p| p[0])`).
    let raw0 = || -> u16 { params.iter().next().map_or(0, |p| p[0]) };
    match action {
        'A' => CsiCommand::CursorUp(n1()),
        'B' => CsiCommand::CursorDown(n1()),
        'C' => CsiCommand::CursorForward(n1()),
        'D' => CsiCommand::CursorBack(n1()),
        'E' => CsiCommand::CursorNextLine(n1()),
        'F' => CsiCommand::CursorPrevLine(n1()),
        'G' => CsiCommand::CursorColumn(n1()),
        'H' | 'f' => {
            let mut it = params.iter();
            let row = it.next().map_or(1, |p| (p[0] as usize).max(1));
            let col = it.next().map_or(1, |p| (p[0] as usize).max(1));
            CsiCommand::CursorPosition { row, col }
        }
        'J' => CsiCommand::EraseDisplay(raw0()),
        'K' => CsiCommand::EraseLine(raw0()),
        'L' => CsiCommand::InsertLines(n1()),
        'M' => CsiCommand::DeleteLines(n1()),
        'P' => CsiCommand::DeleteChars(n1()),
        '@' => CsiCommand::InsertChars(n1()),
        'X' => CsiCommand::EraseChars(n1()),
        'b' => CsiCommand::RepeatChar(n1()),
        'S' => CsiCommand::ScrollUp(n1()),
        'T' => CsiCommand::ScrollDown(n1()),
        'd' => CsiCommand::VerticalPosition(n1()),
        'n' => CsiCommand::DeviceStatusReport(raw0()),
        'r' => {
            let mut it = params.iter();
            let top = it.next().map_or(1, |p| (p[0] as usize).max(1));
            let bottom = it.next().map(|p| (p[0] as usize).max(1));
            CsiCommand::SetScrollRegion { top, bottom }
        }
        'c' => CsiCommand::PrimaryDeviceAttributes,
        'Z' => CsiCommand::CursorBackwardTab(n1()),
        'g' => CsiCommand::TabClear(raw0()),
        'h' => CsiCommand::SetAnsiMode(params.iter().map(|p| p[0]).collect()),
        'l' => CsiCommand::ResetAnsiMode(params.iter().map(|p| p[0]).collect()),
        's' => CsiCommand::SaveCursor,
        'u' => CsiCommand::RestoreCursor,
        't' => CsiCommand::WindowOp(raw0()),
        other => CsiCommand::Unknown(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_matches_the_legacy_format_strings_byte_for_byte() {
        // kitty-keyboard flags: ESC [ ? <flags> u
        let flags = 5u32;
        assert_eq!(
            csi(true, &[flags], "", b'u'),
            format!("\x1b[?{flags}u").into_bytes()
        );
        // DECRQM: ESC [ <mode> ; <state> $ y
        let (mode, state) = (2069u32, 1u32);
        assert_eq!(
            csi(false, &[mode, state], "$", b'y'),
            format!("\x1b[{mode};{state}$y").into_bytes()
        );
        // DECRQM private: ESC [ ? <mode> ; <state> $ y
        assert_eq!(
            csi(true, &[mode, state], "$", b'y'),
            format!("\x1b[?{mode};{state}$y").into_bytes()
        );
    }

    #[test]
    fn dcs_and_apc_match_the_legacy_format_strings() {
        let (top, bottom) = (1u32, 24u32);
        assert_eq!(
            dcs(&format!("1$r{top};{bottom}r")),
            format!("\x1bP1$r{top};{bottom}r\x1b\\").into_bytes()
        );
        let id = 42u32;
        assert_eq!(
            apc(&format!("Gi={id};OK")),
            format!("\x1b_Gi={id};OK\x1b\\").into_bytes()
        );
    }

    #[test]
    fn csi_with_no_params_is_just_envelope_and_final() {
        assert_eq!(csi(false, &[], "", b'c'), b"\x1b[c");
        assert_eq!(csi(true, &[], "", b'c'), b"\x1b[?c");
    }

    #[test]
    fn osc_envelope_bel_and_st() {
        assert_eq!(osc(9, &["hi"], OscTerminator::Bel), b"\x1b]9;hi\x07");
        assert_eq!(
            osc(99, &["m", "p"], OscTerminator::St),
            b"\x1b]99;m;p\x1b\\"
        );
        // No params → just introducer + code + terminator.
        assert_eq!(osc(0, &[], OscTerminator::Bel), b"\x1b]0\x07");
    }

    #[test]
    fn osc9_and_777_builders_are_byte_exact() {
        assert_eq!(osc9_notify("Build done"), b"\x1b]9;Build done\x07");
        assert_eq!(
            osc777_notify("Mado", "All tests passed"),
            b"\x1b]777;notify;Mado;All tests passed\x07"
        );
    }

    #[test]
    fn osc99_builder_encodes_metadata_and_payload() {
        assert_eq!(
            osc99_notify("t1", false, 2, Osc99Part::Title, "Title"),
            b"\x1b]99;i=t1:d=0:u=2:p=title;Title\x1b\\"
        );
        assert_eq!(
            osc99_notify("t1", true, 1, Osc99Part::Body, "Body"),
            b"\x1b]99;i=t1:d=1:u=1:p=body;Body\x1b\\"
        );
    }

    #[test]
    fn osc1337_request_attention_builder() {
        assert_eq!(
            osc1337_request_attention(true),
            b"\x1b]1337;RequestAttention=1\x07"
        );
        assert_eq!(
            osc1337_request_attention(false),
            b"\x1b]1337;RequestAttention=0\x07"
        );
    }

    #[test]
    fn osc133_marks_build() {
        // ST-terminated per FinalTerm; letter in params[1], exit in params[2].
        assert_eq!(osc133(Osc133Mark::PromptStart), b"\x1b]133;A\x1b\\");
        assert_eq!(osc133(Osc133Mark::CommandStart), b"\x1b]133;B\x1b\\");
        assert_eq!(osc133(Osc133Mark::CommandOutput), b"\x1b]133;C\x1b\\");
        assert_eq!(osc133(Osc133Mark::CommandEnd(None)), b"\x1b]133;D\x1b\\");
        assert_eq!(
            osc133(Osc133Mark::CommandEnd(Some(0))),
            b"\x1b]133;D;0\x1b\\"
        );
        assert_eq!(
            osc133(Osc133Mark::CommandEnd(Some(130))),
            b"\x1b]133;D;130\x1b\\"
        );
    }

    #[test]
    fn osc_color_reply_matches_the_legacy_format_string() {
        // Byte-for-byte the former terminal.rs `osc_rgb_query_response` output
        // (the M5 typed-emission migration is no-behaviour-change).
        for (code, r, g, b) in [
            (10u16, 0x2Eu8, 0x34u8, 0x40u8),
            (11, 0, 255, 16),
            (12, 0xD8, 0xDE, 0xE9),
        ] {
            assert_eq!(
                osc_color_reply(code, r, g, b),
                format!("\x1b]{code};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\")
                    .into_bytes()
            );
        }
        assert_eq!(
            osc_color_reply(11, 0x2E, 0x34, 0x40),
            b"\x1b]11;rgb:2e2e/3434/4040\x1b\\"
        );
    }

    #[test]
    fn osc4_color_reply_matches_the_legacy_format_string() {
        for (idx, r, g, b) in [
            (5usize, 0xBFu8, 0x61u8, 0x6Au8),
            (0, 0, 0, 0),
            (255, 255, 255, 255),
        ] {
            assert_eq!(
                osc4_color_reply(idx, r, g, b),
                format!("\x1b]4;{idx};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\")
                    .into_bytes()
            );
        }
        assert_eq!(
            osc4_color_reply(5, 0xBF, 0x61, 0x6A),
            b"\x1b]4;5;rgb:bfbf/6161/6a6a\x1b\\"
        );
    }
}
