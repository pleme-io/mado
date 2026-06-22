//! [`ScrollSystem`] — the ONE well-defined system that controls scrolling.
//!
//! Every scroll gesture in mado flows through this one typed value. A
//! windowing-layer gesture ([`ScrollGesture`] — discrete wheel ticks or
//! precise trackpad pixels) plus the live terminal context
//! ([`ScrollContext`] — mouse-tracking mode, alternate screen, DECCKM, cell
//! height) maps, through a typed policy ([`ScrollConfig`]) that *abstracts
//! the available behaviors*, to a typed [`ScrollAction`] the input engine
//! executes. **Pure decision in, typed action out**; the engine owns the I/O
//! edge (PTY writes, the terminal lock). This is the mado FSM idiom — a typed
//! sub-state advanced by pure functions, never free bool-flag modes.
//!
//! ## The behaviors it abstracts
//!
//! Three scroll sources over one viewport, plus two forwarding sinks:
//!
//! | Source  | Policy            | Behaviors |
//! |---------|-------------------|-----------|
//! | wheel   | [`WheelMode`]     | `Lines` (direct N/notch) · `Momentum` (decaying glide) |
//! | precise | [`PreciseMode`]   | `Pixels` (ghostty accumulator, OS inertia) · `Momentum` (synthetic glide) |
//! | drag    | [`AutoScroll`]    | sustained velocity past a viewport edge |
//! | → app   | `forward_to_tracking_apps` | wheel-button reports (xterm 1000–1003) |
//! | → TUI   | `alt_screen_arrows`        | cursor-key alt-scroll (xterm 1007) |
//!
//! Wheel-`Momentum` and the drag auto-scroll share ONE [`ScrollKinetics`]
//! cell (velocity + friction). The precise [`PreciseMode::Pixels`] path
//! carries its own signed sub-cell pixel remainder — ghostty's
//! `pending_scroll_y` accumulator — and applies whole cells immediately with
//! NO synthetic friction: the OS momentum-phase stream (winit forwards every
//! coast event as another precise delta) supplies the inertia, so layering a
//! second app-side decay on top is the double-physics this path removes.
//!
//! ## Why a system and not scattered branches
//!
//! Before this, scroll logic lived inline in the input engine across
//! `on_mouse_scroll` + `tick_scroll_kinetics`, with the wheel-vs-trackpad
//! distinction already lost at the madori boundary (one flattened `dy`). The
//! result was trackpad pixels re-quantized to lines (steppy) AND the synthetic
//! glide stacked on the OS coast (floaty). Concentrating every behavior in one
//! typed policy makes the modes mutually exclusive by construction, unit-
//! testable without a GPU, and tunable by configuration — solve once, in one
//! place.
//!
//! ## Determinism contract
//!
//! [`ScrollSystem::tick`] is the only per-frame mutator and is a strict no-op
//! at `dt == 0.0` (it delegates to [`ScrollKinetics::tick`], which guards
//! that). Gesture handling ([`ScrollSystem::on_gesture`]) mutates only on a
//! real event, never on a frame tick — so the render loop's L1/L2 byte-
//! determinism ladders (which drive the engine headless at `dt = 0`) stay
//! byte-stable.

use crate::ux::ScrollKinetics;

/// Wheel-impulse gain: lines/sec of momentum velocity injected per scrolled
/// line when [`WheelMode::Momentum`] is active. One notch (`per_notch` lines)
/// becomes a short glide; repeated same-direction notches accumulate AND
/// accelerate (the [`ScrollKinetics`] streak ramp) toward `max_velocity`.
/// Tuned so a single notch is a brisk weighted nudge — quick off the line,
/// never a launch.
const WHEEL_IMPULSE_GAIN: f32 = 30.0;

/// A scroll gesture from the windowing layer, classified by source. Mirrors
/// `madori::ScrollDelta` (and therefore winit's `MouseScrollDelta`); the
/// engine translates at the boundary so this system stays windowing-agnostic
/// and unit-testable. Positive is UP (into history), negative is DOWN (toward
/// the live tail) — the fleet terminal convention.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollGesture {
    /// Discrete mouse wheel. `ticks` is signed detents (≈ ±1 per notch; some
    /// mice report fractional ticks).
    Wheel { ticks: f64 },
    /// Precise trackpad / Magic Mouse. `pixels` is a signed PHYSICAL-pixel
    /// delta (winit already `to_physical`-scaled it), including the OS
    /// momentum-phase coast as a continued event stream.
    Precise { pixels: f64 },
}

impl ScrollGesture {
    /// The signed magnitude (ticks for wheel, pixels for precise).
    fn amount(self) -> f64 {
        match self {
            Self::Wheel { ticks } => ticks,
            Self::Precise { pixels } => pixels,
        }
    }

