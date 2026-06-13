//! Ambience governor — a typed perf FSM that scales the composed
//! ambience layer's QUALITY to the frame budget, never killing perf
//! (operator perf wave, 2026-06-13).
//!
//! The aurora curtain (the costliest ambience member) is tiered
//! `Off ≤ Low ≤ Medium ≤ High` via a uniform word the WGSL switches on
//! — so quality scales **rebuild-free**: the governor flips the word
//! per frame, never recompiles the graph. This governor drives that
//! word for the WHOLE ambience composition, not just aurora: a single
//! [`AmbienceQualityEffect::SetAmbienceQuality`] flows through the
//! params sink each tick.
//!
//! # The FSM (operator FSM directive, 2026-06-12, made structural)
//!
//! ```text
//!   Off  ⇄  Low  ⇄  Med  ⇄  High        (bounded above by `ceiling`)
//!     ◄── StepDown (30 over frames)
//!     ──► StepUp   (300 calm frames)
//! ```
//!
//! * STATES — [`AuroraQuality`] (the catalog's tier enum, reused so the
//!   word the FSM mints is the word the WGSL reads; no parallel enum).
//! * EVENTS — per frame from the measured `frame_us` vs the budget:
//!   [`GovernorEvent::TickOverBudget`] (frame > 0.85 × budget),
//!   [`GovernorEvent::TickCalm`] (frame < 0.60 × budget),
//!   [`GovernorEvent::TickNeutral`] (the dead band between).
//! * EFFECT — exactly one [`AmbienceQualityEffect::SetAmbienceQuality`]
//!   per tick (the params-sink write; the renderer applies it to the
//!   aurora quality word).
//!
//! ## No oscillation, by construction
//!
//! `N_DOWN = 30` (~0.5 s @60Hz) ≪ `M_UP = 300` (~5 s) and the 0.60/0.85
//! dead band mean the machine drops fast under sustained load but climbs
//! back slowly only after a long calm — it cannot ping-pong on a single
//! noisy frame. A `TickNeutral` (in the dead band) resets BOTH streaks,
//! so a mixed workload parks at its current tier.
//!
//! ## reduce_motion bypasses the governor entirely
//!
//! Under `reduce_motion` the ambience composition is empty (zero nodes)
//! — the renderer omits the aurora node, so there is nothing to quality.
//! The governor is simply not ticked in that case (the renderer gates
//! the poll on a non-empty composition); the FSM never has to encode the
//! accessibility floor, mirroring how aurora/snow/glow handle it.
//!
//! ## The forcing function
//!
//! [`AmbienceGovernor::on_event`] matches on the (state, event) product
//! with NO wildcard arm on the state enum — a new quality tier is a
//! compile error until every transition is decided. The mechanical
//! exhaustiveness test below drives ALL states × ALL events from
//! derive-emitted registries.

use engawa_wgpu::catalog::aurora::AuroraQuality;

/// Consecutive over-budget frames before a `StepDown` (~0.5 s @60Hz).
pub(crate) const N_DOWN: u32 = 30;
/// Consecutive calm frames before a `StepUp` (~5 s @60Hz). `M_UP ≫
/// N_DOWN` is half the no-oscillation guarantee.
pub(crate) const M_UP: u32 = 300;

/// Over-budget fraction: a frame above this fraction of the budget is
/// "over". 0.85 leaves headroom before the budget is actually blown.
pub(crate) const OVER_FRAC: f32 = 0.85;
/// Calm fraction: a frame below this fraction of the budget is "calm".
/// The 0.60..0.85 gap is the dead band that kills oscillation.
pub(crate) const CALM_FRAC: f32 = 0.60;

/// The default frame budget in microseconds — one 60 Hz frame
/// (16.67 ms). The governor reads this; a future vsync/target-fps wire
/// can re-budget it, but the FSM logic is budget-relative either way.
pub(crate) const DEFAULT_BUDGET_US: u64 = 16_667;

/// One governor input per frame — the measured-frame verdict against
/// the budget. Classified by [`classify_frame`] so the FSM never sees
/// raw microseconds (the budget arithmetic is one pure function, tested
/// at its boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(pleme_allvariants_derive::AllVariants))]
pub(crate) enum GovernorEvent {
    /// frame_us > OVER_FRAC × budget — the layer is costing too much.
    TickOverBudget,
    /// frame_us < CALM_FRAC × budget — there is headroom to climb.
    TickCalm,
    /// In the dead band — hold; reset both streaks.
    TickNeutral,
}

