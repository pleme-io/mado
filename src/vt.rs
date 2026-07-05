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

/// How an OSC string is terminated: `BEL` (0x07) or `ST` (`ESC \`).
/// Both are valid per ECMA-48; individual protocols pick one (iTerm2 OSC 9
/// and OSC 1337 use BEL; the kitty OSC 99 protocol uses ST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscTerminator {
    /// `BEL` — 0x07.
    Bel,
    /// `ST` — `ESC \`.
    St,
}

/// An OSC sequence: `ESC ]` · numeric code · `;`-joined string params ·
/// terminator. The envelope grammar (introducer, `;` separators,
/// terminator) lives here once; the call site declares the numeric code
/// and the typed string parameters, never the escape bytes. This is the
/// OSC peer of [`csi`] / [`dcs`] / [`apc`] — the outbound half of the
/// terminal-notification protocols mado also parses (OSC 9 / 777 / 99 /
/// 1337, see `terminal.rs::osc_dispatch`).
#[must_use]
pub fn osc(code: u16, params: &[&str], terminator: OscTerminator) -> Vec<u8> {
    let total: usize = params.iter().map(|p| p.len() + 1).sum();
    let mut out = Vec::with_capacity(6 + total);
    out.extend_from_slice(b"\x1b]");
    let _ = write!(out, "{code}");
    for p in params {
        out.push(b';');
        out.extend_from_slice(p.as_bytes());
    }
    match terminator {
        OscTerminator::Bel => out.push(0x07),
        OscTerminator::St => out.extend_from_slice(b"\x1b\\"),
    }
    out
}

/// OSC 9 (iTerm2) simple notification: `ESC ] 9 ; <body> BEL`.
#[must_use]
pub fn osc9_notify(body: &str) -> Vec<u8> {
    osc(9, &[body], OscTerminator::Bel)
}

/// OSC 777 (urxvt/foot) notification:
/// `ESC ] 777 ; notify ; <title> ; <body> BEL`.
#[must_use]
pub fn osc777_notify(title: &str, body: &str) -> Vec<u8> {
    osc(777, &["notify", title, body], OscTerminator::Bel)
}

/// Which field an OSC 99 payload chunk carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc99Part {
    /// The notification title.
    Title,
    /// The notification body.
    Body,
}

impl Osc99Part {
    fn as_str(self) -> &'static str {
        match self {
            Osc99Part::Title => "title",
            Osc99Part::Body => "body",
        }
    }
}

/// One chunk of a kitty OSC 99 notification:
/// `ESC ] 99 ; i=<id>:d=<0|1>:u=<urgency>:p=<part> ; <payload> ST`.
///
/// `done` marks the final chunk (the receiver renders on `d=1`);
/// `urgency` is 0 (low) / 1 (normal) / 2 (critical). The metadata block
/// is assembled from typed pieces — no escape or `:`/`;` grammar leaks to
/// the call site.
#[must_use]
pub fn osc99_notify(id: &str, done: bool, urgency: u8, part: Osc99Part, payload: &str) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut meta = String::with_capacity(id.len() + 20);
    let _ = write!(meta, "i={id}:d={}:u={urgency}:p={}", u8::from(done), part.as_str());
    osc(99, &[&meta, payload], OscTerminator::St)
}

/// OSC 1337 (iTerm2) `RequestAttention`: `ESC ] 1337 ; RequestAttention=<0|1> BEL`.
/// Bounces the dock until focus returns (mado maps it to a Critical,
/// focus-bypassing notification + dock attention).
#[must_use]
pub fn osc1337_request_attention(on: bool) -> Vec<u8> {
    let param = if on { "RequestAttention=1" } else { "RequestAttention=0" };
    osc(1337, &[param], OscTerminator::Bel)
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
    CursorPosition { row: usize, col: usize },
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
    SetScrollRegion { top: usize, bottom: Option<usize> },
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
        assert_eq!(csi(true, &[flags], "", b'u'), format!("\x1b[?{flags}u").into_bytes());
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
        assert_eq!(apc(&format!("Gi={id};OK")), format!("\x1b_Gi={id};OK\x1b\\").into_bytes());
    }

    #[test]
    fn csi_with_no_params_is_just_envelope_and_final() {
        assert_eq!(csi(false, &[], "", b'c'), b"\x1b[c");
        assert_eq!(csi(true, &[], "", b'c'), b"\x1b[?c");
    }

    #[test]
    fn osc_envelope_bel_and_st() {
        assert_eq!(osc(9, &["hi"], OscTerminator::Bel), b"\x1b]9;hi\x07");
        assert_eq!(osc(99, &["m", "p"], OscTerminator::St), b"\x1b]99;m;p\x1b\\");
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
        assert_eq!(osc1337_request_attention(true), b"\x1b]1337;RequestAttention=1\x07");
        assert_eq!(osc1337_request_attention(false), b"\x1b]1337;RequestAttention=0\x07");
    }
}
