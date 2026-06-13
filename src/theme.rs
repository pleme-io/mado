//! Color theme adapter — thin shim over `irodzuki::ColorScheme`.
//!
//! The 8 built-in themes (nord, dracula, ...) live in
//! `irodzuki::presets`, NOT here. This file owns the mado-side
//! adaptation: take an irodzuki `ColorScheme` and project it into
//! mado's `Theme` shape (u8-RGB `terminal::Color` for the renderer,
//! plus the selection-overlay RGBA + ANSI table the GPU pipeline
//! wants).
//!
//! Adding a new theme = adding one entry to `irodzuki::presets` —
//! no mado-side change required. Substrate compounding by
//! construction.

use std::sync::OnceLock;

use irodzuki::scheme::{Base16Slot, Color as IroColor, ColorScheme};

use crate::terminal::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    pub background: Color,
    pub foreground: Color,
    pub cursor: Color,
    /// Selection-overlay colour, **already linearized**. The rect
    /// pipeline (`RectInstance.color`) writes its value verbatim to the
    /// sRGB-storage surface where wgpu performs the linear→sRGB encode
    /// on store, so every colour that pipeline consumes MUST be linear
    /// (the same discipline `render::color_to_f32` enforces for per-cell
    /// backgrounds). `theme_from_scheme` runs `scheme.base02` (raw sRGB)
    /// through `ishou_tokens::SrgbA::to_linear` before it lands here, so
    /// a raw-sRGB value can never reach the GPU through this field.
    pub selection_bg: [f32; 4],
    pub ansi: [Color; 16],
    /// AGENT-RESERVED accent (`SemanticRoles.agent` → Borealis
    /// `fable_violet`). The vigy / MCP-activity / attention chrome —
    /// search-status text today — paints with THIS token, never a hex.
    /// On non-Borealis presets this is the preset's foreground (no
    /// agent-band concept exists in irodzuki schemes), so legacy themes
    /// keep their prior look. `u8`-RGB for the renderer; the GPU path
    /// linearizes at paint time exactly like every other `Color` field.
    pub agent_accent: Color,
    /// CURRENT search-match highlight fill (`Borealis first_light`
    /// #EDC980 — the inverse-video search band). The search-match rect
    /// pipeline paints THIS token (linearized at paint time), never a
    /// hex. On non-Borealis presets it falls back to Nord aurora yellow
    /// #EBCB8B (irodzuki schemes carry no search-band concept), so legacy
    /// themes keep their prior look. `u8`-RGB for the renderer.
    pub search_current: Color,
    /// OTHER (non-current) search-match highlight fill (`Borealis
    /// search_others` #4E443A). See [`Self::search_current`].
    pub search_others: Color,
}

impl Theme {
    #[must_use]
    pub fn by_name(name: &str) -> Option<&'static Theme> {
        // `"borealis"` is the operator-friendly short form of the
        // prescribed `"borealis-night"` (the only Borealis variant that
        // ships today). Normalize it here so both names resolve through
        // the same registered theme — no duplicate entry, no drift.
        let canonical = if name.eq_ignore_ascii_case("borealis") {
            "borealis-night"
        } else {
            name
        };
        all().iter().find(|t| t.name.eq_ignore_ascii_case(canonical))
    }

    #[must_use]
    pub fn available() -> &'static [Theme] {
        all()
    }
}

