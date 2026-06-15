//! Ctrl-S session picker — the praça "browse + switch" fallback overlay.
//!
//! Auto-attach-on-cd ([`crate::auto_attach`]) is the headline praça
//! automation: `cd` into a project seats you at its session with zero
//! keys. The **picker** is the rare fallback the operator reaches for to
//! *look around* — a fuzzy-filtered, frecency-ranked list of every
//! session praça knows, opened on Ctrl-S, navigated with the arrows /
//! Ctrl-N / Ctrl-P, committed with Enter, cancelled with Esc.
//!
//! ## The shape mirrors [`crate::dir_picker::DirPickerState`]
//!
//! Both are reader-only modal overlay states: pure data the renderer
//! reads via a shared `Arc<Mutex<_>>`, mutated by the input engine
//! through the overlay FSM ([`crate::ux::modes::Overlay`]). The
//! difference is the data source + the accept action:
//!
//! * dir-picker reads `pleme_io_wadachi` and on accept injects `cd …`
//!   into the PTY.
//! * session-picker reads [`praca::Praca::search`] (frecency-ranked
//!   sessions) through a [`SessionPickerBridge`] and on accept asks the
//!   bridge to **switch** to the chosen [`SessionId`] — which posts the
//!   session's live [`tear_types::PaneId`] into the SAME
//!   [`crate::session_switch::SwitchRequests`] channel auto-attach + the
//!   `switch_session` MCP tool drive. There is no second switch path.
//!
//! ## Why a bridge trait
//!
//! The input engine ([`crate::ux::InputEngine`]) owns no `praca`,
//! `tear_core::InProcess`, or `SwitchRequests` — those live in the
//! embedded event loop. The [`SessionPickerBridge`] trait is the seam:
//! the engine drives the picker *state* (query/selection) and calls the
//! bridge to (re)populate the list and to perform the switch, while the
//! concrete [`PracaPickerBridge`] (constructed in `gui_tear_attach`)
//! owns the shared praça index + the live registry + the switch
//! channel. This keeps the engine mode-agnostic, exactly as the
//! `cursor_keys_mode` / `PtySink` seams already do.
//!
//! ## Gating
//!
//! The picker switches panes, which needs the switchable event loop
//! (`tear.session_switching = true`). When switching is off no bridge is
//! constructed, so Ctrl-S opens an empty picker that shows a typed
//! "switching disabled" hint and Enter is inert — mirroring the
//! `switch_session` MCP tool's `switching-disabled` answer.

use std::sync::{Arc, Mutex};

use tear_types::{PaneId, SessionId};

/// One row the picker displays + can switch to. Plain data (no praca /
/// tear type leaks into the render/state layer beyond [`SessionId`], the
/// stable switch handle) — keeps the renderer dependency surface
/// minimal, exactly like [`crate::dir_picker::DirPickerState`]'s
/// `(PathBuf, f64)` rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPickerRow {
    /// The session to switch to when this row is chosen.
    pub id: SessionId,
    /// The display line: the emoji/glyph session name + the project
    /// basename (`🌊 tide  mado`). Pre-composed by the bridge so the
    /// renderer draws plain text.
    pub label: String,
}

/// The seam between the mode-agnostic input engine and the live praça
/// index + registry + switch channel. The engine holds an optional
/// `Box<dyn SessionPickerBridge>`; `None` means the picker is inert
/// (session-switching disabled), and the FSM still routes Ctrl-S to a
/// "switching disabled" hint rather than into the void.
pub trait SessionPickerBridge: Send {
    /// Frecency-ranked rows for the given fuzzy `query` (empty →
    /// everything by frecency). `now` is unix-seconds, injected for the
    /// frecency tie-break — the engine supplies its single clock-read,
    /// keeping praça time-injected end to end.
    fn list(&self, query: &str, now: u64) -> Vec<SessionPickerRow>;

    /// Switch the displayed pane to `session` by posting its live first
    /// pane into the switch channel. Returns `true` if a pane was posted
    /// (the session still has a live pane), `false` if the session
    /// vanished from the registry between listing + accept.
    fn switch_to(&self, session: SessionId) -> bool;
}

/// The modal state of the Ctrl-S session picker overlay. Mirrors
/// [`crate::dir_picker::DirPickerState`]: pure data, driven by the
/// overlay FSM, read by the renderer.
///
/// The bridge is NOT stored here (it lives on the engine) — this struct
/// is the renderer-shared mirror, kept free of `Send`-only trait objects
/// so the `Arc<Mutex<_>>` the renderer holds stays cheap to lock.
#[derive(Default)]
pub struct SessionPickerState {
    /// Whether the overlay is open (gates rendering + input capture).
    pub open: bool,
    /// `true` when the picker is open but session-switching is disabled
    /// (no bridge): the overlay shows a typed hint and Enter is inert.
    /// Distinct from `results.is_empty()` (which also covers "no
    /// sessions match the query") so the render + the FSM can tell
    /// "disabled" from "no matches".
    pub disabled: bool,
    /// The fuzzy filter needle typed so far.
    pub query: String,
    /// Frecency-ranked rows for the current needle (populated by the
    /// engine from the bridge on open + every edit).
    pub results: Vec<SessionPickerRow>,
    /// Index of the highlighted row.
    pub selected: usize,
}

