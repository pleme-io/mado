//! Auto-attach-on-cd — the headline praça automation.
//!
//! When the **displayed** session's shell `cd`s into a *different*
//! project than the one it is currently seated at, mado auto-switches
//! its pane to that project's session — spawning + naming + binding a
//! fresh session when the project has none yet. Built on the
//! switchable-pane enabler ([`crate::session_switch::SwitchRequests`] +
//! the switchable attach in [`crate::gui_tear_attach`]): the action this
//! driver decides is realized by posting a [`tear_types::PaneId`] into
//! the same switch channel the `switch_session` MCP tool drives.
//!
//! ## Where the typed surface lives
//!
//! * [`praca::Praca`] is the decision engine — it owns the
//!   [`praca::SessionIndex`] (every live session as a
//!   [`praca::SessionRecord`]), the [`praca::ProjectBinding`] map (the
//!   project→session memory), and the [`praca::AttachPolicy`]. praca
//!   stays **time-injected**: mado supplies `now` (unix-seconds) at
//!   every call site here.
//! * praca decides over [`tear_types::SessionId`]; the switch channel
//!   speaks [`tear_types::PaneId`]. This driver owns the translation
//!   (an embedded session is one window with one `active_pane`), so the
//!   rest of the loop never sees the mismatch.
//!
//! ## Gating
//!
//! The driver is constructed ONLY when `tear.auto_attach != Off` AND
//! `tear.session_switching == true` (the latter because auto-attach
//! drives the switch channel — without a drainer the post is a no-op).
//! When `auto_attach != Off` but `session_switching == false`, the
//! caller logs a one-time warning and constructs no driver: the whole
//! path is then byte-identical to today's behaviour.
//!
//! ## No-thrash invariant
//!
//! A switch changes the displayed pane to one whose project root IS the
//! `cd` destination. The very next cwd observation must therefore
//! resolve to [`praca::AttachDecision::Stay`] — never a second switch.
//! This is guaranteed structurally: [`AutoAttachDriver::current_session`]
//! is updated to the just-attached session BEFORE the next tick reads
//! the cwd, so `praca::on_cwd_change` sees `current.project_root == root`
//! and stays. `tests/auto_attach_cd.rs` pins the property end-to-end.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

use crate::session_switch::SwitchRequests;

/// Unix-seconds now — the single clock-read for the whole auto-attach
/// path. praca is time-injected; this is mado's injection point.
#[must_use]
pub fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The typed outcome of observing a displayed-pane cwd change — what
/// the driver decided to do, surfaced for logging + tests. Distinct
/// from [`praca::AttachDecision`] (which is keyed by `SessionId` and is
/// policy-shaped) — this is the *realized* action after translation +
/// I/O, keyed by the live `PaneId` actually posted to the switch
/// channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoAttachOutcome {
    /// cwd is still inside the current session's project (or the path
    /// did not actually change) — nothing posted.
    Stay,
    /// Switched to an existing session bound to the destination project.
    /// `pane` was posted to the switch channel.
    Switched { session: SessionId, pane: PaneId },
    /// Spawned a fresh auto-named session for a brand-new project, bound
    /// it, indexed it, and posted its first pane to the switch channel.
    Spawned {
        session: SessionId,
        pane: PaneId,
        root: PathBuf,
        name: String,
    },
    /// Policy is `Suggest`: a switch/spawn WOULD fire, but the driver
    /// only surfaces it (no pane moved). Carries a human phrase.
    Suggested { hint: String },
    /// The decision needed a session/pane that could not be resolved or
    /// created (e.g. the bound session vanished from the live registry,
    /// or a spawn failed). Nothing posted; carries a typed reason.
    Skipped { reason: AutoAttachSkip },
}

/// Why an auto-attach decision did not produce a pane move. Each arm is
/// a distinct, testable cause rather than a bare bool — drift in the
/// SessionId↔PaneId translation or a spawn failure surfaces as a named
/// reason in logs instead of a silent no-op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoAttachSkip {
    /// A `SwitchTo(id)` decision but the live registry has no pane for
    /// that session (it was reaped between index + decision).
    BoundSessionGone(SessionId),
    /// `new_session_with_source_and_size` returned an error.
    SpawnFailed(String),
    /// A freshly-spawned session reported no first pane.
    SpawnedSessionHadNoPane(SessionId),
}

