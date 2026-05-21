//! Runtime auto-detection helpers that feed `MadoConfig`'s defaults.
//!
//! The philosophy: mado's zero-config launch should be useful on
//! every machine. Hardcoded magic numbers (window 1200×800, theme
//! "nord", etc.) are progressively replaced with detect-then-default
//! pairs here.
//!
//! Each axis exposes TWO things:
//!
//!   1. A typed `FALLBACK_*` constant — the safe, opinionated value
//!      used when detection fails OR isn't applicable to the
//!      platform. Documented + publicly inspectable so operators
//!      see exactly what they'd land on.
//!   2. A `detect_*() -> Option<...>` function — best-effort
//!      runtime query, returns `None` (not a default) when it
//!      can't answer authoritatively (off-main-thread, headless
//!      CI, missing platform API, implausible value).
//!
//! The "or fallback" wrapper (`detect_*_or_fallback()`) is the
//! one `MadoConfig` calls — it makes the resolution explicit:
//! "try detection; fall back to FALLBACK_*". Operators can
//! always override the resolved value via `mado.yaml` per the
//! usual config layering.
//!
//! Per task #147 in pleme-io/CLAUDE.md.

// ── Window dimensions ────────────────────────────────────────────

/// Safe-fallback window dimensions when display detection isn't
/// available (CI / off-main-thread / non-macOS / implausibly tiny
/// virtual display). The original hardcoded mado default; operators
/// can override per-config via `window.width` / `window.height`.
pub const FALLBACK_WINDOW_DIMS: (u32, u32) = (1200, 800);

/// Bounds the detected window comes from: at least 800×600 so it
/// stays usable on small portable displays; at most 1600×1100 so
/// even on an ultrawide it doesn't fill the screen.
const WINDOW_DIM_MIN: (u32, u32) = (800, 600);
const WINDOW_DIM_MAX: (u32, u32) = (1600, 1100);

/// Fraction of the focused display's visible area mado claims by
/// default. 0.60 = 60% — comfortable for a single mado window
/// alongside other panes on the same monitor.
const WINDOW_DIM_FRACTION: f64 = 0.60;

/// Detect the focused display's preferred window dimensions.
///
/// Returns `None` when:
/// * Detection is not implemented for the platform
/// * Called off the main thread (macOS NSScreen requires main)
/// * The detected display has an implausibly small visible frame
///
/// Caller decides what to do with the `None` — typically pass it
/// to [`detect_window_dims_or_fallback`].
#[must_use]
pub fn detect_window_dims() -> Option<(u32, u32)> {
    detect_window_dims_impl()
}

/// Try detection; if it returns `None`, use [`FALLBACK_WINDOW_DIMS`].
/// This is the function `MadoConfig::default()` calls.
#[must_use]
pub fn detect_window_dims_or_fallback() -> (u32, u32) {
    detect_window_dims().unwrap_or(FALLBACK_WINDOW_DIMS)
}

#[cfg(target_os = "macos")]
fn detect_window_dims_impl() -> Option<(u32, u32)> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    let mtm = MainThreadMarker::new()?;
    let screen = NSScreen::mainScreen(mtm)?;
    let frame = unsafe { screen.visibleFrame() };
    let visible_w = frame.size.width.max(0.0);
    let visible_h = frame.size.height.max(0.0);
    if visible_w < 200.0 || visible_h < 200.0 {
        return None;
    }
    let proposed_w = (visible_w * WINDOW_DIM_FRACTION).round() as u32;
    let proposed_h = (visible_h * WINDOW_DIM_FRACTION).round() as u32;
    Some((
        proposed_w.clamp(WINDOW_DIM_MIN.0, WINDOW_DIM_MAX.0),
        proposed_h.clamp(WINDOW_DIM_MIN.1, WINDOW_DIM_MAX.1),
    ))
}

