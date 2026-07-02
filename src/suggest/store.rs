//! The ephemeral, ranked, in-memory suggestion store — the live cache the
//! parallel source watchers feed and the Ctrl-S picker reads.
//!
//! It is deliberately SEPARATE from praça's persisted preset catalog
//! (`PracaSnapshot.definitions`): suggestions are transient external signals,
//! not authored sessions, and must never pollute the saved presets. Each
//! source OWNS its slice of the store — an `ingest` replaces exactly that
//! source's set, preserving `first_seen_ms` for ids that persist (so the
//! shade-in animation rides on a stable birth time) and dropping vanished
//! ones. Other sources' entries are untouched.
//!
//! Beyond the raw set, the store carries the LIVING-BOARD algebra:
//!
//! * **Recurrence** — a vanished id leaves a bounded tombstone; if the same
//!   issue re-fires within the tombstone window it comes back with its
//!   original birth time and a bumped `times_seen` (the anomaly-recurrence
//!   pattern), so a repeat offender ranks above a first-timer and the picker
//!   can stamp `×N`.
//! * **Aging** — the ranked read adds a bounded waiting-time bonus, so an
//!   item that has sat unhandled slowly escalates within its urgency tier
//!   (never across tiers: urgency stays the dominant axis).
//! * **Lifecycle** — each row is `Offered → Accepted{session} /
//!   Snoozed{until} / Dismissed`. Accepted rows are soft-acked (demoted to
//!   the Idle tier — in progress, don't crowd new work); snoozed rows hide
//!   until their deadline; dismissed rows never surface again (their state
//!   survives re-ingest AND the tombstone window).
//! * **Source health** — the typed `PollOutcome` border records whether a
//!   source was actually observed (`Ok`) or could not be
//!   (`Unconfigured`/`AuthMissing`/`Error`), so "the board is calm" and "the
//!   board is blind" are distinguishable at a glance.
//!
//! A best-effort JSON snapshot lets a warm restart re-surface the last-known
//! set instantly while the watchers re-poll; a torn/absent snapshot simply
//! starts empty. The snapshot stamps its save time so ages rebase on load —
//! rows fresh at save stay fresh, rows stale at save still decay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use tokio::sync::watch;

use super::core::{SourceKind, SourceStatus, Suggestion, SuggestionId};
use crate::livestream::Reactive;

/// Snapshot framing magic — a schema tag so a future format bump is rejected
/// (start-empty) rather than silently mis-parsed.
const SNAPSHOT_MAGIC: &[u8] = b"mado-suggest v1\n";

/// How long a vanished id's tombstone (recurrence memory) is kept, ms. A
/// re-fire inside this window restores birth + bumps `times_seen`; outside it
/// the issue counts as brand-new. Six hours: long enough to catch a flapping
/// alert across a workday, short enough that yesterday's noise doesn't haunt.
const TOMBSTONE_TTL_MS: u64 = 6 * 60 * 60 * 1000;

/// Hard cap on remembered tombstones (memory insurance) — oldest-vanished
/// evicted first.
const TOMBSTONE_CAP: usize = 512;

/// Aging escalation: +this much effective score per minute waited…
const AGE_BONUS_PER_MIN: u64 = 2;
/// …capped at this many minutes (3h → +360), so aging nudges within a tier
/// but can never dominate genuine urgency/score differences alone.
const AGE_BONUS_CAP_MIN: u64 = 180;

/// Recurrence escalation: +this much effective score per re-sighting…
const RECUR_BONUS_PER_SEEN: u64 = 40;
/// …counting at most this many re-sightings (10 → +400).
const RECUR_BONUS_CAP: u32 = 10;

/// The low-bit budget of the composite rank key (urgency sits above bit 20).
/// score (≤1000) + aging (≤360) + recurrence (≤400) ≤ 1760 fits comfortably;
/// the mask is defense-in-depth so a future bonus can never leak into the
/// urgency bits.
const RANK_LOW_MASK: u64 = 0xF_FFFF;

/// Where a suggestion sits in its worked-on lifecycle. Serialized in the
/// snapshot so a dismissal survives a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SuggestionState {
    /// On the board, unworked.
    #[default]
    Offered,
    /// The operator opened a session for it — in progress. Soft-acked: the
    /// ranked read demotes it to the Idle tier so it stops crowding new work,
    /// and the picker badges it ◐ instead of ○.
    Accepted {
        /// The session name the accept spawned/switched to.
        session: String,
    },
    /// Hidden until the deadline passes, then Offered again.
    Snoozed { until_ms: u64 },
    /// Never offered again (until the source itself stops reporting it AND
    /// the tombstone window lapses).
    Dismissed,
}

/// A suggestion plus the store-side bookkeeping the renderer needs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredSuggestion {
    pub suggestion: Suggestion,
    /// When this id first appeared (ms) — the shade-in ramp anchor + the
    /// aging-escalation clock.
    #[serde(default)]
    pub first_seen_ms: u64,
    /// When this id was last confirmed present (ms) — decay/freshness.
    #[serde(default)]
    pub last_seen_ms: u64,
    /// How many times this issue has (re)appeared — 1 on first sighting,
    /// bumped when a tombstoned id re-fires (anomaly-recurrence).
    #[serde(default = "default_times_seen")]
    pub times_seen: u32,
    /// Worked-on lifecycle state.
    #[serde(default)]
    pub state: SuggestionState,
}

fn default_times_seen() -> u32 {
    1
}

/// Recurrence memory for a vanished id — enough to restore identity
/// continuity (birth, count, a sticky Dismissed) if the issue re-fires.
#[derive(Clone, Debug)]
struct Tombstone {
    first_seen_ms: u64,
    times_seen: u32,
    state: SuggestionState,
    vanished_ms: u64,
}

/// A source's last observed poll health — fed by the typed `PollOutcome`
/// border so an unobservable upstream is visible instead of silently empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceHealth {
    pub status: SourceStatus,
    /// When the source last completed a poll attempt (ms).
    pub last_poll_ms: u64,
    /// When the source last observed its upstream successfully (ms). 0 =
    /// never this process lifetime.
    pub last_ok_ms: u64,
}

/// Everything behind the ONE mutex — entries + recurrence tombstones +
/// per-source health share a lock so no two-lock ordering can deadlock.
#[derive(Default)]
struct StoreInner {
    entries: BTreeMap<SuggestionId, StoredSuggestion>,
    tombstones: BTreeMap<SuggestionId, Tombstone>,
    health: BTreeMap<SourceKind, SourceHealth>,
}