/// The driver mado threads through the switchable event loop when
/// auto-attach is active. Holds the praça decision engine, the live
/// control plane (the GUI's own `InProcess`), the switch channel, and
/// the bookkeeping the no-thrash invariant rests on (`current_session`
/// + `last_cwd`).
///
/// Embedded-runtime only — the switch targets a pane in the GUI's own
/// `tear_core::InProcess`, mirroring the [`crate::gui_tear_attach`]
/// `SwitchDriver`. Daemon-mode auto-attach is a later phase (it needs
/// the cross-process session-graph + persistence story).
pub struct AutoAttachDriver {
    /// The praça decision engine — index + binding + policy + name
    /// style. Mutated as panes are created + visits recorded.
    praca: praca::Praca,
    /// The live in-process tear control plane. The session source +
    /// spawn target for `SpawnNew`, and the registry the
    /// SessionId↔PaneId translation reads.
    inproc: Arc<tear_core::InProcess>,
    /// The shared switch channel the switchable attach drains. A
    /// decided action posts the target pane here; the event loop's
    /// switch block does the teardown/rebuild.
    switch: SwitchRequests,
    /// The session mado is currently displaying. Drives praca's
    /// `current_id` so a `cd` *within* the current project stays put,
    /// and is advanced to the just-attached session BEFORE the next
    /// tick — the no-thrash guard.
    current_session: SessionId,
    /// The last cwd we acted on. Only an actual change triggers a
    /// decision (cheap per-tick string compare, no praca/fs work on the
    /// common no-change tick).
    last_cwd: Option<String>,
    /// The shell mado spawns into a new session (the configured/default
    /// shell — same value the initial embedded session used).
    shell: String,
    /// mado's typed capability env projection, re-applied (with the new
    /// project root as cwd) before every `SpawnNew` so a spawned
    /// session inherits the same truecolor/terminfo env + a correct
    /// `PWD` as the boot session. Cloned per spawn + given the spawn
    /// root via `with_cwd`.
    spawn_env_base: tear_types::SpawnEnv,
}

