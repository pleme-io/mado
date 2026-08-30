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
/// Why a panel ratio could not be resolved.
///
/// ★ Four causes, not one. A bare `Unavailable` made them the same answer, and
/// they have opposite remedies: `NoPhysicalSize` is a compositor bug worth
/// filing, `NoOutputYet` is a race worth retrying, `OutsideSaneWindow` is a
/// garbage probe worth distrusting, and `NotProbed` means nobody asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatioUnknown {
    /// `wl_output` reported a physical size of 0x0, so no ratio is derivable.
    /// Measured on plo: omoya publishes exactly this despite a valid EDID.
    NoPhysicalSize,
    /// No output has been advertised yet — a startup race, not a defect.
    NoOutputYet,
    /// The probe produced a value outside `(0.25, 1.0]`, i.e. not a real
    /// compositor downscale. Snapping on it would corrupt the grid.
    OutsideSaneWindow,
    /// No probe has run.
    NotProbed,
}

impl RatioUnknown {
    /// A short operator-facing phrase.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoPhysicalSize => "wl_output reports a 0x0 physical size (compositor bug)",
            Self::NoOutputYet => "no wl_output yet (startup race, retry)",
            Self::OutsideSaneWindow => "probe outside the sane window (0.25, 1.0]",
            Self::NotProbed => "no probe has run",
        }
    }
}

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
    ///
    /// ── ★ CARRIES ITS CAUSE (2026-08-29) ─────────────────────────────────
    ///
    /// This was a bare `Unavailable`, and FOUR different failures collapsed
    /// into it: the compositor published a 0x0 physical size, no `wl_output`
    /// had arrived yet, the probe returned a ratio outside the sane window, or
    /// no probe had run at all. On plo the real cause was the first — omoya
    /// publishes `PhysicalProperties { size: 0x0 }` despite an EDID reading
    /// 54cm x 30cm — and the operator-visible line was only
    ///
    ///     panel_ratio: unavailable (probe failed → fell back to 1.0;
    ///                               a downscaled display WILL seam)
    ///
    /// which names a symptom and no cause. Three of the four causes are
    /// somebody else's bug and one is a race; telling them apart is the whole
    /// difference between "fix omoya's wl_output" and "wait for a frame".
    Unavailable(RatioUnknown),
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
            // ★ The two failures reaching here are DIFFERENT and now say so:
            // `None` means nobody produced a value; a `Some` that fell through
            // the guard means the probe produced one and it was nonsense.
            None => PanelRatio::Unavailable(RatioUnknown::NotProbed),
            Some(_) => PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow),
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
            PanelRatio::Unavailable(_) => 1.0,
        }
    }

    /// Whether the ratio is trustworthy (probed or configured) rather than
    /// a fallback. A `false` here on a display that seams is the smoking
    /// gun — the seam is a probe failure, not a snap bug.
    #[must_use]
    pub fn is_known(self) -> bool {
        !matches!(self, PanelRatio::Unavailable(_))
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
            // ★ NAMES THE CAUSE. The old text said only "probe failed",
            // which is true of four different situations with opposite
            // remedies -- a compositor bug, a startup race, a garbage probe,
            // and nobody having asked. On plo the real cause was the first,
            // and the operator-visible line could not say so.
            PanelRatio::Unavailable(why) => write!(
                f,
                "unavailable: {} → fell back to 1.0; a downscaled display WILL seam",
                why.as_str()
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
        assert_eq!(failed, PanelRatio::Unavailable(RatioUnknown::NotProbed));
        assert_eq!(
            failed.ratio(),
            1.0,
            "falls back to 1.0 so rendering proceeds"
        );
        assert!(!failed.is_known(), "but it is RECORDED unknown, not silent");
        assert!(!failed.is_downscaled());
    }

    #[test]
    fn a_nonsense_probe_is_a_failed_probe_never_snapped_on() {
        // > 1.0, non-finite, or non-positive is not a real downscale.
        assert_eq!(
            PanelRatio::from_probe(Some(1.5)),
            PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow)
        );
        assert_eq!(
            PanelRatio::from_probe(Some(f32::NAN)),
            PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow)
        );
        assert_eq!(
            PanelRatio::from_probe(Some(f32::INFINITY)),
            PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow)
        );
        assert_eq!(
            PanelRatio::from_probe(Some(0.0)),
            PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow)
        );
        assert_eq!(
            PanelRatio::from_probe(Some(-0.8)),
            PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow)
        );
    }

    #[test]
    fn a_real_fractional_probe_is_discovered_and_downscaled() {
        // The 0.8405 from the 2026-07-11 operator report.
        let r = PanelRatio::from_probe(Some(0.8405));
        assert_eq!(r, PanelRatio::Discovered(0.8405));
        assert!(r.is_known());
        assert!(
            r.is_downscaled(),
            "0.84 is a real downscale — the snap does work"
        );
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
        assert!(
            genuine.is_known(),
            "a probed 1.0 is genuine, snapping needs nothing"
        );
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
        assert!(
            PanelRatio::Discovered(0.84)
                .to_string()
                .contains("discovered")
        );
        // ★ The string now names the CAUSE instead of saying "probe failed",
        // which was true of four different situations.
        assert!(
            PanelRatio::Unavailable(RatioUnknown::NotProbed)
                .to_string()
                .contains("no probe has run")
        );
    }
}

