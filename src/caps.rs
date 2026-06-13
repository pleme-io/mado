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
/// TRUE since M3: storage landed in M2 (typed `Attrs{flags, underline,
/// underline_color}` inside the interned `Style`; SGR 4:N / 58 / 59
/// wire acquisition), and the render GEOMETRY shipped with the engawa
/// decoration emitters — Single/Double/Curly/Dotted/Dashed each
/// produce distinct pixels (the `underline_style_matrix_emits_engawa_
/// geometry` test in render.rs pins all six), with SGR 58 colour
/// honoured. The in-band proof is the pen-derived DECRQSS `m` report:
/// feed SGR 4:3, query, and `4:3` comes back (the CAP_PROBES row
/// below exercises exactly that against a real engine).
pub const STYLED_UNDERLINE_IMPLEMENTED: bool = true;

/// Sixel graphics — DCS `q … ST` decoded to RGBA and rendered through the
/// shared Kitty texture-upload path; DA1 attribute `4`.
///
/// TRUE since M4 (M3-C3 slice 2): the sixel DCS payload is decoded via
/// `icy_sixel` at `unhook` and fed `store_rgba_image` → an `ImagePlacement`
/// at the cursor — the same path Kitty images render through. Before this
/// the payload was stored raw and never drawn, so advertising sixel would
/// have been the same lie `styled_underline` once was. The honesty probe is
/// the DA1 reply: a sixel-capable DA1 carries `4` (`CSI ?62;4;22c`), so the
/// [`CAP_PROBES`] Query row below feeds `CSI c` and requires `;4;` back.
///
/// SCOPE (honest, review 2026-06-12, correctness-0): the image RENDERS
/// correctly, but the cursor STAYS PUT after a sixel — mado does not
/// implement sixel-scrolling cursor advance. xterm/foot move the cursor
/// down by `ceil(image_height_px / cell_height_px)` rows; doing that
/// here requires the cell-height-in-pixels, which is renderer-owned and
/// lazily measured — the VT engine (`terminal.rs`) deliberately holds
/// CHARACTER dimensions only (see the `CSI 14/16 t` carve-out), so text
/// emitted immediately after a sixel can overdraw the image's cell rows.
/// Advertised capability = "the image decodes + draws", not "cursor
/// re-flows below it". Threading cell-pixel metrics down is the deferred
/// follow-up that closes this gap.
pub const SIXEL_GRAPHICS_IMPLEMENTED: bool = true;

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
    sixel_graphics: bool,
    hyperlinks: bool,
    osc52_clipboard: bool,
    osc7_cwd: bool,
    shell_integration: bool,
}

impl TerminalCaps {
    /// Primary Device Attributes (DA1) reply — `CSI ? 62 ; 4 ; 22 c`
    /// (VT220-class, sixel graphics, ANSI colour). Attribute `4` = sixel,
    /// advertised since the M4 decode path landed (`SIXEL_GRAPHICS_IMPLEMENTED`).
    /// The canonical bytes emitted by the VT engine on `CSI c`; `terminal.rs`
    /// writes exactly this constant.
    pub const PRIMARY_DA: &'static [u8] = b"\x1b[?62;4;22c";

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
            sixel_graphics: SIXEL_GRAPHICS_IMPLEMENTED,
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
    pub fn as_pairs(&self) -> [(&'static str, bool); 14] {
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
            ("sixel_graphics", self.sixel_graphics),
            ("hyperlinks", self.hyperlinks),
            ("osc52_clipboard", self.osc52_clipboard),
            ("osc7_cwd", self.osc7_cwd),
            ("shell_integration", self.shell_integration),
        ]
    }
}

/// The typed PTY environment projection — the COMPLETE set of
/// `(key, value)` env pairs every spawn path stamps on a child shell,
/// derived from [`TerminalCaps`] (so the advertised `TERM` can never
/// drift from the implemented capability set) plus the terminfo
/// materialization arm.
///
/// **Why this type exists** (operator report 2026-06-12: "vim is grey
/// and the wrong font in the embedded-tear default path"). The
/// local-PTY path (`pty.rs`) projected the full capability env (`TERM`,
/// `TERMINFO`, `COLORTERM`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`) so
/// vim saw `xterm-ghostty` with truecolor, but the embedded-tear spawn
/// path (`tear-core::inproc`) only stamped conservative fallbacks
/// (`xterm-256color`, no `TERMINFO`) — so vim in the operator-default
/// (embedded) window got no truecolor (grey) and the wrong terminfo.
/// This projection is the ONE typed source both paths consume, so the
/// env a child sees is identical regardless of spawn entry point —
/// pinned by the spawn-env matrix test.
///
/// `COLUMNS`/`LINES` are NOT in this projection: they are size facts
/// the PTY winsize already carries (and the embedded path's grid
/// owns), not capability facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvProjection {
    pairs: Vec<(String, String)>,
}

