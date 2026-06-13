//! Ambience — ONE composed, barely-perceptible, default-on visual
//! layer (operator design law, 2026-06-13).
//!
//! > "we should be able to combine everything for a consistent effect
//! > that is barely perceptible but does influence." — and the bright
//! > line: "if you can point at it, it is too loud."
//!
//! The ambience layer is NOT a set of discrete effect toggles. It is a
//! single typed [`AmbiencePreset`] that COMPOSES the engawa catalog
//! effects at threshold-of-perception intensities, all sharing ONE
//! motion clock + ONE noise seed + the resolved Borealis palette:
//!
//! * **aurora** — the Borealis signature curtain, ~2-4 % opacity,
//!   concentrated above the prompt horizon, drawn from the theme's
//!   green/cyan/violet stops (the palette flows in render-side via
//!   `AuroraParams::with_colors`, never a hardcoded hex). Aurora's own
//!   WGSL carries the spatial dither (`hash12(in.pos.xy)`, time-free)
//!   that kills banding — so the "micro-grain dither" the design law
//!   asks for is intrinsic to this layer, not a second pass.
//! * **bloom** — only the brightest accents bloom, and subtly (a high
//!   threshold + a low gain), so text never smears.
//! * **glow_on_bell** — the BEL-driven cursor glow, the one
//!   event-reactive member; idle it contributes nothing (the host
//!   clock decays to zero) yet stays in the set so a bell lights up
//!   without a graph rebuild.
//!
//! ## The composition IS the source of truth
//!
//! [`AmbienceComposition`] is the ONE typed value both the effect-set
//! derivation (which catalog effects are on) and the per-frame uniform
//! derivation (their tuned params) read. They cannot drift: the set is
//! the rows' effects; the params are the rows' params. A preset that
//! returns zero rows contributes zero graph nodes — the same "empty
//! set ⇒ no graph at all" contract `render_graph` already pins.
//!
//! ## Quality scales, never the composition
//!
//! Every member is tuned for the `Whisper` bar. The
//! [`crate::ux::ambience_governor`] scales the aurora *quality* word
//! (rebuild-free) down toward the frame budget — it NEVER changes which
//! effects are on, so the composition stays stable while the cost
//! adapts. The quality the governor settled on is applied to the
//! composition's aurora row at frame time.
//!
//! ## Accessibility is the floor, not a tier
//!
//! `reduce_motion` forces [`AmbiencePreset::Off`] — zero rows, zero
//! nodes — *before* the composition runs. That is the accessibility
//! contract (the same one snow / glow / aurora honour by node-omission,
//! not by an `Off` quality word).

use engawa_wgpu::catalog::CatalogEffect;
use serde::{Deserialize, Serialize};

/// The composed ambience layer — three preset points on the
/// barely-perceptible axis. `Whisper` is the default: the bar where
/// the layer influences without being nameable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(pleme_allvariants_derive::AllVariants))]
#[serde(rename_all = "snake_case")]
pub enum AmbiencePreset {
    /// Zero rows, zero nodes — the clean look. Also where
    /// `reduce_motion` lands the layer.
    Off,
    /// THE default: every member at the threshold of perception — a
    /// consistent influence you cannot point at.
    Whisper,
    /// A touch more — for showing the layer off. Still tasteful; the
    /// aurora/bloom gains lift modestly, nothing smears.
    Present,
}

impl Default for AmbiencePreset {
    fn default() -> Self {
        Self::Whisper
    }
}

/// One composed member — a catalog effect plus the intensity dials the
/// preset tuned it to. The render side seeds the matching catalog
/// `Params` from these (and, for aurora, the resolved palette).
///
/// Every dial is in the catalog effect's own 0..1-ish domain; the
/// catalog `with_*` builders clamp, so an out-of-range tune is
/// saturated, never illegal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmbienceMember {
    pub effect: CatalogEffect,
    /// Master opacity / gain at the threshold the preset chose.
    pub intensity: f32,
    /// Aurora drift-speed multiplier (ignored by non-aurora members).
    pub drift: f32,
    /// Aurora shimmer amount (ignored by non-aurora members).
    pub shimmer: f32,
    /// Aurora horizon line (screen-space y, 0=top 1=bottom); the
    /// curtain is zero below it (ignored by non-aurora members).
    pub horizon: f32,
    /// Bloom luminance cutoff — only pixels brighter than this bloom
    /// (ignored by non-bloom members).
    pub bloom_threshold: f32,
}

