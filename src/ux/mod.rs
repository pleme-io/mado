//! M1 — the Unified Input/UX Engine (docs/REMEDIATION-PLAN.md §M1).
//!
//! ONE render-mode-agnostic input/UX engine drives both the
//! embedded-tear default path (`gui_tear_attach.rs`) and the
//! local-PTY fallback (`main.rs`) over identical inputs: a shared
//! `Arc<RwLock<Terminal>>`, a [`PtySink`] for PTY writes, a
//! [`ResizeSink`] for grid pushes, and the shared selection /
//! search / dir-picker Arcs the renderer highlights from.
//!
//! Before M1 the two event loops carried TWO copies of the UX logic
//! (selection, copy/paste, mouse forwarding, search overlay routing,
//! kitty encoding, URL click, focus, IME, font zoom, grid
//! reconciler) — named interim debt. After M1, every capability is
//! implemented exactly once in [`InputEngine`]; the loops are thin
//! adapters that translate `AppEvent` fields into engine calls and
//! map [`EventOutcome`] back to `madori::EventResponse`. The
//! structural invariant is pinned by `tests/ux_unification.rs`.

pub mod behavior;
pub mod engine;
pub mod font_zoom;
pub mod mouse_report;
pub mod outcome;
pub mod side_effects;
pub mod sinks;

pub use behavior::UxBehavior;
pub use engine::{ActionOutcome, InputEngine, InputEngineParams, SharedUxState};
pub use font_zoom::FontZoomTarget;
pub use outcome::EventOutcome;
#[allow(unused_imports)] // typed drain payload — Terminal::drain_side_effects() wires in M4
pub use side_effects::TerminalSideEffects;
pub use sinks::{PtySink, ResizeSink, ResponseWriter};