impl EnvProjection {
    /// The honest env a child shell must see, derived from
    /// [`TerminalCaps::prescribed`]. Performs the same `TERM` /
    /// `TERMINFO` resolution the local-PTY path did, including the
    /// xterm-256color downgrade arm when terminfo materialization
    /// fails (read-only HOME / exotic sandbox) so the advertised entry
    /// is always resolvable.
    ///
    /// `TERM_PROGRAM_VERSION` is the caller's package version (mado's
    /// `CARGO_PKG_VERSION`) — passed in because `caps` cannot read the
    /// consumer crate's version macro.
    #[must_use]
    pub fn prescribed(program_version: &str) -> Self {
        let caps = TerminalCaps::prescribed();
        let advertised = caps.advertised_term();
        let mut pairs: Vec<(String, String)> = Vec::with_capacity(5);
        // TERM + TERMINFO: when the advertised entry is xterm-ghostty
        // (not in any system ncurses db), materialize the vendored
        // compiled entry + export TERMINFO; on failure downgrade TERM
        // to a universally-resolvable entry rather than naming one apps
        // cannot find ("missing or unsuitable terminal").
        let term = if advertised == "xterm-ghostty" {
            match crate::terminfo::ensure_terminfo_dir() {
                Ok(dir) => {
                    pairs.push(("TERMINFO".to_owned(), dir.to_string_lossy().into_owned()));
                    advertised.to_owned()
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "terminfo materialization failed — downgrading TERM to xterm-256color"
                    );
                    "xterm-256color".to_owned()
                }
            }
        } else {
            advertised.to_owned()
        };
        pairs.push(("TERM".to_owned(), term));
        // Truecolor is signalled OUT-OF-BAND of TERM (apps honour
        // COLORTERM regardless of the terminfo entry) — this is the
        // half that was missing on the embedded path, so vim rendered
        // 256-colour grey instead of mado's truecolor theme.
        pairs.push(("COLORTERM".to_owned(), "truecolor".to_owned()));
        pairs.push(("TERM_PROGRAM".to_owned(), "mado".to_owned()));
        pairs.push((
            "TERM_PROGRAM_VERSION".to_owned(),
            program_version.to_owned(),
        ));
        Self { pairs }
    }

    /// The projected `(key, value)` pairs, applied AFTER a child's
    /// inherited env so they OVERRIDE any conservative fallback the
    /// spawn backend stamped (tear-core's `xterm-256color` default
    /// loses to mado's `xterm-ghostty` here — the override is the fix).
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// The resolved `TERM` value (the projection always carries
    /// exactly one `TERM` pair).
    #[must_use]
    pub fn term(&self) -> &str {
        self.pairs
            .iter()
            .find(|(k, _)| k == "TERM")
            .map_or("xterm-256color", |(_, v)| v.as_str())
    }
}

