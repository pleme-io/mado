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
    pub selection_bg: [f32; 4],
    pub ansi: [Color; 16],
}

impl Theme {
    #[must_use]
    pub fn by_name(name: &str) -> Option<&'static Theme> {
        all().iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    #[must_use]
    pub fn available() -> &'static [Theme] {
        all()
    }
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
        selection_bg: scheme.base02.to_array(),
        ansi,
    }
}

fn all() -> &'static [Theme] {
    static THEMES: OnceLock<Vec<Theme>> = OnceLock::new();
    THEMES.get_or_init(|| {
        irodzuki::presets::all()
            .into_iter()
            .map(theme_from_scheme)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_loads() {
        let themes = Theme::available();
        assert_eq!(themes.len(), 8);
        for t in themes {
            assert!(!t.name.is_empty(), "theme has empty name");
        }
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
