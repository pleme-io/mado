//! [`ScrollKinetics`] — the typed momentum-scroll sub-state.
//!
//! Scrolling is a physical motion, not an instantaneous jump. A wheel
//! flick injects *velocity* into this value; a pure per-frame
//! [`ScrollKinetics::tick`] integrates that velocity into whole-line
//! deltas and bleeds it off with exponential friction, so a flick
//! glides and eases to a weighty stop. Selection auto-scroll drives
//! the SAME value through [`ScrollKinetics::set_sustained`] — a held,
//! non-decaying drive while the pointer is past a viewport edge.
//!
//! This is a TYPED sub-state advanced by a PURE function, not a free
//! bool-flag "mode" (the mado FSM idiom — no new free bool-flag
//! modes): the engine owns one `ScrollKinetics` cell, calls `tick`
//! once per frame, and routes the returned line delta through the
//! existing `Terminal::scroll_up`/`scroll_down` effects.
//!
//! ## Determinism contract
//!
//! [`ScrollKinetics::tick`] MUST be a no-op when `dt == 0.0`: it
//! returns `0` and mutates nothing. The render loop's L1/L2
//! determinism ladders render at `elapsed = 0` / `dt = 0` and assert
//! byte-identical frame hashes; a kinetics tick that moved the
//! viewport at `dt == 0.0` would break that byte-stability. The guard
//! is the first statement in `tick`.
//!
//! ## Units
//!
//! `velocity` is in fractional **lines per second**. The sign is
//! `+` = scroll UP into history (increasing `scroll_offset`),
//! `-` = scroll DOWN toward the live tail (decreasing `scroll_offset`)
//! — matching the engine's `dy > 0.0 ⇒ scroll_up` convention.

/// Velocity below this magnitude (lines/sec) is treated as a clean
/// stop: `tick` zeroes both `velocity` and `residual` so momentum
/// never decays into an infinite sub-pixel crawl. At ~0.5 lines/sec a
/// 60 Hz frame moves <0.01 line — imperceptible, so cutting here is
/// invisible and leaves a crisp halt.
const STOP_EPSILON: f32 = 0.5;

/// The momentum-scroll kinetic state. One value, owned by the engine,
/// advanced once per frame by [`Self::tick`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollKinetics {
    /// Current scroll velocity in fractional lines per second.
    /// `+` = up into history, `-` = down toward the live tail.
    velocity: f32,
    /// Sub-line accumulator. `tick` integrates `velocity * dt` here
    /// and only peels off WHOLE lines as the returned delta, carrying
    /// the fraction across frames — so a slow drive still eventually
    /// moves a line, and a fast flick never quantizes away travel.
    residual: f32,
    /// `true` while a sustained (selection auto-scroll) drive owns the
    /// velocity. Lets the engine ask "is this a sustained selection
    /// drive I should cancel when the pointer re-enters the viewport?"
    /// without confusing it with a decaying wheel glide.
    sustained: bool,
}

impl Default for ScrollKinetics {
    fn default() -> Self {
        Self::at_rest()
    }
}

impl ScrollKinetics {
    /// A kinetics value at rest — zero velocity, no pending fraction.
    #[must_use]
    pub const fn at_rest() -> Self {
        Self {
            velocity: 0.0,
            residual: 0.0,
            sustained: false,
        }
    }

    /// Inject a wheel impulse. `lines` is the signed flick magnitude in
    /// lines/sec to add (sign = direction); same-direction impulses
    /// ACCUMULATE so a fast repeated flick builds a longer glide.
    /// `|velocity|` is clamped to `max_velocity` so a frantic flick
    /// can't launch straight to the scrollback top. Any impulse cancels
    /// the sustained flag — a wheel flick is a fresh decaying glide.
    pub fn add_impulse(&mut self, lines: f32, max_velocity: f32) {
        let max = max_velocity.max(0.0);
        self.sustained = false;
        self.velocity = (self.velocity + lines).clamp(-max, max);
    }

    /// Drive a held, non-decaying velocity (selection auto-scroll past
    /// a viewport edge). Unlike [`Self::add_impulse`] this REPLACES the
    /// velocity (it tracks the live overshoot, it doesn't accumulate)
    /// and marks the drive sustained, so friction in [`Self::tick`]
    /// can't bleed it off while the pointer stays past the edge.
    pub fn set_sustained(&mut self, velocity: f32) {
        self.velocity = velocity;
        self.sustained = true;
    }