/// Thread-safe ranked suggestion cache. Cloneable handle pattern is not used
/// here (callers hold an `Arc<SuggestionStore>`); the inner state is mutexed.
///
/// A monotonic `generation` is bumped only on a *meaningful* change (an id
/// added/removed, a row's displayed/ranked fields or lifecycle state changed,
/// a source's health status transitioned — NOT a mere `last_seen_ms`
/// heartbeat). The picker memoizes its ranked read by it (O(1) reads while
/// the watchers idle) and the persist task uses it as the dirty signal — one
/// counter, the shikumi swap-then-observe contract.
///
/// The generation + the change-broadcast are the shared
/// [`Reactive`](crate::livestream::Reactive) core (stage 2 of the live-stream
/// substrate): every meaningful mutation bumps AND notifies every
/// [`subscribe`](SuggestionStore::subscribe)r, so the Ctrl-S board (and any
/// other surface) re-renders on the fact of a change instead of a fixed timer.
#[derive(Default)]
pub struct SuggestionStore {
    inner: Mutex<StoreInner>,
    reactive: Reactive,
}

/// Serializable view for the warm-restart snapshot.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct StoreSnapshot {
    pub entries: Vec<StoredSuggestion>,
    /// When this snapshot was written (ms). On load, each entry's freshness
    /// rebases by `now - saved_at` so age-at-save is preserved — a row fresh
    /// at save doesn't get decayed as stale just because the restart came
    /// hours later. 0 (legacy snapshots) = no rebase.
    #[serde(default)]
    pub saved_at_ms: u64,
}

/// The effective (living-board) rank key of a stored row at `now_ms`:
/// urgency dominates in the high bits — with an Accepted row demoted to the
/// Idle tier (soft-ack) — while the low bits carry score + the bounded
/// aging + recurrence escalations. Pure; the one ordering every read uses.
#[must_use]
pub fn effective_rank_key(st: &StoredSuggestion, now_ms: u64) -> u64 {
    let urgency = match st.state {
        SuggestionState::Accepted { .. } => super::core::Urgency::Idle,
        _ => st.suggestion.urgency,
    };
    let waited_min = now_ms.saturating_sub(st.first_seen_ms) / 60_000;
    let bonus = u64::from(st.suggestion.score.min(1000))
        + waited_min.min(AGE_BONUS_CAP_MIN) * AGE_BONUS_PER_MIN
        + u64::from(st.times_seen.saturating_sub(1).min(RECUR_BONUS_CAP))
            * RECUR_BONUS_PER_SEEN;
    (u64::from(urgency.weight()) << 20) | (bonus & RANK_LOW_MASK)
}

/// Whether a row is offerable on the board at `now_ms` — filters the
/// dismissed and the still-snoozed. Accepted rows remain offerable (demoted +
/// badged) so "in progress" stays legible instead of vanishing.
#[must_use]
fn offerable(st: &StoredSuggestion, now_ms: u64) -> bool {
    match st.state {
        SuggestionState::Dismissed => false,
        SuggestionState::Snoozed { until_ms } => now_ms >= until_ms,
        SuggestionState::Offered | SuggestionState::Accepted { .. } => true,
    }
}

