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
    /// AGENT-RESERVED accent (`SemanticRoles.agent` → Vellum
    /// `fable_violet`). The vigy / MCP-activity / attention chrome —
    /// search-status text today — paints with THIS token, never a hex.
    /// On non-Vellum presets this is the preset's foreground (no
    /// agent-band concept exists in irodzuki schemes), so legacy themes
    /// keep their prior look. `u8`-RGB for the renderer; the GPU path
    /// linearizes at paint time exactly like every other `Color` field.
    pub agent_accent: Color,
    /// CURRENT search-match highlight fill (`Vellum first_light`
    /// #D7C489 — the inverse-video search band). The search-match rect
    /// pipeline paints THIS token (linearized at paint time), never a
    /// hex. On non-Vellum presets it falls back to Nord aurora yellow
    /// #EBCB8B (irodzuki schemes carry no search-band concept), so legacy
    /// themes keep their prior look. `u8`-RGB for the renderer.
    pub search_current: Color,
    /// OTHER (non-current) search-match highlight fill (`Vellum
    /// search_others` #443E2A). See [`Self::search_current`].
    pub search_others: Color,
}

impl Theme {
    #[must_use]
    pub fn by_name(name: &str) -> Option<&'static Theme> {
        // `"nord-matte"` is the descriptive alias of the prescribed
        // `"vellum"` (the warm aged-paper Nord-matte fleet theme).
        // Normalize it here so both names resolve through the same
        // registered theme — no duplicate entry, no drift.
        let canonical = if name.eq_ignore_ascii_case("nord-matte") {
            "vellum"
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
/// Linear-ish per-channel lerp `a*(1-t) + b*t` over sRGB u8 channels —
/// good enough for deriving the popup card's elevated surface + highlight
/// tints from the theme (these are chrome fills, not colour-critical
/// content; the GPU still linearizes them at paint time).
#[must_use]
fn blend(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8
    };
    Color::new(mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b))
}

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
    // `agent_accent` — Vellum `fable_violet` via the SEMANTIC role.
    // Routed through this ONE shared point so the local-PTY loop AND the
    // embedded-tear loop pick up the agent accent identically.
    renderer.set_search_status_color(theme.agent_accent);
    // Search-match highlight fills — the CURRENT match (inverse-video
    // first_light on Vellum) and the OTHER matches (search_others).
    // Routed through this ONE shared point so the local-PTY loop AND the
    // embedded-tear loop pick up the search palette identically; the
    // render path linearizes both at paint time via `overlay_rect_color`.
    renderer.set_search_current_color(theme.search_current);
    renderer.set_search_other_color(theme.search_others);
    // Picker overlay chrome (Ctrl-S switcher + Ctrl-T dirs): resolve every
    // colour from the active theme so the pickers track the theme instead
    // of hardcoded Nord literals. The Center popup gets a SOLID card:
    //   panel       = the terminal bg lifted ~10% toward fg (an elevated,
    //                 still-dark surface that reads as a floating card),
    //   border      = the theme accent (a hairline edge),
    //   selected_bg = the bg tinted ~32% toward the accent (the highlight
    //                 bar that tracks the selection as you juggle sessions).
    let panel = blend(theme.background, theme.foreground, 0.10);
    let selected_bg = blend(theme.background, theme.agent_accent, 0.32);
    renderer.set_overlay_style(crate::picker::component::OverlayStyle {
        query: theme.agent_accent,
        row: theme.foreground,
        selected: theme.ansi[15],
        hint: theme.ansi[8],
        panel,
        border: theme.agent_accent,
        selected_bg,
    });
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

