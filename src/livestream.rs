//! The reusable **live-stream** substrate: a reactive in-memory store that
//! broadcasts change notifications, plus an interval refresh-loop that drives a
//! source into a sink on a cadence. Generic over the item type and the source,
//! so any surface — the Ctrl-S suggestion board, the mado MCP readers, tear,
//! future panels — consumes the SAME primitive instead of re-implementing
//! "continuously refresh + async broadcast" per consumer.
//!
//! Three stages, each a reusable piece:
//!
//! 1. **source → memory** — [`spawn_interval_refresh`] (the *RefreshLoop*): one
//!    tokio task per source, ticking on its own interval, pushing fresh results
//!    into a sink every tick. The first tick fires immediately, then on cadence
//!    — so a source's data is re-fetched continuously and the store is updated,
//!    never fetched once (staleness dies by construction).
//! 2. **memory** — [`Reactive`] / [`LiveStore`]: a monotonic change-generation
//!    plus a tokio [`watch`] broadcast; every meaningful mutation bumps the
//!    generation AND notifies every subscriber.
//! 3. **memory → GUI** — [`Reactive::subscribe`] hands out a [`watch::Receiver`]
//!    any consumer subscribes to: async consumers `await` [`watch::Receiver::changed`];
//!    a synchronous GUI polls [`watch::Receiver::has_changed`] cheaply each frame
//!    and re-renders on the fact of a change (values coalesce to the newest).
//!
//! `Reactive` is the shared broadcast core the domain-specific
//! `crate::suggest::SuggestionStore` composes; `LiveStore<T>` is the fully
//! generic push/subscribe store future surfaces reuse directly. The refresh
//! loop is the real engine watcher's driver (see
//! `crate::suggest::source::SuggestionEngine`).

// The generic surface is a reusable substrate the binary consumes partially
// (the suggestion store uses `Reactive` + `spawn_interval_refresh`; `LiveStore`
// is exercised by other surfaces + the test suite). Allow dead_code so the
// generic API can carry its whole shape without per-item allows.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The shared reactive core: a monotonic change-generation plus a [`watch`]
/// broadcast that fires the new generation on every meaningful change. This is
/// the "memory broadcasts changes" primitive that stage-3 subscribers wake on.
///
/// A sync consumer reads [`generation`](Reactive::generation) (an atomic load)
/// and re-renders when it advances; an async consumer holds a
/// [`subscribe`](Reactive::subscribe) receiver and `await`s changes. Both are
/// runtime-agnostic for read/notify — only `changed().await` needs a runtime,
/// so a non-async GUI thread can [`has_changed`](watch::Receiver::has_changed)
/// freely.
pub struct Reactive {
    generation: AtomicU64,
    tx: watch::Sender<u64>,
}

impl Default for Reactive {
    fn default() -> Self {
        Self::new()
    }
}

impl Reactive {
    /// A fresh reactive core at generation 0. The `watch` channel is seeded with
    /// 0, so a subscriber taken now sees no change until the first `bump`.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(0);
        Self {
            generation: AtomicU64::new(0),
            tx,
        }
    }

    /// Current change-generation (Acquire). Bumped only on a meaningful change.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Record a meaningful change: bump the generation (Release) and broadcast
    /// the new value to every subscriber. `watch` retains only the latest value,
    /// so a slow subscriber coalesces to the newest generation — exactly the
    /// "re-render to current" semantics a live view wants.
    pub fn bump(&self) {
        let g = self.generation.fetch_add(1, Ordering::Release) + 1;
        // The receiver kept inside `tx` (via the never-dropped `_rx` seed is not
        // held; but `send` to zero receivers is fine) — send never fails here
        // because the sender owns the channel for its whole lifetime.
        let _ = self.tx.send(g);
    }

    /// Subscribe to change notifications. Async consumers
    /// `.changed().await`; a synchronous GUI polls `.has_changed()` per frame.
    /// The receiver never misses the *fact* of a change (values may coalesce).
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }
}

/// A generic reactive in-memory store: hold one `T`, mutate it under a lock,
/// broadcast on every meaningful change. The generic peer of the
/// domain-specific `SuggestionStore` — future surfaces (tear panes, MCP readers,
/// new panels) reuse THIS rather than re-implementing the reactive plumbing.
///
/// * `push` replaces the whole value and always broadcasts.
/// * `update` mutates in place and broadcasts iff the mutation reports a
///   meaningful change — the staleness-vs-noise seam (a heartbeat write can
///   skip the wake).
/// * `snapshot` clones the current value (the reader path).
/// * `subscribe` / `generation` are the stage-3 subscription surface.
pub struct LiveStore<T> {
    inner: Mutex<T>,
    reactive: Reactive,
}