/// Apply the named config theme to BOTH the renderer (ANSI palette,
/// selection bg, cursor colour, bg/fg) AND the mirror `Terminal`
/// (`apply_theme`) — the ONE shared theme-application point both the
/// local-PTY loop (`main.rs`) and the tear-attach loop
/// (`gui_tear_attach.rs`) call, so the two render modes cannot diverge.
///
/// **Why this is shared** (operator report 2026-06-12: "wrong font +
/// palette / vim grey in the embedded-tear window"): the tear-attach
/// path previously skipped theme application entirely — it never
/// called `Terminal::apply_theme`, so the mirror palette + OSC 11
/// background-query answer stayed at the default, and the operator's
/// configured theme never reached an embedded window. Routing both
/// paths through this function makes the palette identical by
/// construction (pinned by the entry-point parity test).
///
/// No-op when the theme name does not resolve (the renderer keeps the
/// foreground/background it was built with). `opacity` is the
/// appearance opacity applied to the theme background through the
/// typed sRGB→linear path so a theme swap doesn't reintroduce gamma
/// confusion.
pub fn apply_config_theme(
    renderer: &mut crate::render::TerminalRenderer,
    terminal: &crate::render::SharedTerminal,
    theme_name: &str,
    opacity: f32,
) {
    let Some(theme) = Theme::by_name(theme_name) else {
        return;
    };
    renderer.set_ansi_colors(theme.ansi);
    renderer.set_selection_bg(theme.selection_bg);
    // Cursor overlay colour at 0.85 alpha — same as the local-PTY path.
    let cursor = ishou_tokens::Srgb::new(theme.cursor.r, theme.cursor.g, theme.cursor.b).to_linear();
    renderer.set_cursor_color([cursor.r, cursor.g, cursor.b, 0.85]);
    // AGENT-RESERVED chrome accent: the search-status line (and any
    // future agent / MCP-activity surface) paints with the theme's
    // `agent_accent` — Borealis `fable_violet` via the SEMANTIC role.
    // Routed through this ONE shared point so the local-PTY loop AND the
    // embedded-tear loop pick up the agent accent identically.
    renderer.set_search_status_color(theme.agent_accent);
    // Search-match highlight fills — the CURRENT match (inverse-video
    // first_light on Borealis) and the OTHER matches (search_others).
    // Routed through this ONE shared point so the local-PTY loop AND the
    // embedded-tear loop pick up the search palette identically; the
    // render path linearizes both at paint time via `overlay_rect_color`.
    renderer.set_search_current_color(theme.search_current);
    renderer.set_search_other_color(theme.search_others);
    // Theme bg through the typed Srgb → Linear path (no gamma confusion).
    let theme_bg: wgpu::Color = ishou_tokens::Srgb::new(
        theme.background.r,
        theme.background.g,
        theme.background.b,
    )
    .to_linear()
    .with_alpha(opacity)
    .into();
    renderer.set_bg_fg(theme_bg, theme.foreground);
    // The mirror Terminal half — palette + OSC 11 background-query
    // answer. This is the call the tear path was MISSING.
    terminal
        .write()
        .apply_theme(theme.foreground, theme.background, theme.ansi);
}

/// Cursor slot per preset — most themes' cursor is the foreground
/// (`Base05`); a few (`one-dark`) want the typed `base0d` blue.
/// Catppuccin Mocha's official cursor is Rosewater (base0F here).
fn cursor_slot_for(name: &str) -> Base16Slot {
    match name {
        "one-dark" => Base16Slot::Base0D,
        "catppuccin-mocha" => Base16Slot::Base0F,
        _ => Base16Slot::Base05,
    }
}