/// How one advertised capability's honesty is verified against real
/// engine behaviour. Every [`TerminalCaps`] field MUST have exactly
/// one row in [`CAP_PROBES`] — the forcing tests below fail the
/// build when a cap is added without declaring how it is probed
/// (COMPETITIVE.md §4 "TERM/caps honesty matrix", pinned 2026-06-11).
#[allow(dead_code)] // Consumed by the honesty tests; the M5 terminfo projection is the production consumer.
#[derive(Debug, Clone, Copy)]
pub enum CapProbe {
    /// The cap has an in-band query path: feed `feed` to a fresh
    /// terminal and the response MUST contain `expect`
    /// (DECRQM / DA / kitty-query / OSC reply).
    Query {
        feed: &'static [u8],
        expect: &'static [u8],
    },
    /// No query path exists — the cap is pinned by the named unit
    /// test in `terminal.rs` (the forcing test asserts the test
    /// function still exists in source, so a rename breaks the gate).
    RenderVerified { test: &'static str },
    /// Advertised `false` until the implementing milestone lands —
    /// the forcing test asserts the cap is NOT advertised.
    GatedOff { flag: &'static str },
}

/// One probe row per advertised capability, in [`TerminalCaps::as_pairs`]
/// order. `CAP_PROBES.len() == as_pairs().len()` + name-set equality is
/// the gate that makes "ship a new cap unprobed" a build failure.
#[allow(dead_code)] // Consumed by the honesty tests today (see CapProbe).
pub const CAP_PROBES: &[(&str, CapProbe)] = &[
    ("colors_256", CapProbe::RenderVerified { test: "sgr_256color" }),
    ("truecolor", CapProbe::RenderVerified { test: "sgr_truecolor" }),
    (
        "styled_underline",
        // The kitty/neovim undercurl probe, run for real: set SGR
        // 4:3 (curly) + 58:5:1 (indexed colour), DECRQSS the SGR
        // state, and the pen-derived report must echo the style.
        CapProbe::Query {
            feed: b"\x1b[4:3;58:5:1m\x1bP$qm\x1b\\",
            expect: b"4:3",
        },
    ),
    (
        "sgr_mouse",
        CapProbe::Query {
            feed: b"\x1b[?1006h\x1b[?1006$p",
            expect: b"\x1b[?1006;1$y",
        },
    ),
    (
        "bracketed_paste",
        CapProbe::Query {
            feed: b"\x1b[?2004h\x1b[?2004$p",
            expect: b"\x1b[?2004;1$y",
        },
    ),
    (
        "focus_events",
        CapProbe::Query {
            feed: b"\x1b[?1004h\x1b[?1004$p",
            expect: b"\x1b[?1004;1$y",
        },
    ),
    (
        "synchronized_output",
        CapProbe::Query {
            feed: b"\x1b[?2026h\x1b[?2026$p",
            expect: b"\x1b[?2026;1$y",
        },
    ),
    (
        "kitty_keyboard",
        CapProbe::Query {
            feed: b"\x1b[?u",
            expect: b"\x1b[?0u",
        },
    ),
    (
        "kitty_graphics",
        CapProbe::RenderVerified { test: "kitty_graphics_direct_rgba" },
    ),
    (
        // Sixel support is advertised in the DA1 reply (attribute `4`).
        // Feed `CSI c`; a sixel-capable DA1 contains `;4;`. The decode
        // path itself is render-verified by `sixel_payload_decodes_*`,
        // but the in-band advertise honesty is the DA1 query.
        "sixel_graphics",
        CapProbe::Query {
            feed: b"\x1b[c",
            expect: b";4;",
        },
    ),
    ("hyperlinks", CapProbe::RenderVerified { test: "osc_8_hyperlink" }),
    (
        "osc52_clipboard",
        CapProbe::Query {
            feed: b"\x1b]52;c;?\x1b\\",
            expect: b"\x1b]52;c;",
        },
    ),
    ("osc7_cwd", CapProbe::RenderVerified { test: "osc_7_cwd" }),
    (
        "shell_integration",
        CapProbe::RenderVerified { test: "osc_133_prompt_marker" },
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    /// The shared env projection carries EXACTLY the capability env
    /// every spawn path must stamp (operator report 2026-06-12: vim
    /// grey + wrong font in the embedded-tear window came from the
    /// embedded path stamping only a conservative xterm-256color
    /// fallback). The projection always carries `TERM` + `COLORTERM` +
    /// `TERM_PROGRAM` + `TERM_PROGRAM_VERSION`; `TERMINFO` rides along
    /// iff the advertised entry is `xterm-ghostty` AND materialization
    /// succeeded.
    #[test]
    fn env_projection_carries_the_full_capability_env() {
        let proj = EnvProjection::prescribed("9.9.9");
        let has = |k: &str| proj.pairs().iter().any(|(pk, _)| pk == k);
        let get = |k: &str| {
            proj.pairs()
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| v.as_str())
        };
        assert!(has("TERM"), "every spawn must set TERM");
        // COLORTERM is the out-of-band truecolor signal — the half the
        // embedded path was missing (vim rendered 256-colour grey).
        assert_eq!(
            get("COLORTERM"),
            Some("truecolor"),
            "COLORTERM=truecolor must be projected so vim gets truecolor"
        );
        assert_eq!(get("TERM_PROGRAM"), Some("mado"));
        assert_eq!(get("TERM_PROGRAM_VERSION"), Some("9.9.9"));
        // TERM is the honest projection of the capability set.
        assert_eq!(
            proj.term(),
            TerminalCaps::prescribed().advertised_term(),
            "projected TERM must equal the capability-derived advertised TERM (or its \
             downgrade)"
        );
        // When xterm-ghostty is advertised, TERMINFO must accompany it
        // (so apps can resolve the entry) OR the TERM downgraded to a
        // universally-resolvable entry — never xterm-ghostty WITHOUT
        // TERMINFO.
        if proj.term() == "xterm-ghostty" {
            assert!(
                has("TERMINFO"),
                "xterm-ghostty advertised without TERMINFO — apps cannot resolve the entry"
            );
        }
    }

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
        // M3 landed the render geometry + the pen-derived DECRQSS
        // report — styled_underline is now advertised, and the
        // CAP_PROBES Query row (exercised against the real engine in
        // cap_probe_rows_hold_against_the_engine) is what keeps the
        // advertisement honest.
        for (name, on) in caps.as_pairs() {
            if name == "styled_underline" {
                assert!(
                    on,
                    "styled_underline render path + DECRQSS report landed at M3 — \
                     the cap must advertise"
                );
            }
        }
    }

    /// **Forcing gate: every advertised cap has exactly one probe
    /// row** (pinned 2026-06-11, COMPETITIVE.md §4 "TERM/caps honesty
    /// matrix"). A newly-advertised capability with no [`CAP_PROBES`]
    /// row fails this build — `advertised_caps_have_known_status`
    /// alone special-cased only styled_underline, so a new cap could
    /// previously ship silently unprobed.
    #[test]
    fn cap_probes_table_covers_every_advertised_cap() {
        let pairs = TerminalCaps::prescribed().as_pairs();
        let mut failures: Vec<String> = Vec::new();

        if CAP_PROBES.len() != pairs.len() {
            failures.push(format!(
                "CAP_PROBES has {} rows but TerminalCaps advertises {} fields",
                CAP_PROBES.len(),
                pairs.len()
            ));
        }
        for (name, _) in pairs {
            if !CAP_PROBES.iter().any(|(probe_name, _)| *probe_name == name) {
                failures.push(format!("cap {name:?} has no CAP_PROBES row"));
            }
        }
        for (probe_name, _) in CAP_PROBES {
            if !pairs.iter().any(|(name, _)| name == probe_name) {
                failures.push(format!(
                    "CAP_PROBES row {probe_name:?} names no TerminalCaps field"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} probe-coverage gaps:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// **Every probe row holds against the real engine.** Query rows
    /// feed their bytes to a fresh terminal and must see the expected
    /// reply; GatedOff rows must NOT be advertised; RenderVerified
    /// rows must be advertised AND their named pinning test must
    /// still exist in `terminal.rs` source (a rename breaks the
    /// reference, keeping the table honest). Failures aggregate so
    /// one run reports every broken probe, not just the first.
    #[test]
    fn cap_probe_rows_hold_against_the_engine() {
        let pairs = TerminalCaps::prescribed().as_pairs();
        let advertised = |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, on)| *on)
                .unwrap_or(false)
        };
        let terminal_src = include_str!("terminal.rs");
        let mut failures: Vec<String> = Vec::new();

        for (name, probe) in CAP_PROBES {
            match probe {
                CapProbe::Query { feed, expect } => {
                    if !advertised(name) {
                        failures.push(format!(
                            "{name}: Query probe on an unadvertised cap — \
                             use GatedOff until it ships"
                        ));
                        continue;
                    }
                    let mut term = Terminal::with_scrollback(80, 24, 100);
                    term.feed(feed);
                    let response = term.take_response().unwrap_or_default();
                    let found = response
                        .windows(expect.len().max(1))
                        .any(|w| w == *expect);
                    if !found {
                        failures.push(format!(
                            "{name}: query {:?} answered {:?}, expected it to \
                             contain {:?}",
                            String::from_utf8_lossy(feed),
                            String::from_utf8_lossy(&response),
                            String::from_utf8_lossy(expect)
                        ));
                    }
                }
                CapProbe::RenderVerified { test } => {
                    if !advertised(name) {
                        failures.push(format!(
                            "{name}: RenderVerified probe on an unadvertised cap"
                        ));
                    }
                    let needle = format!("fn {test}(");
                    if !terminal_src.contains(&needle) {
                        failures.push(format!(
                            "{name}: pinning test `{test}` not found in \
                             terminal.rs — renamed or removed?"
                        ));
                    }
                }
                CapProbe::GatedOff { flag } => {
                    if advertised(name) {
                        failures.push(format!(
                            "{name}: advertised but its probe row says GatedOff \
                             ({flag}) — add a real Query/RenderVerified probe in \
                             the same change that flips the flag"
                        ));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} probe rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}