#[cfg(not(target_os = "macos"))]
fn detect_window_dims_impl() -> Option<(u32, u32)> {
    // Future: x11/wayland + windows GetSystemMetrics paths. For M1
    // we ship macOS-only and fall back to FALLBACK_WINDOW_DIMS
    // elsewhere — pleme-io fleet is macOS-first today.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// detect_window_dims_or_fallback ALWAYS returns a usable
    /// value — that's the function MadoConfig::default() calls.
    /// Range bounded by either the detection clamp OR the safe
    /// fallback.
    #[test]
    fn detect_or_fallback_always_returns_usable() {
        let (w, h) = detect_window_dims_or_fallback();
        assert!(w >= WINDOW_DIM_MIN.0 || (w, h) == FALLBACK_WINDOW_DIMS);
        assert!(h >= WINDOW_DIM_MIN.1 || (w, h) == FALLBACK_WINDOW_DIMS);
        assert!(w <= 4000);
        assert!(h <= 3000);
    }

    /// FALLBACK constant is honored explicitly when detection
    /// returns None. Test runners hit this case (off-main-thread).
    #[test]
    fn fallback_constant_is_used_when_detection_returns_none() {
        // detect_window_dims() is None off-main-thread on macOS +
        // None on every non-macOS platform. The wrapper resolves
        // to exactly FALLBACK_WINDOW_DIMS in those cases.
        if detect_window_dims().is_none() {
            assert_eq!(detect_window_dims_or_fallback(), FALLBACK_WINDOW_DIMS);
        }
    }

    /// Bounds: detection-supplied dims are clamped to the
    /// configured min/max so even on a 6K ultrawide we don't
    /// explode past a comfortable size.
    #[test]
    fn detection_respects_min_max_bounds() {
        if let Some((w, h)) = detect_window_dims() {
            assert!(w >= WINDOW_DIM_MIN.0);
            assert!(h >= WINDOW_DIM_MIN.1);
            assert!(w <= WINDOW_DIM_MAX.0);
            assert!(h <= WINDOW_DIM_MAX.1);
        }
    }

    /// The fallback constant is the documented + opinionated
    /// pre-detection default. Pinning so a future change is a
    /// deliberate operator-visible diff.
    #[test]
    fn fallback_window_dims_pinned() {
        assert_eq!(FALLBACK_WINDOW_DIMS, (1200, 800));
    }

    #[test]
    fn fallback_theme_pinned() {
        assert_eq!(FALLBACK_THEME, "nord");
    }

    #[test]
    fn fallback_font_family_pinned() {
        assert_eq!(FALLBACK_FONT_FAMILY, "JetBrainsMono Nerd Font Mono");
    }

    #[test]
    fn fallback_font_size_pinned() {
        assert!((FALLBACK_FONT_SIZE - 14.0).abs() < 0.001);
    }

    #[test]
    fn fallback_padding_pinned() {
        assert_eq!(FALLBACK_PADDING, 0);
    }

    #[test]
    fn fallback_scrollback_pinned() {
        assert_eq!(FALLBACK_SCROLLBACK_LINES, 10_000);
    }

    /// detect_theme_or_fallback returns SOMETHING — either the
    /// detected macOS appearance or FALLBACK_THEME.
    #[test]
    fn detect_theme_or_fallback_always_returns_a_theme() {
        let t = detect_theme_or_fallback();
        assert!(!t.is_empty());
    }

    #[test]
    fn detect_font_family_or_fallback_always_returns_a_family() {
        let f = detect_font_family_or_fallback();
        assert!(!f.is_empty());
    }
}

// ── Theme (light/dark detection) ─────────────────────────────────

/// Safe-fallback theme when display appearance can't be detected.
/// Nord is the operator-blessed default — dark, high-contrast,
/// terminal-friendly.
pub const FALLBACK_THEME: &str = "nord";

/// Detect the host's appearance (light/dark) and pick a
/// terminal-friendly theme name accordingly. Returns None when
/// detection isn't available; caller falls back to FALLBACK_THEME.
///
/// M1 returns None unconditionally — the appearance-detection FFI
/// (macOS NSDistributedNotificationCenter watcher / xdg-portal
/// dbus query) is a follow-up. The pattern + tests are in place
/// so the implementation is a drop-in.
#[must_use]
pub fn detect_theme() -> Option<&'static str> {
    None
}

#[must_use]
pub fn detect_theme_or_fallback() -> &'static str {
    detect_theme().unwrap_or(FALLBACK_THEME)
}

// ── Font family (Nerd-Font probe) ────────────────────────────────

/// Safe-fallback font family. JetBrainsMono Nerd Font Mono ships
/// in the pleme-io stack profile + has the widest icon coverage.
pub const FALLBACK_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// Preferred-fallback ladder mado probes. The first one found
/// installed on the host wins. Operators with a strong opinion
/// just set `font_family` in mado.yaml.
#[allow(dead_code)] // referenced by detect_font_family when wired
const FONT_FAMILY_LADDER: &[&str] = &[
    "JetBrainsMono Nerd Font Mono",
    "Iosevka Nerd Font",
    "MesloLGS Nerd Font",
    "FiraCode Nerd Font",
    "Hack Nerd Font",
    // System mono fallback — present everywhere.
    "Menlo",
    "Monaco",
    "Consolas",
];

