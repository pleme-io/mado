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

use crate::suggest::{SuggestionId, SuggestionStore};

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
    /// Spawn a session aimed at an external task suggestion (PR, ticket,
    /// failing resource, …), then switch to it. The continuously-refreshing
    /// ○ rows the suggestion stream shades in beneath sessions + presets.
    Suggestion(SuggestionId),
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

    /// Spawn a session aimed at a task suggestion (by id) and switch to it —
    /// the [`RowKind::Suggestion`] accept path. Looks the suggestion up in the
    /// store, spawns into its target cwd with its emoji-native name, indexes +
    /// switches. Default `false` (inert bridge); the live praça bridge
    /// overrides it.
    fn spawn_suggestion(&self, _id: SuggestionId, _now: u64) -> bool {
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
    /// Whether `list()` surfaces latent presets as ○ Instantiate rows
    /// (`tear.session_picker_surface_presets`). `false` = strictly live
    /// sessions (the stripped/bare flow), even if presets are saved.
    surface_presets: bool,
    /// How rows are badged (`tear.session_picker_badges`): never / only when
    /// the list mixes live+latent (the minimal-impact default) / always.
    badges: crate::config::BadgeMode,
    /// The shared suggestion store the watcher engine fills — read to surface
    /// the continuously-refreshing ○ task-suggestion rows. `None` = the stream
    /// is off (no suggestion rows). Shared, never forked.
    suggestions: Option<Arc<SuggestionStore>>,
    /// Max suggestion rows to surface (`suggestions.max_visible`). 0 = none.
    suggest_max: usize,
    /// Max rows a single source may contribute to the band
    /// (`suggestions.per_source_cap`) — keeps the band diverse. 0 = no cap.
    suggest_cap: usize,
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
        surface_presets: bool,
        badges: crate::config::BadgeMode,
        suggestions: Option<Arc<SuggestionStore>>,
        suggest_max: usize,
        suggest_cap: usize,
    ) -> Self {
        Self {
            praca,
            inproc,
            switch,
            shell,
            spawn_env_base,
            surface_presets,
            badges,
            suggestions,
            suggest_max,
            suggest_cap,
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
    fn row_label(display_name: &str, project_root: &std::path::Path, badge: Option<bool>) -> String {
        let mut label = String::new();
        match badge {
            Some(true) => label.push_str("\u{25cb} "),  // ○ latent
            Some(false) => label.push_str("\u{25cf} "), // ● live
            None => {}                                  // no badge (stripped / all-homogeneous)
        }
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

    /// Build the continuously-refreshing ○ task-suggestion rows: the store's
    /// urgency/score-ranked set, query-filtered, capped at `suggest_max`.
    /// Empty when the stream is off or no store is wired.
    fn suggestion_rows(&self, query: &str, now: u64) -> Vec<SessionPickerRow> {
        if self.suggest_max == 0 {
            return Vec::new();
        }
        let Some(store) = self.suggestions.as_ref() else {
            return Vec::new();
        };
        let q = query.trim().to_ascii_lowercase();
        // Over-fetch then query-filter so a needle still finds lower-ranked
        // suggestions (the store is already urgency/score ranked). `ranked_stored`
        // carries each row's birth time for the freshness nudge.
        let filtered: Vec<crate::suggest::StoredSuggestion> = store
            .ranked_stored(self.suggest_max.saturating_mul(4).max(self.suggest_max))
            .into_iter()
            .filter(|st| {
                let s = &st.suggestion;
                q.is_empty()
                    || s.title.to_ascii_lowercase().contains(&q)
                    || s.detail
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
            })
            // A suggestion you've already STARTED is a live ● session row now —
            // suppress its ○ twin so the picker never lists the same task twice.
            // The visual half of "nothing duplicate is ever offered".
            .filter(|st| self.live_session_for(&st.suggestion.spawn).is_none())
            .collect();
        // Balance the band: cap per-source so one noisy source (20 CrashLoop
        // pods) can't drown your PRs/tickets/incidents — the band stays diverse.
        crate::suggest::store::balance_per_source(filtered, self.suggest_max, self.suggest_cap)
            .into_iter()
            .map(|st| {
                let mut label = String::from("\u{25cb} "); // ○ latent
                label.push_str(&st.suggestion.picker_label());
                // Freshness nudge: once a task has been waiting a while (>= 5m)
                // stamp its age, so the picker stays calm for fresh arrivals and
                // quietly nags the ones that have been sitting.
                let age = now.saturating_sub(st.first_seen_ms / 1000);
                if age >= 300 {
                    label.push_str("  ");
                    label.push_str(&crate::suggest::sources::util::relative_age(age));
                }
                SessionPickerRow {
                    label,
                    kind: RowKind::Suggestion(st.suggestion.id),
                }
            })
            .collect()
    }

    /// Find a LIVE session that already realizes this spawn target — same repo
    /// (project root) AND same emoji-native name. The accept-path dedup anchor:
    /// re-selecting a task you already started switches to it instead of
    /// spawning a twin. Collects candidate ids under the praça lock, then probes
    /// liveness through the registry (a separate lock) — never holds both at once.
    fn live_session_for(&self, spawn: &crate::suggest::SpawnSpec) -> Option<SessionId> {
        let candidates: Vec<SessionId> = {
            let praca = self.praca();
            praca
                .index
                .all()
                .iter()
                .filter(|rec| {
                    rec.project_root.as_path() == spawn.cwd()
                        && rec.display_name().as_str() == spawn.name()
                })
                .map(|rec| rec.id)
                .collect()
        };
        candidates.into_iter().find(|id| self.first_pane_of(*id).is_some())
    }

    /// Spawn a session aimed at a suggestion + switch — the
    /// [`RowKind::Suggestion`] accept path. Spawns into the suggestion's target
    /// cwd with its emoji-native name, indexes the new session, and posts its
    /// first pane to the shared switch channel.
    fn spawn_suggestion_inner(&self, id: SuggestionId, now: u64) -> bool {
        let Some(store) = self.suggestions.as_ref() else {
            return false;
        };
        let Some(sug) = store.get(id) else {
            return false;
        };
        // Idempotent accept — if you already started this exact task (a live
        // session at the same repo with the same emoji-native name), switch to
        // it instead of spawning a duplicate. Makes the destination's "nothing
        // duplicate is ever offered" true on the accept path, not just promised.
        if let Some(existing) = self.live_session_for(&sug.spawn) {
            return self.switch_to(existing);
        }
        // Spawn into the suggestion's cwd (mado's capability env, cwd
        // overridden).
        let cwd = sug.spawn.cwd().to_string_lossy().into_owned();
        let env = self.spawn_env_base.clone().with_cwd(Some(cwd));
        self.inproc.set_spawn_env(env);
        let Ok(sid) = self.inproc.new_session_with_source_and_size(
            sug.spawn.name(),
            &self.shell,
            SessionSource::Named("mado-suggestion".into()),
            (80, 24),
        ) else {
            return false;
        };
        let Some(pane) = self.first_pane_of(sid) else {
            return false;
        };
        {
            let mut praca = self.praca();
            let style = praca.name_style;
            let mut rec =
                praca::SessionRecord::for_project(sid, sug.spawn.cwd().to_path_buf(), style, now);
            rec.rename(sug.spawn.name().to_owned());
            praca.index.upsert(rec);
            praca.record_visit(sid, now);
        }
        // The magic: kick off the task in the fresh session. Enter on a
        // "🔍 pr#1234 fix the parser" suggestion lands you in the repo AND runs
        // `gh pr checkout 1234`. Sent through the typed MultiplexerControl seam;
        // PTY input buffering carries the keystrokes until the shell is ready,
        // so this works exactly like typing-ahead. No-op when the suggestion
        // carries no kickoff command (e.g. a dirty-repo row just seats you there).
        if let Some(cmd) = sug.spawn.initial_command() {
            let _ = self.inproc.send_keys(pane, &kickoff_keystrokes(cmd));
        }
        self.switch.post(pane);
        true
    }
}

/// Encode a suggestion's kickoff command as keystrokes: the trimmed command
/// text followed by Enter — the typed "run this now in the fresh session"
/// encoding the [`RowKind::Suggestion`] accept path delivers through
/// [`tear_types::MultiplexerControl::send_keys`].
fn kickoff_keystrokes(command: &str) -> Vec<u8> {
    let mut bytes = command.trim().as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
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
        let (mut rows, style) = {
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
            // Latent presets are surfaced only when the tier opts in
            // (`surface_presets`); bare keeps the picker live-only.
            if self.surface_presets {
                for def in praca.definitions.all() {
                    if !live_roots.contains(def.project_root.as_path()) {
                        items.push(praca::Ranked::Latent(def));
                    }
                }
            }
            // Badge decision per the tiered mode. Auto badges ONLY when the
            // list actually mixes live + latent — so an all-live picker (the
            // common case) stays byte-identical to legacy.
            let has_live = items.iter().any(|r| matches!(r, praca::Ranked::Live(_)));
            let has_latent = items.iter().any(|r| matches!(r, praca::Ranked::Latent(_)));
            let badge_rows = match self.badges {
                crate::config::BadgeMode::Off => false,
                crate::config::BadgeMode::Always => true,
                crate::config::BadgeMode::Auto => has_live && has_latent,
            };
            let mut rows: Vec<SessionPickerRow> = praca::rank_mixed(items, query, now)
                .into_iter()
                .map(|ranked| match ranked {
                    praca::Ranked::Live(rec) => SessionPickerRow {
                        label: Self::row_label(
                            &rec.display_name(),
                            &rec.project_root,
                            badge_rows.then_some(false),
                        ),
                        kind: RowKind::Switch(rec.id),
                    },
                    praca::Ranked::Latent(def) => SessionPickerRow {
                        label: Self::row_label(
                            &def.display_name(),
                            &def.project_root,
                            badge_rows.then_some(true),
                        ),
                        kind: RowKind::Instantiate(def.def_id),
                    },
                })
                .collect();

            // MRU rule: on an EMPTY query, the FIRST row is the session you
            // last came FROM. `last_seen` is a true visit order (every switch
            // records a visit), so the most-recent live session is the one
            // you're in NOW and the 2nd-most-recent is where you came from —
            // hoist that to the top so Ctrl-S+Enter toggles back. With a real
            // query the fuzzy rank wins (we don't reorder).
            if query.trim().is_empty() {
                let mut by_recency = live_recs.clone();
                by_recency.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                if let Some(came_from) = by_recency.get(1).map(|r| r.id) {
                    if let Some(pos) = rows
                        .iter()
                        .position(|row| matches!(row.kind, RowKind::Switch(id) if id == came_from))
                    {
                        let row = rows.remove(pos);
                        rows.insert(0, row);
                    }
                }
            }
            (rows, praca.name_style)
        };
        // Shade in the continuously-refreshing task suggestions BELOW the live
        // sessions + presets (the union-navigator law: Ctrl-S stays the one
        // navigator). Query-filtered + capped; empty when the stream is off.
        rows.extend(self.suggestion_rows(query, now));
        // If anything matched (live, latent, or a suggestion), that's the
        // picker's answer; only an empty union falls through to create.
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
                // Record the visit so `last_seen` stays a true MRU order:
                // the switched-to session becomes most-recent, so the NEXT
                // Ctrl-S surfaces the session you came FROM as the first row
                // (Ctrl-S+Enter toggles back). Without this, picker switches
                // wouldn't move the MRU cursor and the toggle would drift.
                let now = crate::auto_attach::now_unix_seconds();
                self.praca().record_visit(session, now);
                self.switch.post(pane);
                true
            }
            None => false,
        }
    }

    fn instantiate_and_switch(&self, def_id: DefinitionId, now: u64) -> bool {
        self.instantiate_preset(def_id, now)
    }

    fn spawn_suggestion(&self, id: SuggestionId, now: u64) -> bool {
        self.spawn_suggestion_inner(id, now)
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
        // Default test bridge: surface presets + Always-badge (deterministic).
        bridge_with_cfg(praca, inproc, true, crate::config::BadgeMode::Always)
    }

    fn bridge_with_cfg(
        praca: praca::Praca,
        inproc: Arc<tear_core::InProcess>,
        surface_presets: bool,
        badges: crate::config::BadgeMode,
    ) -> PracaPickerBridge {
        let switch = crate::session_switch::SwitchRequests::default();
        switch.attach_sink();
        PracaPickerBridge::new(
            Arc::new(Mutex::new(praca)),
            inproc,
            switch,
            "/bin/sh".to_owned(),
            tear_types::SpawnEnv::none(),
            surface_presets,
            badges,
            None,
            0,
            0,
        )
    }

    /// A bridge wired to a suggestion store (surfacing up to `max` ○
    /// suggestion rows) — the suggestion-stream test seam.
    fn bridge_with_suggestions(
        praca: praca::Praca,
        inproc: Arc<tear_core::InProcess>,
        store: Arc<SuggestionStore>,
        max: usize,
    ) -> PracaPickerBridge {
        let switch = crate::session_switch::SwitchRequests::default();
        switch.attach_sink();
        PracaPickerBridge::new(
            Arc::new(Mutex::new(praca)),
            inproc,
            switch,
            "/bin/sh".to_owned(),
            tear_types::SpawnEnv::none(),
            true,
            crate::config::BadgeMode::Auto,
            Some(store),
            max,
            0,
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
    fn empty_query_hoists_the_session_you_came_from_first() {
        // MRU rule: the FIRST empty-query row is the session you last came
        // FROM — the 2nd-most-recently-visited live session (the most-recent
        // being the one you're in NOW). So Ctrl-S + Enter toggles back.
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let mut spawn = |name: &str| {
            inproc
                .new_session_with_source_and_size(
                    name,
                    "/bin/sh",
                    SessionSource::Named("test".into()),
                    (80, 24),
                )
                .expect("spawn")
        };
        let current = spawn("current"); // most-recent → where you are now
        let came_from = spawn("camefrom"); // 2nd-most-recent → where you came from

        let mut praca = praca::Praca::new();
        // last_seen sets the visit recency: current=2000 (newest), came_from=1000.
        praca.index.upsert(praca::SessionRecord::for_project(
            current,
            PathBuf::from("/code/a"),
            SessionNameStyle::Emoji,
            2000,
        ));
        praca.index.upsert(praca::SessionRecord::for_project(
            came_from,
            PathBuf::from("/code/b"),
            SessionNameStyle::Emoji,
            1000,
        ));

        let bridge = bridge_with(praca, Arc::clone(&inproc));
        let rows = bridge.list("", 3000);
        assert!(rows.len() >= 2, "both live sessions listed");
        assert_eq!(
            rows[0].kind,
            RowKind::Switch(came_from),
            "first empty-query row must be the session you came from (2nd-most-recent), not the current one"
        );
        // The hoist is empty-query only; with ≥2 sessions the current one is
        // still present, just not first.
        assert!(
            rows.iter().any(|r| r.kind == RowKind::Switch(current)),
            "the current session is still listed, just not first"
        );
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
    fn auto_badges_only_when_the_list_mixes_live_and_latent() {
        // All-live (no surfaced preset): Auto → NO badge, so the common
        // case is byte-identical to the legacy picker.
        let (inproc, live) = live_inproc();
        let mut praca = praca::Praca::new();
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        let bridge = bridge_with_cfg(praca, Arc::clone(&inproc), true, crate::config::BadgeMode::Auto);
        let rows = bridge.list("", 1000);
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|r| !r.label.starts_with('\u{25cf}') && !r.label.starts_with('\u{25cb}')),
            "all-live + Auto must NOT badge (byte-identical legacy): {:?}",
            rows.iter().map(|r| &r.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn surface_presets_false_strips_latent_rows_even_when_presets_exist() {
        // The bare/stripped tier: a saved preset must NOT appear.
        let (inproc, _live) = live_inproc();
        let mut praca = praca::Praca::new();
        praca.definitions.upsert(praca::SessionDefinition::single_pane(
            "/code/pleme-io/substrate",
            "/bin/sh",
            praca::NameStyle::Emoji,
            1000,
        ));
        let bridge =
            bridge_with_cfg(praca, Arc::clone(&inproc), false, crate::config::BadgeMode::Auto);
        let rows = bridge.list("", 1000);
        assert!(
            rows.iter().all(|r| !matches!(r.kind, RowKind::Instantiate(_))),
            "surface_presets=false must surface NO latent rows"
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

    fn sug(source: crate::suggest::SourceKind, key: &str, title: &str, cwd: &str) -> crate::suggest::Suggestion {
        crate::suggest::Suggestion::new(
            source,
            key,
            title,
            crate::suggest::SpawnSpec::new(cwd, title).unwrap(),
        )
    }

    #[test]
    fn bridge_shades_in_suggestions_below_sessions_and_spawns_on_accept() {
        let (inproc, live) = live_inproc();
        let mut praca = praca::Praca::new();
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        let store = Arc::new(SuggestionStore::new());
        let s = sug(
            crate::suggest::SourceKind::GitBranchPr,
            "tear#7",
            "pr#7 fix tear",
            "/code/pleme-io/tear",
        );
        let id = s.id;
        store.ingest(crate::suggest::SourceKind::GitBranchPr, vec![s], 1000);

        let bridge = bridge_with_suggestions(praca, Arc::clone(&inproc), store, 6);
        let rows = bridge.list("", 1000);

        let sgrow = rows
            .iter()
            .find(|r| matches!(r.kind, RowKind::Suggestion(_)))
            .expect("a suggestion row is shaded in");
        assert_eq!(sgrow.kind, RowKind::Suggestion(id));
        assert!(sgrow.label.starts_with('\u{25cb}'), "○ latent badge: {}", sgrow.label);
        assert!(sgrow.label.contains("pr#7 fix tear"));

        // Suggestions shade in BELOW the live session.
        let live_pos = rows.iter().position(|r| matches!(r.kind, RowKind::Switch(_))).unwrap();
        let sug_pos = rows.iter().position(|r| matches!(r.kind, RowKind::Suggestion(_))).unwrap();
        assert!(live_pos < sug_pos, "suggestions shade in below sessions");

        // Accept spawns a session aimed at the task.
        let before = inproc.with_registry(|r| r.sessions.len());
        assert!(bridge.spawn_suggestion(id, 1000), "accept spawns the suggestion");
        let after = inproc.with_registry(|r| r.sessions.len());
        assert_eq!(after, before + 1, "a session was spawned for the suggestion");
    }

    #[test]
    fn suggestions_off_when_max_zero() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        store.ingest(
            crate::suggest::SourceKind::TendRepos,
            vec![sug(crate::suggest::SourceKind::TendRepos, "a", "a", "/x")],
            1000,
        );
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, store, 0);
        let rows = bridge.list("", 1000);
        assert!(rows.iter().all(|r| !matches!(r.kind, RowKind::Suggestion(_))));
    }

    #[test]
    fn suggestions_filter_by_query() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        store.ingest(
            crate::suggest::SourceKind::GitBranchPr,
            vec![
                sug(crate::suggest::SourceKind::GitBranchPr, "1", "alpha task", "/x"),
                sug(crate::suggest::SourceKind::GitBranchPr, "2", "beta task", "/y"),
            ],
            1000,
        );
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, store, 6);
        let rows = bridge.list("alpha", 1000);
        let sg: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Suggestion(_)))
            .collect();
        assert_eq!(sg.len(), 1, "only the matching suggestion");
        assert!(sg[0].label.contains("alpha"));
    }

    #[test]
    fn kickoff_keystrokes_trims_and_appends_enter() {
        assert_eq!(kickoff_keystrokes("gh pr checkout 7"), b"gh pr checkout 7\n");
        assert_eq!(kickoff_keystrokes("  spaced  "), b"spaced\n");
    }

    #[test]
    fn spawn_suggestion_with_a_command_spawns_and_kicks_off() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let spawn = crate::suggest::SpawnSpec::new("/code/pleme-io/tear", "\u{1F50D} pr#7")
            .unwrap()
            .with_command("echo kicked");
        let s = crate::suggest::Suggestion::new(
            crate::suggest::SourceKind::GitBranchPr,
            "tear#7",
            "pr#7 fix tear",
            spawn,
        );
        let id = s.id;
        store.ingest(crate::suggest::SourceKind::GitBranchPr, vec![s], 1000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), Arc::clone(&inproc), store, 6);

        let before = inproc.with_registry(|r| r.sessions.len());
        assert!(
            bridge.spawn_suggestion(id, 1000),
            "a suggestion with a kickoff command spawns + sends it without error"
        );
        let after = inproc.with_registry(|r| r.sessions.len());
        assert_eq!(after, before + 1, "the suggestion's session was spawned");
    }

    #[test]
    fn spawn_suggestion_is_idempotent_no_duplicate_session() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let s = sug(
            crate::suggest::SourceKind::GitBranchPr,
            "tear#7",
            "\u{1F50D} pr#7",
            "/code/pleme-io/tear",
        );
        let id = s.id;
        store.ingest(crate::suggest::SourceKind::GitBranchPr, vec![s], 1000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), Arc::clone(&inproc), store, 6);

        let before = inproc.with_registry(|r| r.sessions.len());
        assert!(bridge.spawn_suggestion(id, 1000), "first accept spawns the task session");
        let after_first = inproc.with_registry(|r| r.sessions.len());
        assert_eq!(after_first, before + 1);

        // Re-accepting the same suggestion switches to the existing session
        // instead of spawning a twin — "nothing duplicate is ever offered".
        assert!(bridge.spawn_suggestion(id, 1001), "second accept switches");
        let after_second = inproc.with_registry(|r| r.sessions.len());
        assert_eq!(
            after_second, after_first,
            "second accept must NOT duplicate the session"
        );
    }

    #[test]
    fn a_started_suggestion_drops_from_the_band_no_double_listing() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let s = sug(
            crate::suggest::SourceKind::GitBranchPr,
            "tear#7",
            "\u{1F50D} pr#7",
            "/code/pleme-io/tear",
        );
        let id = s.id;
        store.ingest(crate::suggest::SourceKind::GitBranchPr, vec![s], 1000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), Arc::clone(&inproc), store, 6);

        // Before starting: it's a ○ Suggestion row.
        assert!(
            bridge
                .list("", 1000)
                .iter()
                .any(|r| matches!(r.kind, RowKind::Suggestion(i) if i == id)),
            "the task shows as a suggestion before it's started"
        );

        assert!(bridge.spawn_suggestion(id, 1000), "start the task");

        // After: it's a live ● Switch row, and its ○ suggestion twin is gone.
        let rows = bridge.list("", 1001);
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::Suggestion(_))),
            "no duplicate ○ suggestion row once the task is live"
        );
        assert!(
            rows.iter().any(|r| matches!(r.kind, RowKind::Switch(_))),
            "the started task is now a live session row"
        );
    }

    #[test]
    fn stale_suggestion_shows_a_freshness_age_fresh_stays_calm() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let s = sug(crate::suggest::SourceKind::GitBranchPr, "x#1", "review parser", "/code/x");
        let id = s.id;
        // first_seen_ms = 1_000_000 → first_seen at unix-second 1000.
        store.ingest(crate::suggest::SourceKind::GitBranchPr, vec![s], 1_000_000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), Arc::clone(&inproc), store, 6);

        // 10 minutes later → a "10m" freshness stamp.
        let stale = bridge.list("", 1000 + 600);
        let row = stale
            .iter()
            .find(|r| matches!(r.kind, RowKind::Suggestion(i) if i == id))
            .unwrap();
        assert!(row.label.contains("10m"), "stale row shows its age: {}", row.label);

        // 1 minute later → fresh, no stamp (the title has no 'm', so this is clean).
        let fresh = bridge.list("", 1000 + 60);
        let frow = fresh
            .iter()
            .find(|r| matches!(r.kind, RowKind::Suggestion(i) if i == id))
            .unwrap();
        assert!(
            !frow.label.contains('m'),
            "a fresh row stays calm (no age stamp): {}",
            frow.label
        );
    }
}