impl SessionPickerState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the overlay, seeding the list from `rows` (the bridge's
    /// empty-query frecency ranking). `disabled` flags the no-bridge
    /// case so the renderer can show the "switching disabled" hint.
    pub fn open(&mut self, rows: Vec<SessionPickerRow>, disabled: bool) {
        self.open = true;
        self.disabled = disabled;
        self.query.clear();
        self.selected = 0;
        self.results = rows;
    }

    /// Close + reset.
    pub fn close(&mut self) {
        self.open = false;
        self.disabled = false;
        self.query.clear();
        self.results.clear();
        self.selected = 0;
    }

    /// Replace the ranked rows after a query edit (the engine recomputes
    /// via the bridge). Resets the highlight to the top, mirroring
    /// `DirPickerState::recompute`.
    pub fn set_results(&mut self, rows: Vec<SessionPickerRow>) {
        self.selected = 0;
        self.results = rows;
    }

    /// Move the highlight down (wraps).
    pub fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
        }
    }

    /// Move the highlight up (wraps).
    pub fn move_up(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// The currently-highlighted session id, if any.
    #[must_use]
    pub fn selected_session(&self) -> Option<SessionId> {
        self.results.get(self.selected).map(|r| r.id)
    }
}

/// The concrete [`SessionPickerBridge`] wired in the embedded event
/// loop: a praça index shared with the auto-attach driver (so spawned
/// sessions appear), the live `InProcess` registry (the `SessionId` →
/// first-pane translation), and the switch channel.
///
/// Embedded-runtime only — the switch targets a pane in the GUI's own
/// `tear_core::InProcess`, exactly like [`crate::auto_attach`]. The
/// shared `Arc<Mutex<praca::Praca>>` is THE live index auto-attach
/// mutates as it spawns/visits sessions, so the picker always reflects
/// the current session graph.
pub struct PracaPickerBridge {
    /// The praça decision engine, shared with the auto-attach driver.
    /// Read here for `search`; mutated there as sessions are
    /// spawned/visited. One index, two readers — never a fork.
    praca: Arc<Mutex<praca::Praca>>,
    /// The live in-process tear control plane — the `SessionId` → first
    /// pane translation reads its registry (same shape as
    /// `AutoAttachDriver::first_pane_of`).
    inproc: Arc<tear_core::InProcess>,
    /// The shared switch channel the switchable attach drains — the
    /// SAME channel auto-attach + the `switch_session` MCP tool post to.
    switch: crate::session_switch::SwitchRequests,
}

impl PracaPickerBridge {
    /// Build the bridge over the shared praça index, the live registry,
    /// and the switch channel.
    #[must_use]
    pub fn new(
        praca: Arc<Mutex<praca::Praca>>,
        inproc: Arc<tear_core::InProcess>,
        switch: crate::session_switch::SwitchRequests,
    ) -> Self {
        Self {
            praca,
            inproc,
            switch,
        }
    }

    /// Resolve a session's first pane from the live registry — the
    /// `SessionId` → `PaneId` half of the switch translation. Identical
    /// shape to `AutoAttachDriver::first_pane_of`.
    fn first_pane_of(&self, session: SessionId) -> Option<PaneId> {
        self.inproc.with_registry(|r| {
            r.sessions
                .get(&session)
                .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
        })
    }
}

