//! The reusable **live-stream** substrate — now the `izumi::refresh` module
//! (extracted with the suggestion plane): a reactive in-memory store that
//! broadcasts change notifications, plus an interval refresh-loop that drives
//! a source into a sink on a cadence (with the optional freshness nudge).
//!
//! This file is a thin shim preserving the historical `crate::livestream::*`
//! import paths; the three stages (source → memory → GUI) and their doc
//! contracts live on the izumi side. `Reactive` is the shared broadcast core
//! `izumi::Store` composes; `LiveStore<T>` is the fully generic
//! push/subscribe store other surfaces reuse directly; the refresh loop is
//! the engine watcher's driver (see [`izumi::Engine`]).

// The generic surface is a reusable substrate the binary consumes partially;
// keep the whole re-export shape without per-item allows.
#[allow(unused_imports)]
pub use izumi::refresh::{
    LiveStore, Reactive, StopFlag, spawn_interval_refresh, spawn_interval_refresh_nudged,
};