    /// `true` when the gesture scrolls UP into history (positive amount).
    fn is_up(self) -> bool {
        self.amount() > 0.0
    }

    /// `true` for a precise / pixel (trackpad) gesture.
    fn is_precise(self) -> bool {
        matches!(self, Self::Precise { .. })
    }
}

/// Live terminal context a scroll decision reads. Snapshotted by the engine
/// under a short read lock and handed in by value, so the decision is pure.
#[derive(Debug, Clone, Copy)]
pub struct ScrollContext {
    /// The app grabs the mouse (`mouse_mode != Off`): a gesture forwards to
    /// it as wheel-button reports instead of moving mado's scrollback.
    pub mouse_tracking: bool,
    /// The operator's shift bypass is active (shift held && capture off):
    /// scrollback wins even while an app tracks the mouse (the
    /// xterm/kitty/ghostty convention).
    pub shift_bypass: bool,
    /// Alternate screen active (a full-screen TUI owns the viewport): a
    /// gesture with no app tracking converts to cursor-key alt-scroll
    /// ([`ScrollAction::ForwardArrows`] — the engine owns the DECCKM
    /// encoding at execution).
    pub alt_screen: bool,
    /// Physical-pixel cell height — the precise accumulator's divisor.
    pub cell_height: f32,
}

/// What the engine must do in response to a gesture or tick. Pure data — the
/// engine performs the I/O (PTY writes via its sinks, viewport scroll under
/// its terminal lock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    /// Do nothing — a zero gesture, sub-cell precise accumulation, or a
    /// momentum impulse that will surface through later [`ScrollSystem::tick`]s.
    None,
    /// Move the scrollback viewport by `cells` whole cells. `+` scrolls up
    /// into history, `−` down toward the live tail.
    Viewport { cells: i32 },
    /// Forward `count` wheel-button reports to a mouse-tracking app.
    ForwardWheel { up: bool, count: usize },
    /// Forward `count` cursor-key sequences (alt-screen alt-scroll).
    ForwardArrows { up: bool, count: usize },
}

/// Momentum (kinetic glide) tuning — shared by [`WheelMode::Momentum`],
/// [`PreciseMode::Momentum`], and the per-frame integration of the drag
/// auto-scroll (which ignores friction, being held by the live overshoot).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MomentumTuning {
    /// Exponential friction per second; the glide's velocity halves every
    /// `ln2/friction` seconds. Larger = stops sooner.
    pub friction: f32,
    /// Velocity cap (lines/sec) so a frantic flick can't launch to the
    /// scrollback top in one frame.
    pub max_velocity: f32,
}

/// How a discrete mouse-wheel notch scrolls the scrollback viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WheelMode {
    /// Jump `per_notch` lines per detent, immediately — classic, no inertia.
    Lines { per_notch: u32 },
    /// Inject a decaying momentum glide: a flick coasts and eases to a stop
    /// (`per_notch` lines per detent feed the velocity).
    Momentum { per_notch: u32 },
}

/// How a precise trackpad / Magic Mouse gesture scrolls the scrollback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreciseMode {
    /// Ghostty-faithful: accumulate physical pixels × `multiplier` into a
    /// signed sub-cell remainder, peel whole cells via cell height, carry the
    /// fraction, apply immediately. Inertia is the OS momentum stream — NO
    /// synthetic friction. The recommended default; true trackpad feel.
    Pixels { multiplier: f32 },
    /// Feed precise pixels into the synthetic momentum glide instead of
    /// applying them directly — app-side inertia on the trackpad, for
    /// platforms/devices with weak OS momentum (or taste). `multiplier`
    /// scales pixels-per-event into the velocity impulse.
    Momentum { multiplier: f32 },
}

/// Selection auto-scroll while dragging a text selection past a viewport edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoScroll {
    /// Whether dragging past an edge scrolls + extends the selection.
    pub enabled: bool,
    /// Sustained velocity (lines/sec) per line of pointer overshoot past the
    /// edge — drag one line past ⇒ a gentle crawl, drag far ⇒ faster reveal.
    pub velocity_per_line: f32,
    /// Overshoot cap (lines) so the auto-scroll speed saturates.
    pub max_overshoot: f32,
}