    /// Whether the current drive is a sustained (selection
    /// auto-scroll) one, as opposed to a decaying wheel glide. The
    /// engine cancels a sustained drive via [`Self::stop`] the moment
    /// the pointer re-enters the viewport.
    #[must_use]
    pub const fn is_sustained(&self) -> bool {
        self.sustained
    }

    /// Advance one frame. Returns the whole-line viewport delta to
    /// apply this tick (`+` up into history, `-` down toward the tail);
    /// the caller routes it through `Terminal::scroll_up`/`scroll_down`.
    ///
    /// Steps:
    /// 1. **dt==0 guard** — return `0` and mutate nothing (determinism).
    /// 2. Integrate `residual += velocity * dt`; peel off the whole
    ///    part as the returned delta, keep the fraction in `residual`.
    /// 3. Apply exponential friction: `velocity *= e^(-friction * dt)`.
    ///    Exponential (not linear) decay is the natural weighty model —
    ///    the velocity halves every `ln2 / friction` seconds, framerate
    ///    independent, so the glide eases out smoothly rather than
    ///    snapping. A SUSTAINED drive skips friction (it's held by the
    ///    live overshoot, not coasting).
    /// 4. Below [`STOP_EPSILON`] zero velocity AND residual — a clean
    ///    halt, no infinite sub-line crawl.
    pub fn tick(&mut self, dt: f32, friction: f32) -> i32 {
        // (1) Determinism: a zero-dt frame moves nothing. This keeps
        // the L1/L2 render ladders byte-identical when the engine is
        // driven headless at dt=0.
        if dt <= 0.0 {
            return 0;
        }

        // (2) Integrate + peel off whole lines, carrying the fraction.
        self.residual += self.velocity * dt;
        let whole = self.residual.trunc();
        self.residual -= whole;
        let delta = whole as i32;

        // (3) Friction decay — exponential, framerate-independent.
        // A sustained drive is held by the live overshoot; it must not
        // bleed off under its own friction.
        if !self.sustained {
            self.velocity *= (-friction.max(0.0) * dt).exp();
        }

        // (4) Clean stop below epsilon — no infinite crawl. Sustained
        // drives never auto-stop here; the engine stops them explicitly
        // when the pointer re-enters the viewport.
        if !self.sustained && self.velocity.abs() < STOP_EPSILON {
            self.velocity = 0.0;
            self.residual = 0.0;
        }

        delta
    }

