//! **Connective fibers** — mado's in-process typed event bus.
//!
//! The in-process expression of the fleet's NATS/vector plane: a
//! subject-addressed pub/sub fabric that reactive invariant-holders (the
//! [`crate::janitors`] plane), the board, and future session machinery
//! communicate over. Subjects are a CLOSED, catalog-reflected enum
//! ([`Subject`]) — **never strings** — so publishing to a subject that does
//! not exist is unrepresentable (there is no constructor for it), and adding
//! a subject is a compile error until every exhaustive match is extended
//! (the izumi `catalog!` idiom, hand-carried here because the fiber catalog
//! carries no urgency/auth/interval payload).
//!
//! One typed payload family per subject keeps the wire honest:
//!
//! | Subject | Payload | Publishers (M0) |
//! |---|---|---|
//! | [`Subject::Sessions`] | [`SessionEvent`] | janitor runner (ghost reaps) |
//! | [`Subject::Janitors`] | [`crate::janitors::JanitorFinding`] | janitor runner (every finding) |
//! | [`Subject::Board`] | [`BoardEvent`] | janitor runner (board projections) |
//!
//! ## Prior fibers this layer absorbs incrementally (NOT rewired in M0)
//!
//! mado already carries several point-to-point strands that are, in
//! hindsight, single-subject fibers authored before the bus existed:
//!
//! - `suggest::board_nudge` — a `tokio::sync::Notify` the GUI fires and the
//!   watchers park on (a payload-less "board" subject);
//! - izumi's `Reactive` (`Store::subscribe`) — a `watch` channel broadcasting
//!   the store generation (a "board changed" subject);
//! - `suggest::EngineControl` — an mpsc carrying config swaps to the engine
//!   loop (a "control" subject).
//!
//! Each keeps working untouched; the **named destination** is to absorb them
//! one at a time onto typed subjects here once a second subscriber
//! materializes per strand (the three-site rule — rewiring them today would
//! churn proven seams for zero new capability). Fleet extraction of this bus
//! (an `izumi`-style substrate crate) is likewise a named destination gated
//! on the second in-process consumer outside mado.
//!
//! ## Delivery semantics
//!
//! `tokio::sync::broadcast` under the hood: every subscriber sees every
//! event published after it subscribed; a slow subscriber that falls more
//! than the channel capacity behind observes a typed `Lagged(n)` error and
//! then continues from the oldest retained event — a lossy-but-honest
//! backpressure model (the observability plane must never block the
//! publisher; missing N findings is recoverable because janitors re-observe
//! every interval). Publishing with zero subscribers is a no-op, never an
//! error — the bus is always safe to publish into.

// M0 ships the publisher side live (the janitor runner) while the
// subscriber surface (subscribe / subscriber_count / payload reads) is
// exercised by the test suite only — the first in-process subscriber (a
// board adapter, a notify bridge) is named follow-up work. Allow dead_code
// module-wide while the plane fills out (the suggest-module precedent)
// rather than scatter per-item allows that churn as each consumer lands.
#![allow(dead_code)]

use std::sync::OnceLock;

use tokio::sync::broadcast;

/// Default per-subject channel capacity. Janitor cadences are seconds-scale,
/// so 64 retained events per subject is generous; a subscriber further
/// behind than that is better served by the typed `Lagged` signal + a fresh
/// observation than by unbounded buffering.
const DEFAULT_CAPACITY: usize = 64;

// ─────────────────────────────────────────────────────────────────
// Subject catalog (CATALOG REFLECTION — closed, never strings)
// ─────────────────────────────────────────────────────────────────

/// Every subject the fiber bus carries — the COMPLETE catalog. Adding a
/// variant is a compile error until [`Subject::slug`], [`Subject::index`],
/// and [`FiberEvent::subject`] are extended, and a test failure until
/// [`Subject::ALL`] lists it (the izumi catalog idiom).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subject {
    /// Session-lifecycle facts (a ghost session reaped, …). Payload:
    /// [`SessionEvent`].
    Sessions,
    /// Janitor findings — every invariant observation, shadow or effect.
    /// Payload: [`crate::janitors::JanitorFinding`].
    Janitors,
    /// Board-projection facts (a janitor row injected onto the Ctrl-S
    /// board, …). Payload: [`BoardEvent`].
    Board,
}

impl Subject {
    /// Every subject, in declaration order — the reflection surface tests
    /// and tooling iterate.
    pub const ALL: &'static [Subject] = &[Subject::Sessions, Subject::Janitors, Subject::Board];

