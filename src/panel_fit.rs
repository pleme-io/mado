//! `panel_fit` — the typed vocabulary for fitting mado's cell grid onto a
//! (possibly fractionally-scaled) physical display panel WITHOUT a seam.
//!
//! ## The bad state this seals
//!
//! On a compositor-downscaled display (`panel_px / framebuffer_px` = a
//! fraction < 1), the cell grid must land on integer PANEL pixels or the
//! downscale filter rasterizes a periodic row seam — the "weird line bugs".
//! render.rs's `snap_cell_height_px` + `snap_origin_px` make that true *by
//! construction*, proven by `every_row_boundary_lands_on_integer_panel_px…`.
//!
//! But the snap is only as good as the RATIO it is handed, and the ratio
//! comes from a probe that can fail. The old code did
//! `display_scaling_ratio().unwrap_or(1.0)` — a **silent fallback**: a failed
//! probe became "no downscale", the snaps became no-ops, and a genuinely
//! downscaled display seamed with **zero signal**. Worse, that fallback
//! `1.0` was *indistinguishable* from a display that is genuinely
//! integer-scaled — the two states were the same value, so the diagnosis
//! ("is it seaming because the probe failed?") was unrepresentable.
//!
//! [`PanelRatio`] is the seal: the ratio CARRIES its provenance, so
//! "the probe failed" can never masquerade as "genuinely integer-scaled".
//! A seam on an `Unavailable` ratio is now a *diagnosable, surfaced* state
//! (a `tracing::warn` + `mado print-posture`), not a mystery.

use std::fmt;

/// The panel-vs-framebuffer downscale ratio (`panel_px / framebuffer_px`,
/// in `(0, 1]`), carrying HOW it was obtained.
///
/// The numeric ratio drives the seam snap; the VARIANT records whether that
/// number is trustworthy. This is the vocabulary-style seal on the seam's
/// one unguarded input: a probe failure is a distinct, visible state, never
/// a silent `1.0` — and a *genuine* `1.0` is a different value than a
/// *fallback* `1.0`, so the diagnosis is representable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelRatio {
    /// The compositor downscale was probed and found. `Discovered(1.0)`
    /// means "measured, and this display genuinely is NOT downscaled".
    Discovered(f32),
    /// An operator override (`display.downscale_ratio` in mado.yaml).
    Configured(f32),
    /// The probe returned nothing / a nonsense value, OR no probe has run
    /// yet. Rendering falls back to ratio `1.0` (no snap) so it proceeds —
    /// but the state is RECORDED, so a seam here is attributable to "the
    /// ratio is unknown", never silently mistaken for a real `1.0`.
    Unavailable,
}

/// The sane downscale window. A ratio outside `(0.25, 1.0]` is not a real
/// compositor downscale — it is a failed/garbage probe, and snapping
/// against it would corrupt the grid.
const MIN_RATIO: f32 = 0.25;
const MAX_RATIO: f32 = 1.0;

impl PanelRatio {
    /// Build from a probe that yields `Some(ratio)` on success, `None` on
    /// failure — the typed replacement for `.unwrap_or(1.0)`. A probed
    /// ratio is accepted only when finite and inside `(0.25, 1.0]`; a
    /// `> 1.0`, non-finite, or non-positive probe is a *nonsense* result
    /// and is treated as `Unavailable` (a failed probe, never snapped on).
    #[must_use]
    pub fn from_probe(probed: Option<f32>) -> Self {
        match probed {
            Some(r) if r.is_finite() && r > MIN_RATIO && r <= MAX_RATIO => {
                PanelRatio::Discovered(r)
            }
            _ => PanelRatio::Unavailable,
        }
    }

    /// Build from an operator config override, clamped to the sane window.
    #[must_use]
    pub fn from_config(ratio: f32) -> Self {
        PanelRatio::Configured(ratio.clamp(MIN_RATIO, MAX_RATIO))
    }

    /// The numeric ratio to snap against. `Unavailable` yields `1.0` (the
    /// honest no-snap fallback so rendering always proceeds) — but callers
    /// that care whether snapping is trustworthy ask [`is_known`](Self::is_known).
    #[must_use]
    pub fn ratio(self) -> f32 {
        match self {
            PanelRatio::Discovered(r) | PanelRatio::Configured(r) => r,
            PanelRatio::Unavailable => 1.0,
        }
    }

