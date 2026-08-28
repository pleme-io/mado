//! `mado` library surface.
//!
//! This exposes ONLY the pure [`motion`] animation algebra so that
//! `benches/*.rs` (which compile as separate crates) can link and bench it,
//! and so the eventual lift of the motion evaluator into
//! `ishou_tokens::motion` (solve-once, 3rd-consumer rule) has a lib target
//! to extract from.
//!
//! The application itself lives in `src/main.rs` (the bin target), which
//! consumes this module via `use mado::motion;` — so `crate::motion::…`
//! keeps resolving at every render/config call site without a second copy
//! of the module compiling into the bin.
//!
//! **Do not widen this surface ad hoc.** Add a module here only when a
//! second pure, self-contained module earns a bench or an extraction.
//!
//! [`float`] is the second such module: the pure, engine-agnostic floating &
//! snapping browser-surface substrate (geometry, snapping, z-stack + window
//! FSM, and the injectable `BrowserBackend` engine seam). It lives here so its
//! property/mock tests link the lib crate and so the snap-geometry / chrome
//! primitives have a lib target to extract from (an egaku/ishou candidate per
//! QUADRO T1). Zero GPU / engine / MCP — those layers (M1+) live in the bin and
//! consume this via `use mado::float;`.
#![allow(dead_code)]

pub mod float;
// hanko (判子) — the frame gate's O(1) stamp. In the LIB rather than the binary
// deliberately: the class it seals is not mado's. seki's daemon leaked 31.8 GB
// to the sibling shape, and every fleet render loop and reconciler asks the same
// "did it change?" question on a hot path. Extraction to a fleet crate is the
// named destination, currently blocked on org repo creation (all 5 shards
// parked), so it lives here and is already importable as `mado::hanko`.
pub mod hanko;
pub mod motion;