    /// Kebab slug — for logs / MCP output. NOT an addressing key: the bus
    /// is addressed by the typed variant only.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Subject::Sessions => "sessions",
            Subject::Janitors => "janitors",
            Subject::Board => "board",
        }
    }

    /// Resolve a slug back to its subject (log/tooling round-trip).
    #[must_use]
    pub fn from_slug(s: &str) -> Option<Subject> {
        Subject::ALL.iter().copied().find(|k| k.slug() == s)
    }

    /// Dense channel index — exhaustive so a new variant cannot silently
    /// alias an existing channel.
    fn index(self) -> usize {
        match self {
            Subject::Sessions => 0,
            Subject::Janitors => 1,
            Subject::Board => 2,
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Typed payload families
// ─────────────────────────────────────────────────────────────────

/// Session-lifecycle facts published on [`Subject::Sessions`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// A fully-exited, unwatched, agent-owned session was closed by the
    /// ghost-session janitor (Effect mode) through the guarded close path.
    GhostSessionReaped {
        /// The tear session id (display form).
        session_id: String,
    },
}

/// Board-projection facts published on [`Subject::Board`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardEvent {
    /// A janitor finding was projected onto the Ctrl-S board as an
    /// agent-lane row (via the `suggest::inject` path).
    JanitorRowInjected {
        /// The stable dedup key of the injected row.
        key: String,
        /// The row title as offered.
        title: String,
    },
}

/// One event on the bus — a closed sum over the subject families. The
/// subject is DERIVED from the payload ([`FiberEvent::subject`], exhaustive
/// match), so an event published to the wrong subject is unrepresentable.
#[derive(Clone, Debug)]
pub enum FiberEvent {
    /// [`Subject::Sessions`] payload.
    Session(SessionEvent),
    /// [`Subject::Janitors`] payload.
    Janitor(crate::janitors::JanitorFinding),
    /// [`Subject::Board`] payload.
    Board(BoardEvent),
}

impl FiberEvent {
    /// The subject this event travels on — total, exhaustive: every payload
    /// family maps to exactly one subject.
    #[must_use]
    pub fn subject(&self) -> Subject {
        match self {
            FiberEvent::Session(_) => Subject::Sessions,
            FiberEvent::Janitor(_) => Subject::Janitors,
            FiberEvent::Board(_) => Subject::Board,
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// The bus
// ─────────────────────────────────────────────────────────────────

/// Subject-addressed pub/sub bus: one `broadcast` channel per catalog
/// subject. Cheap to construct (three senders), `Send + Sync`, no runtime
/// required to publish (`broadcast::Sender::send` is sync); receivers are
/// polled with `try_recv` from sync code or awaited from async code.
pub struct FiberBus {
    channels: Vec<broadcast::Sender<FiberEvent>>,
}

impl FiberBus {
    /// A bus whose subjects each retain up to `capacity` undelivered events
    /// per subscriber before lagging (tokio rounds up to a power of two).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            channels: Subject::ALL
                .iter()
                .map(|_| broadcast::channel(capacity.max(1)).0)
                .collect(),
        }
    }

    /// Publish an event on its (payload-derived) subject. Returns the number
    /// of subscribers that will observe it — `0` (no subscribers) is a
    /// successful no-op, never an error.
    pub fn publish(&self, event: FiberEvent) -> usize {
        let subject = event.subject();
        // `send` errs only when there are zero receivers; the event is then
        // intentionally dropped (fibers carry observations, not commands).
        self.channels[subject.index()].send(event).unwrap_or(0)
    }

    /// Subscribe to one subject. The receiver sees every event published
    /// after this call; falling more than the capacity behind yields a
    /// typed `Lagged(n)` once, then delivery resumes from the oldest
    /// retained event.
    #[must_use]
    pub fn subscribe(&self, subject: Subject) -> broadcast::Receiver<FiberEvent> {
        self.channels[subject.index()].subscribe()
    }

    /// Live subscriber count for a subject (observability / tests).
    #[must_use]
    pub fn subscriber_count(&self, subject: Subject) -> usize {
        self.channels[subject.index()].receiver_count()
    }
}

impl Default for FiberBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

/// The process-wide fiber bus — created lazily on first access (the
/// `suggest::store()` idiom). The janitor runner publishes into it; any
/// module may subscribe. Tests construct their OWN [`FiberBus`] instead so
/// parallel tests never cross-talk.
static BUS: OnceLock<FiberBus> = OnceLock::new();

/// The shared bus handle (lazy global). Always available; publishing with
/// no subscribers is a no-op.
#[must_use]
pub fn bus() -> &'static FiberBus {
    BUS.get_or_init(FiberBus::default)
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;

