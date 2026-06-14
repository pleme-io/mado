//! Runtime auto-detection that feeds `MadoConfig`'s defaults — now declared
//! via the shared [`kanchi`] environment-discovery primitive.
//!
//! mado's zero-config launch should be useful on every machine. Each axis
//! below is one [`kanchi::defaxes!`] line that generates the trio mado (and
//! the rest of the fleet) used to hand-roll per axis:
//!
//!   * `FALLBACK_<AXIS>` — the safe, opinionated value used when detection
//!     fails / isn't applicable; documented + publicly inspectable.
//!   * `detect_<axis>() -> Option<…>` — best-effort runtime probe; `None`
//!     when it can't answer (off-main-thread, headless CI, missing API).
//!   * `detect_<axis>_or_fallback() -> …` — the resolver `MadoConfig` calls.
//!
//! The platform FFI (NSScreen, `sysctl hw.memsize`, `/proc/meminfo`,
//! `AppleInterfaceStyle`) lives ONCE in [`kanchi::probe`] — no re-vendoring.
//! Operators always override the resolved value via `mado.yaml`.

use kanchi::probe;

// Window-dim clamp bounds + screen fraction (fed to the window_dims probe):
// at least 800×600 so mado stays usable on small displays; at most
// 1600×1100 so even an ultrawide doesn't get filled; 60% of the visible
// frame is comfortable alongside other panes.
const WINDOW_DIM_MIN: (u32, u32) = (800, 600);
const WINDOW_DIM_MAX: (u32, u32) = (1600, 1100);
const WINDOW_DIM_FRACTION: f64 = 0.60;

/// Map physical RAM (GiB) to a scrollback line budget — high-memory hosts
/// keep more history, low-memory ones stay lean.
fn scrollback_for_ram(gib: u64) -> u32 {
    match gib {
        0..=7 => 10_000,    // ≤8 GiB: lean (the fleet fallback)
        8..=15 => 50_000,   // 8–16 GiB: comfortable
        16..=31 => 100_000, // 16–32 GiB: generous
        _ => 200_000,       // 32 GiB+: keep a lot
    }
}

/// Preferred-fallback Nerd-font ladder mado probes (first installed wins).
/// Consumed when the fontdb probe is wired into the `font_family` axis.
#[allow(dead_code)]
const FONT_FAMILY_LADDER: &[&str] = &[
    "JetBrainsMono Nerd Font",
    "JetBrainsMono Nerd Font Mono",
    "Iosevka Nerd Font",
    "MesloLGS Nerd Font",
    "FiraCode Nerd Font",
    "Hack Nerd Font",
    "Menlo",
    "Monaco",
    "Consolas",
];

/// Preferred-fallback ladder for the symbol/icon family (ends at the primary
/// Nerd font, which patches the same ranges). Consumed when wired.
#[allow(dead_code)]
const FONT_SYMBOLS_LADDER: &[&str] = &["Symbols Nerd Font Mono", "Symbols Nerd Font", "JetBrainsMono Nerd Font"];

