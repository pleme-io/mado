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

/// Reconciles the praça index against the live session set of whichever
/// tear backend the window holds — the embedded `InProcess` or a daemon
/// `Client` — through the one `MultiplexerControl` seam.
///
/// * Every live session NOT already in the index is added (labelled by
///   its tear session name, so out-of-band-spawned sessions become
///   browsable + switchable in the Ctrl-S picker).
/// * Every indexed session that is no longer live is pruned (index +
///   binding), so the picker never lists a dead session.
///
/// ★ A FAILED READ IS BLIND, NOT EMPTY. Over a daemon, `list_sessions` is an
/// RPC that can fail transiently; reading that failure as "zero sessions"
/// would prune every record from the index at once. A failed read changes
/// nothing and reports no change.
///
/// Auto-attach remains the sole *authoritative* writer for project-bound
/// sessions; the reconciler only fills the gap for sessions it doesn't
/// recognise + reaps the dead. One shared `Arc<Mutex<praca::Praca>>`,
/// many writers, never a fork.
pub struct ControlSessionReconciler {
    control: Arc<dyn tear_types::MultiplexerControl>,
}

impl ControlSessionReconciler {
    #[must_use]
    pub fn new(control: Arc<dyn tear_types::MultiplexerControl>) -> Self {
        Self { control }
    }

    /// Snapshot the live `(id, name)` set, or `None` when the backend could
    /// not be read (blind — see the type's doc).
    fn live_sessions(&self) -> Option<Vec<(SessionId, String)>> {
        match self.control.list_sessions() {
            Ok(sessions) => Some(sessions.into_iter().map(|s| (s.id, s.name)).collect()),
            Err(e) => {
                tracing::debug!(error = %e, "session reconcile: backend unreadable; index left as-is");
                None
            }
        }
    }
}

impl IndexReconciler for ControlSessionReconciler {
    fn reconcile(&self, praca: &mut praca::Praca, now: u64) -> bool {
        let Some(live) = self.live_sessions() else {
            return false;
        };
        let live_ids: std::collections::HashSet<SessionId> =
            live.iter().map(|(id, _)| *id).collect();
        let style = praca.name_style;
        let mut changed = false;

        // Add live sessions praça doesn't yet know. Labelled by the tear
        // session name (custom_name), with a neutral root so the picker
        // label is just the name (no "name  basename" doubling).
        for (id, name) in &live {
            if praca.index.get(*id).is_none() {
                let mut rec = praca::SessionRecord::for_project(
                    *id,
                    std::path::PathBuf::from("/"),
                    style,
                    now,
                );
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
                &[],
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
        assert!(
            praca.index.get(sid).is_none(),
            "not indexed before reconcile"
        );

        let rec = ControlSessionReconciler::new(Arc::clone(&inproc) as Arc<dyn MultiplexerControl>);
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
        let rec = ControlSessionReconciler::new(Arc::clone(&inproc) as Arc<dyn MultiplexerControl>);

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

    /// A backend that answers every call with a transport error — a daemon
    /// that has gone away mid-session.
    struct Unreachable;

    macro_rules! unreachable_backend {
        ($( fn $name:ident(&self $(, $arg:ident : $ty:ty)* ) -> $ret:ty; )*) => {
            impl MultiplexerControl for Unreachable {
                $( fn $name(&self $(, $arg: $ty)*) -> $ret {
                    $( let _ = $arg; )*
                    Err(tear_types::ControlError::Transport("daemon unreachable".into()))
                } )*
            }
        };
    }

    unreachable_backend! {
        fn list_sessions(&self) -> tear_types::ControlResult<Vec<tear_types::TearSession>>;
        fn get_session(&self, id: SessionId) -> tear_types::ControlResult<tear_types::TearSession>;
        fn get_window(&self, id: tear_types::WindowId) -> tear_types::ControlResult<(SessionId, tear_types::TearWindow)>;
        fn get_pane(&self, id: tear_types::PaneId) -> tear_types::ControlResult<tear_types::TearPane>;
        fn new_session_with_source_and_size(&self, name: &str, shell: &str, args: &[String], source: SessionSource, size: (u16, u16)) -> tear_types::ControlResult<SessionId>;
        fn rename_session(&self, id: SessionId, new_name: &str) -> tear_types::ControlResult<()>;
        fn kill_session(&self, id: SessionId) -> tear_types::ControlResult<()>;
        fn new_window(&self, session: SessionId, name: &str, shell: &str, args: &[String]) -> tear_types::ControlResult<tear_types::WindowId>;
        fn kill_window(&self, id: tear_types::WindowId) -> tear_types::ControlResult<()>;
        fn select_window(&self, id: tear_types::WindowId) -> tear_types::ControlResult<()>;
        fn split_pane(&self, origin: tear_types::PaneId, direction: tear_types::Direction, shell: &str, args: &[String]) -> tear_types::ControlResult<tear_types::PaneId>;
        fn kill_pane(&self, id: tear_types::PaneId) -> tear_types::ControlResult<()>;
        fn select_pane(&self, id: tear_types::PaneId) -> tear_types::ControlResult<()>;
        fn resize_pane(&self, id: tear_types::PaneId, direction: tear_types::Direction, delta: i16) -> tear_types::ControlResult<()>;
        fn apply_layout(&self, window: tear_types::WindowId, kind: tear_types::LayoutKind) -> tear_types::ControlResult<()>;
        fn send_keys(&self, id: tear_types::PaneId, bytes: &[u8]) -> tear_types::ControlResult<()>;
        fn pane_subscriber_count(&self, id: tear_types::PaneId) -> tear_types::ControlResult<u32>;
        fn set_input_policy(&self, id: tear_types::PaneId, policy: tear_types::InputPolicy) -> tear_types::ControlResult<()>;
    }

    /// ★ A daemon that cannot be read must not look like a daemon with no
    /// sessions. Red-run: make `live_sessions` map `Err` to an empty list and
    /// this prunes the record.
    #[test]
    fn an_unreadable_backend_prunes_nothing() {
        let mut praca = praca::Praca::new();
        let known = SessionId::from_seed("still-running-elsewhere");
        praca.index.upsert(praca::SessionRecord::for_project(
            known,
            std::path::PathBuf::from("/code/work"),
            ishou_tokens::SessionNameStyle::Emoji,
            1000,
        ));
        let rec = ControlSessionReconciler::new(Arc::new(Unreachable));
        assert!(
            !rec.reconcile(&mut praca, 1001),
            "a blind read reports no change"
        );
        assert!(
            praca.index.get(known).is_some(),
            "a blind read pruned a live session"
        );
    }
}