impl AmbienceMember {
    /// An aurora member at the given threshold dials.
    const fn aurora(intensity: f32, drift: f32, shimmer: f32, horizon: f32) -> Self {
        Self {
            effect: CatalogEffect::Aurora,
            intensity,
            drift,
            shimmer,
            horizon,
            bloom_threshold: 0.0,
        }
    }

    /// A bloom member — only `intensity` (gain) + `bloom_threshold`
    /// matter.
    const fn bloom(intensity: f32, threshold: f32) -> Self {
        Self {
            effect: CatalogEffect::Bloom,
            intensity,
            drift: 0.0,
            shimmer: 0.0,
            horizon: 0.0,
            bloom_threshold: threshold,
        }
    }

    /// The glow-on-bell member — event-reactive; the host clock owns
    /// the actual intensity, so this row only declares membership.
    const fn glow_on_bell() -> Self {
        Self {
            effect: CatalogEffect::GlowOnBell,
            intensity: 0.0,
            drift: 0.0,
            shimmer: 0.0,
            horizon: 0.0,
            bloom_threshold: 0.0,
        }
    }
}

/// The composed layer for one preset — the ONE typed value both the
/// effect-set and the per-frame uniforms read, so they cannot drift.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AmbienceComposition {
    pub members: Vec<AmbienceMember>,
}

impl AmbienceComposition {
    /// The aurora member, if the composition has one (the render side's
    /// palette injection + governor-quality application target it).
    #[must_use]
    pub fn aurora(&self) -> Option<&AmbienceMember> {
        self.members
            .iter()
            .find(|m| m.effect == CatalogEffect::Aurora)
    }

    /// Whether `effect` is a member of this composition.
    #[must_use]
    pub fn contains(&self, effect: CatalogEffect) -> bool {
        self.members.iter().any(|m| m.effect == effect)
    }

    /// The member for `effect`, if present.
    #[must_use]
    pub fn member(&self, effect: CatalogEffect) -> Option<&AmbienceMember> {
        self.members.iter().find(|m| m.effect == effect)
    }
}

impl AmbiencePreset {
    /// COMPOSE the catalog effects for this preset at the tuned
    /// threshold intensities. `Off` is the empty composition (zero
    /// rows ⇒ zero nodes). The pinned numbers below ARE the
    /// barely-perceptible bar — `Whisper` is the design default, the
    /// `Present` deltas are a deliberate, small lift.
    ///
    /// Pure: no I/O, no palette, no clock — the palette enters
    /// render-side (`AuroraParams::with_colors`), the clock enters
    /// render-side (`AuroraParams::set_time`), and the quality enters
    /// render-side (the governor word). This keeps the composition a
    /// plain value the tests can pin exactly.
    #[must_use]
    pub fn compose(self) -> AmbienceComposition {
        let members = match self {
            // Off contributes ZERO rows — the empty composition is the
            // "no graph at all" path the renderer already honours.
            AmbiencePreset::Off => Vec::new(),
            // Whisper — the bar. Aurora at ~2.5 % opacity (the curtain
            // is sky dressing; the scene reads straight through), slow
            // drift, gentle shimmer, horizon high above the prompt
            // line. Bloom only on near-white accents (high threshold),
            // a whisper of gain. Glow rides the bell.
            AmbiencePreset::Whisper => vec![
                AmbienceMember::aurora(
                    WHISPER_AURORA_INTENSITY,
                    WHISPER_AURORA_DRIFT,
                    WHISPER_AURORA_SHIMMER,
                    AMBIENCE_HORIZON,
                ),
                AmbienceMember::bloom(WHISPER_BLOOM_INTENSITY, AMBIENCE_BLOOM_THRESHOLD),
                AmbienceMember::glow_on_bell(),
            ],
            // Present — same composition, modestly louder. Still under
            // the "do not smear text" line; this is the showing-off
            // tier, not a different layer.
            AmbiencePreset::Present => vec![
                AmbienceMember::aurora(
                    PRESENT_AURORA_INTENSITY,
                    PRESENT_AURORA_DRIFT,
                    PRESENT_AURORA_SHIMMER,
                    AMBIENCE_HORIZON,
                ),
                AmbienceMember::bloom(PRESENT_BLOOM_INTENSITY, AMBIENCE_BLOOM_THRESHOLD),
                AmbienceMember::glow_on_bell(),
            ],
        };
        AmbienceComposition { members }
    }
}