/// The complete typed scroll policy — the one value that selects every scroll
/// behavior. Built from the flat config knobs via the engine's `UxBehavior`
/// (see [`crate::ux::behavior`]); hot-reloadable via
/// [`ScrollSystem::set_config`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollConfig {
    /// Discrete mouse-wheel response.
    pub wheel: WheelMode,
    /// Precise (trackpad / Magic Mouse) response.
    pub precise: PreciseMode,
    /// Shared momentum tuning for both `Momentum` modes.
    pub momentum: MomentumTuning,
    /// Selection drag auto-scroll.
    pub autoscroll: AutoScroll,
    /// Forward gestures to mouse-tracking apps (xterm 1000–1003). `false`
    /// keeps the wheel on mado's own scrollback even inside vim/htop.
    pub forward_to_tracking_apps: bool,
    /// Alt-screen alt-scroll (xterm 1007): a wheel/precise gesture in a
    /// full-screen TUI without tracking converts to arrow keys so pagers
    /// scroll their content. `false` makes it a no-op.
    pub alt_screen_arrows: bool,
}

impl ScrollConfig {
    /// A behavior-preserving baseline matching the historical hard-coded
    /// defaults — momentum wheel, ghostty pixel trackpad, auto-scroll on.
    /// The production surface builds this from typed config instead; this is
    /// for tests and as a documented reference point.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            wheel: WheelMode::Momentum { per_notch: 2 },
            precise: PreciseMode::Pixels { multiplier: 2.0 },
            momentum: MomentumTuning {
                friction: 3.0,
                max_velocity: 200.0,
            },
            autoscroll: AutoScroll {
                enabled: true,
                velocity_per_line: 18.0,
                max_overshoot: 6.0,
            },
            forward_to_tracking_apps: true,
            alt_screen_arrows: true,
        }
    }
}

/// The scroll system. Owns the kinetic + accumulator sub-state and the typed
/// policy; turns gestures and frame ticks into [`ScrollAction`]s.
#[derive(Debug, Clone)]
pub struct ScrollSystem {
    config: ScrollConfig,
    /// Momentum glide + drag auto-scroll velocity. One cell, two drivers:
    /// `add_impulse` (decaying wheel/precise glide) and `set_sustained`
    /// (held drag auto-scroll).
    kinetics: ScrollKinetics,
    /// Signed sub-cell pixel remainder for [`PreciseMode::Pixels`] — ghostty's
    /// `pending_scroll_y`. In PHYSICAL pixels, carried across events, NEVER
    /// reset on direction reversal (a reversal nets against the carried
    /// fraction, exactly like ghostty).
    pixel_remainder: f32,
}

impl ScrollSystem {
    /// Build the system from a typed policy.
    #[must_use]
    pub fn new(config: ScrollConfig) -> Self {
        Self {
            config,
            kinetics: ScrollKinetics::at_rest(),
            pixel_remainder: 0.0,
        }
    }

    /// Replace the policy. The engine re-feeds the current config on every
    /// gesture/tick so `behavior` stays the single source of truth (and a
    /// future config hot-reload is picked up with no extra wiring); the
    /// `ScrollConfig` is `Copy`, so this is a trivial field assignment.
    /// In-flight kinetic + accumulator state is preserved — a mid-glide
    /// retune adjusts feel without jolting the viewport position.
    pub fn set_config(&mut self, config: ScrollConfig) {
        self.config = config;
    }

    /// Decide what one scroll gesture does, given the live terminal context.
    /// The single entry point for wheel + trackpad events.
    pub fn on_gesture(&mut self, gesture: ScrollGesture, ctx: &ScrollContext) -> ScrollAction {
        let up = gesture.is_up();

        // A precise gesture is direct manipulation: it cancels any in-flight
        // wheel/precise momentum glide. Touching the trackpad halts coasting,
        // like grabbing a spinning scrollwheel — and without it the residual
        // glide would scroll alongside the OS momentum stream (the exact
        // double-inertia the precise path exists to remove).
        if gesture.is_precise() {
            self.kinetics.stop();
        }

        // ── Forwarding sinks take an IMMEDIATE whole-cell count (apps never
        // want a synthetic glide). Tracking forwarding wins over alt-scroll.
        let to_app = self.config.forward_to_tracking_apps && ctx.mouse_tracking && !ctx.shift_bypass;
        if to_app {
            let cells = self.direct_cells(gesture, ctx);
            return if cells == 0 {
                ScrollAction::None
            } else {
                ScrollAction::ForwardWheel {
                    up,
                    count: cells.unsigned_abs() as usize,
                }
            };
        }
        if self.config.alt_screen_arrows && ctx.alt_screen {
            let cells = self.direct_cells(gesture, ctx);
            return if cells == 0 {
                ScrollAction::None
            } else {
                ScrollAction::ForwardArrows {
                    up,
                    count: cells.unsigned_abs() as usize,
                }
            };
        }

        // ── Plain scrollback viewport.
        if self.viewport_uses_momentum(gesture) {
            let velocity = self.momentum_velocity(gesture);
            if velocity != 0.0 {
                self.kinetics
                    .add_impulse(velocity, self.config.momentum.max_velocity);
            }
            return ScrollAction::None;
        }
        let cells = self.direct_cells(gesture, ctx);
        if cells == 0 {
            ScrollAction::None
        } else {
            ScrollAction::Viewport { cells }
        }
    }

