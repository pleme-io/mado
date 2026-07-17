//! Pure snapping geometry — the decision core the operator's "floating **and**
//! snapping" goal turns on. No near-miss existed in mado; this is the net-new
//! pure module (modelled on [`crate::ux::scroll::ScrollSystem`]'s pure-edge
//! shape: typed input + a by-value context → a typed action out, zero I/O).
//!
//! Layering:
//! - [`ZoneGeom`] maps a viewport rect → a target rect (left/right/top/bottom
//!   half, corner quadrant, maximize, or a custom edge-fraction).
//! - [`Trigger`] is the cursor activation band (an edge or corner region).
//! - [`SnapZone`] pairs a name + geometry + optional hover trigger. The
//!   built-in zones are authored *once* through the [`snap_zones!`] Layer-B
//!   table macro (modelled on `terminal.rs`'s `dec_private_modes!`), which
//!   generates the resolve / list / classify surfaces from that one table — no
//!   drift across the three. Custom zones arrive at runtime from
//!   `(defsnapzone …)` declarations.
//! - [`SnapSystem`] holds the zone set + a `Copy` [`SnapConfig`] and answers
//!   [`SnapSystem::resolve`] purely.

use std::borrow::Cow;

use egaku::Rect;

use super::geom::{Corner, Edge, RectExt};

/// How a snap zone maps a viewport rect to a target rect. `Copy`, pure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoneGeom {
    /// A half of the viewport hugging `edge` (left/right/top/bottom half).
    Half(Edge),
    /// A quarter of the viewport at `corner`.
    Quadrant(Corner),
    /// The whole viewport.
    Maximize,
    /// A custom `frac`-of-`edge` slice (authored via `(defsnapzone …)`).
    EdgeFraction { edge: Edge, frac: f32 },
}

impl ZoneGeom {
    /// Resolve to a concrete rect within `vp`, rounded to whole pixels and
    /// clamped inside the viewport. Pure + total.
    #[must_use]
    pub fn resolve(self, vp: Rect) -> Rect {
        let r = match self {
            ZoneGeom::Half(edge) => vp.edge_fraction(edge, 0.5),
            ZoneGeom::Quadrant(corner) => vp.quadrant(corner),
            ZoneGeom::Maximize => vp,
            ZoneGeom::EdgeFraction { edge, frac } => vp.edge_fraction(edge, frac),
        };
        r.round_to_int_px().clamp_within(vp.round_to_int_px())
    }
}

/// The cursor activation region that triggers a hover-snap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trigger {
    /// The band hugging one screen edge (activates a half).
    Edge(Edge),
    /// The band in one screen corner (activates a quadrant).
    Corner(Corner),
}

impl Trigger {
    /// Does `cursor` fall in this trigger's activation band of `vp`? `band` is
    /// the band thickness as a fraction of the viewport dimension (clamped to
    /// `0.0..=0.5`). Pure.
    #[must_use]
    pub fn contains(self, cursor: (f32, f32), vp: Rect, band: f32) -> bool {
        let band = band.clamp(0.0, 0.5);
        let (cx, cy) = cursor;
        if !vp.contains(cx, cy) {
            return false;
        }
        let bx = vp.width * band;
        let by = vp.height * band;
        let near_left = cx <= vp.x + bx;
        let near_right = cx >= vp.right() - bx;
        let near_top = cy <= vp.y + by;
        let near_bottom = cy >= vp.bottom() - by;
        match self {
            Trigger::Corner(Corner::TopLeft) => near_left && near_top,
            Trigger::Corner(Corner::TopRight) => near_right && near_top,
            Trigger::Corner(Corner::BottomLeft) => near_left && near_bottom,
            Trigger::Corner(Corner::BottomRight) => near_right && near_bottom,
            Trigger::Edge(Edge::Left) => near_left,
            Trigger::Edge(Edge::Right) => near_right,
            Trigger::Edge(Edge::Top) => near_top,
            Trigger::Edge(Edge::Bottom) => near_bottom,
        }
    }
}

