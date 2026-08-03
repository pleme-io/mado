//! The curated structure — a long-term, in-memory, deduped, recurrence-counted,
//! decaying set of one tracked data-type's items, converged efficiently to
//! upstream in batch (never a per-item round-trip; never a full rebuild after
//! the first seed). See `docs/SAFRA.md` §4–§5.

use std::collections::HashMap;

/// Severity of a curated item — drives ranking + the Ctrl-S accent.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Severity {
    /// Informational — surfaced last.
    Info,
    /// Warning — degraded but not failing.
    Warning,
    /// Critical — failing / paging.
    Critical,
}

impl Severity {
    /// Rank weight (higher = surfaced first).
    #[must_use]
    pub fn weight(self) -> u32 {
        match self {
            Severity::Info => 1,
            Severity::Warning => 4,
            Severity::Critical => 16,
        }
    }
}

/// One item the safra curates. The `signature` is the STABLE identity used for
/// dedup + recurrence (two observations with the same signature are the same
/// incident); it is internal identity, not emitted output, so it is built with
/// `push_str` (no `format!`, per TYPED EMISSION).
pub trait CuratedItem: Clone {
    /// Stable identity across observations (e.g. `alert:OOMKilled:api-gateway`).
    fn signature(&self) -> String;
    /// Severity for ranking.
    fn severity(&self) -> Severity;
    /// Human label for the Ctrl-S row (e.g. `OOMKilled api-gateway`).
    fn title(&self) -> String;
}

/// A curated item plus its long-term provenance — when it was first seen, last
/// seen, and how many times it has recurred (the anomaly-recurrence surface).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tracked<I> {
    pub item: I,
    /// Times this signature has been observed (>= 1).
    pub recurrence: u32,
    /// First observation (unix seconds).
    pub first_seen: u64,
    /// Most recent observation (unix seconds) — drives decay + recency rank.
    pub last_seen: u64,
}

/// The result of one batch convergence — what changed, so the caller can decide
/// cheaply whether anything is worth re-projecting into Ctrl-S.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
    /// New signatures inserted this batch.
    pub added: usize,
    /// Existing signatures re-observed (recurrence bumped).
    pub updated: usize,
    /// Signatures decayed out (not seen within the TTL).
    pub expired: usize,
}

impl Delta {
    /// True if the curated set's membership or counts changed at all.
    #[must_use]
    pub fn is_change(self) -> bool {
        self.added > 0 || self.updated > 0 || self.expired > 0
    }
}

/// The long-term curated set for one (tracked-data-kind × tracked-environment)
/// cell. Keyed by signature; converges in batch; decays by TTL.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CuratedSet<I> {
    items: HashMap<String, Tracked<I>>,
    /// Drop an item not re-observed within this many seconds. `0` = never decay.
    ttl_secs: u64,
}