/// Detect the highest-preference installed Nerd Font (or system
/// mono fallback). M1 returns None unconditionally — fontdb
/// enumeration is loaded lazily for performance; this would need
/// a small synchronous probe path. Wired but not yet active.
#[must_use]
pub fn detect_font_family() -> Option<&'static str> {
    None
}

#[must_use]
pub fn detect_font_family_or_fallback() -> &'static str {
    detect_font_family().unwrap_or(FALLBACK_FONT_FAMILY)
}

// ── Font size (DPR-aware) ────────────────────────────────────────

/// Safe-fallback font size at 14pt — readable on standard-density
/// displays, comfortable on HiDPI without operator intervention.
pub const FALLBACK_FONT_SIZE: f32 = 14.0;

/// Detect a font size scaled to the focused display's DPR.
/// HiDPI Retina = 14pt (operating system handles the upscale);
/// non-Retina low-DPI = bump to 16pt for readability. Returns
/// None when DPR can't be probed.
#[must_use]
pub fn detect_font_size() -> Option<f32> {
    None
}

#[must_use]
pub fn detect_font_size_or_fallback() -> f32 {
    detect_font_size().unwrap_or(FALLBACK_FONT_SIZE)
}

// ── Padding (operator-facing default) ────────────────────────────

/// Safe-fallback padding. Zero = first cell flush against window
/// edge; matches ghostty / alacritty defaults.
pub const FALLBACK_PADDING: u32 = 0;

#[must_use]
pub fn detect_padding() -> Option<u32> {
    None
}

#[must_use]
pub fn detect_padding_or_fallback() -> u32 {
    detect_padding().unwrap_or(FALLBACK_PADDING)
}

// ── Scrollback lines ─────────────────────────────────────────────

/// Safe-fallback scrollback. 10k lines = ~5MB of grid memory at
/// 80×24; small enough to keep memory low, large enough that
/// `grep` over yesterday's output works.
pub const FALLBACK_SCROLLBACK_LINES: u32 = 10_000;

// ── Fleet convergence guard ──────────────────────────────────────
//
// Every FALLBACK_* constant above MUST match the corresponding
// field on `ishou_tokens::FleetDefaults::prescribed()`. If they
// drift, mado's defaults silently diverge from the rest of the
// fleet (escriba, namimado, hibiki, hikki, etc.). The tests below
// enforce convergence at compile-time-of-the-test-suite.
//
// When the next "fleet rebrand" comes — bump font, switch theme,
// raise default scrollback — touch `FleetDefaults::prescribed()`
// once; this test fails until the matching FALLBACK_* is updated
// to match. Drift becomes a build failure, not a silent slip.

#[cfg(test)]
mod fleet_convergence_tests {
    use super::*;
    use ishou_tokens::FleetDefaults;

    #[test]
    fn fallback_font_family_matches_fleet_defaults() {
        let fd = FleetDefaults::prescribed();
        assert_eq!(
            FALLBACK_FONT_FAMILY, fd.font_family,
            "mado's FALLBACK_FONT_FAMILY drifted from FleetDefaults::prescribed().font_family — \
             pick ONE source. Per the configuration prime directive, FleetDefaults wins."
        );
    }

    #[test]
    fn fallback_font_size_matches_fleet_defaults() {
        let fd = FleetDefaults::prescribed();
        assert!(
            (FALLBACK_FONT_SIZE - fd.font_size).abs() < 0.001,
            "FALLBACK_FONT_SIZE ({}) drifted from FleetDefaults ({}).",
            FALLBACK_FONT_SIZE, fd.font_size
        );
    }

    #[test]
    fn fallback_padding_matches_fleet_defaults() {
        let fd = FleetDefaults::prescribed();
        assert_eq!(
            FALLBACK_PADDING, fd.padding,
            "FALLBACK_PADDING drifted from FleetDefaults::prescribed().padding"
        );
    }

    #[test]
    fn fallback_scrollback_matches_fleet_defaults() {
        let fd = FleetDefaults::prescribed();
        assert_eq!(
            FALLBACK_SCROLLBACK_LINES as usize, fd.scrollback_lines,
            "FALLBACK_SCROLLBACK_LINES drifted from FleetDefaults::prescribed().scrollback_lines"
        );
    }
}

/// Detect scrollback based on available RAM (high-memory machines
/// get more). M1 returns None; the RAM probe is a follow-up.
#[must_use]
pub fn detect_scrollback_lines() -> Option<u32> {
    None
}

#[must_use]
pub fn detect_scrollback_lines_or_fallback() -> u32 {
    detect_scrollback_lines().unwrap_or(FALLBACK_SCROLLBACK_LINES)
}
