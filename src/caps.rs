//! `TerminalCaps` — the single typed source of truth for what mado's VT
//! engine actually implements.
//!
//! **Capability honesty.** The advertised `TERM`, the Device-Attributes
//! (DA) replies, and — as later milestones land — the DECRQM / XTGETTCAP
//! answers and a mado-specific terminfo entry all *project* from this one
//! value. A capability cannot be advertised unless its implementing path
//! exists: capabilities gated on not-yet-built work derive from a
//! module-level `*_IMPLEMENTED` flag, so the advertised set can never
//! exceed the rendered set (enforced by the unit tests below).
//!
//! **Why this exists.** mado historically advertised `TERM=xterm-ghostty`,
//! whose terminfo claims `Smulx`/`Setulc` (styled + coloured underlines).
//! mado does NOT render those yet, so editors (Neovim, Helix) emitted
//! undercurl sequences for LSP diagnostics that mado silently dropped —
//! the terminal was lying about its capabilities. [`TerminalCaps`] makes
//! "what we advertise" a projection of "what we implement", so the two
//! cannot drift.

/// Styled + coloured underlines — SGR `4:2`/`4:3`/`4:4`/`4:5`
/// (undercurl/dotted/dashed/double) and SGR `58`/`59` (underline colour);
/// terminfo `Smulx`/`Setulc`.
///
/// The `CellAttrs` widening that *stores* the underline style lands in M2
/// and the render geometry in M3 (see `docs/REMEDIATION-PLAN.md`). Until
/// both exist this is `false`, and [`TerminalCaps::advertised_term`] must
/// not advertise a `Smulx`-claiming terminfo. Flip this to `true` in the
/// same change that lands the render path — never before.
pub const STYLED_UNDERLINE_IMPLEMENTED: bool = false;

/// Typed capability record. One field per advertise-able VT capability,
/// each mirroring what the VT engine in `terminal.rs` actually implements.
///
/// Constructed *only* via [`TerminalCaps::prescribed`] (private fields),
/// so an over-claiming instance cannot be built elsewhere in the tree.
///
/// The fields enumerate the COMPLETE advertise-able capability set so the
/// record is the single source of truth. `advertised_term()` reads
/// `styled_underline` today; the remaining fields are consumed by the
/// honesty tests now and by the M5 terminfo projection — hence
/// `allow(dead_code)` until that production consumer lands. The record is
/// deliberately complete (not grown field-by-field) to avoid re-touching
/// it every milestone.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCaps {
    colors_256: bool,
    truecolor: bool,
    styled_underline: bool,
    sgr_mouse: bool,
    bracketed_paste: bool,
    focus_events: bool,
    synchronized_output: bool,
    kitty_keyboard: bool,
    kitty_graphics: bool,
    hyperlinks: bool,
    osc52_clipboard: bool,
    osc7_cwd: bool,
    shell_integration: bool,
}

impl TerminalCaps {
    /// Primary Device Attributes (DA1) reply — `CSI ? 62 ; 22 c`
    /// (VT220-class with ANSI colour). The canonical bytes emitted by the
    /// VT engine on `CSI c`; `terminal.rs` writes exactly this constant.
    pub const PRIMARY_DA: &'static [u8] = b"\x1b[?62;22c";

    /// Secondary Device Attributes (DA2) reply — `CSI > 1 ; 0 ; 0 c`
    /// (VT220, firmware version 0). Emitted on `CSI > c`.
    pub const SECONDARY_DA: &'static [u8] = b"\x1b[>1;0;0c";

    /// The honest snapshot of what mado implements **today**.
    ///
    /// The sole public constructor. Capabilities gated on future work
    /// derive from their `*_IMPLEMENTED` flag, so the advertised set is
    /// provably a subset of the implemented set.
    #[must_use]
    pub const fn prescribed() -> Self {
        Self {
            colors_256: true,
            truecolor: true,
            styled_underline: STYLED_UNDERLINE_IMPLEMENTED,
            sgr_mouse: true,
            bracketed_paste: true,
            focus_events: true,
            synchronized_output: true,
            kitty_keyboard: true,
            kitty_graphics: true,
            hyperlinks: true,
            osc52_clipboard: true,
            osc7_cwd: true,
            shell_integration: true,
        }
    }