kanchi::defaxes! {
    /// Window dimensions — 60% of the focused display's visible area,
    /// clamped to [800×600, 1600×1100]; falls back to 1200×800. (macOS
    /// NSScreen; `None` off-main-thread / non-macOS.)
    window_dims: (u32, u32) = (1200, 800)
        => || probe::screen_frac(WINDOW_DIM_FRACTION, WINDOW_DIM_MIN, WINDOW_DIM_MAX);

    /// Theme — dark/light system-appearance auto-select is a follow-up (the
    /// probe `kanchi::probe::appearance_dark()` is ready; the theme-name
    /// mapping lands next). Falls back to the prescribed fleet theme,
    /// `vellum` (= `ResolvedTheme::vellum().name`). The convergence test
    /// below pins this string to the ishou fleet theme so a fleet rebrand
    /// can't leave mado on a stale theme name.
    theme: &'static str = "vellum" => kanchi::none;

    /// Highest-preference installed Nerd font — fontdb-enumeration probe is
    /// a follow-up. Falls back to the fleet primary `JetBrainsMono Nerd
    /// Font` (the non-`Mono` variant — matches ghostty's inline-glyph look;
    /// see `FleetDefaults::prescribed`). Pinned to the fleet value by the
    /// convergence guard below.
    font_family: &'static str = "JetBrainsMono Nerd Font" => kanchi::none;

    /// Symbol / Nerd-icon / powerline family — probe wiring is a follow-up.
    font_symbols: &'static str = "Symbols Nerd Font Mono" => kanchi::none;

    /// DPR-aware font size — falls back to the fleet 13pt (ghostty parity;
    /// see `FleetDefaults::prescribed`). Retina 13pt / low-DPI 15pt.
    /// (macOS NSScreen backing scale.) Pinned to the fleet value by the
    /// convergence guard below.
    font_size: f32 = 13.0 => || probe::dpr_font_size(13.0, 15.0);

    /// Window padding — flush against the edge by default (ghostty/alacritty).
    padding: u32 = 0 => kanchi::none;

    /// Scrollback — scaled to physical RAM (sysctl / `/proc/meminfo`).
    /// Feeds the *discovered* tier only: the prescribed default keeps the
    /// unlimited "never lose anything" contract (`config::default_scrollback`
    /// = usize::MAX), which a RAM cap could only downgrade.
    scrollback_lines: u32 = 10_000 => || probe::total_ram_gib().map(scrollback_for_ram);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `detect_window_dims_or_fallback` ALWAYS returns a usable value —
    /// that's what `MadoConfig::default()` calls.
    #[test]
    fn detect_or_fallback_always_returns_usable() {
        let (w, h) = detect_window_dims_or_fallback();
        assert!(w >= WINDOW_DIM_MIN.0 || (w, h) == FALLBACK_WINDOW_DIMS);
        assert!(h >= WINDOW_DIM_MIN.1 || (w, h) == FALLBACK_WINDOW_DIMS);
        assert!(w <= 4000 && h <= 3000);
    }

    /// The documented FALLBACK is honored when detection returns None (test
    /// runners hit this — off-main-thread).
    #[test]
    fn fallback_constant_is_used_when_detection_returns_none() {
        if detect_window_dims().is_none() {
            assert_eq!(detect_window_dims_or_fallback(), FALLBACK_WINDOW_DIMS);
        }
    }

    /// Detection-supplied dims are clamped to the configured min/max.
    #[test]
    fn detection_respects_min_max_bounds() {
        if let Some((w, h)) = detect_window_dims() {
            assert!(w >= WINDOW_DIM_MIN.0 && w <= WINDOW_DIM_MAX.0);
            assert!(h >= WINDOW_DIM_MIN.1 && h <= WINDOW_DIM_MAX.1);
        }
    }

    #[test]
    fn fallback_window_dims_pinned() {
        assert_eq!(FALLBACK_WINDOW_DIMS, (1200, 800));
    }
    #[test]
    fn fallback_theme_pinned() {
        // Pinned to the prescribed fleet theme by NAME (not a stale
        // literal) — a fleet rebrand in ishou propagates here on the
        // next compile, and this test fails until FALLBACK_THEME is
        // updated to match.
        assert_eq!(FALLBACK_THEME, "vellum");
        assert_eq!(
            FALLBACK_THEME,
            ishou_tokens::ResolvedTheme::vellum().name,
        );
        assert_eq!(
            FALLBACK_THEME,
            ishou_tokens::FleetTheme::prescribed_default().resolve().name,
        );
    }
    #[test]
    fn fallback_font_family_pinned() {
        assert_eq!(FALLBACK_FONT_FAMILY, "JetBrainsMono Nerd Font");
    }
    #[test]
    fn fallback_font_size_pinned() {
        assert!((FALLBACK_FONT_SIZE - 13.0).abs() < 0.001);
    }
    #[test]
    fn fallback_padding_pinned() {
        assert_eq!(FALLBACK_PADDING, 0);
    }
    #[test]
    fn fallback_scrollback_pinned() {
        assert_eq!(FALLBACK_SCROLLBACK_LINES, 10_000);
    }

    #[test]
    fn detect_theme_or_fallback_always_returns_a_theme() {
        assert!(!detect_theme_or_fallback().is_empty());
    }
    #[test]
    fn detect_font_family_or_fallback_always_returns_a_family() {
        assert!(!detect_font_family_or_fallback().is_empty());
    }

    /// RAM→scrollback tiers are monotonic + bounded by the fallback floor.
    #[test]
    fn scrollback_tiers_monotonic() {
        assert_eq!(scrollback_for_ram(4), FALLBACK_SCROLLBACK_LINES);
        assert!(scrollback_for_ram(12) >= scrollback_for_ram(4));
        assert!(scrollback_for_ram(24) >= scrollback_for_ram(12));
        assert!(scrollback_for_ram(64) >= scrollback_for_ram(24));
    }
}

// ── Fleet convergence guard ──────────────────────────────────────
//
// Every FALLBACK_* generated above MUST match the corresponding field on
// `ishou_tokens::FleetDefaults::prescribed()`. If they drift, mado's defaults
// silently diverge from the rest of the fleet. The test enforces convergence
// at test-suite-compile time; a future fleet rebrand touches
// `FleetDefaults::prescribed()` once and this fails until the matching
// FALLBACK_* is updated.
#[cfg(test)]
mod fleet_convergence_tests {
    use super::*;

    #[test]
    fn fallback_defaults_converge_with_fleet() {
        ishou_tokens::convergence::Guard::for_app("mado")
            .expect_font_family(FALLBACK_FONT_FAMILY)
            .expect_font_size(FALLBACK_FONT_SIZE)
            .expect_padding(FALLBACK_PADDING)
            .expect_scrollback_lines(FALLBACK_SCROLLBACK_LINES as usize)
            .run();
    }
}