    /// Whether any motion is pending (non-zero velocity). The engine
    /// can skip the scroll-effect path entirely on a still kinetics.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.velocity != 0.0
    }

    /// Halt immediately — zero velocity and drop the pending fraction.
    /// Used when momentum hits a scrollback wall (so it doesn't fight
    /// the bound) or when a sustained selection drive should cancel.
    pub fn stop(&mut self) {
        self.velocity = 0.0;
        self.residual = 0.0;
        self.sustained = false;
    }

    /// Test-only read of the raw velocity (lines/sec). Production code
    /// never inspects the velocity directly — it consumes the
    /// `tick`-returned line delta — so this stays test-gated.
    #[cfg(test)]
    pub(crate) const fn velocity(&self) -> f32 {
        self.velocity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test-only friction that gives a visible-but-short glide so the
    /// monotonic-decay + finite-travel assertions read clearly.
    const FRICTION: f32 = 5.0;
    const DT: f32 = 1.0 / 60.0;
    const MAX_V: f32 = 4000.0;

    /// `dt == 0.0` is a strict no-op: returns 0 and the whole value is
    /// untouched — the determinism contract the L1/L2 ladders rely on.
    #[test]
    fn dt_zero_is_a_noop() {
        let mut k = ScrollKinetics::at_rest();
        k.add_impulse(1000.0, MAX_V);
        let before = k;
        let delta = k.tick(0.0, FRICTION);
        assert_eq!(delta, 0, "dt=0 must move nothing");
        assert_eq!(k, before, "dt=0 must not mutate the kinetics");
    }

    /// A single impulse glides then stops: velocity decays monotonically
    /// to exactly 0, and `is_active()` flips false at the end.
    #[test]
    fn single_impulse_glides_then_stops() {
        let mut k = ScrollKinetics::at_rest();
        k.add_impulse(600.0, MAX_V);
        assert!(k.is_active());

        let mut prev = f32::INFINITY;
        let mut stopped = false;
        // 10 simulated seconds is far more than any glide lasts.
        for _ in 0..600 {
            k.tick(DT, FRICTION);
            let v = k.velocity.abs();
            assert!(v <= prev + 1e-3, "velocity must not increase mid-glide");
            prev = v;
            if !k.is_active() {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "a single impulse must come to rest");
        assert_eq!(k.velocity, 0.0, "stopped velocity is exactly 0");
        assert_eq!(k.residual, 0.0, "stopped residual is exactly 0");
    }

    /// Total lines traveled from one impulse is FINITE and bounded —
    /// momentum can't run away.
    #[test]
    fn total_travel_is_finite_and_bounded() {
        let mut k = ScrollKinetics::at_rest();
        k.add_impulse(600.0, MAX_V);
        let mut total: i64 = 0;
        for _ in 0..600 {
            total += i64::from(k.tick(DT, FRICTION));
            if !k.is_active() {
                break;
            }
        }
        assert!(total > 0, "an up-impulse travels up (positive)");
        // Analytic ceiling: ∫v0·e^(-f·t) dt = v0/f. With v0=600, f=5
        // that's 120 lines; a comfortable bound proves no runaway.
        assert!(total <= 130, "travel {total} exceeded the analytic bound");
    }

    /// Fractional residual accumulates across ticks: a velocity slow
    /// enough to move <1 line per frame STILL eventually moves a line.
    #[test]
    fn fractional_residual_accumulates_to_a_line() {
        // 30 lines/sec at 60 Hz = 0.5 line/frame → a line every 2nd
        // frame. Drive it sustained so friction doesn't interfere.
        let mut k = ScrollKinetics::at_rest();
        k.set_sustained(30.0);
        let mut moved = 0;
        for _ in 0..4 {
            moved += k.tick(DT, FRICTION);
        }
        assert!(moved >= 1, "sub-line velocity must accumulate to a move");
    }

    /// A sustained drive does NOT decay under friction — it's held by
    /// the live overshoot until the engine stops it.
    #[test]
    fn sustained_drive_does_not_decay() {
        let mut k = ScrollKinetics::at_rest();
        k.set_sustained(-500.0);
        for _ in 0..120 {
            k.tick(DT, FRICTION);
        }
        assert!(k.is_sustained());
        assert_eq!(k.velocity, -500.0, "sustained velocity is held, not decayed");
        k.stop();
        assert!(!k.is_active());
        assert!(!k.is_sustained());
    }

    /// `add_impulse` clamps to `max_velocity`; same-direction impulses
    /// accumulate but still saturate at the cap.
    #[test]
    fn impulse_accumulates_and_clamps() {
        let mut k = ScrollKinetics::at_rest();
        k.add_impulse(300.0, 1000.0);
        k.add_impulse(300.0, 1000.0);
        assert_eq!(k.velocity, 600.0, "same-direction impulses add");
        k.add_impulse(900.0, 1000.0);
        assert_eq!(k.velocity, 1000.0, "accumulation saturates at the cap");
        // Opposite direction is also clamped.
        k.add_impulse(-5000.0, 1000.0);
        assert_eq!(k.velocity, -1000.0);
    }

    /// Caller zeroing velocity at a wall (via `stop`) leaves a clean
    /// rest — the next tick moves nothing and isn't active.
    #[test]
    fn wall_clamp_via_stop_is_a_clean_rest() {
        let mut k = ScrollKinetics::at_rest();
        k.add_impulse(1000.0, MAX_V);
        // Simulate hitting the scrollback wall: engine zeroes velocity.
        k.stop();
        assert!(!k.is_active());
        assert_eq!(k.tick(DT, FRICTION), 0, "no travel after a wall stop");
        assert_eq!(k.velocity, 0.0);
        assert_eq!(k.residual, 0.0);
    }

    /// An up-impulse yields positive deltas, a down-impulse negative —
    /// the sign convention the engine maps to scroll_up/scroll_down.
    #[test]
    fn impulse_sign_drives_delta_sign() {
        let mut up = ScrollKinetics::at_rest();
        up.add_impulse(2000.0, MAX_V);
        assert!(up.tick(DT, FRICTION) > 0, "up-impulse → positive delta");

        let mut down = ScrollKinetics::at_rest();
        down.add_impulse(-2000.0, MAX_V);
        assert!(down.tick(DT, FRICTION) < 0, "down-impulse → negative delta");
    }
}