    /// Advance momentum one frame; returns the whole-cell viewport delta to
    /// apply (`+` up / `−` down). A strict no-op at `dt == 0.0` (determinism
    /// contract). The caller wall-clamps the delta and calls [`Self::stop`]
    /// at a scrollback bound so the glide doesn't grind against it.
    pub fn tick(&mut self, dt: f32) -> i32 {
        self.kinetics.tick(dt, self.config.momentum.friction)
    }

    /// Drive (or release) the drag auto-scroll sustained velocity from the
    /// pointer's RAW row relative to the viewport: `raw_row < 0` is above the
    /// top (scroll up into history), `raw_row >= rows` is below the bottom
    /// (scroll down toward the tail), inside releases any sustained drive.
    /// Returns whether a sustained drive is active after the call. A no-op
    /// (beyond releasing) when auto-scroll is disabled.
    pub fn update_autoscroll(&mut self, raw_row: f32, rows: f32) -> bool {
        if !self.config.autoscroll.enabled {
            self.end_autoscroll();
            return false;
        }
        let a = self.config.autoscroll;
        if raw_row < 0.0 {
            let over = (-raw_row).min(a.max_overshoot);
            self.kinetics.set_sustained(over * a.velocity_per_line);
            true
        } else if raw_row >= rows {
            let over = (raw_row - (rows - 1.0)).min(a.max_overshoot);
            self.kinetics.set_sustained(-over * a.velocity_per_line);
            true
        } else {
            self.end_autoscroll();
            false
        }
    }

    /// Release a sustained drag auto-scroll drive (drag ended, pointer back
    /// inside, or auto-scroll disabled). Leaves a decaying wheel glide alone.
    pub fn end_autoscroll(&mut self) {
        if self.kinetics.is_sustained() {
            self.kinetics.stop();
        }
    }

    /// Halt all motion immediately — used at a scrollback wall (so momentum
    /// doesn't fight the bound) and to cancel a drive.
    pub fn stop(&mut self) {
        self.kinetics.stop();
    }

