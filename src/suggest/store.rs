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
//! A best-effort JSON snapshot (M4) lets a warm restart re-surface the last
//! known set instantly while the watchers re-poll; a torn/absent snapshot
//! simply starts empty.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use super::core::{SourceKind, Suggestion, SuggestionId};

/// A suggestion plus the store-side bookkeeping the renderer needs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredSuggestion {
    pub suggestion: Suggestion,
    /// When this id first appeared (ms) — the shade-in ramp anchor.
    pub first_seen_ms: u64,
    /// When this id was last confirmed present (ms) — decay/freshness.
    pub last_seen_ms: u64,
}

/// Thread-safe ranked suggestion cache. Cloneable handle pattern is not used
/// here (callers hold an `Arc<SuggestionStore>`); the inner map is mutexed.
#[derive(Default)]
pub struct SuggestionStore {
    inner: Mutex<BTreeMap<SuggestionId, StoredSuggestion>>,
}

/// Serializable view for the warm-restart snapshot.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct StoreSnapshot {
    pub entries: Vec<StoredSuggestion>,
}

impl SuggestionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SuggestionId, StoredSuggestion>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace the suggestion set contributed by `source`. Ids that persist
    /// keep their `first_seen_ms`; ids of this source that vanished are
    /// dropped; entries from other sources are untouched.
    pub fn ingest(&self, source: SourceKind, items: Vec<Suggestion>, now_ms: u64) {
        let mut g = self.lock();
        let incoming: BTreeSet<SuggestionId> = items.iter().map(|s| s.id).collect();
        // Drop this source's vanished ids (keep all other sources').
        g.retain(|id, st| st.suggestion.source != source || incoming.contains(id));
        for s in items {
            match g.get_mut(&s.id) {
                Some(existing) => {
                    existing.last_seen_ms = now_ms;
                    existing.suggestion = s; // refresh title/urgency/score
                }
                None => {
                    g.insert(
                        s.id,
                        StoredSuggestion {
                            first_seen_ms: now_ms,
                            last_seen_ms: now_ms,
                            suggestion: s,
                        },
                    );
                }
            }
        }
    }

    /// Drop every entry whose `last_seen_ms` is older than `ttl_ms` — the
    /// decay pass (a source that stops reporting an item lets it age out even
    /// if the source itself never re-polls).
    pub fn decay(&self, now_ms: u64, ttl_ms: u64) {
        let mut g = self.lock();
        g.retain(|_, st| now_ms.saturating_sub(st.last_seen_ms) <= ttl_ms);
    }

    /// The top `max` suggestions, ranked by urgency→score→age→id.
    #[must_use]
    pub fn ranked(&self, max: usize) -> Vec<Suggestion> {
        self.ranked_stored(max)
            .into_iter()
            .map(|st| st.suggestion)
            .collect()
    }

    /// Like [`SuggestionStore::ranked`] but carries `first_seen_ms` so the
    /// renderer can compute the per-row shade-in alpha.
    #[must_use]
    pub fn ranked_stored(&self, max: usize) -> Vec<StoredSuggestion> {
        let g = self.lock();
        let mut v: Vec<StoredSuggestion> = g.values().cloned().collect();
        v.sort_by(|a, b| {
            b.suggestion
                .rank_key()
                .cmp(&a.suggestion.rank_key())
                .then(a.first_seen_ms.cmp(&b.first_seen_ms))
                .then(a.suggestion.id.cmp(&b.suggestion.id))
        });
        v.truncate(max);
        v
    }

    /// Resolve a suggestion by id (the accept path looks up the spawn target).
    #[must_use]
    pub fn get(&self, id: SuggestionId) -> Option<Suggestion> {
        self.lock().get(&id).map(|st| st.suggestion.clone())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Alpha (0..=255) for an id's shade-in ramp: 0 at birth, 255 after
    /// `shade_in_ms`. Used by the renderer to gently fade new rows in.
    #[must_use]
    pub fn shade_alpha(&self, id: SuggestionId, now_ms: u64, shade_in_ms: u64) -> u8 {
        let first = match self.lock().get(&id) {
            Some(st) => st.first_seen_ms,
            None => return 255,
        };
        shade_ramp(first, now_ms, shade_in_ms)
    }

    /// Snapshot the current set for a warm restart.
    #[must_use]
    pub fn to_snapshot(&self) -> StoreSnapshot {
        StoreSnapshot {
            entries: self.lock().values().cloned().collect(),
        }
    }

    /// Replace the store from a snapshot (warm restart). Birth times are kept
    /// so a row that survived the restart doesn't re-shade-in.
    pub fn load_snapshot(&self, snap: StoreSnapshot) {
        let mut g = self.lock();
        g.clear();
        for st in snap.entries {
            g.insert(st.suggestion.id, st);
        }
    }

    /// Best-effort load from a JSON file — a torn/absent file starts empty.
    pub fn load_file(&self, path: &Path) {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(snap) = serde_json::from_slice::<StoreSnapshot>(&bytes) {
                self.load_snapshot(snap);
            }
        }
    }

    /// Best-effort atomic persist (temp → rename) — never blocks the caller on
    /// a write failure.
    pub fn persist_file(&self, path: &Path) {
        let snap = self.to_snapshot();
        let Ok(bytes) = serde_json::to_vec(&snap) else {
            return;
        };
        let mut tmp = path.to_path_buf();
        let mut ext = tmp.extension().map(|e| e.to_os_string()).unwrap_or_default();
        ext.push(".tmp");
        tmp.set_extension(ext);
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
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
        let st = store.ranked_stored(10);
        let first = st.iter().find(|s| s.suggestion.id == id).unwrap().first_seen_ms;
        assert_eq!(first, 100, "first_seen preserved across re-ingest");
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
        let ranked = store.ranked(10);
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
    fn snapshot_round_trips_and_preserves_birth() {
        let store = SuggestionStore::new();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        let snap = store.to_snapshot();
        let restored = SuggestionStore::new();
        restored.load_snapshot(snap);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.ranked_stored(1)[0].first_seen_ms, 100);
    }
}