impl SuggestionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current change-generation (Acquire). Bumped only on a meaningful change;
    /// the picker memoizes its ranked read by it + the persist task uses it as
    /// the dirty signal.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.reactive.generation()
    }

    /// A change-notification subscription (stage 3): a [`watch::Receiver`] that
    /// fires on every meaningful mutation. Async consumers `.changed().await`;
    /// the synchronous Ctrl-S board polls `.has_changed()` per frame and
    /// re-lists on the fact of a change — no fixed timer, no missed update.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.reactive.subscribe()
    }

    /// Bump the change-generation (Release) AND broadcast — called after a
    /// meaningful mutation.
    fn bump(&self) {
        self.reactive.bump();
    }

    /// Insert-or-update ONE row under an already-held lock, honoring the
    /// tombstone window (recurrence restore) and preserving lifecycle state on
    /// update. Returns whether the mutation was meaningful.
    fn upsert_locked(g: &mut StoreInner, s: Suggestion, now_ms: u64) -> bool {
        match g.entries.get_mut(&s.id) {
            Some(existing) => {
                // Bump only on a DISPLAYED/RANKED change, not a mere
                // last_seen heartbeat — else every 30s poll would force a
                // re-render + a disk write for an unchanged row.
                let meaningful = existing.suggestion != s;
                existing.last_seen_ms = now_ms;
                existing.suggestion = s;
                meaningful
            }
            None => {
                // A re-fire inside the tombstone window keeps its identity:
                // original birth (aging keeps counting), bumped times_seen
                // (recurrence escalation), and a sticky Dismissed.
                let tomb = g.tombstones.remove(&s.id).filter(|t| {
                    now_ms.saturating_sub(t.vanished_ms) <= TOMBSTONE_TTL_MS
                });
                let (first_seen_ms, times_seen, state) = match tomb {
                    Some(t) => (t.first_seen_ms, t.times_seen.saturating_add(1), t.state),
                    None => (now_ms, 1, SuggestionState::Offered),
                };
                g.entries.insert(
                    s.id,
                    StoredSuggestion {
                        first_seen_ms,
                        last_seen_ms: now_ms,
                        times_seen,
                        state,
                        suggestion: s,
                    },
                );
                true
            }
        }
    }

    /// Move a dropped entry into the recurrence tombstones (bounded).
    fn tombstone_locked(g: &mut StoreInner, st: &StoredSuggestion, now_ms: u64) {
        g.tombstones.insert(
            st.suggestion.id,
            Tombstone {
                first_seen_ms: st.first_seen_ms,
                times_seen: st.times_seen,
                state: st.state.clone(),
                vanished_ms: now_ms,
            },
        );
        if g.tombstones.len() > TOMBSTONE_CAP {
            // Evict oldest-vanished first.
            let mut by_age: Vec<(SuggestionId, u64)> = g
                .tombstones
                .iter()
                .map(|(id, t)| (*id, t.vanished_ms))
                .collect();
            by_age.sort_by(|a, b| a.1.cmp(&b.1));
            let drop_n = g.tombstones.len() - TOMBSTONE_CAP;
            for (id, _) in by_age.into_iter().take(drop_n) {
                g.tombstones.remove(&id);
            }
        }
    }

    /// Replace the suggestion set contributed by `source`. Ids that persist
    /// keep their `first_seen_ms` + lifecycle state; ids of this source that
    /// vanished are tombstoned (recurrence memory) and dropped; entries from
    /// other sources are untouched.
    pub fn ingest(&self, source: SourceKind, items: Vec<Suggestion>, now_ms: u64) {
        let mut changed = false;
        {
            let mut g = self.lock();
            let incoming: BTreeSet<SuggestionId> = items.iter().map(|s| s.id).collect();
            // Tombstone + drop this source's vanished ids (keep all other
            // sources'). Two passes because the borrow of the drop set ends
            // before mutation of the tombstone map.
            let dropped: Vec<StoredSuggestion> = g
                .entries
                .values()
                .filter(|st| st.suggestion.source == source && !incoming.contains(&st.suggestion.id))
                .cloned()
                .collect();
            for st in &dropped {
                Self::tombstone_locked(&mut g, st, now_ms);
                g.entries.remove(&st.suggestion.id);
                changed = true;
            }
            for s in items {
                if Self::upsert_locked(&mut g, s, now_ms) {
                    changed = true;
                }
            }
        }
        if changed {
            self.bump();
        }
    }

    /// Additive single-row upsert — the agent/MCP inject path. Unlike
    /// [`SuggestionStore::ingest`] it never drops the source's other rows, so
    /// repeated injections accumulate (and decay by TTL like everything else).
    pub fn upsert(&self, s: Suggestion, now_ms: u64) {
        let changed = {
            let mut g = self.lock();
            Self::upsert_locked(&mut g, s, now_ms)
        };
        if changed {
            self.bump();
        }
    }

    /// Record a source's poll health from the typed `PollOutcome` border.
    /// Bumps/broadcasts only on a STATUS transition (Ok→Error, …) — the
    /// timestamps alone are heartbeats.
    pub fn record_poll(&self, source: SourceKind, status: SourceStatus, now_ms: u64) {
        let changed = {
            let mut g = self.lock();
            let entry = g.health.entry(source).or_insert(SourceHealth {
                status,
                last_poll_ms: 0,
                last_ok_ms: 0,
            });
            let transitioned = entry.status != status || entry.last_poll_ms == 0;
            entry.status = status;
            entry.last_poll_ms = now_ms;
            if status == SourceStatus::Ok {
                entry.last_ok_ms = now_ms;
            }
            transitioned
        };
        if changed {
            self.bump();
        }
    }

    /// Every source's last-known poll health, catalog-ordered.
    #[must_use]
    pub fn health(&self) -> Vec<(SourceKind, SourceHealth)> {
        let g = self.lock();
        g.health.iter().map(|(k, h)| (*k, *h)).collect()
    }

    /// Mark a row Accepted (in progress) — the picker's accept path calls this
    /// with the session it spawned/switched to. `false` if the id is gone.
    pub fn mark_accepted(&self, id: SuggestionId, session: &str) -> bool {
        self.set_state(
            id,
            SuggestionState::Accepted {
                session: session.to_owned(),
            },
        )
    }

    /// Dismiss a row — never offered again (survives re-ingest + tombstone).
    pub fn dismiss(&self, id: SuggestionId) -> bool {
        self.set_state(id, SuggestionState::Dismissed)
    }

    /// Snooze a row until `until_ms`.
    pub fn snooze(&self, id: SuggestionId, until_ms: u64) -> bool {
        self.set_state(id, SuggestionState::Snoozed { until_ms })
    }

    /// Whether this id is Dismissed — on its live entry OR its tombstone
    /// (a dismissed row that decayed still counts, so the exemption in
    /// `apply_poll`'s budget survives the gap between decay and re-fire).
    #[must_use]
    pub fn is_dismissed(&self, id: SuggestionId) -> bool {
        let g = self.lock();
        match g.entries.get(&id) {
            Some(st) => matches!(st.state, SuggestionState::Dismissed),
            None => g
                .tombstones
                .get(&id)
                .is_some_and(|t| matches!(t.state, SuggestionState::Dismissed)),
        }
    }

    fn set_state(&self, id: SuggestionId, state: SuggestionState) -> bool {
        let changed = {
            let mut g = self.lock();
            match g.entries.get_mut(&id) {
                Some(st) if st.state != state => {
                    st.state = state;
                    true
                }
                Some(_) => return true, // already in that state — success, no bump
                None => return false,
            }
        };
        if changed {
            self.bump();
        }
        true
    }

    /// Drop every entry whose `last_seen_ms` is older than `ttl_ms` — the
    /// decay pass (a source that stops reporting an item lets it age out even
    /// if the source itself never re-polls). Decayed ids leave tombstones so a
    /// slow flap still counts as recurrence.
    pub fn decay(&self, now_ms: u64, ttl_ms: u64) {
        self.decay_per_source(now_ms, |_| ttl_ms);
    }

    /// Per-source decay: each entry ages out against `ttl_for(its source)`, so a
    /// slow source (e.g. a 1h poll) doesn't flicker under a fast global TTL. A
    /// `ttl_for` of 0 means that source's entries never age out. Also purges
    /// tombstones past their recurrence window.
    pub fn decay_per_source(&self, now_ms: u64, ttl_for: impl Fn(SourceKind) -> u64) {
        let removed = {
            let mut g = self.lock();
            let stale: Vec<StoredSuggestion> = g
                .entries
                .values()
                .filter(|st| {
                    let ttl = ttl_for(st.suggestion.source);
                    ttl != 0 && now_ms.saturating_sub(st.last_seen_ms) > ttl
                })
                .cloned()
                .collect();
            for st in &stale {
                Self::tombstone_locked(&mut g, st, now_ms);
                g.entries.remove(&st.suggestion.id);
            }
            g.tombstones
                .retain(|_, t| now_ms.saturating_sub(t.vanished_ms) <= TOMBSTONE_TTL_MS);
            !stale.is_empty()
        };
        if removed {
            self.bump();
        }
    }

    /// Hard memory cap: if the store exceeds `max_entries`, evict until it
    /// fits — non-offerable rows (Dismissed, still-Snoozed) go first (they
    /// are invisible; their lifecycle state survives in the tombstone), then
    /// the lowest EFFECTIVE-ranked / stalest (the same living-board axis the
    /// picker orders by, so an Accepted row's demotion counts here too).
    /// Every eviction is tombstoned, so a gc under pressure can never
    /// resurrect a dismissal or forget a recurrence count. `max_entries == 0`
    /// is unbounded. Insurance against a source that stops polling with no TTL.
    pub fn gc(&self, max_entries: usize, now_ms: u64) {
        if max_entries == 0 {
            return;
        }
        let removed = {
            let mut g = self.lock();
            if g.entries.len() <= max_entries {
                return;
            }
            let mut ranked: Vec<(SuggestionId, bool, u64, u64)> = g
                .entries
                .values()
                .map(|st| {
                    (
                        st.suggestion.id,
                        offerable(st, now_ms),
                        effective_rank_key(st, now_ms),
                        st.last_seen_ms,
                    )
                })
                .collect();
            // Keep the top `max_entries`: offerable first, then effective
            // rank desc, then fresher, then id.
            ranked.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then(b.2.cmp(&a.2))
                    .then(b.3.cmp(&a.3))
                    .then(a.0.cmp(&b.0))
            });
            let keep: BTreeSet<SuggestionId> =
                ranked.into_iter().take(max_entries).map(|(id, ..)| id).collect();
            let evicted: Vec<StoredSuggestion> = g
                .entries
                .values()
                .filter(|st| !keep.contains(&st.suggestion.id))
                .cloned()
                .collect();
            for st in &evicted {
                Self::tombstone_locked(&mut g, st, now_ms);
                g.entries.remove(&st.suggestion.id);
            }
            !evicted.is_empty()
        };
        if removed {
            self.bump();
        }
    }

    /// The top `max` offerable suggestions at `now_ms`, in living-board order
    /// ([`effective_rank_key`]: urgency → score+aging+recurrence → birth → id).
    #[must_use]
    pub fn ranked(&self, max: usize, now_ms: u64) -> Vec<Suggestion> {
        self.ranked_stored(max, now_ms)
            .into_iter()
            .map(|st| st.suggestion)
            .collect()
    }

    /// Like [`SuggestionStore::ranked`] but carries the store bookkeeping
    /// (birth for shade-in, `times_seen` for ×N, `state` for the ◐ badge).
    /// Dismissed and still-snoozed rows are filtered out here — the one
    /// offerability gate every read shares.
    #[must_use]
    pub fn ranked_stored(&self, max: usize, now_ms: u64) -> Vec<StoredSuggestion> {
        let g = self.lock();
        let mut v: Vec<StoredSuggestion> = g
            .entries
            .values()
            .filter(|st| offerable(st, now_ms))
            .cloned()
            .collect();
        drop(g);
        v.sort_by(|a, b| {
            effective_rank_key(b, now_ms)
                .cmp(&effective_rank_key(a, now_ms))
                .then(a.first_seen_ms.cmp(&b.first_seen_ms))
                .then(a.suggestion.id.cmp(&b.suggestion.id))
        });
        v.truncate(max);
        v
    }

    /// Resolve a suggestion by id (the accept path looks up the spawn target).
    #[must_use]
    pub fn get(&self, id: SuggestionId) -> Option<Suggestion> {
        self.lock().entries.get(&id).map(|st| st.suggestion.clone())
    }

    /// Resolve the full stored row (lifecycle + recurrence bookkeeping).
    #[must_use]
    pub fn get_stored(&self, id: SuggestionId) -> Option<StoredSuggestion> {
        self.lock().entries.get(&id).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }
    pub fn clear(&self) {
        let had = {
            let mut g = self.lock();
            let had = !g.entries.is_empty();
            g.entries.clear();
            g.tombstones.clear();
            had
        };
        if had {
            self.bump();
        }
    }

    /// Alpha (0..=255) for an id's shade-in ramp: 0 at birth, 255 after
    /// `shade_in_ms`. Used by the renderer to gently fade new rows in.
    #[must_use]
    pub fn shade_alpha(&self, id: SuggestionId, now_ms: u64, shade_in_ms: u64) -> u8 {
        let first = match self.lock().entries.get(&id) {
            Some(st) => st.first_seen_ms,
            None => return 255,
        };
        shade_ramp(first, now_ms, shade_in_ms)
    }

    /// Snapshot the current set for a warm restart, stamped with the save time
    /// so ages rebase on load.
    #[must_use]
    pub fn to_snapshot(&self, now_ms: u64) -> StoreSnapshot {
        StoreSnapshot {
            entries: self.lock().entries.values().cloned().collect(),
            saved_at_ms: now_ms,
        }
    }

    /// Replace the store from a snapshot (warm restart). Each entry's
    /// freshness is REBASED by `now - saved_at` so age-at-save is preserved:
    /// a row fresh at save is fresh now (survives the post-load decay), a row
    /// already stale at save still decays. Birth times stay absolute — aging
    /// keeps counting from the true first sighting. Legacy snapshots
    /// (`saved_at_ms == 0`, or an implausible future stamp) load unrebased.
    pub fn load_snapshot(&self, snap: StoreSnapshot, now_ms: u64) {
        let shift = if snap.saved_at_ms > 0 && now_ms >= snap.saved_at_ms {
            now_ms - snap.saved_at_ms
        } else {
            0
        };
        {
            let mut g = self.lock();
            g.entries.clear();
            for mut st in snap.entries {
                st.last_seen_ms = st.last_seen_ms.saturating_add(shift);
                g.entries.insert(st.suggestion.id, st);
            }
        }
        self.bump();
    }

    /// Warm-restart load: a magic-framed, BLAKE3-verified snapshot file. A
    /// missing file, a wrong schema magic (format bump), or a torn body (hash
    /// mismatch) all start empty — never feed garbage rows to the picker.
    pub fn load_file(&self, path: &Path, now_ms: u64) {
        // Reclaim `.tmp.<pid>` temps a crashed prior run left behind, before we
        // read. The atomic persist renames its own temp away on success, so the
        // only way one lingers is a crash between create and rename.
        sweep_orphan_temps(path);
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Some(json) = unframe_snapshot(&bytes) else {
            return; // bad magic / hash mismatch → start empty
        };
        if let Ok(snap) = serde_json::from_slice::<StoreSnapshot>(&json) {
            self.load_snapshot(snap, now_ms);
        }
    }

    /// Crash-safe atomic persist: serialize → BLAKE3-frame → write a pid-tagged
    /// temp (after `create_dir_all`) → `sync_all` → rename. Snapshotting clones
    /// under the lock then drops it, so the disk I/O is lock-free. Best-effort —
    /// a write failure never blocks the caller.
    pub fn persist_file(&self, path: &Path, now_ms: u64) {
        let snap = self.to_snapshot(now_ms);
        let Ok(json) = serde_json::to_vec(&snap) else {
            return;
        };
        let framed = frame_snapshot(&json);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // pid-tagged temp so two processes never race the same temp name.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp.");
        tmp.push(std::process::id().to_string());
        let tmp = std::path::PathBuf::from(tmp);
        use std::io::Write;
        let Ok(mut f) = std::fs::File::create(&tmp) else {
            return;
        };
        if f.write_all(&framed).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let _ = f.sync_all(); // durable before the rename
        drop(f);
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Reclaim sibling temp files (`<file>.tmp.<pid>`) left by a crashed prior
/// persist. Default staleness floor is 5 minutes so a CONCURRENT process's
/// in-flight temp — always seconds old — is never deleted out from under it.
/// Best-effort; any I/O error is ignored.
fn sweep_orphan_temps(path: &Path) {
    sweep_orphan_temps_with(path, std::time::SystemTime::now(), 300);
}

/// Inner, testable form of [`sweep_orphan_temps`]: `now` + the staleness floor
/// are injected so a test can prove both directions (fresh temp kept, stale
/// temp reclaimed) without touching a file's real mtime.
fn sweep_orphan_temps_with(path: &Path, now: std::time::SystemTime, max_age_secs: u64) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let mut prefix = String::from(fname);
    prefix.push_str(".tmp.");
    let Ok(rd) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Only `<file>.tmp.<digits>` — our own pid-tagged temp shape.
        let Some(suffix) = name.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age.as_secs() >= max_age_secs);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Frame a JSON snapshot: `MAGIC || blake3-hex || '\n' || json`. The magic is a