    /// Catalog completeness: ALL lists every variant exactly once, slugs are
    /// unique, round-trip, and channel indices are dense + collision-free.
    #[test]
    fn subject_catalog_is_complete_and_collision_free() {
        assert_eq!(Subject::ALL.len(), 3);
        let mut slugs: Vec<&str> = Subject::ALL.iter().map(|s| s.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), Subject::ALL.len(), "slug collision");
        let mut indices: Vec<usize> = Subject::ALL.iter().map(|s| s.index()).collect();
        indices.sort_unstable();
        assert_eq!(indices, (0..Subject::ALL.len()).collect::<Vec<_>>());
        for &s in Subject::ALL {
            assert_eq!(Subject::from_slug(s.slug()), Some(s));
        }
        assert_eq!(Subject::from_slug("no-such-subject"), None);
    }

    /// Every payload family maps onto its declared subject (the typed
    /// addressing law: the payload IS the address).
    #[test]
    fn payload_families_derive_their_subjects() {
        let session = FiberEvent::Session(SessionEvent::GhostSessionReaped {
            session_id: String::from("00000000deadbeef"),
        });
        assert_eq!(session.subject(), Subject::Sessions);
        let board = FiberEvent::Board(BoardEvent::JanitorRowInjected {
            key: String::from("janitor:ghost-session:x"),
            title: String::from("ghost session"),
        });
        assert_eq!(board.subject(), Subject::Board);
        let finding = FiberEvent::Janitor(crate::janitors::JanitorFinding::observation(
            crate::janitors::JanitorKind::SuggestHealth,
            crate::janitors::FindingKind::SourceUnhealthy,
            String::from("k"),
            String::from("t"),
            String::from("d"),
            crate::suggest::Urgency::Low,
            0,
        ));
        assert_eq!(finding.subject(), Subject::Janitors);
    }

    /// Round-trip: a subscriber sees exactly the events published on its
    /// subject after it subscribed — and nothing from other subjects.
    #[test]
    fn publish_subscribe_round_trip_is_subject_scoped() {
        let bus = FiberBus::default();
        // Published before subscribe: never delivered.
        bus.publish(FiberEvent::Session(SessionEvent::GhostSessionReaped {
            session_id: String::from("early"),
        }));
        let mut sessions = bus.subscribe(Subject::Sessions);
        let mut board = bus.subscribe(Subject::Board);
        let n = bus.publish(FiberEvent::Session(SessionEvent::GhostSessionReaped {
            session_id: String::from("s1"),
        }));
        assert_eq!(n, 1, "one sessions subscriber");
        let got = sessions.try_recv().expect("sessions event delivered");
        match got {
            FiberEvent::Session(SessionEvent::GhostSessionReaped { session_id }) => {
                assert_eq!(session_id, "s1");
            }
            other => panic!("wrong payload family: {other:?}"),
        }
        // The board subscriber saw nothing — subjects are isolated.
        assert!(matches!(board.try_recv(), Err(TryRecvError::Empty)));
        // No-subscriber publish is a successful no-op.
        let none = FiberBus::default().publish(FiberEvent::Board(BoardEvent::JanitorRowInjected {
            key: String::from("k"),
            title: String::from("t"),
        }));
        assert_eq!(none, 0);
    }

    /// Lagged receiver: overflow the capacity, observe the typed `Lagged`
    /// signal ONCE, then delivery resumes from the oldest retained event —
    /// the publisher was never blocked.
    #[test]
    fn slow_subscriber_lags_then_recovers() {
        let bus = FiberBus::with_capacity(2);
        let mut rx = bus.subscribe(Subject::Sessions);
        for i in 0..5_u32 {
            bus.publish(FiberEvent::Session(SessionEvent::GhostSessionReaped {
                session_id: i.to_string(),
            }));
        }
        // Capacity 2 retains the newest 2; the first read reports the skip.
        match rx.try_recv() {
            Err(TryRecvError::Lagged(skipped)) => assert_eq!(skipped, 3),
            other => panic!("expected Lagged, got {other:?}"),
        }
        // Delivery resumes with the retained tail, in order.
        let mut tail = Vec::new();
        while let Ok(FiberEvent::Session(SessionEvent::GhostSessionReaped { session_id })) =
            rx.try_recv()
        {
            tail.push(session_id);
        }
        assert_eq!(tail, vec![String::from("3"), String::from("4")]);
    }
}
