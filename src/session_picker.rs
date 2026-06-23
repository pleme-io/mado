//! Ctrl-S session switcher — the praça centred fuzzy popup.
//!
//! The headline praça automation is auto-attach-on-cd
//! ([`crate::auto_attach`]): `cd` into a project seats you at its session
//! with zero keys. The **switcher** is the explicit surface the operator
//! reaches for on Ctrl-S — a centred popup that fuzzy-filters every live
//! session as you type (like the shell's Ctrl-R / Ctrl-T finders), and:
//!
//! * **switches** to a session on Enter when one matches the query, or
//! * **creates + switches** to a fresh session when none does — choosing
//!   from the **emoji presets** (one per [`ishou_tokens::FleetSessionNames`]
//!   atlas entry, fuzzy-matched by name: type `smile`, get the `:smile:`
//!   preset) when the query matches an emoji, else a session named
//!   literally by the typed query.
//!
//! ## Base + delta
//!
//! The modal state is the generic [`crate::picker::state::FuzzyPicker`]
//! over [`SessionPickerRow`]; this file owns only the session-specific
//! delta — the [`SessionPickerBridge`] seam (`list` produces rows,
//! `switch_to` / `create_and_switch` perform the accept) and the concrete
//! [`PracaPickerBridge`] that reads the shared praça index + the live
//! registry + the switch channel + the spawn capability.
//!
//! ## Why a bridge trait
//!
//! The input engine ([`crate::ux::InputEngine`]) owns no `praca`,
//! `tear_core::InProcess`, or `SwitchRequests` — those live in the
//! embedded event loop. The [`SessionPickerBridge`] trait is the seam:
//! the engine drives the picker *state* (query/selection) and calls the
//! bridge to (re)populate the list and to perform switch / create, while
//! the concrete [`PracaPickerBridge`] (constructed in `gui_tear_attach`)
//! owns the shared praça index + the live registry + the switch channel +
//! the spawn shell/env.
//!
//! ## Gating
//!
//! Switch + create need the switchable event loop
//! (`tear.session_switching = true`). When switching is off no bridge is
//! constructed, so Ctrl-S opens an empty picker showing a typed
//! "switching disabled" hint and Enter is inert — mirroring the
//! `switch_session` MCP tool's `switching-disabled` answer.

use std::sync::{Arc, Mutex};

use ishou_tokens::{FleetSessionNames, SessionName};
use tear_types::{DefinitionId, MultiplexerControl, PaneId, SessionId, SessionSource};

/// What creating a session from the picker should name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateSpec {
    /// Create a session whose identity is exactly
    /// [`FleetSessionNames::POOL`]`[pool_index]` (the chosen emoji preset).
    Preset { pool_index: usize },
    /// Create a session named literally by the typed query.
    Named { name: String },
}

/// What accepting a picker row does — the typed delta the engine's accept
/// arm dispatches on. Either jump to an existing live session, or create a
/// new one (from an emoji preset or a typed name) and jump to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Switch to this existing, live session.
    Switch(SessionId),
    /// Instantiate this latent preset (a saved/authored definition with no
    /// live instance), then switch to it. The ○ rows in the union picker.
    Instantiate(DefinitionId),
    /// Create a new session per the spec, then switch to it.
    Create(CreateSpec),
}

/// One row the picker displays. Plain data (the renderer draws `label`;
/// the engine's accept dispatches on `kind`) — no praca / tear type leaks
/// into the render layer beyond the [`SessionId`] inside
/// [`RowKind::Switch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPickerRow {
    /// The display line: `🌊 tide  mado` for a live session, `🌊 tide` for
    /// an emoji preset, or `＋ create foo` for a typed name. Pre-composed
    /// by the bridge so the renderer draws plain text.
    pub label: String,
    /// What Enter on this row does.
    pub kind: RowKind,
}

/// The modal state of the Ctrl-S switcher — the generic fuzzy-picker over
/// [`SessionPickerRow`]. (`SessionPickerState` is now just this alias; the
/// shape is shared with the dir picker via
/// [`crate::picker::state::FuzzyPicker`].)
pub type SessionPickerState = crate::picker::state::FuzzyPicker<SessionPickerRow>;