impl<I: CuratedItem> CuratedSet<I> {
    #[must_use]
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            items: HashMap::new(),
            ttl_secs,
        }
    }

    /// Converge the curated set to a freshly-observed BATCH of upstream items at
    /// `now`. Efficient + idempotent: O(observed + set). New signatures are
    /// inserted; re-observed signatures bump `recurrence` + `last_seen`; items
    /// absent past their TTL decay out. Returns the typed [`Delta`].
    ///
    /// This is the single mutation path — there is no per-item fetch, and after
    /// the first seed it only applies the delta, never a full rebuild.
    pub fn converge(&mut self, observed: Vec<I>, now: u64) -> Delta {
        let mut delta = Delta::default();
        for item in observed {
            let sig = item.signature();
            match self.items.get_mut(&sig) {
                Some(tracked) => {
                    // Re-observation of a known incident — bump recurrence,
                    // refresh recency, keep the original first_seen.
                    tracked.recurrence = tracked.recurrence.saturating_add(1);
                    tracked.last_seen = now;
                    tracked.item = item;
                    delta.updated += 1;
                }
                None => {
                    self.items.insert(
                        sig,
                        Tracked {
                            item,
                            recurrence: 1,
                            first_seen: now,
                            last_seen: now,
                        },
                    );
                    delta.added += 1;
                }
            }
        }
        delta.expired = self.decay(now);
        delta
    }

    /// Drop items not re-observed within the TTL. Returns how many expired.
    /// `ttl_secs == 0` disables decay (nothing ever expires by age).
    pub fn decay(&mut self, now: u64) -> usize {
        if self.ttl_secs == 0 {
            return 0;
        }
        let ttl = self.ttl_secs;
        let before = self.items.len();
        self.items
            .retain(|_, t| now.saturating_sub(t.last_seen) <= ttl);
        before - self.items.len()
    }

    /// Enforce the per-cell memory cap — the "keep them under control"
    /// invariant. Keeps at most `max_items` highest-ranked items, evicting the
    /// lowest-ranked beyond it (which by construction are never on the bounded
    /// visible board). `0` = unbounded. Returns how many were evicted.
    pub fn cap(&mut self, max_items: usize) -> usize {
        if max_items == 0 || self.items.len() <= max_items {
            return 0;
        }
        // Signatures to KEEP = the top `max_items` ranked (the map key IS the
        // signature). Collect owned keys first so the immutable `ranked` borrow
        // is released before the mutable `retain`.
        let keep: std::collections::HashSet<String> = self
            .ranked()
            .into_iter()
            .take(max_items)
            .map(|t| t.item.signature())
            .collect();
        let before = self.items.len();
        self.items.retain(|sig, _| keep.contains(sig));
        before - self.items.len()
    }

    /// Items ranked for surfacing: severity weight × recurrence, tie-broken by
    /// recency (most-recent first). Highest-priority first.
    #[must_use]
    pub fn ranked(&self) -> Vec<&Tracked<I>> {
        let mut v: Vec<&Tracked<I>> = self.items.values().collect();
        v.sort_by(|a, b| {
            let sa = a.item.severity().weight().saturating_mul(a.recurrence);
            let sb = b.item.severity().weight().saturating_mul(b.recurrence);
            sb.cmp(&sa).then(b.last_seen.cmp(&a.last_seen))
        });
        v
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Alert {
        name: &'static str,
        sev: Severity,
    }
    impl CuratedItem for Alert {
        fn signature(&self) -> String {
            let mut s = String::from("alert:");
            s.push_str(self.name);
            s
        }
        fn severity(&self) -> Severity {
            self.sev
        }
        fn title(&self) -> String {
            self.name.to_string()
        }
    }

    fn a(name: &'static str, sev: Severity) -> Alert {
        Alert { name, sev }
    }

    #[test]
    fn first_batch_all_added() {
        let mut set = CuratedSet::new(300);
        let d = set.converge(
            vec![a("x", Severity::Warning), a("y", Severity::Critical)],
            1000,
        );
        assert_eq!(
            d,
            Delta {
                added: 2,
                updated: 0,
                expired: 0
            }
        );
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn reobservation_bumps_recurrence_not_count() {
        let mut set = CuratedSet::new(300);
        set.converge(vec![a("x", Severity::Warning)], 1000);
        let d = set.converge(vec![a("x", Severity::Warning)], 1060);
        assert_eq!(
            d,
            Delta {
                added: 0,
                updated: 1,
                expired: 0
            }
        );
        assert_eq!(set.len(), 1, "same signature is the same incident");
        assert_eq!(set.ranked()[0].recurrence, 2);
        assert_eq!(set.ranked()[0].first_seen, 1000, "first_seen preserved");
        assert_eq!(set.ranked()[0].last_seen, 1060, "last_seen refreshed");
    }

    #[test]
    fn absent_item_decays_after_ttl() {
        let mut set = CuratedSet::new(300);
        set.converge(vec![a("x", Severity::Warning)], 1000);
        // A later batch that no longer contains "x" — past the 300s TTL it decays.
        let d = set.converge(vec![a("y", Severity::Info)], 1000 + 301);
        assert_eq!(d.added, 1);
        assert_eq!(d.expired, 1, "x not re-observed within TTL → expired");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn ttl_zero_never_decays() {
        let mut set = CuratedSet::new(0);
        set.converge(vec![a("x", Severity::Warning)], 1000);
        let d = set.converge(vec![a("y", Severity::Info)], 1_000_000);
        assert_eq!(d.expired, 0, "ttl=0 disables decay");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn ranked_by_severity_times_recurrence_then_recency() {
        let mut set = CuratedSet::new(0);
        // critical×1 (weight 16) vs warning×5 (weight 4×5=20) → warning ranks first.
        // 5 observations = 1 insert + 4 recurrence bumps → recurrence 5.
        set.converge(vec![a("crit", Severity::Critical)], 1000);
        for t in 0..5 {
            set.converge(vec![a("warn", Severity::Warning)], 1000 + t);
        }
        let ranked = set.ranked();
        assert_eq!(
            ranked[0].item.title(),
            "warn",
            "5×warning (score 20) out-ranks 1×critical (16)"
        );
        assert_eq!(ranked[0].recurrence, 5);
    }

    #[test]
    fn converge_is_idempotent_on_steady_state() {
        // The efficiency contract: an unchanged upstream produces only `updated`
        // (recurrence bumps), never churn — so the caller can skip re-projection.
        let mut set = CuratedSet::new(0);
        set.converge(
            vec![a("x", Severity::Warning), a("y", Severity::Info)],
            1000,
        );
        let d = set.converge(
            vec![a("x", Severity::Warning), a("y", Severity::Info)],
            1060,
        );
        assert_eq!(d.added, 0);
        assert_eq!(d.expired, 0);
        assert_eq!(d.updated, 2);
    }

    #[test]
    fn cap_keeps_top_ranked_under_control() {
        let mut set = CuratedSet::new(0);
        // 1 critical (score 16) + 3 info (score 1 each).
        set.converge(vec![a("crit", Severity::Critical)], 1000);
        set.converge(
            vec![
                a("i1", Severity::Info),
                a("i2", Severity::Info),
                a("i3", Severity::Info),
            ],
            1000,
        );
        assert_eq!(set.len(), 4);
        let evicted = set.cap(2);
        assert_eq!(evicted, 2, "two lowest-ranked evicted to honor cap=2");
        assert_eq!(set.len(), 2);
        assert_eq!(
            set.ranked()[0].item.title(),
            "crit",
            "the critical is always kept"
        );
    }

    #[test]
    fn cap_zero_is_unbounded_and_noop_under_cap() {
        let mut set = CuratedSet::new(0);
        set.converge(
            vec![a("x", Severity::Warning), a("y", Severity::Info)],
            1000,
        );
        assert_eq!(set.cap(0), 0, "cap=0 is unbounded");
        assert_eq!(set.cap(5), 0, "len under cap → no eviction");
        assert_eq!(set.len(), 2);
    }
}
