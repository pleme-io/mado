//! Declarative authoring border — the tatara-lisp `(deffloatingbrowser …)` /
//! `(defsnapzone …)` forms (org Pillar 1: Rust owns types + invariants,
//! tatara-lisp owns declarative authoring; the boundary is one proc-macro,
//! `#[derive(DeriveTataraDomain)]`).
//!
//! A spec is a typed *declaration*; [`FloatingBrowserSpec::compile`] /
//! [`SnapZoneSpec::compile`] project it — totally, with typed errors, never a
//! silent default — onto the pure L0 primitives the geometry core already
//! speaks ([`ResolvedBrowser`] + [`SnapZone`]). So a browser layout authored
//! once in lisp drives the *same* typed core the MCP and GUI paths do — one
//! algebra, three front-ends.
//!
//! Example:
//! ```lisp
//! (deffloatingbrowser :url "https://example.com" :snap-zone "right-half"
//!                     :z 1 :opacity 0.96)
//! (defsnapzone :name "left-third" :edge "left" :fraction 0.33)
//! ```
//! Field idents kebab-convert to keywords automatically
//! (`snap_zone` → `:snap-zone`). Numeric geometry is pixels; `opacity` /
//! `fraction` are `0.0..=1.0`.

use std::borrow::Cow;

use egaku::Rect;
use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;
use url::Url;

use super::geom::{Edge, RectExt};
use super::snap::{SnapZone, Trigger, ZoneGeom};

/// A typed spec-compilation failure — the ABSENCE of a silent bad default. A
/// malformed URL or an unknown edge is a *visible* error, never a fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// `:url` did not parse as a URL.
    BadUrl(String),
    /// `:edge` was not one of `left`/`right`/`top`/`bottom`.
    UnknownEdge(String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::BadUrl(u) => write!(f, "not a valid url: {u}"),
            SpecError::UnknownEdge(e) => {
                write!(f, "unknown edge: {e} (want left/right/top/bottom)")
            }
        }
    }
}

impl std::error::Error for SpecError {}

/// Where a declared surface opens. `Zone`/`At` are resolved against the live
/// viewport at open time (the spec stays viewport-independent + pure).
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Open into a named snap zone (resolved against the live viewport).
    Zone(String),
    /// Open at an explicit rect (already integer-px).
    At(Rect),
    /// Open at the host's default floating rect.
    Default,
}

/// A validated, projected [`FloatingBrowserSpec`] — the value the L4 control
/// layer opens.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedBrowser {
    /// The (parsed, valid) initial URL.
    pub url: Url,
    /// Where the surface opens.
    pub placement: Placement,
    /// Initial stacking hint.
    pub z: u16,
    /// Surface opacity, clamped `0.0..=1.0`.
    pub opacity: f32,
}

/// `(deffloatingbrowser :url "…" :x 100 :y 80 :w 900 :h 600
///  :snap-zone "right-half" :z 1 :opacity 0.96)` — one declared floating
/// browser surface. All geometry is optional: an explicit `:snap-zone`
/// overrides `:x/:y/:w/:h`, and absent geometry opens at the host default.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[tatara(keyword = "deffloatingbrowser")]
pub struct FloatingBrowserSpec {
    /// Initial URL to navigate to.
    pub url: String,
    /// Left edge (panel px).
    #[serde(default)]
    pub x: Option<f64>,
    /// Top edge (panel px).
    #[serde(default)]
    pub y: Option<f64>,
    /// Width (panel px).
    #[serde(default)]
    pub w: Option<f64>,
    /// Height (panel px).
    #[serde(default)]
    pub h: Option<f64>,
    /// Open into this named snap zone (overrides x/y/w/h).
    #[serde(default)]
    pub snap_zone: Option<String>,
    /// Stacking hint.
    #[serde(default)]
    pub z: Option<i64>,
    /// Opacity `0.0..=1.0`.
    #[serde(default)]
    pub opacity: Option<f64>,
}

impl FloatingBrowserSpec {
    /// Project to a [`ResolvedBrowser`] — parse the URL, choose the placement,
    /// clamp opacity. Typed error on a bad URL (never a silent default).
    ///
    /// # Errors
    /// [`SpecError::BadUrl`] if `url` does not parse.
    pub fn compile(&self) -> Result<ResolvedBrowser, SpecError> {
        let url = Url::parse(&self.url).map_err(|_| SpecError::BadUrl(self.url.clone()))?;
        let placement = if let Some(zone) = &self.snap_zone {
            Placement::Zone(zone.clone())
        } else if let (Some(x), Some(y), Some(w), Some(h)) = (self.x, self.y, self.w, self.h) {
            #[allow(clippy::cast_possible_truncation)]
            Placement::At(Rect::new(x as f32, y as f32, w as f32, h as f32).round_to_int_px())
        } else {
            Placement::Default
        };
        let z = u16::try_from(self.z.unwrap_or(0).clamp(0, i64::from(u16::MAX))).unwrap_or(0);
        #[allow(clippy::cast_possible_truncation)]
        let opacity = self.opacity.unwrap_or(1.0).clamp(0.0, 1.0) as f32;
        Ok(ResolvedBrowser {
            url,
            placement,
            z,
            opacity,
        })
    }
}

/// `(defsnapzone :name "left-third" :edge "left" :fraction 0.33)` — a custom
/// snap zone. Built-in zones (`left-half`, `top-right`, …) need no declaration.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[tatara(keyword = "defsnapzone")]
pub struct SnapZoneSpec {
    /// Zone name — the MCP / lisp / keybind key.
    pub name: String,
    /// Which edge the zone hugs: `left`/`right`/`top`/`bottom`.
    pub edge: String,
    /// Fraction of that edge's dimension the zone covers (`0.0..=1.0`,
    /// default `0.5`).
    #[serde(default)]
    pub fraction: Option<f64>,
}