/// A named snap zone: a geometry + an optional hover trigger. Built-ins carry a
/// `'static` name; `(defsnapzone …)` custom zones carry an owned name — hence
/// [`Cow`]. Clone (not Copy) because of the name.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapZone {
    /// Zone name — the MCP / lisp / keybind key (`"left-half"`, `"top-right"`).
    pub name: Cow<'static, str>,
    /// How this zone maps the viewport to a rect.
    pub geom: ZoneGeom,
    /// The cursor band that hover-activates this zone during a drag. `None` =
    /// name/keybind-only (e.g. `maximize`), never triggered by hover.
    pub trigger: Option<Trigger>,
}

/// The one authored built-in snap-zone table. `snap_zones!` generates, from
/// this single source, the resolve-by-name / list / classify surfaces — the
/// Layer-B "one table, no drift" idiom (see `terminal.rs::dec_private_modes!`).
macro_rules! snap_zones {
    ( $( $name:literal => $geom:expr, trigger: $trig:expr ),* $(,)? ) => {
        /// Every built-in snap-zone name — the reflection const, complete by
        /// construction (a new table row cannot be omitted from it).
        pub const BUILTIN_ZONE_NAMES: &[&str] = &[ $( $name ),* ];

        /// Resolve a built-in zone name to its [`ZoneGeom`] (`None` if unknown).
        #[must_use]
        pub fn builtin_zone_geom(name: &str) -> ::core::option::Option<ZoneGeom> {
            match name {
                $( $name => ::core::option::Option::Some($geom), )*
                _ => ::core::option::Option::None,
            }
        }

        /// The full built-in [`SnapZone`] set a fresh [`SnapSystem`] starts with.
        #[must_use]
        pub fn builtin_zones() -> ::std::vec::Vec<SnapZone> {
            ::std::vec![ $(
                SnapZone {
                    name: ::std::borrow::Cow::Borrowed($name),
                    geom: $geom,
                    trigger: $trig,
                }
            ),* ]
        }

        /// Which built-in zone (if any) a `rect` currently *fills* within `vp`
        /// (integer-px exact match) — the snap fixpoint / "am I already
        /// snapped?" query.
        #[must_use]
        pub fn classify_builtin(rect: Rect, vp: Rect) -> ::core::option::Option<&'static str> {
            $( if $geom.resolve(vp) == rect { return ::core::option::Option::Some($name); } )*
            ::core::option::Option::None
        }
    };
}

// Corners are listed before edges so a corner (the more specific region) wins
// over a half when the cursor is in a corner band — first-match in resolve().
snap_zones! {
    "top-left"     => ZoneGeom::Quadrant(Corner::TopLeft),     trigger: Some(Trigger::Corner(Corner::TopLeft)),
    "top-right"    => ZoneGeom::Quadrant(Corner::TopRight),    trigger: Some(Trigger::Corner(Corner::TopRight)),
    "bottom-left"  => ZoneGeom::Quadrant(Corner::BottomLeft),  trigger: Some(Trigger::Corner(Corner::BottomLeft)),
    "bottom-right" => ZoneGeom::Quadrant(Corner::BottomRight), trigger: Some(Trigger::Corner(Corner::BottomRight)),
    "left-half"    => ZoneGeom::Half(Edge::Left),              trigger: Some(Trigger::Edge(Edge::Left)),
    "right-half"   => ZoneGeom::Half(Edge::Right),             trigger: Some(Trigger::Edge(Edge::Right)),
    "top-half"     => ZoneGeom::Half(Edge::Top),               trigger: Some(Trigger::Edge(Edge::Top)),
    "bottom-half"  => ZoneGeom::Half(Edge::Bottom),            trigger: Some(Trigger::Edge(Edge::Bottom)),
    "maximize"     => ZoneGeom::Maximize,                      trigger: None,
}

/// The drag phase a [`SnapSystem::resolve`] call is answering for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    /// Mid-drag — the answer is a *preview* (draw the zone outline, do not move).
    Moving,
    /// Pointer released — the answer is a *commit* (place the window there).
    Released,
}

