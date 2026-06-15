//! [`UxBehavior`] — the typed subset of `MadoConfig` the unified
//! input engine consumes.
//!
//! The engine never holds the whole `MadoConfig`: it names exactly
//! the knobs the lifted handlers read, so the config surface the
//! engine depends on is auditable at the type level (and the M0
//! ConfigCoverage invariant can point each knob at one consumer).

use crate::config::MadoConfig;

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
}

impl From<&MadoConfig> for UxBehavior {
    fn from(config: &MadoConfig) -> Self {
        Self {
            copy_on_select: config.behavior.copy_on_select,
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