    /// Whether any motion is pending (a live glide or sustained drive) — the
    /// engine skips the per-frame tick entirely when this is false.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.kinetics.is_active()
    }

    /// Whether the current drive is a sustained drag auto-scroll (vs a
    /// decaying wheel glide). Test-only introspection — production drives the
    /// sustained state through `update_autoscroll` / `end_autoscroll`.
    #[cfg(test)]
    pub(crate) fn is_sustained(&self) -> bool {
        self.kinetics.is_sustained()
    }

    // ── Internal decision helpers ────────────────────────────────────────

    /// The IMMEDIATE whole-cell delta a gesture represents right now —
    /// the precise pixel accumulator for a trackpad, `ticks × per_notch` for
    /// a wheel. Used for direct viewport scroll AND for app/alt-screen
    /// forwarding (one report/arrow per cell). `+` up / `−` down.
    fn direct_cells(&mut self, gesture: ScrollGesture, ctx: &ScrollContext) -> i32 {
        match gesture {
            ScrollGesture::Precise { pixels } => {
                let multiplier = self.precise_multiplier();
                self.accumulate_pixels(pixels, multiplier, ctx.cell_height)
            }
            ScrollGesture::Wheel { ticks } => {
                let lines = self.wheel_lines(ticks);
                #[allow(clippy::cast_possible_truncation)]
                let signed = lines as i32;
                if ticks < 0.0 {
                    -signed
                } else {
                    signed
                }
            }
        }
    }

    /// Magnitude (in lines) of one wheel gesture: `max(|ticks|, 1) × per_notch`.
    /// Flooring the tick magnitude to ≥1 counters platforms (macOS) that ramp
    /// non-precision magnitudes, where a slow notch can report ~0.1.
    fn wheel_lines(&self, ticks: f64) -> u32 {
        let per_notch = match self.config.wheel {
            WheelMode::Lines { per_notch } | WheelMode::Momentum { per_notch } => per_notch.max(1),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let notches = ticks.abs().trunc().max(1.0) as u32;
        notches.saturating_mul(per_notch)
    }

    /// The precise pixel multiplier from whichever precise mode is active.
    fn precise_multiplier(&self) -> f32 {
        match self.config.precise {
            PreciseMode::Pixels { multiplier } | PreciseMode::Momentum { multiplier } => multiplier,
        }
    }

    /// Ghostty's precise-scroll pixel accumulator (Surface.zig `scrollCallback`
    /// precision branch). `pixels` is a PHYSICAL-pixel delta; `cell_height` is
    /// physical too (winit's precise delta is already `to_physical`-scaled, so
    /// no scale-factor division — that would double the coast speed on a
    /// Retina display). Returns the whole-cell delta and carries the SIGNED
    /// sub-cell remainder forward. The divide is guarded by
    /// `cell_height.max(1.0)` so a precise event before the first measured
    /// frame (cell height can init to 0) can't poison the accumulator with
    /// NaN/inf (mirrors the renderer's own `.max(1.0)` guard).
    fn accumulate_pixels(&mut self, pixels: f64, multiplier: f32, cell_height: f32) -> i32 {
        let ch = cell_height.max(1.0);
        #[allow(clippy::cast_possible_truncation)]
        let delta_px = pixels as f32 * multiplier.max(0.0);
        let poff = self.pixel_remainder + delta_px;
        if poff.abs() < ch {
            // Not enough accumulated for one cell — store and move nothing.
            self.pixel_remainder = poff;
            return 0;
        }
        // Peel whole cells (trunc toward zero — never overshoot the
        // accumulated travel) and carry the signed leftover.
        let cells = (poff / ch).trunc();
        self.pixel_remainder = poff - cells * ch;
        #[allow(clippy::cast_possible_truncation)]
        let out = cells as i32;
        out
    }

    /// Whether this gesture drives the synthetic momentum glide on the
    /// viewport (vs an immediate jump). Wheel → [`WheelMode::Momentum`];
    /// precise → [`PreciseMode::Momentum`].
    fn viewport_uses_momentum(&self, gesture: ScrollGesture) -> bool {
        match gesture {
            ScrollGesture::Wheel { .. } => matches!(self.config.wheel, WheelMode::Momentum { .. }),
            ScrollGesture::Precise { .. } => {
                matches!(self.config.precise, PreciseMode::Momentum { .. })
            }
        }
    }

    /// The signed velocity impulse (lines/sec) a gesture injects in a
    /// `Momentum` mode. Wheel: `lines × WHEEL_IMPULSE_GAIN`. Precise: pixels ×
    /// multiplier as a lines/sec contribution (approximate by design — the
    /// glide is shaped by `friction`/`max_velocity`, not by exact units).
    fn momentum_velocity(&self, gesture: ScrollGesture) -> f32 {
        match gesture {
            ScrollGesture::Wheel { ticks } => {
                #[allow(clippy::cast_precision_loss)]
                let mag = self.wheel_lines(ticks) as f32 * WHEEL_IMPULSE_GAIN;
                if ticks < 0.0 {
                    -mag
                } else {
                    mag
                }
            }
            ScrollGesture::Precise { pixels } => {
                #[allow(clippy::cast_possible_truncation)]
                let v = pixels as f32 * self.precise_multiplier().max(0.0);
                v
            }
        }
    }

    // ── Test-only introspection ──────────────────────────────────────────

    /// Raw kinetic velocity (lines/sec) — `+` up, `−` down.
    #[cfg(test)]
    pub(crate) fn velocity(&self) -> f32 {
        self.kinetics.velocity()
    }

    /// Carried sub-cell pixel remainder (physical px).
    #[cfg(test)]
    pub(crate) fn pixel_remainder(&self) -> f32 {
        self.pixel_remainder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_H: f32 = 20.0;

    fn viewport_ctx() -> ScrollContext {
        ScrollContext {
            mouse_tracking: false,
            shift_bypass: false,
            alt_screen: false,
            cell_height: CELL_H,
        }
    }

    /// A config with the precise path in pure-pixel mode at 1× gain — the
    /// simplest math (`cell_height` px ⇒ exactly one cell).
    fn pixel_cfg(multiplier: f32) -> ScrollConfig {
        ScrollConfig {
            precise: PreciseMode::Pixels { multiplier },
            ..ScrollConfig::prescribed()
        }
    }

    // ── Precise pixel accumulator ────────────────────────────────────────

    #[test]
    fn precise_sub_cell_accumulates_then_peels_one_cell() {
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = viewport_ctx();
        // Half a cell up: not enough — no move, remainder carried.
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: CELL_H as f64 / 2.0 }, &ctx),
            ScrollAction::None
        );
        assert!((s.pixel_remainder() - CELL_H / 2.0).abs() < 1e-3);
        // Another half cell: crosses one cell.
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: CELL_H as f64 / 2.0 }, &ctx),
            ScrollAction::Viewport { cells: 1 }
        );
        assert!(s.pixel_remainder().abs() < 1e-3, "remainder nets to ~0 after a clean cell");
    }

    #[test]
    fn precise_multiplier_scales_speed() {
        // At 2× gain, half a cell of pixels peels a whole cell in one event.
        let mut s = ScrollSystem::new(pixel_cfg(2.0));
        let ctx = viewport_ctx();
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: CELL_H as f64 / 2.0 }, &ctx),
            ScrollAction::Viewport { cells: 1 }
        );
    }

    #[test]
    fn precise_remainder_is_signed_and_nets_on_reversal() {
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = viewport_ctx();
        // +0.7 cell up: carried, no move.
        let up = 0.7 * CELL_H as f64;
        assert_eq!(s.on_gesture(ScrollGesture::Precise { pixels: up }, &ctx), ScrollAction::None);
        // -0.4 cell down: nets to +0.3, still sub-cell, no move — NOT reset.
        let down = -0.4 * CELL_H as f64;
        assert_eq!(s.on_gesture(ScrollGesture::Precise { pixels: down }, &ctx), ScrollAction::None);
        assert!((s.pixel_remainder() - 0.3 * CELL_H).abs() < 1e-2, "reversal nets, never resets");
    }

    #[test]
    fn precise_round_trip_returns_to_start_no_drift() {
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = viewport_ctx();
        let mut net: i64 = 0;
        // 10 cells up then 10 cells down, in third-cell dribbles.
        let step = CELL_H as f64 / 3.0;
        for _ in 0..30 {
            if let ScrollAction::Viewport { cells } = s.on_gesture(ScrollGesture::Precise { pixels: step }, &ctx) {
                net += i64::from(cells);
            }
        }
        for _ in 0..30 {
            if let ScrollAction::Viewport { cells } = s.on_gesture(ScrollGesture::Precise { pixels: -step }, &ctx) {
                net += i64::from(cells);
            }
        }
        assert_eq!(net, 0, "equal up/down pixel travel must net to zero cells");
    }

    #[test]
    fn precise_guards_zero_cell_height() {
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = ScrollContext { cell_height: 0.0, ..viewport_ctx() };
        // A big precise delta with cell_height 0 must not NaN/inf the
        // remainder; cell_height clamps to 1.0 so it peels a bounded count.
        let action = s.on_gesture(ScrollGesture::Precise { pixels: 5.0 }, &ctx);
        assert!(matches!(action, ScrollAction::Viewport { .. }));
        assert!(s.pixel_remainder().is_finite(), "remainder must stay finite");
    }

    #[test]
    fn precise_direction_sign_maps_to_cell_sign() {
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = viewport_ctx();
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: CELL_H as f64 }, &ctx),
            ScrollAction::Viewport { cells: 1 }
        );
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: -CELL_H as f64 }, &ctx),
            ScrollAction::Viewport { cells: -1 }
        );
    }

    #[test]
    fn precise_never_drives_synthetic_momentum_in_pixel_mode() {
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = viewport_ctx();
        s.on_gesture(ScrollGesture::Precise { pixels: 3.0 * CELL_H as f64 }, &ctx);
        assert_eq!(s.velocity(), 0.0, "PreciseMode::Pixels injects no kinetic velocity");
        assert!(!s.is_active());
    }

    #[test]
    fn precise_cancels_in_flight_wheel_glide() {
        // Wheel-momentum flick builds a glide; a precise gesture must halt it
        // (direct manipulation) before applying its own cells — else the OS
        // coast and the synthetic glide double-scroll.
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Momentum { per_notch: 3 },
            precise: PreciseMode::Pixels { multiplier: 1.0 },
            ..ScrollConfig::prescribed()
        });
        let ctx = viewport_ctx();
        s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx);
        assert!(s.is_active(), "wheel-momentum flick starts a glide");
        s.on_gesture(ScrollGesture::Precise { pixels: 1.0 }, &ctx);
        assert_eq!(s.velocity(), 0.0, "precise gesture cancels the wheel glide");
    }

    // ── Wheel modes ──────────────────────────────────────────────────────

    #[test]
    fn wheel_lines_mode_scrolls_directly() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Lines { per_notch: 3 },
            ..ScrollConfig::prescribed()
        });
        let ctx = viewport_ctx();
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::Viewport { cells: 3 }
        );
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: -1.0 }, &ctx),
            ScrollAction::Viewport { cells: -3 }
        );
        assert_eq!(s.velocity(), 0.0, "Lines mode is a direct jump, no glide");
    }

    #[test]
    fn wheel_momentum_mode_injects_velocity_not_immediate_cells() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Momentum { per_notch: 2 },
            ..ScrollConfig::prescribed()
        });
        let ctx = viewport_ctx();
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::None,
            "momentum mode defers the move to tick()"
        );
        assert!(s.velocity() > 0.0, "an up flick injects positive velocity");
        // The glide then surfaces as viewport cells over frames.
        let mut moved = 0;
        for _ in 0..120 {
            moved += s.tick(1.0 / 60.0);
            if !s.is_active() {
                break;
            }
        }
        assert!(moved > 0, "the glide scrolls up over subsequent ticks");
    }

    // ── Forwarding sinks ─────────────────────────────────────────────────

    #[test]
    fn mouse_tracking_forwards_wheel_reports() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Lines { per_notch: 1 },
            ..ScrollConfig::prescribed()
        });
        let ctx = ScrollContext { mouse_tracking: true, ..viewport_ctx() };
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::ForwardWheel { up: true, count: 1 }
        );
    }

    #[test]
    fn shift_bypass_scrolls_scrollback_instead_of_forwarding() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Lines { per_notch: 1 },
            ..ScrollConfig::prescribed()
        });
        let ctx = ScrollContext { mouse_tracking: true, shift_bypass: true, ..viewport_ctx() };
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::Viewport { cells: 1 }
        );
    }

    #[test]
    fn forward_to_tracking_apps_disabled_keeps_scrollback() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Lines { per_notch: 1 },
            forward_to_tracking_apps: false,
            ..ScrollConfig::prescribed()
        });
        let ctx = ScrollContext { mouse_tracking: true, ..viewport_ctx() };
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::Viewport { cells: 1 },
            "disabling forwarding keeps the wheel on mado's scrollback"
        );
    }

    #[test]
    fn alt_screen_forwards_arrows() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Lines { per_notch: 1 },
            ..ScrollConfig::prescribed()
        });
        let ctx = ScrollContext { alt_screen: true, ..viewport_ctx() };
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::ForwardArrows { up: true, count: 1 }
        );
    }

    #[test]
    fn alt_screen_arrows_disabled_is_noop() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Lines { per_notch: 1 },
            alt_screen_arrows: false,
            ..ScrollConfig::prescribed()
        });
        let ctx = ScrollContext { alt_screen: true, ..viewport_ctx() };
        // No alt-scroll, and no scrollback on the alt screen either → None.
        assert_eq!(
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx),
            ScrollAction::Viewport { cells: 1 },
            "with alt arrows off the gesture falls through to the viewport path"
        );
    }

    #[test]
    fn precise_forwards_accumulated_cell_count_to_apps() {
        // A trackpad in a mouse-tracking app forwards ONE report per peeled
        // cell — the accumulator runs, so a sub-cell event forwards nothing.
        let mut s = ScrollSystem::new(pixel_cfg(1.0));
        let ctx = ScrollContext { mouse_tracking: true, ..viewport_ctx() };
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: CELL_H as f64 / 4.0 }, &ctx),
            ScrollAction::None,
            "sub-cell trackpad motion forwards nothing"
        );
        assert_eq!(
            s.on_gesture(ScrollGesture::Precise { pixels: 2.0 * CELL_H as f64 }, &ctx),
            ScrollAction::ForwardWheel { up: true, count: 2 }
        );
    }

    // ── Auto-scroll ──────────────────────────────────────────────────────

    #[test]
    fn autoscroll_drives_sustained_velocity_past_edges() {
        let mut s = ScrollSystem::new(ScrollConfig::prescribed());
        // Pointer two lines above the top → sustained up velocity.
        assert!(s.update_autoscroll(-2.0, 24.0));
        assert!(s.is_sustained());
        assert!(s.velocity() > 0.0);
        // Back inside → released.
        assert!(!s.update_autoscroll(5.0, 24.0));
        assert!(!s.is_sustained());
    }

    #[test]
    fn autoscroll_speed_and_overshoot_are_live_knobs() {
        // Faster speed → higher sustained velocity for the same overshoot.
        let mut slow = ScrollSystem::new(ScrollConfig {
            autoscroll: AutoScroll { enabled: true, velocity_per_line: 10.0, max_overshoot: 6.0 },
            ..ScrollConfig::prescribed()
        });
        let mut fast = ScrollSystem::new(ScrollConfig {
            autoscroll: AutoScroll { enabled: true, velocity_per_line: 40.0, max_overshoot: 6.0 },
            ..ScrollConfig::prescribed()
        });
        slow.update_autoscroll(-3.0, 24.0);
        fast.update_autoscroll(-3.0, 24.0);
        assert!(fast.velocity() > slow.velocity(), "velocity_per_line is live");
        // Overshoot cap bounds the speed: a huge overshoot saturates.
        let mut capped = ScrollSystem::new(ScrollConfig {
            autoscroll: AutoScroll { enabled: true, velocity_per_line: 10.0, max_overshoot: 2.0 },
            ..ScrollConfig::prescribed()
        });
        capped.update_autoscroll(-100.0, 24.0);
        assert!((capped.velocity() - 20.0).abs() < 1e-3, "max_overshoot caps the speed");
    }

    #[test]
    fn autoscroll_disabled_never_drives() {
        let mut s = ScrollSystem::new(ScrollConfig {
            autoscroll: AutoScroll { enabled: false, velocity_per_line: 18.0, max_overshoot: 6.0 },
            ..ScrollConfig::prescribed()
        });
        assert!(!s.update_autoscroll(-5.0, 24.0));
        assert!(!s.is_sustained());
        assert_eq!(s.velocity(), 0.0);
    }

    // ── Momentum tuning knobs are live ───────────────────────────────────

    #[test]
    fn friction_is_a_live_knob() {
        // Lower friction → a longer glide (more total travel) from one flick.
        let travel = |friction: f32| {
            let mut s = ScrollSystem::new(ScrollConfig {
                wheel: WheelMode::Momentum { per_notch: 3 },
                momentum: MomentumTuning { friction, max_velocity: 4000.0 },
                ..ScrollConfig::prescribed()
            });
            s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &viewport_ctx());
            let mut total: i64 = 0;
            for _ in 0..600 {
                total += i64::from(s.tick(1.0 / 60.0));
                if !s.is_active() {
                    break;
                }
            }
            total
        };
        assert!(travel(1.0) > travel(8.0), "lower friction glides farther");
    }

    #[test]
    fn max_velocity_is_a_live_knob() {
        let peak = |cap: f32| {
            let mut s = ScrollSystem::new(ScrollConfig {
                wheel: WheelMode::Momentum { per_notch: 100 },
                momentum: MomentumTuning { friction: 3.0, max_velocity: cap },
                ..ScrollConfig::prescribed()
            });
            // A few hard flicks to saturate.
            for _ in 0..5 {
                s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &viewport_ctx());
            }
            s.velocity()
        };
        assert!((peak(80.0) - 80.0).abs() < 1e-3);
        assert!((peak(300.0) - 300.0).abs() < 1e-3);
    }

    // ── tick determinism ─────────────────────────────────────────────────

    #[test]
    fn tick_at_dt_zero_is_a_noop() {
        let mut s = ScrollSystem::new(ScrollConfig {
            wheel: WheelMode::Momentum { per_notch: 3 },
            ..ScrollConfig::prescribed()
        });
        s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &viewport_ctx());
        let before = s.velocity();
        assert_eq!(s.tick(0.0), 0, "dt=0 moves nothing");
        assert_eq!(s.velocity(), before, "dt=0 mutates nothing");
    }

    // ── Behavior matrix — every (wheel, precise) variant produces a sane
    // action over the viewport (the CLOSED-LOOP MASS-SYNTHESIS forcing
    // function: a new mode without a matrix row fails the build). ──────────

    #[test]
    fn behavior_matrix_every_mode_combination_is_sane() {
        let wheels = [WheelMode::Lines { per_notch: 2 }, WheelMode::Momentum { per_notch: 2 }];
        let precises = [PreciseMode::Pixels { multiplier: 2.0 }, PreciseMode::Momentum { multiplier: 1.0 }];
        let mut failures = vec![];
        for w in wheels {
            for p in precises {
                let mut s = ScrollSystem::new(ScrollConfig { wheel: w, precise: p, ..ScrollConfig::prescribed() });
                let ctx = viewport_ctx();
                // A wheel flick either moves the viewport (Lines) or arms a
                // glide (Momentum) — never forwards on a plain viewport.
                let wa = s.on_gesture(ScrollGesture::Wheel { ticks: 1.0 }, &ctx);
                let wheel_ok = match w {
                    WheelMode::Lines { .. } => matches!(wa, ScrollAction::Viewport { cells } if cells > 0),
                    WheelMode::Momentum { .. } => wa == ScrollAction::None && s.velocity() > 0.0,
                };
                if !wheel_ok {
                    failures.push(format!("wheel {w:?}/{p:?} → {wa:?}"));
                }
                // A precise gesture of several cells either moves (Pixels) or
                // arms the glide (Momentum).
                let mut s2 = ScrollSystem::new(ScrollConfig { wheel: w, precise: p, ..ScrollConfig::prescribed() });
                let pa = s2.on_gesture(ScrollGesture::Precise { pixels: 3.0 * CELL_H as f64 }, &ctx);
                let precise_ok = match p {
                    PreciseMode::Pixels { .. } => matches!(pa, ScrollAction::Viewport { cells } if cells > 0),
                    PreciseMode::Momentum { .. } => pa == ScrollAction::None && s2.velocity() > 0.0,
                };
                if !precise_ok {
                    failures.push(format!("precise {w:?}/{p:?} → {pa:?}"));
                }
            }
        }
        assert!(failures.is_empty(), "{} mode(s) misbehaved:\n  - {}", failures.len(), failures.join("\n  - "));
    }
}
