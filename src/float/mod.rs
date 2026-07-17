//! `float` — the pure, engine-agnostic substrate for mado's **floating &
//! snapping browser surfaces**.
//!
//! This module is the L0/L1 core of the floating-browser stack (design:
//! [`theory/BROWSER.md`], reuse-first per Op#1): everything here is pure
//! geometry, state, and one injectable engine seam — **zero GPU, zero live
//! engine, zero MCP wiring**. That is deliberate: because the whole
//! float/snap/focus/control machinery is engine-agnostic behind
//! [`engine::BrowserBackend`], it ships and is *fully proven* before a single
//! pixel or a real web engine lands. The engine is a swappable back-trait, never
//! the gating dependency.
//!
//! Layers (thin → wide):
//! - [`geom`] — the tier-honest rect vocabulary over [`egaku::Rect`]
//!   ([`geom::RectExt`], [`geom::Edge`]/[`geom::Corner`],
//!   [`geom::RectProvenance`]). No second `Rect` type.
//! - [`snap`] — pure snapping geometry ([`snap::SnapSystem`], the built-in zone
//!   table authored once via the `snap_zones!` Layer-B macro).
//! - [`state`] — the [`state::FloatFocus`] z-stack (single-focus invariant by
//!   construction) + the [`state::transition`] window-interaction FSM
//!   (no-wildcard, total).
//! - [`engine`] — the [`engine::BrowserBackend`] seam + pure
//!   [`engine::NavControl`], proven against a `#[cfg(test)]` mock.
//!
//! Home: this lives in `src/lib.rs` (the lib target), like [`crate::motion`] —
//! a pure, self-contained, testable module that earns a lib home for tests +
//! future extraction (the snap-geometry / chrome widget is an egaku/ishou
//! extraction candidate per QUADRO T1). The bin consumes it via `use
//! mado::float;` when the GPU/input/MCP layers (M1+) wire it in.
//!
//! Downstream (NOT here — named so the boundary is explicit):
//! - **L2 GPU composite** extends mado's Kitty-graphics image path
//!   (`render.rs`) to draw a browser RGBA texture as a floating quad.
//! - **L3 input/focus** adds `Action::Browser*` + a `WindowDrag` FSM.
//! - **L4 control** adds `(deffloatingbrowser …)`/`(defsnapzone …)`
//!   ([`spec`], gated on the tatara-lisp dep), `(mado-browser-*)` vigy
//!   intrinsics, `browser_*` MCP tools, and the tear-session binding.

pub mod engine;
pub mod geom;
pub mod snap;
pub mod spec;
pub mod state;

pub use engine::{BackendError, BrowserBackend, LoadState, NavControl, RenderedFrame};
pub use geom::{Corner, Edge, RectExt, RectProvenance};
pub use snap::{
    builtin_zone_geom, builtin_zones, classify_builtin, DragPhase, SnapAction, SnapConfig,
    SnapSystem, SnapZone, Trigger, ZoneGeom, BUILTIN_ZONE_NAMES,
};
pub use spec::{
    browsers_from_str, snap_zones_from_str, FloatingBrowserSpec, Placement, ResolvedBrowser,
    SnapZoneSpec, SpecError,
};
pub use state::{
    transition, BrowserId, FloatEffect, FloatEvent, FloatFocus, FloatMode, FloatState, FloatStep,
    FloatWindowFsm, FloatingSurface,
};
