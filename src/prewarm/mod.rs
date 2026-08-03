//! Session pre-warming — the on-call cockpit (mado side).
//!
//! The typed vocabulary — [`PrewarmSpec`], [`PrewarmStep`], the shared
//! [`reject_injection`] guard — is **colocated in the izumi substrate** (beside
//! `SpawnSpec`), so every board consumer shares it; mado re-exports it here.
//! `SpawnSpec::prewarm()` returns the full ordered strategy (the legacy
//! `initial_command` folded in as the first `RunCommand`, then the richer
//! [`with_prewarm`](izumi::SpawnSpec::with_prewarm) steps).
//!
//! mado owns the terminal **executor** (the verb): the [`interp`] module walks a
//! [`PrewarmSpec`] against the injectable [`PrewarmEnv`] trait (mocked in tests;
//! the real impl is `SessionPrewarmEnv` in `session_picker.rs`, which runs the
//! steps in the freshly-spawned session's shell — the scope boundary, see
//! `docs/PREWARM.md`). The strategy builder for alert sources is
//! `crate::safra::prewarm`.

pub mod interp;

// The typed prewarm border lives in izumi (colocated + redistributed to every
// board consumer). Re-export so mado call sites read `crate::prewarm::…`.
// `reject_injection` / `PrewarmError` are part of the surface even where mado
// doesn't call them directly today (the safra builder + the interpreter use the
// rest); allow the re-exports.
#[allow(unused_imports)]
pub use interp::{PrewarmEnv, PrewarmError, apply};
#[allow(unused_imports)]
pub use izumi::{PrewarmSpec, PrewarmStep, reject_injection};
