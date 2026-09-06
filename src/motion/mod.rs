//! `motion` — RETIRED IN PLACE. The algebra now lives in
//! [`ishou_tokens::motion`]; this module is the re-export that keeps every
//! `crate::motion::…` path in mado resolving.
//!
//! ## Why it moved
//!
//! This module's own doc named the destination: *"lift this evaluator up
//! into `ishou_tokens::motion` so egaku / quadro / tela / garasu share one
//! motion evaluator … extraction lands at the 3rd consumer."* It landed at
//! the second (tobira), because mado is an APPLICATION — a second consumer
//! cannot depend on it, so the choice was never "extract now or later" but
//! **extract or copy**.
//!
//! ## Why the module still exists
//!
//! ★★ MODULARIZE, DON'T DELETE. Retirement here is a BINDING change, not a
//! deletion: `crate::motion::Tween` still resolves, `render.rs`'s bell flash
//! is untouched, and the diff that moved 1,042 lines out of mado changes no
//! behaviour at all. The curve vocabulary was always ishou's
//! (`Cubic`/`Easings`/`Durations`); only the evaluator was ever mado-local,
//! and now neither half is.
//!
//! The property tests travelled with the code — a tween never overshoots, a
//! decay never rises, an oscillator's period is stable, every arm is a
//! strict no-op at `dt <= 0` — and run in ishou's suite.

pub use ishou_tokens::motion::{
    Advance, Curve, Decay, EasingKind, NonNegSecs, Oscillator, Seconds, Tween, Unit, UnitBounds,
    blink_on, frame_decay, secs,
};
