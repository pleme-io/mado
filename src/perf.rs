//! Launch-perf timeline helpers.
//!
//! `set_launch_start` records the process's effective start
//! timestamp once (in `main`); `log_phase` then emits an
//! INFO-level tracing line stamping milliseconds-since-start
//! at every named milestone. The operator reads these out of
//! mado's stderr to see where time goes from binary-exec to
//! first frame.
//!
//! Cost: one `Instant::elapsed()` + one tracing call per
//! milestone. Tracing skips the formatting work entirely when
//! the level is filtered out, so this is effectively zero
//! when `RUST_LOG=warn` is set.

use std::sync::OnceLock;
use std::time::Instant;

static LAUNCH_START: OnceLock<Instant> = OnceLock::new();

/// Stamp the launch start instant. Call once in `main`
/// immediately after tracing is initialized.
pub fn set_launch_start(instant: Instant) {
    let _ = LAUNCH_START.set(instant);
}

/// Log a named launch-perf phase with elapsed-ms since start.
/// No-op (returns immediately) if `set_launch_start` was never
/// called.
pub fn log_phase(name: &'static str) {
    if let Some(start) = LAUNCH_START.get() {
        let ms = start.elapsed().as_millis();
        tracing::info!(target: "mado::perf", phase = name, ms, "launch phase");
    }
}