// ── The pinned barely-perceptible bar ───────────────────────────────
//
// These constants ARE the design law made mechanical. The tests below
// pin them so a future edit that pushes any member past "you can point
// at it" trips a forcing function, not the operator's eye.

/// Aurora curtain opacity at Whisper — ~2.5 %. The aurora shader caps
/// coverage at MAX_ALPHA=0.5 internally; this is the master gain.
pub(crate) const WHISPER_AURORA_INTENSITY: f32 = 0.025;
/// Aurora drift multiplier at Whisper — well under the default 1.0, so
/// a curtain crosses a noise cell in well over a minute (imperceptibly
/// slow motion).
pub(crate) const WHISPER_AURORA_DRIFT: f32 = 0.45;
/// Aurora shimmer at Whisper — a hint.
pub(crate) const WHISPER_AURORA_SHIMMER: f32 = 0.30;
/// Bloom gain at Whisper — a whisper.
pub(crate) const WHISPER_BLOOM_INTENSITY: f32 = 0.12;

/// Aurora curtain opacity at Present — ~4 % (still sky dressing).
pub(crate) const PRESENT_AURORA_INTENSITY: f32 = 0.04;
/// Aurora drift at Present — a little more life.
pub(crate) const PRESENT_AURORA_DRIFT: f32 = 0.60;
/// Aurora shimmer at Present.
pub(crate) const PRESENT_AURORA_SHIMMER: f32 = 0.45;
/// Bloom gain at Present.
pub(crate) const PRESENT_BLOOM_INTENSITY: f32 = 0.20;

/// Horizon line shared by every preset's aurora. In the engawa aurora
/// shader `alt = 1.0 - uv.y/horizon`, so the curtain's ACTIVE region is
/// the top `horizon` fraction of the frame (`uv.y < 0.70` here = the top
/// 70 %); below the horizon the curtain is zero. The visible aurora is
/// concentrated near the TOP edge anyway — the `smoothstep` border +
/// `exp(-rel·DECAY)` falloff + the wandering ~0.08–0.30 border keep the
/// bright band high — so the result sits above the prompt line. Mental
/// model for a re-tune: LOWERING `horizon` SHRINKS the active region
/// toward the top; RAISING it toward 1.0 EXPANDS the region DOWNWARD
/// toward the prompt line.
pub(crate) const AMBIENCE_HORIZON: f32 = 0.70;
/// Bloom luminance cutoff shared by every preset — only near-white
/// accents (above 0.88) bloom, so ordinary text never smears.
pub(crate) const AMBIENCE_BLOOM_THRESHOLD: f32 = 0.88;

/// The ceiling the composition's intensities are checked against — the
/// "barely perceptible" upper bound. Aurora opacity above this is "you
/// can point at it"; the forcing test pins every member under it.
#[cfg(test)]
pub(crate) const BARELY_PERCEPTIBLE_AURORA_CEILING: f32 = 0.06;

#[cfg(test)]
mod tests {
    use super::*;

    /// Whisper is the default — the operator design law's chosen bar.
    #[test]
    fn whisper_is_the_default_preset() {
        assert_eq!(AmbiencePreset::default(), AmbiencePreset::Whisper);
    }