/// The seam between the mode-agnostic input engine and the live praça
/// index + registry + switch channel + spawn capability. The engine holds
/// an optional `Box<dyn SessionPickerBridge>`; `None` means the picker is
/// inert (session-switching disabled).
pub trait SessionPickerBridge: Send {
    /// Rows for the given fuzzy `query` (empty → live sessions by
    /// frecency, or every emoji preset when no session exists). `now` is
    /// unix-seconds, injected for the frecency tie-break. This is the
    /// [`crate::picker::state::PickerSource`] `list` seam for the session
    /// picker.
    fn list(&self, query: &str, now: u64) -> Vec<SessionPickerRow>;

    /// Switch the displayed pane to `session` by posting its live first
    /// pane into the switch channel. `true` if a pane was posted, `false`
    /// if the session vanished between listing + accept.
    fn switch_to(&self, session: SessionId) -> bool;

    /// Create a new session per `spec`, index it, and switch to it.
    /// `true` on success, `false` if the spawn failed or produced no pane.
    fn create_and_switch(&self, spec: CreateSpec, now: u64) -> bool;

    /// Instantiate a latent preset by id and switch to it — the
    /// [`RowKind::Instantiate`] accept path. Default `false` (an inert
    /// bridge doesn't instantiate); the live praça bridge overrides it.
    fn instantiate_and_switch(&self, _def_id: DefinitionId, _now: u64) -> bool {
        false
    }

    /// Capture a live session as a reusable preset — the save-as-preset
    /// gesture. Dehydrates the session's layout + spawn specs (via the
    /// shipped `praca::SessionDefinition::from_live`) into the latent
    /// catalog, so it appears as a ○ Instantiate row once not running.
    /// Default `false`; the live praça bridge overrides it.
    fn save_as_preset(&self, _session: SessionId, _now: u64) -> bool {
        false
    }

    /// Reconcile the index against the live session registry before
    /// listing, so out-of-band-spawned sessions (MCP `spawn_term`, `tear
    /// new-session`, manual attach) are tracked + browsable too — the
    /// "always tracking + curating" sync. Default no-op (test bridges).
    fn refresh(&self, _now: u64) {}
}

/// The concrete [`SessionPickerBridge`] wired in the embedded event loop:
/// the praça index shared with the auto-attach driver + the reconciler
/// (so spawned/visited/out-of-band sessions all appear), the live
/// `InProcess` registry (the `SessionId` → first-pane translation + the
/// spawn target), the switch channel, and the spawn shell/env (so the
/// picker can create new sessions like auto-attach does).
pub struct PracaPickerBridge {
    /// The praça decision engine, shared with the auto-attach driver +
    /// the reconciler. One index, many readers/writers — never a fork.
    praca: Arc<Mutex<praca::Praca>>,
    /// The live in-process tear control plane — the `SessionId` → first
    /// pane translation + the spawn target.
    inproc: Arc<tear_core::InProcess>,
    /// The shared switch channel the switchable attach drains — the SAME
    /// channel auto-attach + the `switch_session` MCP tool post to.
    switch: crate::session_switch::SwitchRequests,
    /// The shell a newly-created session spawns into (the configured /
    /// default shell — same value auto-attach spawns with).
    shell: String,
    /// mado's typed capability env, re-applied before each spawn so a
    /// created session inherits the same truecolor/terminfo env as the
    /// boot session (mirrors `AutoAttachDriver`).
    spawn_env_base: tear_types::SpawnEnv,
}

impl PracaPickerBridge {
    /// Build the bridge over the shared praça index, the live registry,
    /// the switch channel, and the spawn shell/env.
    #[must_use]
    pub fn new(
        praca: Arc<Mutex<praca::Praca>>,
        inproc: Arc<tear_core::InProcess>,
        switch: crate::session_switch::SwitchRequests,
        shell: String,
        spawn_env_base: tear_types::SpawnEnv,
    ) -> Self {
        Self {
            praca,
            inproc,
            switch,
            shell,
            spawn_env_base,
        }
    }