impl AutoAttachDriver {
    /// Build a driver seeded from the live embedded session graph.
    ///
    /// `boot_session` / `boot_pane` are the session+pane mado opened on
    /// launch; `boot_root` is that session's project root (already
    /// resolved by the caller from the boot cwd). The driver seeds its
    /// index + binding with this one live session so a `cd` back into
    /// the boot project resolves to `SwitchTo` (not a duplicate spawn),
    /// and seats `current_session` at it so the first cwd observation
    /// inside the boot project stays put.
    ///
    /// `policy` comes from `tear.auto_attach` via
    /// [`crate::config::AutoAttachMode::policy`]; `now` is the caller's
    /// injected clock.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        inproc: Arc<tear_core::InProcess>,
        switch: SwitchRequests,
        policy: praca::AttachPolicy,
        name_style: ishou_tokens::SessionNameStyle,
        boot_session: SessionId,
        boot_pane: PaneId,
        boot_root: PathBuf,
        shell: String,
        spawn_env_base: tear_types::SpawnEnv,
        now: u64,
    ) -> Self {
        let mut index = praca::SessionIndex::new();
        index.upsert(praca::SessionRecord::for_project(
            boot_session,
            boot_root.clone(),
            name_style,
            now,
        ));
        let mut binding = praca::ProjectBinding::new();
        binding.bind(boot_root, boot_session);
        let praca = praca::Praca::with(index, binding, policy, name_style);
        // The boot pane lives in the registry already; nothing to map
        // beyond recording the seat.
        let _ = boot_pane;
        Self {
            praca,
            inproc,
            switch,
            current_session: boot_session,
            last_cwd: None,
            shell,
            spawn_env_base,
        }
    }

    /// The session mado is currently seated at — the no-thrash anchor.
    /// Read by the test suite (the no-thrash + seat-advance assertions);
    /// kept on the type as the observability surface for a future
    /// status-line / MCP consumer.
    #[must_use]
    #[allow(dead_code)] // test/observability accessor; no non-test reader yet
    pub fn current_session(&self) -> SessionId {
        self.current_session
    }

    /// Resolve a session's first pane from the live registry. An
    /// embedded session is one window with one `active_pane`; this is
    /// the SessionId→PaneId half of the translation praca's
    /// decisions need before they can drive the PaneId switch channel.
    #[must_use]
    pub fn first_pane_of(&self, session: SessionId) -> Option<PaneId> {
        self.inproc.with_registry(|r| {
            r.sessions
                .get(&session)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
    }

    /// Observe the displayed pane's current OSC-7 cwd. Acts ONLY on an
    /// actual change (cheap compare against `last_cwd`); a no-change
    /// tick returns `None` having done no praca/registry/fs work. On a
    /// real change, decides + performs the auto-attach action and
    /// returns the typed [`AutoAttachOutcome`].
    ///
    /// `now` is the caller's injected unix-seconds clock.
    pub fn on_displayed_cwd(
        &mut self,
        cwd: Option<&str>,
        now: u64,
    ) -> Option<AutoAttachOutcome> {
        let cwd = cwd?;
        // Only an ACTUAL change triggers work — the common idle tick
        // (cwd unchanged) costs one string compare and returns.
        if self.last_cwd.as_deref() == Some(cwd) {
            return None;
        }
        self.last_cwd = Some(cwd.to_owned());
        Some(self.act_on_cwd(Path::new(cwd), now))
    }

    /// Decide + perform the auto-attach action for a confirmed cwd
    /// change. Split from [`Self::on_displayed_cwd`] so the change-gate
    /// is separate from the decision logic.
    fn act_on_cwd(&mut self, new_cwd: &Path, now: u64) -> AutoAttachOutcome {
        let decision =
            self.praca
                .on_cwd_change(Some(self.current_session), new_cwd, now);
        match decision {
            praca::AttachDecision::Stay => AutoAttachOutcome::Stay,
            praca::AttachDecision::SwitchTo(session) => self.perform_switch(session, now),
            praca::AttachDecision::SpawnNew { project_root, name } => {
                self.perform_spawn(project_root, name.to_string(), now)
            }
            praca::AttachDecision::Suggest(inner) => self.surface_suggestion(*inner),
        }
    }

    /// Realize a `SwitchTo`: translate the session to its live pane,
    /// post it to the switch channel, record the visit, and advance the
    /// seat (the no-thrash guard — the next tick sees the new project as
    /// "current" and stays).
    fn perform_switch(&mut self, session: SessionId, now: u64) -> AutoAttachOutcome {
        let Some(pane) = self.first_pane_of(session) else {
            // Bound session vanished between index + decision. Drop the
            // stale index/binding entry so a retry spawns fresh, and
            // skip this tick.
            self.praca.index.remove(session);
            self.praca.binding.remove_session(session);
            return AutoAttachOutcome::Skipped {
                reason: AutoAttachSkip::BoundSessionGone(session),
            };
        };
        self.switch.post(pane);
        self.praca.record_visit(session, now);
        self.current_session = session;
        AutoAttachOutcome::Switched { session, pane }
    }

    /// Realize a `SpawnNew`: spawn an auto-named session at the project
    /// root, bind + index it, post its first pane to the switch
    /// channel, and advance the seat.
    fn perform_spawn(
        &mut self,
        root: PathBuf,
        name: String,
        now: u64,
    ) -> AutoAttachOutcome {
        // Spawn with mado's capability env + the project root as cwd, so
        // the new session's shell starts in the project (and inherits
        // truecolor/terminfo) just like the boot session did.
        let spawn_env = self
            .spawn_env_base
            .clone()
            .with_cwd(Some(root.to_string_lossy().into_owned()));
        self.inproc.set_spawn_env(spawn_env);

        let session = match self.inproc.new_session_with_source_and_size(
            &name,
            &self.shell,
            SessionSource::Named("mado-auto-attach".into()),
            (80, 24),
        ) {
            Ok(sid) => sid,
            Err(e) => {
                return AutoAttachOutcome::Skipped {
                    reason: AutoAttachSkip::SpawnFailed(e.to_string()),
                };
            }
        };
        let Some(pane) = self.first_pane_of(session) else {
            return AutoAttachOutcome::Skipped {
                reason: AutoAttachSkip::SpawnedSessionHadNoPane(session),
            };
        };

        // Bind + index BEFORE posting so a `cd` back here later resolves
        // to SwitchTo, and so the seat advance keeps the index honest.
        self.praca.binding.bind(root.clone(), session);
        self.praca.index.upsert(praca::SessionRecord::for_project(
            session,
            root.clone(),
            self.praca.name_style,
            now,
        ));
        self.switch.post(pane);
        self.praca.record_visit(session, now);
        self.current_session = session;
        AutoAttachOutcome::Spawned {
            session,
            pane,
            root,
            name,
        }
    }

    /// Surface a `Suggest` decision without moving the pane. Lightest
    /// reasonable surfacing: a typed outcome carrying a human phrase the
    /// caller logs (and a future status-line/MCP consumer can render).
    fn surface_suggestion(&self, inner: praca::AttachDecision) -> AutoAttachOutcome {
        let hint = match inner {
            praca::AttachDecision::SwitchTo(session) => {
                let word = self
                    .praca
                    .index
                    .get(session)
                    .map(praca::SessionRecord::name_word)
                    .unwrap_or("session");
                format!("cd suggests switching to {word}")
            }
            praca::AttachDecision::SpawnNew { name, .. } => {
                format!("cd suggests a new session {name}")
            }
            // A Suggest never wraps Stay/Suggest (praca guarantees), but
            // be total rather than panic.
            _ => "cd suggests an attach".to_owned(),
        };
        AutoAttachOutcome::Suggested { hint }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutoAttachMode;

    /// A real on-disk project tree so the production `project_root`
    /// marker-walk behaves exactly as it does at runtime: two
    /// `.git`-marked roots (A, B) plus a nested subdir under each. Using
    /// real dirs keeps the test honest — the string→`project_root`→
    /// decision path is the SAME code production runs, not a mock.
    struct TempProjects {
        _tmp: tempfile::TempDir,
        a: PathBuf,
        a_deep: PathBuf,
        b: PathBuf,
        b_deep: PathBuf,
    }

    impl TempProjects {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let a = tmp.path().join("proj-a");
            let b = tmp.path().join("proj-b");
            let a_deep = a.join("src").join("deep");
            let b_deep = b.join("src");
            std::fs::create_dir_all(a.join(".git")).expect("a/.git");
            std::fs::create_dir_all(b.join(".git")).expect("b/.git");
            std::fs::create_dir_all(&a_deep).expect("a_deep");
            std::fs::create_dir_all(&b_deep).expect("b_deep");
            Self {
                _tmp: tmp,
                a,
                a_deep,
                b,
                b_deep,
            }
        }
        fn s(p: &Path) -> String {
            p.to_string_lossy().into_owned()
        }
    }

    /// Spawn a `/bin/sh` session in a fresh InProcess seeded at `root`,
    /// returning the control plane + the boot session/pane for driver
    /// construction.
    fn boot(root: &Path) -> (Arc<tear_core::InProcess>, SessionId, PaneId) {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(
            tear_types::SpawnEnv::none().with_cwd(Some(root.to_string_lossy().into_owned())),
        );
        let sid = inproc
            .new_session_with_source_and_size(
                "boot",
                "/bin/sh",
                SessionSource::Named("boot".into()),
                (80, 24),
            )
            .expect("spawn boot session");
        let pane = inproc
            .with_registry(|r| {
                r.sessions
                    .get(&sid)
                    .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
            })
            .expect("boot session has a pane");
        (inproc, sid, pane)
    }

    /// A driver booted at project A's root (so A's project_root resolves
    /// to `a`, and any cwd under `a` is a same-project cd).
    fn driver_at(root: &Path, mode: AutoAttachMode) -> (AutoAttachDriver, SwitchRequests) {
        let (inproc, sid, pane) = boot(root);
        let switch = SwitchRequests::default();
        switch.attach_sink();
        let driver = AutoAttachDriver::new(
            inproc,
            switch.clone(),
            mode.policy(),
            ishou_tokens::SessionNameStyle::Emoji,
            sid,
            pane,
            // The boot root must be the RESOLVED project root, exactly as
            // production passes `project_root(boot_cwd)`.
            praca::project::project_root(root),
            "/bin/sh".to_owned(),
            tear_types::SpawnEnv::none(),
            1000,
        );
        (driver, switch)
    }

    #[test]
    fn config_mode_maps_to_policy() {
        assert_eq!(AutoAttachMode::Off.policy(), praca::AttachPolicy::PickerOnly);
        assert_eq!(
            AutoAttachMode::AutoSwitch.policy(),
            praca::AttachPolicy::AutoSwitch
        );
        assert_eq!(
            AutoAttachMode::Suggest.policy(),
            praca::AttachPolicy::SuggestOnly
        );
        assert!(!AutoAttachMode::Off.is_active());
        assert!(AutoAttachMode::AutoSwitch.is_active());
        assert!(AutoAttachMode::Suggest.is_active());
    }

    #[test]
    fn no_change_tick_does_nothing() {
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::AutoSwitch);
        // First observation of the boot cwd is a "change" from None →
        // the boot cwd, but it resolves to Stay (same project).
        let first = driver.on_displayed_cwd(Some(&TempProjects::s(&p.a)), 1001);
        assert_eq!(first, Some(AutoAttachOutcome::Stay));
        // A repeat of the same cwd does no work at all.
        let again = driver.on_displayed_cwd(Some(&TempProjects::s(&p.a)), 1002);
        assert_eq!(again, None, "an unchanged cwd is a pure no-op");
        assert!(switch.take().is_none(), "nothing posted on a no-op");
    }

    #[test]
    fn cd_within_same_project_stays() {
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::AutoSwitch);
        // A deeper cwd inside the SAME .git-marked project root → Stay,
        // no switch (project_root walks up to `a`).
        let out = driver.on_displayed_cwd(Some(&TempProjects::s(&p.a_deep)), 1001);
        assert_eq!(out, Some(AutoAttachOutcome::Stay));
        assert!(switch.take().is_none(), "same-project cd posts nothing");
    }

    #[test]
    fn cd_into_new_project_spawns_binds_switches() {
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::AutoSwitch);
        let boot_sid = driver.current_session();
        let b_root = praca::project::project_root(&p.b);
        // cd into a brand-new project → spawn + bind + index + post.
        let out = driver
            .on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1001)
            .expect("a cwd change must produce an outcome");
        let (spawned_session, spawned_pane) = match &out {
            AutoAttachOutcome::Spawned {
                session,
                pane,
                root,
                name,
            } => {
                assert_eq!(root, &b_root, "spawned at B's resolved project root");
                assert!(!name.is_empty(), "deterministic emoji name is non-empty");
                (*session, *pane)
            }
            other => panic!("expected Spawned, got {other:?}"),
        };
        assert_ne!(spawned_session, boot_sid, "a distinct new session");
        // The seat advanced to the new session (the no-thrash guard).
        assert_eq!(driver.current_session(), spawned_session);
        // The new pane was posted to the switch channel.
        assert_eq!(switch.take(), Some(spawned_pane));
    }

    #[test]
    fn deterministic_name_on_spawn() {
        // The spawned session's name is exactly the praça/ishou emoji
        // name of the project root — not a private scheme.
        let p = TempProjects::new();
        let (mut driver, _switch) = driver_at(&p.a, AutoAttachMode::AutoSwitch);
        let b_root = praca::project::project_root(&p.b);
        let out = driver
            .on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1001)
            .unwrap();
        let expected = ishou_tokens::FleetSessionNames::from_project_path(
            &b_root,
            ishou_tokens::SessionNameStyle::Emoji,
        )
        .to_string();
        match out {
            AutoAttachOutcome::Spawned { name, .. } => assert_eq!(name, expected),
            other => panic!("expected Spawned, got {other:?}"),
        }
    }

    #[test]
    fn no_thrash_after_switch_then_same_project_cd() {
        // THE no-thrash property: after a switch to project B, observing
        // B's cwd again (and deeper cwds within B) must NOT fire a second
        // switch — the seat advanced + last_cwd remembers B.
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::AutoSwitch);
        // 1. cd into B → spawn + switch.
        let spawned = driver
            .on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1001)
            .unwrap();
        assert!(matches!(spawned, AutoAttachOutcome::Spawned { .. }));
        let spawned_sid = driver.current_session();
        assert!(switch.take().is_some(), "the spawn posted a pane");
        // 2. The displayed pane is now B; its shell re-reports B's EXACT
        //    cwd (the spawn cwd = B's root). last_cwd already remembers
        //    it, so this is a pure no-op — the strongest no-thrash form.
        let after = driver.on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1002);
        assert_eq!(
            after, None,
            "re-observing the identical just-attached cwd is a pure no-op"
        );
        // 3. A DEEPER cwd within B is a change-of-string but a
        //    same-project cd → Stay, never a second switch.
        let deeper = driver.on_displayed_cwd(Some(&TempProjects::s(&p.b_deep)), 1003);
        assert_eq!(
            deeper,
            Some(AutoAttachOutcome::Stay),
            "a deeper cwd within the just-attached project must Stay"
        );
        assert!(switch.take().is_none(), "no-thrash: nothing further posted");
        // The seat never moved off the spawned session.
        assert_eq!(driver.current_session(), spawned_sid);
    }

    #[test]
    fn cd_back_into_bound_project_switches_not_respawns() {
        // cd A → B → A: the third cd must SwitchTo the original session
        // (bound + still live), not spawn a duplicate.
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::AutoSwitch);
        let boot_sid = driver.current_session();
        let boot_pane = driver.first_pane_of(boot_sid).unwrap();
        // cd into B (spawn).
        driver
            .on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1001)
            .unwrap();
        let _ = switch.take();
        // cd back into A → SwitchTo the original boot session.
        let back = driver
            .on_displayed_cwd(Some(&TempProjects::s(&p.a)), 1002)
            .unwrap();
        match back {
            AutoAttachOutcome::Switched { session, pane } => {
                assert_eq!(session, boot_sid, "back to the original A session");
                assert_eq!(pane, boot_pane, "back to A's live pane");
            }
            other => panic!("expected Switched, got {other:?}"),
        }
        assert_eq!(switch.take(), Some(boot_pane), "A's pane re-posted");
        assert_eq!(driver.current_session(), boot_sid);
    }

    #[test]
    fn suggest_mode_never_moves_the_pane() {
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::Suggest);
        let boot_sid = driver.current_session();
        let out = driver
            .on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1001)
            .unwrap();
        match out {
            AutoAttachOutcome::Suggested { hint } => {
                assert!(hint.contains("new session"), "got hint {hint:?}");
            }
            other => panic!("expected Suggested, got {other:?}"),
        }
        assert!(
            switch.take().is_none(),
            "Suggest must never post a switch"
        );
        // The seat did NOT advance — the operator is still on A, so a
        // later actual decision still treats A as current.
        assert_eq!(
            driver.current_session(),
            boot_sid,
            "Suggest leaves the seat at the boot session"
        );
    }

    #[test]
    fn off_mode_policy_is_pickeronly_always_stays() {
        // Even constructed (which the event loop never does for Off),
        // the PickerOnly policy means every cwd change resolves to Stay.
        let p = TempProjects::new();
        let (mut driver, switch) = driver_at(&p.a, AutoAttachMode::Off);
        let out = driver.on_displayed_cwd(Some(&TempProjects::s(&p.b)), 1001);
        assert_eq!(out, Some(AutoAttachOutcome::Stay));
        assert!(switch.take().is_none(), "Off never posts a switch");
    }
}
