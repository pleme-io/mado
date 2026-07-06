//! [`UxBehavior`] — the typed subset of `MadoConfig` the unified
//! input engine consumes.
//!
//! The engine never holds the whole `MadoConfig`: it names exactly
//! the knobs the lifted handlers read, so the config surface the
//! engine depends on is auditable at the type level (and the M0
//! ConfigCoverage invariant can point each knob at one consumer).

use crate::config::{MadoConfig, PreciseScrollMode};
use crate::ux::{AutoScroll, MomentumTuning, PreciseMode, ScrollConfig, WheelMode};

/// Behavior knobs the input/UX engine consumes. Built once at
/// adapter setup via `From<&MadoConfig>`; both render modes derive
/// it from the same config so the knobs cannot diverge per mode.
/// (`Copy` dropped when `word_chars: String` landed — M3 review
/// 2026-06-12; clone at the adapter seam instead.)
// `Eq` dropped when the `f32` scroll-friction / max-velocity knobs
// landed (momentum scrolling) — `f32` is `PartialEq` but not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct UxBehavior {
    /// Selection release copies straight to the clipboard
    /// (the muscle-memory contract).
    pub copy_on_select: bool,
    /// After `copy_on_select` auto-copies on release, ALSO clear the
    /// highlight — so lifting the mouse both copies AND unhighlights and no
    /// click is ever needed to copy. Inert when `copy_on_select` is off.
    pub deselect_on_copy: bool,
    /// Window close requires a second close request. Consumed by the
    /// loop-side `exit_response` (close handling stays loop-specific
    /// — child-exit semantics differ per mode).
    pub confirm_close: bool,
    /// Hide the pointer while typing; restore on mouse move.
    pub mouse_hide_while_typing: bool,
    /// Wheel-scroll line multiplier for the mado-side scrollback view.
    pub mouse_scroll_multiplier: u32,
    /// Whether a mouse-tracking app may capture SHIFT-modified mouse
    /// events. `false` (default) reserves shift as the operator's
    /// bypass — shift+click selects, shift+wheel scrolls scrollback
    /// (the xterm/kitty/ghostty contract). `true` forwards
    /// shift-modified events to the app (iTerm2's
    /// "apps may capture shift" knob).
    pub mouse_shift_capture: bool,
    /// Double-click word-snap BOUNDARY set (non-empty replaces the
    /// not-alphanumeric-or-underscore default rule; listed chars
    /// split words), threaded from `config.selection.word_chars`.
    /// The M3 review (2026-06-12) found the sole production call
    /// site hard-coding `""`, leaving the operator knob dead — this
    /// field is the thread.
    pub word_chars: String,
    /// Momentum scrolling: a wheel flick injects velocity that eases
    /// to a stop instead of jumping line-for-line. `false` keeps the
    /// direct scroll (behavior-preserving opt-out).
    pub scroll_momentum: bool,
    /// Exponential friction (per second) bleeding off scroll velocity.
    pub scroll_friction: f32,
    /// Velocity cap (lines/sec) so a frantic flick can't launch to the
    /// scrollback top in one frame.
    pub scroll_max_velocity: f32,
    /// Selection auto-scroll: dragging past a viewport edge scrolls the
    /// viewport and extends the highlight to the revealed lines.
    pub selection_autoscroll: bool,
    /// Precise (trackpad) scroll behavior — pixel accumulator vs synthetic
    /// glide. Selects the precise arm of the scroll system.
    pub precise_scroll_mode: PreciseScrollMode,
    /// Precise-scroll pixel gain (trackpad only). `2.0` ≈ ghostty's macOS feel.
    pub precise_scroll_multiplier: f32,
    /// Selection auto-scroll sustained speed (lines/sec per overshoot line).
    pub selection_autoscroll_speed: f32,
    /// Selection auto-scroll overshoot cap (lines).
    pub selection_autoscroll_max_overshoot: f32,
}

impl UxBehavior {
    /// Project the flat scroll knobs into the typed [`ScrollConfig`] the
    /// [`crate::ux::ScrollSystem`] consumes. The ONE place the operator-facing
    /// YAML knobs map onto the scroll system's typed behavior selectors, so a
    /// new knob has exactly one wiring site.
    #[must_use]
    pub fn scroll_config(&self) -> ScrollConfig {
        let momentum = MomentumTuning {
            friction: self.scroll_friction,
            max_velocity: self.scroll_max_velocity,
        };
        ScrollConfig {
            wheel: if self.scroll_momentum {
                WheelMode::Momentum {
                    per_notch: self.mouse_scroll_multiplier,
                }
            } else {
                WheelMode::Lines {
                    per_notch: self.mouse_scroll_multiplier,
                }
            },
            precise: match self.precise_scroll_mode {
                PreciseScrollMode::Pixels => PreciseMode::Pixels {
                    multiplier: self.precise_scroll_multiplier,
                },
                PreciseScrollMode::Momentum => PreciseMode::Momentum {
                    multiplier: self.precise_scroll_multiplier,
                },
            },
            momentum,
            autoscroll: AutoScroll {
                enabled: self.selection_autoscroll,
                velocity_per_line: self.selection_autoscroll_speed,
                max_overshoot: self.selection_autoscroll_max_overshoot,
            },
            // `forward_to_tracking_apps` (wheel works in vim/htop) and
            // `alt_screen_arrows` (pagers scroll) are standard terminal
            // behavior, not yet operator-tunable — taken from the prescribed
            // baseline. The scroll system supports flipping them; mado pins
            // them on.
            ..ScrollConfig::prescribed()
        }
    }
}