#[cfg(test)]
mod ratio_cause_tests {
    use super::{PanelRatio, RatioUnknown};

    /// ★ FOUR CAUSES, ONE SYMPTOM (plo, 2026-08-29).
    ///
    /// `Unavailable` was bare, so a compositor bug, a startup race, a garbage
    /// probe and "nobody asked" produced the identical operator-visible line:
    ///
    ///     panel_ratio: unavailable (probe failed → fell back to 1.0)
    ///
    /// On plo the real cause was omoya publishing `wl_output` physical size
    /// 0x0 despite an EDID of 54cm x 30cm. The remedies differ completely —
    /// file a compositor bug, retry, distrust the probe, or run one — and the
    /// line named none of them.
    #[test]
    fn the_reason_reaches_the_operator_visible_string() {
        let s = PanelRatio::Unavailable(RatioUnknown::NoPhysicalSize).to_string();
        assert!(s.contains("0x0"), "must name the actual cause: {s}");
        assert!(s.contains("compositor bug"), "{s}");
    }

    /// Anti-vacuity: the four causes must not render the same. A single shared
    /// string would pass the test above while restoring the exact defect.
    #[test]
    fn the_four_causes_are_distinguishable() {
        let all = [
            RatioUnknown::NoPhysicalSize,
            RatioUnknown::NoOutputYet,
            RatioUnknown::OutsideSaneWindow,
            RatioUnknown::NotProbed,
        ];
        let rendered: std::collections::BTreeSet<&str> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            rendered.len(),
            all.len(),
            "causes must differ: {rendered:?}"
        );
    }

    /// A probe that produced NOTHING and one that produced NONSENSE are
    /// different failures — the first means nobody measured, the second means
    /// the measurement is untrustworthy.
    #[test]
    fn no_probe_is_not_the_same_as_a_bad_probe() {
        assert_eq!(
            PanelRatio::from_probe(None),
            PanelRatio::Unavailable(RatioUnknown::NotProbed)
        );
        assert_eq!(
            PanelRatio::from_probe(Some(3.0)),
            PanelRatio::Unavailable(RatioUnknown::OutsideSaneWindow),
            "a ratio above 1.0 is a garbage probe, not an absent one"
        );
        // And a good probe is still accepted.
        assert_eq!(
            PanelRatio::from_probe(Some(0.8)),
            PanelRatio::Discovered(0.8)
        );
    }
}