/// schema tag; the embedded hash makes a torn file detectable on load.
#[must_use]
fn frame_snapshot(json: &[u8]) -> Vec<u8> {
    let hex = blake3::hash(json).to_hex();
    let mut out = Vec::with_capacity(SNAPSHOT_MAGIC.len() + hex.len() + 1 + json.len());
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(hex.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(json);
    out
}

/// Inverse of [`frame_snapshot`]: `None` on wrong magic (schema bump) or a hash
/// mismatch (torn/corrupt) — both mean start-empty.
#[must_use]
fn unframe_snapshot(bytes: &[u8]) -> Option<Vec<u8>> {
    let rest = bytes.strip_prefix(SNAPSHOT_MAGIC)?;
    let nl = rest.iter().position(|&b| b == b'\n')?;
    let (hex, after) = rest.split_at(nl);
    let json = &after[1..]; // skip the newline
    if blake3::hash(json).to_hex().as_bytes() != hex {
        return None; // torn / corrupt
    }
    Some(json.to_vec())
}

/// Diversify a rank-ordered suggestion list: keep the order, but cap how many
/// rows any single source may contribute, so one noisy source (20 CrashLoop
/// pods) can't drown your PRs / tickets / incidents. `cap == 0` disables the
/// cap. Pure — the unit the picker's balanced band is tested through.
#[must_use]
pub fn balance_per_source(
    items: Vec<StoredSuggestion>,
    max: usize,
    cap: usize,
) -> Vec<StoredSuggestion> {
    let mut counts: BTreeMap<SourceKind, usize> = BTreeMap::new();
    let mut out: Vec<StoredSuggestion> = Vec::with_capacity(max.min(items.len()));
    for st in items {
        if out.len() >= max {
            break;
        }
        if cap > 0 {
            let c = counts.entry(st.suggestion.source).or_insert(0);
            if *c >= cap {
                continue;
            }
            *c += 1;
        }
        out.push(st);
    }
    out
}

/// Pure shade-in ramp — factored out for testing.
#[must_use]
pub fn shade_ramp(first_seen_ms: u64, now_ms: u64, shade_in_ms: u64) -> u8 {
    if shade_in_ms == 0 {
        return 255;
    }
    let elapsed = now_ms.saturating_sub(first_seen_ms);
    if elapsed >= shade_in_ms {
        return 255;
    }
    // 0..255 linear ramp.
    let frac = (elapsed.saturating_mul(255)) / shade_in_ms;
    u8::try_from(frac.min(255)).unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::{SpawnSpec, Urgency};
    use proptest::prelude::*;

    fn sug(source: SourceKind, key: &str, title: &str) -> Suggestion {
        Suggestion::new(source, key, title, SpawnSpec::new("/code/x", title).unwrap())
    }

    #[test]
    fn ingest_replaces_only_its_own_source() {
        let store = SuggestionStore::new();
        store.ingest(
            SourceKind::TendRepos,
            vec![sug(SourceKind::TendRepos, "a", "a"), sug(SourceKind::TendRepos, "b", "b")],
            100,
        );
        store.ingest(SourceKind::GitBranchPr, vec![sug(SourceKind::GitBranchPr, "c", "c")], 100);
        assert_eq!(store.len(), 3);
        // Re-ingest tend with only "a" → "b" drops, git "c" untouched.
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 200);
        assert_eq!(store.len(), 2);
        assert!(store.get(SuggestionId::derive(SourceKind::GitBranchPr, "c")).is_some());
        assert!(store.get(SuggestionId::derive(SourceKind::TendRepos, "b")).is_none());
    }

    #[test]
    fn first_seen_is_preserved_across_reingest() {
        let store = SuggestionStore::new();
        let id = SuggestionId::derive(SourceKind::TendRepos, "a");
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 500);
        // shade_alpha uses first_seen=100, so at now=100+shade it's full; the
        // re-ingest at 500 must NOT reset birth.
        let st = store.ranked_stored(10, 500);
        let first = st.iter().find(|s| s.suggestion.id == id).unwrap().first_seen_ms;
        assert_eq!(first, 100, "first_seen preserved across re-ingest");
    }

    #[test]
    fn recurrence_survives_the_tombstone_window() {
        let store = SuggestionStore::new();
        let id = SuggestionId::derive(SourceKind::GrafanaAlerts, "flap");
        // Fires, resolves (vanishes), re-fires within the window → identity
        // continuity: original birth, times_seen 2.
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "flap", "flap")],
            1_000,
        );
        store.ingest(SourceKind::GrafanaAlerts, vec![], 2_000);
        assert!(store.get(id).is_none(), "vanished from the board");
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "flap", "flap")],
            60_000,
        );
        let st = store.get_stored(id).expect("re-fired");
        assert_eq!(st.times_seen, 2, "recurrence counted");
        assert_eq!(st.first_seen_ms, 1_000, "original birth restored");

        // A re-fire AFTER the window is a brand-new issue.
        store.ingest(SourceKind::GrafanaAlerts, vec![], 70_000);
        let later = 70_000 + TOMBSTONE_TTL_MS + 1;
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "flap", "flap")],
            later,
        );
        let st = store.get_stored(id).expect("re-fired late");
        assert_eq!(st.times_seen, 1, "outside the window = new issue");
        assert_eq!(st.first_seen_ms, later);
    }

    #[test]
    fn recurrence_and_aging_escalate_within_a_tier_never_across() {
        let now = 10 * 60 * 60 * 1000; // 10h
        let mk = |first_seen_ms: u64, times_seen: u32, urgency: Urgency| StoredSuggestion {
            suggestion: sug(SourceKind::TendRepos, "k", "t").urgent(urgency),
            first_seen_ms,
            last_seen_ms: now,
            times_seen,
            state: SuggestionState::Offered,
        };
        // Same tier: an old repeat offender outranks a fresh first-timer.
        let fresh = mk(now, 1, Urgency::Normal);
        let veteran = mk(now - 3_600_000, 5, Urgency::Normal);
        assert!(
            effective_rank_key(&veteran, now) > effective_rank_key(&fresh, now),
            "aging + recurrence escalate within the tier"
        );
        // Across tiers: no amount of aging/recurrence beats real urgency.
        let ancient_low = mk(0, u32::MAX, Urgency::Low);
        let fresh_normal = mk(now, 1, Urgency::Normal);
        assert!(
            effective_rank_key(&fresh_normal, now) > effective_rank_key(&ancient_low, now),
            "urgency stays the dominant axis"
        );
    }

    #[test]
    fn lifecycle_accept_demotes_snooze_hides_dismiss_removes() {
        let store = SuggestionStore::new();
        let hot = sug(SourceKind::GrafanaAlerts, "hot", "hot").urgent(Urgency::Critical);
        let calm = sug(SourceKind::TendRepos, "calm", "calm").urgent(Urgency::Normal);
        let hot_id = hot.id;
        let calm_id = calm.id;
        store.ingest(SourceKind::GrafanaAlerts, vec![hot], 1_000);
        store.ingest(SourceKind::TendRepos, vec![calm], 1_000);
        assert_eq!(store.ranked(10, 1_000)[0].id, hot_id, "critical on top");

        // Accept → soft-ack: demoted below the calm Offered row, still listed.
        assert!(store.mark_accepted(hot_id, "🔥 hot"));
        let ranked = store.ranked(10, 1_000);
        assert_eq!(ranked.len(), 2, "accepted stays on the board");
        assert_eq!(ranked[0].id, calm_id, "accepted row is demoted (in progress)");
        assert!(matches!(
            store.get_stored(hot_id).unwrap().state,
            SuggestionState::Accepted { .. }
        ));

        // Accepted state survives the source re-reporting the item.
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "hot", "hot").urgent(Urgency::Critical)],
            2_000,
        );
        assert!(matches!(
            store.get_stored(hot_id).unwrap().state,
            SuggestionState::Accepted { .. }
        ));

        // Snooze hides until the deadline, then re-offers.
        assert!(store.snooze(calm_id, 5_000));
        assert!(
            !store.ranked(10, 4_999).iter().any(|s| s.id == calm_id),
            "snoozed row hidden before the deadline"
        );
        assert!(
            store.ranked(10, 5_000).iter().any(|s| s.id == calm_id),
            "snoozed row re-offered at the deadline"
        );

        // Dismiss removes from every read and STICKS across re-ingest + the
        // tombstone window.
        assert!(store.dismiss(calm_id));
        assert!(!store.ranked(10, 6_000).iter().any(|s| s.id == calm_id));
        store.ingest(
            SourceKind::TendRepos,
            vec![sug(SourceKind::TendRepos, "calm", "calm")],
            7_000,
        );
        assert!(
            !store.ranked(10, 7_000).iter().any(|s| s.id == calm_id),
            "dismissed survives re-ingest"
        );
        store.ingest(SourceKind::TendRepos, vec![], 8_000); // vanish → tombstone
        store.ingest(
            SourceKind::TendRepos,
            vec![sug(SourceKind::TendRepos, "calm", "calm")],
            9_000,
        );
        assert!(
            !store.ranked(10, 9_000).iter().any(|s| s.id == calm_id),
            "dismissed survives the tombstone round-trip"
        );

        // An unknown id fails, a repeat set succeeds without a bump.
        assert!(!store.dismiss(SuggestionId(42)));
    }

    #[test]
    fn upsert_is_additive_never_drops_siblings() {
        let store = SuggestionStore::new();
        store.upsert(sug(SourceKind::TendRepos, "a", "a"), 100);
        store.upsert(sug(SourceKind::TendRepos, "b", "b"), 200);
        assert_eq!(store.len(), 2, "upserts accumulate");
        store.upsert(sug(SourceKind::TendRepos, "a", "a2"), 300);
        assert_eq!(store.len(), 2, "same id updates in place");
        assert_eq!(
            store.get(SuggestionId::derive(SourceKind::TendRepos, "a")).unwrap().title,
            "a2"
        );
    }

    #[test]
    fn record_poll_bumps_on_transition_not_heartbeat() {
        let store = SuggestionStore::new();
        let g0 = store.generation();
        store.record_poll(SourceKind::JiraAssigned, SourceStatus::Ok, 1_000);
        let g1 = store.generation();
        assert!(g1 > g0, "first poll records (transition from unknown)");
        store.record_poll(SourceKind::JiraAssigned, SourceStatus::Ok, 2_000);
        assert_eq!(store.generation(), g1, "same-status heartbeat must not bump");
        store.record_poll(SourceKind::JiraAssigned, SourceStatus::Error, 3_000);
        assert!(store.generation() > g1, "a status transition bumps");
        let health = store.health();
        let (_, h) = health.iter().find(|(k, _)| *k == SourceKind::JiraAssigned).unwrap();
        assert_eq!(h.status, SourceStatus::Error);
        assert_eq!(h.last_ok_ms, 2_000, "last_ok remembers the last good poll");
    }

    #[test]
    fn ranked_orders_by_urgency_then_score() {
        let store = SuggestionStore::new();
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "fire", "fire").urgent(Urgency::Critical)],
            100,
        );
        store.ingest(
            SourceKind::TendRepos,
            vec![sug(SourceKind::TendRepos, "repo", "repo").urgent(Urgency::Low)],
            100,
        );
        let ranked = store.ranked(10, 100);
        assert_eq!(ranked[0].source, SourceKind::GrafanaAlerts, "critical first");
    }

    #[test]
    fn decay_drops_stale_entries() {
        let store = SuggestionStore::new();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        store.decay(100 + 5000, 1000);
        assert_eq!(store.len(), 0, "stale entry decayed");
    }

    #[test]
    fn shade_ramp_is_linear_and_clamps() {
        assert_eq!(shade_ramp(0, 0, 600), 0);
        assert_eq!(shade_ramp(0, 300, 600), 127);
        assert_eq!(shade_ramp(0, 600, 600), 255);
        assert_eq!(shade_ramp(0, 10_000, 600), 255);
        assert_eq!(shade_ramp(0, 0, 0), 255, "zero shade = instant solid");
    }

    #[test]
    fn snapshot_round_trips_and_rebases_ages() {
        let store = SuggestionStore::new();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100_000);
        let snap = store.to_snapshot(200_000);
        assert_eq!(snap.saved_at_ms, 200_000);

        // Load 1 hour later: the entry was 100s old at save; it must be 100s
        // old relative to NOW after load (not 1h+100s old).
        let restored = SuggestionStore::new();
        let now = 3_800_000;
        restored.load_snapshot(snap, now);
        assert_eq!(restored.len(), 1);
        let st = restored.ranked_stored(1, now)[0].clone();
        assert_eq!(now - st.last_seen_ms, 100_000, "age-at-save preserved");
        assert_eq!(st.first_seen_ms, 100_000, "birth stays absolute (aging clock)");
        // The post-load decay with a 15-min TTL keeps it (it is 100s old).
        restored.decay(now, 900_000);
        assert_eq!(restored.len(), 1, "fresh-at-save row survives restart decay");

        // Legacy snapshot (saved_at 0) loads unrebased.
        let legacy = StoreSnapshot {
            entries: vec![StoredSuggestion {
                suggestion: sug(SourceKind::TendRepos, "b", "b"),
                first_seen_ms: 50,
                last_seen_ms: 50,
                times_seen: 1,
                state: SuggestionState::Offered,
            }],
            saved_at_ms: 0,
        };
        let old = SuggestionStore::new();
        old.load_snapshot(legacy, now);
        assert_eq!(old.ranked_stored(1, now)[0].last_seen_ms, 50, "legacy: no rebase");
    }

    #[test]
    fn framed_persist_load_round_trips_and_rejects_corruption() {
        let dir = std::env::temp_dir().join("mado-suggest-test-persist");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("snap.json");
        let _ = std::fs::remove_file(&path);

        let store = SuggestionStore::new();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        store.persist_file(&path, 100);

        // Round-trip: a fresh store warm-loads the set.
        let loaded = SuggestionStore::new();
        loaded.load_file(&path, 100);
        assert_eq!(loaded.len(), 1, "warm restart re-surfaces the set");

        // A torn body (flip a json byte) → hash mismatch → start empty.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let torn = SuggestionStore::new();
        torn.load_file(&path, 100);
        assert_eq!(torn.len(), 0, "a torn body is rejected → empty");

        // Wrong magic (a foreign/old file) → start empty.
        std::fs::write(&path, b"garbage not ours").unwrap();
        let bad = SuggestionStore::new();
        bad.load_file(&path, 100);
        assert_eq!(bad.len(), 0, "wrong magic → empty");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn orphan_temp_sweep_reclaims_stale_keeps_fresh() {
        let dir = std::env::temp_dir().join("mado-suggest-test-sweep");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("snap.json");

        // A crashed run's leftover temp + a non-matching sibling + a "live"
        // concurrent temp (created fresh now).
        let orphan = dir.join("snap.json.tmp.4242");
        let unrelated = dir.join("snap.json.bak");
        let live = dir.join("snap.json.tmp.9999");
        std::fs::write(&orphan, b"x").unwrap();
        std::fs::write(&unrelated, b"x").unwrap();
        std::fs::write(&live, b"x").unwrap();

        // With max_age 0, every matching temp counts as stale → orphan + live
        // both reclaimed; the unrelated sibling is untouched.
        sweep_orphan_temps_with(&path, std::time::SystemTime::now(), 0);
        assert!(!orphan.exists(), "a pid-tagged temp is reclaimed");
        assert!(!live.exists(), "max_age 0 reclaims even a fresh temp");
        assert!(unrelated.exists(), "a non-temp sibling is never touched");

        // Safety direction: a fresh temp under the real 5-min floor SURVIVES,
        // so a concurrent process's in-flight write is never deleted.
        std::fs::write(&orphan, b"x").unwrap();
        sweep_orphan_temps_with(&path, std::time::SystemTime::now(), 300);
        assert!(orphan.exists(), "a fresh temp is kept under the staleness floor");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generation_bumps_on_change_not_on_last_seen_heartbeat() {
        let store = SuggestionStore::new();
        let g0 = store.generation();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        let g1 = store.generation();
        assert!(g1 > g0, "first insert bumps");

        // Re-ingest the SAME suggestion (only last_seen advances) → NO bump.
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 500);
        assert_eq!(store.generation(), g1, "a last_seen heartbeat must not bump");

        // A changed displayed field → bump.
        let mut changed = sug(SourceKind::TendRepos, "a", "a");
        changed.title = String::from("a CHANGED");
        store.ingest(SourceKind::TendRepos, vec![changed], 600);
        let g2 = store.generation();
        assert!(g2 > g1, "a displayed change bumps");

        // Removal → bump.
        store.ingest(SourceKind::TendRepos, vec![], 700);
        assert!(store.generation() > g2, "removal bumps");
    }

    #[test]
    fn subscribe_broadcasts_on_change_not_on_heartbeat() {
        // Stage 2: an ingest (meaningful change) wakes a subscriber; a pure
        // last_seen heartbeat does not. This is the store→GUI wake the Ctrl-S
        // board polls each frame.
        let store = SuggestionStore::new();
        let mut rx = store.subscribe();
        assert!(!rx.has_changed().unwrap(), "no change before first ingest");

        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        assert!(rx.has_changed().unwrap(), "an ingest broadcasts to subscribers");
        rx.mark_unchanged();

        // Re-ingest the SAME row (only last_seen advances) → no broadcast.
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 500);
        assert!(!rx.has_changed().unwrap(), "a heartbeat must not wake a subscriber");

        // A new row → broadcast again.
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "b", "b")], 600);
        assert!(rx.has_changed().unwrap(), "a new row wakes the subscriber");
    }

    #[test]
    fn gc_caps_total_keeping_highest_ranked() {
        let store = SuggestionStore::new();
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "fire", "fire").urgent(Urgency::Critical)],
            100,
        );
        let mut lows = Vec::new();
        for k in ["a", "b", "c", "d", "e"] {
            lows.push(sug(SourceKind::TendRepos, k, k).urgent(Urgency::Low));
        }
        store.ingest(SourceKind::TendRepos, lows, 100);
        assert_eq!(store.len(), 6);

        store.gc(3, 100);
        assert_eq!(store.len(), 3, "capped to 3");
        assert!(
            store.get(SuggestionId::derive(SourceKind::GrafanaAlerts, "fire")).is_some(),
            "the Critical row is kept (highest rank)"
        );

        store.gc(0, 100); // unbounded → no-op
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn gc_tombstones_evictions_and_prefers_evicting_non_offerable() {
        let store = SuggestionStore::new();
        // Three rows: a dismissed Critical (invisible, raw-rank highest), an
        // offerable Normal, an offerable Low.
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "dead", "dead").urgent(Urgency::Critical)],
            100,
        );
        store.ingest(
            SourceKind::TendRepos,
            vec![
                sug(SourceKind::TendRepos, "mid", "mid").urgent(Urgency::Normal),
                sug(SourceKind::TendRepos, "low", "low").urgent(Urgency::Low),
            ],
            100,
        );
        let dead_id = SuggestionId::derive(SourceKind::GrafanaAlerts, "dead");
        assert!(store.dismiss(dead_id));
        // Cap to 2: the DISMISSED row is evicted first despite its raw rank —
        // offerable rows win the survival contest.
        store.gc(2, 200);
        assert_eq!(store.len(), 2);
        assert!(store.get(dead_id).is_none(), "non-offerable evicted first");
        assert!(store.get(SuggestionId::derive(SourceKind::TendRepos, "mid")).is_some());
        // The eviction left a tombstone carrying Dismissed: a re-ingest from
        // the still-firing source must NOT resurrect the row as Offered.
        store.ingest(
            SourceKind::GrafanaAlerts,
            vec![sug(SourceKind::GrafanaAlerts, "dead", "dead").urgent(Urgency::Critical)],
            300,
        );
        assert!(
            !store.ranked(10, 300).iter().any(|s| s.id == dead_id),
            "gc under pressure must not launder a dismissal"
        );
        assert!(store.is_dismissed(dead_id), "dismissal restored from the tombstone");
    }

    #[test]
    fn decay_per_source_respects_each_sources_ttl() {
        let store = SuggestionStore::new();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 1000);
        store.ingest(SourceKind::GrafanaAlerts, vec![sug(SourceKind::GrafanaAlerts, "b", "b")], 1000);
        // At now=6000: tend (ttl 1000) is 5000ms stale → drop; grafana (ttl huge) kept.
        store.decay_per_source(6000, |k| match k {
            SourceKind::TendRepos => 1000,
            _ => 100_000,
        });
        assert!(
            store.get(SuggestionId::derive(SourceKind::TendRepos, "a")).is_none(),
            "the short-TTL source aged out"
        );
        assert!(
            store.get(SuggestionId::derive(SourceKind::GrafanaAlerts, "b")).is_some(),
            "the long-TTL source is kept (no flicker)"
        );
    }

    fn stored(source: SourceKind, key: &str) -> StoredSuggestion {
        StoredSuggestion {
            suggestion: sug(source, key, key),
            first_seen_ms: 100,
            last_seen_ms: 100,
            times_seen: 1,
            state: SuggestionState::Offered,
        }
    }

    #[test]
    fn balance_caps_per_source_keeping_rank_order() {
        // One Critical grafana alert + five Low tend repos.
        let mut items = vec![stored(SourceKind::GrafanaAlerts, "fire")];
        items[0].suggestion = items[0].suggestion.clone().urgent(Urgency::Critical);
        for key in ["a", "b", "c", "d", "e"] {
            items.push(stored(SourceKind::TendRepos, key));
        }
        // Rank first so balance sees urgency order (grafana on top).
        items.sort_by(|x, y| y.suggestion.rank_key().cmp(&x.suggestion.rank_key()));
        let out = balance_per_source(items, 10, 2);
        assert_eq!(out.len(), 3, "1 grafana + 2 (capped) tend");
        assert_eq!(out[0].suggestion.source, SourceKind::GrafanaAlerts, "critical kept on top");
        assert_eq!(
            out.iter().filter(|s| s.suggestion.source == SourceKind::TendRepos).count(),
            2,
            "tend capped at 2"
        );
    }

    proptest! {
        #[test]
        fn ranked_is_sorted_by_effective_key_desc(n in 0usize..20) {
            let store = SuggestionStore::new();
            let items: Vec<Suggestion> = (0..n)
                .map(|i| sug(SourceKind::TendRepos, &i.to_string(), "t").scored((u32::try_from(i).unwrap_or(0) * 37) % 1001))
                .collect();
            store.ingest(SourceKind::TendRepos, items, 100);
            let ranked = store.ranked_stored(100, 100);
            for w in ranked.windows(2) {
                prop_assert!(effective_rank_key(&w[0], 100) >= effective_rank_key(&w[1], 100));
            }
        }

        #[test]
        fn balance_never_exceeds_cap_or_max(total in 0usize..30, cap in 1usize..5, max in 0usize..15) {
            let items: Vec<StoredSuggestion> =
                (0..total).map(|i| stored(SourceKind::TendRepos, &i.to_string())).collect();
            let out = balance_per_source(items, max, cap);
            prop_assert!(out.len() <= max);
            prop_assert!(
                out.iter().filter(|s| s.suggestion.source == SourceKind::TendRepos).count() <= cap
            );
        }

        /// Aging + recurrence bonuses are BOUNDED: no combination of waiting
        /// time, score, and times_seen lets a row cross into the next urgency
        /// tier's key range.
        #[test]
        fn escalation_never_crosses_an_urgency_tier(
            waited_min in 0u64..1_000_000,
            score in 0u32..=1000,
            times in 1u32..10_000,
        ) {
            let now = waited_min.saturating_mul(60_000).saturating_add(1);
            let st = StoredSuggestion {
                suggestion: sug(SourceKind::TendRepos, "k", "t")
                    .urgent(Urgency::Normal)
                    .scored(score),
                first_seen_ms: 1,
                last_seen_ms: now,
                times_seen: times,
                state: SuggestionState::Offered,
            };
            let key = effective_rank_key(&st, now);
            let tier_floor = u64::from(Urgency::Normal.weight()) << 20;
            let next_tier = u64::from(Urgency::High.weight()) << 20;
            prop_assert!(key >= tier_floor, "never falls below its own tier");
            prop_assert!(key < next_tier, "never escalates across tiers");
        }

        /// DETERMINISTIC race coverage. Every store op takes the Mutex, so the
        /// store is LINEARIZABLE — concurrent interleavings reduce to some
        /// ordering of complete ops. Exhaustively exercising random ORDERINGS
        /// (proptest-seeded → deterministic, replayable) therefore soundly
        /// covers the map's concurrent behaviour. We assert the invariants the
        /// lock-free generation counter + the picker memoization depend on:
        /// generation is monotonic non-decreasing, and `ranked` is always sorted.
        #[test]
        fn store_is_linearizable_invariants_hold(
            seq in prop::collection::vec((0u8..6, 0u8..8), 0..40),
        ) {
            let store = SuggestionStore::new();
            let mut prev_gen = store.generation();
            let mut now = 1000u64;
            for (kind, arg) in seq {
                now += 10;
                match kind {
                    0 => {
                        let items: Vec<_> = (0..arg)
                            .map(|i| sug(SourceKind::TendRepos, &i.to_string(), "t"))
                            .collect();
                        store.ingest(SourceKind::TendRepos, items, now);
                    }
                    1 => {
                        let items: Vec<_> = (0..arg)
                            .map(|i| {
                                sug(SourceKind::GrafanaAlerts, &i.to_string(), "t")
                                    .urgent(Urgency::Critical)
                            })
                            .collect();
                        store.ingest(SourceKind::GrafanaAlerts, items, now);
                    }
                    2 => store.decay(now, u64::from(arg) * 5),
                    3 => store.gc(usize::from(arg), now),
                    4 => {
                        let _ = store.dismiss(SuggestionId::derive(
                            SourceKind::TendRepos,
                            &arg.to_string(),
                        ));
                    }
                    _ => {
                        let _ = store.mark_accepted(
                            SuggestionId::derive(SourceKind::GrafanaAlerts, &arg.to_string()),
                            "s",
                        );
                    }
                }
                let g = store.generation();
                prop_assert!(g >= prev_gen, "generation must be monotonic non-decreasing");
                prev_gen = g;
                let ranked = store.ranked_stored(1000, now);
                for w in ranked.windows(2) {
                    prop_assert!(
                        effective_rank_key(&w[0], now) >= effective_rank_key(&w[1], now),
                        "ranked must stay sorted after every op"
                    );
                }
            }
        }
    }

    /// REAL-concurrency smoke (complements the deterministic linearizability
    /// proptest): 8 threads hammer the one store. The Mutex makes a data race on
    /// the map unrepresentable; this catches what an ordering test can't — a
    /// DEADLOCK (lock-ordering / a lock held across a call → the test hangs and
    /// CI times out), a panic, or an atomic-ordering bug. Final invariants hold.
    #[test]
    fn concurrent_hammering_no_deadlock_no_panic_invariants_hold() {
        let store = std::sync::Arc::new(SuggestionStore::new());
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let s = std::sync::Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                // Two threads share each source so ingest's retain/insert races.
                let src = if t % 2 == 0 {
                    SourceKind::TendRepos
                } else {
                    SourceKind::GitBranchPr
                };
                for i in 0..300u64 {
                    let n = usize::try_from(i % 6).unwrap_or(0);
                    let items: Vec<_> = (0..n).map(|j| sug(src, &j.to_string(), "t")).collect();
                    s.ingest(src, items, 1000 + i);
                    let _ = s.generation();
                    let _ = s.ranked_stored(10, 1000 + i);
                    let _ = s.record_poll(src, SourceStatus::Ok, 1000 + i);
                    if i % 5 == 0 {
                        s.decay(1000 + i, 50);
                    }
                    if i % 7 == 0 {
                        let _ = s.dismiss(SuggestionId::derive(src, "1"));
                    }
                    if i % 9 == 0 {
                        s.gc(30, 1000 + i);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("a store thread panicked or deadlocked");
        }
        let ranked = store.ranked_stored(1000, 2000);
        for w in ranked.windows(2) {
            assert!(effective_rank_key(&w[0], 2000) >= effective_rank_key(&w[1], 2000));
        }
    }
}
