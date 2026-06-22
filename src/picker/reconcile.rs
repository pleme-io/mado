//! [`IndexReconciler`] — the "always tracking + curating" loop that keeps
//! the praça session index a faithful projection of the live session set.
//!
//! ## The gap this closes
//!
//! Pre-reconciler, the praça index was mutated ONLY on `cd`
//! ([`crate::auto_attach`]): a session spawned out-of-band — via the
//! `spawn_term` MCP tool, `tear new-session`, a manual attach — never
//! landed in the index, so it never appeared in the Ctrl-S picker. The
//! reconciler runs on the embedded event loop's idle tick and syncs the
//! ground truth ([`tear_core::InProcess`]'s registry) into praça: every
//! live session that isn't indexed gets a record; every indexed session
//! that died gets pruned. Auto-attach's richer, project-bound records are
//! left untouched (the reconciler only *adds* sessions it doesn't already
//! know).
//!
//! Time is injected (`now` unix-seconds), same discipline as the rest of
//! the praça path — the reconciler never reads the clock.

use std::sync::Arc;

use tear_types::SessionId;

/// Reconcile a curated index against an external ground truth.
/// Returns `true` if the index changed (so the caller can skip a redraw
/// on the common no-op tick).
pub trait IndexReconciler {
    /// Bring `praca` into agreement with the live world at `now`.
    fn reconcile(&self, praca: &mut praca::Praca, now: u64) -> bool;
}

/// Reconciles the praça index against the live `tear_core::InProcess`
/// session registry — the embedded runtime's ground truth.
///
/// * Every live session NOT already in the index is added (labelled by
///   its tear session name, so out-of-band-spawned sessions become
///   browsable + switchable in the Ctrl-S picker).
/// * Every indexed session that is no longer live is pruned (index +
///   binding), so the picker never lists a dead session.
///
/// Auto-attach remains the sole *authoritative* writer for project-bound
/// sessions; the reconciler only fills the gap for sessions it doesn't
/// recognise + reaps the dead. One shared `Arc<Mutex<praca::Praca>>`,
/// many writers, never a fork.
pub struct InProcessSessionReconciler {
    inproc: Arc<tear_core::InProcess>,
}

impl InProcessSessionReconciler {
    #[must_use]
    pub fn new(inproc: Arc<tear_core::InProcess>) -> Self {
        Self { inproc }
    }

    /// Snapshot the live `(id, name)` set from the registry.
    fn live_sessions(&self) -> Vec<(SessionId, String)> {
        self.inproc.with_registry(|r| {
            r.sessions
                .iter()
                .map(|(id, s)| (*id, s.name.clone()))
                .collect()
        })
    }
}

impl IndexReconciler for InProcessSessionReconciler {
    fn reconcile(&self, praca: &mut praca::Praca, now: u64) -> bool {
        let live = self.live_sessions();
        let live_ids: std::collections::HashSet<SessionId> = live.iter().map(|(id, _)| *id).collect();
        let style = praca.name_style;
        let mut changed = false;

        // Add live sessions praça doesn't yet know. Labelled by the tear
        // session name (custom_name), with a neutral root so the picker
        // label is just the name (no "name  basename" doubling).
        for (id, name) in &live {
            if praca.index.get(*id).is_none() {
                let mut rec =
                    praca::SessionRecord::for_project(*id, std::path::PathBuf::from("/"), style, now);
                rec.rename(name.clone());
                praca.index.upsert(rec);
                changed = true;
            }
        }

        // Prune indexed sessions that are no longer live (the registry is
        // ground truth — absence means reaped).
        let dead: Vec<SessionId> = praca
            .index
            .all()
            .iter()
            .filter(|r| !live_ids.contains(&r.id))
            .map(|r| r.id)
            .collect();
        for id in dead {
            praca.index.remove(id);
            praca.binding.remove_session(id);
            changed = true;
        }

        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_types::{MultiplexerControl, SessionSource, SpawnEnv};

    fn spawn(inproc: &Arc<tear_core::InProcess>, name: &str) -> SessionId {
        inproc
            .new_session_with_source_and_size(
                name,
                "/bin/sh",
                SessionSource::Named("test".into()),
                (80, 24),
            )
            .expect("spawn session")
    }

    #[test]
    fn reconcile_indexes_an_out_of_band_session() {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(SpawnEnv::none());
        // A session spawned with NO cd — auto-attach would never index it.
        let sid = spawn(&inproc, "scratch");

        let mut praca = praca::Praca::new();
        assert!(praca.index.get(sid).is_none(), "not indexed before reconcile");

        let rec = InProcessSessionReconciler::new(Arc::clone(&inproc));
        assert!(rec.reconcile(&mut praca, 1000), "first reconcile adds it");
        let indexed = praca.index.get(sid).expect("now indexed");
        assert_eq!(indexed.display_name(), "scratch", "labelled by tear name");

        // Idempotent — a second reconcile with no change reports false.
        assert!(!rec.reconcile(&mut praca, 1001), "no change → no-op");
    }

    #[test]
    fn reconcile_prunes_a_dead_session() {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(SpawnEnv::none());
        let rec = InProcessSessionReconciler::new(Arc::clone(&inproc));

        // Index a session id that is NOT in the live registry.
        let mut praca = praca::Praca::new();
        let ghost = SessionId::from_seed("ghost");
        praca.index.upsert(praca::SessionRecord::for_project(
            ghost,
            std::path::PathBuf::from("/code/gone"),
            ishou_tokens::SessionNameStyle::Emoji,
            1000,
        ));
        assert!(praca.index.get(ghost).is_some());

        assert!(rec.reconcile(&mut praca, 1000), "prunes the dead ghost");
        assert!(praca.index.get(ghost).is_none(), "ghost removed");
    }
}