/// **Vellum** (`vellum`) — the prescribed fleet theme, warm aged-paper
/// Nord-matte. The CHROME is built from the BORN ishou tokens
/// (`VellumPalette::vellum().surfaces()`), so it can never drift from the
/// canonical palette: the night0 background, the snow1 foreground, the
/// `green_bright` cursor, and the byte-exact violet-glass selection all
/// flow straight from spec §5.
///
/// The ANSI-16 CONTENT palette is sourced straight from the ishou
/// keystone via `VellumPalette::vellum().content_ansi_16()` — the vivid
/// Nord content table (parchment NEUTRALS in slots 0/7/8/15 = night2 /
/// snow1 / shadow0 / snow3; vivid Nord aurora/frost CHROMATICS in 1–6,
/// 9–14), DECOUPLED from the muted BORN parchment ANSI on purpose
/// (washed-out-colors fix, 2026-06-14). Apps paint their content (vim
/// syntax, shell, autocomplete) with these vivid colors at full contrast,
/// while the parchment chrome stays Vellum. Sourcing it from the keystone
/// (instead of a mado-local hex dup) means a content-palette retune in
/// ishou propagates here on the next compile.
///
/// The agent accent comes through the SEMANTIC `agent` role
/// (`SemanticRoles::vellum().agent` → `fable_violet`), never a hex — so
/// a future agent-band retune in ishou propagates here on the next
/// compile.
fn vellum_theme() -> Theme {
    let surfaces = ishou_tokens::VellumPalette::vellum().surfaces();
    let roles = ishou_tokens::SemanticRoles::vellum();

    // Decoupled CONTENT palette: the vivid Nord content table sourced
    // straight from the ishou keystone (`content_ansi_16()`), NOT the
    // muted BORN parchment `ResolvedTheme::vellum().ansi_16`. The keystone
    // table is parchment NEUTRALS (slots 0/7/8/15 = night2/snow1/shadow0/
    // snow3) + vivid Nord aurora/frost CHROMATICS (1–6, 9–14) — vivid ink
    // on a matte parchment ground. A future content-palette retune in
    // ishou now propagates here on the next compile (no mado-local dup).
    // The CHROME below (bg/fg/cursor/selection/search/agent) still derives
    // from the BORN Vellum surfaces + semantic roles.
    let content = ishou_tokens::VellumPalette::vellum().content_ansi_16();
    let ansi: [Color; 16] = core::array::from_fn(|i| Color::new(content[i].r, content[i].g, content[i].b));

    // The agent accent via the SEMANTIC role, resolved through the
    // palette's own `get` (the role key is `"fable_violet"`).
    let agent_rgb = ishou_tokens::VellumPalette::vellum()
        .get(roles.agent)
        .unwrap_or(surfaces.foreground);

    Theme {
        name: "vellum",
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
        // §5 — the search band: CURRENT match is first_light #D7C489
        // (inverse video), OTHER matches are search_others #443E2A. Both
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
        // Vellum is the prescribed fleet theme — registered alongside
        // the irodzuki presets so `Theme::by_name("vellum")` resolves on
        // the boot path AND the M4 ConfigApplier
        // (`ux::config_apply::resolve` → `Theme::by_name`). The alias
        // `"nord-matte"` is wired in `Theme::by_name` so the descriptive
        // form also resolves.
        let mut themes: Vec<Theme> = irodzuki::presets::all()
            .into_iter()
            .map(theme_from_scheme)
            .collect();
        themes.push(vellum_theme());
        themes
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_loads() {
        let themes = Theme::available();
        // 8 irodzuki presets + the registered Vellum fleet theme.
        assert_eq!(themes.len(), 9);
        for t in themes {
            assert!(!t.name.is_empty(), "theme has empty name");
        }
    }

    #[test]
    fn vellum_chrome_is_born_tokens_content_palette_is_vivid() {
        // Both the canonical name and the descriptive alias resolve to
        // the SAME registered theme (no duplicate, no drift).
        let b = Theme::by_name("vellum").expect("vellum theme");
        let alias = Theme::by_name("nord-matte").expect("nord-matte alias");
        assert_eq!(b.name, "vellum");
        assert_eq!(alias.name, "vellum");
        // CHROME stays BORN (§5): background night0 (#16140E), foreground
        // snow1 (#E2DBC8), cursor green_bright (#ADD7A3) — straight from
        // the ishou BORN tokens via `ResolvedTheme::vellum()`.
        assert_eq!(b.background, Color::new(0x16, 0x14, 0x0E));
        assert_eq!(b.foreground, Color::new(0xE2, 0xDB, 0xC8));
        assert_eq!(b.cursor, Color::new(0xAD, 0xD7, 0xA3));
        // CONTENT palette is decoupled + vivid, sourced from the ishou
        // keystone `VellumPalette::vellum().content_ansi_16()` (NOT a
        // mado-local hex dup, washed-out-colors fix). The table is vivid
        // Nord CHROMATICS in 1–6/9–14 with parchment NEUTRALS in 0/7/8/15.
        // The CHROMATICS: ANSI 2 (green) is vivid aurora_green #A3BE8C
        // (was the muted #A9BB8C); 1 (red) is vivid #BF616A; 6 (cyan) is
        // frost_1 #88C0D0; 9 (br-red) is Nord orange #D08770.
        assert_eq!(b.ansi[1], Color::new(0xBF, 0x61, 0x6A));
        assert_eq!(b.ansi[2], Color::new(0xA3, 0xBE, 0x8C));
        assert_eq!(b.ansi[6], Color::new(0x88, 0xC0, 0xD0)); // cyan frost_1
        assert_eq!(b.ansi[9], Color::new(0xD0, 0x87, 0x70)); // br-red Nord orange
        // The NEUTRALS (slots 0/7/8/15) are the keystone parchment tones:
        // night2 / snow1 / shadow0 / snow3 — coherent with the aged-paper
        // ground, NOT the old Nord polar-night/snow-storm values.
        assert_eq!(b.ansi[0], Color::new(0x2B, 0x28, 0x20)); // night2 parchment
        assert_eq!(b.ansi[7], Color::new(0xE2, 0xDB, 0xC8)); // snow1 cream fg
        assert_eq!(b.ansi[15], Color::new(0xF4, 0xEF, 0xE2)); // snow3 bright cream
        // And green is NOT the muted parchment value any longer.
        assert_ne!(b.ansi[2], Color::new(0xA9, 0xBB, 0x8C));
    }

    #[test]
    fn vellum_agent_accent_is_the_fable_violet_semantic_token() {
        // The agent accent flows through the SEMANTIC `agent` role
        // (= `fable_violet` #B29EC4), not a hand-pinned hex — so a
        // future agent-band retune in ishou propagates on next compile.
        let b = Theme::by_name("vellum").expect("vellum theme");
        let fable_violet = ishou_tokens::VellumPalette::vellum()
            .get(ishou_tokens::SemanticRoles::vellum().agent)
            .expect("fable_violet token");
        assert_eq!(
            b.agent_accent,
            Color::new(fable_violet.r, fable_violet.g, fable_violet.b),
        );
        assert_eq!(b.agent_accent, Color::new(0xB2, 0x9E, 0xC4));
    }

    #[test]
    fn vellum_selection_is_the_byte_exact_violet_glass() {
        // The selection overlay is the byte-exact violet-glass blend
        // product (#3A343E), linearized for the rect pipeline. Round-
        // tripping the linear value back to sRGB recovers the spec hex.
        let b = Theme::by_name("vellum").expect("vellum theme");
        let [r, g, bl, _a] = b.selection_bg;
        let back = ishou_tokens::Linear { r, g, b: bl }.to_srgb();
        assert_eq!(back.hex(), "#3A343E");
    }

    #[test]
    fn vellum_search_matches_resolve_from_born_surfaces() {
        // §5 — the search band flows from the BORN surfaces, never a
        // hand-pinned hex: current = first_light #D7C489 (inverse
        // video), other = search_others #443E2A. A future search-band
        // retune in ishou propagates here on next compile.
        let b = Theme::by_name("vellum").expect("vellum theme");
        let surfaces = ishou_tokens::VellumPalette::vellum().surfaces();
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
        assert_eq!(b.search_current, Color::new(0xD7, 0xC4, 0x89));
        assert_eq!(b.search_others, Color::new(0x44, 0x3E, 0x2A));
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
