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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl From<&MadoConfig> for UxBehavior {
    fn from(config: &MadoConfig) -> Self {
        Self {
            copy_on_select: config.behavior.copy_on_select,
            confirm_close: config.behavior.confirm_close,
            mouse_hide_while_typing: config.behavior.mouse_hide_while_typing,
            mouse_scroll_multiplier: config.behavior.mouse_scroll_multiplier,
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
    }
}
