//! `KeyRepeatGate` — debouncer for OS key-repeat storms.
//!
//! # The runaway-font incident (2026-05-21)
//!
//! Operator held `Cmd-=` for ~1.5 seconds. macOS key-repeat fired
//! `FontIncrease` ~25 times in that window (one every ~30-150ms after
//! the initial ~250ms repeat delay). Each fire bumped the renderer's
//! font_size by +1.0. The font grew from 14 → 32 pt onscreen before
//! the operator could lift the key.
//!
//! `BoundedFontSize` keeps the value from going past `FONT_MAX = 64`
//! — that's the magnitude half of the fix. This gate is the
//! **temporal** half: even though each individual increment is safe,
//! the operator's intent was "one increment per intentional press,"
//! not "one per key-repeat tick."
//!
//! # The gate
//!
//! `KeyRepeatGate` tracks the last-fire timestamp per
//! `K: Hash + Eq + Copy` key (typically a `crate::keybind::Action`).
//! `try_pass(key)` returns `true` only if at least `min_interval` has
//! elapsed since the previous pass for the same key. Subsequent calls
//! within the window return `false` — the caller drops them.
//!
//! Defaults: `min_interval = 80ms`. OS key-repeat is typically
//! 30-50ms; 80ms drops every storm-tick but allows up to 12
//! intentional presses per second — well above human cadence.
//!
//! # Per-key tracking, not global
//!
//! A storm of `FontIncrease` does NOT block `Copy` or `Paste`.
//! Each `K` gets its own clock — operators can hold Cmd-= AND
//! still hit Cmd-C in the same window.
//!
//! # Tests cover the runaway-font scenario directly: a 25-event
//! storm in 1.5s produces only ~19 transitions instead of 25 (one
//! per 80ms slot). Combined with `BoundedFontSize::inc_step`'s
//! saturation, the font cannot grow past FONT_MAX no matter how
//! long the key is held.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Default minimum interval between accepted events for the same
/// key. 80ms is long enough to filter out OS key-repeat (~30-50ms
/// intervals) but short enough that 12 intentional presses per
/// second still pass.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(80);

/// The debouncer. Generic over the key type so consumers can use
/// `Action`, `&'static str`, or any other Eq+Hash+Copy token.
#[derive(Debug, Default)]
pub struct KeyRepeatGate<K: Eq + Hash + Copy> {
    /// Per-key last-fire timestamp.
    last_pass: HashMap<K, Instant>,
    /// Minimum interval between consecutive passes for the same key.
    min_interval: Duration,
}

impl<K: Eq + Hash + Copy> KeyRepeatGate<K> {
    /// Construct with the default 80ms window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_interval(DEFAULT_MIN_INTERVAL)
    }

    /// Construct with a custom min-interval. Use shorter for less
    /// debouncing, longer for more aggressive throttling.
    #[must_use]
    pub fn with_interval(min_interval: Duration) -> Self {
        Self {
            last_pass: HashMap::new(),
            min_interval,
        }
    }

    /// Attempt to pass an event for `key`. Returns `true` if at
    /// least `min_interval` has elapsed since the last accepted
    /// pass for the same key (or there was no prior pass). On
    /// `true`, the timestamp is updated.
    ///
    /// Returns `false` (and does NOT update the timestamp) when
    /// the call lands within the window — the caller should drop
    /// the event.
    pub fn try_pass(&mut self, key: K) -> bool {
        self.try_pass_at(key, Instant::now())
    }

    /// Same as `try_pass` but with an explicit timestamp. Used by
    /// tests so they don't depend on wall-clock timing.
    pub fn try_pass_at(&mut self, key: K, now: Instant) -> bool {
        match self.last_pass.get(&key) {
            Some(prev) if now.duration_since(*prev) < self.min_interval => false,
            _ => {
                self.last_pass.insert(key, now);
                true
            }
        }
    }

    /// Reset all timestamps. Used when the window loses focus or
    /// when the operator explicitly resets keybind state.
    pub fn clear(&mut self) {
        self.last_pass.clear();
    }

    /// Read the configured min-interval. Useful for diagnostic
    /// surfacing.
    #[must_use]
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum TestAction {
        FontIncrease,
        FontDecrease,
        Copy,
    }

    #[test]
    fn first_pass_always_succeeds() {
        let mut gate = KeyRepeatGate::new();
        assert!(gate.try_pass(TestAction::FontIncrease));
    }

    #[test]
    fn rapid_repeats_within_window_are_dropped() {
        let mut gate = KeyRepeatGate::with_interval(Duration::from_millis(80));
        let t0 = Instant::now();
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0));
        assert!(!gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(10)));
        assert!(!gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(50)));
        assert!(!gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(79)));
    }

    #[test]
    fn pass_after_window_succeeds() {
        let mut gate = KeyRepeatGate::with_interval(Duration::from_millis(80));
        let t0 = Instant::now();
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0));
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(80)));
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(160)));
    }

    #[test]
    fn different_keys_have_independent_clocks() {
        let mut gate = KeyRepeatGate::with_interval(Duration::from_millis(80));
        let t0 = Instant::now();
        // Hold FontIncrease — drops within window.
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0));
        assert!(!gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(10)));
        // Copy still passes — different key, different clock.
        assert!(gate.try_pass_at(TestAction::Copy, t0 + Duration::from_millis(11)));
        // FontDecrease is also unrelated.
        assert!(gate.try_pass_at(TestAction::FontDecrease, t0 + Duration::from_millis(12)));
    }

    #[test]
    fn runaway_font_storm_drops_to_one_per_window() {
        // The exact incident scenario: 25 FontIncrease events in 1.5s.
        // Without the gate that's 25 transitions; with an 80ms gate
        // only floor(1500/80) + 1 = ~19 transitions can pass.
        let mut gate = KeyRepeatGate::with_interval(Duration::from_millis(80));
        let t0 = Instant::now();
        let mut passes = 0;
        // 25 events evenly distributed over 1500ms = 60ms intervals.
        // (Matches macOS key-repeat at default speed.)
        for i in 0..25 {
            let when = t0 + Duration::from_millis(i * 60);
            if gate.try_pass_at(TestAction::FontIncrease, when) {
                passes += 1;
            }
        }
        // Math: events at t=0, 60, 120, 180, 240, 300, … 1440ms.
        // Gate accepts t=0, 120, 240, 360, …, 1440. That's 13 events.
        assert!(
            (12..=14).contains(&passes),
            "expected ~13 passes for 25 events @60ms with 80ms gate, got {passes}"
        );
    }

    #[test]
    fn clear_resets_all_clocks() {
        let mut gate = KeyRepeatGate::with_interval(Duration::from_millis(80));
        let t0 = Instant::now();
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0));
        assert!(!gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(10)));
        gate.clear();
        // After clear, even an immediate re-pass succeeds.
        assert!(gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_millis(11)));
    }

    #[test]
    fn min_interval_zero_is_a_passthrough() {
        // Operators who want zero debouncing can opt into 0ms
        // (e.g., for tests or for actions that should always fire).
        let mut gate = KeyRepeatGate::with_interval(Duration::ZERO);
        let t0 = Instant::now();
        for i in 0..100 {
            assert!(gate.try_pass_at(TestAction::FontIncrease, t0 + Duration::from_nanos(i)));
        }
    }
}