impl<T> LiveStore<T> {
    /// A store seeded with `initial`, at generation 0.
    #[must_use]
    pub fn new(initial: T) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(initial),
            reactive: Reactive::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current change-generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.reactive.generation()
    }

    /// A change-notification subscription (see [`Reactive::subscribe`]).
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.reactive.subscribe()
    }

    /// Replace the whole value and broadcast — the continuous-refresh write
    /// path (a source tick pushes its fresh set; the prior, now-stale value is
    /// overwritten and unrepresentable afterward).
    pub fn push(&self, value: T) {
        *self.lock() = value;
        self.reactive.bump();
    }

    /// Mutate in place; broadcast IFF `f` reports a meaningful change (returns
    /// `true`). A `false` return is a heartbeat — the value is updated but no
    /// subscriber is woken.
    pub fn update(&self, f: impl FnOnce(&mut T) -> bool) {
        let changed = {
            let mut g = self.lock();
            f(&mut g)
        };
        if changed {
            self.reactive.bump();
        }
    }
}

impl<T: Clone> LiveStore<T> {
    /// Clone the current value.
    #[must_use]
    pub fn snapshot(&self) -> T {
        self.lock().clone()
    }
}

/// A stop flag shared by every spawned refresh task — flip it to end the loops.
pub type StopFlag = Arc<AtomicBool>;