/// Classify a measured frame (microseconds) against the budget into a
/// governor event. The ONE place the budget arithmetic lives.
#[must_use]
pub(crate) fn classify_frame(frame_us: u64, budget_us: u64) -> GovernorEvent {
    // budget==0 would divide-by-zero; treat a zero budget as "always
    // calm" so the governor climbs to the ceiling and never panics
    // (a zero budget means "no budget pressure").
    if budget_us == 0 {
        return GovernorEvent::TickCalm;
    }
    let frac = frame_us as f32 / budget_us as f32;
    if frac > OVER_FRAC {
        GovernorEvent::TickOverBudget
    } else if frac < CALM_FRAC {
        GovernorEvent::TickCalm
    } else {
        GovernorEvent::TickNeutral
    }
}

/// The single typed effect — the params-sink write the renderer applies
/// to the aurora quality word. One per tick, always.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AmbienceQualityEffect {
    SetAmbienceQuality(AuroraQuality),
}

/// One transition's output — the next state + the (always-present)
/// quality effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GovernorStep {
    pub state: AuroraQuality,
    pub effect: AmbienceQualityEffect,
}

/// Ordered ladder Off < Low < Medium < High — the only place the tier
/// order lives. Derived from `AuroraQuality::as_u32` so it cannot drift
/// from the catalog wire word.
const LADDER: [AuroraQuality; 4] = [
    AuroraQuality::Off,
    AuroraQuality::Low,
    AuroraQuality::Medium,
    AuroraQuality::High,
];

/// Position of a tier on the ladder (0=Off .. 3=High). Total over the
/// catalog enum — a new tier is a compile error here.
fn rung(q: AuroraQuality) -> usize {
    match q {
        AuroraQuality::Off => 0,
        AuroraQuality::Low => 1,
        AuroraQuality::Medium => 2,
        AuroraQuality::High => 3,
    }
}

/// One rung down (saturates at Off).
fn step_down(q: AuroraQuality) -> AuroraQuality {
    LADDER[rung(q).saturating_sub(1)]
}

/// One rung up, clamped to `ceiling` (saturates at High otherwise).
fn step_up(q: AuroraQuality, ceiling: AuroraQuality) -> AuroraQuality {
    let next = LADDER[(rung(q) + 1).min(LADDER.len() - 1)];
    if rung(next) > rung(ceiling) {
        ceiling
    } else {
        next
    }
}

/// The typed perf FSM. `state` is the live quality word; `over_streak`
/// / `calm_streak` accumulate the consecutive verdicts; `ceiling` caps
/// the climb to the user's tier; `budget_us` is the frame budget the
/// classifier measures against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AmbienceGovernor {
    state: AuroraQuality,
    over_streak: u32,
    calm_streak: u32,
    ceiling: AuroraQuality,
    budget_us: u64,
}

impl Default for AmbienceGovernor {
    fn default() -> Self {
        // Recommended default: start at the ceiling's start tier
        // (Medium — the catalog's shipped default) with the High
        // ceiling; the governor only ever steps DOWN from here under
        // load, never above the ceiling.
        Self::new(AuroraQuality::High)
    }
}

impl AmbienceGovernor {
    /// New governor with `ceiling` as the climb cap. Starts at the
    /// catalog default (`Medium`) clamped to the ceiling, the 60 Hz
    /// budget, and zero streaks.
    #[must_use]
    pub(crate) fn new(ceiling: AuroraQuality) -> Self {
        let start = if rung(AuroraQuality::Medium) > rung(ceiling) {
            ceiling
        } else {
            AuroraQuality::Medium
        };
        Self {
            state: start,
            over_streak: 0,
            calm_streak: 0,
            ceiling,
            budget_us: DEFAULT_BUDGET_US,
        }
    }

    /// The live quality word the renderer applies to the aurora curtain
    /// this frame.
    #[must_use]
    pub(crate) fn quality(&self) -> AuroraQuality {
        self.state
    }

