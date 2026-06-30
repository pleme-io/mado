//! [`EventOutcome`] — the typed result every [`crate::ux::InputEngine`]
//! method returns, plus the TOTAL mapping onto `madori::EventResponse`.
//!
//! The engine never names `EventResponse` itself: outcomes are
//! render-loop-agnostic values; the adapters call `.into()` at the
//! loop boundary. Totality is enforced at compile time — the `From`
//! impl constructs `EventResponse` with NO `..Default::default()`
//! spread, so madori adding a response field breaks this build until
//! the mapping names it.

/// What an input event amounted to. Field-for-field peer of
/// `madori::EventResponse` (every field round-trips — see
/// `total_mapping_round_trips`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventOutcome {
    /// Whether the event was consumed (prevents default handling).
    pub consumed: bool,
    /// Request the event loop to exit.
    pub exit: bool,
    /// Request a window title change.
    pub set_title: Option<String>,
    /// Request fullscreen toggle.
    pub toggle_fullscreen: bool,
    /// Request cursor visibility change (None = no change).
    pub set_cursor_visible: Option<bool>,
    /// Request a mouse pointer SHAPE change (None = no change) — e.g. the
    /// hand/`Pointer` shape while hovering a clickable link. This is a
    /// mado-side typed decision with NO `madori::EventResponse` peer yet:
    /// the pinned madori exposes only `set_cursor_visible`, not a
    /// cursor-ICON channel, so the `From` impl below cannot forward it to
    /// the OS window. Tracked here (and unit-tested) at the engine boundary
    /// exactly like the OSC 22 pointer shape — both await a madori
    /// `set_cursor_icon` field to reach the live cursor.
    pub set_pointer_shape: Option<crate::pointer_shape::PointerShape>,
}

impl EventOutcome {
    /// Outcome that consumes the event.
    #[must_use]
    pub fn consumed() -> Self {
        Self {
            consumed: true,
            ..Default::default()
        }
    }

    /// Outcome that does not consume the event.
    #[must_use]
    pub fn ignored() -> Self {
        Self::default()
    }

    /// Attach a cursor-visibility change when `mouse_hide_while_typing`
    /// flipped it (the `with_cursor_visibility` helper lifted from
    /// `main.rs` — `None` leaves the outcome untouched).
    #[must_use]
    pub fn with_cursor_visibility(mut self, visible: Option<bool>) -> Self {
        if let Some(v) = visible {
            self.set_cursor_visible = Some(v);
        }
        self
    }
}

impl From<EventOutcome> for madori::EventResponse {
    fn from(o: EventOutcome) -> Self {
        // Deliberately NO `..Default::default()` — when madori grows
        // a response field this stops compiling and the mapping must
        // name it. Totality by construction.
        //
        // `o.set_pointer_shape` has no `EventResponse` peer on the pinned
        // madori (it carries `set_cursor_visible` but no cursor-ICON
        // channel), so it is intentionally not forwarded here — it would
        // reach the OS window once madori grows a `set_cursor_icon` field.
        madori::EventResponse {
            consumed: o.consumed,
            exit: o.exit,
            set_title: o.set_title,
            toggle_fullscreen: o.toggle_fullscreen,
            set_cursor_visible: o.set_cursor_visible,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acceptance (REMEDIATION-PLAN §M1): EventOutcome→EventResponse
    /// total-mapping — every field round-trips. Every field is set to
    /// a non-default value so a dropped field can't hide behind a
    /// default.
    #[test]
    fn total_mapping_round_trips() {
        let outcome = EventOutcome {
            consumed: true,
            exit: true,
            set_title: Some("mado — press close again to exit".into()),
            toggle_fullscreen: true,
            set_cursor_visible: Some(false),
            // mado-side only — no EventResponse peer (see the From impl).
            set_pointer_shape: Some(crate::pointer_shape::PointerShape::Pointer),
        };
        let resp: madori::EventResponse = outcome.clone().into();
        assert_eq!(resp.consumed, outcome.consumed);
        assert_eq!(resp.exit, outcome.exit);
        assert_eq!(resp.set_title, outcome.set_title);
        assert_eq!(resp.toggle_fullscreen, outcome.toggle_fullscreen);
        assert_eq!(resp.set_cursor_visible, outcome.set_cursor_visible);
    }

    #[test]
    fn pointer_shape_is_mado_side_only_and_defaults_to_none() {
        // The hover pointer shape is tracked on the outcome but has no
        // madori EventResponse peer yet, so it never forces a default-cursor
        // request: a fresh outcome carries None (no change).
        assert!(EventOutcome::ignored().set_pointer_shape.is_none());
        assert!(EventOutcome::consumed().set_pointer_shape.is_none());
        let hover = EventOutcome {
            set_pointer_shape: Some(crate::pointer_shape::PointerShape::Pointer),
            ..EventOutcome::consumed()
        };
        assert_eq!(
            hover.set_pointer_shape,
            Some(crate::pointer_shape::PointerShape::Pointer),
        );
    }

    #[test]
    fn default_outcome_maps_to_ignored_response() {
        let resp: madori::EventResponse = EventOutcome::ignored().into();
        assert!(!resp.consumed);
        assert!(!resp.exit);
        assert!(resp.set_title.is_none());
        assert!(!resp.toggle_fullscreen);
        assert!(resp.set_cursor_visible.is_none());
    }

    #[test]
    fn consumed_outcome_maps_to_consumed_response() {
        let resp: madori::EventResponse = EventOutcome::consumed().into();
        assert!(resp.consumed);
        assert!(!resp.exit);
    }

    // The `with_cursor_visibility` contract lifted from the pre-M1
    // `main.rs` helper — same asserted behavior, new seam.

    #[test]
    fn with_cursor_visibility_some_true() {
        let o = EventOutcome::consumed().with_cursor_visibility(Some(true));
        assert_eq!(o.set_cursor_visible, Some(true));
        assert!(o.consumed);
    }

    #[test]
    fn with_cursor_visibility_some_false() {
        let o = EventOutcome::consumed().with_cursor_visibility(Some(false));
        assert_eq!(o.set_cursor_visible, Some(false));
    }

    #[test]
    fn with_cursor_visibility_none_is_no_change() {
        let o = EventOutcome::consumed().with_cursor_visibility(None);
        assert!(o.set_cursor_visible.is_none());
    }
}
