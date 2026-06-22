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
}