    /// Tick the governor with a measured frame (microseconds). Classifies
    /// against the budget and advances the FSM — the renderer's per-frame
    /// poll. Returns the typed effect the params sink applies.
    pub(crate) fn tick_frame(&mut self, frame_us: u64) -> AmbienceQualityEffect {
        let event = classify_frame(frame_us, self.budget_us);
        self.on_event(event).effect
    }

    /// The pure, total transition `(state, event) → (state, effect)`.
    /// No I/O. The outer match carries NO wildcard arm on the state
    /// enum — the forcing function. A `TickOverBudget` grows the
    /// over-streak (resetting calm) and steps DOWN at `N_DOWN`; a
    /// `TickCalm` grows the calm-streak (resetting over) and steps UP at
    /// `M_UP`, clamped to `ceiling`; a `TickNeutral` resets both streaks
    /// and holds. On a step the relevant streak resets so the next step
    /// needs a fresh run.
    pub(crate) fn on_event(&mut self, event: GovernorEvent) -> GovernorStep {
        let next = match self.state {
            // Off — the floor. Over-budget cannot drop further; calm
            // can climb (if the ceiling allows). The match is written
            // per (state, event) on purpose: collapsing the arms would
            // erase the forcing function.
            AuroraQuality::Off => self.advance(event),
            AuroraQuality::Low => self.advance(event),
            AuroraQuality::Medium => self.advance(event),
            AuroraQuality::High => self.advance(event),
        };
        self.state = next;
        GovernorStep {
            state: next,
            effect: AmbienceQualityEffect::SetAmbienceQuality(next),
        }
    }