    fn praca(&self) -> std::sync::MutexGuard<'_, praca::Praca> {
        self.praca
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Resolve a session's first pane from the live registry — the
    /// `SessionId` → `PaneId` half of the switch translation.
    fn first_pane_of(&self, session: SessionId) -> Option<PaneId> {
        self.inproc.with_registry(|r| {
            r.sessions
                .get(&session)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
    }

    /// Compose a picker row label: `<badge> display_name  basename`. A
    /// running session gets a `● ` live badge; a not-running preset gets a
    /// `○ ` latent badge — so the union picker reads at a glance which rows
    /// are alive vs launchable. The renderer draws it verbatim.
    fn row_label(display_name: &str, project_root: &std::path::Path, latent: bool) -> String {
        let mut label = String::new();
        label.push_str(if latent {
            "\u{25cb} " // ○ latent
        } else {
            "\u{25cf} " // ● live
        });
        label.push_str(display_name);
        let basename = project_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !basename.is_empty() && basename != "/" {
            label.push_str("  ");
            label.push_str(&basename);
        }
        label
    }

    /// Instantiate a latent preset by its [`DefinitionId`] and switch to it
    /// — the [`RowKind::Instantiate`] accept path. Looks the definition up
    /// in the catalog, runs the shipped `praca::instantiate` interpreter
    /// against the live backend, indexes + binds the fresh session, and
    /// posts its first pane to the same switch channel `switch_to` uses.
    fn instantiate_preset(&self, def_id: tear_types::DefinitionId, now: u64) -> bool {
        let def = {
            let praca = self.praca();
            praca.definitions.get(def_id).cloned()
        };
        let Some(def) = def else {
            return false;
        };
        let live = match praca::instantiate(&def, self.inproc.as_ref()) {
            Ok(l) => l,
            Err(_) => return false,
        };
        let sid = live.instance();
        let Some(pane) = self.first_pane_of(sid) else {
            return false;
        };
        // Index + bind the new session so it's browsable/switchable and a
        // `cd` back resolves to it (mirrors create_and_switch's bookkeeping).
        {
            let mut praca = self.praca();
            let style = praca.name_style;
            let mut rec =
                praca::SessionRecord::for_project(sid, def.project_root.clone(), style, now);
            rec.name_seed = def.name_seed;
            praca.index.upsert(rec);
            praca.binding.bind(def.project_root.clone(), sid);
            praca.record_visit(sid, now);
        }
        self.switch.post(pane);
        true
    }

    /// Spawn a fresh session named `tear_name` into the live registry,
    /// inheriting mado's capability env. Mirrors
    /// `AutoAttachDriver::perform_spawn`'s spawn half.
    fn spawn_named(&self, tear_name: &str) -> Option<SessionId> {
        self.inproc.set_spawn_env(self.spawn_env_base.clone());
        self.inproc
            .new_session_with_source_and_size(
                tear_name,
                &self.shell,
                SessionSource::Named("mado-session-picker".into()),
                (80, 24),
            )
            .ok()
    }
}

/// Capture a live session into the latent preset catalog — the shared
/// save-as-preset logic the bridge method AND the `save_session_as_preset`
/// MCP verb both call (one place, per the prime directive). Dehydrates the
/// session's layout + spawn specs via the shipped
/// `praca::SessionDefinition::from_live`, keyed on the session's bound
/// project root. `false` if the session is gone or untracked (no project).
pub(crate) fn capture_preset(
    inproc: &tear_core::InProcess,
    praca: &Mutex<praca::Praca>,
    session: SessionId,
    now: u64,
) -> bool {
    let Ok(tear_session) = inproc.get_session(session) else {
        return false;
    };
    let lock = || praca.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(project_root) = lock().index.get(session).map(|r| r.project_root.clone()) else {
        return false;
    };
    let def = praca::SessionDefinition::from_live(&tear_session, project_root, now);
    lock().definitions.upsert(def);
    true
}

impl SessionPickerBridge for PracaPickerBridge {
    fn list(&self, query: &str, now: u64) -> Vec<SessionPickerRow> {
        let (rows, style) = {
            let praca = self.praca();
            // Live sessions with a live pane (a reaped one would post a
            // no-op — keep the picker honest).
            let live_recs: Vec<&praca::SessionRecord> = praca
                .index
                .all()
                .iter()
                .filter(|rec| self.first_pane_of(rec.id).is_some())
                .collect();
            // Project roots with a live session — suppress a latent row for
            // a running preset (it shows as its Switch row instead).
            let live_roots: std::collections::HashSet<&std::path::Path> =
                live_recs.iter().map(|r| r.project_root.as_path()).collect();
            // ONE heterogeneous set: live sessions + non-running presets,
            // ranked TOGETHER by the shared comparator so they interleave by
            // frecency / fuzzy match — not "live always above latent".
            let mut items: Vec<praca::Ranked> =
                live_recs.iter().map(|r| praca::Ranked::Live(*r)).collect();
            for def in praca.definitions.all() {
                if !live_roots.contains(def.project_root.as_path()) {
                    items.push(praca::Ranked::Latent(def));
                }
            }
            let rows: Vec<SessionPickerRow> = praca::rank_mixed(items, query, now)
                .into_iter()
                .map(|ranked| match ranked {
                    praca::Ranked::Live(rec) => SessionPickerRow {
                        // ● live badge.
                        label: Self::row_label(&rec.display_name(), &rec.project_root, false),
                        kind: RowKind::Switch(rec.id),
                    },
                    praca::Ranked::Latent(def) => SessionPickerRow {
                        // ○ latent badge.
                        label: Self::row_label(&def.display_name(), &def.project_root, true),
                        kind: RowKind::Instantiate(def.def_id),
                    },
                })
                .collect();
            (rows, praca.name_style)
        };
        // If anything matched (live or latent), that's the picker's answer;
        // only an empty union falls through to the create surface.
        if !rows.is_empty() {
            return rows;
        }

        // No live session matched → the CREATE surface. Emoji presets
        // first (every emoji on an empty query; fuzzy-matched by name /
        // keyword otherwise — "type smile, get the smile preset").
        let presets = crate::picker::presets::matching(query, style);
        if !presets.is_empty() {
            return presets
                .into_iter()
                .map(|p| SessionPickerRow {
                    label: p.label,
                    kind: RowKind::Create(CreateSpec::Preset {
                        pool_index: p.pool_index,
                    }),
                })
                .collect();
        }

        // The query matches no session AND no emoji → offer to create a
        // session named literally by the query.
        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let mut label = String::from("\u{ff0b} create ");
        label.push_str(q);
        vec![SessionPickerRow {
            label,
            kind: RowKind::Create(CreateSpec::Named { name: q.to_owned() }),
        }]
    }

    fn switch_to(&self, session: SessionId) -> bool {
        match self.first_pane_of(session) {
            Some(pane) => {
                self.switch.post(pane);
                true
            }
            None => false,
        }
    }

    fn instantiate_and_switch(&self, def_id: DefinitionId, now: u64) -> bool {
        self.instantiate_preset(def_id, now)
    }

    fn save_as_preset(&self, session: SessionId, now: u64) -> bool {
        capture_preset(self.inproc.as_ref(), &self.praca, session, now)
    }

    fn refresh(&self, now: u64) {
        use crate::picker::reconcile::IndexReconciler;
        let reconciler =
            crate::picker::reconcile::InProcessSessionReconciler::new(Arc::clone(&self.inproc));
        let mut praca = self.praca();
        reconciler.reconcile(&mut praca, now);
    }

    fn create_and_switch(&self, spec: CreateSpec, now: u64) -> bool {
        let style = self.praca().name_style;
        // The tear-registry name: the emoji label for a preset, the typed
        // name otherwise.
        let tear_name = match &spec {
            CreateSpec::Preset { pool_index } => SessionName {
                identity: FleetSessionNames::POOL[*pool_index],
                style,
            }
            .to_string(),
            CreateSpec::Named { name } => name.clone(),
        };
        let Some(sid) = self.spawn_named(&tear_name) else {
            return false;
        };
        let Some(pane) = self.first_pane_of(sid) else {
            return false;
        };

        // Index the new session so it's immediately browsable + switchable
        // (and a `cd` back here later resolves to a switch). A preset's
        // record reproduces the chosen identity (name_seed = POOL index,
        // whole-pool theme); a named one carries the typed custom name.
        {
            let mut praca = self.praca();
            let mut rec =
                praca::SessionRecord::for_project(sid, std::path::PathBuf::from("/"), style, now);
            match &spec {
                CreateSpec::Preset { pool_index } => rec.name_seed = *pool_index as u64,
                CreateSpec::Named { name } => rec.rename(name.clone()),
            }
            praca.index.upsert(rec);
            praca.record_visit(sid, now);
        }
        self.switch.post(pane);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ishou_tokens::SessionNameStyle;
    use std::path::PathBuf;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    fn bridge_with(praca: praca::Praca, inproc: Arc<tear_core::InProcess>) -> PracaPickerBridge {
        let switch = crate::session_switch::SwitchRequests::default();
        switch.attach_sink();
        PracaPickerBridge::new(
            Arc::new(Mutex::new(praca)),
            inproc,
            switch,
            "/bin/sh".to_owned(),
            tear_types::SpawnEnv::none(),
        )
    }

    fn live_inproc() -> (Arc<tear_core::InProcess>, SessionId) {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let live = inproc
            .new_session_with_source_and_size(
                "live",
                "/bin/sh",
                SessionSource::Named("test".into()),
                (80, 24),
            )
            .expect("spawn live session");
        (inproc, live)
    }

    #[test]
    fn bridge_lists_live_sessions_and_switch_posts_pane() {
        let (inproc, live) = live_inproc();
        let live_pane = inproc
            .with_registry(|r| {
                r.sessions
                    .get(&live)
                    .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
            })
            .expect("live session has a pane");

        let mut praca = praca::Praca::new();
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        // A dead session (no live pane) → filtered out of the list.
        praca.index.upsert(praca::SessionRecord::for_project(
            sid("ghost"),
            PathBuf::from("/code/pleme-io/tear"),
            SessionNameStyle::Emoji,
            1000,
        ));

        let bridge = bridge_with(praca, Arc::clone(&inproc));
        let rows = bridge.list("mado", 1000);
        assert_eq!(rows.len(), 1, "only the live-pane session, fuzzy-matched");
        assert_eq!(rows[0].kind, RowKind::Switch(live));
        assert!(rows[0].label.contains("mado"));

        assert!(bridge.switch_to(live), "live session switches");
        assert!(!bridge.switch_to(sid("ghost")), "dead session does not");
    }

    #[test]
    fn bridge_surfaces_a_latent_preset_as_an_instantiate_row() {
        let (inproc, _live) = live_inproc();
        let mut praca = praca::Praca::new();
        // Save a preset for a project that is NOT running.
        let preset = praca::SessionDefinition::single_pane(
            "/code/pleme-io/substrate",
            "/bin/sh",
            praca::NameStyle::Emoji,
            1000,
        );
        let def_id = preset.def_id;
        praca.definitions.upsert(preset);
        let bridge = bridge_with(praca, Arc::clone(&inproc));

        // Empty query → the latent preset appears as a ○ Instantiate row.
        let rows = bridge.list("", 1000);
        let inst = rows
            .iter()
            .find(|r| matches!(r.kind, RowKind::Instantiate(_)))
            .expect("latent preset surfaces as an Instantiate row");
        assert_eq!(inst.kind, RowKind::Instantiate(def_id));
        assert!(inst.label.starts_with('\u{25cb}'), "○ latent badge: {}", inst.label);

        // Accepting it instantiates the preset + switches (spawns a live pane).
        assert!(
            bridge.instantiate_and_switch(def_id, 1000),
            "instantiate spawns the preset + posts its pane"
        );
    }

    #[test]
    fn bridge_hides_a_preset_whose_project_is_already_live() {
        let (inproc, live) = live_inproc();
        let mut praca = praca::Praca::new();
        // The live session is bound to /code/pleme-io/mado.
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        praca.binding.bind(PathBuf::from("/code/pleme-io/mado"), live);
        // A preset for the SAME project must NOT appear as a duplicate latent row.
        praca.definitions.upsert(praca::SessionDefinition::single_pane(
            "/code/pleme-io/mado",
            "/bin/sh",
            praca::NameStyle::Emoji,
            1000,
        ));
        let bridge = bridge_with(praca, Arc::clone(&inproc));
        let rows = bridge.list("", 1000);
        assert!(
            rows.iter().all(|r| !matches!(r.kind, RowKind::Instantiate(_))),
            "a running preset is its Switch row, not a duplicate latent row"
        );
    }

    #[test]
    fn bridge_saves_a_live_session_as_a_preset_then_can_instantiate_it() {
        let (inproc, live) = live_inproc();
        let mut praca = praca::Praca::new();
        // The live session is tracked + bound to a project.
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        let bridge = bridge_with(praca, Arc::clone(&inproc));

        // Save it as a preset → into the latent catalog.
        assert!(bridge.save_as_preset(live, 1000), "captures the live session");
        // The preset is now in the catalog: instantiating its def (keyed on
        // the same project root) spawns a fresh session + posts a pane.
        let def_id = tear_types::DefinitionId::from_project(std::path::Path::new("/code/pleme-io/mado"));
        assert!(
            bridge.instantiate_and_switch(def_id, 1000),
            "the saved preset is in the catalog + instantiable"
        );
    }

    #[test]
    fn bridge_save_as_preset_rejects_an_untracked_session() {
        let (inproc, live) = live_inproc();
        // No index entry for `live` → no project to key the preset on.
        let bridge = bridge_with(praca::Praca::new(), Arc::clone(&inproc));
        assert!(!bridge.save_as_preset(live, 1000));
    }

    #[test]
    fn empty_index_lists_every_emoji_preset() {
        // No sessions at all → the picker lists presets (every emoji).
        let (inproc, _live) = live_inproc();
        // praça index is empty even though a session is live, because the
        // empty query path returns live sessions; here we test the
        // no-session branch by querying a needle no live session matches.
        let bridge = bridge_with(praca::Praca::new(), inproc);
        let rows = bridge.list("", 1000);
        assert_eq!(
            rows.len(),
            FleetSessionNames::POOL.len(),
            "an empty index lists one preset per emoji"
        );
        assert!(matches!(rows[0].kind, RowKind::Create(CreateSpec::Preset { .. })));
    }

    #[test]
    fn query_smile_offers_a_preset_then_named_create() {
        let bridge = bridge_with(praca::Praca::new(), live_inproc().0);
        // "wave" matches the tide preset by keyword → a preset create row.
        let wave = bridge.list("wave", 1000);
        assert!(wave.iter().any(|r| matches!(
            &r.kind,
            RowKind::Create(CreateSpec::Preset { .. })
        ) && r.label.contains("tide")));

        // A query matching no session and no emoji → a named create row.
        let named = bridge.list("zzqq-not-an-emoji", 1000);
        assert_eq!(named.len(), 1);
        assert!(matches!(
            &named[0].kind,
            RowKind::Create(CreateSpec::Named { name }) if name == "zzqq-not-an-emoji"
        ));
        assert!(named[0].label.contains("create"));
    }

    #[test]
    fn create_named_spawns_indexes_and_switches() {
        let (inproc, _live) = live_inproc();
        let bridge = bridge_with(praca::Praca::new(), Arc::clone(&inproc));
        let before = inproc.with_registry(|r| r.sessions.len());

        assert!(
            bridge.create_and_switch(CreateSpec::Named { name: "billing".into() }, 1000),
            "create succeeds"
        );
        let after = inproc.with_registry(|r| r.sessions.len());
        assert_eq!(after, before + 1, "a session was spawned");
        // The new session is indexed + labelled by the typed name.
        let listed = bridge.list("billing", 1001);
        assert!(listed.iter().any(|r| matches!(r.kind, RowKind::Switch(_)) && r.label.contains("billing")));
    }

    #[test]
    fn create_preset_reproduces_the_chosen_emoji() {
        let (inproc, _live) = live_inproc();
        let bridge = bridge_with(praca::Praca::new(), Arc::clone(&inproc));
        // Pick the "frost" preset by its POOL index.
        let frost_idx = FleetSessionNames::POOL
            .iter()
            .position(|i| i.word == "frost")
            .expect("frost in pool");
        assert!(bridge.create_and_switch(CreateSpec::Preset { pool_index: frost_idx }, 1000));
        // The created session is indexed with the frost identity.
        let rows = bridge.list("frost", 1001);
        assert!(
            rows.iter().any(|r| matches!(r.kind, RowKind::Switch(_)) && r.label.contains("frost")),
            "the preset session is named frost, got {rows:?}"
        );
    }
}