/// A snap decision. `Copy` (holds only a `Rect`), matches the pure-edge shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SnapAction {
    /// The cursor is in no zone — free float, no snapping.
    None,
    /// Show this rect as a snap preview (mid-drag).
    Preview(Rect),
    /// Commit the surface to this rect (pointer released over a zone).
    Commit(Rect),
}

/// Copy config for [`SnapSystem`] — mirrors the `ScrollConfig` pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapConfig {
    /// Activation-band thickness, as a fraction of the viewport dimension.
    pub band: f32,
    /// Master enable. When `false`, [`SnapSystem::resolve`] always returns
    /// [`SnapAction::None`] (free-float only) — the dead-knob-tested off state.
    pub enabled: bool,
}

impl Default for SnapConfig {
    fn default() -> Self {
        Self {
            band: 0.06,
            enabled: true,
        }
    }
}

/// The snapping engine: a zone set + a `Copy` config, answering [`Self::resolve`]
/// purely. Holds no clock and no I/O — the whole "should this drag snap, and
/// where?" decision is a pure function of its inputs, so it is proven by plain
/// construction + typed-input assertions with zero mocks.
#[derive(Debug, Clone)]
pub struct SnapSystem {
    config: SnapConfig,
    zones: Vec<SnapZone>,
}

impl Default for SnapSystem {
    fn default() -> Self {
        Self::new(SnapConfig::default())
    }
}

impl SnapSystem {
    /// A fresh system with the built-in zone set.
    #[must_use]
    pub fn new(config: SnapConfig) -> Self {
        Self {
            config,
            zones: builtin_zones(),
        }
    }

    /// A system seeded with an explicit zone set (built-ins + custom).
    #[must_use]
    pub fn with_zones(config: SnapConfig, zones: Vec<SnapZone>) -> Self {
        Self { config, zones }
    }

    /// Hot-reload the config (mirrors `ScrollSystem::set_config`).
    pub fn set_config(&mut self, config: SnapConfig) {
        self.config = config;
    }

    /// Append a custom zone (from a `(defsnapzone …)` declaration).
    pub fn add_zone(&mut self, zone: SnapZone) {
        self.zones.push(zone);
    }

    /// The live zone set.
    #[must_use]
    pub fn zones(&self) -> &[SnapZone] {
        &self.zones
    }

    /// The current config.
    #[must_use]
    pub fn config(&self) -> SnapConfig {
        self.config
    }

    /// The pure snap decision: given the cursor, the dragged window rect (kept
    /// for future edge-aware policies; unused today), the viewport, and the drag
    /// phase, decide whether/where to snap. First hover-triggered zone wins
    /// (corners before edges, by table order). Disabled → always `None`.
    #[must_use]
    pub fn resolve(
        &self,
        cursor: (f32, f32),
        _window: Rect,
        viewport: Rect,
        phase: DragPhase,
    ) -> SnapAction {
        if !self.config.enabled {
            return SnapAction::None;
        }
        for zone in &self.zones {
            if let Some(trigger) = zone.trigger {
                if trigger.contains(cursor, viewport, self.config.band) {
                    let rect = zone.geom.resolve(viewport);
                    return match phase {
                        DragPhase::Moving => SnapAction::Preview(rect),
                        DragPhase::Released => SnapAction::Commit(rect),
                    };
                }
            }
        }
        SnapAction::None
    }