impl SnapZoneSpec {
    /// Project to a [`SnapZone`] the [`super::snap::SnapSystem`] accepts.
    ///
    /// # Errors
    /// [`SpecError::UnknownEdge`] if `edge` is not a valid edge name.
    pub fn compile(&self) -> Result<SnapZone, SpecError> {
        let edge = Edge::from_str_kind(&self.edge)
            .ok_or_else(|| SpecError::UnknownEdge(self.edge.clone()))?;
        #[allow(clippy::cast_possible_truncation)]
        let frac = self.fraction.unwrap_or(0.5).clamp(0.05, 1.0) as f32;
        Ok(SnapZone {
            name: Cow::Owned(self.name.clone()),
            geom: ZoneGeom::EdgeFraction { edge, frac },
            trigger: Some(Trigger::Edge(edge)),
        })
    }
}

/// Parse every `(deffloatingbrowser …)` form in `src`.
///
/// # Errors
/// Propagates any tatara-lisp read / expand / compile error.
pub fn browsers_from_str(src: &str) -> tatara_lisp::Result<Vec<FloatingBrowserSpec>> {
    tatara_lisp::compile_typed::<FloatingBrowserSpec>(src)
}

/// Parse every `(defsnapzone …)` form in `src`.
///
/// # Errors
/// Propagates any tatara-lisp read / expand / compile error.
pub fn snap_zones_from_str(src: &str) -> tatara_lisp::Result<Vec<SnapZoneSpec>> {
    tatara_lisp::compile_typed::<SnapZoneSpec>(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deffloatingbrowser_round_trips_from_lisp() {
        let src = r#"(deffloatingbrowser :url "https://example.com/" :snap-zone "right-half" :z 2 :opacity 0.96)"#;
        let specs = browsers_from_str(src).expect("compiles");
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.url, "https://example.com/");
        assert_eq!(s.snap_zone.as_deref(), Some("right-half"));
        assert_eq!(s.z, Some(2));
        assert_eq!(s.opacity, Some(0.96));

        let resolved = s.compile().expect("resolves");
        assert_eq!(resolved.placement, Placement::Zone("right-half".to_owned()));
        assert_eq!(resolved.z, 2);
        assert!((resolved.opacity - 0.96).abs() < 1e-6);
        assert_eq!(resolved.url.as_str(), "https://example.com/");
    }

    #[test]
    fn explicit_geometry_compiles_to_a_placement_rect() {
        let src = r#"(deffloatingbrowser :url "https://a.test/" :x 100 :y 80 :w 900 :h 600)"#;
        let s = &browsers_from_str(src).unwrap()[0];
        match s.compile().unwrap().placement {
            Placement::At(r) => assert_eq!(r, Rect::new(100.0, 80.0, 900.0, 600.0)),
            other => panic!("expected an explicit rect, got {other:?}"),
        }
    }

    #[test]
    fn absent_geometry_opens_at_the_host_default() {
        let src = r#"(deffloatingbrowser :url "https://a.test/")"#;
        let s = &browsers_from_str(src).unwrap()[0];
        assert_eq!(s.compile().unwrap().placement, Placement::Default);
    }

    #[test]
    fn a_malformed_url_is_a_typed_error_not_a_default() {
        let src = r#"(deffloatingbrowser :url "not a url")"#;
        let s = &browsers_from_str(src).unwrap()[0];
        assert_eq!(s.compile(), Err(SpecError::BadUrl("not a url".to_owned())));
    }

    #[test]
    fn defsnapzone_compiles_to_an_edge_fraction_zone() {
        let src = r#"(defsnapzone :name "left-third" :edge "left" :fraction 0.33)"#;
        let specs = snap_zones_from_str(src).expect("compiles");
        assert_eq!(specs.len(), 1);
        let zone = specs[0].compile().expect("resolves");
        assert_eq!(zone.name, Cow::Owned::<str>("left-third".to_owned()));
        assert_eq!(
            zone.geom,
            ZoneGeom::EdgeFraction {
                edge: Edge::Left,
                frac: 0.33
            }
        );
        assert_eq!(zone.trigger, Some(Trigger::Edge(Edge::Left)));

        // The resolved zone drives the geometry core: left third of a 900px vp.
        let vp = Rect::new(0.0, 0.0, 900.0, 600.0);
        assert_eq!(zone.geom.resolve(vp), Rect::new(0.0, 0.0, 297.0, 600.0));
    }

    #[test]
    fn defsnapzone_default_fraction_is_half() {
        let src = r#"(defsnapzone :name "l" :edge "left")"#;
        let zone = snap_zones_from_str(src).unwrap()[0].compile().unwrap();
        assert_eq!(
            zone.geom,
            ZoneGeom::EdgeFraction {
                edge: Edge::Left,
                frac: 0.5
            }
        );
    }

    #[test]
    fn an_unknown_edge_is_a_typed_error() {
        let src = r#"(defsnapzone :name "z" :edge "diagonal")"#;
        let s = &snap_zones_from_str(src).unwrap()[0];
        assert_eq!(
            s.compile(),
            Err(SpecError::UnknownEdge("diagonal".to_owned()))
        );
    }

    #[test]
    fn the_two_keywords_are_distinct() {
        // A deffloatingbrowser source yields no snap zones and vice-versa —
        // the two authoring keywords do not collide.
        let b = r#"(deffloatingbrowser :url "https://a.test/")"#;
        assert_eq!(snap_zones_from_str(b).unwrap().len(), 0);
        let z = r#"(defsnapzone :name "z" :edge "top")"#;
        assert_eq!(browsers_from_str(z).unwrap().len(), 0);
    }
}