fn iro_to_color(c: IroColor) -> Color {
    Color::new(
        (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// Project an irodzuki `Color` (raw sRGB floats, byte/255 with no gamma)
/// into a **linear** RGBA tuple for the rect pipeline. The overlay rect
/// shader writes its colour verbatim to the sRGB-storage surface, so the
/// value handed to it must already be linear — exactly the discipline
/// `render::color_to_f32` applies to per-cell backgrounds. We round-trip
/// the 8-bit channels through the typed `ishou_tokens::SrgbA::to_linear`
/// path (alpha stays linear by convention) so a raw-sRGB value can never
/// reach the GPU through `Theme::selection_bg`.
fn iro_to_linear_rgba(c: IroColor) -> [f32; 4] {
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let lin = ishou_tokens::Srgb::new(to_u8(c.r), to_u8(c.g), to_u8(c.b))
        .with_alpha(to_u8(c.a))
        .to_linear();
    [lin.r, lin.g, lin.b, lin.a]
}

fn f32_array_to_color(a: [f32; 4]) -> Color {
    Color::new(
        (a[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (a[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (a[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn theme_from_scheme(scheme: ColorScheme) -> Theme {
    // Leak the name as `'static` so consumers can treat presets as
    // long-lived (the global OnceLock keeps them alive forever
    // anyway; the leak is bounded by the 8-preset universe).
    let name: &'static str = Box::leak(scheme.name.clone().into_boxed_str());
    let cursor = scheme.get(cursor_slot_for(&scheme.name));
    let ansi_f32 = scheme.to_ansi_colors();
    let mut ansi = [Color::BLACK; 16];
    for (i, a) in ansi_f32.iter().enumerate() {
        ansi[i] = f32_array_to_color(*a);
    }
    Theme {
        name,
        background: iro_to_color(scheme.base00),
        foreground: iro_to_color(scheme.base05),
        cursor: iro_to_color(cursor),
        selection_bg: iro_to_linear_rgba(scheme.base02),
        ansi,
        // irodzuki presets have no agent band — the agent accent falls
        // back to the preset foreground so legacy themes are unchanged.
        agent_accent: iro_to_color(scheme.base05),
        // irodzuki presets carry no search-band surfaces — fall back to
        // Nord aurora yellow #EBCB8B (the prior hardcoded fill) so legacy
        // presets render exactly as before.
        search_current: Color::new(0xEB, 0xCB, 0x8B),
        search_others: Color::new(0xEB, 0xCB, 0x8B),
    }
}

/// Parse a `#RRGGBB` hex string into a u8-RGB `Color`. The Borealis
/// `ResolvedTheme` carries hex strings (consumers parse with their own
/// hex path); this is mado's. A malformed hex (impossible from the
/// BORN tokens) falls back to black so the theme still loads.
fn color_from_hex(hex: &str) -> Color {
    ishou_tokens::Srgb::from_hex(hex)
        .map_or(Color::BLACK, |s| Color::new(s.r, s.g, s.b))
}

/// **Borealis** (`borealis-night`) — the prescribed fleet theme. Built
/// from the BORN ishou tokens (`ResolvedTheme::borealis_night()` +
/// `BorealisPalette::night().surfaces()`), so this projection can never
/// drift from the canonical palette: the ANSI-16 table, the night0
/// background, the snow1 foreground, the `green_bright` cursor, and the
/// byte-exact violet-glass selection all flow straight from spec §4/§5.
///
/// The agent accent comes through the SEMANTIC `agent` role
/// (`SemanticRoles::borealis_night().agent` → `fable_violet`), never a
/// hex — so a future agent-band retune in ishou propagates here on the
/// next compile.
fn borealis_night_theme() -> Theme {
    let resolved = ishou_tokens::ResolvedTheme::borealis_night();
    let surfaces = ishou_tokens::BorealisPalette::night().surfaces();
    let roles = ishou_tokens::SemanticRoles::borealis_night();

    // §4 — the one canonical ANSI-16 mapping fleet-wide.
    let ansi: [Color; 16] = core::array::from_fn(|i| color_from_hex(&resolved.ansi_16[i]));

    // The agent accent via the SEMANTIC role, resolved through the
    // palette's own `get` (the role key is `"fable_violet"`).
    let agent_rgb = ishou_tokens::BorealisPalette::night()
        .get(roles.agent)
        .unwrap_or(surfaces.foreground);

    Theme {
        name: "borealis-night",
        background: Color::new(surfaces.background.r, surfaces.background.g, surfaces.background.b),
        foreground: Color::new(surfaces.foreground.r, surfaces.foreground.g, surfaces.foreground.b),
        // §5 — block cursor is `green_bright` (an inverse pair ≥7.0).
        cursor: Color::new(surfaces.cursor.r, surfaces.cursor.g, surfaces.cursor.b),
        // The byte-exact violet-glass blend product, linearized for the
        // overlay rect pipeline (same discipline as the irodzuki path:
        // the rect shader writes its colour verbatim to the sRGB-storage
        // surface, so the value handed to it must already be linear).
        selection_bg: {
            let s = surfaces.selection_background;
            let lin = ishou_tokens::Srgb::new(s.r, s.g, s.b).with_alpha(0xFF).to_linear();
            [lin.r, lin.g, lin.b, lin.a]
        },
        ansi,
        agent_accent: Color::new(agent_rgb.r, agent_rgb.g, agent_rgb.b),
        // §5 — the search band: CURRENT match is first_light #EDC980
        // (inverse video), OTHER matches are search_others #4E443A. Both
        // flow straight from the BORN surfaces, so a future search-band
        // retune in ishou propagates here on the next compile.
        search_current: Color::new(
            surfaces.search_current_background.r,
            surfaces.search_current_background.g,
            surfaces.search_current_background.b,
        ),
        search_others: Color::new(
            surfaces.search_others_background.r,
            surfaces.search_others_background.g,
            surfaces.search_others_background.b,
        ),
    }
}

fn all() -> &'static [Theme] {
    static THEMES: OnceLock<Vec<Theme>> = OnceLock::new();
    THEMES.get_or_init(|| {
        // Borealis is the prescribed fleet theme — registered alongside
        // the irodzuki presets so `Theme::by_name("borealis-night")`
        // resolves on the boot path AND the M4 ConfigApplier
        // (`ux::config_apply::resolve` → `Theme::by_name`). The alias
        // `"borealis"` is wired in `Theme::by_name` so the short form
        // operators are likely to type also resolves.
        let mut themes: Vec<Theme> = irodzuki::presets::all()
            .into_iter()
            .map(theme_from_scheme)
            .collect();
        themes.push(borealis_night_theme());
        themes
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_loads() {
        let themes = Theme::available();
        // 8 irodzuki presets + the registered Borealis fleet theme.
        assert_eq!(themes.len(), 9);
        for t in themes {
            assert!(!t.name.is_empty(), "theme has empty name");
        }
    }

    #[test]
    fn borealis_night_resolves_from_born_tokens() {
        // Both the canonical name and the short alias resolve to the
        // SAME registered theme (no duplicate, no drift).
        let b = Theme::by_name("borealis-night").expect("borealis-night theme");
        let alias = Theme::by_name("borealis").expect("borealis alias");
        assert_eq!(b.name, "borealis-night");
        assert_eq!(alias.name, "borealis-night");
        // §5 — background night0 (#1F222F), foreground snow1 (#D4D9E3),
        // cursor green_bright (#74E29F). These come straight from the
        // ishou BORN tokens via `ResolvedTheme::borealis_night()`.
        assert_eq!(b.background, Color::new(0x1F, 0x22, 0x2F));
        assert_eq!(b.foreground, Color::new(0xD4, 0xD9, 0xE3));
        assert_eq!(b.cursor, Color::new(0x74, 0xE2, 0x9F));
        // §4 — ANSI 2 is the signature aurora_green (#67D191); ANSI 0 is
        // night2 (surface), NEVER base00.
        assert_eq!(b.ansi[2], Color::new(0x67, 0xD1, 0x91));
        assert_eq!(b.ansi[0], Color::new(0x38, 0x3B, 0x4C));
        assert_eq!(b.ansi[15], Color::new(0xF5, 0xF7, 0xFA)); // snow3 = base07
    }

    #[test]
    fn borealis_agent_accent_is_the_fable_violet_semantic_token() {
        // The agent accent flows through the SEMANTIC `agent` role
        // (= `fable_violet` #B69AE9), not a hand-pinned hex — so a
        // future agent-band retune in ishou propagates on next compile.
        let b = Theme::by_name("borealis-night").expect("borealis-night theme");
        let fable_violet = ishou_tokens::BorealisPalette::night()
            .get(ishou_tokens::SemanticRoles::borealis_night().agent)
            .expect("fable_violet token");
        assert_eq!(
            b.agent_accent,
            Color::new(fable_violet.r, fable_violet.g, fable_violet.b),
        );
        assert_eq!(b.agent_accent, Color::new(0xB6, 0x9A, 0xE9));
    }

    #[test]
    fn borealis_selection_is_the_byte_exact_violet_glass() {
        // The selection overlay is the byte-exact violet-glass blend
        // product (#3F3955), linearized for the rect pipeline. Round-
        // tripping the linear value back to sRGB recovers the spec hex.
        let b = Theme::by_name("borealis-night").expect("borealis-night theme");
        let [r, g, bl, _a] = b.selection_bg;
        let back = ishou_tokens::Linear { r, g, b: bl }.to_srgb();
        assert_eq!(back.hex(), "#3F3955");
    }

    #[test]
    fn borealis_search_matches_resolve_from_born_surfaces() {
        // §5 — the search band flows from the BORN surfaces, never a
        // hand-pinned hex: current = first_light #EDC980 (inverse
        // video), other = search_others #4E443A. A future search-band
        // retune in ishou propagates here on next compile.
        let b = Theme::by_name("borealis-night").expect("borealis-night theme");
        let surfaces = ishou_tokens::BorealisPalette::night().surfaces();
        assert_eq!(
            b.search_current,
            Color::new(
                surfaces.search_current_background.r,
                surfaces.search_current_background.g,
                surfaces.search_current_background.b,
            ),
        );
        assert_eq!(
            b.search_others,
            Color::new(
                surfaces.search_others_background.r,
                surfaces.search_others_background.g,
                surfaces.search_others_background.b,
            ),
        );
        assert_eq!(b.search_current, Color::new(0xED, 0xC9, 0x80));
        assert_eq!(b.search_others, Color::new(0x4E, 0x44, 0x3A));
    }

    #[test]
    fn legacy_presets_keep_nord_aurora_search_fill() {
        // irodzuki presets carry no search band — the fields fall back
        // to Nord aurora yellow #EBCB8B so legacy themes render exactly
        // as before the surface map landed.
        let nord = Theme::by_name("nord").expect("nord preset");
        assert_eq!(nord.search_current, Color::new(0xEB, 0xCB, 0x8B));
        assert_eq!(nord.search_others, Color::new(0xEB, 0xCB, 0x8B));
    }

    #[test]
    fn nord_lookup_succeeds() {
        let nord = Theme::by_name("nord").expect("nord preset");
        assert_eq!(nord.name, "nord");
        // background = #2E3440 → (46, 52, 64)
        assert_eq!(nord.background, Color::new(46, 52, 64));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert!(Theme::by_name("NORD").is_some());
        assert!(Theme::by_name("DraCuLa").is_some());
    }
}