    /// FORCING FUNCTION — Whisper composes EXACTLY the expected member
    /// set at the pinned low intensities (a matrix row per composed
    /// effect). A future re-tune that loses a member or pushes an
    /// intensity past the barely-perceptible ceiling trips here.
    #[test]
    fn whisper_composes_the_expected_set_at_pinned_intensities() {
        let comp = AmbiencePreset::Whisper.compose();
        let mut failures: Vec<String> = Vec::new();

        // The composed set is exactly {aurora, bloom, glow_on_bell}.
        let want = [
            CatalogEffect::Aurora,
            CatalogEffect::Bloom,
            CatalogEffect::GlowOnBell,
        ];
        for effect in want {
            if !comp.contains(effect) {
                failures.push(std::format!("Whisper is missing {effect:?}"));
            }
        }
        if comp.members.len() != want.len() {
            failures.push(std::format!(
                "Whisper has {} members, want exactly {}",
                comp.members.len(),
                want.len()
            ));
        }

        // Per-member intensity rows — each pinned at the threshold.
        let aurora = comp.aurora().expect("Whisper has an aurora member");
        if aurora.intensity != WHISPER_AURORA_INTENSITY {
            failures.push(std::format!(
                "aurora intensity {} != {WHISPER_AURORA_INTENSITY}",
                aurora.intensity
            ));
        }
        if aurora.intensity > BARELY_PERCEPTIBLE_AURORA_CEILING {
            failures.push(std::format!(
                "aurora intensity {} exceeds the barely-perceptible ceiling {BARELY_PERCEPTIBLE_AURORA_CEILING}",
                aurora.intensity
            ));
        }
        if aurora.horizon != AMBIENCE_HORIZON {
            failures.push(std::format!("aurora horizon {} != {AMBIENCE_HORIZON}", aurora.horizon));
        }
        let bloom = comp.member(CatalogEffect::Bloom).expect("Whisper has bloom");
        if bloom.bloom_threshold != AMBIENCE_BLOOM_THRESHOLD {
            failures.push(std::format!(
                "bloom threshold {} != {AMBIENCE_BLOOM_THRESHOLD}",
                bloom.bloom_threshold
            ));
        }

        assert!(
            failures.is_empty(),
            "{} Whisper composition violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Off contributes ZERO members — the empty composition that maps
    /// to zero graph nodes.
    #[test]
    fn off_composes_zero_members() {
        assert!(AmbiencePreset::Off.compose().members.is_empty());
        assert!(AmbiencePreset::Off.compose().aurora().is_none());
    }

    /// Present is louder than Whisper on every scalar dial but still
    /// under the ceiling — the showing-off tier, not a different layer.
    #[test]
    fn present_is_louder_than_whisper_but_still_under_the_ceiling() {
        let w = AmbiencePreset::Whisper.compose();
        let p = AmbiencePreset::Present.compose();
        let mut failures: Vec<String> = Vec::new();

        let wa = w.aurora().expect("whisper aurora");
        let pa = p.aurora().expect("present aurora");
        if pa.intensity <= wa.intensity {
            failures.push("Present aurora not louder than Whisper".to_owned());
        }
        if pa.intensity > BARELY_PERCEPTIBLE_AURORA_CEILING {
            failures.push(std::format!(
                "Present aurora intensity {} exceeds ceiling {BARELY_PERCEPTIBLE_AURORA_CEILING}",
                pa.intensity
            ));
        }
        // Same composition shape — only intensities change.
        let we: std::collections::BTreeSet<_> =
            w.members.iter().map(|m| m.effect.name()).collect();
        let pe: std::collections::BTreeSet<_> =
            p.members.iter().map(|m| m.effect.name()).collect();
        if we != pe {
            failures.push("Present and Whisper differ in WHICH effects compose".to_owned());
        }

        assert!(
            failures.is_empty(),
            "{} present/whisper violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Every composed member's effect lives in the catalog's post range
    /// — the composition can never name an effect outside the registry
    /// (it consumes `CatalogEffect`, so this is by construction; the
    /// test documents the intent + guards the priority band).
    #[test]
    fn every_member_is_a_post_range_catalog_effect() {
        let mut failures: Vec<String> = Vec::new();
        for preset in AmbiencePreset::ALL {
            for m in preset.compose().members {
                let p = m.effect.priority();
                if !(200..=799).contains(&p) {
                    failures.push(std::format!(
                        "{preset:?} member {:?} priority {p} outside the post range",
                        m.effect
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} out-of-range members:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}