impl SessionPickerBridge for PracaPickerBridge {
    fn list(&self, query: &str, now: u64) -> Vec<SessionPickerRow> {
        let praca = self
            .praca
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        praca
            .search(query, now)
            .into_iter()
            // Only rows whose session still has a live pane are
            // switchable — a row whose session was reaped would post a
            // no-op. Filtering here keeps the picker honest (you can't
            // highlight + Enter a dead session).
            .filter(|rec| self.first_pane_of(rec.id).is_some())
            .map(|rec| {
                let basename = rec
                    .project_root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // "🌊 tide  mado" — the emoji/glyph name + the project
                // basename. Composed from typed pieces (no format! of a
                // syntax surface — this is a plain display label).
                let mut label = rec.name().to_string();
                if !basename.is_empty() {
                    label.push_str("  ");
                    label.push_str(&basename);
                }
                SessionPickerRow { id: rec.id, label }
            })
            .collect()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ishou_tokens::SessionNameStyle;
    use std::path::PathBuf;
    use tear_types::MultiplexerControl;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    fn row(id: &str, label: &str) -> SessionPickerRow {
        SessionPickerRow {
            id: sid(id),
            label: label.to_owned(),
        }
    }

    #[test]
    fn open_seeds_rows_and_resets_cursor() {
        let mut st = SessionPickerState::new();
        st.selected = 5;
        st.open(vec![row("a", "🌊 tide  mado"), row("b", "❄ frost  nix")], false);
        assert!(st.open);
        assert!(!st.disabled);
        assert_eq!(st.selected, 0);
        assert_eq!(st.results.len(), 2);
        assert_eq!(st.selected_session(), Some(sid("a")));
    }

    #[test]
    fn open_disabled_flags_the_hint() {
        let mut st = SessionPickerState::new();
        st.open(vec![], true);
        assert!(st.open);
        assert!(st.disabled);
        assert!(st.results.is_empty());
        assert_eq!(st.selected_session(), None);
    }

    #[test]
    fn close_resets_everything() {
        let mut st = SessionPickerState::new();
        st.open(vec![row("a", "x")], false);
        st.query.push_str("ma");
        st.close();
        assert!(!st.open);
        assert!(!st.disabled);
        assert!(st.query.is_empty());
        assert!(st.results.is_empty());
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn move_down_up_wrap() {
        let mut st = SessionPickerState::new();
        st.open(vec![row("a", "a"), row("b", "b"), row("c", "c")], false);
        assert_eq!(st.selected, 0);
        st.move_down();
        assert_eq!(st.selected, 1);
        st.move_up();
        assert_eq!(st.selected, 0);
        // Wrap up from the top → last row.
        st.move_up();
        assert_eq!(st.selected, 2);
        // Wrap down from the bottom → first row.
        st.move_down();
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn move_on_empty_is_a_noop() {
        let mut st = SessionPickerState::new();
        st.open(vec![], false);
        st.move_down();
        st.move_up();
        assert_eq!(st.selected, 0);
        assert_eq!(st.selected_session(), None);
    }

    #[test]
    fn set_results_resets_highlight() {
        let mut st = SessionPickerState::new();
        st.open(vec![row("a", "a"), row("b", "b")], false);
        st.move_down();
        assert_eq!(st.selected, 1);
        st.set_results(vec![row("c", "c")]);
        assert_eq!(st.selected, 0);
        assert_eq!(st.selected_session(), Some(sid("c")));
    }

    /// The bridge lists frecency-ranked rows from a live praça index +
    /// only sessions with a live pane, labels them `name + basename`,
    /// and `switch_to` posts the session's pane into the SAME switch
    /// channel auto-attach drives. Uses a real `InProcess` so the
    /// `SessionId` → pane translation is the production path.
    #[test]
    fn bridge_lists_live_sessions_and_switch_posts_pane() {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let live = inproc
            .new_session_with_source_and_size(
                "live",
                "/bin/sh",
                tear_types::SessionSource::Named("test".into()),
                (80, 24),
            )
            .expect("spawn live session");
        let live_pane = inproc
            .with_registry(|r| {
                r.sessions
                    .get(&live)
                    .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
            })
            .expect("live session has a pane");

        // Praca index: the live session (with a real pane) + a dead one
        // (no pane in the registry → filtered out of the list).
        let mut praca = praca::Praca::new();
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        praca.index.upsert(praca::SessionRecord::for_project(
            sid("ghost"),
            PathBuf::from("/code/pleme-io/tear"),
            SessionNameStyle::Emoji,
            1000,
        ));

        let switch = crate::session_switch::SwitchRequests::default();
        switch.attach_sink();
        let bridge = PracaPickerBridge::new(
            Arc::new(Mutex::new(praca)),
            Arc::clone(&inproc),
            switch.clone(),
        );

        // Empty query → only the live-pane session, labelled with the
        // project basename.
        let rows = bridge.list("", 1000);
        assert_eq!(rows.len(), 1, "the dead 'ghost' session is filtered out");
        assert_eq!(rows[0].id, live);
        assert!(
            rows[0].label.contains("mado"),
            "label carries the project basename, got {:?}",
            rows[0].label
        );

        // A fuzzy query that matches the live session's project keeps it.
        let filtered = bridge.list("mado", 1000);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, live);

        // switch_to posts the live pane into the SAME channel.
        assert!(bridge.switch_to(live), "live session switches");
        assert_eq!(switch.take(), Some(live_pane), "posted the live pane");

        // Switching to the dead session posts nothing + reports false.
        assert!(!bridge.switch_to(sid("ghost")), "dead session does not switch");
        assert!(switch.take().is_none(), "nothing posted for a dead session");
    }
}