    /// Shared streak/ladder advance — identical arithmetic for every
    /// state (the per-state match above is the forcing function; the
    /// transition rule itself is uniform: step the ladder when a streak
    /// matures, bounded by Off below and `ceiling` above).
    fn advance(&mut self, event: GovernorEvent) -> AuroraQuality {
        match event {
            GovernorEvent::TickOverBudget => {
                self.calm_streak = 0;
                self.over_streak = self.over_streak.saturating_add(1);
                if self.over_streak >= N_DOWN {
                    self.over_streak = 0;
                    step_down(self.state)
                } else {
                    self.state
                }
            }
            GovernorEvent::TickCalm => {
                self.over_streak = 0;
                self.calm_streak = self.calm_streak.saturating_add(1);
                if self.calm_streak >= M_UP {
                    self.calm_streak = 0;
                    step_up(self.state, self.ceiling)
                } else {
                    self.state
                }
            }
            GovernorEvent::TickNeutral => {
                self.over_streak = 0;
                self.calm_streak = 0;
                self.state
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_lines)] // matrix tests enumerate every cell on purpose
mod tests {
    use super::*;

    /// Payload-free state mirror — `ALL` is derive-emitted so the
    /// state-variant count is mechanical (same rationale as
    /// `ux::modes::PointerKind`). A new `AuroraQuality` tier would force
    /// a new mirror variant here (the total `state_kind` match below
    /// refuses to compile otherwise), which grows `ALL` and fails the
    /// len-pinned registry test.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, pleme_allvariants_derive::AllVariants)]
    enum QualityKind {
        Off,
        Low,
        Medium,
        High,
    }

    fn state_kind(q: AuroraQuality) -> QualityKind {
        match q {
            AuroraQuality::Off => QualityKind::Off,
            AuroraQuality::Low => QualityKind::Low,
            AuroraQuality::Medium => QualityKind::Medium,
            AuroraQuality::High => QualityKind::High,
        }
    }

    const ALL_STATES: [AuroraQuality; 4] = [
        AuroraQuality::Off,
        AuroraQuality::Low,
        AuroraQuality::Medium,
        AuroraQuality::High,
    ];

    /// MECHANICAL registry completeness: the hand list of states ⇄ the
    /// derive-emitted mirror `ALL`, and the events list ⇄ the
    /// derive-emitted `GovernorEvent::ALL`. A new tier/event cannot land
    /// without growing these mechanically.
    #[test]
    fn governor_registries_cover_every_variant() {
        let mut failures: Vec<String> = Vec::new();
        let state_ords: std::collections::BTreeSet<usize> = ALL_STATES
            .iter()
            .map(|s| {
                QualityKind::ALL
                    .iter()
                    .position(|k| *k == state_kind(*s))
                    .expect("kind in ALL")
            })
            .collect();
        if state_ords != (0..QualityKind::ALL.len()).collect() {
            failures.push(std::format!(
                "state registry covers {state_ords:?}, want 0..{}",
                QualityKind::ALL.len()
            ));
        }
        if GovernorEvent::ALL.len() != 3 {
            failures.push(std::format!(
                "expected 3 governor events, ALL has {}",
                GovernorEvent::ALL.len()
            ));
        }
        assert!(
            failures.is_empty(),
            "{} registry gaps:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// THE FULL MATRIX — every (state, event) pair holds the FSM
    /// invariants: exactly one SetAmbienceQuality effect; the state
    /// never climbs above the ceiling; a single tick never steps more
    /// than one rung; the effect word always equals the new state.
    /// Failures aggregate matrix-style.
    #[test]
    fn governor_state_event_matrix_holds_invariants() {
        let mut failures: Vec<String> = Vec::new();
        let ceiling = AuroraQuality::High;
        for &start in &ALL_STATES {
            for &event in GovernorEvent::ALL {
                // Fresh governor pinned to `start` (streaks zero) — one
                // tick from each state with each event.
                let mut g = AmbienceGovernor {
                    state: start,
                    over_streak: 0,
                    calm_streak: 0,
                    ceiling,
                    budget_us: DEFAULT_BUDGET_US,
                };
                let step = g.on_event(event);
                let ctx = std::format!("{start:?} × {event:?}");

                // The effect word always equals the new state (single
                // effect through the sink, never a divergent word).
                let AmbienceQualityEffect::SetAmbienceQuality(word) = step.effect;
                if word != step.state {
                    failures.push(std::format!("{ctx}: effect word {word:?} != state {:?}", step.state));
                }
                // Never above the ceiling.
                if rung(step.state) > rung(ceiling) {
                    failures.push(std::format!("{ctx}: stepped above the ceiling"));
                }
                // A single tick (streaks were zero) never matures a
                // streak, so it must HOLD the start state — the step
                // only happens when a streak reaches its bound.
                if step.state != start {
                    failures.push(std::format!(
                        "{ctx}: a single tick from zero streaks changed state to {:?}",
                        step.state
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} governor matrix violations:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// N_DOWN consecutive over-budget frames step DOWN exactly once,
    /// and the over-streak resets after the step (the next step needs a
    /// fresh N_DOWN run).
    #[test]
    fn n_down_consecutive_over_frames_step_down_once() {
        let mut g = AmbienceGovernor::new(AuroraQuality::High); // starts Medium
        assert_eq!(g.quality(), AuroraQuality::Medium);
        // N_DOWN-1 over frames hold; the N_DOWN-th steps down.
        for _ in 0..(N_DOWN - 1) {
            g.on_event(GovernorEvent::TickOverBudget);
            assert_eq!(g.quality(), AuroraQuality::Medium, "must hold before N_DOWN");
        }
        g.on_event(GovernorEvent::TickOverBudget);
        assert_eq!(g.quality(), AuroraQuality::Low, "N_DOWN over frames step down");
        // The streak reset: another full N_DOWN run to drop again.
        for _ in 0..(N_DOWN - 1) {
            g.on_event(GovernorEvent::TickOverBudget);
        }
        assert_eq!(g.quality(), AuroraQuality::Low, "streak reset after a step");
        g.on_event(GovernorEvent::TickOverBudget);
        assert_eq!(g.quality(), AuroraQuality::Off, "second N_DOWN run reaches Off");
        // Off is the floor — no further drop, ever.
        for _ in 0..(N_DOWN * 3) {
            g.on_event(GovernorEvent::TickOverBudget);
        }
        assert_eq!(g.quality(), AuroraQuality::Off, "Off is the perf floor");
    }

    /// M_UP consecutive calm frames step UP exactly once, clamped to the
    /// ceiling.
    #[test]
    fn m_up_consecutive_calm_frames_step_up_clamped_to_ceiling() {
        // Ceiling Medium so the climb stops there even with endless calm.
        let mut g = AmbienceGovernor::new(AuroraQuality::Medium); // starts Medium
        // Drop to Off first (so there is room to climb).
        for _ in 0..(N_DOWN * 2) {
            g.on_event(GovernorEvent::TickOverBudget);
        }
        assert_eq!(g.quality(), AuroraQuality::Off);
        // M_UP calm frames step up once: Off → Low.
        for _ in 0..(M_UP - 1) {
            g.on_event(GovernorEvent::TickCalm);
            assert_eq!(g.quality(), AuroraQuality::Off, "must hold before M_UP");
        }
        g.on_event(GovernorEvent::TickCalm);
        assert_eq!(g.quality(), AuroraQuality::Low, "M_UP calm frames step up");
        // Another M_UP run: Low → Medium (the ceiling).
        for _ in 0..M_UP {
            g.on_event(GovernorEvent::TickCalm);
        }
        assert_eq!(g.quality(), AuroraQuality::Medium, "climb reaches the ceiling");
        // Endless calm cannot climb above the ceiling.
        for _ in 0..(M_UP * 3) {
            g.on_event(GovernorEvent::TickCalm);
        }
        assert_eq!(g.quality(), AuroraQuality::Medium, "ceiling caps the climb");
    }

    /// No oscillation: N_DOWN ≪ M_UP, so a single noisy over-frame in a
    /// calm run does NOT step down, and a TickNeutral resets both
    /// streaks (a mixed workload parks at its tier).
    #[test]
    fn no_oscillation_on_mixed_or_noisy_frames() {
        let mut g = AmbienceGovernor::new(AuroraQuality::High); // Medium
        // A long calm run interrupted by single over-frames never drops:
        // each over resets calm but one over is far below N_DOWN.
        for _ in 0..1000 {
            g.on_event(GovernorEvent::TickCalm);
            g.on_event(GovernorEvent::TickOverBudget); // single, resets calm
        }
        assert_eq!(
            g.quality(),
            AuroraQuality::Medium,
            "alternating calm/over must not move the tier"
        );
        // A TickNeutral resets a building over-streak — the dead band
        // parks the tier. Clear any residual streak first (the loop
        // above left a 1-frame over-streak), then build to one short of
        // N_DOWN, drop a neutral, and build again to one short — never
        // reaching N_DOWN consecutively, so no drop.
        g.on_event(GovernorEvent::TickNeutral); // clean slate
        for _ in 0..(N_DOWN - 1) {
            g.on_event(GovernorEvent::TickOverBudget);
        }
        g.on_event(GovernorEvent::TickNeutral); // resets the over-streak
        for _ in 0..(N_DOWN - 1) {
            g.on_event(GovernorEvent::TickOverBudget);
        }
        assert_eq!(
            g.quality(),
            AuroraQuality::Medium,
            "a neutral frame resets the over-streak — no drop"
        );
    }

    /// `classify_frame` boundary: the 0.60 / 0.85 fractions partition the
    /// budget into calm / neutral / over, and a zero budget is treated
    /// as always-calm (no divide-by-zero, no panic).
    #[test]
    fn classify_frame_partitions_the_budget() {
        let budget = 10_000u64;
        let mut failures: Vec<String> = Vec::new();
        let rows = [
            (5_000u64, GovernorEvent::TickCalm),     // 0.50 < 0.60
            (6_500, GovernorEvent::TickNeutral),     // 0.65 in band
            (8_000, GovernorEvent::TickNeutral),     // 0.80 in band
            (9_000, GovernorEvent::TickOverBudget),  // 0.90 > 0.85
            (20_000, GovernorEvent::TickOverBudget), // 2.0 way over
        ];
        for (frame, want) in rows {
            let got = classify_frame(frame, budget);
            if got != want {
                failures.push(std::format!("classify({frame}, {budget}) = {got:?}, want {want:?}"));
            }
        }
        // Zero budget never panics — always calm.
        if classify_frame(1_000_000, 0) != GovernorEvent::TickCalm {
            failures.push("zero budget must classify as calm".to_owned());
        }
        assert!(
            failures.is_empty(),
            "{} classify rows diverged:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// The recommended default: ceiling High, starting at the catalog
    /// default Medium (the governor only steps down from here under
    /// load).
    #[test]
    fn default_governor_is_high_ceiling_starting_medium() {
        let g = AmbienceGovernor::default();
        assert_eq!(g.quality(), AuroraQuality::Medium, "starts at the catalog default tier");
        assert_eq!(g.ceiling, AuroraQuality::High, "default ceiling is High");
    }
}