    /// Whether the ratio is trustworthy (probed or configured) rather than
    /// a fallback. A `false` here on a display that seams is the smoking
    /// gun — the seam is a probe failure, not a snap bug.
    #[must_use]
    pub fn is_known(self) -> bool {
        !matches!(self, PanelRatio::Unavailable)
    }

    /// Whether a real fractional downscale is in effect (the seam snap is
    /// doing work). `false` for `Unavailable` and for a genuine `1.0`.
    #[must_use]
    pub fn is_downscaled(self) -> bool {
        self.is_known() && (self.ratio() - 1.0).abs() >= 1.0e-4
    }
}

impl fmt::Display for PanelRatio {
    /// Operator-facing one-liner for `mado print-posture`. `write!` in a
    /// `Display` impl is the sanctioned typed-emission surface (never
    /// `format!` a free string — ★★ TYPED EMISSION).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PanelRatio::Discovered(r) => write!(f, "discovered {r:.4}"),
            PanelRatio::Configured(r) => write!(f, "configured {r:.4}"),
            PanelRatio::Unavailable => write!(
                f,
                "unavailable (probe failed → fell back to 1.0; a downscaled display WILL seam)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_failure_is_a_distinct_recorded_state_not_a_silent_one() {
        let failed = PanelRatio::from_probe(None);
        assert_eq!(failed, PanelRatio::Unavailable);
        assert_eq!(failed.ratio(), 1.0, "falls back to 1.0 so rendering proceeds");
        assert!(!failed.is_known(), "but it is RECORDED unknown, not silent");
        assert!(!failed.is_downscaled());
    }

    #[test]
    fn a_nonsense_probe_is_a_failed_probe_never_snapped_on() {
        // > 1.0, non-finite, or non-positive is not a real downscale.
        assert_eq!(PanelRatio::from_probe(Some(1.5)), PanelRatio::Unavailable);
        assert_eq!(PanelRatio::from_probe(Some(f32::NAN)), PanelRatio::Unavailable);
        assert_eq!(PanelRatio::from_probe(Some(f32::INFINITY)), PanelRatio::Unavailable);
        assert_eq!(PanelRatio::from_probe(Some(0.0)), PanelRatio::Unavailable);
        assert_eq!(PanelRatio::from_probe(Some(-0.8)), PanelRatio::Unavailable);
    }

    #[test]
    fn a_real_fractional_probe_is_discovered_and_downscaled() {
        // The 0.8405 from the 2026-07-11 operator report.
        let r = PanelRatio::from_probe(Some(0.8405));
        assert_eq!(r, PanelRatio::Discovered(0.8405));
        assert!(r.is_known());
        assert!(r.is_downscaled(), "0.84 is a real downscale — the snap does work");
        assert!((r.ratio() - 0.8405).abs() < 1e-6);
    }

    /// THE SEAL — the distinction the old `unwrap_or(1.0)` erased: a display
    /// genuinely at integer scale and a display whose probe FAILED both
    /// yield `ratio() == 1.0`, but they are now DIFFERENT typed states, so
    /// "am I seaming because the probe failed?" is answerable.
    #[test]
    fn genuine_integer_scale_is_distinct_from_a_failed_probe() {
        let genuine = PanelRatio::from_probe(Some(1.0));
        let failed = PanelRatio::from_probe(None);
        assert_eq!(genuine.ratio(), failed.ratio(), "both are 1.0 numerically");
        assert_ne!(genuine, failed, "…but they are DIFFERENT states");
        assert!(genuine.is_known(), "a probed 1.0 is genuine, snapping needs nothing");
        assert!(!failed.is_known(), "a fallback 1.0 is a recorded unknown");
    }

    #[test]
    fn configured_override_is_trusted_and_clamped() {
        let r = PanelRatio::from_config(0.75);
        assert!(r.is_known());
        assert!(r.is_downscaled());
        assert_eq!(r.ratio(), 0.75);
        // A garbage config value clamps into the sane window (it is an
        // explicit operator choice, not a probe — so we honor it, clamped).
        assert_eq!(PanelRatio::from_config(9.0).ratio(), 1.0);
        assert_eq!(PanelRatio::from_config(0.1).ratio(), 0.25);
    }

    #[test]
    fn display_surfaces_the_provenance() {
        assert!(PanelRatio::Discovered(0.84).to_string().contains("discovered"));
        assert!(PanelRatio::Unavailable.to_string().contains("probe failed"));
    }
}
