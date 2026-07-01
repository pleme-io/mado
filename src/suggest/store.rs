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

use tokio::sync::watch;

use super::core::{SourceKind, Suggestion, SuggestionId};
use crate::livestream::Reactive;

/// Snapshot framing magic — a schema tag so a future format bump is rejected
/// (start-empty) rather than silently mis-parsed.
const SNAPSHOT_MAGIC: &[u8] = b"mado-suggest v1\n";

/// A suggestion plus the store-side bookkeeping the renderer needs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredSuggestion {
    pub suggestion: Suggestion,
    /// When this id first appeared (ms) — the shade-in ramp anchor.
    #[serde(default)]
    pub first_seen_ms: u64,
    /// When this id was last confirmed present (ms) — decay/freshness.
    #[serde(default)]
    pub last_seen_ms: u64,
}

/// Thread-safe ranked suggestion cache. Cloneable handle pattern is not used
/// here (callers hold an `Arc<SuggestionStore>`); the inner map is mutexed.
///
/// A monotonic `generation` is bumped only on a *meaningful* change (an id
/// added/removed, or a row's displayed/ranked fields changed — NOT a mere
/// `last_seen_ms` heartbeat). The picker memoizes its ranked read by it (O(1)
/// reads while the watchers idle) and the persist task uses it as the dirty
/// signal — one counter, the shikumi swap-then-observe contract.
///
/// The generation + the change-broadcast are the shared
/// [`Reactive`](crate::livestream::Reactive) core (stage 2 of the live-stream
/// substrate): every meaningful mutation bumps AND notifies every
/// [`subscribe`](SuggestionStore::subscribe)r, so the Ctrl-S board (and any
/// other surface) re-renders on the fact of a change instead of a fixed timer.
#[derive(Default)]
pub struct SuggestionStore {
    inner: Mutex<BTreeMap<SuggestionId, StoredSuggestion>>,
    reactive: Reactive,
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