/// Spawn ONE interval-driven refresh task (the reusable *RefreshLoop*, stage 1).
///
/// Every `interval` the task runs `refresh` — which may block (subprocess /
/// HTTP), so it runs on the tokio blocking pool — and hands its result to
/// `sink` on the async side. The first tick fires immediately (tokio interval
/// semantics), so data appears shortly after start, then on cadence: the
/// continuous-refresh contract. `stop` ends the loop cooperatively.
///
/// A panic inside `refresh` is reported to `on_panic` and the last-known state
/// is PRESERVED (the `sink` is not called with a bogus value), never a silent
/// wipe — the source's prior rows survive a transient failure. Must be called
/// from within a tokio runtime.
pub fn spawn_interval_refresh<T>(
    interval: Duration,
    stop: StopFlag,
    refresh: Arc<dyn Fn() -> T + Send + Sync>,
    sink: impl Fn(T) + Send + 'static,
    on_panic: impl Fn() + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let r = Arc::clone(&refresh);
            match tokio::task::spawn_blocking(move || r()).await {
                Ok(value) => sink(value),
                Err(_join_err) => on_panic(),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;

    // ── Stage 2: the reactive store broadcasts ──────────────────────────

    #[test]
    fn push_updates_snapshot_and_bumps_generation() {
        let store = LiveStore::new(0u32);
        let g0 = store.generation();
        store.push(7);
        assert_eq!(store.snapshot(), 7);
        assert!(store.generation() > g0, "push bumps the generation");
    }

    #[test]
    fn staleness_is_impossible_a_push_overwrites_the_prior_value() {
        // A source tick pushes its fresh set; the prior value is gone. Reading
        // after any push always sees the latest — a stale value has no path.
        let store = LiveStore::new("v1".to_string());
        store.push("v2".into());
        store.push("v3".into());
        assert_eq!(store.snapshot(), "v3", "only the newest value is readable");
    }

    #[test]
    fn update_broadcasts_only_on_a_meaningful_change() {
        let store = LiveStore::new(0u32);
        let g0 = store.generation();
        store.update(|v| {
            *v = 0;
            false // heartbeat: no meaningful change
        });
        assert_eq!(store.generation(), g0, "a heartbeat does not bump");
        store.update(|v| {
            *v = 1;
            true
        });
        assert!(store.generation() > g0, "a real change bumps");
    }

    #[test]
    fn multi_source_merge_two_sources_both_land_in_the_store() {
        // Two independent "sources" write disjoint slices of one shared map;
        // both land — the store is the single merge point (stage 2).
        let store: Arc<LiveStore<BTreeMap<&'static str, u32>>> = LiveStore::new(BTreeMap::new());
        store.update(|m| {
            m.insert("jira", 1);
            true
        });
        store.update(|m| {
            m.insert("github", 2);
            true
        });
        let snap = store.snapshot();
        assert_eq!(snap.get("jira"), Some(&1), "source A landed");
        assert_eq!(snap.get("github"), Some(&2), "source B landed");
        assert_eq!(snap.len(), 2, "both sources merged into one store");
    }

    #[test]
    fn subscriber_sees_the_fact_of_each_change_synchronously() {
        // The sync-GUI subscription path: has_changed() flips true on each
        // bump; mark_unchanged() re-arms it. This is exactly what the Ctrl-S
        // board's per-frame poll does.
        let store = LiveStore::new(0u32);
        let mut rx = store.subscribe();
        assert!(!rx.has_changed().unwrap(), "no change before first push");
        store.push(1);
        assert!(rx.has_changed().unwrap(), "push woke the subscriber");
        rx.mark_unchanged();
        assert!(!rx.has_changed().unwrap(), "re-armed after consuming");
        store.push(2);
        assert!(rx.has_changed().unwrap(), "the next push wakes it again");
    }

    // ── Stage 3: an async subscriber wakes on every change ──────────────

    #[tokio::test]
    async fn async_subscriber_wakes_on_change_and_reads_the_new_value() {
        let store = LiveStore::new(0u32);
        let mut rx = store.subscribe();
        let writer = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            for v in 1..=3u32 {
                writer.push(v);
                tokio::task::yield_now().await;
            }
        });
        // Await changes and read the latest value each time; watch coalesces,
        // so we assert we converge to the final pushed value.
        rx.changed().await.unwrap();
        handle.await.unwrap();
        // Drain any further pending change and read the newest snapshot.
        let _ = rx.has_changed();
        assert_eq!(store.snapshot(), 3, "subscriber reads the newest value");
    }

    // ── Stage 1: the interval refresh loop drives a source into the store ─

    #[tokio::test(start_paused = true)]
    async fn refresh_loop_repolls_on_every_tick_updating_the_store() {
        // A mock source that emits CHANGING data over ticks: an incrementing
        // counter. The loop ticks; each tick pushes the fresh value into the
        // store; a subscriber observes the sequence advance — proving the
        // continuous refresh (not a one-shot fetch).
        let store = LiveStore::new(0u32);
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let stop: StopFlag = Arc::new(AtomicBool::new(false));

        let c = Arc::clone(&counter);
        let refresh: Arc<dyn Fn() -> u32 + Send + Sync> =
            Arc::new(move || c.fetch_add(1, Ordering::SeqCst) + 1);
        let sink_store = Arc::clone(&store);
        let handle = spawn_interval_refresh(
            Duration::from_secs(1),
            Arc::clone(&stop),
            refresh,
            move |v| sink_store.push(v),
            || {},
        );

        // The interval fires at t=0 then every 1s; sample halfway between ticks
        // (t=0.5, 1.5, 2.5s) so each sample sees exactly one more tick — proving
        // the store is re-fetched and overwritten with fresh data on EVERY tick
        // (a continuous refresh, not a one-shot fetch). The paused clock
        // auto-advances when idle, so the sleeps are deterministic.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(store.snapshot(), 1, "tick 1 pushed fresh data");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(store.snapshot(), 2, "tick 2 re-polled and overwrote");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(store.snapshot(), 3, "tick 3 re-polled and overwrote");

        stop.store(true, Ordering::SeqCst);
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_loop_panic_preserves_last_known_state() {
        // The refresher panics; on_panic fires and the store is NOT wiped —
        // the last-known value survives (best-effort source contract).
        let store = LiveStore::new(42u32);
        let panics = Arc::new(AtomicUsize::new(0));
        let stop: StopFlag = Arc::new(AtomicBool::new(false));

        let refresh: Arc<dyn Fn() -> u32 + Send + Sync> = Arc::new(|| panic!("source blew up"));
        let p = Arc::clone(&panics);
        let handle = spawn_interval_refresh(
            Duration::from_secs(1),
            Arc::clone(&stop),
            refresh,
            move |_v| unreachable!("sink must not run when refresh panics"),
            move || {
                p.fetch_add(1, Ordering::SeqCst);
            },
        );

        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(panics.load(Ordering::SeqCst) >= 1, "panic was reported");
        assert_eq!(store.snapshot(), 42, "store preserved its last-known value");
        stop.store(true, Ordering::SeqCst);
        handle.abort();
        let _ = handle.await;
    }
}