    /// Resolve a zone *by name* to its rect within `viewport` — the path
    /// `browser_snap id :left-half` / `(mado-browser-snap id :left-half)` and a
    /// keybind take (no cursor, no hover). `None` if the name is unknown.
    #[must_use]
    pub fn resolve_named(&self, name: &str, viewport: Rect) -> Option<Rect> {
        self.zones
            .iter()
            .find(|z| z.name == name)
            .map(|z| z.geom.resolve(viewport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn vp() -> Rect {
        Rect::new(0.0, 0.0, 1000.0, 800.0)
    }

    #[test]
    fn builtin_table_is_complete_and_named() {
        // The three generated surfaces agree on the same 9 zones.
        assert_eq!(BUILTIN_ZONE_NAMES.len(), 9);
        assert_eq!(builtin_zones().len(), 9);
        for name in BUILTIN_ZONE_NAMES {
            assert!(builtin_zone_geom(name).is_some(), "no geom for {name}");
        }
        assert!(builtin_zone_geom("nope").is_none());
    }

    #[test]
    fn resolve_named_produces_the_expected_rects() {
        let s = SnapSystem::default();
        let v = vp();
        assert_eq!(s.resolve_named("left-half", v), Some(Rect::new(0.0, 0.0, 500.0, 800.0)));
        assert_eq!(s.resolve_named("maximize", v), Some(v));
        assert_eq!(
            s.resolve_named("bottom-right", v),
            Some(Rect::new(500.0, 400.0, 500.0, 400.0)),
        );
        assert_eq!(s.resolve_named("does-not-exist", v), None);
    }

    #[test]
    fn cursor_in_a_corner_snaps_to_the_quadrant_not_the_half() {
        let s = SnapSystem::default();
        let v = vp();
        // Top-left corner: expect the TL quadrant (corner beats edge).
        match s.resolve((5.0, 5.0), v, v, DragPhase::Moving) {
            SnapAction::Preview(r) => assert_eq!(r, Rect::new(0.0, 0.0, 500.0, 400.0)),
            other => panic!("expected TL quadrant preview, got {other:?}"),
        }
        // Left edge mid-height: expect the left half.
        match s.resolve((5.0, 400.0), v, v, DragPhase::Released) {
            SnapAction::Commit(r) => assert_eq!(r, Rect::new(0.0, 0.0, 500.0, 800.0)),
            other => panic!("expected left-half commit, got {other:?}"),
        }
        // Center: no zone.
        assert_eq!(s.resolve((500.0, 400.0), v, v, DragPhase::Moving), SnapAction::None);
    }

    #[test]
    fn disabled_never_snaps() {
        let mut s = SnapSystem::default();
        s.set_config(SnapConfig {
            band: 0.06,
            enabled: false,
        });
        assert_eq!(s.resolve((5.0, 5.0), vp(), vp(), DragPhase::Released), SnapAction::None);
    }

    #[test]
    fn classify_is_the_fixpoint_of_resolve_named() {
        let s = SnapSystem::default();
        let v = vp();
        for name in BUILTIN_ZONE_NAMES {
            let rect = s.resolve_named(name, v).unwrap();
            // A rect produced by a zone classifies back to *a* zone that fills
            // the same area (maximize + a full-size zone could alias; assert the
            // rect is recognised, and self-consistent).
            let back = classify_builtin(rect, v);
            assert!(back.is_some(), "zone {name} rect not classifiable");
            // Re-resolving the classified zone reproduces the identical rect.
            let reflowed = s.resolve_named(back.unwrap(), v).unwrap();
            assert_eq!(reflowed, rect, "classify→resolve not a fixpoint for {name}");
        }
    }

    proptest! {
        #[test]
        fn snapped_rect_is_always_inside_the_viewport_and_integer_px(
            cx in 0.0f32..1000.0, cy in 0.0f32..800.0,
            released in any::<bool>(),
        ) {
            let s = SnapSystem::default();
            let v = vp();
            let phase = if released { DragPhase::Released } else { DragPhase::Moving };
            let rect = match s.resolve((cx, cy), v, v, phase) {
                SnapAction::None => return Ok(()),
                SnapAction::Preview(r) | SnapAction::Commit(r) => r,
            };
            // Inside the viewport.
            prop_assert!(rect.x >= v.x - 0.01 && rect.y >= v.y - 0.01);
            prop_assert!(rect.right() <= v.right() + 0.01 && rect.bottom() <= v.bottom() + 0.01);
            // Integer pixels.
            prop_assert_eq!(rect.x, rect.x.round());
            prop_assert_eq!(rect.width, rect.width.round());
        }

        #[test]
        fn resolve_is_deterministic(
            cx in 0.0f32..1000.0, cy in 0.0f32..800.0,
        ) {
            let s = SnapSystem::default();
            let v = vp();
            let a = s.resolve((cx, cy), v, v, DragPhase::Moving);
            let b = s.resolve((cx, cy), v, v, DragPhase::Moving);
            prop_assert_eq!(a, b);
        }
    }
}