    /// Replace the suggestion set contributed by `source`. Ids that persist
    /// keep their `first_seen_ms`; ids of this source that vanished are
    /// dropped; entries from other sources are untouched.
    pub fn ingest(&self, source: SourceKind, items: Vec<Suggestion>, now_ms: u64) {
        let mut changed = false;
        {
            let mut g = self.lock();
            let incoming: BTreeSet<SuggestionId> = items.iter().map(|s| s.id).collect();
            // Drop this source's vanished ids (keep all other sources').
            let before = g.len();
            g.retain(|id, st| st.suggestion.source != source || incoming.contains(id));
            if g.len() != before {
                changed = true; // an id of this source vanished
            }
            for s in items {
                match g.get_mut(&s.id) {
                    Some(existing) => {
                        // Bump only on a DISPLAYED/RANKED change, not a mere
                        // last_seen heartbeat — else every 30s poll would force a
                        // re-render + a disk write for an unchanged row.
                        if existing.suggestion != s {
                            changed = true;
                        }
                        existing.last_seen_ms = now_ms;
                        existing.suggestion = s;
                    }
                    None => {
                        changed = true;
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
        if changed {
            self.bump();
        }
    }

    /// Drop every entry whose `last_seen_ms` is older than `ttl_ms` — the
    /// decay pass (a source that stops reporting an item lets it age out even
    /// if the source itself never re-polls).
    pub fn decay(&self, now_ms: u64, ttl_ms: u64) {
        let removed = {
            let mut g = self.lock();
            let before = g.len();
            g.retain(|_, st| now_ms.saturating_sub(st.last_seen_ms) <= ttl_ms);
            g.len() != before
        };
        if removed {
            self.bump();
        }
    }

    /// Per-source decay: each entry ages out against `ttl_for(its source)`, so a
    /// slow source (e.g. a 1h poll) doesn't flicker under a fast global TTL. A
    /// `ttl_for` of 0 means that source's entries never age out.
    pub fn decay_per_source(&self, now_ms: u64, ttl_for: impl Fn(SourceKind) -> u64) {
        let removed = {
            let mut g = self.lock();
            let before = g.len();
            g.retain(|_, st| {
                let ttl = ttl_for(st.suggestion.source);
                ttl == 0 || now_ms.saturating_sub(st.last_seen_ms) <= ttl
            });
            g.len() != before
        };
        if removed {
            self.bump();
        }
    }

    /// Hard memory cap: if the store exceeds `max_entries`, evict the
    /// lowest-ranked / stalest until it fits (urgency→score→freshness order, the
    /// same axis the picker ranks by). `max_entries == 0` is unbounded. Insurance
    /// against a source that stops polling with no TTL.
    pub fn gc(&self, max_entries: usize) {
        if max_entries == 0 {
            return;
        }
        let removed = {
            let mut g = self.lock();
            if g.len() <= max_entries {
                return;
            }
            let mut ranked: Vec<(SuggestionId, u64, u64)> = g
                .values()
                .map(|st| (st.suggestion.id, st.suggestion.rank_key(), st.last_seen_ms))
                .collect();
            // Keep the top `max_entries`: rank desc, then fresher, then id.
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
            let keep: BTreeSet<SuggestionId> =
                ranked.into_iter().take(max_entries).map(|(id, _, _)| id).collect();
            let before = g.len();
            g.retain(|id, _| keep.contains(id));
            g.len() != before
        };
        if removed {
            self.bump();
        }
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
        let had = {
            let mut g = self.lock();
            let had = !g.is_empty();
            g.clear();
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
        {
            let mut g = self.lock();
            g.clear();
            for st in snap.entries {
                g.insert(st.suggestion.id, st);
            }
        }
        self.bump();
    }

    /// Warm-restart load: a magic-framed, BLAKE3-verified snapshot file. A
    /// missing file, a wrong schema magic (format bump), or a torn body (hash
    /// mismatch) all start empty — never feed garbage rows to the picker.
    pub fn load_file(&self, path: &Path) {
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
            self.load_snapshot(snap);
        }
    }

    /// Crash-safe atomic persist: serialize → BLAKE3-frame → write a pid-tagged
    /// temp (after `create_dir_all`) → `sync_all` → rename. Snapshotting clones
    /// under the lock then drops it, so the disk I/O is lock-free. Best-effort —
    /// a write failure never blocks the caller.
    pub fn persist_file(&self, path: &Path) {
        let snap = self.to_snapshot();
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

    #[test]
    fn framed_persist_load_round_trips_and_rejects_corruption() {
        let dir = std::env::temp_dir().join("mado-suggest-test-persist");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("snap.json");
        let _ = std::fs::remove_file(&path);

        let store = SuggestionStore::new();
        store.ingest(SourceKind::TendRepos, vec![sug(SourceKind::TendRepos, "a", "a")], 100);
        store.persist_file(&path);

        // Round-trip: a fresh store warm-loads the set.
        let loaded = SuggestionStore::new();
        loaded.load_file(&path);
        assert_eq!(loaded.len(), 1, "warm restart re-surfaces the set");

        // A torn body (flip a json byte) → hash mismatch → start empty.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let torn = SuggestionStore::new();
        torn.load_file(&path);
        assert_eq!(torn.len(), 0, "a torn body is rejected → empty");

        // Wrong magic (a foreign/old file) → start empty.
        std::fs::write(&path, b"garbage not ours").unwrap();
        let bad = SuggestionStore::new();
        bad.load_file(&path);
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

        store.gc(3);
        assert_eq!(store.len(), 3, "capped to 3");
        assert!(
            store.get(SuggestionId::derive(SourceKind::GrafanaAlerts, "fire")).is_some(),
            "the Critical row is kept (highest rank)"
        );

        store.gc(0); // unbounded → no-op
        assert_eq!(store.len(), 3);
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
        fn ranked_is_sorted_by_rank_key_desc(n in 0usize..20) {
            let store = SuggestionStore::new();
            let items: Vec<Suggestion> = (0..n)
                .map(|i| sug(SourceKind::TendRepos, &i.to_string(), "t").scored((u32::try_from(i).unwrap_or(0) * 37) % 1001))
                .collect();
            store.ingest(SourceKind::TendRepos, items, 100);
            let ranked = store.ranked(100);
            for w in ranked.windows(2) {
                prop_assert!(w[0].rank_key() >= w[1].rank_key());
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

        /// DETERMINISTIC race coverage. Every store op takes the Mutex, so the
        /// store is LINEARIZABLE — concurrent interleavings reduce to some
        /// ordering of complete ops. Exhaustively exercising random ORDERINGS
        /// (proptest-seeded → deterministic, replayable) therefore soundly
        /// covers the map's concurrent behaviour. We assert the invariants the
        /// lock-free generation counter + the picker memoization depend on:
        /// generation is monotonic non-decreasing, and `ranked` is always sorted.
        #[test]
        fn store_is_linearizable_invariants_hold(
            seq in prop::collection::vec((0u8..4, 0u8..8), 0..40),
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
                    _ => store.gc(usize::from(arg)),
                }
                let g = store.generation();
                prop_assert!(g >= prev_gen, "generation must be monotonic non-decreasing");
                prev_gen = g;
                let ranked = store.ranked_stored(1000);
                for w in ranked.windows(2) {
                    prop_assert!(
                        w[0].suggestion.rank_key() >= w[1].suggestion.rank_key(),
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
                    let _ = s.ranked_stored(10);
                    if i % 5 == 0 {
                        s.decay(1000 + i, 50);
                    }
                    if i % 9 == 0 {
                        s.gc(30);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("a store thread panicked or deadlocked");
        }
        let ranked = store.ranked_stored(1000);
        for w in ranked.windows(2) {
            assert!(w[0].suggestion.rank_key() >= w[1].suggestion.rank_key());
        }
    }
}