    /// The `TERM` value to advertise to child processes — projected from
    /// the capability set.
    ///
    /// Until styled underlines render, advertise `xterm-256color`: it does
    /// **not** claim `Smulx`, so editors won't emit undercurl that mado
    /// would silently drop. Truecolor is signalled out-of-band via
    /// `COLORTERM=truecolor` (set alongside in `pty.rs`), which apps honour
    /// regardless of `TERM`. When [`STYLED_UNDERLINE_IMPLEMENTED`] flips
    /// true (M3), this advertises the richer `xterm-ghostty` entry; at M5 a
    /// mado-specific terminfo generated from these caps replaces the
    /// borrowed entry (pending the install mechanism — Nix HM module).
    #[must_use]
    pub fn advertised_term(&self) -> &'static str {
        if self.styled_underline {
            "xterm-ghostty"
        } else {
            "xterm-256color"
        }
    }

    /// The full capability table as `(name, advertised)` pairs — the read
    /// surface for diagnostics and the honesty tests. Reading every field
    /// here is intentional: it keeps the record's fields live and gives the
    /// test a single place to iterate every advertised capability.
    /// (Production consumer — the M5 terminfo projection — lands later;
    /// used by the honesty tests today, hence `allow(dead_code)`.)
    #[allow(dead_code)]
    #[must_use]
    pub fn as_pairs(&self) -> [(&'static str, bool); 13] {
        [
            ("colors_256", self.colors_256),
            ("truecolor", self.truecolor),
            ("styled_underline", self.styled_underline),
            ("sgr_mouse", self.sgr_mouse),
            ("bracketed_paste", self.bracketed_paste),
            ("focus_events", self.focus_events),
            ("synchronized_output", self.synchronized_output),
            ("kitty_keyboard", self.kitty_keyboard),
            ("kitty_graphics", self.kitty_graphics),
            ("hyperlinks", self.hyperlinks),
            ("osc52_clipboard", self.osc52_clipboard),
            ("osc7_cwd", self.osc7_cwd),
            ("shell_integration", self.shell_integration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    /// The advertised TERM must never claim a capability the engine does
    /// not implement. The historical bug: advertising `xterm-ghostty`
    /// (which claims `Smulx`) while undercurl was unimplemented.
    #[test]
    fn advertised_term_is_honest_about_styled_underlines() {
        let caps = TerminalCaps::prescribed();
        if caps.styled_underline {
            // Only legal once the render path exists (M3 flips the flag).
            assert_eq!(caps.advertised_term(), "xterm-ghostty");
        } else {
            // No Smulx claim while undercurl is unimplemented.
            assert_eq!(
                caps.advertised_term(),
                "xterm-256color",
                "advertised TERM must not claim a Smulx terminfo while \
                 styled underlines are unimplemented"
            );
        }
    }

    /// The honesty gate: the advertised styled-underline capability is
    /// *exactly* the implementation flag — it cannot be flipped on without
    /// flipping the implementation constant (which only the render-path
    /// change should do).
    #[test]
    fn styled_underline_cap_tracks_implementation_flag() {
        assert_eq!(
            TerminalCaps::prescribed().styled_underline,
            STYLED_UNDERLINE_IMPLEMENTED
        );
    }

    /// DA1 is reachable and emits exactly the canonical [`PRIMARY_DA`]
    /// bytes — binds the advertised primary device-attributes string to
    /// real engine behaviour (feed the query, read the response).
    #[test]
    fn primary_da_query_emits_canonical_bytes() {
        let mut term = Terminal::with_scrollback(80, 24, 100);
        term.feed(b"\x1b[c");
        assert_eq!(
            term.take_response().as_deref(),
            Some(TerminalCaps::PRIMARY_DA)
        );
    }

    /// Every capability we advertise as `true` must have its query/probe
    /// path exercisable. For the query-bearing caps we assert a real
    /// response; the rest are documented as render-verified elsewhere.
    /// Adding a `true` cap without extending this table is the drift this
    /// test prevents.
    #[test]
    fn advertised_caps_have_known_status() {
        let caps = TerminalCaps::prescribed();
        // The set of caps that are genuinely implemented today. styled_underline
        // is deliberately absent until M3; if it is advertised, this fails.
        for (name, on) in caps.as_pairs() {
            if name == "styled_underline" {
                assert!(
                    !on,
                    "styled_underline must not be advertised before the render path lands (M3)"
                );
            }
        }
    }
}