impl From<&MadoConfig> for UxBehavior {
    fn from(config: &MadoConfig) -> Self {
        Self {
            copy_on_select: config.behavior.copy_on_select,
            deselect_on_copy: config.behavior.deselect_on_copy,
            confirm_close: config.behavior.confirm_close,
            mouse_hide_while_typing: config.behavior.mouse_hide_while_typing,
            mouse_scroll_multiplier: config.behavior.mouse_scroll_multiplier,
            mouse_shift_capture: matches!(
                config.behavior.mouse_shift_capture,
                crate::config::MouseShiftCapture::True
                    | crate::config::MouseShiftCapture::Always
            ),
            word_chars: config.selection.word_chars.clone(),
            scroll_momentum: config.behavior.scroll_momentum,
            scroll_friction: config.behavior.scroll_friction,
            scroll_max_velocity: config.behavior.scroll_max_velocity,
            selection_autoscroll: config.behavior.selection_autoscroll,
            precise_scroll_mode: config.behavior.precise_scroll_mode,
            precise_scroll_multiplier: config.behavior.precise_scroll_multiplier,
            selection_autoscroll_speed: config.behavior.selection_autoscroll_speed,
            selection_autoscroll_max_overshoot: config.behavior.selection_autoscroll_max_overshoot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_from_config_behavior_section() {
        let mut config = MadoConfig::default();
        config.behavior.copy_on_select = true;
        config.behavior.confirm_close = true;
        config.behavior.mouse_hide_while_typing = true;
        config.behavior.mouse_scroll_multiplier = 3;
        let b = UxBehavior::from(&config);
        assert!(b.copy_on_select);
        assert!(b.confirm_close);
        assert!(b.mouse_hide_while_typing);
        assert_eq!(b.mouse_scroll_multiplier, 3);
        // Default capture is False — shift stays the operator's
        // bypass unless the config opts the app in.
        assert!(!b.mouse_shift_capture);
        config.behavior.mouse_shift_capture = crate::config::MouseShiftCapture::Always;
        assert!(UxBehavior::from(&config).mouse_shift_capture);
    }

    #[test]
    fn scroll_config_projects_the_knobs() {
        use crate::ux::{PreciseMode, WheelMode};

        let mut config = MadoConfig::default();
        config.behavior.scroll_momentum = false;
        config.behavior.mouse_scroll_multiplier = 4;
        config.behavior.precise_scroll_mode = PreciseScrollMode::Pixels;
        config.behavior.precise_scroll_multiplier = 3.5;
        config.behavior.scroll_friction = 7.0;
        config.behavior.scroll_max_velocity = 150.0;
        config.behavior.selection_autoscroll = true;
        config.behavior.selection_autoscroll_speed = 25.0;
        config.behavior.selection_autoscroll_max_overshoot = 4.0;

        let sc = UxBehavior::from(&config).scroll_config();
        assert_eq!(sc.wheel, WheelMode::Lines { per_notch: 4 }, "momentum off → Lines");
        assert_eq!(sc.precise, PreciseMode::Pixels { multiplier: 3.5 });
        assert!((sc.momentum.friction - 7.0).abs() < 1e-6);
        assert!((sc.momentum.max_velocity - 150.0).abs() < 1e-6);
        assert!(sc.autoscroll.enabled);
        assert!((sc.autoscroll.velocity_per_line - 25.0).abs() < 1e-6);
        assert!((sc.autoscroll.max_overshoot - 4.0).abs() < 1e-6);

        // Momentum wheel + momentum trackpad project to the Momentum arms.
        config.behavior.scroll_momentum = true;
        config.behavior.precise_scroll_mode = PreciseScrollMode::Momentum;
        let sc = UxBehavior::from(&config).scroll_config();
        assert_eq!(sc.wheel, WheelMode::Momentum { per_notch: 4 });
        assert_eq!(sc.precise, PreciseMode::Momentum { multiplier: 3.5 });
    }

    #[test]
    fn word_chars_thread_from_selection_section() {
        let mut config = MadoConfig::default();
        config.selection.word_chars = ":@-".into();
        assert_eq!(UxBehavior::from(&config).word_chars, ":@-");
        // The prescribed default is non-empty (pinned in config.rs)
        // and must survive the projection — a hard-coded "" at any
        // consumer is the dead-knob regression this guards.
        let d = UxBehavior::from(&MadoConfig::default());
        assert_eq!(d.word_chars, MadoConfig::default().selection.word_chars);
    }
}
