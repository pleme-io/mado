//! OSC 22 — mouse pointer shape control.
//!
//! Terminals advertise "the shell / app wants the mouse pointer to
//! look like `<shape>`" via `ESC ] 22 ; <name> ST`. The shape names
//! are the CSS `cursor` property vocabulary (`text`, `pointer`,
//! `wait`, …) so web apps / TUIs that cross into the terminal can
//! reuse one dictionary. Ghostty, kitty, and foot all implement
//! this. Mado did not — this module fills the gap with a typed
//! enum instead of a free-form string so round-trips don't leak
//! unvalidated names into renderer code.
//!
//! The query form `ESC ] 22 ; ? ST` asks the terminal for the
//! currently-active shape; mado responds with `ESC ] 22 ; <name> ST`
//! echoing the typed value's canonical name.
//!
//! ## Why a typed enum
//!
//! Every terminal that ships OSC 22 parses the argument as a string
//! and passes it straight to the platform cursor API. That's
//! two-plus rounds of untyped string matching (parse → validate →
//! platform lookup) and every round can drift against the CSS spec.
//! The typed variant collapses the three matches into one and
//! gives the platform layer a pattern-matchable enum that can't
//! be misspelled.

/// CSS `cursor`-style pointer shape. Round-trips through
/// [`Self::from_name`] ↔ [`Self::as_name`]. Unknown names decode to
/// `None` so the caller distinguishes "shape is `Default`" from
/// "the sender asked for a shape we don't recognize".
///
/// The vocabulary mirrors the CSS Basic User Interface Level 3
/// cursor keyword set; mado supports the entries ghostty / kitty /
/// foot do. Adding a new variant is an app-renderer concern — the
/// VT layer doesn't care what the OS does with the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerShape {
    /// Platform default pointer (typically an arrow).
    #[default]
    Default,
    /// I-beam text caret. Set by editors / `less` over text.
    Text,
    /// Link / clickable pointing hand.
    Pointer,
    /// Spinning wait indicator — app is unresponsive.
    Wait,
    /// In-progress indicator — app is busy but still interactive.
    Progress,
    /// Precision crosshair — often used by graphics tools.
    Crosshair,
    /// Horizontal column-resize double-arrow.
    ColResize,
    /// Vertical row-resize double-arrow.
    RowResize,
    /// Generic move indicator (four-way arrow).
    Move,
    /// Openhand — draggable area at rest.
    Grab,
    /// Closedhand — draggable area being dragged.
    Grabbing,
    /// Disabled / not-allowed slashed-circle.
    NotAllowed,
    /// Help question-mark pointer.
    Help,
    /// NW↔SE diagonal resize.
    NwseResize,
    /// NE↔SW diagonal resize.
    NeswResize,
    /// Horizontal bidirectional resize.
    EwResize,
    /// Vertical bidirectional resize.
    NsResize,
    /// Magnifier-plus.
    ZoomIn,
    /// Magnifier-minus.
    ZoomOut,
}

impl PointerShape {
    /// Parse a CSS-cursor name. Case-sensitive (CSS is too). Unknown
    /// names return `None` so the OSC 22 handler can log + ignore
    /// rather than silently falling back to `Default`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "default" => Self::Default,
            "text" => Self::Text,
            "pointer" => Self::Pointer,
            "wait" => Self::Wait,
            "progress" => Self::Progress,
            "crosshair" => Self::Crosshair,
            "col-resize" => Self::ColResize,
            "row-resize" => Self::RowResize,
            "move" => Self::Move,
            "grab" => Self::Grab,
            "grabbing" => Self::Grabbing,
            "not-allowed" => Self::NotAllowed,
            "help" => Self::Help,
            "nwse-resize" => Self::NwseResize,
            "nesw-resize" => Self::NeswResize,
            "ew-resize" => Self::EwResize,
            "ns-resize" => Self::NsResize,
            "zoom-in" => Self::ZoomIn,
            "zoom-out" => Self::ZoomOut,
            _ => return None,
        })
    }

    /// Canonical CSS-cursor name. Inverse of [`Self::from_name`].
    #[must_use]
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Text => "text",
            Self::Pointer => "pointer",
            Self::Wait => "wait",
            Self::Progress => "progress",
            Self::Crosshair => "crosshair",
            Self::ColResize => "col-resize",
            Self::RowResize => "row-resize",
            Self::Move => "move",
            Self::Grab => "grab",
            Self::Grabbing => "grabbing",
            Self::NotAllowed => "not-allowed",
            Self::Help => "help",
            Self::NwseResize => "nwse-resize",
            Self::NeswResize => "nesw-resize",
            Self::EwResize => "ew-resize",
            Self::NsResize => "ns-resize",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
        }
    }

    /// Every recognized variant. Useful for round-trip tests + the
    /// MCP tool that exposes the valid shape vocabulary to clients.
    #[must_use]
    pub fn all() -> &'static [PointerShape] {
        &[
            Self::Default,
            Self::Text,
            Self::Pointer,
            Self::Wait,
            Self::Progress,
            Self::Crosshair,
            Self::ColResize,
            Self::RowResize,
            Self::Move,
            Self::Grab,
            Self::Grabbing,
            Self::NotAllowed,
            Self::Help,
            Self::NwseResize,
            Self::NeswResize,
            Self::EwResize,
            Self::NsResize,
            Self::ZoomIn,
            Self::ZoomOut,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_name_preserves_every_variant() {
        // For each variant, render it as a name, parse that name,
        // compare. Pins the from_name ↔ as_name inverse invariant.
        for variant in PointerShape::all() {
            let name = variant.as_name();
            let parsed = PointerShape::from_name(name);
            assert_eq!(parsed, Some(*variant), "{name} failed round-trip");
        }
    }

    #[test]
    fn unknown_names_return_none() {
        assert!(PointerShape::from_name("").is_none());
        assert!(PointerShape::from_name("laser").is_none());
        // CSS is case-sensitive — mado must be too, otherwise
        // "POINTER" would silently match and round-trip to the
        // lowercase form.
        assert!(PointerShape::from_name("POINTER").is_none());
    }

    #[test]
    fn every_name_is_lowercase_ascii_with_optional_hyphen() {
        // Keeps the vocabulary aligned with CSS without us hand-
        // typing each variant's acceptance. A typo in `as_name`
        // that introduces e.g. an underscore would trip this.
        for variant in PointerShape::all() {
            let name = variant.as_name();
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-'),
                "{name} contains non-lowercase / non-hyphen chars",
            );
            assert!(!name.starts_with('-'), "{name} starts with hyphen");
            assert!(!name.ends_with('-'), "{name} ends with hyphen");
        }
    }

    #[test]
    fn default_resolves_to_default_variant() {
        // The #[default] attribute must line up with the "default"
        // CSS name — if that drifts, OSC 22 ; ? round-trips would
        // report something unexpected.
        let d = PointerShape::default();
        assert_eq!(d, PointerShape::Default);
        assert_eq!(d.as_name(), "default");
    }

    #[test]
    fn all_returns_canonical_count() {
        // Pin the variant count so adding a new variant needs a
        // conscious update (rather than slipping into the vocab
        // silently and getting missed by round-trip tests that
        // enumerate `::all()`).
        assert_eq!(PointerShape::all().len(), 19);
    }
}
